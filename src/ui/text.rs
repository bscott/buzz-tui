//! Text measurement, wrapping, and the light markup the timeline understands.
//!
//! Ratatui exposes no public wrapping helper — its `reflow` module is private —
//! so every layout decision that depends on how many cells a string occupies
//! has to be made here. Terminals are grids of cells rather than of characters,
//! and a chat client sees the full range of user text: CJK, emoji, combining
//! marks, and pasted URLs longer than the pane they land in. Measuring with
//! [`unicode_width`] rather than `str::len` or `chars().count()` is what keeps a
//! Japanese message from tearing the sidebar off its column.
//!
//! Everything here works on `char` boundaries. A char whose width is two cells
//! is never split, which is the property that matters in practice; full
//! grapheme clustering would cost a dependency for a difference no one can see
//! at these sizes.

use std::borrow::Cow;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The marker appended when text is clipped. One cell wide, so the budget
/// arithmetic below can simply reserve a single cell for it.
const ELLIPSIS: &str = "…";

/// Extensions we are willing to hand to the image decoder.
const IMAGE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif"];

/// Display width in terminal cells.
pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Control characters have no defined width. Counting them as zero means a
/// stray escape in a message body shifts nothing, which is the safe failure.
fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Byte end and cell width of the longest prefix of `text` fitting in `max`
/// cells. A wide char is either taken whole or left behind, so the returned
/// width may fall one cell short of the budget.
fn prefix(text: &str, max: usize) -> (usize, usize) {
    let mut used = 0;
    for (offset, ch) in text.char_indices() {
        let cells = char_width(ch);
        if used + cells > max {
            return (offset, used);
        }
        used += cells;
    }
    (text.len(), used)
}

/// The mirror of [`prefix`]: byte start and cell width of the longest suffix
/// fitting in `max` cells.
fn suffix(text: &str, max: usize) -> (usize, usize) {
    let mut used = 0;
    for (offset, ch) in text.char_indices().rev() {
        let cells = char_width(ch);
        if used + cells > max {
            return (offset + ch.len_utf8(), used);
        }
        used += cells;
    }
    (0, used)
}

/// Truncates to `max` cells, appending `…`. At `max == 1` the result is just
/// `…`. At `max == 0` the result is empty. Never splits a wide grapheme.
pub fn truncate_end(text: &str, max: usize) -> Cow<'_, str> {
    if width(text) <= max {
        return Cow::Borrowed(text);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    if max == 1 {
        return Cow::Borrowed(ELLIPSIS);
    }
    let (end, _) = prefix(text, max - 1);
    let mut out = String::with_capacity(end + ELLIPSIS.len());
    out.push_str(&text[..end]);
    out.push_str(ELLIPSIS);
    Cow::Owned(out)
}

/// Elides the middle, keeping both ends: `verylongname.rs` -> `very…e.rs`.
///
/// Filenames, channel topics, and npubs all carry their distinguishing
/// information at the edges, so clipping the tail off would make two different
/// values render identically.
pub fn middle_elide(text: &str, max: usize) -> Cow<'_, str> {
    if width(text) <= max {
        return Cow::Borrowed(text);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    if max == 1 {
        return Cow::Borrowed(ELLIPSIS);
    }
    let budget = max - 1;
    let (head_end, head_used) = prefix(text, budget.div_ceil(2));
    let (tail_offset, _) = suffix(&text[head_end..], budget - head_used);
    let tail_start = head_end + tail_offset;
    let head = &text[..head_end];
    let tail = &text[tail_start..];
    let mut out = String::with_capacity(head.len() + ELLIPSIS.len() + tail.len());
    out.push_str(head);
    out.push_str(ELLIPSIS);
    out.push_str(tail);
    Cow::Owned(out)
}

/// Word-wraps to `width` cells. Honours existing `\n` as hard breaks, breaks
/// over-long words mid-word rather than overflowing, never splits a wide
/// grapheme, and preserves a trailing empty line so a text ending in `\n`
/// reports the extra row the cursor will occupy. A `width` of 0 yields one
/// empty line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    wrap_lines(text, width, &mut |line| lines.push(line.to_string()));
    lines
}

/// Number of rows [`wrap`] would produce, without allocating the rows.
pub fn wrapped_height(text: &str, width: usize) -> usize {
    let mut rows = 0;
    wrap_lines(text, width, &mut |_| rows += 1);
    rows
}

/// Feeds `sink` one borrowed slice per rendered row.
///
/// Both [`wrap`] and [`wrapped_height`] are written against this so that a
/// measured height can never disagree with the rows that are later drawn. Every
/// row is a slice of the input: rows are cut either at a space, which is
/// dropped, or between two chars, so no row ever needs to be built up.
fn wrap_lines<'a>(text: &'a str, cells: usize, sink: &mut dyn FnMut(&'a str)) {
    if cells == 0 {
        sink("");
        return;
    }
    for segment in text.split('\n') {
        wrap_segment(segment, cells, sink);
    }
}

/// Wraps one hard line. Greedy: a word joins the current row when it still
/// fits, otherwise it opens the next one.
fn wrap_segment<'a>(segment: &'a str, cells: usize, sink: &mut dyn FnMut(&'a str)) {
    // The row under construction, as (byte start, byte end, cell width).
    let mut row: Option<(usize, usize, usize)> = None;
    let mut emitted = false;
    let mut pos = 0;

    loop {
        let end = match segment[pos..].find(' ') {
            Some(rel) => pos + rel,
            None => segment.len(),
        };
        let word = &segment[pos..end];
        let word_cells = UnicodeWidthStr::width(word);

        if let Some((start, stop, used)) = row {
            // The separating space is charged to the word that follows it, and
            // stays inside the slice because the row is contiguous.
            if used + 1 + word_cells <= cells {
                row = Some((start, end, used + 1 + word_cells));
            } else {
                sink(&segment[start..stop]);
                emitted = true;
                row = None;
            }
        }

        if row.is_none() {
            if word_cells <= cells {
                row = Some((pos, end, word_cells));
            } else {
                // An over-long word is cut into full rows rather than allowed
                // to overflow the pane. A single char wider than the whole
                // pane still gets a row of its own; dropping it would be worse.
                let mut chunk_start = pos;
                let mut chunk_cells = 0;
                let mut offset = pos;
                for ch in word.chars() {
                    let ch_cells = char_width(ch);
                    if chunk_cells + ch_cells > cells && offset > chunk_start {
                        sink(&segment[chunk_start..offset]);
                        emitted = true;
                        chunk_start = offset;
                        chunk_cells = 0;
                    }
                    chunk_cells += ch_cells;
                    offset += ch.len_utf8();
                }
                row = Some((chunk_start, end, chunk_cells));
            }
        }

        if end == segment.len() {
            break;
        }
        pos = end + 1;
    }

    if let Some((start, stop, _)) = row {
        sink(&segment[start..stop]);
    } else if !emitted {
        // An empty hard line is still a row on screen.
        sink("");
    }
}

/// Pads to exactly `width` cells with spaces, truncating when too long.
pub fn pad_to(text: &str, width: usize) -> String {
    let clipped = truncate_end(text, width);
    let used = self::width(&clipped);
    let mut out = String::with_capacity(clipped.len() + width - used);
    out.push_str(&clipped);
    for _ in used..width {
        out.push(' ');
    }
    out
}

/// Splits a message body into rendered segments so the timeline can style code
/// spans, links, and mentions differently from prose.
///
/// [`Segment::Code`] carries the span's contents without its backticks, and
/// [`Segment::Mention`] carries the sigil or the `npub1` prefix, because the
/// renderer wants to draw the former without its delimiters and the latter
/// with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment<'a> {
    Text(&'a str),
    Code(&'a str),
    Link(&'a str),
    Mention(&'a str),
}

/// Recognises `` `inline code` ``, bare `http://` and `https://` URLs, and
/// `@name` / `npub1…` mentions. Anything unrecognised stays [`Segment::Text`].
///
/// This is deliberately not a Markdown parser. Chat messages are typed in a
/// hurry and half of them contain stray asterisks; recognising only the three
/// constructs that carry an action for the reader keeps the timeline honest.
pub fn segments(body: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut text_start = 0;
    let mut pos = 0;

    while pos < body.len() {
        let rest = &body[pos..];
        let ch = match rest.chars().next() {
            Some(ch) => ch,
            None => break,
        };

        if ch == '`'
            && let Some(rel) = rest[1..].find('`')
            && rel > 0
        {
            flush_text(&mut out, body, text_start, pos);
            out.push(Segment::Code(&rest[1..1 + rel]));
            pos += rel + 2;
            text_start = pos;
            continue;
        }

        let at_boundary = starts_token(body, pos);

        if at_boundary && (rest.starts_with("http://") || rest.starts_with("https://")) {
            let end = rest.find(url_terminator).unwrap_or(rest.len());
            let url = trim_url_tail(&rest[..end]);
            // A bare scheme with no host is prose, not a link.
            if let Some((_, host)) = url.split_once("//")
                && !host.is_empty()
            {
                flush_text(&mut out, body, text_start, pos);
                out.push(Segment::Link(url));
                pos += url.len();
                text_start = pos;
                continue;
            }
        }

        if at_boundary && ch == '@' {
            let end = mention_end(&rest[1..]) + 1;
            let name = rest[..end].trim_end_matches(['.', '-', '_']);
            if name.len() > 1 {
                flush_text(&mut out, body, text_start, pos);
                out.push(Segment::Mention(name));
                pos += name.len();
                text_start = pos;
                continue;
            }
        }

        if at_boundary && rest.starts_with("npub1") {
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(rest.len());
            // A bech32 npub is 63 characters, but relays and humans both
            // abbreviate; anything shorter than a handful of data characters is
            // just a word that happens to start with `npub1`.
            if end >= "npub1".len() + 8 {
                flush_text(&mut out, body, text_start, pos);
                out.push(Segment::Mention(&rest[..end]));
                pos += end;
                text_start = pos;
                continue;
            }
        }

        pos += ch.len_utf8();
    }

    if text_start < body.len() {
        out.push(Segment::Text(&body[text_start..]));
    }
    out
}

/// Emits the run of prose preceding a recognised construct.
fn flush_text<'a>(out: &mut Vec<Segment<'a>>, body: &'a str, from: usize, to: usize) {
    if from < to {
        out.push(Segment::Text(&body[from..to]));
    }
}

/// Whether the byte offset begins a token rather than sitting inside a word, so
/// that the `@` of an email address and the `http` of `xhttp://` are left as
/// prose.
fn starts_token(body: &str, pos: usize) -> bool {
    match body[..pos].chars().next_back() {
        None => true,
        Some(prev) => !prev.is_alphanumeric() && !matches!(prev, '_' | '-' | '.' | '/' | '@'),
    }
}

/// A URL runs to the first character that cannot plausibly be inside one.
fn url_terminator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '`' | '<' | '>' | '"' | '\'' | '\\' | '|' | '^' | '{' | '}')
}

/// Sentences end in punctuation and URLs are quoted inside brackets, so the
/// trailing run of punctuation belongs to the prose rather than to the link.
fn trim_url_tail(url: &str) -> &str {
    url.trim_end_matches(|ch: char| {
        matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'' | '>')
    })
}

/// Byte length of the name following an `@`.
fn mention_end(rest: &str) -> usize {
    rest.find(|ch: char| !(ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.')))
        .unwrap_or(rest.len())
}

/// Every http(s) URL in a body, in order, deduplicated.
///
/// Deduplicating matters because the link picker numbers what it shows and a
/// quoted reply routinely repeats the URL it is answering.
pub fn links(body: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for segment in segments(body) {
        if let Segment::Link(url) = segment
            && !out.contains(&url)
        {
            out.push(url);
        }
    }
    out
}

/// Links whose path ends in a common raster image extension, plus Buzz Blossom
/// media URLs of the shape `/media/<64-hex-sha256>.<ext>`.
pub fn image_links(body: &str) -> Vec<&str> {
    links(body)
        .into_iter()
        .filter(|url| is_image_url(url))
        .collect()
}

fn is_image_url(url: &str) -> bool {
    let path = match url.split_once(['?', '#']) {
        Some((path, _)) => path,
        None => url,
    };
    let Some((stem, ext)) = path.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() || ext.contains('/') {
        return false;
    }
    if IMAGE_EXTENSIONS
        .iter()
        .any(|known| ext.eq_ignore_ascii_case(known))
    {
        return true;
    }
    // Blossom names a blob by the hex sha256 of its contents, so the shape of
    // the path identifies media even when the extension is one we do not know.
    match stem.rsplit_once("/media/") {
        Some((_, hash)) => hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_counts_cells_not_characters() {
        assert_eq!(width("hello"), 5);
        assert_eq!(width("日本語"), 6);
        assert_eq!(width("😀"), 2);
        assert_eq!(width(""), 0);
    }

    #[test]
    fn truncate_end_leaves_text_that_already_fits() {
        assert_eq!(truncate_end("hello", 5), "hello");
        assert!(matches!(truncate_end("hello", 5), Cow::Borrowed(_)));
        assert_eq!(truncate_end("hello", 9), "hello");
    }

    #[test]
    fn truncate_end_handles_tiny_budgets() {
        assert_eq!(truncate_end("hello", 0), "");
        assert_eq!(truncate_end("hello", 1), "…");
        assert_eq!(truncate_end("hello", 2), "h…");
        for max in 0..=6 {
            assert!(width(&truncate_end("hello", max)) <= max);
        }
    }

    #[test]
    fn truncate_end_never_overruns_on_wide_characters() {
        // The budget of four leaves three cells for content, and a third
        // ideograph would need a fourth, so one cell goes unused rather than
        // the line overflowing.
        assert_eq!(truncate_end("日本語", 4), "日…");
        assert_eq!(truncate_end("日本語", 5), "日本…");
        for max in 0..=8 {
            let clipped = truncate_end("日本語テキスト", max);
            assert!(
                width(&clipped) <= max,
                "width {} exceeds budget {max}",
                width(&clipped)
            );
        }
    }

    #[test]
    fn middle_elide_keeps_both_ends() {
        assert_eq!(middle_elide("verylongname.rs", 9), "very…e.rs");
        assert_eq!(middle_elide("short.rs", 20), "short.rs");
        for max in 0..=15 {
            let elided = middle_elide("verylongname.rs", max);
            assert!(width(&elided) <= max, "{elided:?} exceeds {max}");
        }
        let elided = middle_elide("npub1abcdefghijklmnop", 11);
        assert!(elided.starts_with("npub"));
        assert!(elided.ends_with("op"));
    }

    #[test]
    fn wrap_breaks_a_paragraph_at_spaces() {
        assert_eq!(
            wrap("the quick brown fox jumps over the lazy dog", 10),
            ["the quick", "brown fox", "jumps over", "the lazy", "dog"]
        );
    }

    #[test]
    fn wrap_honours_hard_breaks_and_a_trailing_newline() {
        assert_eq!(wrap("one\ntwo", 10), ["one", "two"]);
        assert_eq!(wrap("one\n\ntwo", 10), ["one", "", "two"]);
        // The cursor sits on a fourth row after the final newline.
        assert_eq!(wrap("one\ntwo\n", 10), ["one", "two", ""]);
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn wrap_splits_a_word_longer_than_the_pane() {
        assert_eq!(wrap("supercalifragilistic", 6), ["superc", "alifra", "gilist", "ic"]);
        assert_eq!(wrap("hi supercalifragilistic", 6), ["hi", "superc", "alifra", "gilist", "ic"]);
    }

    #[test]
    fn wrap_never_splits_a_wide_character() {
        let text = "日本語のテキストを折り返す";
        for cells in 2..=12 {
            let lines = wrap(text, cells);
            for line in &lines {
                assert!(
                    width(line) <= cells,
                    "line {line:?} is {} cells, budget {cells}",
                    width(line)
                );
            }
            assert_eq!(lines.concat(), text, "wrapping dropped characters");
        }
        // An odd budget cannot be filled exactly by two-cell characters.
        assert_eq!(wrap("日本語", 5), ["日本", "語"]);
    }

    #[test]
    fn wrap_at_zero_width_yields_one_empty_line() {
        assert_eq!(wrap("anything at all", 0), [""]);
        assert_eq!(wrap("with\nbreaks", 0), [""]);
    }

    #[test]
    fn wrapped_height_agrees_with_wrap() {
        let cases = [
            ("", 10),
            ("short", 10),
            ("the quick brown fox jumps over the lazy dog", 10),
            ("one\ntwo\n", 4),
            ("supercalifragilistic", 6),
            ("日本語のテキストを折り返す", 7),
            ("anything", 0),
        ];
        for (text, cells) in cases {
            assert_eq!(
                wrapped_height(text, cells),
                wrap(text, cells).len(),
                "height disagreed for {text:?} at {cells}"
            );
        }
    }

    #[test]
    fn pad_to_produces_exactly_the_requested_width() {
        assert_eq!(pad_to("ab", 5), "ab   ");
        assert_eq!(width(&pad_to("日本語", 7)), 7);
        assert_eq!(width(&pad_to("far too long to fit", 6)), 6);
        assert_eq!(pad_to("", 3), "   ");
    }

    #[test]
    fn segments_finds_markup_and_keeps_the_prose_around_it() {
        let body = "hey @alice run `cargo test` then see https://example.com/x ok";
        assert_eq!(
            segments(body),
            [
                Segment::Text("hey "),
                Segment::Mention("@alice"),
                Segment::Text(" run "),
                Segment::Code("cargo test"),
                Segment::Text(" then see "),
                Segment::Link("https://example.com/x"),
                Segment::Text(" ok"),
            ]
        );
    }

    #[test]
    fn segments_recognises_an_npub_but_not_an_email() {
        let body = "ping npub1qqqqqqqqqqqqqqzyx here";
        assert!(segments(body).contains(&Segment::Mention("npub1qqqqqqqqqqqqqqzyx")));
        assert_eq!(
            segments("mail me at bob@example.com"),
            [Segment::Text("mail me at bob@example.com")]
        );
    }

    #[test]
    fn links_strip_trailing_punctuation_and_deduplicate() {
        assert_eq!(links("see https://example.com/a.  "), ["https://example.com/a"]);
        assert_eq!(
            links("(https://example.com/b) and again https://example.com/b"),
            ["https://example.com/b"]
        );
        assert_eq!(links("nothing here"), Vec::<&str>::new());
    }

    #[test]
    fn image_links_accept_pictures_and_blossom_blobs() {
        let hash = "a".repeat(64);
        let body = format!(
            "https://cdn.test/a.png https://cdn.test/b.jpg https://cdn.test/c.webp \
             https://relay.test/media/{hash}.png https://example.com/page.html"
        );
        let blossom = format!("https://relay.test/media/{hash}.png");
        assert_eq!(
            image_links(&body),
            [
                "https://cdn.test/a.png",
                "https://cdn.test/b.jpg",
                "https://cdn.test/c.webp",
                blossom.as_str(),
            ]
        );
        assert!(image_links("https://example.com/page.html").is_empty());
        assert!(image_links("https://example.com").is_empty());
        // A short hash is not a Blossom blob, and `.bin` is not an image.
        assert!(image_links("https://relay.test/media/abc.bin").is_empty());
    }
}
