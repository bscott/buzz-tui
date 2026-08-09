//! The Buzz dialect of Nostr.
//!
//! Buzz speaks NIP-29 relay-based groups on the wire: a channel is a UUID
//! carried in an `h` tag, membership and metadata arrive as relay-signed
//! addressable events, and everything else is ordinary NIP-01. This module
//! holds the vocabulary — kind numbers, tag accessors, and the small structs we
//! project relay events onto — so that no other module has to know which tag
//! position carries a reply marker.

use nostr::event::{Event, Tag};
use serde::Deserialize;

/// Event kinds Buzz uses, as raw wire numbers.
///
/// `nostr::event::Kind` implements `PartialEq` by hand, which makes it
/// unusable in `match` patterns, and the numbers are what the database columns
/// and NIP-01 filters want anyway. Convert at the boundary with
/// [`kinds::filter`].
pub mod kinds {
    use nostr::event::Kind;

    /// NIP-01 profile metadata.
    pub const METADATA: u16 = 0;
    /// NIP-09 deletion request; self-authored events only.
    pub const DELETION: u16 = 5;
    /// NIP-25 emoji reaction. The relay derives the channel from the `e` target.
    pub const REACTION: u16 = 7;
    /// NIP-29 group chat message. Requires an `h` tag.
    pub const CHAT: u16 = 9;
    /// NIP-17 private direct message rumor, found inside a gift wrap.
    pub const PRIVATE_DM: u16 = 14;
    /// NIP-59 gift wrap carrying a direct message.
    pub const GIFT_WRAP: u16 = 1059;

    /// Ephemeral presence heartbeat.
    pub const PRESENCE: u16 = 20001;
    /// Ephemeral typing indicator.
    pub const TYPING: u16 = 20002;

    /// NIP-29 moderation and lifecycle commands.
    pub const ADD_USER: u16 = 9000;
    pub const REMOVE_USER: u16 = 9001;
    pub const EDIT_METADATA: u16 = 9002;
    pub const ADMIN_DELETE: u16 = 9005;
    pub const CREATE_GROUP: u16 = 9007;
    pub const JOIN_REQUEST: u16 = 9021;
    pub const LEAVE_REQUEST: u16 = 9022;

    /// Relay-signed group discovery events, addressed by a `d` tag.
    pub const GROUP_METADATA: u16 = 39000;
    pub const GROUP_ADMINS: u16 = 39001;
    pub const GROUP_MEMBERS: u16 = 39002;

    /// Relay-signed membership notifications, delivered community-globally.
    pub const MEMBER_ADDED: u16 = 44100;
    pub const MEMBER_REMOVED: u16 = 44101;

    /// Buzz-native message variants.
    pub const CHAT_V2: u16 = 40002;
    pub const CHAT_EDIT: u16 = 40003;

    /// Kinds that render as a message in a channel timeline.
    pub const TIMELINE: [u16; 2] = [CHAT, CHAT_V2];

    /// Every kind a connected client subscribes to for a channel.
    pub const CHANNEL_STREAM: [u16; 6] =
        [CHAT, CHAT_V2, CHAT_EDIT, REACTION, DELETION, ADMIN_DELETE];

    /// Converts wire numbers into the type `Filter::kinds` expects.
    pub fn filter<I: IntoIterator<Item = u16>>(kinds: I) -> impl Iterator<Item = Kind> {
        kinds.into_iter().map(Kind::from_u16)
    }
}

/// Returns the first value of the first tag with the given name.
pub fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event
        .tags
        .iter()
        .find(|tag| tag.kind() == name)
        .and_then(Tag::content)
}

/// Returns every value of every tag with the given name.
pub fn tag_values<'a>(event: &'a Event, name: &'a str) -> impl Iterator<Item = &'a str> {
    event
        .tags
        .iter()
        .filter(move |tag| tag.kind() == name)
        .filter_map(Tag::content)
}

/// The channel a channel-scoped event belongs to.
pub fn channel_of(event: &Event) -> Option<&str> {
    tag_value(event, "h")
}

/// The `d` identifier of an addressable event, which for Buzz discovery events
/// is the channel UUID.
pub fn identifier_of(event: &Event) -> Option<&str> {
    tag_value(event, "d")
}

/// NIP-10 thread position resolved from an event's `e` tags.
#[derive(Debug, Default, Clone)]
pub struct Thread<'a> {
    pub root: Option<&'a str>,
    pub reply: Option<&'a str>,
}

impl<'a> Thread<'a> {
    /// Parses `e` tags using NIP-10 markers, falling back to the positional
    /// convention where the first `e` tag is the root and the last is the
    /// direct parent.
    pub fn parse(event: &'a Event) -> Self {
        let mut thread = Thread::default();
        let mut positional: Vec<&str> = Vec::new();

        for tag in event.tags.iter() {
            if tag.kind() != "e" {
                continue;
            }
            let parts = tag.as_slice();
            let Some(id) = parts.get(1).map(String::as_str).filter(|s| !s.is_empty()) else {
                continue;
            };
            match parts.get(3).map(String::as_str) {
                Some("root") => thread.root = Some(id),
                Some("reply") => thread.reply = Some(id),
                Some("mention") => {}
                _ => positional.push(id),
            }
        }

        if thread.reply.is_none() && thread.root.is_none() {
            match positional.as_slice() {
                [] => {}
                [only] => thread.reply = Some(only),
                [first, .., last] => {
                    thread.root = Some(first);
                    thread.reply = Some(last);
                }
            }
        }
        if thread.root.is_none() {
            thread.root = thread.reply;
        }
        thread
    }

    /// The event this message hangs off, if any.
    pub fn parent(&self) -> Option<&'a str> {
        self.reply.or(self.root)
    }
}

/// The event a reaction or deletion refers to.
pub fn target_of(event: &Event) -> Option<&str> {
    tag_value(event, "e")
}

/// Channel state projected from a relay-signed kind:39000 event.
#[derive(Debug, Clone)]
pub struct ChannelMeta {
    pub id: String,
    pub name: String,
    pub about: Option<String>,
    pub private: bool,
    pub closed: bool,
    /// Direct-message channels are marked hidden and are titled from members.
    pub hidden: bool,
}

impl ChannelMeta {
    pub fn from_event(event: &Event) -> Option<Self> {
        if event.kind.as_u16() != kinds::GROUP_METADATA {
            return None;
        }
        let id = identifier_of(event)?.to_string();
        let name = tag_value(event, "name")
            .filter(|n| !n.is_empty())
            .unwrap_or(&id)
            .to_string();
        Some(Self {
            id,
            name,
            about: tag_value(event, "about")
                .filter(|a| !a.is_empty())
                .map(str::to_string),
            private: has_flag(event, "private"),
            closed: has_flag(event, "closed"),
            hidden: has_flag(event, "hidden"),
        })
    }
}

/// NIP-29 marks booleans by the mere presence of a single-element tag.
fn has_flag(event: &Event, name: &str) -> bool {
    event.tags.iter().any(|tag| tag.kind() == name)
}

/// A member of a channel, with the role the relay assigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub pubkey: String,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "owner" => Role::Owner,
            "admin" => Role::Admin,
            _ => Role::Member,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }

    /// A one-character sigil rendered beside names in the member list.
    pub fn sigil(self) -> &'static str {
        match self {
            Role::Owner => "★",
            Role::Admin => "◆",
            Role::Member => " ",
        }
    }
}

/// Extracts entries from a kind:39001 (admins) or kind:39002 (members)
/// event. Role labels in an admin event are relay-defined; every listed key is
/// therefore at least an administrator, while the conventional `owner` label
/// retains its stronger presentation. A member event carries no roles and may
/// expose only a subset of the complete membership.
pub fn roster_from_event(event: &Event) -> Vec<Member> {
    let admins = event.kind.as_u16() == kinds::GROUP_ADMINS;
    event
        .tags
        .iter()
        .filter(|tag| tag.kind() == "p")
        .filter_map(|tag| {
            let parts = tag.as_slice();
            let pubkey = parts.get(1)?.to_string();
            if pubkey.len() != 64 {
                return None;
            }
            let role = if admins {
                match parts.get(2).map(String::as_str) {
                    Some("owner") => Role::Owner,
                    _ => Role::Admin,
                }
            } else {
                Role::Member
            };
            Some(Member { pubkey, role })
        })
        .collect()
}

/// NIP-01 profile metadata, decoded from the JSON body of a kind:0 event.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub about: Option<String>,
    #[serde(default)]
    pub picture: Option<String>,
    #[serde(default)]
    pub nip05: Option<String>,
}

impl Profile {
    pub fn from_event(event: &Event) -> Option<Self> {
        if event.kind.as_u16() != kinds::METADATA {
            return None;
        }
        serde_json::from_str(&event.content).ok()
    }

    /// The name to render, preferring the human-chosen display name.
    pub fn label(&self) -> Option<&str> {
        self.display_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.name.as_deref().filter(|s| !s.trim().is_empty()))
    }
}

/// Shortens a hex pubkey to the eight-character form used when no profile is
/// known, so unnamed authors still read as stable identities.
pub fn short_pubkey(pubkey: &str) -> String {
    if pubkey.len() <= 8 {
        return pubkey.to_string();
    }
    format!("{}…{}", &pubkey[..4], &pubkey[pubkey.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::event::{EventBuilder, FinalizeEvent, Kind};
    use nostr::key::Keys;

    fn event(kind: u16, tags: Vec<Tag>, content: &str) -> Event {
        EventBuilder::new(Kind::from_u16(kind), content)
            .tags(tags)
            .finalize(&Keys::generate())
            .unwrap()
    }

    #[test]
    fn thread_prefers_explicit_markers() {
        let root = "a".repeat(64);
        let parent = "b".repeat(64);
        let ev = event(
            kinds::CHAT,
            vec![
                Tag::parse(["e", &root, "", "root"]).unwrap(),
                Tag::parse(["e", &parent, "", "reply"]).unwrap(),
            ],
            "hi",
        );
        let thread = Thread::parse(&ev);
        assert_eq!(thread.root.unwrap(), root);
        assert_eq!(thread.parent().unwrap(), parent);
    }

    #[test]
    fn thread_falls_back_to_positional_e_tags() {
        let root = "a".repeat(64);
        let parent = "b".repeat(64);
        let ev = event(
            kinds::CHAT,
            vec![
                Tag::parse(["e", &root]).unwrap(),
                Tag::parse(["e", &parent]).unwrap(),
            ],
            "hi",
        );
        let thread = Thread::parse(&ev);
        assert_eq!(thread.root.unwrap(), root);
        assert_eq!(thread.parent().unwrap(), parent);
    }

    #[test]
    fn single_positional_e_tag_is_parent_and_root() {
        let parent = "c".repeat(64);
        let ev = event(kinds::CHAT, vec![Tag::parse(["e", &parent]).unwrap()], "hi");
        let thread = Thread::parse(&ev);
        assert_eq!(thread.parent().unwrap(), parent);
        assert_eq!(thread.root.unwrap(), parent);
    }

    #[test]
    fn channel_metadata_reads_nip29_flag_tags() {
        let ev = event(
            kinds::GROUP_METADATA,
            vec![
                Tag::parse(["d", "chan-uuid"]).unwrap(),
                Tag::parse(["name", "engineering"]).unwrap(),
                Tag::parse(["about", "where the work happens"]).unwrap(),
                Tag::parse(["closed"]).unwrap(),
                Tag::parse(["private"]).unwrap(),
            ],
            "",
        );
        let meta = ChannelMeta::from_event(&ev).unwrap();
        assert_eq!(meta.id, "chan-uuid");
        assert_eq!(meta.name, "engineering");
        assert_eq!(meta.about.as_deref(), Some("where the work happens"));
        assert!(meta.private && meta.closed && !meta.hidden);
    }

    #[test]
    fn roster_reads_standard_and_custom_admin_roles() {
        let owner = "1".repeat(64);
        let moderator = "2".repeat(64);
        let unlabeled = "3".repeat(64);
        let ev = event(
            kinds::GROUP_ADMINS,
            vec![
                Tag::parse(["d", "chan"]).unwrap(),
                Tag::parse(["p", &owner, "owner"]).unwrap(),
                Tag::parse(["p", &moderator, "moderator"]).unwrap(),
                Tag::parse(["p", &unlabeled]).unwrap(),
                Tag::parse(["p", "too-short", "ceo"]).unwrap(),
            ],
            "",
        );
        let roster = roster_from_event(&ev);
        assert_eq!(roster.len(), 3);
        assert_eq!(roster[0].role, Role::Owner);
        assert_eq!(roster[1].role, Role::Admin);
        assert_eq!(roster[2].role, Role::Admin);
    }
}
