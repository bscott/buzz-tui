//! Non-blocking release checks and verified self-updates.
//!
//! Checks are advisory: networking, authentication, and the local cache keep
//! working when GitHub does not. Installation is explicit. It downloads the
//! target archive and the release checksum manifest, verifies the archive,
//! reads only the expected binary from it, and atomically replaces the running
//! executable without unpacking arbitrary paths.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

const LATEST_RELEASE: &str = "https://api.github.com/repos/bscott/buzz-tui/releases/latest";
const RELEASE_BY_TAG: &str = "https://api.github.com/repos/bscott/buzz-tui/releases/tags";
const ASSET_PREFIX: &str = "https://github.com/bscott/buzz-tui/releases/download/";
const CHECK_TIMEOUT: Duration = Duration::from_secs(6);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_RELEASE_BYTES: usize = 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 64 * 1024;
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Checking,
    Current,
    Available(Version),
    Installing(Version),
    Installed(Version),
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Check,
    Install(Version),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Checked(Status),
    Installed(Version),
    InstallFailed { version: Version, error: String },
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Starts one update operation and returns its one-shot result channel.
pub fn spawn(request: Request) -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let event = match request {
            Request::Check => Event::Checked(match latest().await {
                Ok(status) => status,
                Err(err) => {
                    tracing::debug!(%err, "release update check unavailable");
                    Status::Unavailable
                }
            }),
            Request::Install(version) => match install(&version).await {
                Ok(()) => Event::Installed(version),
                Err(err) => {
                    tracing::warn!(%err, %version, "release update failed");
                    Event::InstallFailed {
                        version,
                        error: format!("{err:#}"),
                    }
                }
            },
        };
        let _ = tx.send(event);
    });
    rx
}

async fn latest() -> Result<Status> {
    let client = client(CHECK_TIMEOUT)?;
    let release = fetch_release(&client, LATEST_RELEASE).await?;
    compare(env!("CARGO_PKG_VERSION"), &release.tag_name)
}

async fn install(version: &Version) -> Result<()> {
    let target = release_target()?;
    let client = client(INSTALL_TIMEOUT)?;
    let url = format!("{RELEASE_BY_TAG}/v{version}");
    let release = fetch_release(&client, &url).await?;
    let released = parse_version(&release.tag_name)?;
    ensure!(
        &released == version,
        "GitHub returned v{released} while v{version} was requested"
    );

    let archive_name = format!("buzztui-v{version}-{target}.tar.gz");
    let archive_url = asset_url(&release, &archive_name)?;
    let checksums_url = asset_url(&release, "SHA256SUMS")?;
    let checksums = download(&client, checksums_url, MAX_CHECKSUM_BYTES).await?;
    let archive = download(&client, archive_url, MAX_ARCHIVE_BYTES).await?;
    let binary_path = format!("buzztui-v{version}-{target}/buzztui");
    let binary = verified_binary(&archive, &checksums, &archive_name, &binary_path)?;
    let executable = std::env::current_exe().context("locating the running executable")?;
    replace_executable(&executable, &binary)
}

fn client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("buzztui/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the update client")
}

async fn fetch_release(client: &reqwest::Client, url: &str) -> Result<Release> {
    let body = download(client, url, MAX_RELEASE_BYTES).await?;
    serde_json::from_slice(&body).context("decoding the GitHub release")
}

async fn download(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("GitHub rejected {url}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        bail!("download from {url} exceeds the {limit}-byte limit");
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading {url}"))?;
        ensure!(
            body.len().saturating_add(chunk.len()) <= limit,
            "download from {url} exceeds the {limit}-byte limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn asset_url<'a>(release: &'a Release, name: &str) -> Result<&'a str> {
    let url = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.as_str())
        .with_context(|| format!("release asset {name} is missing"))?;
    ensure!(
        url.starts_with(ASSET_PREFIX),
        "release asset {name} has an unexpected download URL"
    );
    Ok(url)
}

fn release_target() -> Result<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-musl"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-musl"),
        ("x86_64", "macos") => Ok("x86_64-apple-darwin"),
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        (arch, os) => bail!("self-update is not available for {arch}-{os}"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn verified_binary(
    archive: &[u8],
    checksums: &[u8],
    archive_name: &str,
    binary_path: &str,
) -> Result<Vec<u8>> {
    let expected = checksum_for(checksums, archive_name)?;
    let actual = sha256_hex(archive);
    ensure!(
        actual.eq_ignore_ascii_case(expected),
        "checksum mismatch for {archive_name}"
    );

    let mut tar = tar::Archive::new(GzDecoder::new(archive));
    let mut binary = None;
    for entry in tar.entries().context("reading the release archive")? {
        let mut entry = entry.context("reading an entry from the release archive")?;
        if entry.path().context("reading an archive path")? != Path::new(binary_path) {
            continue;
        }
        ensure!(
            entry.header().entry_type().is_file(),
            "{binary_path} is not a regular file"
        );
        ensure!(binary.is_none(), "the archive contains {binary_path} twice");
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(MAX_BINARY_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("reading the release executable")?;
        ensure!(
            bytes.len() as u64 <= MAX_BINARY_BYTES,
            "the release executable exceeds the size limit"
        );
        ensure!(!bytes.is_empty(), "the release executable is empty");
        binary = Some(bytes);
    }
    binary.with_context(|| format!("release archive does not contain {binary_path}"))
}

fn checksum_for<'a>(manifest: &'a [u8], name: &str) -> Result<&'a str> {
    let manifest = std::str::from_utf8(manifest).context("SHA256SUMS is not UTF-8")?;
    for line in manifest.lines() {
        let mut fields = line.split_whitespace();
        let Some(checksum) = fields.next() else {
            continue;
        };
        let Some(file) = fields.next() else {
            continue;
        };
        if file.trim_start_matches('*') != name {
            continue;
        }
        ensure!(
            checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "SHA256SUMS has an invalid checksum for {name}"
        );
        return Ok(checksum);
    }
    bail!("SHA256SUMS does not name {name}")
}

/// Writes beside the current binary before renaming, so the replacement cannot
/// expose a partially written executable and never crosses a filesystem.
fn replace_executable(path: &Path, binary: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("the executable has no parent directory")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("the executable name is not UTF-8")?;
    let permissions = fs::metadata(path)
        .with_context(|| format!("reading permissions for {}", path.display()))?
        .permissions();
    let (temporary, mut file) = create_temporary(parent, name)?;

    let result = (|| -> Result<()> {
        file.write_all(binary)
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.set_permissions(permissions)
            .with_context(|| format!("setting permissions on {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
        drop(file);
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "replacing {}; the installation may be read-only",
                path.display()
            )
        })?;
        // The rename is already complete. A directory sync only strengthens
        // crash durability and must not turn that success into a reported
        // failure on filesystems that reject directory handles.
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary(parent: &Path, name: &str) -> Result<(PathBuf, File)> {
    for serial in 0..100 {
        let path = parent.join(format!(".{name}.update-{}-{serial}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    bail!("could not reserve a temporary executable beside {name}")
}

fn parse_version(tag: &str) -> Result<Version> {
    let tag = tag.trim().strip_prefix('v').unwrap_or(tag.trim());
    if tag.is_empty() {
        bail!("the latest release has no version tag");
    }
    Version::parse(tag).context("the release tag is not semantic")
}

fn compare(current: &str, latest: &str) -> Result<Status> {
    let current = Version::parse(current).context("the built version is not semantic")?;
    let latest = parse_version(latest)?;
    Ok(if latest > current {
        Status::Available(latest)
    } else {
        Status::Current
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn only_a_newer_semantic_release_is_an_update() {
        assert_eq!(
            compare("0.1.0", "v0.2.0").unwrap(),
            Status::Available(Version::new(0, 2, 0))
        );
        assert_eq!(compare("0.2.0", "v0.2.0").unwrap(), Status::Current);
        assert_eq!(compare("0.2.0", "v0.1.9").unwrap(), Status::Current);
        assert_eq!(compare("1.0.0", "v1.0.0-rc.1").unwrap(), Status::Current);
    }

    #[test]
    fn malformed_release_tags_do_not_claim_an_update() {
        assert!(compare("0.1.0", "latest").is_err());
        assert!(compare("0.1.0", "v").is_err());
    }

    #[test]
    fn release_assets_must_come_from_this_repository() {
        let good = Release {
            tag_name: "v1.0.0".into(),
            assets: vec![Asset {
                name: "SHA256SUMS".into(),
                browser_download_url:
                    "https://github.com/bscott/buzz-tui/releases/download/v1.0.0/SHA256SUMS".into(),
            }],
        };
        assert!(asset_url(&good, "SHA256SUMS").is_ok());

        let bad = Release {
            tag_name: "v1.0.0".into(),
            assets: vec![Asset {
                name: "SHA256SUMS".into(),
                browser_download_url: "https://example.com/SHA256SUMS".into(),
            }],
        };
        assert!(asset_url(&bad, "SHA256SUMS").is_err());
    }

    #[test]
    fn a_verified_archive_atomically_replaces_the_executable() {
        let dir = test_dir("success");
        let executable = dir.join("buzztui");
        fs::write(&executable, b"old executable").unwrap();
        let (archive, name, path) = archive_fixture(b"new executable");
        let checksum = format!("{}  {name}\n", sha256_hex(&archive));

        let binary = verified_binary(&archive, checksum.as_bytes(), &name, &path).unwrap();
        replace_executable(&executable, &binary).unwrap();

        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_bad_checksum_cannot_touch_the_executable() {
        let dir = test_dir("checksum");
        let executable = dir.join("buzztui");
        fs::write(&executable, b"old executable").unwrap();
        let (archive, name, path) = archive_fixture(b"untrusted executable");
        let checksum = format!("{}  {name}\n", "0".repeat(64));

        assert!(verified_binary(&archive, checksum.as_bytes(), &name, &path).is_err());
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_release_archive_must_contain_the_exact_binary_path() {
        let (archive, name, _) = archive_fixture(b"new executable");
        let checksum = format!("{}  {name}\n", sha256_hex(&archive));
        assert!(
            verified_binary(
                &archive,
                checksum.as_bytes(),
                &name,
                "another-directory/buzztui"
            )
            .is_err()
        );
    }

    fn archive_fixture(binary: &[u8]) -> (Vec<u8>, String, String) {
        let name = "buzztui-v9.8.7-x86_64-unknown-linux-musl.tar.gz".to_string();
        let path = "buzztui-v9.8.7-x86_64-unknown-linux-musl/buzztui".to_string();
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, path.as_str(), binary)
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        (encoder.finish().unwrap(), name, path)
    }

    fn test_dir(label: &str) -> PathBuf {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "buzztui-update-{label}-{}-{serial}",
            std::process::id()
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
