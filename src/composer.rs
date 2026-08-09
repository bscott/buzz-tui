//! The message composer.
//!
//! `tui-textarea` pins ratatui 0.29 and is therefore ABI-incompatible with the
//! 0.30 widgets the rest of the interface is built on, so buzz-tui owns its
//! editor. That turns out to be the better trade anyway: the composer is driven
//! entirely by [`Action`], never by raw key codes, so every editing key — down
//! to backspace — stays rebindable from `keys.toml`, and the help overlay can
//! describe the editor with the same machinery it uses for everything else.
//!
//! Positions are byte offsets into a `String` because that is what `str`
//! slicing wants, but every motion lands on a character boundary; a paste of
//! CJK or emoji must never split a scalar value.

use std::ops::Range;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::keys::Action;

/// Undo depth. Deep enough to walk back through a mistyped paragraph, shallow
/// enough that a long session cannot retain an unbounded pile of drafts.
const UNDO_LIMIT: usize = 100;

/// How many sent messages stay recallable.
const HISTORY_LIMIT: usize = 200;

/// Byte offset immediately after the last whitespace scalar in `text`.
pub(crate) fn token_start(text: &str) -> usize {
    text.char_indices()
        .rfind(|(_, c)| c.is_whitespace())
        .map_or(0, |(index, c)| index + c.len_utf8())
}

/// One point on the undo stack. Restoring the cursor along with the text is
/// what makes undo feel like a rewind rather than a replace.
#[derive(Debug, Clone)]
struct Snapshot {
    text: String,
    cursor: usize,
}

/// A multi-line editor for the message being written.
#[derive(Debug, Clone, Default)]
pub struct Composer {
    text: String,
    /// Byte offset of the caret; always on a character boundary.
    cursor: usize,
    /// Column, in characters, that vertical motion is trying to return to. A
    /// run of up and down presses should track the column the caret started
    /// from rather than degrading over short lines.
    desired: Option<usize>,
    undo: Vec<Snapshot>,
    /// Whether the newest undo entry may still absorb further typing.
    coalescing: bool,
    history: Vec<String>,
    /// Position within [`Composer::history`] while recalling, newest last.
    recall: Option<usize>,
    /// The unsent draft displaced by the current recall, restored on the way
    /// back out.
    draft: Option<String>,
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// True when the body holds nothing worth sending.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn set_text(&mut self, text: &str) {
        self.begin_edit();
        self.text.clear();
        self.text.push_str(text);
        self.cursor = self.text.len();
    }

    /// Takes the body, clears the composer, and records it in the recall
    /// history.
    pub fn take(&mut self) -> String {
        let body = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.desired = None;
        self.coalescing = false;
        self.undo.clear();
        self.recall = None;
        self.draft = None;
        if !body.trim().is_empty() && self.history.last().map(String::as_str) != Some(body.as_str())
        {
            self.history.push(body.clone());
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
        }
        body
    }

    pub fn clear(&mut self) {
        if self.text.is_empty() {
            self.cursor = 0;
            return;
        }
        self.begin_edit();
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        if c == '\n' {
            self.insert_newline();
            return;
        }
        // Consecutive characters share one undo step so that undo removes a
        // word, not a keystroke.
        if !self.coalescing {
            self.push_undo();
        }
        self.forget_recall();
        self.desired = None;
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.coalescing = true;
    }

    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // A paste arrives as one gesture and undoes as one step.
        self.begin_edit();
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// Replaces only the token under the caret with a marked completion. Text
    /// before that token is never considered part of the edit, even when the
    /// token currently consists of only `@` or `/`.
    pub fn complete_token(&mut self, marker: char, value: &str) {
        let start = token_start(&self.text[..self.cursor]);
        let end = self.text[self.cursor..]
            .find(char::is_whitespace)
            .map_or(self.text.len(), |offset| self.cursor + offset);
        let mut completed = String::with_capacity(marker.len_utf8() + value.len() + 1);
        completed.push(marker);
        completed.push_str(value);
        if end == self.text.len() {
            completed.push(' ');
        }

        self.begin_edit();
        self.text.replace_range(start..end, &completed);
        self.cursor = start + completed.len();
    }

    /// Applies an editing action. Returns false when the action is not one this
    /// widget handles, so the caller can fall through to other handlers.
    pub fn apply(&mut self, action: Action) -> bool {
        match action {
            Action::CursorLeft => {
                self.settle();
                self.cursor = self.prev_boundary(self.cursor);
            }
            Action::CursorRight => {
                self.settle();
                self.cursor = self.next_boundary(self.cursor);
            }
            Action::CursorUp => return self.move_vertical(true),
            Action::CursorDown => return self.move_vertical(false),
            Action::WordLeft => {
                self.settle();
                self.cursor = self.word_start_before(self.cursor);
            }
            Action::WordRight => {
                self.settle();
                self.cursor = self.word_end_after(self.cursor);
            }
            Action::LineStart => {
                self.settle();
                self.cursor = self.line_start(self.cursor);
            }
            Action::LineEnd => {
                self.settle();
                self.cursor = self.line_end(self.cursor);
            }
            Action::DeleteBack => {
                let from = self.prev_boundary(self.cursor);
                self.delete(from, self.cursor);
            }
            Action::DeleteForward => {
                let to = self.next_boundary(self.cursor);
                self.delete(self.cursor, to);
            }
            Action::DeleteWordBack => {
                let from = self.word_start_before(self.cursor);
                self.delete(from, self.cursor);
            }
            Action::DeleteWordForward => {
                let to = self.word_end_after(self.cursor);
                self.delete(self.cursor, to);
            }
            Action::KillToEnd => {
                let end = self.line_end(self.cursor);
                // At the end of a line there is nothing left to kill, so take
                // the break instead and pull the next line up, as readline does.
                let to = if end == self.cursor {
                    self.next_boundary(end)
                } else {
                    end
                };
                self.delete(self.cursor, to);
            }
            Action::KillToStart => {
                let start = self.line_start(self.cursor);
                self.delete(start, self.cursor);
            }
            Action::Newline => self.insert_newline(),
            Action::Undo => return self.undo(),
            Action::HistoryPrev => return self.history_prev(),
            Action::HistoryNext => return self.history_next(),
            _ => return false,
        }
        true
    }

    /// Byte offset of the cursor within [`Composer::text`]. Always a `char`
    /// boundary, which is what lets callers slice the body around it safely.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Visual cursor cell, wrapped to `width` columns, as (column, row).
    pub fn cursor_cell(&self, width: usize) -> (u16, u16) {
        let width = width.max(1);
        self.place(&self.wrap(width), width)
    }

    /// Rows the body occupies when wrapped to `width`, at least 1.
    ///
    /// A caret sitting just past a row that is exactly full needs a row of its
    /// own, so the height also covers the cursor cell; `cursor_cell(w).1` is
    /// always less than `height(w)`.
    pub fn height(&self, width: usize) -> u16 {
        let width = width.max(1);
        let rows = self.wrap(width);
        let (_, row) = self.place(&rows, width);
        clamp_u16(rows.len().max(1)).max(row.saturating_add(1))
    }

    /// The wrapped lines to render.
    pub fn lines(&self, width: usize) -> Vec<String> {
        self.wrap(width.max(1))
            .into_iter()
            .map(|row| self.text[row].to_string())
            .collect()
    }

    // -- editing internals ------------------------------------------------

    /// Opens a fresh undo step for a mutation that should never merge with
    /// surrounding typing.
    fn begin_edit(&mut self) {
        self.push_undo();
        self.coalescing = false;
        self.desired = None;
        self.forget_recall();
    }

    /// Ends any run of coalesced typing without recording an undo step; used by
    /// motions, which change nothing but must not let typing straddle them.
    fn settle(&mut self) {
        self.coalescing = false;
        self.desired = None;
    }

    fn push_undo(&mut self) {
        self.undo.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }

    fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop() else {
            return false;
        };
        self.text = snapshot.text;
        self.cursor = snapshot.cursor;
        self.coalescing = false;
        self.desired = None;
        self.forget_recall();
        true
    }

    fn insert_newline(&mut self) {
        self.begin_edit();
        self.text.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    fn delete(&mut self, from: usize, to: usize) {
        if from >= to {
            // Nothing to remove, so leave the undo stack alone; a backspace at
            // the start of the body should not cost an undo step.
            self.settle();
            return;
        }
        self.begin_edit();
        self.text.replace_range(from..to, "");
        self.cursor = from;
    }

    // -- recall -----------------------------------------------------------

    fn forget_recall(&mut self) {
        // Editing a recalled message detaches it from the history walk, so that
        // a later `history_next` cannot silently discard the new text.
        self.recall = None;
        self.draft = None;
    }

    fn show_recalled(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
        self.coalescing = false;
        self.desired = None;
    }

    fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let index = match self.recall {
            None => {
                // Stash the draft so walking back out restores what was typed.
                self.draft = Some(self.text.clone());
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.recall = Some(index);
        let text = self.history[index].clone();
        self.show_recalled(text);
        true
    }

    fn history_next(&mut self) -> bool {
        match self.recall {
            None => false,
            Some(i) if i + 1 < self.history.len() => {
                self.recall = Some(i + 1);
                let text = self.history[i + 1].clone();
                self.show_recalled(text);
                true
            }
            Some(_) => {
                self.recall = None;
                let draft = self.draft.take().unwrap_or_default();
                self.show_recalled(draft);
                true
            }
        }
    }

    // -- motion -----------------------------------------------------------

    fn prev_boundary(&self, at: usize) -> usize {
        match self.text[..at].chars().next_back() {
            Some(c) => at - c.len_utf8(),
            None => 0,
        }
    }

    fn next_boundary(&self, at: usize) -> usize {
        match self.text[at..].chars().next() {
            Some(c) => at + c.len_utf8(),
            None => self.text.len(),
        }
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |i| at + i)
    }

    fn word_start_before(&self, at: usize) -> usize {
        let mut i = at;
        while let Some(c) = self.text[..i].chars().next_back() {
            if is_word(c) {
                break;
            }
            i -= c.len_utf8();
        }
        while let Some(c) = self.text[..i].chars().next_back() {
            if !is_word(c) {
                break;
            }
            i -= c.len_utf8();
        }
        i
    }

    fn word_end_after(&self, at: usize) -> usize {
        let mut i = at;
        while let Some(c) = self.text[i..].chars().next() {
            if is_word(c) {
                break;
            }
            i += c.len_utf8();
        }
        while let Some(c) = self.text[i..].chars().next() {
            if !is_word(c) {
                break;
            }
            i += c.len_utf8();
        }
        i
    }

    /// Moves between logical lines, keeping the desired column. Vertical motion
    /// deliberately ignores soft wrapping: the composer is a handful of rows
    /// tall and users think in the lines they typed.
    fn move_vertical(&mut self, up: bool) -> bool {
        let start = self.line_start(self.cursor);
        let column = self
            .desired
            .unwrap_or_else(|| self.text[start..self.cursor].chars().count());
        let end = self.line_end(self.cursor);

        // At the far edge of the body there is no line to move to, so slide to
        // the edge of this one instead. That is what makes `up` in a one-line
        // composer feel like a jump to the start rather than a dead key.
        if up && start == 0 {
            self.cursor = 0;
            self.desired = Some(0);
            return true;
        }
        if !up && end == self.text.len() {
            self.cursor = end;
            self.desired = Some(self.text[start..end].chars().count());
            return true;
        }

        let (target_start, target_end) = if up {
            let target_end = start - 1;
            (self.line_start(target_end), target_end)
        } else {
            let target_start = end + 1;
            (target_start, self.line_end(target_start))
        };
        self.cursor = target_start + advance(&self.text[target_start..target_end], column);
        self.desired = Some(column);
        true
    }

    // -- layout -----------------------------------------------------------

    /// Byte ranges of the visual rows, in order. Ranges cover the whole body
    /// apart from the line breaks themselves, which lets the cursor be placed
    /// by a range lookup rather than by re-measuring the text.
    fn wrap(&self, width: usize) -> Vec<Range<usize>> {
        let mut rows = Vec::new();
        let mut start = 0;
        loop {
            let end = self.line_end(start);
            wrap_line(&self.text, start, end, width, &mut rows);
            if end == self.text.len() {
                break;
            }
            start = end + 1;
        }
        rows
    }

    fn place(&self, rows: &[Range<usize>], width: usize) -> (u16, u16) {
        let index = rows
            .iter()
            .rposition(|row| row.start <= self.cursor)
            .unwrap_or(0);
        let Some(row) = rows.get(index) else {
            return (0, 0);
        };
        let column = self.text[row.start..self.cursor.min(row.end)].width();
        if column < width {
            return (clamp_u16(column), clamp_u16(index));
        }
        // The caret sits past the last cell of a full row. If a row follows it
        // belongs to another logical line, so hold the caret on the last cell;
        // otherwise it earns a row of its own, which `height` accounts for.
        if index + 1 < rows.len() {
            (clamp_u16(width - 1), clamp_u16(index))
        } else {
            (0, clamp_u16(index + 1))
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offset of the `column`-th character of `line`, clamped to its length.
fn advance(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(i, _)| i)
}

/// Greedy word wrap. Trailing spaces are allowed to overhang the right edge so
/// that a break never opens a row with a stray space.
fn wrap_line(text: &str, start: usize, end: usize, width: usize, rows: &mut Vec<Range<usize>>) {
    let mut row_start = start;
    let mut column = 0usize;
    let mut last_break = None;

    for (offset, c) in text[start..end].char_indices() {
        let at = start + offset;
        let w = c.width().unwrap_or(0);
        if c != ' ' && column + w > width && at > row_start {
            let split = match last_break {
                Some(b) if b > row_start => b,
                // A word longer than the whole row has to be cut somewhere.
                _ => at,
            };
            rows.push(row_start..split);
            row_start = split;
            column = text[row_start..at].width() + w;
            last_break = None;
        } else {
            column += w;
        }
        if c == ' ' {
            last_break = Some(at + c.len_utf8());
        }
    }
    rows.push(row_start..end);
}

fn clamp_u16(value: usize) -> u16 {
    value.min(u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(text: &str) -> Composer {
        let mut c = Composer::new();
        c.set_text(text);
        c
    }

    /// Places the cursor at the byte offset of the first occurrence of `at`.
    fn seek(c: &mut Composer, at: &str) {
        let offset = c.text().find(at).expect("marker not in body");
        while c.cursor() > offset {
            c.apply(Action::CursorLeft);
        }
        while c.cursor() < offset {
            c.apply(Action::CursorRight);
        }
    }

    #[test]
    fn backspace_over_multibyte_text_removes_whole_characters() {
        for (typed, after_one) in [("héllo", "héll"), ("日本", "日"), ("ab🎉", "ab")] {
            let mut c = Composer::new();
            for ch in typed.chars() {
                c.insert_char(ch);
            }
            assert_eq!(c.text(), typed);
            assert_eq!(c.cursor(), typed.len());

            assert!(c.apply(Action::DeleteBack));
            assert_eq!(c.text(), after_one);
            assert_eq!(c.cursor(), after_one.len());
        }
    }

    #[test]
    fn completion_after_a_bare_marker_preserves_the_draft() {
        let mut c = composer("hello @");
        c.complete_token('@', "Alice");
        assert_eq!(c.text(), "hello @Alice ");
        assert_eq!(c.cursor(), c.text().len());
        let mut unicode = composer("hello\u{2003}@");
        unicode.complete_token('@', "Alice");
        assert_eq!(unicode.text(), "hello\u{2003}@Alice ");
    }

    #[test]
    fn completion_replaces_only_the_partial_token() {
        let mut c = composer("hello @ali there");
        seek(&mut c, " there");
        c.complete_token('@', "Alice");
        assert_eq!(c.text(), "hello @Alice there");

        let mut command = composer("/sear");
        command.complete_token('/', "search");
        assert_eq!(command.text(), "/search ");
    }

    #[test]
    fn motion_never_splits_a_scalar_value() {
        let mut c = composer("é日🎉");
        // Walking the whole body in both directions must only ever produce
        // offsets that `str` is willing to slice at.
        for _ in 0..6 {
            c.apply(Action::CursorLeft);
            assert!(c.text().is_char_boundary(c.cursor()));
        }
        assert_eq!(c.cursor(), 0);
        for _ in 0..6 {
            c.apply(Action::CursorRight);
            assert!(c.text().is_char_boundary(c.cursor()));
        }
        assert_eq!(c.cursor(), c.text().len());

        // Deleting forward from the start empties it a character at a time.
        for _ in 0..3 {
            c.apply(Action::LineStart);
            c.apply(Action::DeleteForward);
        }
        assert!(c.text().is_empty());
    }

    #[test]
    fn word_motions_treat_underscores_as_word_characters() {
        let body = "foo bar_baz  qux";
        let mut c = composer(body);

        assert!(c.apply(Action::WordLeft));
        assert_eq!(c.cursor(), body.find("qux").unwrap());
        c.apply(Action::WordLeft);
        assert_eq!(c.cursor(), body.find("bar_baz").unwrap());
        c.apply(Action::WordLeft);
        assert_eq!(c.cursor(), 0);
        // Already at the start, so it stays there.
        c.apply(Action::WordLeft);
        assert_eq!(c.cursor(), 0);

        c.apply(Action::WordRight);
        assert_eq!(c.cursor(), "foo".len());
        c.apply(Action::WordRight);
        assert_eq!(c.cursor(), "foo bar_baz".len());
        c.apply(Action::WordRight);
        assert_eq!(c.cursor(), body.len());
    }

    #[test]
    fn delete_word_back_eats_the_word_and_the_gap_before_it() {
        let mut c = composer("foo bar_baz  qux");
        assert!(c.apply(Action::DeleteWordBack));
        assert_eq!(c.text(), "foo bar_baz  ");
        c.apply(Action::DeleteWordBack);
        assert_eq!(c.text(), "foo ");
        c.apply(Action::DeleteWordBack);
        assert_eq!(c.text(), "");
        assert_eq!(c.cursor(), 0);
    }

    #[test]
    fn delete_word_forward_stops_at_the_end_of_the_word() {
        let mut c = composer("foo bar_baz  qux");
        c.apply(Action::LineStart);
        assert!(c.apply(Action::DeleteWordForward));
        assert_eq!(c.text(), " bar_baz  qux");
        c.apply(Action::DeleteWordForward);
        assert_eq!(c.text(), "  qux");
    }

    #[test]
    fn line_start_and_line_end_stay_within_the_logical_line() {
        let mut c = composer("alpha\nbeta\ngamma");
        seek(&mut c, "eta");

        assert!(c.apply(Action::LineEnd));
        assert_eq!(c.cursor(), "alpha\nbeta".len());
        assert!(c.apply(Action::LineStart));
        assert_eq!(c.cursor(), "alpha\n".len());
    }

    #[test]
    fn vertical_motion_keeps_the_desired_column_across_a_short_line() {
        let mut c = composer("hello world\nhi\ngoodbye all");
        seek(&mut c, "world");
        let column = "hello ".len();
        assert_eq!(c.cursor(), column);

        // The middle line is too short, so the caret clamps to its end but
        // remembers where it wanted to be.
        assert!(c.apply(Action::CursorDown));
        assert_eq!(c.cursor(), "hello world\nhi".len());
        assert!(c.apply(Action::CursorDown));
        assert_eq!(c.cursor(), "hello world\nhi\n".len() + column);

        assert!(c.apply(Action::CursorUp));
        assert_eq!(c.cursor(), "hello world\nhi".len());
        assert!(c.apply(Action::CursorUp));
        assert_eq!(c.cursor(), column);
    }

    #[test]
    fn vertical_motion_at_the_edges_slides_to_the_edge_of_the_line() {
        let mut c = composer("only line");
        seek(&mut c, "line");

        // Up on the first line is a jump to the start, not a dead key.
        assert!(c.apply(Action::CursorUp));
        assert_eq!(c.cursor(), 0);
        assert!(c.apply(Action::CursorDown));
        assert_eq!(c.cursor(), c.text().len());
    }

    #[test]
    fn kill_to_end_and_start_operate_on_the_middle_line() {
        let mut c = composer("alpha\nbeta\ngamma");
        seek(&mut c, "ta\ng");
        assert!(c.apply(Action::KillToEnd));
        assert_eq!(c.text(), "alpha\nbe\ngamma");

        assert!(c.apply(Action::KillToStart));
        assert_eq!(c.text(), "alpha\n\ngamma");
        assert_eq!(c.cursor(), "alpha\n".len());
    }

    #[test]
    fn kill_to_end_at_a_line_end_joins_the_next_line() {
        let mut c = composer("alpha\nbeta");
        c.apply(Action::LineStart);
        c.apply(Action::CursorUp);
        c.apply(Action::LineEnd);
        assert_eq!(c.cursor(), "alpha".len());
        c.apply(Action::KillToEnd);
        assert_eq!(c.text(), "alphabeta");
    }

    #[test]
    fn undo_coalesces_a_run_of_typing_into_one_step() {
        let mut c = Composer::new();
        for ch in "abc".chars() {
            c.insert_char(ch);
        }
        assert!(c.apply(Action::Undo));
        assert_eq!(c.text(), "");
        assert_eq!(c.cursor(), 0);
        assert!(!c.apply(Action::Undo));
    }

    #[test]
    fn a_deletion_starts_a_new_undo_step() {
        let mut c = Composer::new();
        for ch in "abc".chars() {
            c.insert_char(ch);
        }
        c.apply(Action::DeleteBack);
        assert_eq!(c.text(), "ab");

        assert!(c.apply(Action::Undo));
        assert_eq!(c.text(), "abc");
        assert_eq!(c.cursor(), 3);
        assert!(c.apply(Action::Undo));
        assert_eq!(c.text(), "");
    }

    #[test]
    fn a_cursor_move_breaks_the_coalescing_run() {
        let mut c = Composer::new();
        c.insert_char('a');
        c.apply(Action::CursorLeft);
        c.insert_char('b');
        assert_eq!(c.text(), "ba");

        c.apply(Action::Undo);
        assert_eq!(c.text(), "a");
        c.apply(Action::Undo);
        assert_eq!(c.text(), "");
    }

    #[test]
    fn a_newline_starts_a_new_undo_step() {
        let mut c = Composer::new();
        c.insert_char('a');
        c.apply(Action::Newline);
        c.insert_char('b');
        assert_eq!(c.text(), "a\nb");

        c.apply(Action::Undo);
        assert_eq!(c.text(), "a\n");
        c.apply(Action::Undo);
        assert_eq!(c.text(), "a");
    }

    #[test]
    fn the_undo_stack_is_bounded() {
        let mut c = Composer::new();
        for i in 0..(UNDO_LIMIT + 50) {
            c.insert_str(&i.to_string());
        }
        assert_eq!(c.undo.len(), UNDO_LIMIT);
    }

    #[test]
    fn history_recall_walks_back_and_restores_the_draft() {
        let mut c = Composer::new();
        c.set_text("first");
        assert_eq!(c.take(), "first");
        c.set_text("second");
        c.take();

        c.set_text("draft");
        assert!(c.apply(Action::HistoryPrev));
        assert_eq!(c.text(), "second");
        assert_eq!(c.cursor(), "second".len());
        assert!(c.apply(Action::HistoryPrev));
        assert_eq!(c.text(), "first");
        // The oldest entry is the end of the walk.
        assert!(!c.apply(Action::HistoryPrev));
        assert_eq!(c.text(), "first");

        assert!(c.apply(Action::HistoryNext));
        assert_eq!(c.text(), "second");
        assert!(c.apply(Action::HistoryNext));
        assert_eq!(c.text(), "draft");
        // Back at the draft there is nowhere further forward to go.
        assert!(!c.apply(Action::HistoryNext));
        assert_eq!(c.text(), "draft");
    }

    #[test]
    fn history_next_does_nothing_before_a_recall_starts() {
        let mut c = Composer::new();
        c.set_text("sent");
        c.take();
        assert!(!c.apply(Action::HistoryNext));
    }

    #[test]
    fn history_skips_blanks_and_consecutive_duplicates() {
        let mut c = Composer::new();
        c.set_text("   \n ");
        c.take();
        assert!(!c.apply(Action::HistoryPrev));

        c.set_text("hello");
        c.take();
        c.set_text("hello");
        c.take();
        assert_eq!(c.history, vec!["hello".to_string()]);
    }

    #[test]
    fn the_history_is_bounded() {
        let mut c = Composer::new();
        for i in 0..(HISTORY_LIMIT + 10) {
            c.set_text(&i.to_string());
            c.take();
        }
        assert_eq!(c.history.len(), HISTORY_LIMIT);
        assert_eq!(c.history[0], 10.to_string());
    }

    #[test]
    fn wrapping_agrees_with_the_cursor_cell_and_the_height() {
        let body = "the quick brown fox jumps over";
        let mut c = composer(body);
        let width = 10;

        let lines = c.lines(width);
        assert_eq!(lines, vec!["the quick ", "brown fox ", "jumps over"]);
        // Rejoining the rows must reproduce the body exactly.
        assert_eq!(lines.concat(), body);

        c.apply(Action::LineStart);
        assert_eq!(c.cursor_cell(width), (0, 0));
        assert_eq!(c.height(width), 3);

        seek(&mut c, "fox");
        assert_eq!(c.cursor_cell(width), (6, 1));

        seek(&mut c, "over");
        assert_eq!(c.cursor_cell(width), (6, 2));

        // Every position in the body must land inside the reported height.
        for offset in 0..=body.len() {
            if !body.is_char_boundary(offset) {
                continue;
            }
            c.apply(Action::LineStart);
            while c.cursor() < offset {
                c.apply(Action::CursorRight);
            }
            let (column, row) = c.cursor_cell(width);
            assert!(usize::from(column) < width);
            assert!(row < c.height(width), "cursor row escaped the height");
        }
    }

    #[test]
    fn a_caret_past_a_full_row_gets_a_row_of_its_own() {
        let c = composer("abcde");
        assert_eq!(c.lines(5), vec!["abcde"]);
        assert_eq!(c.cursor_cell(5), (0, 1));
        assert_eq!(c.height(5), 2);
    }

    #[test]
    fn wide_characters_wrap_by_display_width() {
        // Double-width characters cost two columns each, so a ten column
        // composer holds five of them per row.
        let c = composer("日本語日本語");
        assert_eq!(c.lines(10), vec!["日本語日本", "語"]);
        assert_eq!(c.height(10), 2);
    }

    #[test]
    fn a_word_longer_than_the_row_is_cut_rather_than_lost() {
        let c = composer("supercalifragilistic");
        assert_eq!(c.lines(5), vec!["super", "calif", "ragil", "istic"]);
        assert_eq!(c.lines(5).concat(), c.text());
    }

    #[test]
    fn hard_line_breaks_start_a_new_row_even_when_short() {
        let c = composer("a\n\nb");
        assert_eq!(c.lines(20), vec!["a", "", "b"]);
        assert_eq!(c.height(20), 3);
    }

    #[test]
    fn an_empty_body_still_occupies_one_row() {
        let c = Composer::new();
        assert_eq!(c.lines(20), vec![""]);
        assert_eq!(c.height(20), 1);
        assert_eq!(c.cursor_cell(20), (0, 0));
    }

    #[test]
    fn a_whitespace_only_body_is_empty() {
        assert!(Composer::new().is_empty());
        assert!(composer("   \n ").is_empty());
        assert!(!composer(" x ").is_empty());
    }

    #[test]
    fn take_clears_the_composer_and_leaves_it_usable() {
        let mut c = composer("hello");
        assert_eq!(c.take(), "hello");
        assert_eq!(c.text(), "");
        assert_eq!(c.cursor(), 0);
        assert!(c.is_empty());
        c.insert_str("again");
        assert_eq!(c.text(), "again");
        assert_eq!(c.cursor(), 5);
    }

    #[test]
    fn unrelated_actions_fall_through() {
        let mut c = composer("hello");
        for action in [
            Action::Quit,
            Action::Send,
            Action::Paste,
            Action::Complete,
            Action::ScrollUp,
        ] {
            assert!(!c.apply(action), "{action:?} should not be handled here");
        }
        assert_eq!(c.text(), "hello");
    }
}
