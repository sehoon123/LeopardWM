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
/// Final shutdown retry covers the maximum in-flight HTTP request plus margin.
pub(crate) const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(12);
const STARTUP_DELAY: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60 * 24);

/// Public release URL for the GUI / tray click action.
pub const RELEASES_PAGE_URL: &str = "https://github.com/sehoon123/LeopardWM/releases";

/// One process-owned update worker. Disabling checks leaves the thread dormant
/// so a rapid false→true toggle cannot race a canceled worker or create two.
pub struct UpdateCheckWorker {
    enabled: Arc<AtomicBool>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    cancel: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl UpdateCheckWorker {
    pub fn new() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    pub fn reconcile<F>(&mut self, enabled: bool, on_update_found: F)
    where
        F: Fn(String, u64) + Send + 'static,
    {
        self.reconcile_with(enabled, |cancel, worker_enabled, generation| {
            spawn_update_checker(cancel, worker_enabled, generation, on_update_found)
        });
    }

    fn reconcile_with<F>(&mut self, enabled: bool, spawn: F)
    where
        F: FnOnce(
            Arc<AtomicBool>,
            Arc<AtomicBool>,
            Arc<std::sync::atomic::AtomicU64>,
        ) -> Option<std::thread::JoinHandle<()>>,
    {
        if self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
        if self.enabled.swap(enabled, Ordering::AcqRel) != enabled {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        if enabled && self.thread.is_none() {
            self.cancel.store(false, Ordering::Release);
            self.thread = spawn(
                self.cancel.clone(),
                self.enabled.clone(),
                self.generation.clone(),
            );
        }
    }

    pub fn accepts(&self, generation: u64) -> bool {
        self.enabled.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) == generation
    }

    pub fn shutdown(&mut self) -> Option<std::thread::JoinHandle<()>> {
        if self.enabled.swap(false, Ordering::AcqRel) {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        self.cancel.store(true, Ordering::Release);
        self.thread.take()
    }
}

/// Spawn the background update-checker thread.
///
/// `on_update_found` runs on the worker thread when a newer release tag is
/// observed. `cancel` stops the process-owned thread; `enabled` suspends all
/// network work without destroying the worker generation.
fn spawn_update_checker<F>(
    cancel: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    on_update_found: F,
) -> Option<std::thread::JoinHandle<()>>
where
    F: Fn(String, u64) + Send + 'static,
{
    match std::thread::Builder::new()
        .name("leopardwm-update-check".to_string())
        .spawn(move || {
            while !cancel.load(Ordering::Acquire) {
                while !enabled.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                    interruptible_sleep(Duration::from_secs(1), &cancel);
                }
                let active_generation = generation.load(Ordering::Acquire);
                if cancel.load(Ordering::Acquire)
                    || !interruptible_sleep_while_enabled(
                        STARTUP_DELAY,
                        &cancel,
                        &enabled,
                        &generation,
                        active_generation,
                    )
                {
                    continue;
                }
                while enabled.load(Ordering::Acquire)
                    && generation.load(Ordering::Acquire) == active_generation
                    && !cancel.load(Ordering::Acquire)
                {
                    run_check_once(&on_update_found, &enabled, &generation, active_generation);
                    if !interruptible_sleep_while_enabled(
                        POLL_INTERVAL,
                        &cancel,
                        &enabled,
                        &generation,
                        active_generation,
                    ) {
                        break;
                    }
                }
            }
        }) {
        Ok(handle) => Some(handle),
        Err(error) => {
            warn!("Failed to spawn update checker: {error}");
            None
        }
    }
}

fn run_check_once(
    on_update_found: &impl Fn(String, u64),
    enabled: &AtomicBool,
    generation: &std::sync::atomic::AtomicU64,
    expected_generation: u64,
) {
    run_check_once_with(
        fetch_latest_release_tag,
        on_update_found,
        enabled,
        generation,
        expected_generation,
    );
}

fn run_check_once_with(
    fetch: impl FnOnce() -> Option<String>,
    on_update_found: &impl Fn(String, u64),
    enabled: &AtomicBool,
    generation: &std::sync::atomic::AtomicU64,
    expected_generation: u64,
) {
    if !enabled.load(Ordering::Acquire) || generation.load(Ordering::Acquire) != expected_generation
    {
        return;
    }
    match fetch() {
        Some(tag) => {
            if !enabled.load(Ordering::Acquire)
                || generation.load(Ordering::Acquire) != expected_generation
            {
                return;
            }
            let current = env!("CARGO_PKG_VERSION");
            if is_newer(&tag, current) {
                info!("Update available: {} (current: {})", tag, current);
                on_update_found(tag, expected_generation);
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
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let step = remaining.min(chunk);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}

fn interruptible_sleep_while_enabled(
    total: Duration,
    cancel: &AtomicBool,
    enabled: &AtomicBool,
    generation: &std::sync::atomic::AtomicU64,
    expected_generation: u64,
) -> bool {
    let chunk = Duration::from_secs(1);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if cancel.load(Ordering::Acquire)
            || !enabled.load(Ordering::Acquire)
            || generation.load(Ordering::Acquire) != expected_generation
        {
            return false;
        }
        let step = remaining.min(chunk);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    !cancel.load(Ordering::Acquire)
        && enabled.load(Ordering::Acquire)
        && generation.load(Ordering::Acquire) == expected_generation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_check_toggle_reconciles_running_worker() {
        use std::sync::atomic::AtomicUsize;

        let starts = Arc::new(AtomicUsize::new(0));
        let mut worker = UpdateCheckWorker::new();
        worker.reconcile_with(false, |_, _, _| panic!("disabled startup must not spawn"));
        assert!(worker.thread.is_none());

        let starts_for_spawn = starts.clone();
        worker.reconcile_with(true, move |cancel, _, _| {
            starts_for_spawn.fetch_add(1, Ordering::SeqCst);
            Some(std::thread::spawn(move || {
                while !cancel.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }))
        });
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(worker.enabled.load(Ordering::Acquire));
        let first_generation = worker.generation.load(Ordering::Acquire);
        assert!(worker.accepts(first_generation));

        worker.reconcile_with(true, |_, _, _| panic!("duplicate enable must not spawn"));
        worker.reconcile_with(false, |_, _, _| panic!("disable must not spawn"));
        assert!(!worker.enabled.load(Ordering::Acquire));
        assert!(!worker.accepts(first_generation));
        worker.reconcile_with(true, |_, _, _| panic!("dormant worker must be reused"));
        assert!(worker.enabled.load(Ordering::Acquire));
        assert!(!worker.accepts(first_generation));
        assert!(worker.accepts(worker.generation.load(Ordering::Acquire)));
        assert_eq!(starts.load(Ordering::SeqCst), 1);

        let mut thread = worker.shutdown();
        thread.take().unwrap().join().unwrap();
        assert!(!worker.enabled.load(Ordering::Acquire));
    }

    #[test]
    fn disabled_generation_cannot_notify_after_false_true_aba() {
        let enabled = AtomicBool::new(true);
        let generation = std::sync::atomic::AtomicU64::new(1);
        let delivered = AtomicBool::new(false);

        run_check_once_with(
            || {
                enabled.store(false, Ordering::Release);
                generation.store(2, Ordering::Release);
                enabled.store(true, Ordering::Release);
                generation.store(3, Ordering::Release);
                Some("v999.0.0".to_string())
            },
            &|_, _| delivered.store(true, Ordering::Release),
            &enabled,
            &generation,
            1,
        );

        assert!(!delivered.load(Ordering::Acquire));
    }

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
