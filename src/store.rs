//! The local event cache.
//!
//! Buzz relays are the source of truth, but a chat client that re-fetches the
//! world on every launch feels broken. Every event we accept is written here
//! first and projected into the tables the interface reads, so the timeline is
//! populated before the socket has even finished its handshake.
//!
//! The schema keeps raw events in one table and derives everything else from
//! them. Locally-owned state — read markers, mutes, pins — lives in columns the
//! relay never writes, so re-ingesting an event can never clobber it.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use nostr::event::Event;
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{Channel, ChannelKind, Delivery, Message, Profile, Reaction};
use crate::proto::{self, kinds};

/// Current schema version. Bumping this requires a matching migration arm.
const SCHEMA_VERSION: i64 = 1;

/// Prefix marking a synthetic direct-message conversation.
pub const DIRECT_PREFIX: &str = "dm:";

/// A decrypted NIP-17 direct message, projected into the columns the events
/// table needs. Rumors carry no signature, so they never pass through the
/// signature-verifying ingest path.
pub struct Rumor<'a> {
    pub id: &'a str,
    pub author: &'a str,
    pub created_at: i64,
    pub body: &'a str,
    /// The rumor's tags, already serialised as JSON.
    pub tags: &'a str,
    pub parent: Option<&'a str>,
    pub root: Option<&'a str>,
    pub mentions_me: bool,
}

/// What changed as a result of accepting an event, so the interface can refresh
/// precisely the part of itself that went stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingested {
    /// Nothing we model, or a duplicate we already had.
    Ignored,
    Message {
        channel: String,
        id: String,
    },
    Reaction {
        channel: Option<String>,
        target: String,
    },
    Deleted {
        channel: Option<String>,
        target: String,
    },
    Edited {
        channel: Option<String>,
        target: String,
    },
    ChannelMeta {
        channel: String,
    },
    Members {
        channel: String,
    },
    Profile {
        pubkey: String,
    },
    Membership {
        channel: String,
        joined: bool,
    },
}

impl Ingested {
    /// The conversation whose timeline or sidebar entry this event disturbed.
    pub fn channel(&self) -> Option<&str> {
        match self {
            Ingested::Message { channel, .. }
            | Ingested::ChannelMeta { channel }
            | Ingested::Members { channel }
            | Ingested::Membership { channel, .. } => Some(channel),
            Ingested::Reaction { channel, .. }
            | Ingested::Deleted { channel, .. }
            | Ingested::Edited { channel, .. } => channel.as_deref(),
            Ingested::Ignored | Ingested::Profile { .. } => None,
        }
    }
}

pub struct Store {
    conn: Mutex<Connection>,
    /// Our own pubkey, needed to resolve "did I react" and "was I mentioned".
    me: String,
}

impl Store {
    pub fn open(path: &Path, me: &str) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        Self::from_connection(conn, me)
    }

    #[cfg(test)]
    pub fn in_memory(me: &str) -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, me)
    }

    /// A throwaway cache with no identity attached, for code paths that need a
    /// store to exist but must never touch a real account's data.
    pub fn scratch() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, "")
    }

    fn from_connection(conn: Connection, me: &str) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;",
        )
        .context("configuring sqlite")?;

        let store = Self {
            conn: Mutex::new(conn),
            me: me.to_string(),
        };
        store.migrate()?;
        store.claim()?;
        Ok(store)
    }

    /// Records which identity owns this cache, and refuses to open one that
    /// belongs to someone else.
    ///
    /// The cache holds decrypted direct messages. Paths already separate
    /// identities, but a copied or restored file would slip past that, and
    /// silently showing one account another's private timeline is the worst
    /// failure this program could have.
    fn claim(&self) -> Result<()> {
        let conn = self.lock();
        let owner: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'owner'", [], |row| {
                row.get(0)
            })
            .optional()?;
        match owner {
            Some(owner) if owner != self.me => bail!(
                "this cache belongs to {}, not {}; refusing to open it",
                proto::short_pubkey(&owner),
                proto::short_pubkey(&self.me)
            ),
            Some(_) => Ok(()),
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('owner', ?1)",
                    params![self.me],
                )?;
                Ok(())
            }
        }
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS events (
                id          TEXT PRIMARY KEY,
                pubkey      TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                kind        INTEGER NOT NULL,
                content     TEXT NOT NULL,
                tags        TEXT NOT NULL,
                sig         TEXT NOT NULL,
                channel     TEXT,
                parent      TEXT,
                root        TEXT,
                target      TEXT,
                mentions_me INTEGER NOT NULL DEFAULT 0,
                deleted     INTEGER NOT NULL DEFAULT 0,
                edit_body   TEXT,
                edited_at   INTEGER,
                delivery    INTEGER,
                error       TEXT,
                received_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_channel_time
                ON events(channel, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_events_target
                ON events(target) WHERE target IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_events_kind_time
                ON events(kind, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_events_pubkey ON events(pubkey);

            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                body,
                id UNINDEXED,
                channel UNINDEXED,
                tokenize = 'unicode61 remove_diacritics 2'
            );

            CREATE TABLE IF NOT EXISTS channels (
                id            TEXT PRIMARY KEY,
                name          TEXT NOT NULL,
                about         TEXT,
                kind          TEXT NOT NULL DEFAULT 'group',
                private       INTEGER NOT NULL DEFAULT 0,
                closed        INTEGER NOT NULL DEFAULT 0,
                hidden        INTEGER NOT NULL DEFAULT 0,
                peer          TEXT,
                updated_at    INTEGER NOT NULL DEFAULT 0,
                joined        INTEGER NOT NULL DEFAULT 0,
                muted         INTEGER NOT NULL DEFAULT 0,
                pinned        INTEGER NOT NULL DEFAULT 0,
                last_read_at  INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS members (
                channel TEXT NOT NULL,
                pubkey  TEXT NOT NULL,
                role    TEXT NOT NULL DEFAULT 'member',
                PRIMARY KEY (channel, pubkey)
            );

            CREATE TABLE IF NOT EXISTS profiles (
                pubkey     TEXT PRIMARY KEY,
                name       TEXT,
                about      TEXT,
                picture    TEXT,
                nip05      TEXT,
                updated_at INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS gift_wraps (
                wrap_id  TEXT PRIMARY KEY,
                rumor_id TEXT
            );

            CREATE TABLE IF NOT EXISTS media (
                url        TEXT PRIMARY KEY,
                path       TEXT,
                mime       TEXT,
                bytes      INTEGER NOT NULL DEFAULT 0,
                failed     INTEGER NOT NULL DEFAULT 0,
                fetched_at INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )
        .context("creating schema")?;

        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    // ---------------------------------------------------------------- ingest

    /// Accepts a verified relay event, projecting it into the derived tables.
    pub fn ingest(&self, event: &Event) -> Result<Ingested> {
        match event.kind.as_u16() {
            kinds::METADATA => self.ingest_profile(event),
            kinds::CHAT | kinds::CHAT_V2 => self.ingest_message(event, None),
            kinds::REACTION => self.ingest_reaction(event),
            kinds::DELETION | kinds::ADMIN_DELETE => self.ingest_deletion(event),
            kinds::CHAT_EDIT => self.ingest_edit(event),
            kinds::GROUP_METADATA => self.ingest_channel_meta(event),
            kinds::GROUP_ADMINS | kinds::GROUP_MEMBERS => self.ingest_roster(event),
            kinds::MEMBER_ADDED | kinds::MEMBER_REMOVED => self.ingest_membership(event),
            _ => Ok(Ingested::Ignored),
        }
    }

    /// Stores a chat message. `channel_override` carries the synthetic
    /// conversation id used for unwrapped direct messages, which have no `h` tag.
    pub fn ingest_message(
        &self,
        event: &Event,
        channel_override: Option<&str>,
    ) -> Result<Ingested> {
        let Some(channel) = channel_override
            .map(str::to_string)
            .or_else(|| proto::channel_of(event).map(str::to_string))
        else {
            return Ok(Ingested::Ignored);
        };

        let thread = proto::Thread::parse(event);
        let mentions_me = self.mentions_me(event);
        let conn = self.lock();
        // The `ON CONFLICT` arm exists for the relay echoing back a message we
        // sent ourselves: it settles the optimistic row rather than duplicating.
        let changed = conn.execute(
            "INSERT INTO events
                (id, pubkey, created_at, kind, content, tags, sig, channel,
                 parent, root, mentions_me, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET delivery = 1, error = NULL
             WHERE events.delivery IS NOT NULL",
            params![
                event.id.to_hex(),
                event.pubkey.to_hex(),
                event.created_at.as_secs() as i64,
                event.kind.as_u16(),
                event.content,
                serde_json::to_string(&event.tags)?,
                event.sig.to_string(),
                channel,
                thread.parent(),
                thread.root,
                mentions_me as i64,
                now(),
            ],
        )?;

        if changed == 0 {
            return Ok(Ingested::Ignored);
        }

        let id = event.id.to_hex();
        if !fts_contains(&conn, &id)? {
            conn.execute(
                "INSERT INTO messages_fts (body, id, channel) VALUES (?1, ?2, ?3)",
                params![event.content, id, channel],
            )?;
        }
        ensure_channel_row(&conn, &channel)?;

        Ok(Ingested::Message { channel, id })
    }

    fn ingest_reaction(&self, event: &Event) -> Result<Ingested> {
        let Some(target) = proto::target_of(event).map(str::to_string) else {
            return Ok(Ingested::Ignored);
        };
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO events
                (id, pubkey, created_at, kind, content, tags, sig, channel, target, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.id.to_hex(),
                event.pubkey.to_hex(),
                event.created_at.as_secs() as i64,
                event.kind.as_u16(),
                event.content,
                serde_json::to_string(&event.tags)?,
                event.sig.to_string(),
                proto::channel_of(event),
                target,
                now(),
            ],
        )?;
        if changed == 0 {
            return Ok(Ingested::Ignored);
        }
        // The relay derives a reaction's channel from its target, so trust the
        // stored target's channel over whatever the client tagged.
        let channel = channel_of_event(&conn, &target)?;
        Ok(Ingested::Reaction { channel, target })
    }

    fn ingest_deletion(&self, event: &Event) -> Result<Ingested> {
        let Some(target) = proto::target_of(event).map(str::to_string) else {
            return Ok(Ingested::Ignored);
        };
        let conn = self.lock();
        // NIP-09 only authorises self-deletion. Kind:9005 is the moderator path,
        // which the relay has already vetted, so accept that unconditionally.
        let moderated = event.kind.as_u16() == kinds::ADMIN_DELETE;
        let affected = conn.execute(
            "UPDATE events SET deleted = 1
             WHERE id = ?1 AND deleted = 0 AND (?2 = 1 OR pubkey = ?3)",
            params![target, moderated as i64, event.pubkey.to_hex()],
        )?;
        if affected == 0 {
            return Ok(Ingested::Ignored);
        }
        conn.execute("DELETE FROM messages_fts WHERE id = ?1", params![target])?;
        let channel = channel_of_event(&conn, &target)?;
        Ok(Ingested::Deleted { channel, target })
    }

    fn ingest_edit(&self, event: &Event) -> Result<Ingested> {
        let Some(target) = proto::target_of(event).map(str::to_string) else {
            return Ok(Ingested::Ignored);
        };
        let conn = self.lock();
        // Only the original author may rewrite a message, and only with an edit
        // newer than the one already applied.
        let affected = conn.execute(
            "UPDATE events SET edit_body = ?1, edited_at = ?2
             WHERE id = ?3 AND pubkey = ?4
               AND (edited_at IS NULL OR edited_at < ?2)",
            params![
                event.content,
                event.created_at.as_secs() as i64,
                target,
                event.pubkey.to_hex(),
            ],
        )?;
        if affected == 0 {
            return Ok(Ingested::Ignored);
        }
        conn.execute(
            "UPDATE messages_fts SET body = ?1 WHERE id = ?2",
            params![event.content, target],
        )?;
        let channel = channel_of_event(&conn, &target)?;
        Ok(Ingested::Edited { channel, target })
    }

    fn ingest_profile(&self, event: &Event) -> Result<Ingested> {
        let Some(profile) = proto::Profile::from_event(event) else {
            return Ok(Ingested::Ignored);
        };
        let pubkey = event.pubkey.to_hex();
        let conn = self.lock();
        let affected = conn.execute(
            "INSERT INTO profiles (pubkey, name, about, picture, nip05, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(pubkey) DO UPDATE SET
                name = excluded.name, about = excluded.about,
                picture = excluded.picture, nip05 = excluded.nip05,
                updated_at = excluded.updated_at
             WHERE excluded.updated_at > profiles.updated_at",
            params![
                pubkey,
                profile.label(),
                profile.about,
                profile.picture,
                profile.nip05,
                event.created_at.as_secs() as i64,
            ],
        )?;
        if affected == 0 {
            return Ok(Ingested::Ignored);
        }
        Ok(Ingested::Profile { pubkey })
    }

    fn ingest_channel_meta(&self, event: &Event) -> Result<Ingested> {
        let Some(meta) = proto::ChannelMeta::from_event(event) else {
            return Ok(Ingested::Ignored);
        };
        let conn = self.lock();
        conn.execute(
            "INSERT INTO channels (id, name, about, kind, private, closed, hidden, updated_at)
             VALUES (?1, ?2, ?3, 'group', ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name, about = excluded.about,
                private = excluded.private, closed = excluded.closed,
                hidden = excluded.hidden, updated_at = excluded.updated_at
             WHERE excluded.updated_at >= channels.updated_at",
            params![
                meta.id,
                meta.name,
                meta.about,
                meta.private as i64,
                meta.closed as i64,
                meta.hidden as i64,
                event.created_at.as_secs() as i64,
            ],
        )?;
        Ok(Ingested::ChannelMeta { channel: meta.id })
    }

    fn ingest_roster(&self, event: &Event) -> Result<Ingested> {
        let Some(channel) = proto::identifier_of(event).map(str::to_string) else {
            return Ok(Ingested::Ignored);
        };
        let roster = proto::roster_from_event(event);
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        ensure_channel_row(&tx, &channel)?;

        if event.kind.as_u16() == kinds::GROUP_MEMBERS {
            // Kind:39002 is the complete roster, so it replaces what we hold.
            // It carries no roles, though, so preserve the ones kind:39001 set.
            tx.execute(
                "DELETE FROM members WHERE channel = ?1 AND role = 'member'",
                params![channel],
            )?;
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO members (channel, pubkey, role) VALUES (?1, ?2, ?3)
                 ON CONFLICT(channel, pubkey) DO UPDATE SET role = excluded.role
                 WHERE excluded.role <> 'member' OR members.role = 'member'",
            )?;
            for member in &roster {
                stmt.execute(params![channel, member.pubkey, member.role.as_str()])?;
            }
        }
        if roster.iter().any(|m| m.pubkey == self.me) {
            tx.execute(
                "UPDATE channels SET joined = 1 WHERE id = ?1",
                params![channel],
            )?;
        }
        tx.commit()?;
        Ok(Ingested::Members { channel })
    }

    fn ingest_membership(&self, event: &Event) -> Result<Ingested> {
        let Some(channel) = proto::channel_of(event).map(str::to_string) else {
            return Ok(Ingested::Ignored);
        };
        if proto::tag_value(event, "p") != Some(self.me.as_str()) {
            return Ok(Ingested::Ignored);
        }
        let joined = event.kind.as_u16() == kinds::MEMBER_ADDED;
        let conn = self.lock();
        ensure_channel_row(&conn, &channel)?;
        conn.execute(
            "UPDATE channels SET joined = ?1 WHERE id = ?2",
            params![joined as i64, channel],
        )?;
        Ok(Ingested::Membership { channel, joined })
    }

    /// True when an event addresses us by `p` tag.
    fn mentions_me(&self, event: &Event) -> bool {
        proto::tag_values(event, "p").any(|p| p == self.me)
    }

    // ------------------------------------------------------------ local echo

    /// Stores a decrypted direct message. Rumors are unsigned by construction,
    /// so they cannot travel through [`Store::ingest`] with the other events.
    pub fn ingest_rumor(&self, rumor: &Rumor<'_>, channel: &str) -> Result<Ingested> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO events
                (id, pubkey, created_at, kind, content, tags, sig, channel,
                 parent, root, mentions_me, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', ?7, ?8, ?9, ?10, ?11)",
            params![
                rumor.id,
                rumor.author,
                rumor.created_at,
                kinds::CHAT,
                rumor.body,
                rumor.tags,
                channel,
                rumor.parent,
                rumor.root,
                rumor.mentions_me as i64,
                now(),
            ],
        )?;
        if changed == 0 {
            return Ok(Ingested::Ignored);
        }
        if !fts_contains(&conn, rumor.id)? {
            conn.execute(
                "INSERT INTO messages_fts (body, id, channel) VALUES (?1, ?2, ?3)",
                params![rumor.body, rumor.id, channel],
            )?;
        }
        ensure_channel_row(&conn, channel)?;
        Ok(Ingested::Message {
            channel: channel.to_string(),
            id: rumor.id.to_string(),
        })
    }

    /// Records a direct message we just wrapped, in plaintext and marked as
    /// pending, so it appears immediately and can still show a delivery state.
    pub fn record_outgoing_rumor(&self, rumor: &Rumor<'_>, channel: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO events
                (id, pubkey, created_at, kind, content, tags, sig, channel,
                 parent, root, mentions_me, delivery, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', ?7, ?8, ?9, 0, 0, ?10)",
            params![
                rumor.id,
                rumor.author,
                rumor.created_at,
                kinds::CHAT,
                rumor.body,
                rumor.tags,
                channel,
                rumor.parent,
                rumor.root,
                now(),
            ],
        )?;
        if !fts_contains(&conn, rumor.id)? {
            conn.execute(
                "INSERT INTO messages_fts (body, id, channel) VALUES (?1, ?2, ?3)",
                params![rumor.body, rumor.id, channel],
            )?;
        }
        ensure_channel_row(&conn, channel)?;
        Ok(())
    }

    /// Records a message we just signed, before the relay has acknowledged it,
    /// so it appears in the timeline immediately.
    pub fn record_outgoing(&self, event: &Event, channel: &str) -> Result<()> {
        let thread = proto::Thread::parse(event);
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO events
                (id, pubkey, created_at, kind, content, tags, sig, channel,
                 parent, root, mentions_me, delivery, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, 0, ?11)",
            params![
                event.id.to_hex(),
                event.pubkey.to_hex(),
                event.created_at.as_secs() as i64,
                event.kind.as_u16(),
                event.content,
                serde_json::to_string(&event.tags)?,
                event.sig.to_string(),
                channel,
                thread.parent(),
                thread.root,
                now(),
            ],
        )?;
        let id = event.id.to_hex();
        if !fts_contains(&conn, &id)? {
            conn.execute(
                "INSERT INTO messages_fts (body, id, channel) VALUES (?1, ?2, ?3)",
                params![event.content, id, channel],
            )?;
        }
        ensure_channel_row(&conn, channel)?;
        Ok(())
    }

    /// Applies a relay `OK` verdict to a message we sent.
    pub fn resolve_outgoing(&self, id: &str, accepted: bool, message: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE events SET delivery = ?1, error = ?2 WHERE id = ?3",
            params![
                if accepted { 1 } else { 2 },
                (!accepted).then(|| message.to_string()),
                id
            ],
        )?;
        Ok(())
    }

    /// Drops a failed local message, used when the user abandons a retry.
    pub fn discard_outgoing(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM events WHERE id = ?1 AND delivery = 2",
            params![id],
        )?;
        conn.execute("DELETE FROM messages_fts WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ----------------------------------------------------------- direct msgs

    /// Remembers that a gift wrap has been opened, so we never pay for the
    /// decryption twice. Returns false when it had already been seen.
    pub fn note_gift_wrap(&self, wrap_id: &str, rumor_id: Option<&str>) -> Result<bool> {
        let conn = self.lock();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO gift_wraps (wrap_id, rumor_id) VALUES (?1, ?2)",
            params![wrap_id, rumor_id],
        )?;
        Ok(inserted > 0)
    }

    /// Creates or renames the local conversation that holds a DM thread.
    pub fn upsert_direct_channel(&self, peer: &str, name: &str) -> Result<String> {
        let id = direct_channel_id(peer);
        let conn = self.lock();
        conn.execute(
            "INSERT INTO channels (id, name, kind, private, peer, joined, updated_at)
             VALUES (?1, ?2, 'direct', 1, ?3, 1, ?4)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, peer = excluded.peer",
            params![id, name, peer, now()],
        )?;
        Ok(id)
    }

    // ----------------------------------------------------------------- reads

    pub fn channels(&self) -> Result<Vec<Channel>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.name, c.about, c.kind, c.private, c.joined, c.muted,
                    c.pinned, c.peer,
                    COALESCE(MAX(e.created_at), 0) AS last_activity,
                    COALESCE(SUM(CASE WHEN e.created_at > c.last_read_at
                                       AND e.pubkey <> ?1 AND e.deleted = 0
                                      THEN 1 ELSE 0 END), 0) AS unread,
                    COALESCE(SUM(CASE WHEN e.created_at > c.last_read_at
                                       AND e.pubkey <> ?1 AND e.deleted = 0
                                       AND e.mentions_me = 1
                                      THEN 1 ELSE 0 END), 0) AS mentions
             FROM channels c
             LEFT JOIN events e
                    ON e.channel = c.id AND e.kind IN (9, 40002)
             GROUP BY c.id
             ORDER BY c.pinned DESC, last_activity DESC, c.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![self.me], |row| {
            Ok(Channel {
                id: row.get(0)?,
                name: row.get(1)?,
                about: row.get(2)?,
                kind: if row.get::<_, String>(3)? == "direct" {
                    ChannelKind::Direct
                } else {
                    ChannelKind::Group
                },
                private: row.get::<_, i64>(4)? != 0,
                joined: row.get::<_, i64>(5)? != 0,
                muted: row.get::<_, i64>(6)? != 0,
                pinned: row.get::<_, i64>(7)? != 0,
                peer: row.get(8)?,
                last_activity: row.get(9)?,
                unread: row.get::<_, i64>(10)? as u32,
                mentions: row.get::<_, i64>(11)? as u32,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Loads the newest `limit` messages in a channel, oldest first. When
    /// `before` is set, loads the page immediately preceding that timestamp.
    pub fn messages(&self, channel: &str, limit: u32, before: Option<i64>) -> Result<Vec<Message>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, channel, pubkey, created_at, kind, content, edit_body,
                    edited_at, deleted, parent, root, delivery, error
             FROM events
             WHERE channel = ?1 AND kind IN (9, 40002) AND created_at < ?2
             ORDER BY created_at DESC, id DESC
             LIMIT ?3",
        )?;
        let mut messages = stmt
            .query_map(
                params![channel, before.unwrap_or(i64::MAX), limit],
                message_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        messages.reverse();
        drop(stmt);
        drop(conn);
        self.attach_reactions(&mut messages)?;
        Ok(messages)
    }

    /// Fetches a single message by id, used to render the quoted parent of a
    /// reply whose parent may have scrolled out of the loaded page.
    pub fn message(&self, id: &str) -> Result<Option<Message>> {
        let conn = self.lock();
        let message = conn
            .query_row(
                "SELECT id, channel, pubkey, created_at, kind, content, edit_body,
                        edited_at, deleted, parent, root, delivery, error
                 FROM events WHERE id = ?1",
                params![id],
                message_from_row,
            )
            .optional()?;
        Ok(message)
    }

    /// Populates the reaction summary for a page of messages in one query,
    /// rather than one query per message.
    pub fn attach_reactions(&self, messages: &mut [Message]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT target, content, COUNT(*) AS total,
                    SUM(CASE WHEN pubkey = ? THEN 1 ELSE 0 END) AS mine
             FROM events
             WHERE kind = 7 AND deleted = 0 AND target IN ({placeholders})
             GROUP BY target, content
             ORDER BY total DESC, content"
        );

        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
        binds.push(&self.me);
        for id in &ids {
            binds.push(id);
        }

        let mut grouped: HashMap<String, Vec<Reaction>> = HashMap::new();
        let rows = stmt.query_map(binds.as_slice(), |row| {
            let target: String = row.get(0)?;
            let raw: String = row.get(1)?;
            Ok((
                target,
                Reaction {
                    emoji: normalise_reaction(&raw),
                    count: row.get::<_, i64>(2)? as u32,
                    mine: row.get::<_, i64>(3)? > 0,
                },
            ))
        })?;
        for row in rows {
            let (target, reaction) = row?;
            grouped.entry(target).or_default().push(reaction);
        }

        for message in messages.iter_mut() {
            if let Some(reactions) = grouped.remove(&message.id) {
                message.reactions = reactions;
            }
        }
        Ok(())
    }

    pub fn members(&self, channel: &str) -> Result<Vec<(String, proto::Role)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT pubkey, role FROM members WHERE channel = ?1
             ORDER BY CASE role WHEN 'owner' THEN 0 WHEN 'admin' THEN 1 ELSE 2 END, pubkey",
        )?;
        let rows = stmt.query_map(params![channel], |row| {
            Ok((
                row.get::<_, String>(0)?,
                proto::Role::parse(&row.get::<_, String>(1)?),
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn profiles(&self) -> Result<HashMap<String, Profile>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT pubkey, name, about, picture, nip05 FROM profiles")?;
        let rows = stmt.query_map([], |row| {
            let pubkey: String = row.get(0)?;
            Ok((
                pubkey.clone(),
                Profile {
                    pubkey,
                    name: row.get(1)?,
                    about: row.get(2)?,
                    picture: row.get(3)?,
                    nip05: row.get(4)?,
                },
            ))
        })?;
        Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
    }

    /// Full-text search over cached message bodies, newest first.
    pub fn search(&self, query: &str, channel: Option<&str>, limit: u32) -> Result<Vec<Message>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.id, e.channel, e.pubkey, e.created_at, e.kind, e.content,
                    e.edit_body, e.edited_at, e.deleted, e.parent, e.root,
                    e.delivery, e.error
             FROM messages_fts f
             JOIN events e ON e.id = f.id
             WHERE messages_fts MATCH ?1
               AND (?2 IS NULL OR f.channel = ?2)
               AND e.deleted = 0
             ORDER BY e.created_at DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![fts_query(trimmed), channel, limit],
            message_from_row,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ----------------------------------------------------------- local state

    pub fn mark_read(&self, channel: &str, through: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE channels SET last_read_at = MAX(last_read_at, ?1) WHERE id = ?2",
            params![through, channel],
        )?;
        Ok(())
    }

    pub fn set_muted(&self, channel: &str, muted: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE channels SET muted = ?1 WHERE id = ?2",
            params![muted as i64, channel],
        )?;
        Ok(())
    }

    pub fn set_pinned(&self, channel: &str, pinned: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE channels SET pinned = ?1 WHERE id = ?2",
            params![pinned as i64, channel],
        )?;
        Ok(())
    }

    pub fn set_joined(&self, channel: &str, joined: bool) -> Result<()> {
        let conn = self.lock();
        ensure_channel_row(&conn, channel)?;
        conn.execute(
            "UPDATE channels SET joined = ?1 WHERE id = ?2",
            params![joined as i64, channel],
        )?;
        Ok(())
    }

    // ----------------------------------------------------------- media cache

    /// Returns the cached file for a URL, or `Err`-free `None` when we have
    /// never fetched it. The boolean reports a previous permanent failure.
    pub fn media_path(&self, url: &str) -> Result<Option<(Option<String>, bool)>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT path, failed FROM media WHERE url = ?1",
                params![url],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        Ok(row)
    }

    pub fn record_media(
        &self,
        url: &str,
        path: Option<&str>,
        bytes: u64,
        failed: bool,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO media (url, path, bytes, failed, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(url) DO UPDATE SET
                path = excluded.path, bytes = excluded.bytes,
                failed = excluded.failed, fetched_at = excluded.fetched_at",
            params![url, path, bytes as i64, failed as i64, now()],
        )?;
        Ok(())
    }
}

/// Creates a placeholder conversation row so that messages arriving before
/// their kind:39000 metadata still have somewhere to live.
fn ensure_channel_row(conn: &Connection, channel: &str) -> Result<()> {
    let (name, kind) = match direct_peer(channel) {
        Some(peer) => (proto::short_pubkey(peer), "direct"),
        None => (channel.to_string(), "group"),
    };
    conn.execute(
        "INSERT OR IGNORE INTO channels (id, name, kind) VALUES (?1, ?2, ?3)",
        params![channel, name, kind],
    )?;
    Ok(())
}

fn channel_of_event(conn: &Connection, id: &str) -> Result<Option<String>> {
    let channel = conn
        .query_row(
            "SELECT channel FROM events WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(channel)
}

/// FTS5 tables have no uniqueness constraint, so guard inserts explicitly to
/// stop a re-ingested message from appearing twice in search results.
fn fts_contains(conn: &Connection, id: &str) -> Result<bool> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM messages_fts WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(existing.is_some())
}

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<Message> {
    let content: String = row.get(5)?;
    let edit_body: Option<String> = row.get(6)?;
    let edited_at: Option<i64> = row.get(7)?;
    let delivery = match row.get::<_, Option<i64>>(11)? {
        Some(0) => Some(Delivery::Sending),
        Some(1) => Some(Delivery::Sent),
        Some(2) => Some(Delivery::Failed),
        _ => None,
    };
    Ok(Message {
        id: row.get(0)?,
        channel: row.get(1)?,
        author: row.get(2)?,
        created_at: row.get(3)?,
        body: edit_body.unwrap_or(content),
        edited: edited_at.is_some(),
        deleted: row.get::<_, i64>(8)? != 0,
        parent: row.get(9)?,
        root: row.get(10)?,
        reactions: Vec::new(),
        delivery,
        error: row.get(12)?,
    })
}

/// NIP-25 says `+` means a like and `-` a dislike; render them as the emoji
/// every other client shows rather than as bare punctuation.
fn normalise_reaction(raw: &str) -> String {
    match raw.trim() {
        "+" | "" => "\u{1f44d}".to_string(),
        "-" => "\u{1f44e}".to_string(),
        other => other.to_string(),
    }
}

/// Escapes user input into an FTS5 phrase query. Quoting each term stops
/// operators that occur in ordinary prose from becoming syntax errors.
fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn direct_channel_id(peer: &str) -> String {
    format!("{DIRECT_PREFIX}{peer}")
}

pub fn direct_peer(channel: &str) -> Option<&str> {
    channel.strip_prefix(DIRECT_PREFIX)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::event::{EventBuilder, FinalizeEvent, Kind, Tag};
    use nostr::key::Keys;
    use nostr::types::Timestamp;

    /// Explicit timestamps keep ordering assertions independent of clock speed.
    fn chat_at(keys: &Keys, channel: &str, body: &str, at: u64) -> Event {
        EventBuilder::new(Kind::from_u16(kinds::CHAT), body)
            .tag(Tag::parse(["h", channel]).unwrap())
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(keys)
            .unwrap()
    }

    fn chat(keys: &Keys, channel: &str, body: &str) -> Event {
        chat_at(keys, channel, body, 1_700_000_000)
    }

    fn store_with(keys: &Keys) -> Store {
        Store::in_memory(&keys.public_key().to_hex()).unwrap()
    }

    /// The cache holds decrypted direct messages, so a second identity must
    /// never be able to read one it does not own. Paths separate them, and this
    /// is the backstop for a file that gets copied or restored anyway.
    #[test]
    fn a_cache_refuses_an_identity_it_does_not_belong_to() {
        let dir = std::env::temp_dir().join(format!("buzztui-owner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("buzz.db");
        let _ = std::fs::remove_file(&path);

        let alice = Keys::generate();
        let bob = Keys::generate();
        let alice_hex = alice.public_key().to_hex();

        // Alice reads a direct message into her cache.
        {
            let store = Store::open(&path, &alice_hex).unwrap();
            let rumor = Rumor {
                id: &"d".repeat(64),
                author: &bob.public_key().to_hex(),
                created_at: 1_700_000_000,
                body: "the vault password is hunter2",
                tags: "[]",
                parent: None,
                root: None,
                mentions_me: true,
            };
            let channel = direct_channel_id(&bob.public_key().to_hex());
            store.ingest_rumor(&rumor, &channel).unwrap();
        }

        // Reopening as Alice is fine.
        assert!(Store::open(&path, &alice_hex).is_ok());

        // Opening the same file as Bob must fail rather than hand over her
        // decrypted timeline.
        let err = match Store::open(&path, &bob.public_key().to_hex()) {
            Ok(_) => panic!("a foreign identity must not open this cache"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("belongs to"), "{err}");

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Each identity gets its own file, which is the first line of defence.
    #[test]
    fn identities_do_not_share_a_cache_path() {
        let paths = crate::config::Paths {
            root: "/tmp/x".into(),
            config: "/tmp/x/config.toml".into(),
            secret: "/tmp/x/secret.key".into(),
            cache: "/tmp/x/cache".into(),
            log: "/tmp/x/log".into(),
        };
        let alice = "a".repeat(64);
        let bob = "b".repeat(64);
        assert_ne!(paths.db_for(&alice), paths.db_for(&bob));
        assert_ne!(paths.media_for(&alice), paths.media_for(&bob));
        // And the same identity is stable across calls.
        assert_eq!(paths.db_for(&alice), paths.db_for(&alice));
    }

    #[test]
    fn messages_round_trip_oldest_first() {
        let keys = Keys::generate();
        let store = store_with(&keys);
        for (offset, body) in ["first", "second", "third"].iter().enumerate() {
            store
                .ingest(&chat_at(&keys, "room", body, 1_700_000_000 + offset as u64))
                .unwrap();
        }
        let messages = store.messages("room", 10, None).unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].body, "first");
        assert_eq!(messages[2].body, "third");
    }

    #[test]
    fn duplicate_events_are_ignored() {
        let keys = Keys::generate();
        let store = store_with(&keys);
        let event = chat(&keys, "room", "hello");
        assert!(matches!(
            store.ingest(&event).unwrap(),
            Ingested::Message { .. }
        ));
        assert_eq!(store.ingest(&event).unwrap(), Ingested::Ignored);
        assert_eq!(store.messages("room", 10, None).unwrap().len(), 1);
        assert_eq!(store.search("hello", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn reactions_aggregate_and_flag_our_own() {
        let me = Keys::generate();
        let them = Keys::generate();
        let store = store_with(&me);
        let target = chat(&them, "room", "ship it");
        store.ingest(&target).unwrap();

        for (keys, emoji) in [(&me, "🎉"), (&them, "🎉"), (&them, "+")] {
            let reaction = EventBuilder::new(Kind::from_u16(kinds::REACTION), emoji)
                .tags([
                    Tag::parse(["e", &target.id.to_hex()]).unwrap(),
                    Tag::parse(["h", "room"]).unwrap(),
                ])
                .finalize(keys)
                .unwrap();
            store.ingest(&reaction).unwrap();
        }

        let messages = store.messages("room", 10, None).unwrap();
        let reactions = &messages[0].reactions;
        let party = reactions.iter().find(|r| r.emoji == "🎉").unwrap();
        assert_eq!(party.count, 2);
        assert!(party.mine, "our own reaction must be flagged");
        assert!(reactions.iter().any(|r| r.emoji == "\u{1f44d}"));
    }

    #[test]
    fn deletion_only_applies_to_self_authored_events() {
        let me = Keys::generate();
        let them = Keys::generate();
        let store = store_with(&me);
        let victim = chat(&them, "room", "important");
        store.ingest(&victim).unwrap();

        let forged = EventBuilder::new(Kind::from_u16(kinds::DELETION), "nope")
            .tag(Tag::parse(["e", &victim.id.to_hex()]).unwrap())
            .finalize(&me)
            .unwrap();
        assert_eq!(store.ingest(&forged).unwrap(), Ingested::Ignored);
        assert!(!store.messages("room", 10, None).unwrap()[0].deleted);

        let genuine = EventBuilder::new(Kind::from_u16(kinds::DELETION), "mine to remove")
            .tag(Tag::parse(["e", &victim.id.to_hex()]).unwrap())
            .finalize(&them)
            .unwrap();
        assert!(matches!(
            store.ingest(&genuine).unwrap(),
            Ingested::Deleted { .. }
        ));
        assert!(store.messages("room", 10, None).unwrap()[0].deleted);
    }

    #[test]
    fn moderator_deletion_bypasses_the_author_check() {
        let me = Keys::generate();
        let author = Keys::generate();
        let moderator = Keys::generate();
        let store = store_with(&me);
        let victim = chat(&author, "room", "off topic");
        store.ingest(&victim).unwrap();

        let takedown = EventBuilder::new(Kind::from_u16(kinds::ADMIN_DELETE), "spam")
            .tag(Tag::parse(["e", &victim.id.to_hex()]).unwrap())
            .finalize(&moderator)
            .unwrap();
        assert!(matches!(
            store.ingest(&takedown).unwrap(),
            Ingested::Deleted { .. }
        ));
        assert!(store.messages("room", 10, None).unwrap()[0].deleted);
    }

    #[test]
    fn edits_replace_the_body_and_only_from_the_author() {
        let me = Keys::generate();
        let them = Keys::generate();
        let store = store_with(&me);
        let original = chat(&me, "room", "teh typo");
        store.ingest(&original).unwrap();

        let hijack = EventBuilder::new(Kind::from_u16(kinds::CHAT_EDIT), "hijacked")
            .tag(Tag::parse(["e", &original.id.to_hex()]).unwrap())
            .finalize(&them)
            .unwrap();
        assert_eq!(store.ingest(&hijack).unwrap(), Ingested::Ignored);

        let fix = EventBuilder::new(Kind::from_u16(kinds::CHAT_EDIT), "the typo")
            .tag(Tag::parse(["e", &original.id.to_hex()]).unwrap())
            .custom_created_at(Timestamp::from_secs(1_700_000_500))
            .finalize(&me)
            .unwrap();
        assert!(matches!(
            store.ingest(&fix).unwrap(),
            Ingested::Edited { .. }
        ));

        let message = &store.messages("room", 10, None).unwrap()[0];
        assert_eq!(message.body, "the typo");
        assert!(message.edited);
    }

    #[test]
    fn unread_counts_exclude_our_own_messages_and_respect_the_read_marker() {
        let me = Keys::generate();
        let them = Keys::generate();
        let store = store_with(&me);
        store.ingest(&chat(&me, "room", "mine")).unwrap();
        let theirs = chat_at(&them, "room", "yours", 1_700_000_010);
        store.ingest(&theirs).unwrap();

        assert_eq!(store.channels().unwrap()[0].unread, 1);
        store.mark_read("room", 1_700_000_010).unwrap();
        assert_eq!(store.channels().unwrap()[0].unread, 0);
    }

    #[test]
    fn mentions_are_counted_separately() {
        let me = Keys::generate();
        let them = Keys::generate();
        let store = store_with(&me);
        let ping = EventBuilder::new(Kind::from_u16(kinds::CHAT), "ping")
            .tags([
                Tag::parse(["h", "room"]).unwrap(),
                Tag::parse(["p", &me.public_key().to_hex()]).unwrap(),
            ])
            .custom_created_at(Timestamp::from_secs(1_700_000_001))
            .finalize(&them)
            .unwrap();
        store.ingest(&ping).unwrap();
        store.ingest(&chat(&them, "room", "unrelated")).unwrap();

        let channel = &store.channels().unwrap()[0];
        assert_eq!(channel.unread, 2);
        assert_eq!(channel.mentions, 1);
    }

    #[test]
    fn channel_metadata_does_not_clobber_local_state() {
        let keys = Keys::generate();
        let store = store_with(&keys);
        store.ingest(&chat(&keys, "room", "hi")).unwrap();
        store.set_pinned("room", true).unwrap();
        store.mark_read("room", 1_800_000_000).unwrap();

        let meta = EventBuilder::new(Kind::from_u16(kinds::GROUP_METADATA), "")
            .tags([
                Tag::parse(["d", "room"]).unwrap(),
                Tag::parse(["name", "the room"]).unwrap(),
            ])
            .finalize(&keys)
            .unwrap();
        store.ingest(&meta).unwrap();

        let channel = &store.channels().unwrap()[0];
        assert_eq!(channel.name, "the room");
        assert!(channel.pinned, "a relay update must not reset local pins");
        assert_eq!(channel.unread, 0, "nor the read marker");
    }

    /// The local echo and the relay echo must agree about a message's parent,
    /// or a reply looks threaded until it round-trips and then does not.
    #[test]
    fn a_local_echo_records_the_same_parent_as_the_relay_echo() {
        let keys = Keys::generate();
        let store = store_with(&keys);
        let root_id = "a".repeat(64);

        let reply = EventBuilder::new(Kind::from_u16(kinds::CHAT), "threaded")
            .tags([
                Tag::parse(["h", "room"]).unwrap(),
                Tag::parse(["e", &root_id, "", "root"]).unwrap(),
            ])
            .custom_created_at(Timestamp::from_secs(1_700_000_100))
            .finalize(&keys)
            .unwrap();

        store.record_outgoing(&reply, "room").unwrap();
        let echoed = store.messages("room", 10, None).unwrap();
        let local = echoed.iter().find(|m| m.body == "threaded").unwrap();
        assert_eq!(
            local.parent.as_deref(),
            Some(root_id.as_str()),
            "a reply to the thread root is parented to it"
        );
        assert_eq!(local.root.as_deref(), Some(root_id.as_str()));

        // The relay echo settles delivery without disturbing the threading.
        store.ingest(&reply).unwrap();
        let settled = store.messages("room", 10, None).unwrap();
        let after = settled.iter().find(|m| m.body == "threaded").unwrap();
        assert_eq!(after.parent, local.parent);
        assert_eq!(after.delivery, Some(Delivery::Sent));
    }

    #[test]
    fn outgoing_messages_track_delivery() {
        let keys = Keys::generate();
        let store = store_with(&keys);
        let event = chat(&keys, "room", "optimistic");
        store.record_outgoing(&event, "room").unwrap();
        assert_eq!(
            store.messages("room", 10, None).unwrap()[0].delivery,
            Some(Delivery::Sending)
        );

        store
            .resolve_outgoing(&event.id.to_hex(), false, "invalid: no h tag")
            .unwrap();
        let message = &store.messages("room", 10, None).unwrap()[0];
        assert_eq!(message.delivery, Some(Delivery::Failed));
        assert_eq!(message.error.as_deref(), Some("invalid: no h tag"));

        // The relay echoing our own event back must settle it as delivered
        // rather than inserting a second copy.
        store.ingest(&event).unwrap();
        let messages = store.messages("room", 10, None).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].delivery, Some(Delivery::Sent));
        assert!(messages[0].error.is_none());
    }

    #[test]
    fn search_matches_prose_containing_fts_operators() {
        let keys = Keys::generate();
        let store = store_with(&keys);
        store
            .ingest(&chat(&keys, "room", "deploy failed AND rolled back"))
            .unwrap();
        let hits = store.search("failed AND rolled", None, 10).unwrap();
        assert_eq!(hits.len(), 1, "bare FTS operators must not break the query");
        assert!(store.search("nonexistent", None, 10).unwrap().is_empty());
    }

    #[test]
    fn deleted_messages_leave_the_search_index() {
        let keys = Keys::generate();
        let store = store_with(&keys);
        let event = chat(&keys, "room", "regrettable");
        store.ingest(&event).unwrap();
        assert_eq!(store.search("regrettable", None, 10).unwrap().len(), 1);

        let deletion = EventBuilder::new(Kind::from_u16(kinds::DELETION), "")
            .tag(Tag::parse(["e", &event.id.to_hex()]).unwrap())
            .finalize(&keys)
            .unwrap();
        store.ingest(&deletion).unwrap();
        assert!(store.search("regrettable", None, 10).unwrap().is_empty());
    }

    #[test]
    fn roster_records_roles_and_our_own_membership() {
        let me = Keys::generate();
        let store = store_with(&me);
        let admins = EventBuilder::new(Kind::from_u16(kinds::GROUP_ADMINS), "")
            .tags([
                Tag::parse(["d", "room"]).unwrap(),
                Tag::parse(["p", &me.public_key().to_hex(), "owner"]).unwrap(),
            ])
            .finalize(&me)
            .unwrap();
        store.ingest(&admins).unwrap();

        let members = store.members("room").unwrap();
        assert_eq!(members[0].1, proto::Role::Owner);
        assert!(store.channels().unwrap()[0].joined);
    }

    #[test]
    fn a_full_member_roster_does_not_demote_known_admins() {
        let me = Keys::generate();
        let admin = Keys::generate().public_key().to_hex();
        let store = store_with(&me);

        let admins = EventBuilder::new(Kind::from_u16(kinds::GROUP_ADMINS), "")
            .tags([
                Tag::parse(["d", "room"]).unwrap(),
                Tag::parse(["p", &admin, "admin"]).unwrap(),
            ])
            .finalize(&me)
            .unwrap();
        store.ingest(&admins).unwrap();

        let roster = EventBuilder::new(Kind::from_u16(kinds::GROUP_MEMBERS), "")
            .tags([
                Tag::parse(["d", "room"]).unwrap(),
                Tag::parse(["p", &admin]).unwrap(),
                Tag::parse(["p", &me.public_key().to_hex()]).unwrap(),
            ])
            .finalize(&me)
            .unwrap();
        store.ingest(&roster).unwrap();

        let members = store.members("room").unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].1, proto::Role::Admin);
    }
}
