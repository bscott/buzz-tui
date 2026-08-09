//! Domain types shared by the store, the network task, and the renderer.
//!
//! These are the projections the interface actually draws. Raw Nostr events are
//! an implementation detail of the store; everything above it works with these.

/// Whether a conversation is a NIP-29 group or a gift-wrapped direct message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChannelKind {
    /// A NIP-29 group addressed by UUID.
    Group,
    /// A NIP-17 direct message thread, keyed by the counterparty's pubkey.
    Direct,
}

/// A conversation in the sidebar.
#[derive(Debug, Clone)]
pub struct Channel {
    /// Group UUID, or `dm:<pubkey>` for a direct message thread.
    pub id: String,
    pub name: String,
    pub about: Option<String>,
    pub kind: ChannelKind,
    pub private: bool,
    /// True once we hold a membership record for ourselves.
    pub joined: bool,
    pub muted: bool,
    pub pinned: bool,
    /// Timestamp of the newest message we have stored.
    pub last_activity: i64,
    /// Messages newer than the local read marker.
    pub unread: u32,
    /// Unread messages that name us directly.
    pub mentions: u32,
    /// Counterparty pubkey for direct messages.
    pub peer: Option<String>,
}

impl Channel {
    /// The sigil that precedes a conversation name, distinguishing groups,
    /// private groups, and direct messages at a glance.
    pub fn sigil(&self) -> &'static str {
        match (self.kind, self.private) {
            (ChannelKind::Direct, _) => "@",
            (ChannelKind::Group, true) => "\u{1f512}",
            (ChannelKind::Group, false) => "#",
        }
    }
}

/// Delivery state of a message we authored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Signed and written to the relay socket, awaiting `OK`.
    Sending,
    /// The relay accepted it.
    Sent,
    /// The relay rejected it; the reason is carried on the message.
    Failed,
}

/// A single rendered message.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub channel: String,
    pub author: String,
    pub created_at: i64,
    /// Effective body, with any accepted edit already applied.
    pub body: String,
    pub edited: bool,
    pub deleted: bool,
    /// Direct parent in a NIP-10 thread.
    pub parent: Option<String>,
    /// Thread root, which equals `parent` for one-level replies.
    pub root: Option<String>,
    pub reactions: Vec<Reaction>,
    /// `None` for messages received from the relay.
    pub delivery: Option<Delivery>,
    /// Rejection reason when `delivery` is `Failed`.
    pub error: Option<String>,
}

impl Message {
    pub fn is_pending(&self) -> bool {
        matches!(self.delivery, Some(Delivery::Sending | Delivery::Failed))
    }
}

/// Aggregated reactions to one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub emoji: String,
    pub count: u32,
    /// True when our own key is among the reactors.
    pub mine: bool,
}

/// A cached profile, already reduced to what the interface renders.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub pubkey: String,
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub nip05: Option<String>,
}

impl Profile {
    /// The display name, falling back to a shortened pubkey so that every
    /// author has a stable, non-empty label.
    pub fn label(&self) -> String {
        match self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            Some(name) => name.to_string(),
            None => crate::proto::short_pubkey(&self.pubkey),
        }
    }
}

/// Presence as reported by ephemeral kind:20001 events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Online,
    Away,
    Offline,
}

impl Presence {
    pub fn parse(status: &str) -> Self {
        match status {
            "offline" | "" => Presence::Offline,
            "away" | "idle" => Presence::Away,
            _ => Presence::Online,
        }
    }

    pub fn dot(self) -> &'static str {
        match self {
            Presence::Online => "\u{25cf}",
            Presence::Away => "\u{25d0}",
            Presence::Offline => "\u{25cb}",
        }
    }
}
