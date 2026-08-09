//! Keybindings.
//!
//! Every keystroke resolves to a named [`Action`] through a [`Keymap`], and
//! nothing in the interface reads a raw key code. That indirection buys three
//! things at once: the whole binding table can be rebound from
//! `~/.config/buzztui/keys.toml`, the help overlay and the which-key hint bar
//! are generated from the live map rather than from hand-written strings that
//! rot, and a rebind is reflected everywhere the moment it is loaded.
//!
//! Bindings are sequences, not single chords, so a leader key can open a whole
//! namespace without stealing a chord from the composer.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// Where a binding applies. Resolution walks the active scope, then `Global`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Active regardless of mode; must use a modified chord so that it cannot
    /// swallow ordinary typing.
    Global,
    /// The composer holds the cursor and unmodified keys insert text.
    Insert,
    /// The timeline holds the cursor and unmodified keys navigate.
    Normal,
    /// Reached by pressing the leader; the next chord completes the command.
    Leader,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Insert => "compose",
            Scope::Normal => "navigate",
            Scope::Leader => "leader",
        }
    }
}

/// Grouping used to lay out the help overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Navigation,
    Messages,
    Compose,
    Channels,
    View,
    Application,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Navigation => "navigation",
            Group::Messages => "messages",
            Group::Compose => "compose",
            Group::Channels => "channels",
            Group::View => "view",
            Group::Application => "application",
        }
    }
}

/// Everything the interface can be asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // navigation
    FocusComposer,
    FocusTimeline,
    FocusSidebar,
    CycleFocus,
    CycleFocusBack,
    NextChannel,
    PrevChannel,
    NextUnread,
    PrevUnread,
    JumpChannel(u8),
    SelectNext,
    SelectPrev,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    ScrollTop,
    ScrollBottom,
    JumpFirstUnread,

    // messages
    Send,
    Reply,
    EditMessage,
    DeleteMessage,
    React,
    CopyMessage,
    QuoteMessage,
    OpenThread,
    OpenLink,
    ViewImage,
    RetrySend,
    DiscardFailed,

    // compose editing
    Newline,
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    DeleteBack,
    DeleteForward,
    DeleteWordBack,
    DeleteWordForward,
    KillToEnd,
    KillToStart,
    Undo,
    Paste,
    HistoryPrev,
    HistoryNext,
    Complete,

    // channels
    CreateChannel,
    JoinChannel,
    LeaveChannel,
    ToggleMute,
    TogglePin,
    MarkRead,
    MarkAllRead,
    OpenSwitcher,
    OpenSearch,
    OpenMembers,
    OpenProfile,
    OpenDirectMessage,
    CopyIdentity,

    // view
    ToggleSidebar,
    ToggleMemberPane,
    ToggleImages,
    ToggleCompact,
    ToggleTimestamps,
    CycleTheme,
    Redraw,

    // application
    OpenHelp,
    OpenCommand,
    ReloadConfig,
    Cancel,
    Quit,
}

/// Static metadata for every bindable action: the config name, the help text,
/// and the group it appears under. This table is the single source of truth
/// for the config parser, the help overlay, and the hint bar.
struct Spec {
    action: Action,
    name: &'static str,
    group: Group,
    help: &'static str,
}

/// Wording is lowercase throughout; mixed casing reads as a different program.
const SPECS: &[Spec] = &[
    spec(
        Action::FocusComposer,
        "focus_composer",
        Group::Navigation,
        "write a message",
    ),
    spec(
        Action::FocusTimeline,
        "focus_timeline",
        Group::Navigation,
        "browse messages",
    ),
    spec(
        Action::FocusSidebar,
        "focus_sidebar",
        Group::Navigation,
        "browse channels",
    ),
    spec(
        Action::CycleFocus,
        "cycle_focus",
        Group::Navigation,
        "next pane",
    ),
    spec(
        Action::CycleFocusBack,
        "cycle_focus_back",
        Group::Navigation,
        "previous pane",
    ),
    spec(
        Action::NextChannel,
        "next_channel",
        Group::Navigation,
        "next channel",
    ),
    spec(
        Action::PrevChannel,
        "previous_channel",
        Group::Navigation,
        "previous channel",
    ),
    spec(
        Action::NextUnread,
        "next_unread",
        Group::Navigation,
        "next unread channel",
    ),
    spec(
        Action::PrevUnread,
        "previous_unread",
        Group::Navigation,
        "previous unread channel",
    ),
    spec(
        Action::SelectNext,
        "select_next",
        Group::Navigation,
        "select next message",
    ),
    spec(
        Action::SelectPrev,
        "select_previous",
        Group::Navigation,
        "select previous message",
    ),
    spec(
        Action::ScrollUp,
        "scroll_up",
        Group::Navigation,
        "scroll up",
    ),
    spec(
        Action::ScrollDown,
        "scroll_down",
        Group::Navigation,
        "scroll down",
    ),
    spec(Action::PageUp, "page_up", Group::Navigation, "page up"),
    spec(
        Action::PageDown,
        "page_down",
        Group::Navigation,
        "page down",
    ),
    spec(
        Action::ScrollTop,
        "scroll_top",
        Group::Navigation,
        "jump to oldest",
    ),
    spec(
        Action::ScrollBottom,
        "scroll_bottom",
        Group::Navigation,
        "jump to newest",
    ),
    spec(
        Action::JumpFirstUnread,
        "jump_first_unread",
        Group::Navigation,
        "jump to first unread",
    ),
    spec(Action::Send, "send", Group::Messages, "send message"),
    spec(Action::Reply, "reply", Group::Messages, "reply in thread"),
    spec(
        Action::EditMessage,
        "edit_message",
        Group::Messages,
        "edit message",
    ),
    spec(
        Action::DeleteMessage,
        "delete_message",
        Group::Messages,
        "delete message",
    ),
    spec(Action::React, "react", Group::Messages, "add reaction"),
    spec(
        Action::CopyMessage,
        "copy_message",
        Group::Messages,
        "copy message text",
    ),
    spec(
        Action::QuoteMessage,
        "quote_message",
        Group::Messages,
        "quote into composer",
    ),
    spec(
        Action::OpenThread,
        "open_thread",
        Group::Messages,
        "open thread",
    ),
    spec(
        Action::OpenLink,
        "open_link",
        Group::Messages,
        "open first link",
    ),
    spec(
        Action::ViewImage,
        "view_image",
        Group::Messages,
        "view attached image",
    ),
    spec(
        Action::RetrySend,
        "retry_send",
        Group::Messages,
        "resend failed message",
    ),
    spec(
        Action::DiscardFailed,
        "discard_failed",
        Group::Messages,
        "discard failed message",
    ),
    spec(
        Action::Newline,
        "newline",
        Group::Compose,
        "insert a line break",
    ),
    spec(
        Action::CursorLeft,
        "cursor_left",
        Group::Compose,
        "cursor left",
    ),
    spec(
        Action::CursorRight,
        "cursor_right",
        Group::Compose,
        "cursor right",
    ),
    spec(Action::CursorUp, "cursor_up", Group::Compose, "cursor up"),
    spec(
        Action::CursorDown,
        "cursor_down",
        Group::Compose,
        "cursor down",
    ),
    spec(
        Action::WordLeft,
        "word_left",
        Group::Compose,
        "back one word",
    ),
    spec(
        Action::WordRight,
        "word_right",
        Group::Compose,
        "forward one word",
    ),
    spec(
        Action::LineStart,
        "line_start",
        Group::Compose,
        "start of line",
    ),
    spec(Action::LineEnd, "line_end", Group::Compose, "end of line"),
    spec(
        Action::DeleteBack,
        "delete_back",
        Group::Compose,
        "delete before cursor",
    ),
    spec(
        Action::DeleteForward,
        "delete_forward",
        Group::Compose,
        "delete after cursor",
    ),
    spec(
        Action::DeleteWordBack,
        "delete_word_back",
        Group::Compose,
        "delete word before",
    ),
    spec(
        Action::DeleteWordForward,
        "delete_word_forward",
        Group::Compose,
        "delete word after",
    ),
    spec(
        Action::KillToEnd,
        "kill_to_end",
        Group::Compose,
        "delete to end of line",
    ),
    spec(
        Action::KillToStart,
        "kill_to_start",
        Group::Compose,
        "delete to start of line",
    ),
    spec(Action::Undo, "undo", Group::Compose, "undo"),
    spec(
        Action::Paste,
        "paste",
        Group::Compose,
        "paste from clipboard",
    ),
    spec(
        Action::HistoryPrev,
        "history_previous",
        Group::Compose,
        "previous sent message",
    ),
    spec(
        Action::HistoryNext,
        "history_next",
        Group::Compose,
        "next sent message",
    ),
    spec(
        Action::Complete,
        "complete",
        Group::Compose,
        "complete mention or emoji",
    ),
    spec(
        Action::CreateChannel,
        "create_channel",
        Group::Channels,
        "create a channel",
    ),
    spec(
        Action::JoinChannel,
        "join_channel",
        Group::Channels,
        "join a channel",
    ),
    spec(
        Action::LeaveChannel,
        "leave_channel",
        Group::Channels,
        "leave this channel",
    ),
    spec(
        Action::ToggleMute,
        "toggle_mute",
        Group::Channels,
        "mute this channel",
    ),
    spec(
        Action::TogglePin,
        "toggle_pin",
        Group::Channels,
        "pin this channel",
    ),
    spec(
        Action::MarkRead,
        "mark_read",
        Group::Channels,
        "mark channel read",
    ),
    spec(
        Action::MarkAllRead,
        "mark_all_read",
        Group::Channels,
        "mark everything read",
    ),
    spec(
        Action::OpenSwitcher,
        "switcher",
        Group::Channels,
        "jump to a channel",
    ),
    spec(
        Action::OpenSearch,
        "search",
        Group::Channels,
        "search messages",
    ),
    spec(
        Action::OpenMembers,
        "members",
        Group::Channels,
        "show members",
    ),
    spec(
        Action::OpenProfile,
        "profile",
        Group::Channels,
        "show author profile",
    ),
    spec(
        Action::CopyIdentity,
        "copy_identity",
        Group::Application,
        "copy your public key",
    ),
    spec(
        Action::OpenDirectMessage,
        "direct_message",
        Group::Channels,
        "start a direct message",
    ),
    spec(
        Action::ToggleSidebar,
        "toggle_sidebar",
        Group::View,
        "show or hide the sidebar",
    ),
    spec(
        Action::ToggleMemberPane,
        "toggle_member_pane",
        Group::View,
        "show or hide members",
    ),
    spec(
        Action::ToggleImages,
        "toggle_images",
        Group::View,
        "show or hide inline images",
    ),
    spec(
        Action::ToggleCompact,
        "toggle_compact",
        Group::View,
        "compact message spacing",
    ),
    spec(
        Action::ToggleTimestamps,
        "toggle_timestamps",
        Group::View,
        "show or hide timestamps",
    ),
    spec(Action::CycleTheme, "cycle_theme", Group::View, "next theme"),
    spec(Action::Redraw, "redraw", Group::View, "redraw the screen"),
    spec(
        Action::OpenHelp,
        "help",
        Group::Application,
        "show keybindings",
    ),
    spec(
        Action::OpenCommand,
        "command",
        Group::Application,
        "run a command",
    ),
    spec(
        Action::ReloadConfig,
        "reload_config",
        Group::Application,
        "reload configuration",
    ),
    spec(
        Action::Cancel,
        "cancel",
        Group::Application,
        "cancel or close",
    ),
    spec(Action::Quit, "quit", Group::Application, "quit buzztui"),
];

const fn spec(action: Action, name: &'static str, group: Group, help: &'static str) -> Spec {
    Spec {
        action,
        name,
        group,
        help,
    }
}

impl Action {
    /// The name used in `keys.toml`.
    pub fn name(self) -> String {
        match self {
            Action::JumpChannel(_) => "jump_channel".to_string(),
            other => SPECS
                .iter()
                .find(|s| s.action == other)
                .map(|s| s.name.to_string())
                .unwrap_or_default(),
        }
    }

    pub fn group(self) -> Group {
        match self {
            Action::JumpChannel(_) => Group::Navigation,
            other => SPECS
                .iter()
                .find(|s| s.action == other)
                .map(|s| s.group)
                .unwrap_or(Group::Application),
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Action::JumpChannel(_) => "jump to channel by position",
            other => SPECS
                .iter()
                .find(|s| s.action == other)
                .map(|s| s.help)
                .unwrap_or(""),
        }
    }

    fn from_name(name: &str) -> Option<Action> {
        SPECS.iter().find(|s| s.name == name).map(|s| s.action)
    }
}

/// One key press: a code plus its modifiers, normalised so that the same
/// physical press always compares equal however the terminal reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

/// Crossterm's key types are not `Ord`, but the binding table needs a stable
/// order for display. Rank by modifiers, then by a synthetic code ordinal that
/// keeps letters together and named keys after them.
impl Ord for Chord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.mods.bits(), code_rank(self.code)).cmp(&(other.mods.bits(), code_rank(other.code)))
    }
}

impl PartialOrd for Chord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn code_rank(code: KeyCode) -> u32 {
    match code {
        KeyCode::Char(c) => c as u32,
        KeyCode::F(n) => 0x0010_0000 + n as u32,
        KeyCode::Enter => 0x0020_0000,
        KeyCode::Esc => 0x0020_0001,
        KeyCode::Tab => 0x0020_0002,
        KeyCode::BackTab => 0x0020_0003,
        KeyCode::Backspace => 0x0020_0004,
        KeyCode::Delete => 0x0020_0005,
        KeyCode::Insert => 0x0020_0006,
        KeyCode::Home => 0x0020_0007,
        KeyCode::End => 0x0020_0008,
        KeyCode::PageUp => 0x0020_0009,
        KeyCode::PageDown => 0x0020_000a,
        KeyCode::Up => 0x0020_000b,
        KeyCode::Down => 0x0020_000c,
        KeyCode::Left => 0x0020_000d,
        KeyCode::Right => 0x0020_000e,
        _ => 0x0030_0000,
    }
}

impl Chord {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        Self { code, mods }.normalised()
    }

    /// Terminals disagree about shift. Some send `shift+k`, others send `K`;
    /// shift-tab arrives as `BackTab` on many and as `shift+Tab` on others.
    /// Collapse both spellings so a binding written either way matches.
    fn normalised(mut self) -> Self {
        if self.code == KeyCode::BackTab {
            self.code = KeyCode::Tab;
            self.mods |= KeyModifiers::SHIFT;
        }
        if let KeyCode::Char(c) = self.code {
            if self.mods.contains(KeyModifiers::SHIFT) && c.is_alphabetic() {
                // `shift+k` and `K` are the same press.
                self.code = KeyCode::Char(c.to_ascii_uppercase());
                self.mods.remove(KeyModifiers::SHIFT);
            } else if c.is_ascii_uppercase() {
                self.mods.remove(KeyModifiers::SHIFT);
            }
            // Control chords are case-insensitive: ctrl+a and ctrl+A are one key.
            if self.mods.contains(KeyModifiers::CONTROL) {
                self.code = KeyCode::Char(c.to_ascii_lowercase());
            }
        }
        // Only the four modifiers a binding can name are significant.
        self.mods &=
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER;
        self
    }

    pub fn from_event(event: KeyEvent) -> Self {
        Chord::new(event.code, event.modifiers)
    }

    /// True when this chord would otherwise type a character, which makes it
    /// unsafe to bind directly in a scope that accepts text.
    pub fn is_printable(self) -> bool {
        matches!(self.code, KeyCode::Char(_))
            && !self
                .mods
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.is_empty() {
            bail!("empty key");
        }
        let mut mods = KeyModifiers::NONE;
        // Split on '+' but keep a trailing literal '+' as the key itself.
        let mut parts: Vec<&str> = text.split('+').collect();
        if text.ends_with("++") || text == "+" {
            parts = vec!["+"];
            for part in text.trim_end_matches("++").split('+') {
                if !part.is_empty() {
                    parts.insert(parts.len() - 1, part);
                }
            }
        }
        let key = parts.pop().unwrap_or_default();
        for modifier in parts {
            match modifier.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "c" => mods |= KeyModifiers::CONTROL,
                "alt" | "option" | "meta" | "m" => mods |= KeyModifiers::ALT,
                "shift" | "s" => mods |= KeyModifiers::SHIFT,
                "super" | "cmd" | "win" => mods |= KeyModifiers::SUPER,
                other => bail!("unknown modifier `{other}` in `{text}`"),
            }
        }

        let lower = key.trim().to_ascii_lowercase();
        let code = match lower.as_str() {
            "enter" | "return" | "cr" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => {
                mods |= KeyModifiers::SHIFT;
                KeyCode::Tab
            }
            "space" => KeyCode::Char(' '),
            "backspace" | "bs" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "minus" | "dash" => KeyCode::Char('-'),
            "plus" => KeyCode::Char('+'),
            "equals" | "equal" => KeyCode::Char('='),
            "comma" => KeyCode::Char(','),
            "period" | "dot" => KeyCode::Char('.'),
            "slash" => KeyCode::Char('/'),
            "backslash" => KeyCode::Char('\\'),
            "quote" => KeyCode::Char('\''),
            "semicolon" => KeyCode::Char(';'),
            "colon" => KeyCode::Char(':'),
            "backtick" | "grave" => KeyCode::Char('`'),
            "question" => KeyCode::Char('?'),
            "bang" | "exclamation" => KeyCode::Char('!'),
            "at" => KeyCode::Char('@'),
            "hash" => KeyCode::Char('#'),
            "percent" => KeyCode::Char('%'),
            "ampersand" => KeyCode::Char('&'),
            "star" | "asterisk" => KeyCode::Char('*'),
            "lbracket" => KeyCode::Char('['),
            "rbracket" => KeyCode::Char(']'),
            other => {
                if let Some(number) = other.strip_prefix('f')
                    && let Ok(n) = number.parse::<u8>()
                    && (1..=12).contains(&n)
                {
                    return Ok(Chord::new(KeyCode::F(n), mods));
                }
                // A bare character keeps the case the user wrote, so `K` and
                // `shift+k` normalise to the same chord.
                let mut chars = key.trim().chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => KeyCode::Char(c),
                    _ => bail!("unknown key `{key}`"),
                }
            }
        };
        Ok(Chord::new(code, mods))
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.contains(KeyModifiers::CONTROL) {
            f.write_str("ctrl+")?;
        }
        if self.mods.contains(KeyModifiers::ALT) {
            f.write_str("alt+")?;
        }
        if self.mods.contains(KeyModifiers::SUPER) {
            f.write_str("super+")?;
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            f.write_str("shift+")?;
        }
        match self.code {
            KeyCode::Enter => f.write_str("enter"),
            KeyCode::Esc => f.write_str("esc"),
            KeyCode::Tab => f.write_str("tab"),
            KeyCode::Backspace => f.write_str("backspace"),
            KeyCode::Delete => f.write_str("del"),
            KeyCode::Insert => f.write_str("ins"),
            KeyCode::Home => f.write_str("home"),
            KeyCode::End => f.write_str("end"),
            KeyCode::PageUp => f.write_str("pgup"),
            KeyCode::PageDown => f.write_str("pgdn"),
            KeyCode::Up => f.write_str("up"),
            KeyCode::Down => f.write_str("down"),
            KeyCode::Left => f.write_str("left"),
            KeyCode::Right => f.write_str("right"),
            KeyCode::F(n) => write!(f, "f{n}"),
            KeyCode::Char(' ') => f.write_str("space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// A binding is one or more chords pressed in order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sequence(pub Vec<Chord>);

impl Sequence {
    pub fn starts_with(&self, prefix: &[Chord]) -> bool {
        self.0.len() > prefix.len() && self.0.starts_with(prefix)
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, chord) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{chord}")?;
        }
        Ok(())
    }
}

/// One row of the binding table.
#[derive(Debug, Clone)]
pub struct Binding {
    pub sequence: Sequence,
    pub action: Action,
    pub scope: Scope,
    /// Declaration order. The first binding a user or the defaults list for an
    /// action is the one shown in hints, so `i` beats `a` for the composer even
    /// though `a` sorts first.
    order: usize,
}

/// The outcome of feeding a chord to the keymap.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// Nothing is bound to this chord in this scope.
    Unbound,
    /// A complete binding fired.
    Run(Action),
    /// A prefix matched; these are the continuations to show in the hint bar.
    Pending(Vec<(Chord, Action)>),
}

/// The complete binding table plus any diagnostics raised while loading it.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: Vec<Binding>,
    pub leader: Chord,
    /// Non-fatal configuration problems, surfaced as an in-band banner rather
    /// than by refusing to start.
    pub diagnostics: Vec<String>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Keymap {
    /// The shipped defaults: readline chords in the composer, vi-flavoured
    /// motions when navigating, and a leader for everything infrequent.
    pub fn builtin() -> Self {
        let leader = Chord::parse("ctrl+b").expect("valid leader");
        let mut map = Keymap {
            bindings: Vec::new(),
            leader,
            diagnostics: Vec::new(),
        };
        for (scope, keys, action) in DEFAULTS {
            for key in *keys {
                let sequence = map.parse_sequence(key).expect("valid default binding");
                let order = map.bindings.len();
                map.bindings.push(Binding {
                    sequence,
                    action: *action,
                    scope: *scope,
                    order,
                });
            }
        }
        for index in 1..=9u8 {
            let sequence = map
                .parse_sequence(&format!("leader+{index}"))
                .expect("valid default binding");
            let order = map.bindings.len();
            map.bindings.push(Binding {
                sequence,
                action: Action::JumpChannel(index),
                scope: Scope::Leader,
                order,
            });
        }
        map
    }

    /// Parses `ctrl+b c` or `leader+c` into a chord sequence. `leader+` expands
    /// to the configured leader chord followed by the rest.
    fn parse_sequence(&self, text: &str) -> Result<Sequence> {
        let mut chords = Vec::new();
        for token in text.split_whitespace() {
            match token.strip_prefix("leader+") {
                Some(rest) => {
                    chords.push(self.leader);
                    chords.push(Chord::parse(rest)?);
                }
                None if token == "leader" => chords.push(self.leader),
                None => chords.push(Chord::parse(token)?),
            }
        }
        if chords.is_empty() {
            bail!("empty key sequence");
        }
        Ok(Sequence(chords))
    }

    /// Applies user overrides on top of the defaults. A configured action
    /// replaces every default binding for that action, which is what a user
    /// writing `send = "ctrl+enter"` expects.
    pub fn with_overrides(file: KeyFile) -> Self {
        let mut map = Keymap::builtin();
        if let Some(leader) = file.leader.as_deref() {
            match Chord::parse(leader) {
                Ok(chord) if chord.is_printable() => map.diagnostics.push(format!(
                    "leader `{leader}` is an unmodified printable key and would swallow typing; keeping {}",
                    map.leader
                )),
                Ok(chord) => {
                    let previous = map.leader;
                    map.leader = chord;
                    // Rewrite defaults that were expressed relative to the leader.
                    for binding in &mut map.bindings {
                        if binding.scope == Scope::Leader
                            && binding.sequence.0.first() == Some(&previous)
                        {
                            binding.sequence.0[0] = chord;
                        }
                    }
                }
                Err(err) => map.diagnostics.push(format!("leader: {err}")),
            }
        }

        for (scope, table) in [
            (Scope::Global, &file.global),
            (Scope::Insert, &file.compose),
            (Scope::Normal, &file.navigate),
            (Scope::Leader, &file.leader_keys),
        ] {
            for (name, binding) in table {
                map.apply_override(scope, name, binding);
            }
        }

        map.bindings
            .sort_by(|a, b| a.scope.cmp(&b.scope).then_with(|| a.order.cmp(&b.order)));
        map
    }

    fn apply_override(&mut self, scope: Scope, name: &str, binding: &KeyBinding) {
        // `jump_channel = "leader+1..9"` expands into nine indexed bindings and
        // is collapsed again for display.
        if name == "jump_channel" {
            self.bindings
                .retain(|b| !matches!(b.action, Action::JumpChannel(_)));
            for key in binding.keys() {
                match key.split_once("1..9") {
                    Some((prefix, suffix)) => {
                        for index in 1..=9u8 {
                            let spelled = format!("{prefix}{index}{suffix}");
                            match self.parse_sequence(&spelled) {
                                Ok(sequence) => {
                                    let order = self.bindings.len();
                                    self.bindings.push(Binding {
                                        sequence,
                                        action: Action::JumpChannel(index),
                                        scope,
                                        order,
                                    })
                                }
                                Err(err) => self.diagnostics.push(format!("jump_channel: {err}")),
                            }
                        }
                    }
                    None => self
                        .diagnostics
                        .push(format!("jump_channel: `{key}` must contain `1..9`")),
                }
            }
            return;
        }

        let Some(action) = Action::from_name(name) else {
            self.diagnostics
                .push(format!("unknown action `{name}` under [{}]", scope.label()));
            return;
        };

        let mut parsed = Vec::new();
        for key in binding.keys() {
            match self.parse_sequence(key) {
                Ok(sequence) => {
                    // Binding a bare printable key in a scope that accepts text
                    // would make that character untypable. Refuse rather than
                    // hand the user a composer that cannot type `q`.
                    if scope == Scope::Insert && sequence.0[0].is_printable() {
                        self.diagnostics.push(format!(
                            "{name}: `{key}` is an unmodified printable key in the composer; use a modifier or the leader"
                        ));
                        continue;
                    }
                    if scope == Scope::Global && sequence.0[0].is_printable() {
                        self.diagnostics.push(format!(
                            "{name}: `{key}` is global and unmodified, so it would swallow typing; bind it under [navigate] instead"
                        ));
                        continue;
                    }
                    parsed.push(sequence);
                }
                Err(err) => self.diagnostics.push(format!("{name}: {err}")),
            }
        }
        if parsed.is_empty() && !binding.keys().is_empty() {
            return;
        }

        self.bindings
            .retain(|b| !(b.action == action && b.scope == scope));
        for sequence in parsed {
            let order = self.bindings.len();
            self.bindings.push(Binding {
                sequence,
                action,
                scope,
                order,
            });
        }
    }

    /// Resolves a chord within a scope, given the chords already pressed.
    ///
    /// Scope governs the continuation search as strictly as it governs an exact
    /// match. Without that, a multi-chord binding in one mode would turn its
    /// first key into a dead prefix in every other mode, and the composer would
    /// silently refuse to type letters like `g`.
    pub fn resolve(&self, scope: Scope, pending: &[Chord], chord: Chord) -> Resolution {
        let mut sequence: Vec<Chord> = pending.to_vec();
        sequence.push(chord);

        let active: &[Scope] = match scope {
            Scope::Insert => &[Scope::Insert, Scope::Global],
            Scope::Normal => &[Scope::Normal, Scope::Global],
            Scope::Global => &[Scope::Global],
            Scope::Leader => &[Scope::Leader],
        };
        // A sequence that opens with the leader belongs to the leader
        // namespace whatever mode it was pressed in; anything else stays in the
        // mode that started it.
        let scopes: &[Scope] = if sequence.first() == Some(&self.leader) {
            &[Scope::Leader, Scope::Global]
        } else {
            active
        };

        for candidate in self.bindings.iter().filter(|b| scopes.contains(&b.scope)) {
            if candidate.sequence.0 == sequence {
                return Resolution::Run(candidate.action);
            }
        }

        let continuations: Vec<(Chord, Action)> = self
            .bindings
            .iter()
            .filter(|b| scopes.contains(&b.scope) && b.sequence.starts_with(&sequence))
            .filter_map(|b| b.sequence.0.get(sequence.len()).map(|c| (*c, b.action)))
            .collect();

        if continuations.is_empty() {
            Resolution::Unbound
        } else {
            Resolution::Pending(continuations)
        }
    }

    /// Every binding for an action in a scope, formatted for display. Indexed
    /// jumps collapse back into a single `leader+1..9` row.
    pub fn keys_for(&self, action: Action) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|b| b.action == action)
            .map(|b| b.sequence.to_string())
            .collect()
    }

    /// The first binding for an action, used in hint text and empty states.
    pub fn hint(&self, action: Action) -> String {
        self.keys_for(action)
            .into_iter()
            .next()
            .unwrap_or_else(|| "unset".to_string())
    }

    /// Bindings grouped for the help overlay, in a stable order.
    pub fn help_rows(&self) -> BTreeMap<Group, Vec<HelpRow>> {
        let mut grouped: BTreeMap<Group, Vec<HelpRow>> = BTreeMap::new();
        let mut jumps: Vec<String> = Vec::new();

        for binding in &self.bindings {
            if let Action::JumpChannel(index) = binding.action {
                if index == 1 {
                    // Collapse the nine indexed bindings back to one row.
                    let text = binding.sequence.to_string();
                    jumps.push(text.replace('1', "1..9"));
                }
                continue;
            }
            grouped
                .entry(binding.action.group())
                .or_default()
                .push(HelpRow {
                    keys: binding.sequence.to_string(),
                    description: binding.action.help(),
                    action: binding.action.name(),
                });
        }

        for jump in jumps {
            grouped.entry(Group::Navigation).or_default().push(HelpRow {
                keys: jump,
                description: "jump to channel by position",
                action: "jump_channel".to_string(),
            });
        }
        for rows in grouped.values_mut() {
            rows.sort_by(|a, b| {
                a.description
                    .cmp(b.description)
                    .then_with(|| a.keys.cmp(&b.keys))
            });
            rows.dedup();
        }
        grouped
    }
}

/// One row of the generated keybinding reference. `action` is the name to write
/// in `keys.toml`, so the overlay teaches rebinding rather than merely listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpRow {
    pub keys: String,
    pub description: &'static str,
    pub action: String,
}

/// `keys.toml`, deserialised. Every table is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeyFile {
    pub leader: Option<String>,
    pub global: BTreeMap<String, KeyBinding>,
    pub compose: BTreeMap<String, KeyBinding>,
    pub navigate: BTreeMap<String, KeyBinding>,
    #[serde(rename = "leader_keys")]
    pub leader_keys: BTreeMap<String, KeyBinding>,
}

impl KeyFile {
    /// Reads `keys.toml`, treating both an absent file and a malformed one as
    /// non-fatal: the defaults still work, and the problem is reported in-band.
    pub fn load(path: &std::path::Path) -> (Self, Vec<String>) {
        if !path.exists() {
            return (Self::default(), Vec::new());
        }
        match std::fs::read_to_string(path) {
            Ok(raw) => match toml::from_str::<KeyFile>(&raw) {
                Ok(file) => (file, Vec::new()),
                Err(err) => (
                    Self::default(),
                    vec![format!("keys.toml: {}", first_line(&err.to_string()))],
                ),
            },
            Err(err) => (Self::default(), vec![format!("keys.toml: {err}")]),
        }
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).to_string()
}

/// A binding value is either one key or a list of alternatives.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum KeyBinding {
    One(String),
    Many(Vec<String>),
}

impl KeyBinding {
    fn keys(&self) -> Vec<&str> {
        match self {
            KeyBinding::One(key) => vec![key.as_str()],
            KeyBinding::Many(keys) => keys.iter().map(String::as_str).collect(),
        }
    }
}

/// The shipped binding table.
///
/// The composer owns every unmodified printable key, so anything reachable
/// while typing is either a control chord, a named key, or behind the leader.
#[rustfmt::skip]
const DEFAULTS: &[(Scope, &[&str], Action)] = &[
    // ---- global: available while typing, so modified chords only ----------
    (Scope::Global, &["ctrl+c"], Action::Quit),
    (Scope::Global, &["ctrl+k"], Action::OpenSwitcher),
    (Scope::Global, &["ctrl+f"], Action::OpenSearch),
    (Scope::Global, &["ctrl+l"], Action::Redraw),
    (Scope::Global, &["f1"], Action::OpenHelp),
    (Scope::Global, &["alt+up"], Action::PrevChannel),
    (Scope::Global, &["alt+down"], Action::NextChannel),
    (Scope::Global, &["alt+shift+up"], Action::PrevUnread),
    (Scope::Global, &["alt+shift+down"], Action::NextUnread),
    (Scope::Global, &["pageup"], Action::PageUp),
    (Scope::Global, &["pagedown"], Action::PageDown),
    (Scope::Global, &["tab"], Action::CycleFocus),
    (Scope::Global, &["shift+tab"], Action::CycleFocusBack),

    // ---- compose: readline muscle memory ----------------------------------
    (Scope::Insert, &["enter"], Action::Send),
    (Scope::Insert, &["shift+enter", "alt+enter"], Action::Newline),
    (Scope::Insert, &["esc"], Action::FocusTimeline),
    (Scope::Insert, &["left"], Action::CursorLeft),
    (Scope::Insert, &["right"], Action::CursorRight),
    (Scope::Insert, &["up"], Action::CursorUp),
    (Scope::Insert, &["down"], Action::CursorDown),
    (Scope::Insert, &["alt+left", "alt+b"], Action::WordLeft),
    (Scope::Insert, &["alt+right", "alt+f"], Action::WordRight),
    (Scope::Insert, &["home", "ctrl+a"], Action::LineStart),
    (Scope::Insert, &["end", "ctrl+e"], Action::LineEnd),
    (Scope::Insert, &["backspace"], Action::DeleteBack),
    (Scope::Insert, &["del", "ctrl+d"], Action::DeleteForward),
    (Scope::Insert, &["ctrl+w", "alt+backspace"], Action::DeleteWordBack),
    (Scope::Insert, &["alt+d"], Action::DeleteWordForward),
    (Scope::Insert, &["ctrl+u"], Action::KillToStart),
    (Scope::Insert, &["ctrl+z"], Action::Undo),
    (Scope::Insert, &["ctrl+v"], Action::Paste),
    (Scope::Insert, &["ctrl+p"], Action::HistoryPrev),
    (Scope::Insert, &["ctrl+n"], Action::HistoryNext),
    (Scope::Insert, &["ctrl+t"], Action::Complete),

    // ---- navigate: unmodified keys are safe here --------------------------
    (Scope::Normal, &["i", "a", "enter"], Action::FocusComposer),
    (Scope::Normal, &["esc"], Action::Cancel),
    (Scope::Normal, &["q"], Action::Quit),
    (Scope::Normal, &["?"], Action::OpenHelp),
    (Scope::Normal, &[":"], Action::OpenCommand),
    (Scope::Normal, &["/"], Action::OpenSearch),
    (Scope::Normal, &["j", "down"], Action::SelectNext),
    (Scope::Normal, &["k", "up"], Action::SelectPrev),
    (Scope::Normal, &["ctrl+e"], Action::ScrollDown),
    (Scope::Normal, &["ctrl+y"], Action::ScrollUp),
    (Scope::Normal, &["g g"], Action::ScrollTop),
    (Scope::Normal, &["G"], Action::ScrollBottom),
    (Scope::Normal, &["g u"], Action::JumpFirstUnread),
    (Scope::Normal, &["n"], Action::NextChannel),
    (Scope::Normal, &["p"], Action::PrevChannel),
    (Scope::Normal, &["N"], Action::NextUnread),
    (Scope::Normal, &["P"], Action::PrevUnread),
    (Scope::Normal, &["h"], Action::FocusSidebar),
    (Scope::Normal, &["l"], Action::FocusTimeline),
    (Scope::Normal, &["r"], Action::Reply),
    (Scope::Normal, &["e"], Action::EditMessage),
    (Scope::Normal, &["d"], Action::DeleteMessage),
    (Scope::Normal, &["s"], Action::React),
    (Scope::Normal, &["y"], Action::CopyMessage),
    (Scope::Normal, &["Q"], Action::QuoteMessage),
    (Scope::Normal, &["t"], Action::OpenThread),
    (Scope::Normal, &["o"], Action::OpenLink),
    (Scope::Normal, &["v"], Action::ViewImage),
    (Scope::Normal, &["u"], Action::OpenProfile),
    (Scope::Normal, &["R"], Action::RetrySend),
    (Scope::Normal, &["X"], Action::DiscardFailed),
    (Scope::Normal, &["m"], Action::OpenMembers),

    // ---- leader: everything infrequent ------------------------------------
    (Scope::Leader, &["leader+?"], Action::OpenHelp),
    (Scope::Leader, &["leader+k"], Action::OpenSwitcher),
    (Scope::Leader, &["leader+/"], Action::OpenSearch),
    (Scope::Leader, &["leader+:"], Action::OpenCommand),
    (Scope::Leader, &["leader+c"], Action::CreateChannel),
    (Scope::Leader, &["leader+j"], Action::JoinChannel),
    (Scope::Leader, &["leader+x"], Action::LeaveChannel),
    (Scope::Leader, &["leader+d"], Action::OpenDirectMessage),
    (Scope::Leader, &["leader+y"], Action::CopyIdentity),
    (Scope::Leader, &["leader+m"], Action::ToggleMute),
    (Scope::Leader, &["leader+p"], Action::TogglePin),
    (Scope::Leader, &["leader+a"], Action::MarkRead),
    (Scope::Leader, &["leader+A"], Action::MarkAllRead),
    (Scope::Leader, &["leader+b"], Action::ToggleSidebar),
    (Scope::Leader, &["leader+u"], Action::ToggleMemberPane),
    (Scope::Leader, &["leader+i"], Action::ToggleImages),
    (Scope::Leader, &["leader+z"], Action::ToggleCompact),
    (Scope::Leader, &["leader+t"], Action::ToggleTimestamps),
    (Scope::Leader, &["leader+T"], Action::CycleTheme),
    (Scope::Leader, &["leader+R"], Action::ReloadConfig),
    (Scope::Leader, &["leader+q"], Action::Quit),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(text: &str) -> Chord {
        Chord::parse(text).unwrap()
    }

    #[test]
    fn chords_round_trip_through_their_display_form() {
        for text in [
            "ctrl+b",
            "alt+enter",
            "shift+tab",
            "f5",
            "pgup",
            "space",
            "ctrl+alt+d",
            "/",
            "?",
        ] {
            let parsed = chord(text);
            assert_eq!(
                Chord::parse(&parsed.to_string()).unwrap(),
                parsed,
                "`{text}` did not survive a round trip as `{parsed}`"
            );
        }
    }

    #[test]
    fn shift_spellings_normalise_to_one_chord() {
        // Terminals disagree; the keymap must not.
        assert_eq!(chord("shift+k"), chord("K"));
        assert_eq!(chord("backtab"), chord("shift+tab"));
        assert_eq!(
            Chord::new(KeyCode::BackTab, KeyModifiers::NONE),
            chord("shift+tab")
        );
        assert_eq!(
            Chord::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
            chord("K")
        );
    }

    #[test]
    fn control_chords_ignore_case() {
        assert_eq!(chord("ctrl+A"), chord("ctrl+a"));
        assert_eq!(
            Chord::new(KeyCode::Char('A'), KeyModifiers::CONTROL),
            chord("ctrl+a")
        );
    }

    #[test]
    fn unknown_keys_and_modifiers_are_rejected() {
        assert!(Chord::parse("hyper+a").is_err());
        assert!(Chord::parse("notakey").is_err());
        assert!(Chord::parse("").is_err());
    }

    #[test]
    fn defaults_resolve_in_the_right_scope() {
        let map = Keymap::builtin();
        // `enter` sends while composing but focuses the composer while browsing.
        assert_eq!(
            map.resolve(Scope::Insert, &[], chord("enter")),
            Resolution::Run(Action::Send)
        );
        assert_eq!(
            map.resolve(Scope::Normal, &[], chord("enter")),
            Resolution::Run(Action::FocusComposer)
        );
        // `j` navigates while browsing and must stay typable while composing.
        assert_eq!(
            map.resolve(Scope::Normal, &[], chord("j")),
            Resolution::Run(Action::SelectNext)
        );
        assert_eq!(
            map.resolve(Scope::Insert, &[], chord("j")),
            Resolution::Unbound
        );
    }

    #[test]
    fn global_bindings_reach_both_modes() {
        let map = Keymap::builtin();
        for scope in [Scope::Insert, Scope::Normal] {
            assert_eq!(
                map.resolve(scope, &[], chord("ctrl+k")),
                Resolution::Run(Action::OpenSwitcher)
            );
        }
    }

    #[test]
    fn a_prefix_reports_its_continuations() {
        let map = Keymap::builtin();
        let leader = map.leader;
        match map.resolve(Scope::Insert, &[], leader) {
            Resolution::Pending(next) => {
                assert!(next.iter().any(|(_, a)| *a == Action::CreateChannel));
                assert!(next.iter().any(|(c, _)| *c == chord("c")));
            }
            other => panic!("leader should be pending, got {other:?}"),
        }
        assert_eq!(
            map.resolve(Scope::Insert, &[leader], chord("c")),
            Resolution::Run(Action::CreateChannel)
        );
    }

    #[test]
    fn multi_chord_navigation_sequences_work() {
        let map = Keymap::builtin();
        assert!(matches!(
            map.resolve(Scope::Normal, &[], chord("g")),
            Resolution::Pending(_)
        ));
        assert_eq!(
            map.resolve(Scope::Normal, &[chord("g")], chord("g")),
            Resolution::Run(Action::ScrollTop)
        );
        assert_eq!(
            map.resolve(Scope::Normal, &[chord("g")], chord("u")),
            Resolution::Run(Action::JumpFirstUnread)
        );
    }

    #[test]
    fn indexed_channel_jumps_are_bound_and_collapse_in_help() {
        let map = Keymap::builtin();
        assert_eq!(
            map.resolve(Scope::Insert, &[map.leader], chord("3")),
            Resolution::Run(Action::JumpChannel(3))
        );
        let rows = map.help_rows();
        let navigation = &rows[&Group::Navigation];
        let collapsed = navigation
            .iter()
            .filter(|row| row.keys.contains("1..9"))
            .count();
        assert_eq!(collapsed, 1, "nine bindings must render as one row");
    }

    #[test]
    fn overrides_replace_the_default_for_that_action() {
        let file: KeyFile = toml::from_str(
            r#"
            [compose]
            send = "ctrl+enter"
            "#,
        )
        .unwrap();
        let map = Keymap::with_overrides(file);
        assert_eq!(
            map.resolve(Scope::Insert, &[], chord("ctrl+enter")),
            Resolution::Run(Action::Send)
        );
        assert_eq!(
            map.resolve(Scope::Insert, &[], chord("enter")),
            Resolution::Unbound,
            "the default must be replaced, not merely joined"
        );
        assert!(map.diagnostics.is_empty());
    }

    #[test]
    fn an_override_may_bind_several_alternatives() {
        let file: KeyFile = toml::from_str(
            r#"
            [navigate]
            select_next = ["ctrl+j", "down"]
            "#,
        )
        .unwrap();
        let map = Keymap::with_overrides(file);
        for key in ["ctrl+j", "down"] {
            assert_eq!(
                map.resolve(Scope::Normal, &[], chord(key)),
                Resolution::Run(Action::SelectNext)
            );
        }
    }

    #[test]
    fn binding_a_bare_letter_in_the_composer_is_refused_with_a_diagnostic() {
        let file: KeyFile = toml::from_str(
            r#"
            [compose]
            send = "q"
            "#,
        )
        .unwrap();
        let map = Keymap::with_overrides(file);
        assert!(
            !map.diagnostics.is_empty(),
            "an untypable composer must be reported"
        );
        // The default survives so the client is still usable.
        assert_eq!(
            map.resolve(Scope::Insert, &[], chord("enter")),
            Resolution::Run(Action::Send)
        );
        assert_eq!(
            map.resolve(Scope::Insert, &[], chord("q")),
            Resolution::Unbound
        );
    }

    #[test]
    fn an_unknown_action_is_reported_rather_than_ignored() {
        let file: KeyFile = toml::from_str(
            r#"
            [navigate]
            teleport = "z"
            "#,
        )
        .unwrap();
        let map = Keymap::with_overrides(file);
        assert!(map.diagnostics.iter().any(|d| d.contains("teleport")));
    }

    #[test]
    fn changing_the_leader_moves_every_leader_binding() {
        let file: KeyFile = toml::from_str(
            r#"
            leader = "ctrl+space"
            "#,
        )
        .unwrap();
        let map = Keymap::with_overrides(file);
        assert_eq!(map.leader, chord("ctrl+space"));
        assert_eq!(
            map.resolve(Scope::Insert, &[map.leader], chord("c")),
            Resolution::Run(Action::CreateChannel)
        );
        assert_eq!(
            map.resolve(Scope::Insert, &[], chord("ctrl+b")),
            Resolution::Unbound,
            "the old leader must stop being a prefix"
        );
    }

    #[test]
    fn a_printable_leader_is_refused() {
        let file: KeyFile = toml::from_str(
            r#"
            leader = "x"
            "#,
        )
        .unwrap();
        let map = Keymap::with_overrides(file);
        assert_eq!(map.leader, chord("ctrl+b"));
        assert!(map.diagnostics.iter().any(|d| d.contains("leader")));
    }

    #[test]
    fn every_default_binding_names_a_real_action() {
        let map = Keymap::builtin();
        for binding in &map.bindings {
            assert!(
                !binding.action.name().is_empty(),
                "{:?} is missing from the action table",
                binding.action
            );
            assert!(
                !binding.action.help().is_empty(),
                "{:?} has no help text",
                binding.action
            );
        }
    }

    #[test]
    fn no_default_steals_a_printable_key_from_the_composer() {
        let map = Keymap::builtin();
        for binding in &map.bindings {
            if matches!(binding.scope, Scope::Insert | Scope::Global)
                && binding.sequence.0[0].is_printable()
            {
                panic!(
                    "`{}` is bound to {:?} in {:?} and would make that character untypable",
                    binding.sequence, binding.action, binding.scope
                );
            }
        }
    }

    #[test]
    fn a_hint_shows_the_first_binding_as_written_not_the_alphabetically_first() {
        // The defaults list `i` before `a` for a reason; sorting by chord would
        // advertise `a`, which is not the key the design intends to teach.
        let map = Keymap::builtin();
        assert_eq!(map.hint(Action::FocusComposer), "i");
        assert!(
            map.keys_for(Action::FocusComposer)
                .contains(&"a".to_string())
        );
    }

    /// A multi-chord binding in one mode must not turn its first key into a
    /// dead prefix in another. `g g` navigates, and `g` must still type.
    #[test]
    fn every_printable_key_stays_typable_in_the_composer() {
        let map = Keymap::builtin();
        for c in ' '..='~' {
            let pressed = Chord::new(KeyCode::Char(c), KeyModifiers::NONE);
            assert_eq!(
                map.resolve(Scope::Insert, &[], pressed),
                Resolution::Unbound,
                "`{c}` is not typable in the composer"
            );
        }
        // The same key is a live prefix while navigating.
        assert!(matches!(
            map.resolve(Scope::Normal, &[], chord("g")),
            Resolution::Pending(_)
        ));
    }

    #[test]
    fn a_pending_navigation_sequence_does_not_escape_into_other_scopes() {
        let map = Keymap::builtin();
        // `g` then `c` is nothing in navigate mode, even though `leader c`
        // creates a channel.
        assert_eq!(
            map.resolve(Scope::Normal, &[chord("g")], chord("c")),
            Resolution::Unbound
        );
        // The leader keeps its own namespace regardless of the mode it began in.
        assert_eq!(
            map.resolve(Scope::Insert, &[map.leader], chord("c")),
            Resolution::Run(Action::CreateChannel)
        );
    }

    #[test]
    fn a_multi_chord_override_starting_with_a_printable_key_is_refused_in_the_composer() {
        let file: KeyFile = toml::from_str(
            r#"
            [compose]
            send = "z z"
            "#,
        )
        .unwrap();
        let map = Keymap::with_overrides(file);
        assert!(
            map.diagnostics.iter().any(|d| d.contains("send")),
            "a printable prefix in the composer must be reported"
        );
        assert_eq!(
            map.resolve(Scope::Insert, &[], chord("z")),
            Resolution::Unbound,
            "`z` must remain typable"
        );
    }

    #[test]
    fn hints_come_from_the_live_map_not_hard_coded_strings() {
        let file: KeyFile = toml::from_str(
            r#"
            [navigate]
            reply = "ctrl+r"
            "#,
        )
        .unwrap();
        let map = Keymap::with_overrides(file);
        assert_eq!(map.hint(Action::Reply), "ctrl+r");
        // An action nobody bound reports itself as unset rather than lying.
        let mut bare = Keymap::builtin();
        bare.bindings.retain(|b| b.action != Action::Reply);
        assert_eq!(bare.hint(Action::Reply), "unset");
    }

    #[test]
    fn malformed_key_files_degrade_instead_of_failing() {
        let dir = std::env::temp_dir().join("buzztui-keys-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.toml");
        std::fs::write(&path, "this is not toml = = =").unwrap();
        let (file, problems) = KeyFile::load(&path);
        assert!(!problems.is_empty());
        assert!(file.navigate.is_empty());
        std::fs::remove_file(&path).ok();
    }
}
