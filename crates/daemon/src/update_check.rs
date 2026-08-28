//! GitHub Releases version checker.
//!
//! Polls `api.github.com` once on startup and once per day. Notifies via the
//! provided callback when a newer version is detected. No telemetry beyond
//! the bare HTTPS GET. Disabled entirely when `check_for_updates = false`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Newest release of this fork, prereleases included. `releases/latest` skips
/// prereleases entirely, so a fork that ships `-rc` tags would either see no
/// release at all or, when pointed at the upstream project, be told that an
/// unrelated upstream version is "newer" than its own prerelease.
const RELEASES_API: &str = "https://api.github.com/repos/sehoon123/LeopardWM/releases?per_page=1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_DELAY: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60 * 24);

/// Public release URL for the GUI / tray click action.
pub const RELEASES_PAGE_URL: &str = "https://github.com/sehoon123/LeopardWM/releases";

/// Spawn the background update-checker thread.
///
/// `on_update_found` runs on the worker thread when a newer release tag is
/// observed. `cancel` lets shutdown abort sleeps early.
pub fn spawn_update_checker<F>(
    cancel: Arc<AtomicBool>,
    on_update_found: F,
) -> Option<std::thread::JoinHandle<()>>
where
    F: Fn(String) + Send + 'static,
{
    match std::thread::Builder::new()
        .name("leopardwm-update-check".to_string())
        .spawn(move || {
            interruptible_sleep(STARTUP_DELAY, &cancel);
            while !cancel.load(Ordering::SeqCst) {
                run_check_once(&on_update_found);
                interruptible_sleep(POLL_INTERVAL, &cancel);
            }
        }) {
        Ok(handle) => Some(handle),
        Err(error) => {
            warn!("Failed to spawn update checker: {error}");
            None
        }
    }
}

fn run_check_once(on_update_found: &impl Fn(String)) {
    match fetch_latest_release_tag() {
        Some(tag) => {
            let current = env!("CARGO_PKG_VERSION");
            if is_newer(&tag, current) {
                info!("Update available: {} (current: {})", tag, current);
                on_update_found(tag);
            } else {
                debug!("Up to date (latest: {}, current: {})", tag, current);
            }
        }
        None => debug!("Update check failed (network/parse error)"),
    }
}

/// Fetch the `tag_name` of the latest GitHub release. Returns `None` on any
/// network or parse failure — the caller treats this as "unknown, try again
/// tomorrow."
fn fetch_latest_release_tag() -> Option<String> {
    let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
    let body = agent
        .get(RELEASES_API)
        .set(
            "User-Agent",
            concat!("LeopardWM/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    parse_latest_release_tag(&body)
}

/// Read the newest tag from a releases-list response. Accepts a single release
/// object too so the endpoint can be pointed back at `releases/latest`.
fn parse_latest_release_tag(body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let release = match &json {
        serde_json::Value::Array(releases) => releases.first()?,
        object => object,
    };
    release.get("tag_name")?.as_str().map(String::from)
}

/// Compare `latest` (e.g. `v0.1.11`) against `current` (e.g. `0.1.10`).
/// Strips a leading `v` from either side. Fails closed (`false`) when either
/// side cannot be parsed as semver.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let l = semver::Version::parse(latest.trim_start_matches('v'));
    let c = semver::Version::parse(current.trim_start_matches('v'));
    match (l, c) {
        (Ok(l), Ok(c)) => l > c,
        _ => false,
    }
}

fn interruptible_sleep(total: Duration, cancel: &AtomicBool) {
    let chunk = Duration::from_secs(1);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if cancel.load(Ordering::SeqCst) {
            return;
        }
        let step = remaining.min(chunk);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_basic() {
        assert!(is_newer("v0.1.11", "0.1.10"));
        assert!(is_newer("0.2.0", "v0.1.10"));
        assert!(is_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn not_newer_when_same() {
        assert!(!is_newer("v0.1.10", "0.1.10"));
        assert!(!is_newer("0.1.10", "v0.1.10"));
    }

    #[test]
    fn not_newer_when_older() {
        assert!(!is_newer("v0.1.9", "0.1.10"));
        assert!(!is_newer("0.0.1", "1.0.0"));
    }

    #[test]
    fn fork_prereleases_are_not_superseded_by_upstream_finals() {
        // The fork's own newest tag must not read as an update, and an upstream
        // final release must never be compared against it at all: this is the
        // pairing that fired a bogus "update available" toast.
        assert!(!is_newer("v0.2.6-sehoon.24-rc3", "0.2.6-sehoon.24-rc3"));
        assert!(is_newer("v0.2.6-sehoon.25-rc1", "0.2.6-sehoon.24-rc3"));
        assert!(RELEASES_API.contains("sehoon123/LeopardWM"));
        assert!(RELEASES_PAGE_URL.contains("sehoon123/LeopardWM"));
    }

    #[test]
    fn latest_tag_is_read_from_a_prerelease_listing() {
        let listing = r#"[{"tag_name":"v0.2.6-sehoon.24-rc3","prerelease":true}]"#;
        assert_eq!(
            parse_latest_release_tag(listing).as_deref(),
            Some("v0.2.6-sehoon.24-rc3")
        );
        let single = r#"{"tag_name":"v0.3.0"}"#;
        assert_eq!(parse_latest_release_tag(single).as_deref(), Some("v0.3.0"));
        assert_eq!(parse_latest_release_tag("[]"), None);
        assert_eq!(parse_latest_release_tag("not json"), None);
    }

    #[test]
    fn fail_closed_on_unparseable() {
        assert!(!is_newer("garbage", "0.1.10"));
        assert!(!is_newer("v0.1.10", "not-a-version"));
        assert!(!is_newer("", ""));
    }
}
