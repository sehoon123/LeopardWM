//! Named-pipe IPC server and client handling.

use super::DaemonEvent;
use crate::events::SubscribeStartup;
use anyhow::{Context, Result};
use leopardwm_ipc::{
    preferred_pipe_name, EventKind, IpcCommand, IpcEvent, IpcResponse, MAX_IPC_MESSAGE_SIZE,
    PIPE_NAME,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
use tokio::sync::{broadcast, mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, warn};

/// IPC read timeout - clients must send within this period.
pub(crate) const IPC_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// IPC responder timeout - daemon must answer within this period.
pub(crate) const IPC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Heartbeat interval for stream-mode subscribers. Subscribers receive a
/// `IpcEvent::Heartbeat` after this much silence so they can detect a
/// dead daemon pipe by missing keepalives.
pub(crate) const STREAM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Poll interval for cooperative timed thread joins.
const JOIN_WITH_TIMEOUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Maximum number of ordinary IPC handlers allowed at once.
pub(crate) const MAX_IPC_COMMAND_HANDLERS: usize = 32;
/// Maximum number of long-lived IPC event streams allowed at once.
pub(crate) const MAX_IPC_SUBSCRIBERS: usize = 32;

/// The atomically-created first pipe instance that establishes daemon
/// ownership before any window-management initialization begins.
///
/// The first server is retained until the regular accept loop begins, so a
/// second daemon cannot pass a probe race and initialize before discovering
/// that it lost ownership. The pipe security descriptor is deliberately held
/// for the process lifetime because Tokio's raw security-attributes API only
/// borrows it while creating later pipe instances.
pub(crate) struct IpcServerOwnership {
    pipe_name: String,
    first_server: NamedPipeServer,
    legacy: Option<(String, NamedPipeServer)>,
    pipe_security_ptr: Option<usize>,
}

fn create_pipe_server(
    pipe_name: &str,
    first_pipe_instance: bool,
    pipe_security_ptr: Option<usize>,
) -> std::io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first_pipe_instance)
        .pipe_mode(PipeMode::Byte);
    match pipe_security_ptr {
        Some(ptr) => unsafe {
            options.create_with_security_attributes_raw(pipe_name, ptr as *mut std::ffi::c_void)
        },
        None => options.create(pipe_name),
    }
}

/// Atomically acquire the daemon's IPC endpoint before daemon initialization.
///
/// `first_pipe_instance(true)` is the ownership primitive: creation succeeds
/// for exactly one server generation. Callers must retain the returned value
/// until passing it to [`run_ipc_server_with_ownership`]; dropping it releases
/// ownership again.
pub(crate) fn acquire_ipc_server_ownership() -> Result<IpcServerOwnership> {
    let pipe_name = preferred_pipe_name();
    let pipe_security = leopardwm_platform_win32::ipc_security::PipeSecurityAttributes::new();
    if pipe_security.is_none() {
        warn!("Could not build IPC pipe security attributes; using defaults (a non-elevated client may not reach an elevated daemon)");
    }
    let pipe_security_ptr = pipe_security
        .as_ref()
        .map(|security| security.as_ptr() as usize);

    let first_server = create_pipe_server(&pipe_name, true, pipe_security_ptr).with_context(|| {
        format!(
            "Failed to atomically acquire IPC ownership for '{}'; another daemon may already own it",
            pipe_name
        )
    })?;
    let legacy = (pipe_name != PIPE_NAME)
        .then(|| {
            create_pipe_server(PIPE_NAME, true, pipe_security_ptr)
                .map(|server| (PIPE_NAME.to_string(), server))
                .with_context(|| {
                    format!(
                        "Failed to acquire legacy IPC ownership for '{}'; an older daemon may still be running",
                        PIPE_NAME
                    )
                })
        })
        .transpose()?;

    // `create_with_security_attributes_raw` borrows this descriptor. Later
    // pipe instances need the same descriptor, so retain it for the daemon
    // process lifetime rather than letting the backing allocation disappear.
    if let Some(security) = pipe_security {
        std::mem::forget(security);
    }

    Ok(IpcServerOwnership {
        pipe_name,
        first_server,
        legacy,
        pipe_security_ptr,
    })
}

pub(crate) fn response_for_ipc_wait_failure(_cmd: &IpcCommand, timed_out: bool) -> IpcResponse {
    if timed_out {
        IpcResponse::error("Timed out waiting for daemon response")
    } else {
        IpcResponse::error("Failed to get response from daemon")
    }
}

/// Run the IPC server after startup has atomically acquired its first pipe
/// instance with [`acquire_ipc_server_ownership`].
pub(crate) async fn run_ipc_server_with_ownership(
    event_tx: mpsc::Sender<DaemonEvent>,
    ownership: IpcServerOwnership,
) {
    let IpcServerOwnership {
        pipe_name,
        first_server,
        legacy,
        pipe_security_ptr,
    } = ownership;
    let command_limit = Arc::new(Semaphore::new(MAX_IPC_COMMAND_HANDLERS));
    let subscriber_limit = Arc::new(Semaphore::new(MAX_IPC_SUBSCRIBERS));
    let _legacy_task = legacy.map(|(legacy_name, legacy_server)| {
        tokio::spawn(run_ipc_accept_loop(
            event_tx.clone(),
            legacy_name,
            legacy_server,
            pipe_security_ptr,
            command_limit.clone(),
            subscriber_limit.clone(),
        ))
    });
    run_ipc_accept_loop(
        event_tx,
        pipe_name,
        first_server,
        pipe_security_ptr,
        command_limit,
        subscriber_limit,
    )
    .await;
}

async fn run_ipc_accept_loop(
    event_tx: mpsc::Sender<DaemonEvent>,
    pipe_name: String,
    first_server: NamedPipeServer,
    pipe_security_ptr: Option<usize>,
    command_limit: Arc<Semaphore>,
    subscriber_limit: Arc<Semaphore>,
) {
    let mut first_server = Some(first_server);

    loop {
        let permit = match command_limit.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                warn!("IPC command limiter closed while accepting client");
                return;
            }
        };

        let server = match first_server.take() {
            Some(server) => server,
            None => match create_pipe_server(&pipe_name, false, pipe_security_ptr) {
                Ok(server) => server,
                Err(error) => {
                    error!(%error, "Failed to create additional named-pipe server instance");
                    drop(permit);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            },
        };

        debug!("Waiting for client connection on {}", pipe_name);

        if let Err(error) = server.connect().await {
            error!(%error, "Failed to accept client connection");
            drop(permit);
            continue;
        }

        debug!("Client connected");

        let event_tx = event_tx.clone();
        let subscriber_limit = subscriber_limit.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_client(server, event_tx, permit, subscriber_limit).await {
                warn!(%error, "Client handler error");
            }
        });
    }
}

/// Serialize an `IpcResponse` to a newline-terminated frame and write it.
async fn write_response_frame<W>(writer: &mut W, response: &IpcResponse) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut response_json = match serde_json::to_string(response) {
        Ok(json) => json + "\n",
        Err(e) => {
            warn!("Failed to serialize IPC response: {}", e);
            "{\"status\":\"error\",\"message\":\"Internal serialization error\"}\n".to_string()
        }
    };

    if response_json.len() > MAX_IPC_MESSAGE_SIZE {
        warn!(
            "IPC response exceeded {} bytes; returning bounded error response instead",
            MAX_IPC_MESSAGE_SIZE
        );
        response_json = serde_json::to_string(&IpcResponse::error(
            "IPC response exceeded maximum size; narrow query scope and retry",
        ))
        .unwrap_or_else(|_| {
            "{\"status\":\"error\",\"message\":\"Internal serialization error\"}".to_string()
        });
        response_json.push('\n');
    }

    writer.write_all(response_json.as_bytes()).await?;
    Ok(())
}

/// Serialize an `IpcEvent` to a newline-terminated frame and write it.
async fn write_event_frame<W>(writer: &mut W, event: &IpcEvent) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut json = serde_json::to_string(event)? + "\n";
    if json.len() > MAX_IPC_MESSAGE_SIZE {
        // Oversize events shouldn't happen given the small variant
        // payloads, but if a future LayoutChanged ever blows the cap,
        // surface a Lagged-style hint instead of a corrupt frame.
        json = serde_json::to_string(&IpcEvent::Lagged { skipped: 0 })? + "\n";
    }
    writer.write_all(json.as_bytes()).await?;
    Ok(())
}

/// Whether an event belongs on a subscriber's stream. `Lagged` remains an
/// unconditional control frame so a client knows its snapshot is stale.
fn stream_event_matches_subscription(requested: &BTreeSet<EventKind>, event: &IpcEvent) -> bool {
    matches!(event, IpcEvent::Lagged { .. }) || requested.contains(&event.kind())
}

/// Handle a single client connection.
async fn handle_client(
    pipe: NamedPipeServer,
    event_tx: mpsc::Sender<DaemonEvent>,
    permit: OwnedSemaphorePermit,
    subscriber_limit: Arc<Semaphore>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(pipe);
    let limited_reader = reader.take(MAX_IPC_MESSAGE_SIZE as u64);
    let mut reader = BufReader::new(limited_reader);
    let mut line = String::new();

    // Read command (single line of JSON) with timeout and size bound
    let read_result = tokio::time::timeout(IPC_READ_TIMEOUT, reader.read_line(&mut line)).await;
    let bytes_read = match read_result {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            // Timeout: client did not send in time, silently close
            return Ok(());
        }
    };
    if bytes_read == 0 {
        return Ok(()); // Client disconnected
    }

    if !line.ends_with('\n') {
        let msg = if bytes_read >= MAX_IPC_MESSAGE_SIZE {
            "Command too large or missing newline terminator"
        } else {
            "IPC command must be newline-terminated"
        };
        write_response_frame(&mut writer, &IpcResponse::error(msg)).await?;
        return Ok(());
    }

    let line = line.trim_end_matches(['\r', '\n']);
    debug!("Received command: {}", line);

    // Parse the command
    let cmd: IpcCommand = match serde_json::from_str(line) {
        Ok(cmd) => cmd,
        Err(e) => {
            let response = IpcResponse::error(format!("Invalid command: {}", e));
            write_response_frame(&mut writer, &response).await?;
            return Ok(());
        }
    };

    // Subscribe is routed through a dedicated DaemonEvent variant whose
    // responder carries (ack, snapshot, broadcast::Receiver). The main
    // daemon loop processes it under the AppState mutex so the receiver
    // creation + snapshot read happen in one atomic critical section —
    // no event between handoff and receiver-creation can be lost.
    if let IpcCommand::Subscribe { events } = cmd {
        return handle_subscribe(writer, event_tx, events, permit, subscriber_limit).await;
    }

    // Everything else: existing oneshot path through the daemon main loop.
    handle_command_oneshot(writer, event_tx, cmd, permit).await
}

/// Existing single-command request/response path.
async fn handle_command_oneshot<W>(
    mut writer: W,
    event_tx: mpsc::Sender<DaemonEvent>,
    cmd: IpcCommand,
    permit: OwnedSemaphorePermit,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let _permit = permit; // released when this task returns

    let (resp_tx, resp_rx) = oneshot::channel();
    let response_cmd = cmd.clone();

    if event_tx
        .send(DaemonEvent::IpcCommand {
            cmd,
            responder: resp_tx,
        })
        .await
        .is_err()
    {
        let response = IpcResponse::error("Daemon is shutting down");
        write_response_frame(&mut writer, &response).await?;
        return Ok(());
    }

    let response = match tokio::time::timeout(IPC_RESPONSE_TIMEOUT, resp_rx).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => response_for_ipc_wait_failure(&response_cmd, false),
        Err(_) => response_for_ipc_wait_failure(&response_cmd, true),
    };

    write_response_frame(&mut writer, &response).await?;
    Ok(())
}

/// Stream-mode entry: route Subscribe through a dedicated DaemonEvent
/// variant so the daemon main loop can subscribe + snapshot atomically
/// under the AppState mutex, then drive an event loop that writes
/// `IpcEvent` frames until the pipe closes or the broadcaster is dropped.
async fn handle_subscribe<W>(
    mut writer: W,
    event_tx: mpsc::Sender<DaemonEvent>,
    requested_raw: BTreeSet<EventKind>,
    permit: OwnedSemaphorePermit,
    subscriber_limit: Arc<Semaphore>,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    // Reserve a distinct stream permit before asking the main loop to create
    // a broadcast receiver. This keeps both pipe handles and stream tasks
    // bounded while immediately releasing ordinary command capacity.
    let _stream_permit = match subscriber_limit.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            write_response_frame(
                &mut writer,
                &IpcResponse::error("Too many active IPC subscribers; retry later"),
            )
            .await?;
            return Ok(());
        }
    };
    drop(permit);

    // Empty-set means "all kinds" so users can do `Subscribe { events: {} }`
    // as a "give me everything" shortcut.
    let requested = if requested_raw.is_empty() {
        EventKind::all()
    } else {
        requested_raw
    };

    // Send Subscribe to the daemon main loop, which builds the bundle
    // (ack + snapshot + broadcast::Receiver) under the AppState mutex.
    let (resp_tx, resp_rx) = oneshot::channel();
    if event_tx
        .send(DaemonEvent::IpcSubscribe {
            events: requested.clone(),
            responder: resp_tx,
        })
        .await
        .is_err()
    {
        let response = IpcResponse::error("Daemon is shutting down");
        let _ = write_response_frame(&mut writer, &response).await;
        return Ok(());
    }

    let startup = match tokio::time::timeout(IPC_RESPONSE_TIMEOUT, resp_rx).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            let _ = write_response_frame(
                &mut writer,
                &IpcResponse::error("Failed to get subscribe response from daemon"),
            )
            .await;
            return Ok(());
        }
        Err(_) => {
            let _ = write_response_frame(
                &mut writer,
                &IpcResponse::error("Timed out waiting for subscribe response"),
            )
            .await;
            return Ok(());
        }
    };

    let SubscribeStartup {
        ack,
        snapshot,
        mut receiver,
    } = startup;

    // Write ack
    if write_response_frame(&mut writer, &ack).await.is_err() {
        return Ok(());
    }

    // Write snapshot frames
    for ev in &snapshot {
        if write_event_frame(&mut writer, ev).await.is_err() {
            return Ok(());
        }
    }

    // Stream loop: events + heartbeat. The heartbeat's uptime field is
    // per-subscriber connection time (since we entered stream mode),
    // NOT the daemon process uptime — that's intentional, callers can
    // detect "did we just reconnect" vs "are we still on the original
    // connection". Computing daemon-wide uptime would require an extra
    // mutex acquisition per heartbeat for negligible signal.
    let stream_started = std::time::Instant::now();
    let mut heartbeat = tokio::time::interval(STREAM_HEARTBEAT_INTERVAL);
    // Skip the immediate first tick so we don't send a heartbeat right
    // after the snapshot.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            recv = receiver.recv() => match recv {
                Ok(ev) => {
                    if !stream_event_matches_subscription(&requested, &ev) {
                        continue;
                    }
                    if write_event_frame(&mut writer, &ev).await.is_err() {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let lagged = IpcEvent::Lagged { skipped };
                    if write_event_frame(&mut writer, &lagged).await.is_err() {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = heartbeat.tick() => {
                let uptime = stream_started.elapsed().as_secs();
                let hb = IpcEvent::Heartbeat { uptime_seconds: uptime };
                if stream_event_matches_subscription(&requested, &hb)
                    && write_event_frame(&mut writer, &hb).await.is_err()
                {
                    return Ok(());
                }
            }
        }
    }
}

/// A forwarding thread plus an explicit stop edge. Source owners live in UI and
/// hook objects that intentionally outlive the event loop, so sender drop alone
/// is not a valid shutdown protocol.
pub(crate) struct ForwardingThreadHandle {
    name: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ForwardingThreadHandle {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn request_stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn join_with_timeout(&mut self, timeout: Duration) -> bool {
        self.request_stop();
        join_with_timeout(&mut self.thread, timeout)
    }

    #[cfg(test)]
    pub(crate) fn join(mut self) -> std::thread::Result<()> {
        self.thread
            .take()
            .expect("forwarding thread handle must exist")
            .join()
    }
}

/// Spawn a named forwarding thread that receives events from a sync channel
/// and forwards them to a Tokio channel. A short timed receive lets an explicit
/// shutdown request terminate the thread even while a source sender remains
/// owned by a long-lived hook/overlay.
pub(crate) fn spawn_forwarding_thread<T: Send + 'static, U: Send + 'static>(
    name: &str,
    receiver: std::sync::mpsc::Receiver<T>,
    sender: mpsc::Sender<U>,
    map_fn: impl Fn(T) -> U + Send + 'static,
) -> Result<ForwardingThreadHandle> {
    let thread_name = name.to_string();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || 'forward: loop {
            if thread_stop.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(event) => {
                    let mut mapped = map_fn(event);
                    loop {
                        if thread_stop.load(std::sync::atomic::Ordering::Acquire) {
                            break 'forward;
                        }
                        match sender.try_send(mapped) {
                            Ok(()) => break,
                            Err(tokio::sync::mpsc::error::TrySendError::Full(value)) => {
                                mapped = value;
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                break 'forward;
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to spawn {} thread: {}", thread_name, e))?;
    Ok(ForwardingThreadHandle {
        name: thread_name,
        stop,
        thread: Some(thread),
    })
}

/// Join a thread with a timeout. Returns true if the thread joined within the deadline,
/// false if it timed out. The join handle remains available on timeout so callers can retry
/// later without losing ownership.
pub(crate) fn join_with_timeout(
    handle: &mut Option<std::thread::JoinHandle<()>>,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;

    loop {
        let Some(join_handle) = handle.as_ref() else {
            return true;
        };
        if join_handle.is_finished() {
            let join_handle = handle
                .take()
                .expect("join handle must exist when finishing timed join");
            let _ = join_handle.join();
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(JOIN_WITH_TIMEOUT_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_respects_subscription_filter() {
        let requested = BTreeSet::from([EventKind::Workspace]);
        let heartbeat = IpcEvent::Heartbeat { uptime_seconds: 1 };
        assert!(!stream_event_matches_subscription(&requested, &heartbeat));

        let requested = BTreeSet::from([EventKind::Heartbeat]);
        assert!(stream_event_matches_subscription(&requested, &heartbeat));
    }

    #[test]
    fn lagged_is_delivered_as_unconditional_stream_control() {
        let requested = BTreeSet::from([EventKind::Workspace]);
        assert!(stream_event_matches_subscription(
            &requested,
            &IpcEvent::Lagged { skipped: 1 }
        ));
    }

    #[test]
    fn subscriber_limit_is_independent_from_command_limit() {
        let subscriber_limit = Arc::new(Semaphore::new(1));
        let command_limit = Arc::new(Semaphore::new(1));
        let stream_permit = subscriber_limit
            .clone()
            .try_acquire_owned()
            .expect("first subscriber fits");
        assert!(subscriber_limit.clone().try_acquire_owned().is_err());
        assert!(command_limit.clone().try_acquire_owned().is_ok());
        drop(stream_permit);
        assert!(subscriber_limit.clone().try_acquire_owned().is_ok());
    }
}
