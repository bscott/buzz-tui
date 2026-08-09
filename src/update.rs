//! Non-blocking release update detection.
//!
//! The check is deliberately advisory: networking, authentication, and the
//! local cache must all work when GitHub does not. A failed check is logged and
//! leaves the version label alone; an available release stays visible in the
//! footer for the rest of the session.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Deserialize;
use tokio::sync::mpsc;

const LATEST_RELEASE: &str = "https://api.github.com/repos/bscott/buzz-tui/releases/latest";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Checking,
    Current,
    Available(Version),
    Unavailable,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

/// Starts one release lookup and returns its result channel. There is no retry
/// loop: this is a courtesy check, not a service the client depends on.
pub fn spawn() -> mpsc::UnboundedReceiver<Status> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let status = match latest().await {
            Ok(status) => status,
            Err(err) => {
                tracing::debug!(%err, "release update check unavailable");
                Status::Unavailable
            }
        };
        let _ = tx.send(status);
    });
    rx
}

async fn latest() -> Result<Status> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("buzztui/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the update client")?;
    let response = client
        .get(LATEST_RELEASE)
        .send()
        .await
        .context("requesting the latest release")?
        .error_for_status()
        .context("GitHub did not return a release")?;
    let release: Release = serde_json::from_str(
        &response
            .text()
            .await
            .context("reading the latest release")?,
    )
    .context("decoding the latest release")?;
    compare(env!("CARGO_PKG_VERSION"), &release.tag_name)
}

fn compare(current: &str, latest: &str) -> Result<Status> {
    let current = Version::parse(current).context("the built version is not semantic")?;
    let latest = latest.trim().strip_prefix('v').unwrap_or(latest.trim());
    if latest.is_empty() {
        bail!("the latest release has no version tag");
    }
    let latest = Version::parse(latest).context("the release tag is not semantic")?;
    Ok(if latest > current {
        Status::Available(latest)
    } else {
        Status::Current
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
