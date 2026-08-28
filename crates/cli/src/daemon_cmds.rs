//! Daemon lifecycle and recovery handlers: run, stop, panic-revert, status, subscribe, autostart.

use crate::args::AutostartAction;
use crate::ipc_client::{
    error_chain_has_command_timeout, error_chain_has_connect_timeout,
    error_chain_has_disconnected_before_response, error_chain_has_pipe_not_found,
    error_chain_indicates_pipe_not_found_timeout, is_non_success_response, open_pipe_with_retry,
    parse_ipc_response_frame, probe_daemon_running, read_ipc_frame_bounded, send_command,
    wait_for_daemon, wait_for_daemon_shutdown, IPC_CONNECT_TIMEOUT, IPC_NOT_FOUND_FAST_FAIL_AFTER,
    SHUTDOWN_CONFIRM_TIMEOUT,
};
use crate::output::print_response;
use anyhow::{Context, Result};
use leopardwm_ipc::{IpcCommand, IpcResponse, MAX_IPC_MESSAGE_SIZE};
use leopardwm_platform_win32::restore_all_windows_moved_offscreen_best_effort;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

fn watchdog_binary_name() -> &'static str {
    if cfg!(windows) {
        "leopardwm-watchdog.exe"
    } else {
        "leopardwm-watchdog"
    }
}

fn daemon_binary_name() -> &'static str {
    if cfg!(windows) {
        "leopardwm.exe"
    } else {
        "leopardwm"
    }
}

fn configured_cargo_target(cwd: &Path) -> Option<String> {
    if let Ok(target) = std::env::var("CARGO_BUILD_TARGET") {
        if !target.trim().is_empty() {
            return Some(target);
        }
    }

    cwd.ancestors().find_map(|dir| {
        let config_path = dir.join(".cargo").join("config.toml");
        let content = std::fs::read_to_string(config_path).ok()?;
        let config = content.parse::<toml::Value>().ok()?;
        config
            .get("build")?
            .get("target")?
            .as_str()
            .filter(|target| !target.trim().is_empty())
            .map(str::to_owned)
    })
}

fn cargo_target_dir(cwd: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => cwd.join(path),
        None => cwd.join("target"),
    }
}

/// Candidate paths Cargo uses for a binary when a configured target triple is
/// present. Do not fall back to an unqualified target directory in that case:
/// it may contain a binary for a different target or an older build.
pub(crate) fn target_binary_candidates(
    target_dir: &Path,
    target_triple: Option<&str>,
    binary_name: &str,
) -> Vec<PathBuf> {
    let profiles = ["debug", "release"];
    match target_triple {
        Some(target) => profiles
            .into_iter()
            .map(|profile| target_dir.join(target).join(profile).join(binary_name))
            .collect(),
        None => profiles
            .into_iter()
            .map(|profile| target_dir.join(profile).join(binary_name))
            .collect(),
    }
}

fn find_binary_in_source_tree(cwd: &Path, binary_name: &str) -> Option<PathBuf> {
    let target_dir = cargo_target_dir(cwd);
    let target_triple = configured_cargo_target(cwd);
    target_binary_candidates(&target_dir, target_triple.as_deref(), binary_name)
        .into_iter()
        .find(|candidate| candidate.exists())
}

fn find_binary(binary_name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let sibling = exe_dir.join(binary_name);
    if sibling.exists() {
        return Some(sibling);
    }

    let cwd = std::env::current_dir().ok()?;
    find_binary_in_source_tree(&cwd, binary_name)
}

pub(crate) fn find_daemon_binary() -> Option<PathBuf> {
    find_binary(daemon_binary_name())
}

fn ensure_daemon_binary() -> Result<PathBuf> {
    if let Some(path) = find_daemon_binary() {
        return Ok(path);
    }

    println!("Daemon binary not found. Building leopardwm-daemon...");
    let status = Command::new("cargo")
        .args(["build", "-p", "leopardwm-daemon"])
        .status()
        .context("Failed to run cargo build for leopardwm-daemon")?;
    if !status.success() {
        anyhow::bail!("cargo build failed for leopardwm-daemon");
    }

    find_daemon_binary().context("Daemon binary still not found after build")
}

#[cfg(windows)]
fn apply_detach_flags(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(windows))]
fn apply_detach_flags(_cmd: &mut Command) {}

fn spawn_daemon(safe_mode: bool) -> Result<u32> {
    let daemon_path = ensure_daemon_binary()?;
    let log_dir = leopardwm_ipc::log_dir();
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    // The daemon writes its own leopardwm-daemon.log; send its stdout to null
    // so we don't open a second handle to the same file. Keep stderr for
    // panics that fire before the tracing subscriber initializes.
    let stderr_path = log_dir.join("leopardwm-daemon.err.log");
    let stderr = File::create(&stderr_path).context("Failed to create daemon stderr log")?;

    let mut cmd = Command::new(daemon_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    if safe_mode {
        cmd.arg("--safe-mode");
    }
    apply_detach_flags(&mut cmd);

    let child = cmd.spawn().context("Failed to start leopardwm daemon")?;
    if safe_mode {
        println!(
            "Started leopardwm daemon in SAFE MODE (PID {}).",
            child.id()
        );
    } else {
        println!("Started leopardwm daemon (PID {}).", child.id());
    }
    println!(
        "Logs: {} / {}",
        log_dir.join("leopardwm-daemon.log").display(),
        stderr_path.display()
    );
    Ok(child.id())
}

pub(crate) fn find_watchdog_binary() -> Option<PathBuf> {
    find_binary(watchdog_binary_name())
}

pub(crate) fn daemon_sibling_for_watchdog(watchdog_path: &Path) -> Option<PathBuf> {
    watchdog_path
        .parent()
        .map(|dir| dir.join(daemon_binary_name()))
}

fn spawn_watchdog(safe_mode: bool) -> Result<u32> {
    let Some(watchdog_path) = find_watchdog_binary() else {
        // Watchdog not bundled (e.g. dev build that didn't `cargo build` it).
        // Fall back to direct daemon spawn rather than failing — preserves
        // backwards-compatible behavior for users who build a partial workspace.
        eprintln!(
            "leopardwm-watchdog binary not found alongside this CLI; \
             falling back to direct daemon spawn (no crash recovery)."
        );
        return spawn_daemon(safe_mode);
    };

    // The watchdog resolves the daemon strictly beside its own executable.
    // Validate that exact sibling instead of accepting a daemon from another
    // target/profile directory that the watchdog could never launch.
    let daemon_sibling = daemon_sibling_for_watchdog(&watchdog_path)
        .context("Watchdog binary has no parent directory")?;
    if !daemon_sibling.is_file() {
        anyhow::bail!(
            "Watchdog at '{}' requires daemon sibling '{}'. Build/package leopardwm and leopardwm-watchdog together for the same target/profile.",
            watchdog_path.display(),
            daemon_sibling.display(),
        );
    }

    let log_dir = leopardwm_ipc::log_dir();
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    let watchdog_log_path = log_dir.join("leopardwm-watchdog.log");
    let stderr_path = log_dir.join("leopardwm-watchdog.err.log");
    let stderr = File::create(&stderr_path).context("Failed to create watchdog stderr log")?;

    let mut cmd = Command::new(watchdog_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    if safe_mode {
        cmd.arg("--safe-mode");
    }
    apply_detach_flags(&mut cmd);

    let child = cmd.spawn().context("Failed to start leopardwm-watchdog")?;
    if safe_mode {
        println!(
            "Started leopardwm-watchdog supervising daemon in SAFE MODE (PID {}).",
            child.id()
        );
    } else {
        println!(
            "Started leopardwm-watchdog supervising daemon (PID {}).",
            child.id()
        );
    }
    println!(
        "Logs: {} / {}",
        watchdog_log_path.display(),
        stderr_path.display()
    );
    Ok(child.id())
}

pub(crate) fn safe_mode_existing_daemon_message() -> &'static str {
    "Daemon is already running. '--safe-mode' only applies when starting a new daemon. Stop it with 'leopardwm-cli stop', then run 'leopardwm-cli run --safe-mode'."
}

pub(crate) fn panic_revert_not_running_message() -> &'static str {
    "Daemon is not running. No automatic cross-process recovery was attempted; ownership cannot be verified. Run `leopardwm-cli emergency-uncloak` only if you explicitly want a global visibility restore."
}

pub(crate) fn panic_revert_unconfirmed_message() -> &'static str {
    "Daemon disconnected before confirming panic-revert completion. No automatic cross-process recovery was attempted because ownership cannot be verified. Run `leopardwm-cli status` (it should fail if daemon exited), and use `leopardwm-cli emergency-uncloak` only if you explicitly want a global visibility restore."
}

pub(crate) fn panic_revert_timeout_recovery_message() -> &'static str {
    "Timed out waiting for panic-revert response. No automatic cross-process recovery was attempted because ownership cannot be verified. Run `leopardwm-cli status` to confirm daemon shutdown."
}

pub(crate) fn stop_timeout_recovery_message() -> &'static str {
    "Timed out waiting for daemon stop confirmation. No automatic cross-process recovery was attempted; run `leopardwm-cli status` to verify shutdown. Use `leopardwm-cli emergency-uncloak` only if you explicitly want a global visibility restore."
}

pub(crate) fn apply_not_running_message() -> &'static str {
    "Daemon is not running. Start it with `leopardwm-cli run` (or `leopardwm-cli run --safe-mode`) before applying layout."
}

pub(crate) fn apply_timeout_recovery_message() -> &'static str {
    "Timed out waiting for `apply` response. No automatic cross-process recovery was attempted because ownership cannot be verified. Run `leopardwm-cli status` before retrying."
}

pub(crate) fn apply_unconfirmed_recovery_message() -> &'static str {
    "Apply completion was not confirmed. No automatic cross-process recovery was attempted because ownership cannot be verified. Run `leopardwm-cli status` before retrying."
}

pub(crate) fn apply_error_response_recovery_message() -> &'static str {
    "Daemon returned a non-success apply response. No automatic cross-process recovery was attempted because ownership cannot be verified. Run `leopardwm-cli status` before retrying."
}

pub(crate) fn stop_error_response_recovery_message() -> &'static str {
    "Daemon returned a non-success stop response. No automatic cross-process recovery was attempted; treat shutdown as unconfirmed and run `leopardwm-cli status`."
}

pub(crate) fn panic_revert_error_response_recovery_message() -> &'static str {
    "Daemon returned a non-success panic-revert response. No automatic cross-process recovery was attempted because ownership cannot be verified. Run `leopardwm-cli status`."
}

pub(crate) fn stop_race_shutdown_message() -> &'static str {
    "Daemon is already stopping or stopped. Run 'leopardwm-cli status' to confirm it no longer responds."
}

pub(crate) fn stop_unconfirmed_message() -> &'static str {
    "Daemon stop was not confirmed. Treat this as unconfirmed shutdown: run 'leopardwm-cli status'. Use `leopardwm-cli emergency-uncloak` only if you explicitly want a global visibility restore."
}

fn run_explicit_global_visibility_restore() -> Result<()> {
    let restored = restore_all_windows_moved_offscreen_best_effort();
    println!("Executed explicit global sentinel sweep: restored={restored}. This opt-in command may move unrelated sentinel-positioned windows; durable recovery remains marker-qualified.");
    Ok(())
}

pub(crate) async fn handle_run(
    no_apply: bool,
    wait_ms: u64,
    safe_mode: bool,
    no_watchdog: bool,
) -> Result<()> {
    let already_running = probe_daemon_running()?;

    if already_running && safe_mode {
        anyhow::bail!(safe_mode_existing_daemon_message());
    }

    if !already_running {
        if no_watchdog {
            spawn_daemon(safe_mode)?;
        } else {
            spawn_watchdog(safe_mode)?;
        }
    } else {
        println!("Daemon already running.");
    }

    wait_for_daemon(Duration::from_millis(wait_ms)).await?;

    if no_apply {
        println!("Daemon is ready.");
        return Ok(());
    }

    let response = send_apply_with_recovery().await?;
    print_response(&response);
    if is_non_success_response(&response) {
        anyhow::bail!(apply_error_response_recovery_message());
    }

    Ok(())
}

async fn send_apply_with_recovery() -> Result<IpcResponse> {
    match send_command(IpcCommand::Apply).await {
        Ok(response) => Ok(response),
        Err(err) if error_chain_indicates_pipe_not_found_timeout(&err) => {
            anyhow::bail!(apply_not_running_message());
        }
        Err(err) if error_chain_has_pipe_not_found(&err) => {
            anyhow::bail!(apply_not_running_message());
        }
        Err(err) if error_chain_has_command_timeout(&err) => {
            anyhow::bail!(apply_timeout_recovery_message());
        }
        Err(err) if error_chain_has_disconnected_before_response(&err) => {
            anyhow::bail!(apply_unconfirmed_recovery_message());
        }
        Err(err) if error_chain_has_connect_timeout(&err) => {
            anyhow::bail!(apply_unconfirmed_recovery_message());
        }
        Err(err) => {
            anyhow::bail!(
                "{}\nUnderlying IPC error: {}",
                apply_unconfirmed_recovery_message(),
                err
            );
        }
    }
}

pub(crate) async fn handle_stop() -> Result<()> {
    let daemon_running = probe_daemon_running()?;

    if !daemon_running {
        println!("Daemon not running.");
        return Ok(());
    }

    let response = match send_command(IpcCommand::Stop).await {
        Ok(response) => response,
        Err(err) if error_chain_has_pipe_not_found(&err) => {
            anyhow::bail!(
                "{}\n{}",
                stop_race_shutdown_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) if error_chain_has_disconnected_before_response(&err) => {
            anyhow::bail!(
                "{}\n{}",
                stop_race_shutdown_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) if error_chain_has_command_timeout(&err) => {
            anyhow::bail!(
                "{}\n{}",
                stop_timeout_recovery_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) if error_chain_has_connect_timeout(&err) => {
            anyhow::bail!(
                "{}\n{}",
                stop_timeout_recovery_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) => {
            anyhow::bail!(
                "{}\nUnderlying IPC error: {}",
                stop_unconfirmed_message(),
                err
            );
        }
    };

    print_response(&response);
    if is_non_success_response(&response) {
        anyhow::bail!(
            "{}\n{}",
            stop_error_response_recovery_message(),
            stop_unconfirmed_message()
        );
    }

    match wait_for_daemon_shutdown(SHUTDOWN_CONFIRM_TIMEOUT).await {
        Ok(true) => {}
        Ok(false) => {
            anyhow::bail!(
                "{}\n{}",
                stop_timeout_recovery_message(),
                stop_unconfirmed_message()
            );
        }
        Err(err) => {
            anyhow::bail!(
                "Failed to confirm daemon shutdown after stop: {}.\n{}",
                err,
                stop_unconfirmed_message()
            );
        }
    }
    Ok(())
}

pub(crate) async fn handle_panic_revert() -> Result<()> {
    let daemon_running = probe_daemon_running()?;
    if !daemon_running {
        println!("{}", panic_revert_not_running_message());
        return Ok(());
    }

    let response = match send_command(IpcCommand::PanicRevert).await {
        Ok(response) => response,
        Err(err) if error_chain_has_pipe_not_found(&err) => {
            anyhow::bail!(panic_revert_unconfirmed_message());
        }
        Err(err) if error_chain_has_disconnected_before_response(&err) => {
            anyhow::bail!(panic_revert_unconfirmed_message());
        }
        Err(err) if error_chain_has_command_timeout(&err) => {
            anyhow::bail!(panic_revert_timeout_recovery_message());
        }
        Err(err) if error_chain_has_connect_timeout(&err) => {
            anyhow::bail!(panic_revert_timeout_recovery_message());
        }
        Err(err) => {
            anyhow::bail!(
                "{}\nUnderlying IPC error: {}",
                panic_revert_unconfirmed_message(),
                err
            );
        }
    };

    print_response(&response);
    if is_non_success_response(&response) {
        anyhow::bail!(
            "{}\n{}",
            panic_revert_error_response_recovery_message(),
            panic_revert_unconfirmed_message()
        );
    }

    match wait_for_daemon_shutdown(SHUTDOWN_CONFIRM_TIMEOUT).await {
        Ok(true) => {}
        Ok(false) => {
            anyhow::bail!(panic_revert_unconfirmed_message());
        }
        Err(_) => {
            anyhow::bail!(panic_revert_unconfirmed_message());
        }
    }
    Ok(())
}

pub(crate) async fn handle_status() -> Result<()> {
    if !probe_daemon_running()? {
        anyhow::bail!("Daemon is not running. Start it with `leopardwm-cli run`.");
    }

    let response = send_command(IpcCommand::QueryStatus)
        .await
        .context("Daemon appears reachable but did not return status")?;
    print_response(&response);
    if is_non_success_response(&response) {
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) fn handle_emergency_uncloak() -> Result<()> {
    run_explicit_global_visibility_restore()
        .context("Failed to execute explicit global visibility restore")
}

/// Subscribe to daemon events and stream them as newline-delimited JSON
/// to stdout. After the daemon answers `Subscribed`, the connection
/// stays open and every subsequent line is an `IpcEvent` frame. This is
/// the documented client-state-machine mode-switch — the response parser
/// is `IpcResponse` for the first frame, `IpcEvent` for all subsequent
/// frames.
pub(crate) async fn handle_subscribe(events: Option<Vec<String>>) -> Result<()> {
    use leopardwm_ipc::EventKind;
    use std::collections::BTreeSet;

    // Parse the requested kinds. Empty/missing means "all".
    let requested: BTreeSet<EventKind> = match events {
        None => BTreeSet::new(), // server interprets empty as all
        Some(list) => {
            let mut out = BTreeSet::new();
            for raw in list {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let kind = match trimmed {
                    "workspace" => EventKind::Workspace,
                    "focused_window" => EventKind::FocusedWindow,
                    "layout" => EventKind::Layout,
                    "config" => EventKind::Config,
                    "heartbeat" => EventKind::Heartbeat,
                    other => anyhow::bail!(
                        "Unknown event kind '{}'. Valid: workspace, focused_window, \
                         layout, config, heartbeat",
                        other
                    ),
                };
                out.insert(kind);
            }
            out
        }
    };

    if !probe_daemon_running()? {
        anyhow::bail!("Daemon is not running. Start it with `leopardwm-cli run`.");
    }

    let client =
        open_pipe_with_retry(IPC_CONNECT_TIMEOUT, Some(IPC_NOT_FOUND_FAST_FAIL_AFTER)).await?;
    let (reader, mut writer) = tokio::io::split(client);
    let cmd = IpcCommand::Subscribe { events: requested };
    let cmd_json = serde_json::to_string(&cmd)? + "\n";
    writer
        .write_all(cmd_json.as_bytes())
        .await
        .context("Failed to send Subscribe command")?;

    // The subscription is long-lived, so bound each individual frame rather
    // than wrapping the reader in a lifetime-sized `take`. This rejects a
    // malicious ack/event without preventing many valid frames from flowing.
    let mut buf = tokio::io::BufReader::new(reader);
    let ack_frame = read_ipc_frame_bounded(&mut buf, MAX_IPC_MESSAGE_SIZE)
        .await?
        .context("Daemon disconnected before sending Subscribed ack")?;
    let ack = parse_ipc_response_frame(&ack_frame, MAX_IPC_MESSAGE_SIZE)
        .context("Failed to parse Subscribed ack")?;
    match ack {
        IpcResponse::Subscribed { .. } => {}
        IpcResponse::Error { message } => anyhow::bail!("Subscribe rejected: {}", message),
        other => anyhow::bail!("Unexpected response to Subscribe: {:?}", other),
    }

    // Each frame is a single line of JSON; raw passthrough to stdout so
    // users can pipe into `jq` etc.
    let mut stdout = tokio::io::stdout();
    loop {
        let Some(event_line) = read_ipc_frame_bounded(&mut buf, MAX_IPC_MESSAGE_SIZE).await? else {
            // Daemon closed the pipe (shutdown, restart, etc.)
            break;
        };
        // Validate as IpcEvent so we surface daemon-side bugs noisily,
        // but pass the raw bytes through to stdout to preserve any
        // formatting subtleties for jq consumers.
        if let Err(e) = serde_json::from_slice::<leopardwm_ipc::IpcEvent>(&event_line) {
            eprintln!(
                "Warning: failed to parse event frame ({}): {}",
                e,
                String::from_utf8_lossy(&event_line).trim_end()
            );
            continue;
        }
        stdout
            .write_all(&event_line)
            .await
            .context("Failed to write event to stdout")?;
        stdout.flush().await.ok();
    }
    Ok(())
}

/// Handle the autostart command (enable/disable Registry run key).
pub(crate) fn handle_autostart(action: AutostartAction) -> Result<()> {
    use leopardwm_platform_win32::autostart;

    match action {
        AutostartAction::Enable => {
            let daemon_path = ensure_daemon_binary()?;
            let autostart_path = autostart::preferred_autostart_executable(&daemon_path);
            autostart::enable_autostart(&autostart_path)?;
            println!("Auto-start enabled: \"{}\"", autostart_path.display());
        }
        AutostartAction::Disable => {
            let was_enabled = autostart::get_autostart().unwrap_or(false);
            autostart::disable_autostart()?;
            if was_enabled {
                println!("Auto-start disabled.");
            } else {
                println!("Auto-start was not enabled.");
            }
        }
    }

    Ok(())
}
