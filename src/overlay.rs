//! Modal overlays: the switcher, search, prompts, confirmations, and help.
//!
//! Overlays own the keyboard while they are open. Their keys are deliberately
//! not part of the rebindable keymap: a picker where `enter` might not accept
//! and `esc` might not cancel would be hostile, and the muscle memory for these
//! four keys is universal. Everything that *opens* an overlay is bindable.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use semver::Version;

use crate::composer::token_start;
use crate::keys::{Group, Keymap};
use crate::model::Message;

/// What the caller should do after an overlay has seen a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Handled; redraw and keep the overlay open.
    Consumed,
    /// Dismiss without acting.
    Close,
    /// Dismiss and carry out the request.
    Submit(Submission),
    /// Not an overlay key; let the normal keymap have it.
    Ignored,
}

/// A completed overlay interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum Submission {
    OpenChannel(String),
    SwitchCommunity(String),
    React(String),
    JumpToMessage { channel: String, id: String },
    InsertMention(String),
    InsertCommand(String),
    Prompt { kind: PromptKind, value: String },
    Confirmed(Confirmation),
}

/// What a single-line prompt is collecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    CreateChannel,
    JoinChannel,
    DirectMessage,
    SetRelay,
    SetTopic,
}

impl PromptKind {
    pub fn title(self) -> &'static str {
        match self {
            PromptKind::CreateChannel => "create channel",
            PromptKind::JoinChannel => "join channel",
            PromptKind::DirectMessage => "direct message",
            PromptKind::SetRelay => "add community",
            PromptKind::SetTopic => "set topic",
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            PromptKind::CreateChannel => "channel name",
            PromptKind::JoinChannel => "channel uuid",
            PromptKind::DirectMessage => "npub or hex pubkey",
            PromptKind::SetRelay => "name wss://relay [https://gateway]",
            PromptKind::SetTopic => "what this channel is for",
        }
    }
}

/// A destructive action awaiting a yes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmation {
    DeleteMessage(String),
    LeaveChannel(String),
    InstallUpdate(Version),
    RestartUpdate(Version),
    Quit,
}

impl Confirmation {
    pub fn question(&self) -> String {
        match self {
            Confirmation::DeleteMessage(_) => "delete this message?".to_string(),
            Confirmation::LeaveChannel(name) => format!("leave {name}?"),
            Confirmation::InstallUpdate(version) => format!("install buzztui v{version}?"),
            Confirmation::RestartUpdate(version) => {
                format!("restart into buzztui v{version} now?")
            }
            Confirmation::Quit => "quit buzztui?".to_string(),
        }
    }

    pub fn detail(&self) -> &'static str {
        match self {
            Confirmation::DeleteMessage(_) => {
                "a deletion request is published; relays and other clients may keep copies"
            }
            Confirmation::LeaveChannel(_) => "you can rejoin an open channel at any time",
            Confirmation::InstallUpdate(_) => {
                "the matching GitHub archive is checksum-verified before replacing this executable"
            }
            Confirmation::RestartUpdate(_) => "the terminal session and relay connection restart",
            Confirmation::Quit => "unsent drafts are discarded",
        }
    }
}

/// The overlay currently on screen.
#[derive(Debug)]
pub enum Overlay {
    Help(Help),
    Picker(Picker),
    Prompt(Prompt),
    Confirm(Confirm),
    Search(Search),
    Profile(String),
    Image(String),
}

impl Overlay {
    /// Whether the overlay dims and covers the whole interface, which decides
    /// if the composer cursor should be hidden.
    pub fn is_modal(&self) -> bool {
        !matches!(self, Overlay::Image(_))
    }

    pub fn title(&self) -> String {
        match self {
            Overlay::Help(_) => "keybindings".to_string(),
            Overlay::Picker(picker) => picker.title.clone(),
            Overlay::Prompt(prompt) => prompt.kind.title().to_string(),
            Overlay::Confirm(_) => "confirm".to_string(),
            Overlay::Search(_) => "search".to_string(),
            Overlay::Profile(_) => "profile".to_string(),
            Overlay::Image(_) => "image".to_string(),
        }
    }

    pub fn handle(&mut self, key: KeyEvent) -> Outcome {
        match self {
            Overlay::Help(help) => help.handle(key),
            Overlay::Picker(picker) => picker.handle(key),
            Overlay::Prompt(prompt) => prompt.handle(key),
            Overlay::Confirm(confirm) => confirm.handle(key),
            Overlay::Search(search) => search.handle(key),
            Overlay::Profile(_) | Overlay::Image(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Outcome::Close,
                _ => Outcome::Consumed,
            },
        }
    }
}

// ------------------------------------------------------------------- filter

/// The incremental filter shared by every list overlay.
#[derive(Debug, Default)]
pub struct Query {
    pub text: String,
}

impl Query {
    /// Applies the ordinary editing keys a one-line filter is expected to
    /// support. Returns false when the key was not one of them.
    fn handle(&mut self, key: KeyEvent) -> bool {
        match (key.code, key.modifiers) {
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.text.clear();
                true
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                let trimmed = self.text.trim_end();
                let cut = token_start(trimmed);
                self.text.truncate(cut);
                true
            }
            (KeyCode::Backspace, _) => {
                self.text.pop();
                true
            }
            (KeyCode::Char(c), m)
                if !m.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.text.push(c);
                true
            }
            _ => false,
        }
    }
}

/// Scores `items` against `query`, keeping the original indices. An empty query
/// keeps everything in its natural order, which is what makes the switcher
/// useful before you have typed anything.
fn rank<'a, I>(matcher: &mut Matcher, query: &str, items: I) -> Vec<usize>
where
    I: IntoIterator<Item = &'a str>,
{
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return items.into_iter().enumerate().map(|(i, _)| i).collect();
    }
    let pattern = Pattern::parse(trimmed, CaseMatching::Ignore, Normalization::Smart);
    let mut buffer = Vec::new();
    let mut scored: Vec<(usize, u32)> = items
        .into_iter()
        .enumerate()
        .filter_map(|(index, haystack)| {
            pattern
                .score(Utf32Str::new(haystack, &mut buffer), matcher)
                .map(|score| (index, score))
        })
        .collect();
    // Ties keep source order so a list never reshuffles arbitrarily.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().map(|(index, _)| index).collect()
}

/// Moves a cursor within `len`, wrapping at both ends.
fn step(cursor: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as isize;
    (((cursor as isize + delta) % len) + len) as usize % len as usize
}

// ------------------------------------------------------------------- picker

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerKind {
    Channel,
    Community,
    Emoji,
    Mention,
    Command,
}

#[derive(Debug, Clone)]
pub struct PickerItem {
    /// What gets submitted.
    pub id: String,
    /// What is drawn and matched against.
    pub label: String,
    /// Secondary text, drawn dim.
    pub detail: Option<String>,
    /// A short marker such as an unread count.
    pub badge: Option<String>,
}

/// A fuzzy list. One widget serves the channel switcher and the reaction
/// picker, because they differ only in their contents.
pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub query: Query,
    pub items: Vec<PickerItem>,
    pub filtered: Vec<usize>,
    pub cursor: usize,
    matcher: Matcher,
}

impl std::fmt::Debug for Picker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Picker")
            .field("kind", &self.kind)
            .field("query", &self.query.text)
            .field("items", &self.items.len())
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl Picker {
    pub fn new(kind: PickerKind, title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            kind,
            title: title.into(),
            query: Query::default(),
            items,
            filtered,
            cursor: 0,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query.text = query.into();
        self.refilter();
        self
    }

    /// The reaction picker, seeded with the emoji people actually reach for.
    pub fn emoji() -> Self {
        let items = EMOJI
            .iter()
            .map(|(emoji, name)| PickerItem {
                id: (*emoji).to_string(),
                label: format!("{emoji}  {name}"),
                detail: None,
                badge: None,
            })
            .collect();
        Picker::new(PickerKind::Emoji, "add reaction", items)
    }

    pub fn accept_hint(&self) -> &'static str {
        match self.kind {
            PickerKind::Channel | PickerKind::Community => "open",
            PickerKind::Emoji => "react",
            PickerKind::Mention | PickerKind::Command => "insert",
        }
    }

    pub fn selection(&self) -> Option<&PickerItem> {
        self.filtered
            .get(self.cursor)
            .map(|&index| &self.items[index])
    }

    fn refilter(&mut self) {
        let labels: Vec<&str> = self.items.iter().map(|item| item.label.as_str()).collect();
        self.filtered = rank(&mut self.matcher, &self.query.text, labels);
        self.cursor = self.cursor.min(self.filtered.len().saturating_sub(1));
    }

    fn handle(&mut self, key: KeyEvent) -> Outcome {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => return Outcome::Close,
            (KeyCode::Enter, _) => {
                return match self.selection() {
                    Some(item) => Outcome::Submit(match self.kind {
                        PickerKind::Channel => Submission::OpenChannel(item.id.clone()),
                        PickerKind::Community => Submission::SwitchCommunity(item.id.clone()),
                        PickerKind::Emoji => Submission::React(item.id.clone()),
                        PickerKind::Mention => Submission::InsertMention(item.id.clone()),
                        PickerKind::Command => Submission::InsertCommand(item.id.clone()),
                    }),
                    // Submitting an empty list would silently do nothing, which
                    // reads as a broken key; closing is honest.
                    None => Outcome::Close,
                };
            }
            (KeyCode::Down, _)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL)
            | (KeyCode::Tab, _) => {
                self.cursor = step(self.cursor, self.filtered.len(), 1);
                return Outcome::Consumed;
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.cursor = step(self.cursor, self.filtered.len(), -1);
                return Outcome::Consumed;
            }
            (KeyCode::PageDown, _) => {
                self.cursor = (self.cursor + 10).min(self.filtered.len().saturating_sub(1));
                return Outcome::Consumed;
            }
            (KeyCode::PageUp, _) => {
                self.cursor = self.cursor.saturating_sub(10);
                return Outcome::Consumed;
            }
            (KeyCode::Home, _) => {
                self.cursor = 0;
                return Outcome::Consumed;
            }
            (KeyCode::End, _) => {
                self.cursor = self.filtered.len().saturating_sub(1);
                return Outcome::Consumed;
            }
            _ => {}
        }
        if self.query.handle(key) {
            self.refilter();
            return Outcome::Consumed;
        }
        Outcome::Ignored
    }
}

// ------------------------------------------------------------------- search

/// Message search. Results arrive from the local cache immediately and are
/// topped up by the relay's NIP-50 response, so the list is never empty while
/// the network is still thinking.
#[derive(Debug, Default)]
pub struct Search {
    pub query: Query,
    pub results: Vec<Message>,
    pub cursor: usize,
    /// Restricts the search to the channel that was open.
    pub scope: Option<String>,
    /// True until the relay has answered, so the empty state can say "searching"
    /// rather than "nothing found".
    pub waiting: bool,
    /// Set when the query changed and the caller should re-run the search.
    pub dirty: bool,
}

impl Search {
    pub fn new(scope: Option<String>) -> Self {
        Self {
            scope,
            ..Default::default()
        }
    }

    pub fn selection(&self) -> Option<&Message> {
        self.results.get(self.cursor)
    }

    pub fn set_results(&mut self, results: Vec<Message>) {
        self.results = results;
        self.cursor = self.cursor.min(self.results.len().saturating_sub(1));
        self.waiting = false;
    }

    fn handle(&mut self, key: KeyEvent) -> Outcome {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => return Outcome::Close,
            (KeyCode::Enter, _) => {
                return match self.selection() {
                    Some(message) => Outcome::Submit(Submission::JumpToMessage {
                        channel: message.channel.clone(),
                        id: message.id.clone(),
                    }),
                    None => Outcome::Consumed,
                };
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.cursor = step(self.cursor, self.results.len(), 1);
                return Outcome::Consumed;
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.cursor = step(self.cursor, self.results.len(), -1);
                return Outcome::Consumed;
            }
            _ => {}
        }
        if self.query.handle(key) {
            self.cursor = 0;
            self.dirty = true;
            self.waiting = true;
            return Outcome::Consumed;
        }
        Outcome::Ignored
    }
}

// ------------------------------------------------------------------- prompt

/// A single-line text prompt.
#[derive(Debug)]
pub struct Prompt {
    pub kind: PromptKind,
    pub value: String,
    /// Rejection text from the last submission attempt, shown under the field.
    pub error: Option<String>,
}

impl Prompt {
    pub fn new(kind: PromptKind) -> Self {
        Self {
            kind,
            value: String::new(),
            error: None,
        }
    }

    fn handle(&mut self, key: KeyEvent) -> Outcome {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => Outcome::Close,
            (KeyCode::Enter, _) => {
                let value = self.value.trim().to_string();
                if value.is_empty() {
                    self.error = Some("cannot be empty".to_string());
                    return Outcome::Consumed;
                }
                Outcome::Submit(Submission::Prompt {
                    kind: self.kind,
                    value,
                })
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.value.clear();
                self.error = None;
                Outcome::Consumed
            }
            (KeyCode::Backspace, _) => {
                self.value.pop();
                self.error = None;
                Outcome::Consumed
            }
            (KeyCode::Char(c), m)
                if !m.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.value.push(c);
                self.error = None;
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }
}

// ------------------------------------------------------------------ confirm

/// A yes-or-no gate in front of something irreversible. It defaults to no, and
/// `enter` follows the highlighted answer rather than always meaning yes.
#[derive(Debug)]
pub struct Confirm {
    pub action: Confirmation,
    pub yes: bool,
}

impl Confirm {
    pub fn new(action: Confirmation) -> Self {
        Self { action, yes: false }
    }

    fn handle(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Outcome::Close,
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                Outcome::Submit(Submission::Confirmed(self.action.clone()))
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Tab
            | KeyCode::Char('h')
            | KeyCode::Char('l') => {
                self.yes = !self.yes;
                Outcome::Consumed
            }
            KeyCode::Enter => {
                if self.yes {
                    Outcome::Submit(Submission::Confirmed(self.action.clone()))
                } else {
                    Outcome::Close
                }
            }
            _ => Outcome::Consumed,
        }
    }
}

// --------------------------------------------------------------------- help

/// The generated keybinding reference. Rows come from the live keymap, so a
/// rebind is reflected here without anyone editing documentation.
pub struct Help {
    pub query: Query,
    rows: Vec<(Group, crate::keys::HelpRow)>,
    pub filtered: Vec<usize>,
    pub scroll: usize,
    matcher: Matcher,
}

impl std::fmt::Debug for Help {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Help")
            .field("query", &self.query.text)
            .field("rows", &self.rows.len())
            .finish()
    }
}

impl Help {
    pub fn new(keymap: &Keymap) -> Self {
        let mut rows = Vec::new();
        for (group, entries) in keymap.help_rows() {
            for row in entries {
                rows.push((group, row));
            }
        }
        let filtered = (0..rows.len()).collect();
        Self {
            query: Query::default(),
            rows,
            filtered,
            scroll: 0,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// Visible rows, grouped, with the group heading attached to the first row
    /// of each run so the renderer can insert headings without regrouping.
    pub fn visible(&self) -> Vec<(Option<Group>, &crate::keys::HelpRow)> {
        let mut out = Vec::new();
        let mut current: Option<Group> = None;
        for &index in &self.filtered {
            let (group, row) = &self.rows[index];
            let heading = (current != Some(*group)).then_some(*group);
            current = Some(*group);
            out.push((heading, row));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    /// The widest key column, so keys can be right-padded into a clean gutter.
    pub fn key_width(&self) -> usize {
        self.filtered
            .iter()
            .map(|&i| self.rows[i].1.keys.chars().count())
            .max()
            .unwrap_or(0)
    }

    fn refilter(&mut self) {
        // Match against the key and the description together, so both "ctrl+k"
        // and "switcher" find the same row.
        let haystacks: Vec<String> = self
            .rows
            .iter()
            .map(|(group, row)| {
                format!(
                    "{} {} {} {}",
                    group.label(),
                    row.keys,
                    row.description,
                    row.action
                )
            })
            .collect();
        let refs: Vec<&str> = haystacks.iter().map(String::as_str).collect();
        self.filtered = rank(&mut self.matcher, &self.query.text, refs);
        self.scroll = 0;
    }

    fn handle(&mut self, key: KeyEvent) -> Outcome {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => return Outcome::Close,
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.scroll = (self.scroll + 1).min(self.filtered.len().saturating_sub(1));
                return Outcome::Consumed;
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.scroll = self.scroll.saturating_sub(1);
                return Outcome::Consumed;
            }
            (KeyCode::PageDown, _) => {
                self.scroll = (self.scroll + 10).min(self.filtered.len().saturating_sub(1));
                return Outcome::Consumed;
            }
            (KeyCode::PageUp, _) => {
                self.scroll = self.scroll.saturating_sub(10);
                return Outcome::Consumed;
            }
            _ => {}
        }
        if self.query.handle(key) {
            self.refilter();
            return Outcome::Consumed;
        }
        Outcome::Ignored
    }
}

/// Reactions worth one keystroke. Kept short on purpose: a picker with every
/// emoji in Unicode is slower than typing the one you want.
const EMOJI: &[(&str, &str)] = &[
    ("\u{1f44d}", "thumbs up yes lgtm"),
    ("\u{1f44e}", "thumbs down no"),
    ("\u{2705}", "check done fixed"),
    ("\u{274c}", "cross failed no"),
    ("\u{1f440}", "eyes looking reviewing"),
    ("\u{1f389}", "party ship celebrate"),
    ("\u{1f680}", "rocket ship deploy"),
    ("\u{1f525}", "fire hot great"),
    ("\u{2764}\u{fe0f}", "heart love"),
    ("\u{1f602}", "laugh funny"),
    ("\u{1f622}", "sad cry"),
    ("\u{1f621}", "angry"),
    ("\u{1f632}", "surprised wow"),
    ("\u{1f914}", "thinking hmm"),
    ("\u{1f64c}", "raised hands praise"),
    ("\u{1f647}", "bow sorry thanks"),
    ("\u{1f44f}", "clap applause"),
    ("\u{1f64f}", "please thanks pray"),
    ("\u{1f4af}", "hundred perfect"),
    ("\u{2b50}", "star favourite"),
    ("\u{26a0}\u{fe0f}", "warning careful"),
    ("\u{1f6d1}", "stop blocked"),
    ("\u{1f41b}", "bug broken"),
    ("\u{1f527}", "wrench fix"),
    ("\u{1f4dd}", "note docs"),
    ("\u{1f4a1}", "idea suggestion"),
    ("\u{1f6a2}", "ship shipit"),
    ("\u{1f440}\u{200d}\u{1f5e8}", "watching"),
    ("\u{1f37b}", "cheers beer"),
    ("\u{2615}", "coffee"),
    ("\u{1f9e0}", "brain smart"),
    ("\u{1f440}", "eyes"),
    ("\u{1f643}", "upside down oh no"),
    ("\u{1f480}", "skull dead"),
    ("\u{1f9ca}", "ice cold"),
    ("\u{1f421}", "fish"),
    ("\u{1f41d}", "bee buzz"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Keymap;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn channels() -> Vec<PickerItem> {
        ["general", "engineering", "design-review", "random"]
            .iter()
            .map(|name| PickerItem {
                id: format!("id-{name}"),
                label: (*name).to_string(),
                detail: None,
                badge: None,
            })
            .collect()
    }

    #[test]
    fn an_empty_query_keeps_every_item_in_source_order() {
        let picker = Picker::new(PickerKind::Channel, "jump", channels());
        assert_eq!(picker.filtered, vec![0, 1, 2, 3]);
        assert_eq!(picker.selection().unwrap().label, "general");
    }

    #[test]
    fn typing_filters_fuzzily_and_submitting_returns_the_id() {
        let mut picker = Picker::new(PickerKind::Channel, "jump", channels());
        for c in "dsgn".chars() {
            picker.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(picker.selection().unwrap().label, "design-review");
        assert_eq!(
            picker.handle(key(KeyCode::Enter)),
            Outcome::Submit(Submission::OpenChannel("id-design-review".into()))
        );
    }

    #[test]
    fn backspace_and_ctrl_u_restore_the_full_list() {
        let mut picker = Picker::new(PickerKind::Channel, "jump", channels());
        for c in "eng".chars() {
            picker.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(picker.filtered.len(), 1);
        picker.handle(key(KeyCode::Backspace));
        picker.handle(ctrl('u'));
        assert_eq!(picker.filtered.len(), 4);
        assert!(picker.query.text.is_empty());
    }

    #[test]
    fn the_cursor_wraps_at_both_ends() {
        let mut picker = Picker::new(PickerKind::Channel, "jump", channels());
        picker.handle(key(KeyCode::Up));
        assert_eq!(picker.selection().unwrap().label, "random");
        picker.handle(key(KeyCode::Down));
        assert_eq!(picker.selection().unwrap().label, "general");
    }

    #[test]
    fn a_filter_that_matches_nothing_submits_nothing() {
        let mut picker = Picker::new(PickerKind::Channel, "jump", channels());
        for c in "zzzz".chars() {
            picker.handle(key(KeyCode::Char(c)));
        }
        assert!(picker.filtered.is_empty());
        assert!(picker.selection().is_none());
        assert_eq!(picker.handle(key(KeyCode::Enter)), Outcome::Close);
    }

    #[test]
    fn the_emoji_picker_matches_on_names_not_just_glyphs() {
        let mut picker = Picker::emoji();
        for c in "lgtm".chars() {
            picker.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(
            picker.handle(key(KeyCode::Enter)),
            Outcome::Submit(Submission::React("\u{1f44d}".into()))
        );
    }

    #[test]
    fn completion_pickers_submit_the_value_the_composer_needs() {
        let item = PickerItem {
            id: "alice-pubkey".into(),
            label: "@Alice_Smith  Alice Smith".into(),
            detail: Some("member".into()),
            badge: None,
        };
        let mut mention = Picker::new(PickerKind::Mention, "mention", vec![item]).with_query("asm");
        assert_eq!(
            mention.handle(key(KeyCode::Enter)),
            Outcome::Submit(Submission::InsertMention("alice-pubkey".into()))
        );

        let item = PickerItem {
            id: "search".into(),
            label: "/search [query]".into(),
            detail: Some("search messages".into()),
            badge: None,
        };
        let mut command =
            Picker::new(PickerKind::Command, "command", vec![item]).with_query("srch");
        assert_eq!(
            command.handle(key(KeyCode::Enter)),
            Outcome::Submit(Submission::InsertCommand("search".into()))
        );
    }

    #[test]
    fn prompts_refuse_blank_input_without_closing() {
        let mut prompt = Prompt::new(PromptKind::CreateChannel);
        assert_eq!(prompt.handle(key(KeyCode::Enter)), Outcome::Consumed);
        assert!(prompt.error.is_some());

        for c in "  releases  ".chars() {
            prompt.handle(key(KeyCode::Char(c)));
        }
        assert!(prompt.error.is_none(), "typing must clear the error");
        assert_eq!(
            prompt.handle(key(KeyCode::Enter)),
            Outcome::Submit(Submission::Prompt {
                kind: PromptKind::CreateChannel,
                value: "releases".into()
            }),
            "the submitted value must be trimmed"
        );
    }

    #[test]
    fn confirmations_default_to_no() {
        let mut confirm = Confirm::new(Confirmation::Quit);
        assert!(!confirm.yes);
        // Enter on the default answer must not perform the action.
        assert_eq!(confirm.handle(key(KeyCode::Enter)), Outcome::Close);

        let mut confirm = Confirm::new(Confirmation::Quit);
        confirm.handle(key(KeyCode::Left));
        assert_eq!(
            confirm.handle(key(KeyCode::Enter)),
            Outcome::Submit(Submission::Confirmed(Confirmation::Quit))
        );
    }

    #[test]
    fn y_and_n_answer_a_confirmation_directly() {
        let mut confirm = Confirm::new(Confirmation::LeaveChannel("general".into()));
        assert_eq!(
            confirm.handle(key(KeyCode::Char('y'))),
            Outcome::Submit(Submission::Confirmed(Confirmation::LeaveChannel(
                "general".into()
            )))
        );
        let mut confirm = Confirm::new(Confirmation::Quit);
        assert_eq!(confirm.handle(key(KeyCode::Char('n'))), Outcome::Close);
    }

    #[test]
    fn help_is_generated_from_the_live_keymap() {
        let file: crate::keys::KeyFile = toml::from_str(
            r#"
            [navigate]
            reply = "ctrl+r"
            "#,
        )
        .unwrap();
        let keymap = Keymap::with_overrides(file);
        let help = Help::new(&keymap);
        let rows = help.visible();
        assert!(
            rows.iter().any(|(_, row)| row.keys == "ctrl+r"),
            "the rebound key must appear in help"
        );
        assert!(
            !rows
                .iter()
                .any(|(_, row)| row.keys == "r" && row.description.contains("reply")),
            "the replaced default must not linger"
        );
        assert!(
            rows.iter().any(|(_, row)| row.action == "reply"),
            "the config name must be shown so the binding can be changed"
        );
    }

    #[test]
    fn help_filters_on_both_the_key_and_the_description() {
        let keymap = Keymap::builtin();
        let mut help = Help::new(&keymap);
        for c in "reaction".chars() {
            help.handle(key(KeyCode::Char(c)));
        }
        assert!(!help.is_empty());
        assert_eq!(
            help.visible()[0].1.action,
            "react",
            "the best match must rank first"
        );

        help.handle(ctrl('u'));
        for c in "ctrlk".chars() {
            help.handle(key(KeyCode::Char(c)));
        }
        // `ctrl+k` and `ctrl+b k` both open the switcher, so assert the action
        // rather than picking one of two equally correct chords.
        assert_eq!(
            help.visible()[0].1.action,
            "switcher",
            "searching by chord must work too"
        );
    }

    #[test]
    fn help_reports_an_empty_filter_rather_than_showing_stale_rows() {
        let mut help = Help::new(&Keymap::builtin());
        for c in "qqqqqq".chars() {
            help.handle(key(KeyCode::Char(c)));
        }
        assert!(help.is_empty());
        assert_eq!(help.key_width(), 0);
    }

    #[test]
    fn help_groups_rows_and_marks_each_heading_once() {
        let help = Help::new(&Keymap::builtin());
        let rows = help.visible();
        let headings: Vec<Group> = rows.iter().filter_map(|(h, _)| *h).collect();
        let mut unique = headings.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            headings.len(),
            unique.len(),
            "each group heading must appear exactly once"
        );
    }

    #[test]
    fn search_marks_itself_dirty_when_the_query_changes() {
        let mut search = Search::new(Some("room".into()));
        assert!(!search.dirty);
        search.handle(key(KeyCode::Char('a')));
        assert!(search.dirty && search.waiting);

        search.set_results(Vec::new());
        assert!(!search.waiting, "an answer clears the waiting state");
    }

    #[test]
    fn escape_always_closes_every_overlay() {
        let keymap = Keymap::builtin();
        let mut overlays = vec![
            Overlay::Help(Help::new(&keymap)),
            Overlay::Picker(Picker::emoji()),
            Overlay::Prompt(Prompt::new(PromptKind::JoinChannel)),
            Overlay::Confirm(Confirm::new(Confirmation::Quit)),
            Overlay::Search(Search::new(None)),
            Overlay::Profile("abc".into()),
            Overlay::Image("http://x/y.png".into()),
        ];
        for overlay in &mut overlays {
            assert_eq!(
                overlay.handle(key(KeyCode::Esc)),
                Outcome::Close,
                "{} did not close on esc",
                overlay.title()
            );
        }
    }
}
