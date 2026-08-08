//! Inline images.
//!
//! Buzz is a workspace where people paste screenshots, and a screenshot
//! rendered as coloured half-blocks is unreadable. When the terminal speaks
//! kitty, sixel, or iTerm2 we hand it real pixels; half-blocks remain as the
//! floor so that the timeline still shows something everywhere else.
//!
//! Everything expensive happens off the render path. `request` never blocks:
//! it starts a download and returns, and the outcome arrives later as a
//! [`MediaEvent`] on the event loop's channel. Resizing works the same way,
//! because encoding a full-resolution screenshot into a graphics protocol
//! takes long enough to be visible as a stutter if it happens during a draw.
//!
//! The disk cache is keyed by a digest of the URL rather than by its path, so
//! two hosts serving `image.png` cannot collide and nothing a relay says ever
//! reaches the filesystem verbatim.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use image::{DynamicImage, GenericImageView};
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::thread::{ResizeRequest, ThreadProtocol};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::MediaConfig;
use crate::store::Store;

/// How long a single image download may take, start to finish. Screenshots are
/// small; anything slower than this is a host that is not going to answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Shown for a URL that a previous run already gave up on, so that a broken
/// link in old scrollback does not generate a request on every launch.
const PREVIOUS_FAILURE: &str = "previously failed to load";

/// What happened to an image we asked for, delivered back to the event loop.
pub enum MediaEvent {
    Loaded { url: String, image: Box<DynamicImage> },
    Failed { url: String, reason: String },
    Resized(ResizeRequest),
}

pub enum Status<'a> {
    /// Not requested yet, or a download is in flight.
    Loading,
    /// Ready to draw; pass the protocol to `StatefulImage` as its state.
    Ready(&'a mut ThreadProtocol),
    Failed(&'a str),
}

/// One tracked URL. Dimensions are kept alongside the protocol because the
/// layout has to know how tall an image wants to be before it draws it, and
/// the protocol does not surrender the source image once it owns it.
enum Entry {
    Loading,
    Ready {
        protocol: ThreadProtocol,
        width: u32,
        height: u32,
    },
    Failed(String),
}

pub struct Media {
    picker: Picker,
    config: MediaConfig,
    cache_dir: PathBuf,
    store: Arc<Store>,
    events: UnboundedSender<MediaEvent>,
    /// Absent when no HTTP client could be built, which leaves inline images
    /// disabled rather than taking the client down.
    client: Option<reqwest::Client>,
    entries: HashMap<String, Entry>,
    /// URLs whose resize requests are in flight, in the order the requests were
    /// posted to the event loop. A [`ResizeRequest`] does not carry any public
    /// identity of its own, so this queue is what lets a completed resize find
    /// its way back to the right image.
    pending: Arc<Mutex<VecDeque<String>>>,
}

impl Media {
    /// `picker` must be built by the caller AFTER entering the alternate screen
    /// and BEFORE the event stream starts reading, because detection writes
    /// query escapes to stdout and parses the replies off stdin.
    pub fn new(
        picker: Picker,
        config: MediaConfig,
        cache_dir: PathBuf,
        store: Arc<Store>,
        events: UnboundedSender<MediaEvent>,
    ) -> Self {
        // rustls is built without a compiled-in default, and reqwest panics
        // rather than erroring when it finds no provider installed. Claiming
        // the slot is a no-op once startup has already filled it.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("buzztui/", env!("CARGO_PKG_VERSION")))
            .build()
            .inspect_err(|err| tracing::warn!(%err, "no http client; inline images disabled"))
            .ok();

        Self {
            picker,
            config,
            cache_dir,
            store,
            events,
            client,
            entries: HashMap::new(),
            pending: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Builds the picker, honouring a forced `protocol` from config and falling
    /// back to half-blocks when detection fails. Never panics.
    pub fn detect(config: &MediaConfig) -> Picker {
        force_protocol(probe(config).0, &config.protocol)
    }

    /// Detects graphics support and reports whether the cell size had to be
    /// assumed, which is the one thing that can make an image look stretched.
    pub fn detect_verbose(config: &MediaConfig) -> (Picker, bool) {
        let (picker, assumed) = probe(config);
        (force_protocol(picker, &config.protocol), assumed)
    }

    /// True when the terminal can draw real pixels rather than half-blocks.
    pub fn high_resolution(&self) -> bool {
        self.picker.protocol_type() != ProtocolType::Halfblocks
    }

    pub fn protocol_name(&self) -> &'static str {
        match self.picker.protocol_type() {
            ProtocolType::Kitty => "kitty",
            ProtocolType::Sixel => "sixel",
            ProtocolType::Iterm2 => "iterm2",
            ProtocolType::Halfblocks => "halfblocks",
        }
    }

    /// Cell size in pixels, used to convert a desired pixel height into rows.
    pub fn font_size(&self) -> (u16, u16) {
        let size = self.picker.font_size();
        (size.width, size.height)
    }

    /// Idempotently begins loading `url`. Safe to call from the render path.
    pub fn request(&mut self, url: &str) {
        if self.entries.contains_key(url) {
            return;
        }

        let (cached, gave_up) = match self.store.media_path(url) {
            Ok(Some((path, failed))) => (path.map(PathBuf::from), failed),
            Ok(None) => (None, false),
            Err(err) => {
                tracing::warn!(%err, url, "cannot read the media cache");
                (None, false)
            }
        };

        // A recorded failure with no file behind it is permanent: re-asking a
        // host that already refused us once only slows the timeline down.
        if cached.is_none() {
            if gave_up {
                self.entries
                    .insert(url.to_owned(), Entry::Failed(PREVIOUS_FAILURE.to_owned()));
                return;
            }
            if let Err(reason) = fetchable(url) {
                record(&self.store, url, None, 0, true);
                self.entries.insert(url.to_owned(), Entry::Failed(reason));
                return;
            }
        }

        self.entries.insert(url.to_owned(), Entry::Loading);
        spawn(load(
            url.to_owned(),
            cached,
            self.client.clone(),
            self.cache_dir.clone(),
            self.config.max_bytes,
            self.store.clone(),
            self.events.clone(),
        ));
    }

    pub fn status(&mut self, url: &str) -> Status<'_> {
        match self.entries.get_mut(url) {
            None | Some(Entry::Loading) => Status::Loading,
            Some(Entry::Ready { protocol, .. }) => Status::Ready(protocol),
            Some(Entry::Failed(reason)) => Status::Failed(reason),
        }
    }

    pub fn handle(&mut self, event: MediaEvent) {
        match event {
            MediaEvent::Loaded { url, image } => {
                let (width, height) = image.dimensions();
                let protocol = self.picker.new_resize_protocol(*image);

                // Each image gets its own request channel so that the worker
                // forwarding those requests knows, without guessing, which URL
                // they belong to.
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                spawn(forward_resizes(
                    rx,
                    url.clone(),
                    self.events.clone(),
                    self.pending.clone(),
                ));

                self.entries.insert(
                    url,
                    Entry::Ready {
                        protocol: ThreadProtocol::new(tx, Some(protocol)),
                        width,
                        height,
                    },
                );
            }
            MediaEvent::Failed { url, reason } => {
                self.entries.insert(url, Entry::Failed(reason));
            }
            MediaEvent::Resized(request) => {
                let queued = self.queue().pop_front();
                let response = match request.resize_encode() {
                    Ok(response) => response,
                    Err(err) => {
                        tracing::debug!(%err, "resizing an image failed");
                        return;
                    }
                };

                // The queue is normally authoritative: the forwarder enqueues a
                // URL and posts its event under the same lock, so queue order
                // and event order agree. The scan is the belt to that braces.
                let target = queued.or_else(|| self.sole_awaiting());
                let Some(target) = target else {
                    tracing::debug!("a resize completed for an image nobody is waiting on");
                    return;
                };
                if let Some(Entry::Ready { protocol, .. }) = self.entries.get_mut(&target)
                    && !protocol.update_resized_protocol(response)
                {
                    tracing::debug!(url = target, "discarded a stale resize");
                }
            }
        }
    }

    /// Rows an image should occupy, preserving aspect ratio within `max_rows`
    /// and the available column count. Returns 0 when the image is unknown.
    pub fn rows_for(&self, url: &str, columns: u16, max_rows: u16) -> u16 {
        let Some(Entry::Ready { width, height, .. }) = self.entries.get(url) else {
            return 0;
        };
        if columns == 0 || max_rows == 0 {
            return 0;
        }

        let (cell_width, cell_height) = self.font_size();
        let natural_columns = divide_up(*width, cell_width.max(1) as u32);
        let natural_rows = divide_up(*height, cell_height.max(1) as u32);

        // Only narrowing matters: an image wider than the area is scaled down
        // by the same factor in both directions, and one that already fits is
        // never blown up past its own resolution.
        let rows = if natural_columns > columns as u32 {
            natural_rows * columns as u32 / natural_columns
        } else {
            natural_rows
        };

        rows.clamp(1, max_rows as u32) as u16
    }

    /// Drops decoded protocols for URLs not in `keep`, bounding memory when the
    /// user scrolls through a long history of images.
    pub fn retain(&mut self, keep: &HashSet<String>) {
        self.entries.retain(|url, _| keep.contains(url));
        self.queue().retain(|url| keep.contains(url));
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        // A poisoned queue means a forwarder panicked between enqueueing and
        // posting. Losing one resize is not worth taking the interface down.
        self.pending.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// The URL of the only image currently without a protocol in hand, if there
    /// is exactly one. With more than one candidate there is nothing to choose
    /// between them, and guessing wrong would show the wrong picture.
    fn sole_awaiting(&self) -> Option<String> {
        let mut awaiting = self.entries.iter().filter(|(_, entry)| {
            matches!(entry, Entry::Ready { protocol, .. } if protocol.protocol_type().is_none())
        });
        match (awaiting.next(), awaiting.next()) {
            (Some((url, _)), None) => Some(url.clone()),
            _ => None,
        }
    }
}

/// Queries the terminal, accepting half-blocks when it does not answer.
fn probe(config: &MediaConfig) -> (Picker, bool) {
    let picker = Picker::from_query_stdio().unwrap_or_else(|err| {
        tracing::debug!(%err, "terminal graphics detection failed");
        Picker::halfblocks()
    });
    if picker.protocol_type() != ProtocolType::Halfblocks {
        return (picker, false);
    }

    // Some hosts answer the capability query affirmatively and then silently
    // discard the image data. Believing them is worse than not asking: the rows
    // are reserved, nothing is drawn, and the user sees a hole. Half-blocks
    // always put something on the screen.
    if let Some(host) = untrustworthy_host() {
        tracing::debug!(
            host,
            "running under a host that reports graphics support it does not deliver; \
             staying on half-blocks. Set media.protocol to override."
        );
        return (picker, false);
    }

    // `from_query_stdio` throws away a protocol it successfully detected when
    // the terminal declines to report its cell size, and terminal multiplexers
    // routinely pass graphics through while answering no geometry query at all.
    // Ask for the capability on its own and supply the size ourselves; an
    // approximate cell only affects how many rows an image occupies, not the
    // resolution it is drawn at.
    match query_graphics_capability(QUERY_TIMEOUT) {
        Some(protocol) => {
            tracing::debug!(
                ?protocol,
                "terminal reports graphics support but no cell size; assuming {:?}",
                config.cell_size
            );
            // `from_fontsize` is deprecated in favour of the querying
            // constructors, but those are exactly the ones that fail here: it is
            // the only way to build a picker with a cell size we had to assume.
            #[allow(deprecated)]
            let mut picker = Picker::from_fontsize(FontSize::new(
                config.cell_size[0].max(1),
                config.cell_size[1].max(1),
            ));
            picker.set_protocol_type(protocol);
            (picker, true)
        }
        None => (picker, false),
    }
}

/// Hosts that answer the kitty capability query but do not forward the image
/// payload to the terminal underneath them. Each entry is an environment
/// variable whose presence identifies the host, verified by hand: a raw kitty
/// transmission inside it produces no picture.
const UNTRUSTWORTHY_HOSTS: &[(&str, &str)] = &[("HERDR_ENV", "herdr")];

fn untrustworthy_host() -> Option<&'static str> {
    UNTRUSTWORTHY_HOSTS.iter().find_map(|(var, name)| {
        std::env::var_os(var)
            .is_some_and(|value| !value.is_empty())
            .then_some(*name)
    })
}

/// How long to wait for the terminal to describe itself.
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);

/// Asks the terminal whether it speaks the kitty graphics protocol.
///
/// The query is terminated with a primary device attributes request, which every
/// terminal answers, so the read finishes even when the graphics query is
/// ignored. Must run after the alternate screen is entered and before the event
/// stream starts consuming input, for the same reason as the crate's own probe.
fn query_graphics_capability(timeout: Duration) -> Option<ProtocolType> {
    use std::io::{IsTerminal, Read, Write};

    // Writing an escape query into a pipe would corrupt whatever is reading it
    // and could never be answered.
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return None;
    }

    // The reply must not be line-buffered or echoed back at the user. Inside the
    // interface raw mode is already on; from `doctor` it is not.
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw && crossterm::terminal::enable_raw_mode().is_err() {
        return None;
    }
    let restore = || {
        if !was_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    };

    let mut stdout = std::io::stdout();
    if stdout
        .write_all(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        restore();
        return None;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut reply = Vec::with_capacity(64);
        let mut byte = [0u8; 1];
        while let Ok(1) = stdin.read(&mut byte) {
            reply.push(byte[0]);
            // The device attributes answer ends in `c` and is the last thing
            // the terminal will send.
            if byte[0] == b'c' && contains(&reply, b"\x1b[?") {
                break;
            }
            if reply.len() >= 512 {
                break;
            }
        }
        let _ = tx.send(reply);
    });

    let reply = rx.recv_timeout(timeout);
    restore();
    contains(&reply.ok()?, b"_Gi=31;OK").then_some(ProtocolType::Kitty)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// Applies a configured protocol override. Detection still runs first, because
/// forcing kitty on a terminal whose cell size we never measured would draw
/// every image at the wrong scale.
fn force_protocol(mut picker: Picker, spec: &str) -> Picker {
    let forced = match spec.trim().to_ascii_lowercase().as_str() {
        "kitty" => ProtocolType::Kitty,
        "sixel" => ProtocolType::Sixel,
        "iterm2" => ProtocolType::Iterm2,
        "halfblocks" => ProtocolType::Halfblocks,
        // "auto", and anything a hand-edited config invented, keeps whatever
        // the terminal told us about itself.
        _ => return picker,
    };
    picker.set_protocol_type(forced);
    picker
}

/// Rejects anything we have no business dereferencing. A relay can put any
/// string in a message; only the two web schemes are ever fetched.
fn fetchable(url: &str) -> Result<(), String> {
    let scheme = url.split_once(':').map_or("", |(scheme, _)| scheme);
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    if scheme.is_empty() {
        return Err("no url scheme; only http and https are fetched".to_owned());
    }
    Err(format!(
        "{} scheme is not fetched; only http and https are",
        scheme.to_ascii_lowercase()
    ))
}

/// Whether a failure is worth remembering across runs. A timeout on a train is
/// not the same as a 404, and recording it as one would blank the image forever.
struct Rejection {
    reason: String,
    permanent: bool,
}

impl Rejection {
    fn permanent(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            permanent: true,
        }
    }

    fn transient(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            permanent: false,
        }
    }
}

/// Loads one image and reports the result exactly once.
async fn load(
    url: String,
    cached: Option<PathBuf>,
    client: Option<reqwest::Client>,
    cache_dir: PathBuf,
    max_bytes: u64,
    store: Arc<Store>,
    events: UnboundedSender<MediaEvent>,
) {
    if let Some(path) = cached {
        match tokio::task::spawn_blocking(move || decode_file(&path)).await {
            Ok(Some(image)) => {
                let _ = events.send(MediaEvent::Loaded { url, image });
                return;
            }
            // A cache file that has been deleted or truncated under us is worth
            // one more download rather than a permanent hole in the timeline.
            Ok(None) => {}
            Err(err) => tracing::warn!(%err, "decoding a cached image failed"),
        }
    }

    if let Err(reason) = fetchable(&url) {
        record(&store, &url, None, 0, true);
        let _ = events.send(MediaEvent::Failed { url, reason });
        return;
    }

    let Some(client) = client else {
        let _ = events.send(MediaEvent::Failed {
            url,
            reason: "no http client is available".to_owned(),
        });
        return;
    };

    let event = match download(&client, &url, max_bytes).await {
        Ok((body, extension)) => {
            let target = url.clone();
            let directory = cache_dir.clone();
            let keeper = store.clone();
            let decoded = tokio::task::spawn_blocking(move || {
                cache_and_decode(&target, body, extension, &directory, &keeper)
            })
            .await;
            match decoded {
                Ok(Ok(image)) => MediaEvent::Loaded {
                    url: url.clone(),
                    image,
                },
                Ok(Err(rejection)) => reject(&store, &url, rejection),
                Err(err) => MediaEvent::Failed {
                    url: url.clone(),
                    reason: format!("decoding did not finish: {err}"),
                },
            }
        }
        Err(rejection) => reject(&store, &url, rejection),
    };

    let _ = events.send(event);
}

/// Downloads the body, refusing anything oversized or obviously not an image
/// before it can occupy memory.
async fn download(
    client: &reqwest::Client,
    url: &str,
    max_bytes: u64,
) -> Result<(Vec<u8>, &'static str), Rejection> {
    let mut response = client.get(url).send().await.map_err(|err| {
        if err.is_timeout() || err.is_connect() {
            Rejection::transient(format!("cannot reach the host: {err}"))
        } else {
            Rejection::permanent(format!("request failed: {err}"))
        }
    })?;

    let status = response.status();
    if !status.is_success() {
        let reason = format!("the server answered {status}");
        return Err(if status.is_server_error() {
            Rejection::transient(reason)
        } else {
            Rejection::permanent(reason)
        });
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // An absent type is tolerated because plenty of hosts omit it, and the
    // decoder is the real arbiter; a type that positively claims to be
    // something else is not.
    if let Some(declared) = content_type.as_deref() {
        let essence = essence(declared);
        if !essence.starts_with("image/") {
            return Err(Rejection::permanent(format!("{essence} is not an image")));
        }
    }

    if let Some(length) = response.content_length()
        && length > max_bytes
    {
        return Err(Rejection::permanent(format!(
            "{length} bytes is over the {max_bytes} byte limit"
        )));
    }

    let mut body = Vec::with_capacity(response.content_length().unwrap_or(0).min(max_bytes) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| Rejection::transient(format!("the download stopped early: {err}")))?
    {
        // A missing or lying Content-Length is normal, so the limit is enforced
        // against what actually arrives.
        if body.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(Rejection::permanent(format!(
                "the body is over the {max_bytes} byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }

    if body.is_empty() {
        return Err(Rejection::permanent("the response was empty"));
    }

    let extension = extension_for(content_type.as_deref(), url);
    Ok((body, extension))
}

/// Decodes the download, then caches the bytes it came from. Decoding first
/// keeps a corrupt response out of the cache, where it would fail forever.
fn cache_and_decode(
    url: &str,
    body: Vec<u8>,
    extension: &str,
    cache_dir: &Path,
    store: &Store,
) -> Result<Box<DynamicImage>, Rejection> {
    let image = image::load_from_memory(&body)
        .map_err(|err| Rejection::permanent(format!("cannot decode the image: {err}")))?;

    let bytes = body.len() as u64;
    let path = cache_dir.join(format!("{}.{extension}", digest(url)));
    let written = std::fs::create_dir_all(cache_dir).and_then(|()| std::fs::write(&path, &body));
    match written {
        Ok(()) => record(store, url, Some(&path.to_string_lossy()), bytes, false),
        Err(err) => {
            // An uncacheable image is a nuisance, not a failure: it is decoded
            // and will show this session, it just costs a download next time.
            tracing::warn!(%err, "cannot write the media cache file");
            record(store, url, None, bytes, false);
        }
    }

    Ok(Box::new(image))
}

fn decode_file(path: &Path) -> Option<Box<DynamicImage>> {
    match image::open(path) {
        Ok(image) => Some(Box::new(image)),
        Err(err) => {
            tracing::debug!(%err, path = %path.display(), "cached image is unreadable");
            None
        }
    }
}

fn reject(store: &Store, url: &str, rejection: Rejection) -> MediaEvent {
    if rejection.permanent {
        record(store, url, None, 0, true);
    }
    MediaEvent::Failed {
        url: url.to_owned(),
        reason: rejection.reason,
    }
}

fn record(store: &Store, url: &str, path: Option<&str>, bytes: u64, failed: bool) {
    if let Err(err) = store.record_media(url, path, bytes, failed) {
        tracing::warn!(%err, url, "cannot record the media outcome");
    }
}

/// Forwards this image's resize requests to the event loop.
///
/// The enqueue and the post happen under one lock so that, with several images
/// resizing at once, the order of the queue matches the order of the events.
/// That correspondence is the only thing tying a completed resize back to the
/// image it belongs to, since `ResizeRequest` exposes no identity of its own.
async fn forward_resizes(
    mut requests: UnboundedReceiver<ResizeRequest>,
    url: String,
    events: UnboundedSender<MediaEvent>,
    pending: Arc<Mutex<VecDeque<String>>>,
) {
    while let Some(request) = requests.recv().await {
        let mut queue = pending.lock().unwrap_or_else(|err| err.into_inner());
        queue.push_back(url.clone());
        if events.send(MediaEvent::Resized(request)).is_err() {
            return;
        }
    }
}

/// Runs background work, tolerating the absence of a runtime rather than
/// panicking inside a draw.
fn spawn(task: impl Future<Output = ()> + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(task);
        }
        Err(err) => tracing::warn!(%err, "no tokio runtime; skipping media work"),
    }
}

/// Cache filenames are a SHA-256 of the URL: stable between runs, free of
/// anything the filesystem could object to, and immune to two hosts both
/// serving `screenshot.png`.
///
/// No hashing crate is a direct dependency of buzztui, but rustls is, and it
/// publishes the SHA-256 implementation backing its TLS 1.3 suites. Borrowing
/// that is cheaper than taking on a crate for one digest.
fn digest(url: &str) -> String {
    let output = rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256
        .tls13()
        .expect("a TLS 1.3 suite is a TLS 1.3 suite")
        .common
        .hash_provider
        .hash(url.as_bytes());

    let bytes = output.as_ref();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The media type without its parameters, lowercased for comparison.
fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Picks the cache file's extension, preferring what the server said over what
/// the URL implies, so that a `.php` endpoint serving PNGs still caches sanely.
fn extension_for(content_type: Option<&str>, url: &str) -> &'static str {
    match content_type.map(essence).as_deref() {
        Some("image/png") => "png",
        Some("image/jpeg" | "image/jpg") => "jpg",
        Some("image/gif") => "gif",
        Some("image/webp") => "webp",
        Some("image/bmp" | "image/x-ms-bmp") => "bmp",
        _ => extension_from_url(url),
    }
}

fn extension_from_url(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('.').next().unwrap_or_default().to_ascii_lowercase().as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        _ => "img",
    }
}

/// Rounds up, because a row of pixels that spills into a cell still needs that
/// whole cell.
fn divide_up(value: u32, divisor: u32) -> u32 {
    value.div_ceil(divisor).max(1)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// A host that claims graphics and then drops the payload leaves a hole on
    /// screen, which is strictly worse than coarse blocks. Verified by hand:
    /// a raw kitty transmission inside a Herdr pane produces no picture.
    #[test]
    fn a_host_that_cannot_deliver_graphics_is_not_believed() {
        // SAFETY: single-threaded test, restored before it returns.
        let previous = std::env::var_os("HERDR_ENV");
        unsafe { std::env::set_var("HERDR_ENV", "1") };
        assert_eq!(untrustworthy_host(), Some("herdr"));

        unsafe { std::env::set_var("HERDR_ENV", "") };
        assert_eq!(untrustworthy_host(), None, "an empty value is not a host");

        unsafe { std::env::remove_var("HERDR_ENV") };
        assert_eq!(untrustworthy_host(), None);
        if let Some(value) = previous {
            unsafe { std::env::set_var("HERDR_ENV", value) };
        }
    }

    /// The capability reply is the only thing that distinguishes "no graphics"
    /// from "graphics, but the terminal will not describe its geometry".
    #[test]
    fn a_kitty_capability_reply_is_recognised_amid_the_device_attributes() {
        // Exactly what a Herdr pane over Ghostty answers.
        let real = b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c";
        assert!(contains(real, b"_Gi=31;OK"));

        // A terminal that answers only the device attributes has no graphics.
        let bare = b"\x1b[?62;22c";
        assert!(!contains(bare, b"_Gi=31;OK"));

        // A truncated reply must not match a prefix of the marker.
        assert!(!contains(b"\x1b_Gi=31;O", b"_Gi=31;OK"));
    }

    #[test]
    fn an_assumed_cell_size_is_never_zero() {
        // A hand-edited config could ask for a zero-sized cell, which would
        // divide by zero when working out how many rows an image needs.
        let mut config = crate::config::MediaConfig::default();
        config.cell_size = [0, 0];
        let width = config.cell_size[0].max(1);
        let height = config.cell_size[1].max(1);
        assert_eq!((width, height), (1, 1));
    }


    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::StatefulWidget;
    use ratatui_image::StatefulImage;

    use super::*;

    const URL: &str = "https://relay.example/screenshot.png";

    /// `Media::detect` itself is deliberately untested: it writes query escapes
    /// to the terminal and puts stdin into raw mode, which a test suite must
    /// never do. Everything it decides beyond that lives in `force_protocol`.
    fn media(protocol: ProtocolType) -> (Media, UnboundedReceiver<MediaEvent>) {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(protocol);
        let store = Arc::new(Store::in_memory("me").expect("in-memory store"));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cache = std::env::temp_dir().join("buzztui-media-tests");
        (
            Media::new(picker, MediaConfig::default(), cache, store, tx),
            rx,
        )
    }

    /// 200x400 pixels against the half-block picker's 10x20 cells is exactly
    /// 20 columns by 20 rows, so the arithmetic below is checkable by hand.
    fn screenshot() -> Box<DynamicImage> {
        Box::new(DynamicImage::new_rgb8(200, 400))
    }

    fn loaded(media: &mut Media, url: &str) {
        media.handle(MediaEvent::Loaded {
            url: url.to_owned(),
            image: screenshot(),
        });
    }

    #[test]
    fn every_forced_protocol_string_is_honoured() {
        let cases = [
            ("kitty", ProtocolType::Kitty),
            ("sixel", ProtocolType::Sixel),
            ("iterm2", ProtocolType::Iterm2),
            ("halfblocks", ProtocolType::Halfblocks),
            ("  KITTY  ", ProtocolType::Kitty),
        ];
        for (spec, expected) in cases {
            let picker = force_protocol(Picker::halfblocks(), spec);
            assert_eq!(picker.protocol_type(), expected, "forcing {spec:?}");
        }
    }

    #[test]
    fn an_unrecognised_protocol_keeps_what_detection_found() {
        let mut detected = Picker::halfblocks();
        detected.set_protocol_type(ProtocolType::Sixel);
        for spec in ["auto", "", "kitten", "sixel-but-better"] {
            let picker = force_protocol(detected.clone(), spec);
            assert_eq!(picker.protocol_type(), ProtocolType::Sixel, "spec {spec:?}");
        }
    }

    #[test]
    fn protocol_name_and_resolution_follow_the_picker() {
        let cases = [
            (ProtocolType::Kitty, "kitty", true),
            (ProtocolType::Sixel, "sixel", true),
            (ProtocolType::Iterm2, "iterm2", true),
            (ProtocolType::Halfblocks, "halfblocks", false),
        ];
        for (protocol, name, high) in cases {
            let (media, _events) = media(protocol);
            assert_eq!(media.protocol_name(), name);
            assert_eq!(media.high_resolution(), high, "{name} resolution");
        }
    }

    #[test]
    fn an_unknown_url_occupies_no_rows() {
        let (media, _events) = media(ProtocolType::Halfblocks);
        assert_eq!(media.rows_for(URL, 80, 16), 0);
    }

    #[tokio::test]
    async fn rows_preserve_aspect_ratio_inside_the_limits() {
        let (mut media, _events) = media(ProtocolType::Halfblocks);
        loaded(&mut media, URL);

        // Wide enough for the image's natural 20 columns: it keeps its 20 rows.
        assert_eq!(media.rows_for(URL, 40, 32), 20);
        // Half the columns it wants, so half the rows it wants.
        assert_eq!(media.rows_for(URL, 10, 32), 10);
        assert_eq!(media.rows_for(URL, 5, 32), 5);
        // The ceiling wins over the natural height.
        assert_eq!(media.rows_for(URL, 40, 16), 16);
        // A sliver of space still draws something rather than nothing.
        assert_eq!(media.rows_for(URL, 1, 16), 1);
        // No space at all draws nothing.
        assert_eq!(media.rows_for(URL, 0, 16), 0);
        assert_eq!(media.rows_for(URL, 40, 0), 0);
    }

    #[tokio::test]
    async fn a_failed_image_occupies_no_rows() {
        let (mut media, _events) = media(ProtocolType::Halfblocks);
        media.handle(MediaEvent::Failed {
            url: URL.to_owned(),
            reason: "gone".to_owned(),
        });
        assert_eq!(media.rows_for(URL, 40, 16), 0);
    }

    #[test]
    fn non_web_schemes_are_refused_by_name() {
        let (mut media, _events) = media(ProtocolType::Halfblocks);
        for (url, scheme) in [
            ("file:///etc/passwd", "file"),
            ("ftp://example.invalid/pic.png", "ftp"),
            ("FILE:///tmp/pic.png", "file"),
        ] {
            media.request(url);
            match media.status(url) {
                Status::Failed(reason) => assert!(
                    reason.contains(scheme),
                    "reason {reason:?} should name the {scheme} scheme"
                ),
                _ => panic!("{url} should have been refused"),
            }
        }
    }

    #[test]
    fn a_url_without_a_scheme_is_refused() {
        let (mut media, _events) = media(ProtocolType::Halfblocks);
        media.request("relay.example/pic.png");
        match media.status("relay.example/pic.png") {
            Status::Failed(reason) => assert!(reason.contains("scheme"), "reason {reason:?}"),
            _ => panic!("a schemeless url should have been refused"),
        }
    }

    #[tokio::test]
    async fn requesting_a_loaded_image_again_does_not_reset_it() {
        let (mut media, _events) = media(ProtocolType::Halfblocks);
        loaded(&mut media, URL);

        media.request(URL);

        assert!(matches!(media.status(URL), Status::Ready(_)));
        assert_eq!(media.rows_for(URL, 40, 32), 20);
    }

    #[test]
    fn requesting_a_refused_image_again_does_not_reset_it() {
        let (mut media, _events) = media(ProtocolType::Halfblocks);
        media.request("ftp://example.invalid/pic.png");
        media.request("ftp://example.invalid/pic.png");
        assert!(matches!(
            media.status("ftp://example.invalid/pic.png"),
            Status::Failed(_)
        ));
    }

    #[tokio::test]
    async fn retain_keeps_only_the_urls_still_on_screen() {
        let (mut media, _events) = media(ProtocolType::Halfblocks);
        let keeper = "https://relay.example/keep.png";
        let goner = "https://relay.example/drop.png";
        loaded(&mut media, keeper);
        loaded(&mut media, goner);

        let keep: HashSet<String> = std::iter::once(keeper.to_owned()).collect();
        media.retain(&keep);

        assert!(matches!(media.status(keeper), Status::Ready(_)));
        assert_eq!(media.rows_for(keeper, 40, 32), 20);
        assert!(matches!(media.status(goner), Status::Loading));
        assert_eq!(media.rows_for(goner, 40, 32), 0);
    }

    /// The whole point of the threaded protocol is that a draw hands the resize
    /// off and carries on. This drives one full round trip: render, which takes
    /// the protocol away and posts a request, then the event that puts it back.
    #[tokio::test]
    async fn a_resize_round_trip_restores_the_protocol() {
        let (mut media, mut events) = media(ProtocolType::Halfblocks);
        loaded(&mut media, URL);

        let area = Rect::new(0, 0, 8, 4);
        let mut buffer = Buffer::empty(area);
        let Status::Ready(protocol) = media.status(URL) else {
            panic!("the image should be ready");
        };
        StatefulImage::<ThreadProtocol>::default().render(area, &mut buffer, protocol);

        let Status::Ready(protocol) = media.status(URL) else {
            panic!("the image should still be ready");
        };
        assert!(
            protocol.protocol_type().is_none(),
            "the draw should have handed the protocol to the resize worker"
        );

        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("a resize request should reach the event loop")
            .expect("the channel should stay open");
        assert!(matches!(event, MediaEvent::Resized(_)));
        media.handle(event);

        let Status::Ready(protocol) = media.status(URL) else {
            panic!("the image should still be ready");
        };
        assert!(
            protocol.protocol_type().is_some(),
            "the resize should have handed the protocol back"
        );
    }

    #[test]
    fn cache_names_are_stable_and_url_specific() {
        assert_eq!(digest(URL), digest(URL));
        assert_ne!(digest(URL), digest("https://other.example/screenshot.png"));
        assert_eq!(digest(URL).len(), 64);
        assert!(digest(URL).chars().all(|c| c.is_ascii_hexdigit()));
        // A known vector, so a change of hash function cannot pass unnoticed.
        assert_eq!(
            digest(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn cache_extensions_prefer_the_served_type() {
        assert_eq!(
            extension_for(Some("image/png; charset=binary"), "https://x/y.jpg"),
            "png"
        );
        assert_eq!(extension_for(None, "https://x/y.JPEG?v=2"), "jpg");
        assert_eq!(extension_for(None, "https://x/render.php"), "img");
        assert_eq!(extension_for(None, "https://x/y.webp#frag"), "webp");
    }
}
