//! Application state and the action dispatcher.
//!
//! Everything the interface can do arrives here as a [`Action`], whether it came
//! from a keystroke, a mouse click, a slash command, or an overlay. The renderer
//! reads this struct and writes nothing back except viewport measurements, which
//! it alone can know.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyEvent, MouseEvent, MouseEventKind};
use nostr::event::{
    Event, EventBuilder, FinalizeEvent, FinalizeUnsignedEvent, Kind, Tag, UnsignedEvent,
};
use nostr::filter::{Filter, SingleLetterTag};
use nostr::key::{Keys, PublicKey};
use nostr::nips::nip19::{FromBech32, ToBech32};
use nostr::nips::nip59::{GiftWrapBuilder, UnwrappedGift};
use nostr::types::Timestamp;
use semver::Version;
use tracing::{debug, warn};

use crate::approval::Request as ApprovalRequest;
use crate::composer::{Composer, token_start};
use crate::config::{Community, Config, Paths};
use crate::keys::{Action, Chord, Keymap, Resolution, Scope};
use crate::media::{Media, MediaEvent};
use crate::model::{Channel, ChannelKind, Delivery, Message, Presence, Profile};
use crate::net::{Command, ConnState, Relay, Update};
use crate::overlay::{
    Approval, Confirm, Confirmation, Help, Outcome, Overlay, Picker, PickerItem, PickerKind,
    Prompt, PromptKind, Search, Submission,
};
use crate::proto::{self, kinds};
use crate::store::{self, Ingested, Store};
use crate::ui::theme::Palette;
use crate::update::{Event as UpdateEvent, Request as UpdateRequest, Status as UpdateStatus};

/// Subscription ids. Fixed strings keep reconnect replay simple.
const SUB_DISCOVERY: &str = "discovery";
const SUB_NOTIFY: &str = "notify";
const SUB_FEED: &str = "feed";
const SUB_ACTIVE: &str = "active";
const SUB_PROFILES: &str = "profiles";
const SUB_SEARCH: &str = "search";
const SUB_BACKFILL: &str = "backfill";

/// How long a typing indicator stays on screen without a refresh.
const TYPING_TTL: Duration = Duration::from_secs(6);
/// How often we re-announce presence, comfortably inside any relay timeout.
const PRESENCE_INTERVAL: Duration = Duration::from_secs(60);
/// Minimum gap between typing indicators, so a fast typist does not flood.
const TYPING_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy)]
struct SlashCommand {
    name: &'static str,
    args: &'static str,
    help: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "join",
        args: "<channel>",
        help: "join a channel",
    },
    SlashCommand {
        name: "create",
        args: "<name>",
        help: "create a channel",
    },
    SlashCommand {
        name: "leave",
        args: "",
        help: "leave this channel",
    },
    SlashCommand {
        name: "dm",
        args: "<npub>",
        help: "open a direct message",
    },
    SlashCommand {
        name: "invite",
        args: "<npub>",
        help: "add a channel member",
    },
    SlashCommand {
        name: "kick",
        args: "<npub>",
        help: "remove a channel member",
    },
    SlashCommand {
        name: "topic",
        args: "[text]",
        help: "set the channel topic",
    },
    SlashCommand {
        name: "community",
        args: "[list|use|add]",
        help: "switch or add a community",
    },
    SlashCommand {
        name: "relay",
        args: "[url]",
        help: "change the relay",
    },
    SlashCommand {
        name: "search",
        args: "[query]",
        help: "search messages",
    },
    SlashCommand {
        name: "theme",
        args: "[name]",
        help: "change the color theme",
    },
    SlashCommand {
        name: "mute",
        args: "",
        help: "toggle channel notifications",
    },
    SlashCommand {
        name: "pin",
        args: "",
        help: "toggle the channel pin",
    },
    SlashCommand {
        name: "read",
        args: "",
        help: "mark this channel read",
    },
    SlashCommand {
        name: "approve",
        args: "",
        help: "approve an agent's pending request",
    },
    SlashCommand {
        name: "deny",
        args: "",
        help: "refuse an agent's pending request",
    },
    SlashCommand {
        name: "approvals",
        args: "[revoke]",
        help: "list or withdraw standing approvals",
    },
    SlashCommand {
        name: "me",
        args: "<action>",
        help: "send an emote",
    },
    SlashCommand {
        name: "whoami",
        args: "",
        help: "show your public key",
    },
    SlashCommand {
        name: "reload",
        args: "",
        help: "reload configuration",
    },
    SlashCommand {
        name: "update",
        args: "[install]",
        help: "check for or install an update",
    },
    SlashCommand {
        name: "help",
        args: "",
        help: "show keybindings",
    },
    SlashCommand {
        name: "keys",
        args: "",
        help: "show keybindings",
    },
    SlashCommand {
        name: "quit",
        args: "",
        help: "quit buzztui",
    },
];

/// Which pane owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Timeline,
    Thread,
    Composer,
}

impl Focus {
    /// The keymap scope this focus implies. Only the composer accepts bare
    /// printable keys as text.
    pub fn scope(self) -> Scope {
        match self {
            Focus::Composer => Scope::Insert,
            Focus::Sidebar | Focus::Timeline | Focus::Thread => Scope::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

/// A transient message in the corner. Toasts never carry information that is
/// not also recoverable elsewhere, because they disappear.
#[derive(Debug, Clone)]
pub struct Toast {
    pub kind: ToastKind,
    pub title: String,
    pub detail: Option<String>,
    pub expires: Instant,
}

/// Measurements the renderer takes and the dispatcher needs, such as how many
/// rows a page key should move.
#[derive(Debug, Default, Clone, Copy)]
pub struct Viewport {
    pub timeline_rows: u16,
    pub timeline_content: u16,
    pub sidebar_rows: u16,
    pub thread_rows: u16,
    pub thread_content: u16,
}

/// What the composer is currently doing with its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeMode {
    New,
    Reply { to: String, root: String },
    Edit { id: String },
}

pub struct App {
    pub config: Config,
    pub paths: Paths,
    pub keymap: Keymap,
    pub palette: Palette,
    pub store: Arc<Store>,
    pub relay: Relay,
    pub media: Media,
    keypair: Keys,
    pub me: String,

    pub running: bool,
    pub focus: Focus,
    /// Chords already pressed in an incomplete sequence.
    pub pending: Vec<Chord>,
    /// Continuations to advertise in the hint bar while a sequence is pending.
    pub hints: Vec<(Chord, Action)>,

    pub channels: Vec<Channel>,
    pub sidebar_cursor: usize,
    pub active: Option<String>,
    pub timeline: Vec<Message>,
    pub selected: Option<usize>,
    pub scroll: u16,
    pub follow: bool,
    pub viewport: Viewport,
    /// Root of the thread shown in the secondary conversation pane.
    pub thread_root: Option<String>,
    /// Selected row and scroll state are independent from the main timeline so
    /// opening a thread never moves the reader's place in the channel.
    pub thread_selected: Option<usize>,
    pub thread_scroll: u16,
    pub thread_follow: bool,

    pub profiles: HashMap<String, Profile>,
    /// Parents of loaded replies that are themselves outside the loaded page,
    /// so a quote never degrades to "a message above" when we hold the original.
    pub parents: HashMap<String, Message>,
    pub members: Vec<(String, proto::Role)>,
    pub presence: HashMap<String, Presence>,
    /// Pubkey to the moment their typing indicator expires.
    pub typing: HashMap<String, Instant>,

    pub composer: Composer,
    pub compose_mode: ComposeMode,
    /// Gift-wrap event id to the rumor it carries, so a relay verdict about a
    /// wrap can settle the plaintext row the timeline actually shows.
    pending_wraps: HashMap<String, String>,
    last_typing: Option<Instant>,
    last_presence: Option<Instant>,

    pub overlay: Option<Overlay>,
    /// Approval requests that arrived while another overlay held the screen.
    /// They queue rather than interrupt, because an agent is waiting on each one
    /// and a dropped request looks to it like silence.
    pending_approvals: VecDeque<Approval>,
    /// Agents allowed to proceed unattended for the rest of this session. The
    /// grant is deliberately not persisted: a standing permission to run
    /// dangerous commands should not outlive the sitting that granted it.
    approval_grants: HashSet<String>,
    pub toasts: Vec<Toast>,
    /// Configuration problems, shown as an in-band banner instead of a crash.
    pub diagnostics: Vec<String>,
    pub conn: ConnState,
    pub update: UpdateStatus,
    update_request: Option<UpdateRequest>,
    restart_requested: bool,
    announce_update_result: bool,

    pub show_sidebar: bool,
    pub show_members: bool,
    pub show_images: bool,
    pub compact: bool,
    pub show_timestamps: bool,
    pub mouse: bool,
    /// Set when something changed and the screen needs repainting.
    pub dirty: bool,
}

/// An install owns the one-shot update channel until its result arrives. A
/// second check must not detach that result or erase the restart state.
fn update_check_transition(status: &UpdateStatus) -> Option<(UpdateStatus, UpdateRequest)> {
    (!matches!(
        status,
        UpdateStatus::Installing(_) | UpdateStatus::Installed(_)
    ))
    .then_some((UpdateStatus::Checking, UpdateRequest::Check))
}

impl App {
    // Eight collaborators is what this application is made of; bundling them
    // into a struct purely to satisfy a lint would add a type with one use.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        paths: Paths,
        keypair: Keys,
        store: Arc<Store>,
        relay: Relay,
        media: Media,
        keymap: Keymap,
        diagnostics: Vec<String>,
    ) -> Self {
        let me = keypair.public_key().to_hex();
        let (palette, mut palette_problems) = build_palette(&config);
        palette_problems.extend(diagnostics);
        let diagnostics = palette_problems;
        let compact = config.ui.compact;
        let show_images = config.media.inline_images;

        let mut app = Self {
            config,
            paths,
            keymap,
            palette,
            store,
            relay,
            media,
            keypair,
            me,
            running: true,
            focus: Focus::Composer,
            pending: Vec::new(),
            hints: Vec::new(),
            channels: Vec::new(),
            sidebar_cursor: 0,
            active: None,
            timeline: Vec::new(),
            selected: None,
            scroll: 0,
            follow: true,
            viewport: Viewport::default(),
            thread_root: None,
            thread_selected: None,
            thread_scroll: 0,
            thread_follow: true,
            profiles: HashMap::new(),
            parents: HashMap::new(),
            members: Vec::new(),
            presence: HashMap::new(),
            typing: HashMap::new(),
            composer: Composer::new(),
            compose_mode: ComposeMode::New,
            pending_wraps: HashMap::new(),
            last_typing: None,
            last_presence: None,
            overlay: None,
            pending_approvals: VecDeque::new(),
            approval_grants: HashSet::new(),
            toasts: Vec::new(),
            diagnostics,
            conn: ConnState::Offline,
            update: UpdateStatus::Checking,
            update_request: None,
            restart_requested: false,
            announce_update_result: false,
            show_sidebar: true,
            show_members: false,
            show_images,
            compact,
            show_timestamps: true,
            mouse: true,
            dirty: true,
        };

        // Paint from the cache before the socket has said anything, so a
        // reconnect never shows an empty client.
        app.reload_channels();
        app.reload_profiles();
        if let Some(first) = app.channels.first().map(|c| c.id.clone()) {
            app.open_channel(&first);
        }
        app.subscribe_baseline();
        app
    }

    // -------------------------------------------------------- subscriptions

    /// Group discovery is only available through historical queries, since the
    /// relay stores those events channel-scoped and excludes them from global
    /// live fan-out.
    fn subscribe_baseline(&mut self) {
        self.relay.subscribe(
            SUB_DISCOVERY,
            vec![
                Filter::new()
                    .kinds(kinds::filter([
                        kinds::GROUP_METADATA,
                        kinds::GROUP_ADMINS,
                        kinds::GROUP_MEMBERS,
                    ]))
                    .limit(500),
            ],
        );

        // Membership notifications and gift wraps are p-gated: the relay rejects
        // any global subscription that does not filter on our own pubkey.
        if let Ok(me) = PublicKey::from_hex(&self.me) {
            self.relay.subscribe(
                SUB_NOTIFY,
                vec![
                    Filter::new()
                        .kinds(kinds::filter([
                            kinds::MEMBER_ADDED,
                            kinds::MEMBER_REMOVED,
                            kinds::GIFT_WRAP,
                        ]))
                        .pubkey(me)
                        .limit(200),
                ],
            );
        }
        self.subscribe_feed();
    }

    /// One subscription covers every known channel, which keeps unread counts
    /// live without opening a socket subscription per room.
    fn subscribe_feed(&mut self) {
        let ids: Vec<String> = self
            .channels
            .iter()
            .filter(|c| c.kind == ChannelKind::Group)
            .map(|c| c.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        self.relay.subscribe(
            SUB_FEED,
            vec![
                Filter::new()
                    .kinds(kinds::filter(kinds::CHANNEL_STREAM))
                    .custom_tags(SingleLetterTag::LOWERCASE_H, ids)
                    .limit(500),
            ],
        );
    }

    fn subscribe_active(&mut self, channel: &str) {
        if store::direct_peer(channel).is_some() {
            // Direct messages arrive through the gift-wrap subscription.
            self.relay.unsubscribe(SUB_ACTIVE);
            return;
        }
        self.relay.subscribe(
            SUB_ACTIVE,
            vec![
                Filter::new()
                    .kinds(kinds::filter(kinds::CHANNEL_STREAM))
                    .custom_tag(SingleLetterTag::LOWERCASE_H, channel)
                    .limit(self.config.ui.backfill as usize),
                Filter::new()
                    .kinds(kinds::filter([kinds::PRESENCE, kinds::TYPING]))
                    .custom_tag(SingleLetterTag::LOWERCASE_H, channel),
            ],
        );
    }

    /// Asks for profiles we are missing, in one query rather than per author.
    fn request_profiles(&mut self) {
        let missing: Vec<PublicKey> = self
            .timeline
            .iter()
            .map(|m| m.author.as_str())
            .chain(self.members.iter().map(|(pubkey, _)| pubkey.as_str()))
            .filter(|pubkey| !self.profiles.contains_key(*pubkey))
            .collect::<HashSet<_>>()
            .into_iter()
            .filter_map(|pubkey| PublicKey::from_hex(pubkey).ok())
            .take(200)
            .collect();
        if missing.is_empty() {
            return;
        }
        self.relay.query(
            SUB_PROFILES,
            vec![
                Filter::new()
                    .kinds(kinds::filter([kinds::METADATA]))
                    .authors(missing),
            ],
        );
    }

    // ------------------------------------------------------------- refreshes

    pub fn reload_channels(&mut self) {
        match self.store.channels() {
            Ok(channels) => {
                let active = self.active.clone();
                self.channels = channels;
                if let Some(active) = active
                    && let Some(index) = self.channels.iter().position(|c| c.id == active)
                {
                    self.sidebar_cursor = index;
                }
                self.sidebar_cursor = self
                    .sidebar_cursor
                    .min(self.channels.len().saturating_sub(1));
                self.dirty = true;

                // On a cold cache the sidebar fills in only once discovery
                // arrives, which is after startup; land the user somewhere
                // rather than leaving them looking at an empty client.
                if self.active.is_none()
                    && let Some(first) = self.channels.first().map(|c| c.id.clone())
                {
                    self.open_channel(&first);
                }
            }
            Err(err) => self.error("could not read channels", err.to_string()),
        }
    }

    fn reload_profiles(&mut self) {
        match self.store.profiles() {
            Ok(profiles) => self.profiles = profiles,
            Err(err) => self.error("could not read profiles", err.to_string()),
        }
    }

    pub fn reload_timeline(&mut self) {
        let Some(channel) = self.active.clone() else {
            self.timeline.clear();
            return;
        };
        match self
            .store
            .messages(&channel, self.config.ui.backfill as u32, None)
        {
            Ok(messages) => {
                self.timeline = messages;
                self.load_parents();
                self.clamp_selection();
                self.dirty = true;
            }
            Err(err) => self.error("could not read messages", err.to_string()),
        }
        match self.store.members(&channel) {
            Ok(members) => self.members = members,
            Err(err) => debug!(%err, "member list unavailable"),
        }
        self.request_profiles();
    }

    /// Fetches the parents of any reply whose original is not in the page.
    fn load_parents(&mut self) {
        let loaded: HashSet<&str> = self.timeline.iter().map(|m| m.id.as_str()).collect();
        let wanted: Vec<String> = self
            .timeline
            .iter()
            .filter_map(|m| m.parent.as_deref())
            .filter(|id| !loaded.contains(id) && !self.parents.contains_key(*id))
            .map(str::to_string)
            .collect();
        for id in wanted {
            match self.store.message(&id) {
                Ok(Some(parent)) => {
                    self.parents.insert(id, parent);
                }
                Ok(None) => {}
                Err(err) => debug!(%err, "could not load a quoted parent"),
            }
        }
    }

    /// A message by id, whether it is on screen or only in the cache.
    pub fn message(&self, id: &str) -> Option<&Message> {
        self.timeline
            .iter()
            .find(|m| m.id == id)
            .or_else(|| self.parents.get(id))
    }
    /// Messages belonging to one thread, in channel chronology. The root is
    /// included first when it is loaded; every descendant names the same NIP-10
    /// root even when its direct parent is another reply.
    pub fn thread_messages<'a>(
        &'a self,
        root: &'a str,
    ) -> impl DoubleEndedIterator<Item = (usize, &'a Message)> + 'a {
        self.timeline
            .iter()
            .enumerate()
            .filter(move |(_, message)| message.belongs_to_thread(root))
    }

    pub fn thread_reply_count(&self, root: &str) -> usize {
        self.thread_messages(root)
            .filter(|(_, message)| message.id != root)
            .count()
    }

    pub fn open_channel(&mut self, id: &str) {
        if self.active.as_deref() == Some(id) {
            return;
        }
        self.mark_active_read();
        self.active = Some(id.to_string());
        self.follow = true;
        self.scroll = 0;
        self.selected = None;
        self.thread_root = None;
        self.thread_selected = None;
        self.thread_scroll = 0;
        self.thread_follow = true;
        self.typing.clear();
        self.compose_mode = ComposeMode::New;
        if let Some(index) = self.channels.iter().position(|c| c.id == id) {
            self.sidebar_cursor = index;
        }
        self.reload_timeline();
        self.subscribe_active(id);
        self.mark_active_read();
        self.reload_channels();
    }

    fn mark_active_read(&mut self) {
        let Some(channel) = self.active.as_deref() else {
            return;
        };
        let Some(newest) = self.timeline.last().map(|m| m.created_at) else {
            return;
        };
        if let Err(err) = self.store.mark_read(channel, newest) {
            debug!(%err, "could not persist the read marker");
        }
    }

    fn clamp_selection(&mut self) {
        if self.timeline.is_empty() {
            self.selected = None;
            self.thread_selected = None;
            return;
        }
        let last = self.timeline.len() - 1;
        if let Some(index) = self.selected {
            self.selected = Some(index.min(last));
        }
        if let Some(index) = self.thread_selected {
            self.thread_selected = Some(index.min(last));
        }
    }

    pub fn active_channel(&self) -> Option<&Channel> {
        let id = self.active.as_deref()?;
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn selected_message(&self) -> Option<&Message> {
        let selected = if self.focus == Focus::Thread {
            self.thread_selected
        } else {
            self.selected
        };
        selected.and_then(|index| self.timeline.get(index))
    }

    /// The label for an author, preferring their profile name.
    pub fn display_name(&self, pubkey: &str) -> String {
        self.profiles
            .get(pubkey)
            .map(Profile::label)
            .unwrap_or_else(|| proto::short_pubkey(pubkey))
    }

    pub fn is_me(&self, pubkey: &str) -> bool {
        pubkey == self.me
    }

    /// Pubkeys currently typing in the open channel, excluding ourselves.
    pub fn typing_now(&self) -> Vec<String> {
        let now = Instant::now();
        let mut names: Vec<String> = self
            .typing
            .iter()
            .filter(|(pubkey, expiry)| **expiry > now && **pubkey != self.me)
            .map(|(pubkey, _)| self.display_name(pubkey))
            .collect();
        names.sort();
        names
    }

    /// Records the result of an asynchronous release check or installation.
    pub fn on_update(&mut self, event: UpdateEvent) {
        match event {
            UpdateEvent::Checked(status) => {
                match &status {
                    UpdateStatus::Available(latest) => self.toast(
                        ToastKind::Info,
                        format!("buzztui {latest} is available"),
                        Some("run /update install to download and verify it".to_string()),
                    ),
                    UpdateStatus::Current if self.announce_update_result => self.toast(
                        ToastKind::Success,
                        format!("buzztui v{} is current", env!("CARGO_PKG_VERSION")),
                        None,
                    ),
                    UpdateStatus::Unavailable if self.announce_update_result => self.toast(
                        ToastKind::Warn,
                        "update check unavailable",
                        Some("try /update again or visit the GitHub releases page".to_string()),
                    ),
                    _ => {}
                }
                self.announce_update_result = false;
                self.update = status;
            }
            UpdateEvent::Installed(version) => {
                self.update = UpdateStatus::Installed(version.clone());
                self.toast(
                    ToastKind::Success,
                    format!("buzztui v{version} installed"),
                    Some("restart to run the new executable".to_string()),
                );
                self.overlay = Some(Overlay::Confirm(Confirm::new(Confirmation::RestartUpdate(
                    version,
                ))));
            }
            UpdateEvent::InstallFailed { version, error } => {
                self.update = UpdateStatus::Available(version);
                self.error("update failed", error);
            }
        }
        self.dirty = true;
    }

    fn request_update_check(&mut self) {
        let Some((status, request)) = update_check_transition(&self.update) else {
            match &self.update {
                UpdateStatus::Installing(version) => {
                    self.info(format!("buzztui v{version} is already downloading"));
                }
                UpdateStatus::Installed(_) => self.info("restart to finish the installed update"),
                _ => unreachable!("only active or installed updates block checks"),
            }
            return;
        };
        self.update = status;
        self.update_request = Some(request);
        self.announce_update_result = true;
        self.info("checking for updates");
    }

    fn request_update_install(&mut self) {
        match self.update.clone() {
            UpdateStatus::Available(version) => {
                self.overlay = Some(Overlay::Confirm(Confirm::new(Confirmation::InstallUpdate(
                    version,
                ))));
            }
            UpdateStatus::Installing(version) => {
                self.info(format!("buzztui v{version} is already downloading"));
            }
            UpdateStatus::Installed(version) => {
                self.overlay = Some(Overlay::Confirm(Confirm::new(Confirmation::RestartUpdate(
                    version,
                ))));
            }
            UpdateStatus::Checking => self.info("wait for the update check to finish"),
            UpdateStatus::Current => self.info("no newer release is available"),
            UpdateStatus::Unavailable => self.info("run /update to check again first"),
        }
    }

    fn begin_update_install(&mut self, version: Version) {
        self.update = UpdateStatus::Installing(version.clone());
        self.update_request = Some(UpdateRequest::Install(version.clone()));
        self.info(format!("downloading and verifying buzztui v{version}"));
    }

    /// Lets the async event loop replace a completed update task on demand.
    pub fn take_update_request(&mut self) -> Option<UpdateRequest> {
        self.update_request.take()
    }

    pub fn take_restart_request(&mut self) -> bool {
        std::mem::take(&mut self.restart_requested)
    }

    // ------------------------------------------------------------ toasting

    pub fn toast(&mut self, kind: ToastKind, title: impl Into<String>, detail: Option<String>) {
        let ttl = match kind {
            ToastKind::Error => Duration::from_secs(10),
            ToastKind::Warn => Duration::from_secs(7),
            _ => Duration::from_secs(4),
        };
        self.toasts.push(Toast {
            kind,
            title: title.into(),
            detail,
            expires: Instant::now() + ttl,
        });
        // Three is as many as fit without covering the conversation.
        while self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
        self.dirty = true;
    }

    pub fn info(&mut self, title: impl Into<String>) {
        self.toast(ToastKind::Info, title, None);
    }

    pub fn error(&mut self, title: impl Into<String>, detail: impl Into<String>) {
        let detail = detail.into();
        warn!(detail, "error toast");
        self.toast(ToastKind::Error, title, Some(detail));
    }

    // -------------------------------------------------------------- input

    pub fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;

        // Overlays own the keyboard while they are open.
        if let Some(overlay) = self.overlay.as_mut() {
            match overlay.handle(key) {
                Outcome::Consumed => {
                    self.after_overlay_key();
                    return;
                }
                Outcome::Close => {
                    self.overlay = None;
                    self.open_next_approval();
                    return;
                }
                Outcome::Submit(submission) => {
                    self.overlay = None;
                    self.submit(submission);
                    self.open_next_approval();
                    return;
                }
                Outcome::Ignored => {}
            }
        }

        let chord = Chord::from_event(key);
        let scope = self.focus.scope();
        match self.keymap.resolve(scope, &self.pending, chord) {
            Resolution::Run(action) => {
                self.pending.clear();
                self.hints.clear();
                self.dispatch(action);
            }
            Resolution::Pending(next) => {
                self.pending.push(chord);
                self.hints = next;
            }
            Resolution::Unbound => {
                if !self.pending.is_empty() {
                    // An unknown continuation cancels the sequence rather than
                    // silently falling through and typing the key.
                    self.pending.clear();
                    self.hints.clear();
                    return;
                }
                // A letter that means nothing while browsing a thread is far
                // more likely to be the start of a reply than a mistake, so it
                // opens the composer rather than being discarded.
                if self.focus == Focus::Thread
                    && matches!(key.code, crossterm::event::KeyCode::Char(_))
                {
                    self.focus_composer();
                }
                if self.focus == Focus::Composer
                    && let crossterm::event::KeyCode::Char(c) = key.code
                {
                    let cursor = self.composer.cursor();
                    let before = &self.composer.text()[..cursor];
                    let token_start = before.chars().next_back().is_none_or(char::is_whitespace);
                    let command_start = c == '/' && before.trim().is_empty();
                    self.composer.insert_char(c);
                    self.note_typing();
                    if c == '@' && token_start {
                        self.open_mention_picker();
                    } else if command_start {
                        self.open_command_picker();
                    }
                }
            }
        }
    }

    /// Search re-runs as the query changes, which has to happen outside the
    /// overlay because only the app can reach the store and the relay.
    fn after_overlay_key(&mut self) {
        let Some(Overlay::Search(search)) = self.overlay.as_mut() else {
            return;
        };
        if !std::mem::take(&mut search.dirty) {
            return;
        }
        let query = search.query.text.clone();
        let scope = search.scope.clone();
        if query.trim().is_empty() {
            search.set_results(Vec::new());
            return;
        }
        match self.store.search(&query, scope.as_deref(), 200) {
            Ok(results) => {
                if let Some(Overlay::Search(search)) = self.overlay.as_mut() {
                    search.set_results(results);
                }
            }
            Err(err) => debug!(%err, "local search failed"),
        }
        // The relay's full-text index rejects shorter terms; local FTS still
        // gets to answer them.
        if query.trim().chars().count() < 3 {
            return;
        }
        let mut filter = Filter::new()
            .kinds(kinds::filter(kinds::TIMELINE))
            .search(query)
            .limit(50);
        if let Some(channel) = scope {
            filter = filter.custom_tag(SingleLetterTag::LOWERCASE_H, channel);
        }
        self.relay.query(SUB_SEARCH, vec![filter]);
    }

    pub fn on_mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::ScrollUp => self.dispatch(Action::ScrollUp),
            MouseEventKind::ScrollDown => self.dispatch(Action::ScrollDown),
            _ => {}
        }
    }

    // ----------------------------------------------------------- dispatch

    pub fn dispatch(&mut self, action: Action) {
        self.dirty = true;
        // Anything that would publish an `h`-tagged event has no private
        // equivalent, so in a gift-wrapped conversation it is refused outright
        // rather than quietly leaking metadata to the relay.
        if self.active_is_direct() && leaks_in_direct(action) {
            self.info("not available in direct messages");
            return;
        }
        match action {
            Action::Quit => self.quit(),
            Action::Cancel => self.cancel(),
            Action::Redraw => {}

            Action::FocusComposer => self.focus_composer(),
            // Stepping out of a reply lands in the thread it was addressed
            // from rather than the channel behind it, so leaving the composer
            // and closing the thread stay separate presses.
            Action::FocusTimeline => {
                self.focus = if self.focus == Focus::Composer && self.thread_root.is_some() {
                    Focus::Thread
                } else {
                    Focus::Timeline
                }
            }
            Action::FocusSidebar => self.focus = Focus::Sidebar,
            Action::CycleFocus => {
                if self.focus == Focus::Thread {
                    self.focus_composer();
                } else {
                    self.focus = match self.focus {
                        Focus::Sidebar => Focus::Timeline,
                        Focus::Timeline if self.thread_root.is_some() => Focus::Thread,
                        Focus::Timeline => Focus::Composer,
                        Focus::Composer => Focus::Sidebar,
                        Focus::Thread => unreachable!(),
                    };
                }
            }
            Action::CycleFocusBack => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Composer,
                    Focus::Timeline => Focus::Sidebar,
                    Focus::Thread => Focus::Timeline,
                    Focus::Composer if self.thread_root.is_some() => Focus::Thread,
                    Focus::Composer => Focus::Timeline,
                }
            }

            Action::NextChannel => self.step_channel(1, false),
            Action::PrevChannel => self.step_channel(-1, false),
            Action::NextUnread => self.step_channel(1, true),
            Action::PrevUnread => self.step_channel(-1, true),
            Action::JumpChannel(index) => {
                if let Some(channel) = self.channels.get(index as usize - 1) {
                    let id = channel.id.clone();
                    self.open_channel(&id);
                }
            }

            Action::SelectNext => self.move_selection(1),
            Action::SelectPrev => self.move_selection(-1),
            Action::ScrollUp => self.scroll_by(3),
            Action::ScrollDown => self.scroll_by(-3),
            Action::PageUp => {
                let rows = if self.focus == Focus::Thread {
                    self.viewport.thread_rows
                } else {
                    self.viewport.timeline_rows
                };
                self.scroll_by(rows.max(1) as i32 - 1);
            }
            Action::PageDown => {
                let rows = if self.focus == Focus::Thread {
                    self.viewport.thread_rows
                } else {
                    self.viewport.timeline_rows
                };
                self.scroll_by(-(rows.max(1) as i32 - 1));
            }
            Action::ScrollTop if self.focus == Focus::Thread => {
                self.thread_scroll = self
                    .viewport
                    .thread_content
                    .saturating_sub(self.viewport.thread_rows);
                self.thread_follow = false;
            }
            Action::ScrollTop => self.load_older(),
            Action::ScrollBottom if self.focus == Focus::Thread => {
                self.thread_follow = true;
                self.thread_scroll = 0;
                self.thread_selected = None;
            }
            Action::ScrollBottom => {
                self.follow = true;
                self.scroll = 0;
                self.selected = None;
            }
            Action::JumpFirstUnread => self.jump_first_unread(),

            Action::Send => self.send(),
            Action::Reply => self.begin_reply(),
            Action::EditMessage => self.begin_edit(),
            Action::DeleteMessage => self.confirm_delete(),
            Action::React => self.overlay = Some(Overlay::Picker(Picker::emoji())),
            Action::CopyMessage => self.copy_selected(),
            Action::QuoteMessage => self.quote_selected(),
            Action::OpenThread => self.open_thread(),
            Action::OpenLink => self.open_link(),
            Action::ViewImage => self.view_image(),
            Action::RetrySend => self.retry_failed(),
            Action::DiscardFailed => self.discard_failed(),

            Action::Paste => self.paste(),
            Action::Complete => self.complete_mention(),
            other if self.composer.apply(other) => {
                if matches!(
                    other,
                    Action::Newline
                        | Action::DeleteBack
                        | Action::DeleteForward
                        | Action::DeleteWordBack
                        | Action::DeleteWordForward
                ) {
                    self.note_typing();
                }
            }

            Action::CreateChannel => {
                self.overlay = Some(Overlay::Prompt(Prompt::new(PromptKind::CreateChannel)))
            }
            Action::JoinChannel => {
                self.overlay = Some(Overlay::Prompt(Prompt::new(PromptKind::JoinChannel)))
            }
            Action::OpenDirectMessage => {
                self.overlay = Some(Overlay::Prompt(Prompt::new(PromptKind::DirectMessage)))
            }
            Action::LeaveChannel => self.confirm_leave(),
            Action::ToggleMute => self.toggle_mute(),
            Action::TogglePin => self.toggle_pin(),
            Action::MarkRead => {
                self.mark_active_read();
                self.reload_channels();
            }
            Action::MarkAllRead => self.mark_all_read(),
            Action::OpenSwitcher => self.open_switcher(),
            Action::OpenCommunitySwitcher => self.open_community_switcher(),
            Action::OpenSearch => {
                self.overlay = Some(Overlay::Search(Search::new(self.active.clone())))
            }
            Action::OpenMembers => {
                self.show_members = !self.show_members;
            }
            Action::CopyIdentity => self.copy_identity(),
            Action::OpenProfile => {
                if let Some(author) = self.selected_message().map(|m| m.author.clone()) {
                    self.overlay = Some(Overlay::Profile(author));
                }
            }

            Action::ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            Action::ToggleMemberPane => self.show_members = !self.show_members,
            Action::ToggleImages => {
                self.show_images = !self.show_images;
                let state = if self.show_images { "on" } else { "off" };
                self.info(format!("inline images {state}"));
            }
            Action::ToggleCompact => self.compact = !self.compact,
            Action::ToggleTimestamps => self.show_timestamps = !self.show_timestamps,
            Action::CycleTheme => self.cycle_theme(),

            Action::OpenHelp => self.overlay = Some(Overlay::Help(Help::new(&self.keymap))),
            Action::OpenCommand => {
                self.focus = Focus::Composer;
                self.composer.set_text("/");
                self.open_command_picker();
            }
            Action::ReloadConfig => self.reload_config(),

            // Editing actions the composer declined, which means there is
            // nothing sensible left to do with them.
            _ => {}
        }
    }
    /// Entering the composer from a thread targets the currently highlighted
    /// reply, while preserving the thread root for NIP-10.
    fn focus_composer(&mut self) {
        if self.focus == Focus::Thread
            && self.compose_mode == ComposeMode::New
            && let Some(message) = self.selected_message()
        {
            self.compose_mode = ComposeMode::Reply {
                to: message.id.clone(),
                root: self
                    .thread_root
                    .clone()
                    .unwrap_or_else(|| message.id.clone()),
            };
        }
        self.focus = Focus::Composer;
    }

    /// Quitting is immediate unless it would silently discard something the
    /// user typed, which is the one case worth a keystroke of friction.
    fn quit(&mut self) {
        if self.composer.text().trim().is_empty() {
            self.running = false;
            return;
        }
        self.overlay = Some(Overlay::Confirm(Confirm::new(Confirmation::Quit)));
    }

    fn cancel(&mut self) {
        if self.overlay.take().is_some() {
            return;
        }
        if !self.pending.is_empty() {
            self.pending.clear();
            self.hints.clear();
            return;
        }
        if self.compose_mode != ComposeMode::New {
            self.compose_mode = ComposeMode::New;
            self.composer.clear();
            if self.thread_root.is_some() {
                self.focus = Focus::Thread;
            }
            return;
        }
        if self.focus == Focus::Thread {
            self.close_thread();
            return;
        }
        if self.focus == Focus::Composer && self.thread_root.is_some() {
            self.focus = Focus::Thread;
            return;
        }
        if self.selected.take().is_some() {
            return;
        }
        self.focus = Focus::Composer;
    }

    // -------------------------------------------------------- navigation

    fn step_channel(&mut self, delta: isize, unread_only: bool) {
        if self.channels.is_empty() {
            return;
        }
        let len = self.channels.len();
        let start = self
            .active
            .as_deref()
            .and_then(|id| self.channels.iter().position(|c| c.id == id))
            .unwrap_or(0);
        for hop in 1..=len {
            let index = ((start as isize + delta * hop as isize).rem_euclid(len as isize)) as usize;
            let candidate = &self.channels[index];
            if !unread_only || candidate.unread > 0 {
                let id = candidate.id.clone();
                self.open_channel(&id);
                return;
            }
        }
        if unread_only {
            self.info("no unread channels");
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.focus == Focus::Sidebar {
            if self.channels.is_empty() {
                return;
            }
            let len = self.channels.len() as isize;
            self.sidebar_cursor = ((self.sidebar_cursor as isize + delta).rem_euclid(len)) as usize;
            let id = self.channels[self.sidebar_cursor].id.clone();
            self.open_channel(&id);
            return;
        }
        if self.focus == Focus::Thread {
            let Some(root) = self.thread_root.as_deref() else {
                return;
            };
            let indices: Vec<usize> = self.thread_messages(root).map(|(index, _)| index).collect();
            if indices.is_empty() {
                return;
            }
            let last = indices.len() - 1;
            let position = self
                .thread_selected
                .and_then(|selected| indices.iter().position(|index| *index == selected));
            let position = match position {
                None => last,
                Some(position) => (position as isize + delta).clamp(0, last as isize) as usize,
            };
            self.thread_selected = Some(indices[position]);
            self.thread_follow = false;
            return;
        }
        if self.timeline.is_empty() {
            return;
        }
        let last = self.timeline.len() - 1;
        self.selected = Some(match self.selected {
            // Selection always begins at the newest message, whichever
            // direction the key implies: the view is pinned to the bottom, so
            // that is where the eye already is.
            None => last,
            Some(index) => (index as isize + delta).clamp(0, last as isize) as usize,
        });
        self.follow = false;
    }

    fn scroll_by(&mut self, rows: i32) {
        if self.focus == Focus::Thread {
            let max = self
                .viewport
                .thread_content
                .saturating_sub(self.viewport.thread_rows);
            let next = (self.thread_scroll as i32 + rows).clamp(0, max as i32) as u16;
            self.thread_scroll = next;
            self.thread_follow = next == 0;
            return;
        }
        let max = self
            .viewport
            .timeline_content
            .saturating_sub(self.viewport.timeline_rows);
        let next = (self.scroll as i32 + rows).clamp(0, max as i32) as u16;
        self.scroll = next;
        self.follow = next == 0;
        if self.follow {
            self.mark_active_read();
        }
        // Reaching the top is the natural moment to fetch the previous page.
        if next >= max && max > 0 {
            self.load_older();
        }
    }

    fn jump_first_unread(&mut self) {
        let Some(channel) = self.active_channel() else {
            return;
        };
        if channel.unread == 0 {
            self.info("nothing unread here");
            return;
        }
        let unread = channel.unread as usize;
        let index = self.timeline.len().saturating_sub(unread);
        self.selected = Some(index.min(self.timeline.len().saturating_sub(1)));
        self.follow = false;
    }

    /// Requests the page of history before the oldest message we hold.
    fn load_older(&mut self) {
        let (Some(channel), Some(oldest)) = (
            self.active.clone(),
            self.timeline.first().map(|m| m.created_at),
        ) else {
            return;
        };
        if store::direct_peer(&channel).is_some() {
            return;
        }
        self.relay.query(
            SUB_BACKFILL,
            vec![
                Filter::new()
                    .kinds(kinds::filter(kinds::TIMELINE))
                    .custom_tag(SingleLetterTag::LOWERCASE_H, channel)
                    .until(Timestamp::from_secs(oldest.max(1) as u64 - 1))
                    .limit(self.config.ui.backfill as usize),
            ],
        );
    }

    // ------------------------------------------------------------ sending

    /// True when the open conversation is gift-wrapped, in which case nothing
    /// may be published as an ordinary channel-tagged event.
    pub fn active_is_direct(&self) -> bool {
        self.active
            .as_deref()
            .is_some_and(|id| store::direct_peer(id).is_some())
    }

    fn send(&mut self) {
        let body = self.composer.text().trim().to_string();
        if body.is_empty() {
            return;
        }
        if let Some(command) = body.strip_prefix('/') {
            let command = command.to_string();
            self.composer.take();
            self.run_command(&command);
            return;
        }
        let Some(channel) = self.active.clone() else {
            self.error("no channel open", "open a channel before sending");
            return;
        };

        let mode = std::mem::replace(&mut self.compose_mode, ComposeMode::New);

        // A direct message never touches the public builders: the whole point of
        // a gift wrap is that the relay learns nothing but the recipient.
        if let Some(peer) = store::direct_peer(&channel).map(str::to_string) {
            if let ComposeMode::Edit { .. } = mode {
                self.error(
                    "cannot edit a direct message",
                    "edits are a channel feature; send a correction instead",
                );
                return;
            }
            let reply = match &mode {
                ComposeMode::Reply { to, root } => Some((to.clone(), root.clone())),
                _ => None,
            };
            match self.send_direct(&peer, &channel, &body, reply) {
                Ok(sent_id) => {
                    self.composer.take();
                    self.follow = true;
                    self.scroll = 0;
                    self.reload_timeline();
                    self.compose_mode =
                        compose_mode_after_send(&mode, self.thread_root.as_deref(), sent_id);
                }
                Err(err) => {
                    self.compose_mode = mode;
                    self.error("could not send", err.to_string());
                }
            }
            return;
        }

        let built = match &mode {
            ComposeMode::Edit { id } => self.build_edit(&channel, id, &body),
            ComposeMode::Reply { to, root } => self.build_reply(&channel, to, root, &body),
            ComposeMode::New => self.build_message(&channel, &body),
        };

        match built {
            Ok(event) => {
                let sent_id = event.id.to_hex();
                self.composer.take();
                // An edit is not a new message, so it must not appear as one.
                if !matches!(mode, ComposeMode::Edit { .. })
                    && let Err(err) = self.store.record_outgoing(&event, &channel)
                {
                    debug!(%err, "could not record the local echo");
                }
                self.relay.publish(event);
                self.follow = true;
                self.scroll = 0;
                self.reload_timeline();
                self.compose_mode =
                    compose_mode_after_send(&mode, self.thread_root.as_deref(), sent_id);
            }
            Err(err) => {
                self.compose_mode = mode;
                self.error("could not send", err.to_string());
            }
        }
    }

    fn build_message(&self, channel: &str, body: &str) -> Result<Event> {
        let event = EventBuilder::new(Kind::from_u16(kinds::CHAT), body)
            .tag(Tag::parse(["h", channel])?)
            .tags(self.mention_tags(body))
            .finalize(&self.keypair)?;
        Ok(event)
    }

    fn build_reply(&self, channel: &str, parent: &str, root: &str, body: &str) -> Result<Event> {
        let mut tags = vec![Tag::parse(["h", channel])?];
        tags.extend(thread_tags(root, parent)?);
        // Tag the author so their client can surface the reply as a mention.
        if let Some(author) = self.message(parent).map(|m| m.author.clone()) {
            tags.push(Tag::parse(["p", &author])?);
        }
        tags.extend(self.mention_tags(body));
        Ok(EventBuilder::new(Kind::from_u16(kinds::CHAT), body)
            .tags(tags)
            .finalize(&self.keypair)?)
    }

    fn build_edit(&self, channel: &str, target: &str, body: &str) -> Result<Event> {
        Ok(EventBuilder::new(Kind::from_u16(kinds::CHAT_EDIT), body)
            .tags([Tag::parse(["h", channel])?, Tag::parse(["e", target])?])
            .finalize(&self.keypair)?)
    }

    /// Sends a NIP-17 direct message, replying inside the rumor when asked.
    ///
    /// The rumor is wrapped twice, once for the recipient and once for
    /// ourselves, because a gift wrap is readable only by the key it addresses
    /// and we would otherwise never see our own sent messages. The plaintext
    /// rumor is what the timeline stores; the self-addressed wrap is recorded as
    /// already opened so its return trip is not decrypted a second time.
    fn send_direct(
        &mut self,
        peer: &str,
        channel: &str,
        body: &str,
        reply: Option<(String, String)>,
    ) -> Result<String> {
        let receiver = PublicKey::from_hex(peer)?;
        let mut tags = vec![Tag::parse(["p", peer])?];
        if let Some((parent, root)) = &reply {
            tags.extend(thread_tags(root, parent)?);
        }
        tags.extend(self.mention_tags(body));

        let (mut rumor, to_peer, to_self) = direct_wraps(&self.keypair, receiver, body, tags)?;
        let rumor_id = rumor.id().to_hex();

        let tags_json = serde_json::to_string(&rumor.tags)?;
        let record = store::Rumor {
            id: &rumor_id,
            author: &self.me,
            created_at: rumor.created_at.as_secs() as i64,
            body,
            tags: &tags_json,
            parent: reply.as_ref().map(|(parent, _)| parent.as_str()),
            root: reply.as_ref().map(|(_, root)| root.as_str()),
            mentions_me: false,
        };
        self.store.record_outgoing_rumor(&record, channel)?;
        self.store
            .note_gift_wrap(&to_self.id.to_hex(), Some(&rumor_id))?;

        // Delivery is decided by the recipient's copy alone. The self-addressed
        // wrap is an archival duplicate, and letting its verdict settle the row
        // would report success or failure for the wrong event.
        self.pending_wraps
            .insert(to_peer.id.to_hex(), rumor_id.clone());

        self.relay.publish(to_peer);
        self.relay.publish(to_self);
        Ok(rumor_id)
    }

    /// `p` tags for everyone the body mentions, so their client can notify.
    ///
    /// Both spellings resolve: a raw `npub`, and an `@name` matched against the
    /// channel roster. Accepting the readable form is what lets the composer
    /// insert `@alice` instead of sixty-three characters of bech32.
    fn mention_tags(&self, body: &str) -> Vec<Tag> {
        let mut mentioned: Vec<String> = Vec::new();
        for word in body.split_whitespace() {
            let candidate = word.trim_end_matches([',', '.', ':', ';', '!', '?']);
            let bare = candidate.trim_start_matches('@');
            if let Ok(pubkey) = PublicKey::from_bech32(bare) {
                mentioned.push(pubkey.to_hex());
                continue;
            }
            if candidate.starts_with('@')
                && let Some(pubkey) = self.pubkey_for_name(bare)
            {
                mentioned.push(pubkey);
            }
        }
        mentioned.sort();
        mentioned.dedup();
        mentioned
            .iter()
            .filter_map(|pubkey| Tag::parse(["p", pubkey]).ok())
            .collect()
    }

    /// The roster member whose display name or composer-safe mention token
    /// matches, case-insensitively.
    fn pubkey_for_name(&self, name: &str) -> Option<String> {
        let needle = name.to_lowercase();
        self.members
            .iter()
            .map(|(pubkey, _)| pubkey)
            .find(|pubkey| {
                let display = self.display_name(pubkey);
                display.to_lowercase() == needle || mention_token(&display).to_lowercase() == needle
            })
            .cloned()
    }

    fn begin_reply(&mut self) {
        let Some(message) = self.selected_message() else {
            self.info("select a message first");
            return;
        };
        let root = message.root.clone().unwrap_or_else(|| message.id.clone());
        self.compose_mode = ComposeMode::Reply {
            to: message.id.clone(),
            root,
        };
        self.focus = Focus::Composer;
    }

    fn begin_edit(&mut self) {
        let Some(message) = self.selected_message() else {
            self.info("select a message first");
            return;
        };
        if !self.is_me(&message.author) {
            self.info("you can only edit your own messages");
            return;
        }
        if self.active_is_direct() {
            self.info("direct messages cannot be edited");
            return;
        }
        let body = message.body.clone();
        let id = message.id.clone();
        self.compose_mode = ComposeMode::Edit { id };
        self.composer.set_text(&body);
        self.focus = Focus::Composer;
    }

    fn confirm_delete(&mut self) {
        let Some(message) = self.selected_message() else {
            self.info("select a message first");
            return;
        };
        if !self.is_me(&message.author) {
            self.info("you can only delete your own messages");
            return;
        }
        self.overlay = Some(Overlay::Confirm(Confirm::new(Confirmation::DeleteMessage(
            message.id.clone(),
        ))));
    }

    fn confirm_leave(&mut self) {
        let Some(channel) = self.active_channel() else {
            return;
        };
        self.overlay = Some(Overlay::Confirm(Confirm::new(Confirmation::LeaveChannel(
            channel.name.clone(),
        ))));
    }

    fn react(&mut self, emoji: &str) {
        let (Some(channel), Some(message)) = (self.active.clone(), self.selected_message()) else {
            self.info("select a message first");
            return;
        };
        let target = message.id.clone();
        let author = message.author.clone();
        let built = (|| -> Result<Event> {
            Ok(EventBuilder::new(Kind::from_u16(kinds::REACTION), emoji)
                .tags([
                    Tag::parse(["e", &target])?,
                    Tag::parse(["h", &channel])?,
                    Tag::parse(["p", &author])?,
                ])
                .finalize(&self.keypair)?)
        })();
        match built {
            Ok(event) => self.relay.publish(event),
            Err(err) => self.error("could not react", err.to_string()),
        }
    }

    fn retry_failed(&mut self) {
        let Some(message) = self
            .selected_message()
            .filter(|m| matches!(m.delivery, Some(Delivery::Failed)))
        else {
            self.info("no failed message selected");
            return;
        };
        let body = message.body.clone();
        let id = message.id.clone();
        let channel = message.channel.clone();
        let thread = message
            .parent
            .clone()
            .map(|parent| (parent.clone(), message.root.clone().unwrap_or(parent)));

        if let Err(err) = self.store.discard_outgoing(&id) {
            debug!(%err, "could not discard the failed message");
        }

        // A failed direct message must be re-wrapped, never re-sent as a public
        // channel event: the retry path is where that mistake would hide.
        if let Some(peer) = store::direct_peer(&channel).map(str::to_string) {
            match self.send_direct(&peer, &channel, &body, thread) {
                Ok(_) => self.reload_timeline(),
                Err(err) => self.error("could not resend", err.to_string()),
            }
            return;
        }

        let built = match &thread {
            Some((parent, root)) => self.build_reply(&channel, parent, root, &body),
            None => self.build_message(&channel, &body),
        };
        match built {
            Ok(event) => {
                let _ = self.store.record_outgoing(&event, &channel);
                self.relay.publish(event);
                self.reload_timeline();
            }
            Err(err) => self.error("could not resend", err.to_string()),
        }
    }

    fn discard_failed(&mut self) {
        let Some(message) = self
            .selected_message()
            .filter(|m| matches!(m.delivery, Some(Delivery::Failed)))
        else {
            return;
        };
        let id = message.id.clone();
        if let Err(err) = self.store.discard_outgoing(&id) {
            self.error("could not discard", err.to_string());
            return;
        }
        self.reload_timeline();
    }

    /// Publishes a typing indicator, rate limited so holding a key does not
    /// turn into a stream of ephemeral events.
    fn note_typing(&mut self) {
        if !self.config.ui.send_typing || !self.conn.is_ready() {
            return;
        }
        let Some(channel) = self.active.clone() else {
            return;
        };
        if store::direct_peer(&channel).is_some() {
            return;
        }
        let now = Instant::now();
        if self
            .last_typing
            .is_some_and(|last| now.duration_since(last) < TYPING_INTERVAL)
        {
            return;
        }
        self.last_typing = Some(now);
        if let Ok(tag) = Tag::parse(["h", channel.as_str()])
            && let Ok(event) = EventBuilder::new(Kind::from_u16(kinds::TYPING), "")
                .tag(tag)
                .finalize(&self.keypair)
        {
            self.relay.publish(event);
        }
    }

    // ------------------------------------------------------------ clipboard

    fn paste(&mut self) {
        match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(text) => {
                self.composer.insert_str(text.trim_end_matches('\n'));
                self.focus = Focus::Composer;
            }
            Err(err) => self.error("clipboard unavailable", err.to_string()),
        }
    }

    /// Your public key, as an npub.
    pub fn npub(&self) -> String {
        self.keypair
            .public_key()
            .to_bech32()
            .unwrap_or_else(|_| self.me.clone())
    }

    /// The reason the relay refused this identity, when the refusal is about
    /// who you are rather than whether the network is reachable. The two need
    /// completely different advice, so they are told apart here.
    pub fn membership_rejected(&self) -> Option<&str> {
        let ConnState::Failed(reason) = &self.conn else {
            return None;
        };
        const MARKERS: [&str; 5] = [
            "not a relay member",
            "restricted:",
            "auth-required",
            "auth rejected",
            "allowlist",
        ];
        let lowered = reason.to_lowercase();
        MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
            .then_some(reason.as_str())
    }

    /// Puts a ready-to-send request on the clipboard. Being told "not a relay
    /// member" is useless on its own, so hand over both things an administrator
    /// needs rather than making the user go and look them up.
    fn copy_identity(&mut self) {
        let npub = self.npub();
        let request = format!(
            "Please add me to the Buzz relay at {}.\n\nMy public key:\n{npub}\n\nTo add me:\n  buzz-admin add-member --pubkey {npub}\n",
            self.config.relay(),
        );
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(request)) {
            Ok(()) => self.toast(
                ToastKind::Success,
                "request copied",
                Some("paste it to whoever runs the relay".to_string()),
            ),
            // Without a clipboard, the key itself is still what they need.
            Err(_) => self.toast(
                ToastKind::Info,
                npub,
                Some("copy this to be added".to_string()),
            ),
        }
    }

    fn copy_selected(&mut self) {
        let Some(message) = self.selected_message() else {
            self.info("select a message first");
            return;
        };
        let body = message.body.clone();
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(body)) {
            Ok(()) => self.toast(ToastKind::Success, "copied", None),
            Err(err) => self.error("clipboard unavailable", err.to_string()),
        }
    }

    fn quote_selected(&mut self) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let quoted: String = message
            .body
            .lines()
            .map(|line| format!("> {line}\n"))
            .collect();
        self.composer.insert_str(&quoted);
        self.focus = Focus::Composer;
    }

    fn open_thread(&mut self) {
        let Some(message) = self.selected_message() else {
            self.info("select a message first");
            return;
        };
        let root = message.root.clone().unwrap_or_else(|| message.id.clone());
        self.thread_selected = self
            .timeline
            .iter()
            .enumerate()
            .rfind(|(_, message)| message.belongs_to_thread(&root))
            .map(|(index, _)| index);
        self.thread_root = Some(root);
        self.thread_scroll = 0;
        self.thread_follow = true;
        // Opening a thread is nearly always the prelude to answering it, so the
        // composer takes the keyboard with the reply already addressed. Browsing
        // the thread is one `esc` away, which is where its navigation keys live;
        // landing in that mode instead would swallow the first thing typed.
        self.focus = Focus::Thread;
        self.focus_composer();
    }

    fn close_thread(&mut self) {
        self.thread_root = None;
        self.thread_selected = None;
        self.thread_scroll = 0;
        self.thread_follow = true;
        self.focus = Focus::Timeline;
    }

    fn open_link(&mut self) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let Some(url) = crate::ui::text::links(&message.body)
            .first()
            .map(|u| u.to_string())
        else {
            self.info("no link in this message");
            return;
        };
        match open::that_detached(&url) {
            Ok(()) => self.info(format!("opened {url}")),
            Err(err) => self.error("could not open link", err.to_string()),
        }
    }

    fn view_image(&mut self) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let Some(url) = crate::ui::text::image_links(&message.body)
            .first()
            .map(|u| u.to_string())
        else {
            self.info("no image in this message");
            return;
        };
        let url = self.config.resolve_media(&url);
        self.media.request(&url);
        self.overlay = Some(Overlay::Image(url));
    }

    fn mention_picker(&self, query: &str) -> Picker {
        let mut items: Vec<PickerItem> = self
            .members
            .iter()
            .map(|(pubkey, role)| {
                let name = self.display_name(pubkey);
                PickerItem {
                    id: pubkey.clone(),
                    label: format!("@{}  {name}", mention_token(&name)),
                    detail: Some(format!(
                        "{} \u{00b7} {}",
                        role.as_str(),
                        proto::short_pubkey(pubkey)
                    )),
                    badge: self
                        .presence
                        .get(pubkey)
                        .map(|presence| presence.dot().to_string()),
                }
            })
            .collect();
        items.sort_by_key(|item| item.label.to_lowercase());
        Picker::new(PickerKind::Mention, "mention a member", items).with_query(query)
    }

    fn open_mention_picker(&mut self) {
        self.overlay = Some(Overlay::Picker(self.mention_picker("")));
    }

    fn open_command_picker(&mut self) {
        let items = SLASH_COMMANDS
            .iter()
            .map(|command| PickerItem {
                id: command.name.to_string(),
                label: format!("/{:<11} {}", command.name, command.args),
                detail: Some(command.help.to_string()),
                badge: None,
            })
            .collect();
        self.overlay = Some(Overlay::Picker(Picker::new(
            PickerKind::Command,
            "run a command",
            items,
        )));
    }

    fn insert_mention(&mut self, pubkey: &str) {
        let name = self.display_name(pubkey);
        self.composer.complete_token('@', &mention_token(&name));
        self.focus = Focus::Composer;
        self.note_typing();
    }

    fn insert_command(&mut self, command: &str) {
        self.composer.complete_token('/', command);
        self.focus = Focus::Composer;
    }

    /// Reopens the fuzzy member picker with the partial token already typed.
    fn complete_mention(&mut self) {
        let text = self.composer.text();
        let head = &text[..self.composer.cursor()];
        let start = token_start(head);
        let query = head[start..].trim_start_matches('@');
        self.overlay = Some(Overlay::Picker(self.mention_picker(query)));
    }

    // ------------------------------------------------------------- channels

    fn open_switcher(&mut self) {
        let items = self
            .channels
            .iter()
            .map(|channel| PickerItem {
                id: channel.id.clone(),
                label: format!("{} {}", channel.sigil(), channel.name),
                detail: channel
                    .about
                    .clone()
                    .or_else(|| relative_time(channel.last_activity)),
                badge: (channel.unread > 0).then(|| channel.unread.to_string()),
            })
            .collect();
        self.overlay = Some(Overlay::Picker(Picker::new(
            PickerKind::Channel,
            "jump to channel",
            items,
        )));
    }
    fn open_community_switcher(&mut self) {
        let current = self
            .config
            .current_community()
            .map(|(name, _)| name.to_string())
            .unwrap_or_default();
        let items = self
            .config
            .communities
            .iter()
            .map(|(name, community)| PickerItem {
                id: name.clone(),
                label: name.clone(),
                detail: Some(community.relay.clone()),
                badge: (name == &current).then(|| "current".to_string()),
            })
            .collect();
        self.overlay = Some(Overlay::Picker(Picker::new(
            PickerKind::Community,
            "switch community",
            items,
        )));
    }

    fn toggle_mute(&mut self) {
        let Some(channel) = self.active_channel().cloned() else {
            return;
        };
        if let Err(err) = self.store.set_muted(&channel.id, !channel.muted) {
            self.error("could not mute", err.to_string());
            return;
        }
        self.reload_channels();
        self.info(if channel.muted { "unmuted" } else { "muted" });
    }

    fn toggle_pin(&mut self) {
        let Some(channel) = self.active_channel().cloned() else {
            return;
        };
        if let Err(err) = self.store.set_pinned(&channel.id, !channel.pinned) {
            self.error("could not pin", err.to_string());
            return;
        }
        self.reload_channels();
    }

    fn mark_all_read(&mut self) {
        let now = Timestamp::now().as_secs() as i64;
        for channel in self.channels.clone() {
            if let Err(err) = self.store.mark_read(&channel.id, now) {
                debug!(%err, "could not mark {} read", channel.id);
            }
        }
        self.reload_channels();
        self.info("all caught up");
    }

    fn cycle_theme(&mut self) {
        let next = Palette::next_theme(&self.config.ui.theme);
        self.config.ui.theme = next.to_string();
        self.palette = build_palette(&self.config).0;
        if let Err(err) = self.config.save(&self.paths) {
            debug!(%err, "could not persist the theme");
        }
        self.info(format!("theme: {next}"));
    }

    fn reload_config(&mut self) {
        let paths = self.paths.clone();
        match Config::load_or_create(&paths) {
            Ok(mut config) => {
                config.apply_env();
                let relay_changed = config.relay() != self.config.relay();
                let prepared = if relay_changed {
                    let Some((name, community)) = config
                        .current_community()
                        .map(|(name, community)| (name.to_string(), community.clone()))
                    else {
                        self.error("could not reload configuration", "no current community");
                        return;
                    };
                    let store = match self.open_community_store(&community) {
                        Ok(store) => store,
                        Err(err) => {
                            self.error("could not open community", err.to_string());
                            return;
                        }
                    };
                    Some((name, community, store))
                } else {
                    None
                };
                let (palette, mut problems) = build_palette(&config);
                self.palette = palette;
                self.config = config;
                let (file, key_problems) =
                    crate::keys::KeyFile::load(&paths.root.join("keys.toml"));
                let keymap = Keymap::with_overrides(file);
                problems.extend(key_problems);
                problems.extend(keymap.diagnostics.iter().cloned());
                self.keymap = keymap;
                self.diagnostics = problems;
                if let Some((name, community, store)) = prepared {
                    self.install_community(&name, &community, store);
                }
                self.info("configuration reloaded");
            }
            Err(err) => self.error("could not reload configuration", err.to_string()),
        }
    }

    // ------------------------------------------------------------ overlays

    fn submit(&mut self, submission: Submission) {
        match submission {
            Submission::OpenChannel(id) => self.open_channel(&id),
            Submission::SwitchCommunity(name) => self.switch_community(&name),
            Submission::React(emoji) => self.react(&emoji),
            Submission::JumpToMessage { channel, id } => self.jump_to_message(&channel, &id),
            Submission::InsertMention(pubkey) => self.insert_mention(&pubkey),
            Submission::InsertCommand(command) => self.insert_command(&command),
            Submission::Prompt { kind, value } => self.run_prompt(kind, &value),
            Submission::Confirmed(action) => self.run_confirmed(action),
            Submission::Answered {
                channel,
                agent,
                reply,
                remember,
            } => self.answer_approval(&channel, &agent, &reply, remember),
        }
    }

    /// Publishes an answer to an agent's request, and records a standing grant
    /// when one was asked for.
    fn answer_approval(&mut self, channel: &str, agent: &str, reply: &str, remember: bool) {
        if remember {
            self.approval_grants.insert(agent.to_string());
            let name = self.display_name(agent);
            self.info(format!(
                "{name} may now proceed without asking, until buzztui closes"
            ));
        }
        self.say(channel, reply);
    }

    /// Sends a line of text to a channel on the user's behalf, taking the same
    /// route their own message would: a gift wrap for a conversation, a plain
    /// event for a room.
    fn say(&mut self, channel: &str, body: &str) {
        if let Some(peer) = store::direct_peer(channel).map(str::to_string) {
            if let Err(err) = self.send_direct(&peer, channel, body, None) {
                self.error("could not answer", err.to_string());
            }
        } else {
            match self.build_message(channel, body) {
                Ok(event) => {
                    if let Err(err) = self.store.record_outgoing(&event, channel) {
                        debug!(%err, "could not record the local echo");
                    }
                    self.relay.publish(event);
                }
                Err(err) => {
                    self.error("could not answer", err.to_string());
                    return;
                }
            }
        }
        if self.active.as_deref() == Some(channel) {
            self.follow = true;
            self.scroll = 0;
        }
        self.reload_timeline();
        self.reload_channels();
    }

    /// Notices an agent asking for permission and decides whether to interrupt.
    ///
    /// A grant covers only the `/approve`-style requests, whose answer is a
    /// fixed token; a menu is a genuine choice between alternatives and always
    /// waits for a person.
    fn note_approval_request(&mut self, event: &Event, channel: &str) {
        let Some(request) = ApprovalRequest::parse(&event.content) else {
            return;
        };
        let agent_key = event.pubkey.to_hex();
        let agent = self.display_name(&agent_key);

        if request.grantable() && self.approval_grants.contains(&agent_key) {
            let summary = crate::ui::text::truncate_end(request.summary(), 48).into_owned();
            self.toast(
                ToastKind::Info,
                format!("approved {summary} for {agent}"),
                Some(
                    "a standing grant answered this; /approvals revoke to withdraw it".to_string(),
                ),
            );
            self.say(channel, "/approve");
            return;
        }

        self.pending_approvals
            .push_back(Approval::new(request, agent, agent_key, channel));
        self.open_next_approval();
    }

    /// Shows the next queued request, once nothing else owns the screen.
    fn open_next_approval(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        if let Some(approval) = self.pending_approvals.pop_front() {
            self.overlay = Some(Overlay::Approval(approval));
            self.dirty = true;
        }
    }

    /// Withdraws every standing grant, for when one turns out to have been a
    /// mistake and the agent is still mid-run.
    fn revoke_approval_grants(&mut self) {
        if self.approval_grants.is_empty() {
            self.info("no agent has a standing approval");
            return;
        }
        let revoked = self.approval_grants.len();
        self.approval_grants.clear();
        self.info(format!(
            "revoked {revoked} standing approval{}; agents will ask again",
            if revoked == 1 { "" } else { "s" }
        ));
    }

    fn jump_to_message(&mut self, channel: &str, id: &str) {
        self.open_channel(channel);
        match self
            .store
            .messages_through(channel, id, self.config.ui.backfill as u32)
        {
            Ok(messages) => {
                let Some(index) = messages.iter().position(|message| message.id == id) else {
                    self.info("search result is no longer in the local cache");
                    return;
                };
                self.timeline = messages;
                self.selected = Some(index);
                self.scroll = 0;
                self.follow = false;
                self.focus = Focus::Timeline;
                self.thread_root = None;
                self.thread_selected = None;
                self.load_parents();
                self.request_profiles();
                self.dirty = true;
            }
            Err(err) => self.error("could not open search result", err.to_string()),
        }
    }

    fn run_prompt(&mut self, kind: PromptKind, value: &str) {
        match kind {
            PromptKind::CreateChannel => self.create_channel(value),
            PromptKind::JoinChannel => self.join_channel(value),
            PromptKind::DirectMessage => self.start_direct(value),
            PromptKind::SetRelay => self.switch_relay(value),
            PromptKind::SetTopic => self.set_topic(value),
        }
    }

    fn run_confirmed(&mut self, action: Confirmation) {
        match action {
            Confirmation::Quit => self.running = false,
            Confirmation::InstallUpdate(version) => self.begin_update_install(version),
            Confirmation::RestartUpdate(_) => {
                self.restart_requested = true;
                self.running = false;
            }
            Confirmation::DeleteMessage(id) => {
                let Some(channel) = self.active.clone() else {
                    return;
                };
                let built = (|| -> Result<Event> {
                    Ok(EventBuilder::new(Kind::from_u16(kinds::DELETION), "")
                        .tags([Tag::parse(["e", &id])?, Tag::parse(["h", &channel])?])
                        .finalize(&self.keypair)?)
                })();
                match built {
                    Ok(event) => self.relay.publish(event),
                    Err(err) => self.error("could not delete", err.to_string()),
                }
            }
            Confirmation::LeaveChannel(_) => {
                let Some(channel) = self.active.clone() else {
                    return;
                };
                let built = (|| -> Result<Event> {
                    Ok(EventBuilder::new(Kind::from_u16(kinds::LEAVE_REQUEST), "")
                        .tag(Tag::parse(["h", &channel])?)
                        .finalize(&self.keypair)?)
                })();
                match built {
                    Ok(event) => {
                        self.relay.publish(event);
                        let _ = self.store.set_joined(&channel, false);
                        self.reload_channels();
                    }
                    Err(err) => self.error("could not leave", err.to_string()),
                }
            }
        }
    }

    fn create_channel(&mut self, name: &str) {
        let built = (|| -> Result<Event> {
            Ok(EventBuilder::new(Kind::from_u16(kinds::CREATE_GROUP), "")
                .tags([
                    Tag::parse(["name", name])?,
                    Tag::parse(["visibility", "open"])?,
                ])
                .finalize(&self.keypair)?)
        })();
        match built {
            Ok(event) => {
                self.relay.publish(event);
                self.info(format!("creating {name}"));
            }
            Err(err) => self.error("could not create channel", err.to_string()),
        }
    }

    fn join_channel(&mut self, id: &str) {
        let built = (|| -> Result<Event> {
            Ok(EventBuilder::new(Kind::from_u16(kinds::JOIN_REQUEST), "")
                .tag(Tag::parse(["h", id])?)
                .finalize(&self.keypair)?)
        })();
        match built {
            Ok(event) => {
                self.relay.publish(event);
                let _ = self.store.set_joined(id, true);
                self.reload_channels();
                self.subscribe_feed();
                self.open_channel(id);
            }
            Err(err) => self.error("could not join", err.to_string()),
        }
    }

    fn start_direct(&mut self, who: &str) {
        let parsed = PublicKey::from_bech32(who)
            .or_else(|_| PublicKey::from_hex(who))
            .map(|key| key.to_hex());
        match parsed {
            Ok(peer) => {
                let name = self.display_name(&peer);
                match self.store.upsert_direct_channel(&peer, &name) {
                    Ok(id) => {
                        self.reload_channels();
                        self.open_channel(&id);
                    }
                    Err(err) => self.error("could not open the conversation", err.to_string()),
                }
            }
            Err(_) => self.error(
                "not a public key",
                format!("`{who}` is neither npub nor hex"),
            ),
        }
    }

    fn open_community_store(&self, community: &Community) -> Result<Arc<Store>> {
        self.paths.ensure_community(&self.me, &community.relay)?;
        let path = self.paths.db_for(&self.me, &community.relay);
        Ok(Arc::new(Store::open(&path, &self.me, &community.relay)?))
    }

    fn install_community(&mut self, name: &str, community: &Community, store: Arc<Store>) {
        self.store = store.clone();
        self.media
            .switch_cache(self.paths.media_for(&self.me, &community.relay), store);
        self.channels.clear();
        self.sidebar_cursor = 0;
        self.active = None;
        self.timeline.clear();
        self.selected = None;
        self.scroll = 0;
        self.follow = true;
        self.thread_root = None;
        self.thread_selected = None;
        self.thread_scroll = 0;
        self.thread_follow = true;
        self.profiles.clear();
        self.parents.clear();
        self.members.clear();
        self.presence.clear();
        self.typing.clear();
        self.pending_wraps.clear();
        self.last_typing = None;
        self.last_presence = None;
        self.composer.clear();
        self.compose_mode = ComposeMode::New;
        self.overlay = None;
        self.conn = ConnState::Connecting;
        self.relay.send(Command::Switch(community.relay.clone()));
        self.reload_channels();
        self.reload_profiles();
        self.subscribe_baseline();
        self.info(format!("switching to {name}"));
    }

    fn switch_community(&mut self, name: &str) {
        let Some(community) = self.config.communities.get(name).cloned() else {
            self.error("unknown community", name.to_string());
            return;
        };
        let store = match self.open_community_store(&community) {
            Ok(store) => store,
            Err(err) => {
                self.error("could not open community", err.to_string());
                return;
            }
        };
        let previous = self.config.clone();
        if let Err(err) = self
            .config
            .activate(name)
            .and_then(|()| self.config.save(&self.paths))
        {
            self.config = previous;
            self.error("could not save community", err.to_string());
            return;
        }
        self.install_community(name, &community, store);
    }

    /// Adds a named community and switches to it. A bare relay remains useful
    /// for the old `/relay wss://…` form and derives its name from the host.
    fn switch_relay(&mut self, spec: &str) {
        let fields: Vec<&str> = spec.split_whitespace().collect();
        let (name, relay, gateway) = match fields.as_slice() {
            [relay] => (
                crate::config::suggested_community_name(relay),
                (*relay).to_string(),
                None,
            ),
            [name, relay] => ((*name).to_string(), (*relay).to_string(), None),
            [name, relay, gateway] => (
                (*name).to_string(),
                (*relay).to_string(),
                Some((*gateway).to_string()),
            ),
            _ => {
                self.error(
                    "community needs a name and relay",
                    "usage: /community add <name> <wss://relay> [https://gateway]",
                );
                return;
            }
        };
        let community = Community { relay, gateway };
        let mut config = self.config.clone();
        if let Err(err) = config.upsert_community(&name, community.clone()) {
            self.error("invalid community", err.to_string());
            return;
        }
        let community = config
            .current_community()
            .map(|(_, community)| community.clone())
            .expect("upsert stored the community");
        let store = match self.open_community_store(&community) {
            Ok(store) => store,
            Err(err) => {
                self.error("could not open community", err.to_string());
                return;
            }
        };
        if let Err(err) = config.save(&self.paths) {
            self.error("could not save community", err.to_string());
            return;
        }
        self.config = config;
        self.install_community(&name, &community, store);
    }

    fn set_topic(&mut self, topic: &str) {
        let Some(channel) = self.active.clone() else {
            return;
        };
        let built = (|| -> Result<Event> {
            Ok(EventBuilder::new(Kind::from_u16(kinds::EDIT_METADATA), "")
                .tags([Tag::parse(["h", &channel])?, Tag::parse(["topic", topic])?])
                .finalize(&self.keypair)?)
        })();
        match built {
            Ok(event) => self.relay.publish(event),
            Err(err) => self.error("could not set the topic", err.to_string()),
        }
    }

    // ------------------------------------------------------------ commands

    /// Slash commands typed into the composer. They exist because some actions
    /// need an argument, which a keybinding cannot carry.
    pub fn run_command(&mut self, line: &str) {
        let mut parts = line.trim().splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or_default().to_lowercase();
        let rest = parts.next().unwrap_or_default().trim();

        match name.as_str() {
            "help" | "?" => self.dispatch(Action::OpenHelp),
            "keys" => self.dispatch(Action::OpenHelp),
            "quit" | "q" | "exit" => self.running = false,
            "join" | "j" => {
                if rest.is_empty() {
                    self.dispatch(Action::JoinChannel)
                } else {
                    self.join_channel(rest)
                }
            }
            "create" | "new" => {
                if rest.is_empty() {
                    self.dispatch(Action::CreateChannel)
                } else {
                    self.create_channel(rest)
                }
            }
            "leave" | "part" => self.dispatch(Action::LeaveChannel),
            "dm" | "msg" => {
                if rest.is_empty() {
                    self.dispatch(Action::OpenDirectMessage)
                } else {
                    self.start_direct(rest)
                }
            }
            "topic" if self.active_is_direct() => self.info("direct messages have no topic"),
            "invite" | "add" | "kick" | "remove" if self.active_is_direct() => {
                self.info("direct messages have no membership")
            }
            "topic" => {
                if rest.is_empty() {
                    self.overlay = Some(Overlay::Prompt(Prompt::new(PromptKind::SetTopic)))
                } else {
                    self.set_topic(rest)
                }
            }
            "community" | "communities" => {
                let mut args = rest.split_whitespace();
                match (args.next(), args.collect::<Vec<_>>()) {
                    (None, _) | (Some("list"), _) => self.dispatch(Action::OpenCommunitySwitcher),
                    (Some("use"), names) if names.len() == 1 => self.switch_community(names[0]),
                    (Some("add"), fields) if fields.is_empty() => {
                        self.overlay = Some(Overlay::Prompt(Prompt::new(PromptKind::SetRelay)))
                    }
                    (Some("add"), fields) => self.switch_relay(&fields.join(" ")),
                    (Some(name), fields) if fields.is_empty() => self.switch_community(name),
                    _ => self.error(
                        "invalid community command",
                        "usage: /community [list|use <name>|add <name> <relay> [gateway]]",
                    ),
                }
            }
            "relay" => {
                if rest.is_empty() {
                    self.overlay = Some(Overlay::Prompt(Prompt::new(PromptKind::SetRelay)))
                } else {
                    self.switch_relay(rest)
                }
            }
            "search" | "s" => {
                let mut search = Search::new(self.active.clone());
                search.query.text = rest.to_string();
                search.dirty = !rest.is_empty();
                self.overlay = Some(Overlay::Search(search));
                self.after_overlay_key();
            }
            "theme" => {
                if rest.is_empty() {
                    self.dispatch(Action::CycleTheme);
                } else if Palette::by_name(rest).is_some() {
                    self.config.ui.theme = rest.to_string();
                    self.palette = build_palette(&self.config).0;
                    let _ = self.config.save(&self.paths);
                } else {
                    self.error(
                        "unknown theme",
                        format!("try one of: {}", Palette::NAMES.join(", ")),
                    );
                }
            }
            "invite" | "add" => self.membership(kinds::ADD_USER, rest, "invited"),
            "kick" | "remove" => self.membership(kinds::REMOVE_USER, rest, "removed"),
            "mute" => self.toggle_mute(),
            "pin" => self.toggle_pin(),
            "read" => self.dispatch(Action::MarkRead),
            "me" => {
                // The classic emote: an ordinary message in italic-ish prose.
                if !rest.is_empty() {
                    let name = self.display_name(&self.me);
                    let body = format!("* {name} {rest}");
                    self.composer.set_text(&body);
                    self.send();
                }
            }
            "whoami" => {
                let npub = self
                    .keypair
                    .public_key()
                    .to_bech32()
                    .unwrap_or_else(|_| self.me.clone());
                self.toast(ToastKind::Info, "your public key", Some(npub));
            }
            "reload" => self.dispatch(Action::ReloadConfig),
            "update" if rest.is_empty() || rest.eq_ignore_ascii_case("check") => {
                self.request_update_check()
            }
            "update" if rest.eq_ignore_ascii_case("install") => self.request_update_install(),
            "update" => self.error("invalid update command", "usage: /update [install]"),
            // These are not buzztui's words but the agent's: they have to reach
            // the room verbatim, which is exactly what the unknown-command arm
            // below used to prevent.
            "approve" | "deny" => match self.active.clone() {
                Some(channel) => {
                    let token = format!("/{name}");
                    self.say(&channel, &token);
                }
                None => self.error("no channel open", "open the channel the agent asked in"),
            },
            "approvals" => match rest {
                "" | "list" => {
                    if self.approval_grants.is_empty() {
                        self.info("no agent has a standing approval");
                    } else {
                        let names: Vec<String> = self
                            .approval_grants
                            .iter()
                            .map(|agent| self.display_name(agent))
                            .collect();
                        self.info(format!("proceeding unattended: {}", names.join(", ")));
                    }
                }
                "revoke" | "clear" => self.revoke_approval_grants(),
                _ => self.error("invalid approvals command", "usage: /approvals [revoke]"),
            },
            "" => {}
            other => self.error(
                format!("unknown command /{other}"),
                "type / to browse commands".to_string(),
            ),
        }
    }

    /// Adds or removes a member. The relay decides whether we are allowed to,
    /// and says so through the `OK` verdict.
    fn membership(&mut self, kind: u16, who: &str, verb: &str) {
        let Some(channel) = self.active.clone() else {
            return;
        };
        if who.is_empty() {
            self.error("who?", format!("usage: /{verb} <npub or hex pubkey>"));
            return;
        }
        let parsed = PublicKey::from_bech32(who)
            .or_else(|_| PublicKey::from_hex(who))
            .map(|key| key.to_hex());
        let Ok(target) = parsed else {
            self.error(
                "not a public key",
                format!("`{who}` is neither npub nor hex"),
            );
            return;
        };
        let built = (|| -> Result<Event> {
            Ok(EventBuilder::new(Kind::from_u16(kind), "")
                .tags([Tag::parse(["h", &channel])?, Tag::parse(["p", &target])?])
                .finalize(&self.keypair)?)
        })();
        match built {
            Ok(event) => {
                self.relay.publish(event);
                self.info(format!("{verb} {}", self.display_name(&target)));
            }
            Err(err) => self.error("could not change membership", err.to_string()),
        }
    }

    // -------------------------------------------------------------- events

    pub fn on_relay(&mut self, update: Update) {
        self.dirty = true;
        match update {
            Update::State(state) => {
                let was_ready = self.conn.is_ready();
                self.conn = state;
                if self.conn.is_ready() && !was_ready {
                    self.announce_presence(true);
                }
                if let ConnState::Failed(reason) = &self.conn {
                    let reason = reason.clone();
                    self.toast(ToastKind::Warn, "relay unreachable", Some(reason));
                }
            }
            Update::Event {
                relay,
                subscription,
                event,
            } => {
                if relay == self.config.relay() {
                    self.ingest(&subscription, *event);
                } else {
                    debug!(%relay, "discarded a late event from the previous community");
                }
            }
            Update::EndOfStored(subscription) => {
                if subscription == SUB_DISCOVERY {
                    self.reload_channels();
                    self.subscribe_feed();
                }
                if subscription == SUB_BACKFILL || subscription == SUB_ACTIVE {
                    self.reload_timeline();
                }
            }
            Update::Verdict {
                id,
                accepted,
                message,
            } => {
                // A direct message is acknowledged by wrap id; the row to settle
                // is the rumor inside it.
                let id = self.pending_wraps.remove(&id).unwrap_or(id);
                if let Err(err) = self.store.resolve_outgoing(&id, accepted, &message) {
                    debug!(%err, "could not record the relay verdict");
                }
                if !accepted {
                    self.toast(
                        ToastKind::Error,
                        "relay rejected the message",
                        Some(message),
                    );
                }
                self.reload_timeline();
            }
            Update::Closed {
                subscription,
                message,
            } => {
                self.toast(
                    ToastKind::Warn,
                    format!("subscription {subscription} closed"),
                    Some(message),
                );
            }
            Update::Notice(message) => self.toast(ToastKind::Info, "relay notice", Some(message)),
        }
    }

    fn ingest(&mut self, subscription: &str, event: Event) {
        // Search results are a transient list, not part of the conversation.
        if subscription == SUB_SEARCH {
            if let Ok(Ingested::Message { .. }) = self.store.ingest(&event)
                && let Some(Overlay::Search(search)) = self.overlay.as_mut()
            {
                let query = search.query.text.clone();
                let scope = search.scope.clone();
                if let Ok(results) = self.store.search(&query, scope.as_deref(), 200)
                    && let Some(Overlay::Search(search)) = self.overlay.as_mut()
                {
                    search.set_results(results);
                }
            }
            return;
        }

        match event.kind.as_u16() {
            kinds::PRESENCE => {
                self.presence
                    .insert(event.pubkey.to_hex(), Presence::parse(&event.content));
                return;
            }
            kinds::TYPING => {
                self.typing
                    .insert(event.pubkey.to_hex(), Instant::now() + TYPING_TTL);
                return;
            }
            kinds::GIFT_WRAP => {
                self.unwrap_direct(&event);
                return;
            }
            _ => {}
        }

        let outcome = match self.store.ingest(&event) {
            Ok(outcome) => outcome,
            Err(err) => {
                debug!(%err, "could not store event");
                return;
            }
        };

        match &outcome {
            Ingested::Ignored => return,
            Ingested::Profile { pubkey } => {
                self.reload_profiles();
                // Only NIP-17 conversations own synthetic `dm:<pubkey>` rows.
                // Legacy hidden NIP-29 rooms derive their labels from profiles
                // in `Store::channels` and must keep their relay UUID.
                if let Some(channel) = self
                    .channels
                    .iter()
                    .find(|channel| store::direct_peer(&channel.id) == Some(pubkey.as_str()))
                {
                    let name = self.display_name(pubkey);
                    if channel.name != name {
                        let _ = self.store.upsert_direct_channel(pubkey, &name);
                    }
                }
                self.reload_channels();
                return;
            }
            Ingested::ChannelMeta { .. } | Ingested::Membership { .. } => {
                self.reload_channels();
                return;
            }
            Ingested::Members { channel } => {
                if self.active.as_deref() == Some(channel.as_str()) {
                    if let Ok(members) = self.store.members(channel) {
                        self.members = members;
                    }
                    self.request_profiles();
                }
                return;
            }
            _ => {}
        }

        let touched = outcome.channel().map(str::to_string);
        if touched.as_deref() == self.active.as_deref() {
            self.reload_timeline();
            if self.follow {
                self.mark_active_read();
            }
        }
        self.reload_channels();

        // An agent blocked on a permission answer interrupts wherever it asked,
        // since it makes no progress until someone replies.
        if let Ingested::Message { channel, .. } = &outcome
            && !self.is_me(&event.pubkey.to_hex())
        {
            let channel = channel.clone();
            self.note_approval_request(&event, &channel);
        }

        // A message from someone else in a channel we are not looking at is the
        // only thing worth interrupting for, and only if it names us.
        if let Ingested::Message { channel, id } = &outcome
            && Some(channel.as_str()) != self.active.as_deref()
            && !self.is_me(&event.pubkey.to_hex())
        {
            let muted = self
                .channels
                .iter()
                .find(|c| &c.id == channel)
                .is_some_and(|c| c.muted);
            let mentions_me = proto::tag_values(&event, "p").any(|p| p == self.me);
            if mentions_me && !muted {
                let name = self.display_name(&event.pubkey.to_hex());
                let where_ = self
                    .channels
                    .iter()
                    .find(|c| &c.id == channel)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| channel.clone());
                self.toast(
                    ToastKind::Info,
                    format!("{name} mentioned you in {where_}"),
                    Some(crate::ui::text::truncate_end(&event.content, 60).into_owned()),
                );
            }
            let _ = id;
        }
    }

    /// Opens a gift wrap and files the message under a conversation keyed by the
    /// other party, whichever direction it travelled.
    fn unwrap_direct(&mut self, wrap: &Event) {
        let wrap_id = wrap.id.to_hex();
        match self.store.note_gift_wrap(&wrap_id, None) {
            Ok(false) => return,
            Ok(true) => {}
            Err(err) => {
                debug!(%err, "could not record the gift wrap");
                return;
            }
        }

        let unwrapped = match UnwrappedGift::from_gift_wrap(&self.keypair, wrap) {
            Ok(unwrapped) => unwrapped,
            Err(err) => {
                debug!(%err, "could not open a gift wrap addressed to us");
                return;
            }
        };
        let rumor = unwrapped.rumor;
        if rumor.kind.as_u16() != kinds::PRIVATE_DM {
            debug!(
                kind = rumor.kind.as_u16(),
                "ignoring a non-message gift wrap"
            );
            return;
        }
        let author = rumor.pubkey.to_hex();
        // A message we sent is addressed to the peer; one we received is from
        // them. Either way the conversation is keyed by the other party.
        let peer = if author == self.me {
            rumor
                .tags
                .iter()
                .find(|tag| tag.kind() == "p")
                .and_then(|tag| tag.content())
                .map(str::to_string)
        } else {
            Some(author.clone())
        };
        let Some(peer) = peer else { return };

        let name = self.display_name(&peer);
        let channel = match self.store.upsert_direct_channel(&peer, &name) {
            Ok(channel) => channel,
            Err(err) => {
                debug!(%err, "could not open the direct conversation");
                return;
            }
        };

        let id = rumor
            .id
            .map(|id| id.to_hex())
            .unwrap_or_else(|| wrap_id.clone());
        let tags = serde_json::to_string(&rumor.tags).unwrap_or_else(|_| "[]".to_string());
        let record = store::Rumor {
            id: &id,
            author: &author,
            created_at: rumor.created_at.as_secs() as i64,
            body: &rumor.content,
            tags: &tags,
            parent: None,
            root: None,
            mentions_me: author != self.me,
        };
        match self.store.ingest_rumor(&record, &channel) {
            Ok(Ingested::Message { .. }) => {
                self.reload_channels();
                if self.active.as_deref() == Some(channel.as_str()) {
                    self.reload_timeline();
                } else if author != self.me {
                    self.toast(
                        ToastKind::Info,
                        format!("{name} sent you a message"),
                        Some(crate::ui::text::truncate_end(&rumor.content, 60).into_owned()),
                    );
                }
            }
            Ok(_) => {}
            Err(err) => debug!(%err, "could not store the direct message"),
        }
    }

    pub fn on_media(&mut self, event: MediaEvent) {
        self.media.handle(event);
        self.dirty = true;
    }

    /// Periodic housekeeping: expiring toasts and typing indicators, and
    /// re-announcing presence.
    pub fn on_tick(&mut self) {
        let now = Instant::now();
        let before = self.toasts.len() + self.typing.len();
        self.toasts.retain(|toast| toast.expires > now);
        self.typing.retain(|_, expiry| *expiry > now);
        if self.toasts.len() + self.typing.len() != before {
            self.dirty = true;
        }
        self.announce_presence(false);
    }

    fn announce_presence(&mut self, force: bool) {
        if !self.config.ui.send_presence || !self.conn.is_ready() {
            return;
        }
        let now = Instant::now();
        if !force
            && self
                .last_presence
                .is_some_and(|last| now.duration_since(last) < PRESENCE_INTERVAL)
        {
            return;
        }
        self.last_presence = Some(now);
        if let Ok(event) =
            EventBuilder::new(Kind::from_u16(kinds::PRESENCE), "online").finalize(&self.keypair)
        {
            self.relay.publish(event);
        }
    }

    /// Every image the timeline wants, with relay-relative paths resolved
    /// against the community's own media endpoint.
    pub fn image_urls(&self) -> Vec<String> {
        self.timeline
            .iter()
            .flat_map(|message| crate::ui::text::image_links(&message.body))
            .map(|url| self.config.resolve_media(url))
            .collect()
    }

    /// Tells the media cache which images are still worth keeping decoded.
    pub fn visible_images(&self) -> HashSet<String> {
        let mut keep: HashSet<String> = self.image_urls().into_iter().collect();
        // The avatar in an open profile is not in the timeline but is on screen.
        if self.config.ui.avatars
            && let Some(Overlay::Profile(pubkey)) = &self.overlay
            && let Some(picture) = self.profiles.get(pubkey).and_then(|p| p.picture.as_deref())
        {
            keep.insert(self.config.resolve_media(picture));
        }
        if let Some(Overlay::Image(url)) = &self.overlay {
            keep.insert(url.clone());
        }
        keep
    }

    pub fn shutdown(&mut self) {
        self.mark_active_read();
        if self.config.ui.send_presence
            && let Ok(event) = EventBuilder::new(Kind::from_u16(kinds::PRESENCE), "offline")
                .finalize(&self.keypair)
        {
            self.relay.publish(event);
        }
        self.relay.send(Command::Shutdown);
    }
}

/// Builds the active palette: the named theme with any per-token overrides
/// applied. Bad overrides degrade to the theme default and are reported rather
/// than refusing to start.
fn build_palette(config: &Config) -> (Palette, Vec<String>) {
    let mut palette = Palette::by_name(&config.ui.theme).unwrap_or_else(Palette::catppuccin);
    let mut problems = Vec::new();
    if Palette::by_name(&config.ui.theme).is_none() {
        problems.push(format!(
            "unknown theme `{}`; using buzz. try one of: {}",
            config.ui.theme,
            Palette::NAMES.join(", ")
        ));
    }
    problems.extend(palette.apply_overrides(&config.theme));
    (palette, problems)
}

/// Turns a display name into one composer token while retaining Unicode names.
/// Whitespace becomes `_`; punctuation the mention parser treats as prose is
/// excluded so the generated `p` tag can always be resolved on send.
fn mention_token(name: &str) -> String {
    name.split_whitespace()
        .map(|part| part.trim_matches(['@', ',', '.', ':', ';', '!', '?']))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

/// A short, human relative time for the channel switcher. Precision past a day
/// is noise when you are only deciding which room to open.
fn relative_time(when: i64) -> Option<String> {
    if when <= 0 {
        return None;
    }
    let now = Timestamp::now().as_secs() as i64;
    let delta = now.saturating_sub(when);
    Some(match delta {
        ..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        86_400..=2_591_999 => format!("{}d ago", delta / 86_400),
        _ => format!("{}w ago", delta / 604_800),
    })
}

/// NIP-10 thread tags. The root is always named; the direct parent is only
/// named separately when it differs, which is the shape other clients expect.
fn thread_tags(root: &str, parent: &str) -> Result<Vec<Tag>> {
    let mut tags = vec![Tag::parse(["e", root, "", "root"])?];
    if parent != root {
        tags.push(Tag::parse(["e", parent, "", "reply"])?);
    }
    Ok(tags)
}

/// A thread composer stays a thread composer after a successful send. Advancing
/// the direct parent to the new event preserves a natural reply chain while the
/// root remains stable across any number of consecutive messages.
fn compose_mode_after_send(
    sent_as: &ComposeMode,
    open_thread: Option<&str>,
    sent_id: String,
) -> ComposeMode {
    match sent_as {
        ComposeMode::Reply { root, .. } if open_thread == Some(root.as_str()) => {
            ComposeMode::Reply {
                to: sent_id,
                root: root.clone(),
            }
        }
        _ => ComposeMode::New,
    }
}

/// Builds a direct message: one rumor, wrapped once for the recipient and once
/// for the sender. Returns the rumor alongside both wraps so the caller can
/// store the plaintext and publish only the ciphertext.
fn direct_wraps(
    keypair: &Keys,
    receiver: PublicKey,
    body: &str,
    tags: Vec<Tag>,
) -> Result<(UnsignedEvent, Event, Event)> {
    let rumor = EventBuilder::new(Kind::from_u16(kinds::PRIVATE_DM), body)
        .tags(tags)
        .finalize_unsigned(keypair.public_key());
    let to_peer = GiftWrapBuilder::new(receiver, rumor.clone()).finalize(keypair)?;
    let to_self = GiftWrapBuilder::new(keypair.public_key(), rumor.clone()).finalize(keypair)?;
    Ok((rumor, to_peer, to_self))
}

/// Actions whose only implementation publishes a public, channel-tagged event.
/// Running one inside a gift-wrapped conversation would tell the relay who is
/// reacting to, deleting, or moderating what, which is exactly what a direct
/// message is supposed to hide.
fn leaks_in_direct(action: Action) -> bool {
    matches!(
        action,
        Action::React
            | Action::DeleteMessage
            | Action::EditMessage
            | Action::LeaveChannel
            | Action::JoinChannel
            | Action::OpenMembers
            | Action::ToggleMemberPane
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::nips::nip59::UnwrappedGift;

    /// The whole point of a direct message is that the relay learns only who it
    /// is for, so assert the wire form rather than trusting the builder.
    #[test]
    fn a_direct_message_travels_only_as_gift_wraps() {
        let sender = Keys::generate();
        let receiver = Keys::generate();
        let body = "the deploy key is in the vault";

        let (_, to_peer, to_self) = direct_wraps(
            &sender,
            receiver.public_key(),
            body,
            vec![Tag::parse(["p", &receiver.public_key().to_hex()]).unwrap()],
        )
        .unwrap();

        for wrap in [&to_peer, &to_self] {
            assert_eq!(wrap.kind.as_u16(), kinds::GIFT_WRAP);
            assert!(
                !wrap.content.contains("deploy key"),
                "the body must not appear in the wrap"
            );
            assert!(
                proto::channel_of(wrap).is_none(),
                "a wrap must not carry a channel tag"
            );
            wrap.verify().expect("wraps must be self-consistent");
        }
        assert_ne!(
            to_peer.pubkey, to_self.pubkey,
            "each wrap uses its own ephemeral key"
        );
    }

    #[test]
    fn both_parties_unwrap_to_the_same_plaintext_rumor() {
        let sender = Keys::generate();
        let receiver = Keys::generate();
        let body = "lunch?";

        let (mut rumor, to_peer, to_self) = direct_wraps(
            &sender,
            receiver.public_key(),
            body,
            vec![Tag::parse(["p", &receiver.public_key().to_hex()]).unwrap()],
        )
        .unwrap();
        let expected = rumor.id().to_hex();

        let theirs = UnwrappedGift::from_gift_wrap(&receiver, &to_peer).unwrap();
        let ours = UnwrappedGift::from_gift_wrap(&sender, &to_self).unwrap();

        for (mut unwrapped, who) in [(theirs, "recipient"), (ours, "sender")] {
            assert_eq!(unwrapped.sender, sender.public_key(), "{who}");
            assert_eq!(unwrapped.rumor.kind.as_u16(), kinds::PRIVATE_DM, "{who}");
            assert_eq!(unwrapped.rumor.content, body, "{who}");
            assert_eq!(unwrapped.rumor.id().to_hex(), expected, "{who}");
        }

        // The recipient must not be able to open the sender's own copy.
        assert!(UnwrappedGift::from_gift_wrap(&receiver, &to_self).is_err());
    }

    #[test]
    fn a_direct_reply_keeps_its_thread_inside_the_rumor() {
        let sender = Keys::generate();
        let receiver = Keys::generate();
        let root = "a".repeat(64);

        let mut tags = vec![Tag::parse(["p", &receiver.public_key().to_hex()]).unwrap()];
        tags.extend(thread_tags(&root, &root).unwrap());
        let (_, to_peer, _) = direct_wraps(&sender, receiver.public_key(), "yes", tags).unwrap();

        assert!(
            proto::target_of(&to_peer).is_none(),
            "the thread must not be visible on the wrap"
        );
        let unwrapped = UnwrappedGift::from_gift_wrap(&receiver, &to_peer).unwrap();
        let inner = Event::new(
            unwrapped.rumor.compute_id(),
            unwrapped.rumor.pubkey,
            unwrapped.rumor.created_at,
            unwrapped.rumor.kind,
            unwrapped.rumor.tags.clone().to_vec(),
            unwrapped.rumor.content.clone(),
            to_peer.sig,
        );
        assert_eq!(proto::Thread::parse(&inner).root.unwrap(), root);
    }

    #[test]
    fn thread_tags_name_the_parent_only_when_it_differs_from_the_root() {
        let root = "a".repeat(64);
        let parent = "b".repeat(64);

        let top_level = thread_tags(&root, &root).unwrap();
        assert_eq!(top_level.len(), 1, "a reply to the root names it once");

        let nested = thread_tags(&root, &parent).unwrap();
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[1].as_slice()[3], "reply");
    }

    #[test]
    fn two_consecutive_thread_sends_keep_advancing_the_reply_parent() {
        let root = "root";
        let first = ComposeMode::Reply {
            to: "original".to_string(),
            root: root.to_string(),
        };

        let second = compose_mode_after_send(&first, Some(root), "first-sent".to_string());
        assert_eq!(
            second,
            ComposeMode::Reply {
                to: "first-sent".to_string(),
                root: root.to_string(),
            }
        );

        let third = compose_mode_after_send(&second, Some(root), "second-sent".to_string());
        assert_eq!(
            third,
            ComposeMode::Reply {
                to: "second-sent".to_string(),
                root: root.to_string(),
            }
        );
        assert_eq!(
            compose_mode_after_send(&third, None, "outside-thread".to_string()),
            ComposeMode::New,
            "leaving the thread must restore ordinary channel composition"
        );
    }

    /// Every action that has no private equivalent must be refused, and every
    /// action that does must still work; a stale list here is a metadata leak.
    #[test]
    fn actions_without_a_private_form_are_blocked_in_direct_messages() {
        for action in [
            Action::React,
            Action::DeleteMessage,
            Action::EditMessage,
            Action::LeaveChannel,
            Action::JoinChannel,
            Action::OpenMembers,
        ] {
            assert!(leaks_in_direct(action), "{action:?} would leak");
        }
        // These stay available because they have a private implementation:
        // sending, replying, and retrying all route through `send_direct`.
        for action in [
            Action::Send,
            Action::Reply,
            Action::RetrySend,
            Action::DiscardFailed,
            Action::CopyMessage,
            Action::QuoteMessage,
            Action::OpenSearch,
            Action::OpenSwitcher,
            Action::ScrollUp,
            Action::Quit,
        ] {
            assert!(!leaks_in_direct(action), "{action:?} must still work");
        }
    }

    #[test]
    fn relative_time_reads_as_prose() {
        let now = Timestamp::now().as_secs() as i64;
        assert_eq!(relative_time(now).unwrap(), "just now");
        assert_eq!(relative_time(now - 300).unwrap(), "5m ago");
        assert_eq!(relative_time(now - 7200).unwrap(), "2h ago");
        assert_eq!(relative_time(now - 172_800).unwrap(), "2d ago");
        assert!(
            relative_time(0).is_none(),
            "a channel with no activity has no time"
        );
    }

    #[test]
    fn mention_tokens_are_single_resolvable_words() {
        assert_eq!(mention_token("Alice Smith"), "Alice_Smith");
        assert_eq!(
            mention_token(" Jos\u{00e9} van Dyke! "),
            "Jos\u{00e9}_van_Dyke"
        );
        assert_eq!(mention_token("@operator,"), "operator");
    }

    #[test]
    fn the_command_picker_exposes_the_documented_command_set() {
        let names: Vec<&str> = SLASH_COMMANDS.iter().map(|command| command.name).collect();
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "command names must be unique");
        assert!(names.contains(&"update"));
        assert!(names.contains(&"community"));
        assert!(names.contains(&"invite"));
        // An agent listens for these two verbatim, so they have to be commands
        // buzztui forwards rather than ones it rejects as unknown.
        assert!(names.contains(&"approve"));
        assert!(names.contains(&"deny"));
    }

    #[test]
    fn update_checks_cannot_detach_an_install_result() {
        let version = Version::new(1, 2, 3);
        assert_eq!(
            update_check_transition(&UpdateStatus::Installing(version.clone())),
            None
        );
        assert_eq!(
            update_check_transition(&UpdateStatus::Installed(version.clone())),
            None
        );
        assert_eq!(
            update_check_transition(&UpdateStatus::Available(version)),
            Some((UpdateStatus::Checking, UpdateRequest::Check))
        );
    }
}
