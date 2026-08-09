//! On-disk layout and user configuration.
//!
//! Everything buzztui owns lives under a single directory, `~/.config/buzztui`
//! by default. The location can be redirected with `BUZZTUI_HOME`, and it
//! otherwise follows `XDG_CONFIG_HOME` when that variable is set. The secret
//! key deliberately lives outside `config.toml` so that the configuration file
//! can be shared, diffed, or version-controlled without leaking an identity.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nostr::types::RelayUrl;
use serde::{Deserialize, Serialize};
use url::Url;

/// Directory name used under the configuration root.
const APP_DIR: &str = "buzztui";

/// Written at the top of a generated `config.toml`. Every setting here can be
/// changed by hand; the interface reloads them without a restart.
const CONFIG_HEADER: &str = r#"# buzztui configuration — https://github.com/bscott/buzz-tui
#
# Edit this by hand and reload from inside buzztui with the reload_config
# binding (ctrl+b R by default), or restart.
#
#   current       name of the community opened at startup
#   communities  named Buzz communities, each with a websocket relay and an
#                optional HTTP gateway for media and relay metadata
"#;

/// Resolved locations of every file buzztui reads or writes.
///
/// The cache is deliberately partitioned by both identity and community. It
/// holds decrypted direct messages and per-community read state; neither may
/// cross an account or relay boundary.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub secret: PathBuf,
    pub cache: PathBuf,
    pub log: PathBuf,
}

impl Paths {
    /// Resolves the application directory, honouring `BUZZTUI_HOME` and then
    /// `XDG_CONFIG_HOME` before falling back to `~/.config/buzztui`.
    pub fn resolve() -> Result<Self> {
        let root = if let Some(explicit) = std::env::var_os("BUZZTUI_HOME") {
            PathBuf::from(explicit)
        } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            PathBuf::from(xdg).join(APP_DIR)
        } else {
            let home = directories::BaseDirs::new()
                .context("cannot determine home directory; set BUZZTUI_HOME")?;
            home.home_dir().join(".config").join(APP_DIR)
        };

        Ok(Self {
            config: root.join("config.toml"),
            secret: root.join("secret.key"),
            cache: root.join("cache"),
            log: root.join("buzztui.log"),
            root,
        })
    }

    fn identity_dir(&self, pubkey: &str) -> PathBuf {
        // A prefix is enough to separate accounts and keeps the path readable;
        // a collision would need a deliberate 64-bit preimage.
        let short: String = pubkey.chars().take(16).collect();
        self.cache.join(short)
    }

    fn community_dir(&self, pubkey: &str, relay: &str) -> PathBuf {
        self.identity_dir(pubkey).join(cache_key(relay))
    }

    pub fn db_for(&self, pubkey: &str, relay: &str) -> PathBuf {
        self.community_dir(pubkey, relay).join("buzz.db")
    }

    pub fn media_for(&self, pubkey: &str, relay: &str) -> PathBuf {
        self.community_dir(pubkey, relay).join("media")
    }

    /// Creates the application directory tree if any part of it is missing.
    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        fs::create_dir_all(&self.cache)
            .with_context(|| format!("creating {}", self.cache.display()))?;
        restrict(&self.root)?;
        restrict(&self.cache)?;
        Ok(())
    }

    /// Creates one identity/community cache. Legacy data is deliberately not
    /// moved here: callers may be opening an ephemeral relay override, while
    /// only the persisted configuration knows which community owned v0.1.1.
    pub fn ensure_community(&self, pubkey: &str, relay: &str) -> Result<()> {
        self.ensure()?;
        let identity = self.identity_dir(pubkey);
        let community = self.community_dir(pubkey, relay);
        fs::create_dir_all(&identity)
            .with_context(|| format!("creating {}", identity.display()))?;
        fs::create_dir_all(&community)
            .with_context(|| format!("creating {}", community.display()))?;
        let media = self.media_for(pubkey, relay);
        fs::create_dir_all(&media).with_context(|| format!("creating {}", media.display()))?;
        restrict(&identity)?;
        restrict(&community)?;
        Ok(())
    }

    /// Assigns every identity-level v0.1.1 cache to the relay from the
    /// persisted single-community configuration. This runs while loading that
    /// configuration, before environment or command-line overrides can apply.
    fn migrate_legacy_caches(&self, relay: &str) -> Result<()> {
        if relay.trim().is_empty() || !self.cache.exists() {
            return Ok(());
        }
        let entries = fs::read_dir(&self.cache)
            .with_context(|| format!("reading {}", self.cache.display()))?;
        for entry in entries {
            let entry =
                entry.with_context(|| format!("reading an entry in {}", self.cache.display()))?;
            if !entry
                .file_type()
                .with_context(|| format!("reading the type of {}", entry.path().display()))?
                .is_dir()
            {
                continue;
            }
            let identity = entry.path();
            let has_legacy_cache = identity.join("buzz.db").exists()
                || identity.join("buzz.db-wal").exists()
                || identity.join("buzz.db-shm").exists()
                || identity.join("media").exists();
            if !has_legacy_cache {
                continue;
            }
            let community = identity.join(cache_key(relay));
            fs::create_dir_all(&community)
                .with_context(|| format!("creating {}", community.display()))?;
            for name in ["buzz.db", "buzz.db-wal", "buzz.db-shm"] {
                move_if_unclaimed(&identity.join(name), &community.join(name))?;
            }
            move_if_unclaimed(&identity.join("media"), &community.join("media"))?;
            restrict(&identity)?;
            restrict(&community)?;
        }
        Ok(())
    }
}

/// One host-authoritative Buzz community.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Community {
    pub relay: String,
    /// HTTP base for media, uploads, and relay metadata. When absent, the
    /// websocket relay's own host is used.
    pub gateway: Option<String>,
}

/// User-facing configuration, persisted as TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    /// Name of the community opened at startup.
    pub current: String,
    /// Human-readable names mapped to their connection details.
    pub communities: BTreeMap<String, Community>,
    /// A CLI or environment override is intentionally never persisted.
    #[serde(skip)]
    relay_override: Option<Community>,
    pub ui: UiConfig,
    pub media: MediaConfig,
    /// Per-token palette overrides applied on top of the named theme, keyed by
    /// the token name: `accent = "#f5c2e7"`.
    pub theme: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Width of the channel rail in columns.
    pub sidebar_width: u16,
    /// Named palette: `buzz`, `midnight`, `paper`, or `mono`.
    pub theme: String,
    /// Render timestamps on a 24-hour clock.
    pub clock_24h: bool,
    /// Collapse the blank line between message groups.
    pub compact: bool,
    /// Fetch and draw profile pictures. Turning this off skips the download
    /// entirely, not merely the drawing.
    pub avatars: bool,
    /// Number of historical messages requested per channel on first open.
    pub backfill: u16,
    /// Publish ephemeral typing indicators while composing.
    pub send_typing: bool,
    /// Announce presence to the relay while connected.
    pub send_presence: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_width: 26,
            theme: "buzz".to_string(),
            clock_24h: true,
            compact: false,
            avatars: true,
            backfill: 200,
            send_typing: true,
            send_presence: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    /// Draw images inline in the timeline when the terminal can display them.
    pub inline_images: bool,
    /// Maximum height, in terminal rows, of an inline image.
    pub max_rows: u16,
    /// Refuse to download anything larger than this, in bytes.
    pub max_bytes: u64,
    /// Force a graphics protocol instead of probing the terminal.
    /// One of `auto`, `kitty`, `sixel`, `iterm2`, `halfblocks`.
    pub protocol: String,
    /// Cell size in pixels, as `[width, height]`. Multiplexers often refuse the
    /// cell-size query even when they pass graphics through, and an image needs
    /// this only to work out how many rows it should occupy.
    pub cell_size: [u16; 2],
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            inline_images: true,
            max_rows: 16,
            max_bytes: 16 * 1024 * 1024,
            protocol: "auto".to_string(),
            cell_size: [10, 20],
        }
    }
}

/// The v0.1.1 shape, accepted only long enough to rewrite it into the
/// multi-community format.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LegacyConfig {
    relay: String,
    gateway: Option<String>,
    ui: UiConfig,
    media: MediaConfig,
    theme: BTreeMap<String, String>,
}

impl Config {
    /// Loads `config.toml`, rewriting the single-community v0.1.1 shape on
    /// first read.
    pub fn load_or_create(paths: &Paths) -> Result<Self> {
        if !paths.config.exists() {
            paths.ensure()?;
            let config = Self::default();
            config.save(paths)?;
            return Ok(config);
        }
        let raw = fs::read_to_string(&paths.config)
            .with_context(|| format!("reading {}", paths.config.display()))?;
        let value: toml::Value =
            toml::from_str(&raw).with_context(|| format!("parsing {}", paths.config.display()))?;
        if value.get("relay").is_some() || value.get("gateway").is_some() {
            let legacy: LegacyConfig = toml::from_str(&raw)
                .with_context(|| format!("migrating {}", paths.config.display()))?;
            let mut config = Self {
                ui: legacy.ui,
                media: legacy.media,
                theme: legacy.theme,
                ..Self::default()
            };
            if !legacy.relay.trim().is_empty() {
                let name = suggested_community_name(&legacy.relay);
                config.upsert_community(
                    &name,
                    Community {
                        relay: legacy.relay,
                        gateway: legacy.gateway,
                    },
                )?;
            }
            config.migrate_legacy_cache(paths)?;
            config.save(paths)?;
            return Ok(config);
        }
        let config: Self =
            toml::from_str(&raw).with_context(|| format!("parsing {}", paths.config.display()))?;
        config.migrate_legacy_cache(paths)?;
        Ok(config)
    }

    fn migrate_legacy_cache(&self, paths: &Paths) -> Result<()> {
        if let Some((_, community)) = self.current_community() {
            paths.migrate_legacy_caches(&community.relay)?;
        }
        Ok(())
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure()?;
        let body = toml::to_string_pretty(self).context("serialising configuration")?;
        let doc = format!("{CONFIG_HEADER}\n{body}");
        fs::write(&paths.config, doc)
            .with_context(|| format!("writing {}", paths.config.display()))?;
        Ok(())
    }

    /// The selected community, with an ephemeral CLI/environment override
    /// taking precedence without entering the saved community list.
    pub fn current_community(&self) -> Option<(&str, &Community)> {
        if let Some(community) = &self.relay_override {
            return Some(("override", community));
        }
        self.communities
            .get_key_value(&self.current)
            .map(|(name, community)| (name.as_str(), community))
    }

    pub fn relay(&self) -> &str {
        self.current_community()
            .map_or("", |(_, community)| community.relay.as_str())
    }

    pub fn has_community(&self) -> bool {
        self.current_community()
            .is_some_and(|(_, community)| !community.relay.trim().is_empty())
    }

    pub fn activate(&mut self, name: &str) -> Result<()> {
        if !self.communities.contains_key(name) {
            bail!("unknown community `{name}`");
        }
        self.current = name.to_string();
        self.relay_override = None;
        Ok(())
    }

    pub fn upsert_community(&mut self, name: &str, mut community: Community) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("community name cannot be empty");
        }
        let relay = RelayUrl::parse(community.relay.trim())
            .with_context(|| format!("{} is not a ws:// or wss:// URL", community.relay))?;
        community.relay = canonical_relay_url(&relay);
        community.gateway = community
            .gateway
            .map(|gateway| gateway.trim().trim_end_matches('/').to_string())
            .filter(|gateway| !gateway.is_empty());
        self.communities.insert(name.to_string(), community);
        self.current = name.to_string();
        self.relay_override = None;
        Ok(())
    }

    pub fn override_relay(&mut self, relay: impl Into<String>) {
        let relay = relay.into();
        self.relay_override = Some(Community {
            relay: relay_identity(&relay),
            gateway: None,
        });
    }

    /// Applies environment overrides. `BUZZTUI_RELAY` wins over `BUZZ_RELAY_URL`
    /// so that a buzztui-specific setting can override the shared Buzz variable.
    pub fn apply_env(&mut self) {
        for key in ["BUZZTUI_RELAY", "BUZZ_RELAY_URL"] {
            if let Ok(value) = std::env::var(key)
                && !value.trim().is_empty()
            {
                self.override_relay(value);
                return;
            }
        }
    }

    pub fn http_origin(&self) -> String {
        self.current_community()
            .map_or_else(String::new, |(_, community)| community.http_origin())
    }

    pub fn resolve_media(&self, url: &str) -> String {
        if url.starts_with("http://") || url.starts_with("https://") {
            return url.to_string();
        }
        format!("{}/{}", self.http_origin(), url.trim_start_matches('/'))
    }
}

impl Community {
    /// The HTTP origin used for Blossom media downloads and NIP-11 metadata.
    pub fn http_origin(&self) -> String {
        if let Some(gateway) = self
            .gateway
            .as_deref()
            .map(str::trim)
            .filter(|gateway| !gateway.is_empty())
        {
            return gateway.trim_end_matches('/').to_string();
        }
        self.http_origin_from_relay()
    }

    pub fn http_origin_from_relay(&self) -> String {
        let relay = self.relay.trim_end_matches('/');
        match relay.split_once("://") {
            Some(("wss", rest)) => format!("https://{rest}"),
            Some(("ws", rest)) => format!("http://{rest}"),
            _ => relay.to_string(),
        }
    }
}

/// A useful initial label that remains stable until the user chooses another.
pub fn suggested_community_name(relay: &str) -> String {
    relay
        .trim()
        .trim_end_matches('/')
        .split_once("://")
        .map_or(relay, |(_, rest)| rest)
        .split('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("community")
        .to_string()
}

/// Stable identity for a relay boundary. URL parsing canonicalizes the scheme,
/// host, and default port. Only the authority's implicit root path is folded;
/// non-root paths retain both case and their trailing slash.
pub(crate) fn relay_identity(relay: &str) -> String {
    RelayUrl::parse(relay.trim())
        .map(|url| canonical_relay_url(&url))
        .unwrap_or_else(|_| relay.trim().to_string())
}

fn canonical_relay_url(relay: &RelayUrl) -> String {
    let url: &Url = relay.into();
    if url.path() == "/" && url.query().is_none() && url.fragment().is_none() {
        url.as_str().trim_end_matches('/').to_string()
    } else {
        url.as_str().to_string()
    }
}

fn cache_key(relay: &str) -> String {
    // FNV-1a is stable across toolchains and needs no heap allocation or crypto
    // dependency. The database claim below remains the collision backstop.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in relay_identity(relay).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn move_if_unclaimed(source: &Path, target: &Path) -> Result<()> {
    if !source.exists() || target.exists() {
        return Ok(());
    }
    fs::rename(source, target)
        .with_context(|| format!("migrating {} to {}", source.display(), target.display()))
}

/// Reads the stored secret key, preferring `BUZZTUI_NSEC` when it is set.
pub fn load_secret(paths: &Paths) -> Result<Option<String>> {
    if let Ok(value) = std::env::var("BUZZTUI_NSEC") {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(Some(value.to_string()));
        }
    }
    if !paths.secret.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&paths.secret)
        .with_context(|| format!("reading {}", paths.secret.display()))?;
    let key = raw.trim();
    if key.is_empty() {
        bail!("{} is empty; run `buzztui login`", paths.secret.display());
    }
    Ok(Some(key.to_string()))
}

/// Writes the secret key with owner-only permissions.
pub fn store_secret(paths: &Paths, nsec: &str) -> Result<()> {
    paths.ensure()?;
    fs::write(&paths.secret, format!("{}\n", nsec.trim()))
        .with_context(|| format!("writing {}", paths.secret.display()))?;
    restrict(&paths.secret)?;
    Ok(())
}

/// Tightens permissions to owner-only on platforms that model them.
#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path)?;
    let mut perms = metadata.permissions();
    let mode = if metadata.is_dir() { 0o700 } else { 0o600 };
    if perms.mode() & 0o777 != mode {
        perms.set_mode(mode);
        fs::set_permissions(path, perms)
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This repository is public. Nothing naming a particular person or a
    /// particular private deployment belongs in it, and a hostname pasted into
    /// a test during development is the easiest way for one to slip out.
    ///
    /// Detection is by shape, not by keyword: a bech32 key is its prefix
    /// followed by a long payload, so the literal `npub1` in parsing code and
    /// the short `npub1abc` of documentation both pass.
    #[test]
    fn no_source_file_names_a_real_deployment_or_identity() {
        fn long_bech32(line: &str, prefix: &str) -> bool {
            line.match_indices(prefix).any(|(at, _)| {
                line[at + prefix.len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    .count()
                    >= 20
            })
        }
        // A private-network hostname has a label before the suffix; the bare
        // string in this test does not.
        fn private_host(line: &str) -> bool {
            line.match_indices(".ts.net").any(|(at, _)| {
                line[..at]
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_ascii_alphanumeric())
            })
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (number, line) in body.lines().enumerate() {
                    if long_bech32(line, "npub1")
                        || long_bech32(line, "nsec1")
                        || private_host(line)
                    {
                        offenders.push(format!(
                            "{}:{}: {}",
                            path.file_name().unwrap_or_default().to_string_lossy(),
                            number + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a real host or key reached the source:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn http_origin_maps_websocket_schemes() {
        let mut config = Config::default();
        config
            .upsert_community(
                "local",
                Community {
                    relay: "ws://localhost:3000".to_string(),
                    gateway: None,
                },
            )
            .unwrap();
        assert_eq!(config.http_origin(), "http://localhost:3000");
        config.communities.get_mut("local").unwrap().relay = "wss://buzz.example.com/".to_string();
        assert_eq!(config.http_origin(), "https://buzz.example.com");
    }

    #[test]
    fn relay_identity_only_folds_the_authority_root_path() {
        assert_eq!(
            relay_identity("wss://EXAMPLE.com"),
            relay_identity("wss://example.com/")
        );
        assert_eq!(
            relay_identity("wss://EXAMPLE.com/Foo/"),
            relay_identity("wss://example.com/Foo/")
        );
        assert_ne!(
            relay_identity("wss://example.com/Foo"),
            relay_identity("wss://example.com/Foo/")
        );
        assert_ne!(
            relay_identity("wss://example.com/Foo"),
            relay_identity("wss://example.com/foo")
        );
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.current, config.current);
        assert_eq!(parsed.communities, config.communities);
        assert_eq!(parsed.ui.sidebar_width, config.ui.sidebar_width);
        assert_eq!(parsed.media.max_rows, config.media.max_rows);
    }
    fn temporary_paths(label: &str) -> Paths {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "buzztui-config-{label}-{}-{serial}",
            std::process::id()
        ));
        Paths {
            config: root.join("config.toml"),
            secret: root.join("secret.key"),
            cache: root.join("cache"),
            log: root.join("buzztui.log"),
            root,
        }
    }

    #[test]
    fn legacy_single_relay_config_migrates_without_losing_settings() {
        let paths = temporary_paths("legacy");
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            &paths.config,
            r#"
relay = "wss://one.example"
gateway = "https://media.example"

[ui]
sidebar_width = 41
"#,
        )
        .unwrap();

        let config = Config::load_or_create(&paths).unwrap();
        let (name, community) = config.current_community().unwrap();
        assert_eq!(name, "one.example");
        assert_eq!(community.relay, "wss://one.example");
        assert_eq!(community.gateway.as_deref(), Some("https://media.example"));
        assert_eq!(config.ui.sidebar_width, 41);

        let saved: toml::Value =
            toml::from_str(&fs::read_to_string(&paths.config).unwrap()).unwrap();
        assert!(saved.get("relay").is_none());
        assert!(saved.get("communities").is_some());
        assert_eq!(
            Config::load_or_create(&paths).unwrap().current,
            "one.example"
        );
        fs::remove_dir_all(&paths.root).ok();
    }

    #[test]
    fn legacy_identity_cache_moves_into_the_selected_community() {
        let paths = temporary_paths("cache");
        let pubkey = "a".repeat(64);
        let legacy = paths.identity_dir(&pubkey);
        fs::create_dir_all(legacy.join("media")).unwrap();
        fs::write(legacy.join("buzz.db"), b"database").unwrap();
        fs::write(legacy.join("media").join("image.png"), b"image").unwrap();

        paths.migrate_legacy_caches("wss://one.example").unwrap();

        assert_eq!(
            fs::read(paths.db_for(&pubkey, "wss://one.example")).unwrap(),
            b"database"
        );
        assert_eq!(
            fs::read(
                paths
                    .media_for(&pubkey, "wss://one.example")
                    .join("image.png")
            )
            .unwrap(),
            b"image"
        );
        assert!(!legacy.join("buzz.db").exists());
        assert!(!legacy.join("media").exists());
        fs::remove_dir_all(&paths.root).ok();
    }

    #[test]
    fn first_launch_override_cannot_claim_the_legacy_community_cache() {
        let paths = temporary_paths("override-migration");
        let pubkey = "b".repeat(64);
        let legacy = paths.identity_dir(&pubkey);
        fs::create_dir_all(&legacy).unwrap();
        let connection = rusqlite::Connection::open(legacy.join("buzz.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                INSERT INTO meta (key, value) VALUES
                    ('owner', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'),
                    ('legacy_marker', 'community one history');",
            )
            .unwrap();
        drop(connection);
        fs::write(
            &paths.config,
            r#"relay = "wss://one.example"
"#,
        )
        .unwrap();

        // Loading performs the assignment from the persisted v0.1.1 relay
        // before an environment or CLI override can replace the runtime relay.
        let mut config = Config::load_or_create(&paths).unwrap();
        config.override_relay("wss://two.example");
        paths.ensure_community(&pubkey, config.relay()).unwrap();

        let original = paths.db_for(&pubkey, "wss://one.example");
        let overridden = paths.db_for(&pubkey, "wss://two.example");
        assert!(original.exists());
        assert!(!overridden.exists());
        let connection = rusqlite::Connection::open(&original).unwrap();
        let marker: String = connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'legacy_marker'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "community one history");
        drop(connection);

        assert!(crate::store::Store::open(&original, &pubkey, "wss://one.example").is_ok());
        assert!(crate::store::Store::open(&original, &pubkey, "wss://two.example").is_err());
        fs::remove_dir_all(&paths.root).ok();
    }
}
