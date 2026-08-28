//! Named-pipe IPC client: connect/retry, send/receive, and error classification.

use anyhow::{Context, Result};
use leopardwm_ipc::{pipe_name_candidates, IpcCommand, IpcResponse, MAX_IPC_MESSAGE_SIZE};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::time::{sleep, timeout};

/// Timeout budget for establishing an IPC connection to the daemon.
pub(crate) const IPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Extended connect timeout budget for recovery commands that can race shutdown/startup.
pub(crate) const IPC_RECOVERY_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
/// Default timeout budget for daemon responses after request send.
pub(crate) const IPC_DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Extended timeout for recovery commands that can race shutdown.
pub(crate) const IPC_RECOVERY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for daemon process/pipe teardown after stop-style commands.
pub(crate) const SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll cadence for daemon shutdown confirmation.
pub(crate) const SHUTDOWN_CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(150);
/// Fast-fail threshold for pure "pipe not found" states on command sends.
pub(crate) const IPC_NOT_FOUND_FAST_FAIL_AFTER: Duration = Duration::from_millis(800);
pub(crate) const PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE: &str =
    "Daemon disconnected before sending a response";

pub(crate) fn is_non_success_response(response: &IpcResponse) -> bool {
    matches!(response, IpcResponse::Error { .. } | IpcResponse::Unknown)
}

pub(crate) fn command_connect_timeout(cmd: &IpcCommand) -> Duration {
    match cmd {
        IpcCommand::Stop | IpcCommand::PanicRevert => IPC_RECOVERY_CONNECT_TIMEOUT,
        _ => IPC_CONNECT_TIMEOUT,
    }
}

pub(crate) fn command_response_timeout(cmd: &IpcCommand) -> Duration {
    match cmd {
        IpcCommand::Stop | IpcCommand::PanicRevert => IPC_RECOVERY_RESPONSE_TIMEOUT,
        _ => IPC_DEFAULT_RESPONSE_TIMEOUT,
    }
}

pub(crate) async fn wait_for_daemon(timeout: Duration) -> Result<()> {
    let _ = open_pipe_with_retry(timeout, None).await?;
    Ok(())
}

pub(crate) async fn wait_for_daemon_shutdown(timeout: Duration) -> Result<bool> {
    let start = Instant::now();
    loop {
        if !probe_daemon_running()? {
            return Ok(true);
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        sleep(SHUTDOWN_CONFIRM_POLL_INTERVAL).await;
    }
}

fn is_pipe_busy(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(231)
}

fn is_pipe_not_found(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(2)
}

pub(crate) fn classify_pipe_probe_error(err: &std::io::Error) -> Option<bool> {
    if is_pipe_busy(err) {
        Some(true)
    } else if is_pipe_not_found(err) {
        Some(false)
    } else {
        None
    }
}

pub(crate) fn pipe_connect_retry_timeout_message(
    timeout: Duration,
    saw_busy: bool,
    saw_not_found: bool,
) -> String {
    let timeout_ms = timeout.as_millis();
    match (saw_busy, saw_not_found) {
        (true, false) => format!(
            "Timed out after {timeout_ms}ms connecting to daemon IPC pipe: the pipe remained busy (daemon may be starting or shutting down). Run `leopardwm-cli status` and retry once the daemon is stable."
        ),
        (false, true) => format!(
            "Timed out after {timeout_ms}ms connecting to daemon IPC pipe: the pipe was not found (daemon is likely not running). Start it with `leopardwm-cli run`."
        ),
        (true, true) => format!(
            "Timed out after {timeout_ms}ms connecting to daemon IPC pipe: observed both busy and not-found states (daemon may be transitioning startup/shutdown). Run `leopardwm-cli status`, then retry."
        ),
        (false, false) => format!(
            "Timed out after {timeout_ms}ms connecting to daemon IPC pipe. Run `leopardwm-cli status` to check daemon health."
        ),
    }
}

pub(crate) fn pipe_connect_not_found_fast_fail_message(cutoff: Duration) -> String {
    format!(
        "Daemon IPC pipe was not found after {}ms. Daemon is likely not running. Start it with `leopardwm-cli run`.",
        cutoff.as_millis()
    )
}

pub(crate) fn error_chain_has_pipe_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(is_pipe_not_found)
            .unwrap_or(false)
    })
}

pub(crate) fn error_chain_has_disconnected_before_response(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string() == PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE)
}

pub(crate) fn error_chain_has_command_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
            || cause
                .to_string()
                .contains("Timed out waiting for daemon response")
    })
}

pub(crate) fn error_chain_indicates_pipe_not_found_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("pipe was not found (daemon is likely not running)")
            || text.contains("IPC pipe was not found")
    })
}

pub(crate) fn error_chain_has_connect_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("Timed out after") && text.contains("connecting to daemon IPC pipe")
    })
}

pub(crate) fn probe_daemon_running() -> Result<bool> {
    for pipe_name in pipe_name_candidates() {
        match ClientOptions::new().open(&pipe_name) {
            Ok(_) => return Ok(true),
            Err(e) => match classify_pipe_probe_error(&e) {
                Some(true) => return Ok(true),
                Some(false) => continue,
                None => {
                    return Err(e).context(format!(
                        "Failed to check daemon state via IPC pipe '{}'",
                        pipe_name
                    ))
                }
            },
        }
    }
    Ok(false)
}

/// Whether trying the legacy endpoint preserves daemon identity after a
/// preferred-endpoint failure. Once the preferred endpoint has ever reported
/// busy, it is known to exist and a legacy endpoint must never be selected for
/// this connection attempt.
pub(crate) fn legacy_fallback_is_safe(
    preferred_endpoint_seen: bool,
    preferred_error: &std::io::Error,
) -> bool {
    !preferred_endpoint_seen && is_pipe_not_found(preferred_error)
}

pub(crate) async fn open_pipe_with_retry(
    timeout: Duration,
    not_found_fast_fail_after: Option<Duration>,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let start = Instant::now();
    let mut saw_busy = false;
    let mut saw_not_found = false;
    let pipe_names = pipe_name_candidates();
    let preferred_pipe = pipe_names
        .first()
        .expect("pipe_name_candidates always returns a preferred endpoint");
    let legacy_pipe = pipe_names.get(1);
    let mut preferred_endpoint_seen = false;

    loop {
        let preferred_error = match ClientOptions::new().open(preferred_pipe) {
            Ok(client) => return Ok(client),
            Err(error) if is_pipe_busy(&error) => {
                // A busy preferred pipe proves that this endpoint exists. Do
                // not probe the legacy name now or on a later retry: it could
                // address a stale daemon generation with a different identity.
                preferred_endpoint_seen = true;
                saw_busy = true;
                None
            }
            Err(error) if is_pipe_not_found(&error) => {
                saw_not_found = true;
                Some(error)
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "Failed to connect to preferred daemon IPC pipe '{}'",
                    preferred_pipe
                )));
            }
        };

        if let (Some(legacy_pipe), Some(preferred_error)) = (legacy_pipe, preferred_error) {
            if legacy_fallback_is_safe(preferred_endpoint_seen, &preferred_error) {
                match ClientOptions::new().open(legacy_pipe) {
                    Ok(client) => return Ok(client),
                    Err(error) if is_pipe_busy(&error) => saw_busy = true,
                    Err(error) if is_pipe_not_found(&error) => saw_not_found = true,
                    Err(error) => {
                        return Err(anyhow::Error::new(error).context(format!(
                            "Failed to connect to legacy daemon IPC pipe '{}'",
                            legacy_pipe
                        )));
                    }
                }
            }
        }

        if let Some(cutoff) = not_found_fast_fail_after {
            if saw_not_found && !saw_busy && start.elapsed() >= cutoff {
                return Err(anyhow::anyhow!(pipe_connect_not_found_fast_fail_message(
                    cutoff
                )));
            }
        }
        if start.elapsed() >= timeout {
            return Err(anyhow::anyhow!(pipe_connect_retry_timeout_message(
                timeout,
                saw_busy,
                saw_not_found,
            )));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) fn parse_ipc_response_line(raw: &str) -> Result<IpcResponse> {
    serde_json::from_str(raw.trim()).context("Failed to parse response")
}

pub(crate) fn parse_ipc_response_frame(frame: &[u8], max_bytes: usize) -> Result<IpcResponse> {
    if frame.len() > max_bytes {
        anyhow::bail!(
            "Daemon response exceeded {} bytes; refusing to parse oversized payload",
            max_bytes
        );
    }
    if !frame.ends_with(b"\n") {
        anyhow::bail!(
            "Daemon response exceeded {} bytes or was not newline-terminated",
            max_bytes
        );
    }
    let raw =
        std::str::from_utf8(frame).context("Daemon response contained invalid UTF-8 bytes")?;
    parse_ipc_response_line(raw)
}

/// Read one newline-delimited IPC frame without imposing a lifetime byte cap
/// on a long-lived `BufReader`. The extra byte distinguishes an exact-limit
/// frame from an oversized or unterminated one.
pub(crate) async fn read_ipc_frame_bounded<R>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let limit = max_bytes
        .checked_add(1)
        .context("IPC frame size limit overflow")?;
    let mut frame = Vec::new();

    loop {
        let buffer = reader
            .fill_buf()
            .await
            .context("Failed to read IPC frame")?;
        if buffer.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            anyhow::bail!(
                "IPC frame exceeded {} bytes or was not newline-terminated",
                max_bytes
            );
        }

        let available = (limit - frame.len()).min(buffer.len());
        let newline = buffer[..available].iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available, |position| position + 1);
        frame.extend_from_slice(&buffer[..consumed]);
        reader.consume(consumed);

        if newline.is_some() {
            if frame.len() > max_bytes {
                anyhow::bail!(
                    "IPC frame exceeded {} bytes or was not newline-terminated",
                    max_bytes
                );
            }
            return Ok(Some(frame));
        }
        if frame.len() == limit {
            anyhow::bail!(
                "IPC frame exceeded {} bytes or was not newline-terminated",
                max_bytes
            );
        }
    }
}

async fn read_ipc_response_bounded<R>(reader: R, max_bytes: usize) -> Result<IpcResponse>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let frame = read_ipc_frame_bounded(&mut reader, max_bytes)
        .await?
        .context(PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE)?;
    parse_ipc_response_frame(&frame, max_bytes)
}

/// Send a command to the daemon and return the response (with timeout).
pub(crate) async fn send_command(cmd: IpcCommand) -> Result<IpcResponse> {
    let connect_timeout = command_connect_timeout(&cmd);
    let response_timeout = command_response_timeout(&cmd);
    send_command_with_timeouts(cmd, connect_timeout, response_timeout).await
}

/// Send command with explicit connect/response timeout budgets.
async fn send_command_with_timeouts(
    cmd: IpcCommand,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<IpcResponse> {
    send_command_inner(cmd, connect_timeout, response_timeout).await
}

/// Inner implementation with separate connect/response timeout control.
async fn send_command_inner(
    cmd: IpcCommand,
    connect_timeout: Duration,
    response_timeout: Duration,
) -> Result<IpcResponse> {
    let client = open_pipe_with_retry(connect_timeout, Some(IPC_NOT_FOUND_FAST_FAIL_AFTER)).await?;

    let (reader, mut writer) = tokio::io::split(client);

    let json = serde_json::to_string(&cmd)? + "\n";
    writer
        .write_all(json.as_bytes())
        .await
        .context("Failed to send command")?;

    timeout(
        response_timeout,
        read_ipc_response_bounded(reader, MAX_IPC_MESSAGE_SIZE),
    )
    .await
    .with_context(|| {
        format!(
            "Timed out waiting for daemon response after {}ms",
            response_timeout.as_millis()
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn reader_with_bytes(bytes: Vec<u8>) -> BufReader<tokio::io::DuplexStream> {
        let (mut writer, reader) = tokio::io::duplex(bytes.len() + 1);
        writer.write_all(&bytes).await.unwrap();
        drop(writer);
        BufReader::new(reader)
    }

    #[test]
    fn busy_preferred_pipe_never_allows_legacy_fallback() {
        let busy = std::io::Error::from_raw_os_error(231);
        let not_found = std::io::Error::from_raw_os_error(2);
        assert!(!legacy_fallback_is_safe(false, &busy));
        assert!(!legacy_fallback_is_safe(true, &not_found));
        assert!(legacy_fallback_is_safe(false, &not_found));
    }

    #[tokio::test]
    async fn bounded_reader_rejects_oversized_subscribe_ack() {
        let ack = format!(
            "{{\"status\":\"subscribed\",\"events\":[],\"padding\":\"{}\"}}\n",
            "x".repeat(MAX_IPC_MESSAGE_SIZE)
        );
        let mut reader = reader_with_bytes(ack.into_bytes()).await;
        let error = read_ipc_frame_bounded(&mut reader, MAX_IPC_MESSAGE_SIZE)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeded"));
    }

    #[tokio::test]
    async fn bounded_reader_rejects_oversized_subscribe_event() {
        let event = format!(
            "{{\"type\":\"heartbeat\",\"uptime_seconds\":0,\"padding\":\"{}\"}}\n",
            "x".repeat(MAX_IPC_MESSAGE_SIZE)
        );
        let mut reader = reader_with_bytes(event.into_bytes()).await;
        let error = read_ipc_frame_bounded(&mut reader, MAX_IPC_MESSAGE_SIZE)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeded"));
    }

    #[tokio::test]
    async fn bounded_reader_accepts_an_exact_limit_frame() {
        let empty = IpcResponse::Error {
            message: String::new(),
        };
        let overhead = serde_json::to_string(&empty).unwrap().len() + 1;
        let frame = format!(
            "{}\n",
            serde_json::to_string(&IpcResponse::Error {
                message: "x".repeat(MAX_IPC_MESSAGE_SIZE - overhead),
            })
            .unwrap()
        );
        assert_eq!(frame.len(), MAX_IPC_MESSAGE_SIZE);

        let mut reader = reader_with_bytes(frame.into_bytes()).await;
        let frame = read_ipc_frame_bounded(&mut reader, MAX_IPC_MESSAGE_SIZE)
            .await
            .unwrap()
            .expect("exact-limit frame is present");
        assert!(matches!(
            parse_ipc_response_frame(&frame, MAX_IPC_MESSAGE_SIZE).unwrap(),
            IpcResponse::Error { .. }
        ));
    }

    #[tokio::test]
    async fn bounded_reader_allows_many_valid_frames_over_the_lifetime_limit() {
        let event = serde_json::to_vec(&leopardwm_ipc::IpcEvent::Heartbeat {
            uptime_seconds: 1,
        })
        .unwrap();
        let mut frame = event;
        frame.push(b'\n');
        let count = MAX_IPC_MESSAGE_SIZE / frame.len() + 2;
        let mut bytes = Vec::with_capacity(frame.len() * count);
        for _ in 0..count {
            bytes.extend_from_slice(&frame);
        }
        assert!(bytes.len() > MAX_IPC_MESSAGE_SIZE);

        let mut reader = reader_with_bytes(bytes).await;
        for _ in 0..count {
            let received = read_ipc_frame_bounded(&mut reader, MAX_IPC_MESSAGE_SIZE)
                .await
                .unwrap()
                .expect("each valid frame is available");
            assert_eq!(received, frame);
        }
        assert!(read_ipc_frame_bounded(&mut reader, MAX_IPC_MESSAGE_SIZE)
            .await
            .unwrap()
            .is_none());
    }
}
