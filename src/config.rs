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
use serde::{Deserialize, Serialize};

/// Directory name used under the configuration root.
const APP_DIR: &str = "buzztui";

/// Written at the top of a generated `config.toml`. Every setting here can be
/// changed by hand; the interface reloads them without a restart.
const CONFIG_HEADER: &str = r#"# buzztui configuration — https://github.com/bscott/buzz-tui
#
# Edit this by hand and reload from inside buzztui with the reload_config
# binding (ctrl+b R by default), or restart.
#
#   relay    websocket address of your community, ws:// or wss://
#   gateway  http base for media and relay metadata. omit it and the relay's
#            own host is used, which is right for most deployments. set it
#            when a gateway or cdn serves media from elsewhere, for example
#            gateway = "https://media.example.com"
"#;


/// Resolved locations of every file buzztui reads or writes.
///
/// The cache is deliberately not a single path. It holds decrypted direct
/// messages and per-account read state, so each identity gets its own
/// directory: rotating or importing a key must never hand one account's
/// private timeline to another.
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

    /// The cache directory belonging to one identity.
    fn identity_dir(&self, pubkey: &str) -> PathBuf {
        // A prefix is enough to separate accounts and keeps the path readable;
        // a collision would need a deliberate 64-bit preimage.
        let short: String = pubkey.chars().take(16).collect();
        self.cache.join(short)
    }

    pub fn db_for(&self, pubkey: &str) -> PathBuf {
        self.identity_dir(pubkey).join("buzz.db")
    }

    pub fn media_for(&self, pubkey: &str) -> PathBuf {
        self.identity_dir(pubkey).join("media")
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

    /// Creates the per-identity cache, owner-readable only.
    pub fn ensure_identity(&self, pubkey: &str) -> Result<()> {
        self.ensure()?;
        let media = self.media_for(pubkey);
        fs::create_dir_all(&media).with_context(|| format!("creating {}", media.display()))?;
        restrict(&self.identity_dir(pubkey))?;
        Ok(())
    }
}

/// User-facing configuration, persisted as TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// WebSocket URL of the active Buzz relay. One relay is one community.
    pub relay: String,
    /// HTTP base for media, uploads, and relay metadata. Most deployments serve
    /// these from the same host as the relay, so this is normally left unset
    /// and derived from `relay`. Set it when a gateway sits in front, or when
    /// media lives on a different host entirely.
    pub gateway: Option<String>,
    pub ui: UiConfig,
    pub media: MediaConfig,
    /// Per-token palette overrides applied on top of the named theme, keyed by
    /// the token name: `accent = "#f5c2e7"`.
    pub theme: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relay: String::new(),
            gateway: None,
            ui: UiConfig::default(),
            media: MediaConfig::default(),
            theme: BTreeMap::new(),
        }
    }
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

impl Config {
    /// Loads `config.toml`, writing a commented default file when absent.
    pub fn load_or_create(paths: &Paths) -> Result<Self> {
        if !paths.config.exists() {
            paths.ensure()?;
            let config = Self::default();
            config.save(paths)?;
            return Ok(config);
        }
        let raw = fs::read_to_string(&paths.config)
            .with_context(|| format!("reading {}", paths.config.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", paths.config.display()))?;
        Ok(config)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure()?;
        let body = toml::to_string_pretty(self).context("serialising configuration")?;
        let doc = format!("{CONFIG_HEADER}\n{body}");
        fs::write(&paths.config, doc)
            .with_context(|| format!("writing {}", paths.config.display()))?;
        Ok(())
    }

    /// Applies environment overrides. `BUZZTUI_RELAY` wins over `BUZZ_RELAY_URL`
    /// so that a buzztui-specific setting can override the shared Buzz variable.
    pub fn apply_env(&mut self) {
        for key in ["BUZZTUI_RELAY", "BUZZ_RELAY_URL"] {
            if let Ok(value) = std::env::var(key) {
                if !value.trim().is_empty() {
                    self.relay = value.trim().to_string();
                    return;
                }
            }
        }
    }

    /// Whether a community has actually been chosen. The relay stays empty
    /// until someone says otherwise, so a freshly written configuration can
    /// never be mistaken for a configured one.
    pub fn has_community(&self) -> bool {
        !self.relay.trim().is_empty()
    }

    /// The HTTP origin used for Blossom media downloads and NIP-11 metadata:
    /// the configured gateway when there is one, otherwise the relay's own host.
    pub fn http_origin(&self) -> String {
        if let Some(gateway) = self
            .gateway
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
        {
            return gateway.trim_end_matches('/').to_string();
        }
        self.http_origin_from_relay()
    }

    /// The HTTP origin the relay's own host implies, ignoring any gateway.
    pub fn http_origin_from_relay(&self) -> String {
        let relay = self.relay.trim_end_matches('/');
        match relay.split_once("://") {
            Some(("wss", rest)) => format!("https://{rest}"),
            Some(("ws", rest)) => format!("http://{rest}"),
            _ => relay.to_string(),
        }
    }

    /// Resolves a possibly relative media path against the relay origin, which
    /// is how Buzz's Blossom endpoint addresses blobs it hosts itself.
    pub fn resolve_media(&self, url: &str) -> String {
        if url.starts_with("http://") || url.starts_with("https://") {
            return url.to_string();
        }
        format!("{}/{}", self.http_origin(), url.trim_start_matches('/'))
    }

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
        config.relay = "ws://localhost:3000".to_string();
        assert_eq!(config.http_origin(), "http://localhost:3000");
        config.relay = "wss://buzz.example.com/".to_string();
        assert_eq!(config.http_origin(), "https://buzz.example.com");
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.relay, config.relay);
        assert_eq!(parsed.ui.sidebar_width, config.ui.sidebar_width);
        assert_eq!(parsed.media.max_rows, config.media.max_rows);
    }
}
