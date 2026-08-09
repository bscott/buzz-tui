//! First-run setup.
//!
//! A new user needs two things before anything else can work: the community they
//! are joining, and an identity to join it with. Both are collected here, in the
//! terminal, before the interface starts — a secret key must never be typed into
//! a screen that is redrawing around it, and never passed as a command-line
//! argument where the shell will record it.

use std::fs;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use nostr::key::Keys;
use nostr::nips::nip19::ToBech32;
use nostr::types::RelayUrl;

use crate::config::{self, Community, Config, Paths};

/// Where the identity came from. An identity that was already on disk is kept
/// distinct so that setup never treats reconfiguring a community as a reason to
/// issue a new account.
pub enum Identity {
    Existing(Keys),
    Generated(Keys),
    Imported(Keys),
}

impl Identity {
    pub fn keys(&self) -> &Keys {
        match self {
            Identity::Existing(keys) | Identity::Generated(keys) | Identity::Imported(keys) => keys,
        }
    }
}

/// True when this install still needs a community or an identity.
///
/// Both are required, and either can be missing on its own: someone who ran
/// `login` has a key but has never named a relay, and someone restoring a
/// configuration has a relay but no key. `configured` reports whether a
/// configuration. Presence of a configuration file proves nothing, since
/// reading one creates it; an unset relay is the signal.
pub fn is_first_run(paths: &Paths, config: &Config) -> bool {
    let has_identity = config::load_secret(paths).ok().flatten().is_some();
    !has_identity || !config.has_community()
}

/// Walks a new user through choosing a community and an identity.
pub fn run(paths: &Paths, config: &mut Config) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("setup needs a terminal; run `buzztui setup` directly");
    }

    println!("buzztui setup\n");
    println!("a buzz community is one relay. you need its address, and a key to");
    println!("identify yourself with.\n");

    prompt_community(paths, config)?;
    println!();

    let identity = prompt_identity(paths)?;
    let keys = identity.keys();
    let npub = keys
        .public_key()
        .to_bech32()
        .unwrap_or_else(|_| keys.public_key().to_hex());

    println!("\nsetup complete");
    let (name, community) = config
        .current_community()
        .expect("setup just stored a community");
    println!("  community  {name} ({})", community.relay);
    println!("  identity   {npub}");
    println!("  files      {}", paths.root.display());

    if matches!(identity, Identity::Generated(_)) {
        println!(
            "\nthis key is your account. back up {} — there is no recovery.",
            paths.secret.display()
        );
    }

    // A key the relay has never seen is refused, and the refusal is opaque
    // unless you already know what to ask for. Say it once, up front.
    if !matches!(identity, Identity::Existing(_)) {
        println!("\nmost relays admit known keys only. whoever runs this one can add you:");
        println!("  buzz-admin add-member --pubkey {npub}");
        println!("\nbuzztui shows that request again, ready to paste, if you are refused.");
    }
    Ok(())
}

/// Asks for the community: the address you would open, and the relay behind it.
///
/// The gateway comes first because it is the address people actually have —
/// the one in a browser or an invitation. The relay is derived from it, so the
/// second question is usually one keypress.
///
/// Exposed separately from [`run`] so that `login` can collect it too: being
/// asked for a key before anyone has said which community it is for makes no
/// sense.
pub fn prompt_community(paths: &Paths, config: &mut Config) -> Result<()> {
    let gateway = prompt_gateway(config)?;
    let suggested = relay_from_gateway(&gateway);
    let relay = prompt_relay(&suggested)?;
    let name = prompt_line(
        "community name",
        Some(&config::suggested_community_name(&relay)),
    )?;

    // Only record a gateway that is not simply the relay's own host; carrying a
    // redundant one would silently break if the relay later moves.
    let mut community = Community {
        relay,
        gateway: None,
    };
    let derived = community.http_origin_from_relay();
    community.gateway = (!gateway.is_empty() && gateway != derived).then_some(gateway);
    config.upsert_community(&name, community)?;
    config.save(paths)
}

/// The web address of the community, which is also where its media is served
/// from unless a deployment splits them.
fn prompt_gateway(config: &Config) -> Result<String> {
    let current = config
        .current_community()
        .and_then(|(_, community)| community.gateway.clone())
        .filter(|gateway| !gateway.is_empty())
        .or_else(|| config.has_community().then(|| config.http_origin()))
        .unwrap_or_default();

    loop {
        let answer = prompt_line("community address", Some(&current))?;
        let answer = answer.trim().trim_end_matches('/').to_string();
        if answer.is_empty() {
            println!("  enter the address you open this community at");
            continue;
        }
        return Ok(match answer.split_once("://") {
            Some(("http" | "https", _)) => answer,
            Some(("ws", rest)) => format!("http://{rest}"),
            Some(("wss", rest)) => format!("https://{rest}"),
            Some((scheme, _)) => {
                println!("  `{scheme}://` is not a web address; use https://");
                continue;
            }
            None => format!("https://{answer}"),
        });
    }
}

/// The websocket address a gateway implies, which is the same host by default.
fn relay_from_gateway(gateway: &str) -> String {
    match gateway.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        _ => String::new(),
    }
}

/// Asks for the relay address, accepting the shapes people actually type.
fn prompt_relay(suggested: &str) -> Result<String> {
    loop {
        let answer = prompt_line("relay websocket url", Some(suggested))?;
        match normalise_relay(&answer) {
            Ok(url) => {
                match describe(&url) {
                    Some(name) => println!("  connected to {name}"),
                    None => println!("  could not reach it just now; saving anyway"),
                }
                return Ok(url);
            }
            Err(err) => println!("  {err}"),
        }
    }
}

/// Accepts `wss://host`, `https://host`, or a bare hostname, and produces the
/// WebSocket URL a relay actually speaks. A bare name is assumed to be secure
/// unless it is plainly local, because anything on the open internet should be.
pub fn normalise_relay(input: &str) -> Result<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("enter an address, such as wss://buzz.example.com");
    }

    let candidate = match trimmed.split_once("://") {
        Some(("ws" | "wss", _)) => trimmed.to_string(),
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        Some((scheme, _)) => bail!("`{scheme}://` is not a relay address; use wss://"),
        None => {
            let host = trimmed.split(['/', ':']).next().unwrap_or(trimmed);
            let local = matches!(host, "localhost" | "127.0.0.1" | "::1")
                || host.starts_with("192.168.")
                || host.starts_with("10.");
            if local {
                format!("ws://{trimmed}")
            } else {
                format!("wss://{trimmed}")
            }
        }
    };

    RelayUrl::parse(&candidate)
        .with_context(|| format!("`{input}` is not a usable relay address"))?;
    Ok(candidate)
}

/// Reads the relay's NIP-11 document, purely to tell the user they typed the
/// address correctly. Any failure is informational, never fatal.
fn describe(relay: &str) -> Option<String> {
    let origin = match relay.split_once("://") {
        Some(("wss", rest)) => format!("https://{rest}"),
        Some(("ws", rest)) => format!("http://{rest}"),
        _ => return None,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(6))
            .build()
            .ok()?;
        let body = client
            .get(&origin)
            .header("Accept", "application/nostr+json")
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?;
        let document: serde_json::Value = serde_json::from_str(&body).ok()?;
        let name = document.get("name")?.as_str()?.to_string();
        let nips = document
            .get("supported_nips")
            .and_then(|n| n.as_array())
            .map(|list| {
                let has = |nip: u64| list.iter().any(|v| v.as_u64() == Some(nip));
                // NIP-29 is what makes a relay a Buzz community rather than a
                // general-purpose one; saying so early avoids a puzzling
                // empty channel list later.
                if has(29) {
                    ""
                } else {
                    " (no nip-29 groups — channels may be empty)"
                }
            })
            .unwrap_or_default();
        Some(format!("{name}{nips}"))
    })
}

/// Offers to keep, create, or import an identity.
///
/// An identity already on disk is kept by default and can only be replaced by
/// typing the word out. Losing a secret key means losing the account — there is
/// no recovery — so no single keystroke may destroy one.
fn prompt_identity(paths: &Paths) -> Result<Identity> {
    let existing = config::load_secret(paths)
        .ok()
        .flatten()
        .and_then(|secret| Keys::parse(secret.trim()).ok());

    println!("identity");
    match &existing {
        Some(keys) => {
            println!("  you already have one: {}", npub_of(keys));
            println!("  1  keep it");
            println!("  2  replace it with a new key");
            println!("  3  replace it with a key you already have");
        }
        None => {
            println!("  1  create a new key");
            println!("  2  paste a key you already have");
        }
    }

    loop {
        let choice = prompt_line("choice", Some("1"))?;
        let choice = choice.trim();
        match (existing.as_ref(), choice) {
            (Some(keys), "1" | "") => return Ok(Identity::Existing(keys.clone())),
            (Some(current), "2") | (Some(current), "3") => {
                if !confirm_replacement(current)? {
                    println!("  keeping the existing key");
                    return Ok(Identity::Existing(current.clone()));
                }
                if choice == "2" {
                    let keys = Keys::generate();
                    store(paths, &keys)?;
                    return Ok(Identity::Generated(keys));
                }
                if let Some(keys) = ask_for_key()? {
                    store(paths, &keys)?;
                    return Ok(Identity::Imported(keys));
                }
            }
            (None, "1" | "") => {
                let keys = Keys::generate();
                store(paths, &keys)?;
                return Ok(Identity::Generated(keys));
            }
            (None, "2") => {
                if let Some(keys) = ask_for_key()? {
                    store(paths, &keys)?;
                    return Ok(Identity::Imported(keys));
                }
            }
            (Some(_), _) => println!("  enter 1, 2, or 3"),
            (None, _) => println!("  enter 1 or 2"),
        }
    }
}

fn ask_for_key() -> Result<Option<Keys>> {
    let secret = prompt_secret("  nsec (hidden)")?;
    match Keys::parse(secret.trim()) {
        Ok(keys) => Ok(Some(keys)),
        Err(_) => {
            println!("  that is not an nsec or a 64-character hex key");
            Ok(None)
        }
    }
}

/// Requires the word, not a keystroke. A mistyped `y` should not be able to
/// discard an account that cannot be recovered.
fn confirm_replacement(current: &Keys) -> Result<bool> {
    println!("\n  this discards {} permanently.", npub_of(current));
    println!("  anything signed with it stays under that key, and it cannot be recovered.");
    let answer = prompt_line("  type `replace` to continue", Some(""))?;
    Ok(answer.trim().eq_ignore_ascii_case("replace"))
}

fn npub_of(keys: &Keys) -> String {
    keys.public_key()
        .to_bech32()
        .unwrap_or_else(|_| keys.public_key().to_hex())
}

pub fn store(paths: &Paths, keys: &Keys) -> Result<()> {
    let secret = keys
        .secret_key()
        .to_bech32()
        .map_err(|_| anyhow::anyhow!("could not encode the key"))?;
    config::store_secret(paths, &secret)
}

/// Reads a visible line, offering a default the user can accept with Enter.
pub fn prompt_line(label: &str, default: Option<&str>) -> Result<String> {
    let mut out = std::io::stdout();
    match default.filter(|d| !d.is_empty()) {
        Some(default) => write!(out, "{label} [{default}]: ")?,
        None => write!(out, "{label}: ")?,
    }
    out.flush()?;

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim().to_string();
    Ok(match (answer.is_empty(), default) {
        (true, Some(default)) => default.to_string(),
        _ => answer,
    })
}

/// Reads a line without echoing it.
///
/// A secret key must not appear on screen, in scrollback, or in a screen share.
/// This is also why importing a key is never a command-line argument: the shell
/// would record it in history and the process list would expose it.
pub fn prompt_secret(label: &str) -> Result<String> {
    let mut out = std::io::stdout();
    write!(out, "{label}: ")?;
    out.flush()?;

    if !std::io::stdin().is_terminal() {
        // Piped input is not echoed anywhere in the first place.
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        println!();
        return Ok(line.trim().to_string());
    }

    crossterm::terminal::enable_raw_mode().context("could not turn off echo")?;
    let secret = read_hidden();
    let _ = crossterm::terminal::disable_raw_mode();
    println!();
    secret
}

fn read_hidden() -> Result<String> {
    let mut stdin = std::io::stdin().lock();
    let mut typed = String::new();
    let mut byte = [0u8; 1];
    while stdin.read(&mut byte)? == 1 {
        match byte[0] {
            b'\r' | b'\n' => break,
            // Ctrl+C and Ctrl+D abort rather than submitting a partial key.
            0x03 | 0x04 => bail!("cancelled"),
            0x7f | 0x08 => {
                typed.pop();
            }
            byte if byte.is_ascii_graphic() => typed.push(byte as char),
            _ => {}
        }
    }
    Ok(typed)
}

/// Reads a key from a file, which is the safe way to script an import.
pub fn read_key_file(path: &Path) -> Result<String> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let key = raw.trim().to_string();
    if key.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::normalise_relay;

    #[test]
    fn a_bare_public_hostname_becomes_secure_websockets() {
        // What people actually type when asked for their community: a bare
        // hostname, including the multi-label kind a private network hands out.
        assert_eq!(
            normalise_relay("buzz.example.org").unwrap(),
            "wss://buzz.example.org"
        );
        assert_eq!(
            normalise_relay("relay.team.internal.example").unwrap(),
            "wss://relay.team.internal.example"
        );
        assert_eq!(
            normalise_relay("  buzz.example.com/  ").unwrap(),
            "wss://buzz.example.com"
        );
    }

    #[test]
    fn a_local_address_stays_plain() {
        // Nobody runs TLS on a loopback dev relay, and defaulting to wss there
        // would fail in a way that looks like the relay is down.
        assert_eq!(
            normalise_relay("localhost:3000").unwrap(),
            "ws://localhost:3000"
        );
        assert_eq!(
            normalise_relay("127.0.0.1:3000").unwrap(),
            "ws://127.0.0.1:3000"
        );
        assert_eq!(
            normalise_relay("192.168.1.10:3000").unwrap(),
            "ws://192.168.1.10:3000"
        );
    }

    #[test]
    fn browser_urls_are_translated_rather_than_rejected() {
        // People paste what is in their address bar.
        assert_eq!(
            normalise_relay("https://buzz.example.com").unwrap(),
            "wss://buzz.example.com"
        );
        assert_eq!(
            normalise_relay("http://localhost:3000").unwrap(),
            "ws://localhost:3000"
        );
    }

    #[test]
    fn an_explicit_websocket_url_is_left_alone() {
        assert_eq!(
            normalise_relay("wss://buzz.example.com").unwrap(),
            "wss://buzz.example.com"
        );
        assert_eq!(
            normalise_relay("ws://10.0.0.5:3000").unwrap(),
            "ws://10.0.0.5:3000"
        );
    }

    #[test]
    fn nonsense_is_refused_with_something_actionable() {
        assert!(normalise_relay("").is_err());
        assert!(normalise_relay("   ").is_err());
        let err = normalise_relay("ftp://buzz.example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("wss://"), "{err}");
    }
}
