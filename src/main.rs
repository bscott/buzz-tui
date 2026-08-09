//! buzztui: a terminal client for Buzz relays.

mod app;
mod composer;
mod config;
mod keys;
mod media;
mod model;
mod net;
mod onboard;
mod overlay;
mod proto;
mod store;
mod ui;
mod update;

use std::io::{IsTerminal, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream, KeyEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use nostr::key::Keys;
use nostr::nips::nip19::ToBech32;
use tokio::sync::mpsc;

use crate::app::App;
use crate::config::{Config, Paths};
use crate::keys::{KeyFile, Keymap};
use crate::media::{Media, MediaEvent};
use crate::store::Store;

/// How often housekeeping runs. Nothing animates, so this only has to be fast
/// enough that a toast expiring feels immediate.
const TICK: Duration = Duration::from_millis(250);

#[derive(Parser)]
#[command(
    name = "buzztui",
    about = "a high-resolution terminal client for Buzz",
    version
)]
struct Cli {
    /// Relay to connect to, overriding the configuration file.
    #[arg(long, short, global = true)]
    relay: Option<String>,
    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(Subcommand)]
enum Sub {
    /// Choose a community and an identity. Runs automatically on first launch.
    Setup,
    /// Store an identity, generating one when none is supplied.
    Login {
        /// Import an existing key, prompting for it without echoing.
        #[arg(long)]
        import: bool,
        /// Read an existing key from a file, for scripted installs.
        #[arg(long, value_name = "PATH")]
        nsec_file: Option<PathBuf>,
        /// Pass a key directly. Unsafe: your shell records it in history and
        /// the process list exposes it. Prefer `--import` or `--nsec-file`.
        #[arg(long, value_name = "NSEC")]
        nsec: Option<String>,
        /// Replace an identity that is already stored.
        #[arg(long)]
        force: bool,
    },
    /// Print the public key of the stored identity.
    Whoami,
    /// Print where configuration, keys, and the cache live.
    Paths,
    /// Print the active keybindings, including any from `keys.toml`.
    Keys,
    /// Check that the configuration, identity, and database are usable.
    Doctor,
}

fn main() -> Result<()> {
    // Both reqwest and tokio-tungstenite construct rustls clients, and doing so
    // without a process-wide provider installed panics rather than erroring.
    // Setup reaches for HTTP before any of them, so it is installed here first.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let paths = Paths::resolve()?;
    // Whether a community has been chosen is a property of the configuration,
    // not of the file existing: reading it creates one.
    paths.ensure()?;

    let mut config = Config::load_or_create(&paths)?;
    config.apply_env();
    if let Some(relay) = cli.relay.clone() {
        config.override_relay(relay);
    }

    match cli.command {
        Some(Sub::Setup) => onboard::run(&paths, &mut config),
        Some(Sub::Login {
            import,
            nsec_file,
            nsec,
            force,
        }) => {
            // A key is meaningless without a community to present it to, so
            // collect that first when nobody has chosen one yet.
            if !config.has_community() && std::io::stdin().is_terminal() {
                onboard::prompt_community(&paths, &mut config)?;
                println!();
            }
            login(&paths, import, nsec_file.as_deref(), nsec.as_deref(), force)
        }
        Some(Sub::Whoami) => whoami(&paths),
        Some(Sub::Paths) => {
            println!("config  {}", paths.config.display());
            println!("keys    {}", paths.secret.display());
            println!("cache   {}", paths.cache.display());
            if let Ok(Some(secret)) = config::load_secret(&paths)
                && let Ok(keys) = Keys::parse(&secret)
                && let Some((name, community)) = config.current_community()
            {
                let me = keys.public_key().to_hex();
                println!("current {name}");
                println!("data    {}", paths.db_for(&me, &community.relay).display());
                println!(
                    "media   {}",
                    paths.media_for(&me, &community.relay).display()
                );
            }
            println!("log     {}", paths.log.display());
            Ok(())
        }
        Some(Sub::Keys) => {
            print_keys(&paths);
            Ok(())
        }
        Some(Sub::Doctor) => doctor(&paths, &config),
        None => {
            // A community and an identity are both required before the
            // interface can show anything; ask for whichever is missing.
            if onboard::is_first_run(&paths, &config) {
                onboard::run(&paths, &mut config)?;
                println!("\nstarting buzztui\n");
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("starting the async runtime")?;
            runtime.block_on(run(config, paths))
        }
    }
}

// ------------------------------------------------------------------ identity

/// Loads the stored identity, or creates one on first run. A chat client that
/// refuses to start because it has no key is a worse first impression than one
/// that quietly makes a key and tells you where it put it.
fn load_or_create_identity(paths: &Paths) -> Result<(Keys, bool)> {
    match config::load_secret(paths)? {
        Some(secret) => Ok((
            Keys::parse(&secret).context("the stored secret key is not a valid nsec or hex key")?,
            false,
        )),
        None => {
            let keys = Keys::generate();
            let nsec = keys
                .secret_key()
                .to_bech32()
                .map_err(|_| anyhow::anyhow!("could not encode the generated key"))?;
            config::store_secret(paths, &nsec)?;
            Ok((keys, true))
        }
    }
}

fn login(
    paths: &Paths,
    import: bool,
    nsec_file: Option<&std::path::Path>,
    nsec: Option<&str>,
    force: bool,
) -> Result<()> {
    if paths.secret.exists() && !force {
        bail!(
            "{} already holds an identity; pass --force to replace it",
            paths.secret.display()
        );
    }

    let supplied = match (import, nsec_file, nsec) {
        (_, Some(path), _) => Some(onboard::read_key_file(path)?),
        (_, None, Some(secret)) => {
            // Honour it, but do not let it pass silently: the key is now in
            // shell history and was visible in the process list while running.
            eprintln!(
                "warning: --nsec puts your secret key in shell history and the process list.\n\
                 warning: prefer --import or --nsec-file, and consider clearing your history."
            );
            Some(secret.to_string())
        }
        (true, None, None) => Some(onboard::prompt_secret("nsec (hidden)")?),
        (false, None, None) => None,
    };

    let generated = supplied.is_none();
    let keys = match supplied {
        Some(secret) => {
            Keys::parse(secret.trim()).context("that is not a valid nsec or hex secret key")?
        }
        None => Keys::generate(),
    };
    onboard::store(paths, &keys)?;

    let npub = keys
        .public_key()
        .to_bech32()
        .unwrap_or_else(|_| keys.public_key().to_hex());
    println!("identity stored in {}", paths.secret.display());
    println!("public key  {npub}");
    if generated {
        println!(
            "\nthis key is your account. back up {} — there is no recovery.",
            paths.secret.display()
        );
    }
    Ok(())
}

fn whoami(paths: &Paths) -> Result<()> {
    let Some(secret) = config::load_secret(paths)? else {
        bail!("no identity yet; run `buzztui login`");
    };
    let keys = Keys::parse(&secret)?;
    println!(
        "{}",
        keys.public_key()
            .to_bech32()
            .unwrap_or_else(|_| keys.public_key().to_hex())
    );
    println!("{}", keys.public_key().to_hex());
    Ok(())
}

/// Prints the binding table the client is actually using, so a rebind can be
/// checked without launching the interface.
fn print_keys(paths: &Paths) {
    let (file, problems) = KeyFile::load(&paths.root.join("keys.toml"));
    let keymap = Keymap::with_overrides(file);
    for problem in problems.iter().chain(keymap.diagnostics.iter()) {
        eprintln!("warning: {problem}");
    }
    println!("leader  {}\n", keymap.leader);
    for (group, rows) in keymap.help_rows() {
        println!("{}", group.label());
        let width = rows
            .iter()
            .map(|r| r.keys.chars().count())
            .max()
            .unwrap_or(0);
        for row in rows {
            println!(
                "  {:<width$}  {:<30}{}",
                row.keys,
                row.description,
                row.action,
                width = width
            );
        }
        println!();
    }
    println!(
        "rebind by action name in {}",
        paths.root.join("keys.toml").display()
    );
}

fn doctor(paths: &Paths, config: &Config) -> Result<()> {
    let mut problems = 0usize;
    if let Some((name, community)) = config.current_community() {
        println!("community  {name}");
        println!("relay      {}", community.relay);
        if nostr::types::RelayUrl::parse(&community.relay).is_err() {
            println!("           ! not a ws:// or wss:// url");
            problems += 1;
        }
    } else {
        println!("community  ! none configured");
        problems += 1;
    }

    match config::load_secret(paths) {
        Ok(Some(secret)) => match Keys::parse(&secret) {
            Ok(keys) => println!(
                "identity   {}",
                keys.public_key()
                    .to_bech32()
                    .unwrap_or_else(|_| keys.public_key().to_hex())
            ),
            Err(err) => {
                println!("identity   ! unreadable: {err}");
                problems += 1;
            }
        },
        Ok(None) => println!("identity   none yet; one will be generated on first run"),
        Err(err) => {
            println!("identity   ! {err}");
            problems += 1;
        }
    }

    match config::load_secret(paths)
        .ok()
        .flatten()
        .and_then(|s| Keys::parse(&s).ok())
    {
        Some(keys) => {
            let me = keys.public_key().to_hex();
            if let Some((_, community)) = config.current_community() {
                let db = paths.db_for(&me, &community.relay);
                paths.ensure_community(&me, &community.relay)?;
                match Store::open(&db, &me, &community.relay) {
                    Ok(_) => println!("database   {}", db.display()),
                    Err(err) => {
                        println!("database   ! {err}");
                        problems += 1;
                    }
                }
            } else {
                println!("database   ! no community configured");
            }
        }
        // Opening a cache under a placeholder pubkey would stamp it with an
        // owner nobody holds the key for.
        None => println!("database   none yet"),
    }

    // Probing writes escape sequences and reads the replies, which is safe here
    // because doctor owns the terminal and never enters the alternate screen.
    let (picker, assumed_cell) = Media::detect_verbose(&config.media);
    let media = Media::new(
        picker,
        config.media.clone(),
        std::env::temp_dir().join("buzztui-probe"),
        Arc::new(Store::scratch()?),
        mpsc::unbounded_channel().0,
    );
    let (cell_w, cell_h) = media.font_size();
    let note = if !media.high_resolution() {
        "  (no pixel protocol detected; images use unicode blocks)".to_string()
    } else if assumed_cell {
        format!(
            "  (cell {cell_w}x{cell_h} assumed — this terminal answers the capability\n           query but not the cell-size one; set media.cell_size if images look stretched)"
        )
    } else {
        format!("  (cell {cell_w}x{cell_h})")
    };
    println!("graphics   {}{note}", media.protocol_name());

    let (file, mut diagnostics) = KeyFile::load(&paths.root.join("keys.toml"));
    let keymap = Keymap::with_overrides(file);
    diagnostics.extend(keymap.diagnostics.iter().cloned());
    if diagnostics.is_empty() {
        println!(
            "keys       {} bindings",
            keymap.help_rows().values().map(Vec::len).sum::<usize>()
        );
    } else {
        for problem in &diagnostics {
            println!("keys       ! {problem}");
        }
        problems += diagnostics.len();
    }

    if problems == 0 {
        println!("\neverything looks fine");
        Ok(())
    } else {
        bail!("{problems} problem(s) found")
    }
}

// ----------------------------------------------------------------- main loop

async fn run(config: Config, paths: Paths) -> Result<()> {
    init_logging(&paths);
    // Linux reports `/proc/self/exe` with a ` (deleted)` suffix after an atomic
    // self-update. Keep the launch path while the original inode still exists.
    let executable = std::env::current_exe().context("locating the running executable")?;

    let (keypair, generated) = load_or_create_identity(&paths)?;
    let me = keypair.public_key().to_hex();
    let (_, community) = config
        .current_community()
        .context("no current community; run `buzztui setup`")?;
    let relay_url = community.relay.clone();
    paths.ensure_community(&me, &relay_url)?;
    let store = Arc::new(Store::open(
        &paths.db_for(&me, &relay_url),
        &me,
        &relay_url,
    )?);

    let (file, mut diagnostics) = KeyFile::load(&paths.root.join("keys.toml"));
    let keymap = Keymap::with_overrides(file);
    diagnostics.extend(keymap.diagnostics.iter().cloned());

    let (relay, mut relay_updates) = net::spawn(relay_url.clone(), keypair.clone());
    let mut update_rx = update::spawn(update::Request::Check);
    let (media_tx, mut media_rx) = mpsc::unbounded_channel::<MediaEvent>();

    let mut terminal = ratatui::try_init().context("could not take over the terminal")?;
    let mouse = execute!(stdout(), EnableMouseCapture).is_ok();

    // Protocol detection writes escape sequences to stdout and reads the replies
    // from stdin, so it has to happen after the alternate screen is up and
    // before the event stream starts consuming input.
    let picker = Media::detect(&config.media);
    let media = Media::new(
        picker,
        config.media.clone(),
        paths.media_for(&me, &relay_url),
        Arc::clone(&store),
        media_tx,
    );

    let mut app = App::new(
        config,
        paths,
        keypair,
        store,
        relay,
        media,
        keymap,
        diagnostics,
    );
    app.mouse = mouse;
    if generated {
        app.toast(
            app::ToastKind::Info,
            "new identity created",
            Some("run `buzztui whoami` to see your public key".to_string()),
        );
    }

    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut relay_updates,
        &mut media_rx,
        &mut update_rx,
    )
    .await;
    let restart = app.take_restart_request();

    app.shutdown();
    // Give the relay task a moment to flush the offline presence event.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    if restart {
        result?;
        restart_process(&executable)
    } else {
        result
    }
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    relay_updates: &mut mpsc::UnboundedReceiver<net::Update>,
    media_rx: &mut mpsc::UnboundedReceiver<MediaEvent>,
    update_rx: &mut mpsc::UnboundedReceiver<update::Event>,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut update_done = false;

    while app.running {
        if let Some(request) = app.take_update_request() {
            *update_rx = update::spawn(request);
            update_done = false;
        }
        if app.dirty {
            terminal.draw(|frame| ui::render(frame, app))?;
            app.dirty = false;
            // Only images still on screen are worth keeping decoded.
            let keep = app.visible_images();
            app.media.retain(&keep);
        }

        tokio::select! {
            event = input.next() => match event {
                Some(Ok(TermEvent::Key(key))) => {
                    // Windows reports a press and a release for every key, so an
                    // unfiltered handler would act on each keystroke twice.
                    if key.kind == KeyEventKind::Press {
                        app.on_key(key);
                    }
                }
                Some(Ok(TermEvent::Mouse(mouse))) => app.on_mouse(mouse),
                Some(Ok(TermEvent::Resize(_, _))) => app.dirty = true,
                Some(Ok(TermEvent::Paste(text))) => {
                    app.composer.insert_str(&text);
                    app.dirty = true;
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => return Err(err.into()),
                None => break,
            },
            update = relay_updates.recv() => match update {
                Some(update) => app.on_relay(update),
                None => break,
            },
            event = media_rx.recv() => {
                if let Some(event) = event {
                    app.on_media(event);
                }
            }
            event = update_rx.recv(), if !update_done => {
                update_done = true;
                if let Some(event) = event {
                    app.on_update(event);
                }
            }
            _ = tick.tick() => app.on_tick(),
        }
    }
    Ok(())
}

fn restart_command(executable: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command.args(std::env::args_os().skip(1));
    command
}

#[cfg(unix)]
fn restart_process(executable: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let error = restart_command(executable).exec();
    bail!("could not restart buzztui: {error}")
}

#[cfg(not(unix))]
fn restart_process(_executable: &Path) -> Result<()> {
    bail!("automatic restart is unavailable on this platform")
}

/// Logs go to a file, because anything written to the terminal would be painted
/// over by the next frame.
fn init_logging(paths: &Paths) {
    use tracing_subscriber::EnvFilter;
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)
    else {
        return;
    };
    let filter = EnvFilter::try_from_env("BUZZTUI_LOG")
        .unwrap_or_else(|_| EnvFilter::new("buzztui=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn restart_uses_the_path_captured_before_atomic_replacement() {
        let dir = std::env::temp_dir().join(format!("buzztui-restart-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let executable = dir.join("buzztui");
        std::fs::write(&executable, b"old executable").unwrap();

        let captured = executable.clone();
        let replacement = dir.join(".buzztui.update");
        std::fs::write(&replacement, b"new executable").unwrap();
        std::fs::rename(replacement, &executable).unwrap();

        let command = restart_command(&captured);
        let program = Path::new(command.get_program());
        assert_eq!(program, captured);
        assert_eq!(std::fs::read(program).unwrap(), b"new executable");
        std::fs::remove_dir_all(dir).ok();
    }
}
