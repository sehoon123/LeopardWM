use leopardwm_platform_win32::overview::OverviewEvent;
use leopardwm_platform_win32::tab_strip::TabAction;
use leopardwm_platform_win32::{GestureEvent, HotkeyEvent, WindowEvent, WindowInfo};
use std::collections::BTreeSet;
use tokio::sync::{broadcast, oneshot};

use crate::animation_worker;
use crate::settings;
use crate::tray;
use leopardwm_ipc::{EventKind, IpcCommand, IpcEvent, IpcResponse};

/// Bundle returned to the per-client IPC task when a `Subscribe` command
/// is processed. Created atomically under the AppState mutex so the
/// snapshot reflects state at the same instant the broadcast receiver
/// was subscribed — no event between subscribe and snapshot can be lost.
pub(crate) struct SubscribeStartup {
    /// The `IpcResponse::Subscribed` ack frame to write to the client first.
    pub(crate) ack: IpcResponse,
    /// Initial snapshot frames to write after the ack.
    pub(crate) snapshot: Vec<IpcEvent>,
    /// Receiver attached to the daemon's event broadcaster — guaranteed
    /// to deliver every event sent after the snapshot was taken.
    pub(crate) receiver: broadcast::Receiver<IpcEvent>,
}

/// Process/class identity observed for a live HWND.
///
/// Numeric HWND values are recycled. Lifecycle work may wait behind a bounded
/// daemon queue, so a later dispatch must not apply old-incarnation cleanup to
/// a replacement window that inherited the same numeric handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowIncarnation {
    pub(crate) process_id: u32,
    pub(crate) class_name: String,
}

impl WindowIncarnation {
    pub(crate) fn from_window_info(info: &WindowInfo) -> Self {
        Self {
            process_id: info.process_id,
            class_name: info.class_name.clone(),
        }
    }

    pub(crate) fn capture(hwnd: u64) -> Option<Self> {
        leopardwm_platform_win32::get_window_info(hwnd)
            .as_ref()
            .map(Self::from_window_info)
    }
}

/// A platform lifecycle event plus the source incarnation when the HWND was
/// still inspectable. Destroy events normally have no live source by the time
/// they are delivered; dispatch additionally checks current liveness before
/// treating those as physical cleanup permission.
#[derive(Debug, Clone)]
pub(crate) struct DaemonWindowEvent {
    pub(crate) event: WindowEvent,
    pub(crate) source_incarnation: Option<WindowIncarnation>,
}

impl DaemonWindowEvent {
    pub(crate) fn capture(event: WindowEvent) -> Self {
        let hwnd = match &event {
            WindowEvent::Created(hwnd)
            | WindowEvent::Destroyed(hwnd)
            | WindowEvent::Hidden(hwnd)
            | WindowEvent::Focused(hwnd)
            | WindowEvent::Minimized(hwnd)
            | WindowEvent::Restored(hwnd)
            | WindowEvent::MovedOrResized(hwnd)
            | WindowEvent::MoveSizeStart(hwnd)
            | WindowEvent::MoveSizeEnd(hwnd)
            | WindowEvent::TitleChanged(hwnd)
            | WindowEvent::MouseEnterWindow(hwnd) => Some(*hwnd),
            WindowEvent::DisplayChange
            | WindowEvent::WorkAreaChanged
            | WindowEvent::AppearanceChanged
            | WindowEvent::MouseLeftManaged => None,
        };
        // Do not synchronously inspect every high-rate location/focus event
        // on the forwarding thread. Creation, restore, hide, and destroy are
        // the lifecycle edges that can alter ownership; those alone carry a
        // source identity for recycled-HWND fencing.
        let source_incarnation = match &event {
            WindowEvent::Created(_)
            | WindowEvent::Destroyed(_)
            | WindowEvent::Hidden(_)
            | WindowEvent::Restored(_) => hwnd.and_then(WindowIncarnation::capture),
            _ => None,
        };
        Self {
            source_incarnation,
            event,
        }
    }
}

/// Events that the daemon event loop processes.
pub(crate) enum DaemonEvent {
    /// An IPC command from a CLI client.
    IpcCommand {
        cmd: IpcCommand,
        responder: oneshot::Sender<IpcResponse>,
    },
    /// An IPC `Subscribe` command. Handled separately from `IpcCommand`
    /// because the response carries a `broadcast::Receiver` and a
    /// snapshot, which the regular `IpcResponse` channel can't express.
    /// The main loop processes this under the AppState mutex so the
    /// snapshot + receiver creation are atomic.
    IpcSubscribe {
        events: BTreeSet<EventKind>,
        responder: oneshot::Sender<SubscribeStartup>,
    },
    /// A window lifecycle event from Win32, guarded by its captured HWND
    /// incarnation when one was available.
    WindowEvent(DaemonWindowEvent),
    /// A global hotkey was pressed.
    Hotkey(HotkeyEvent),
    /// A touchpad gesture was detected.
    Gesture(GestureEvent),
    /// A tray menu event.
    Tray(tray::TrayEvent),
    /// A settings window event.
    Settings(settings::SettingsEvent),
    /// An animation frame was applied by the worker thread.
    AnimationFrameApplied(animation_worker::FrameResult),
    /// A worker-driven crossfade for the given epoch completed (normal
    /// end-of-fade OR aborted). Daemon clears `active_crossfade` and
    /// releases the `crossfade_sources` re-registration barrier when the
    /// epoch matches.
    CrossfadeComplete { epoch: u64 },
    /// Hide snap hint overlay after timeout.
    HideSnapHint,
    /// Apply focus-follows-mouse focus after delay.
    FocusFollowsMouse { window_id: u64 },
    /// Debounced display change — fires after WM_DISPLAYCHANGE settles. Stale
    /// timers are ignored by generation.
    DisplayChangeSettled { generation: u64 },
    /// Power state changed (AC/battery or power saver toggled).
    PowerStateChanged { on_battery_or_saver: bool },
    /// Update checker observed a newer release tag (e.g. `v0.1.11`).
    UpdateAvailable(String),
    /// User invoked an action on a tab in the tab strip overlay. The
    /// strip captures the column identity (`monitor`, `workspace_idx`,
    /// `column_idx`) at `show()` time so the action routes to the column
    /// the user *saw* — not whatever happens to be focused when the main
    /// loop processes this event. (Focus can change between the
    /// originating WM_* arriving in the strip's WndProc and the main loop
    /// draining the channel.) `action` discriminates left-click vs.
    /// close/untab/rename surfaces.
    TabAction {
        monitor: isize,
        workspace_idx: usize,
        column_idx: usize,
        tab_idx: usize,
        action: TabAction,
    },
    /// A pointer gesture on a monitor-edge preview. A preview is a DWM
    /// thumbnail, so the pointer lands on its overlay rather than on the
    /// previewed window; the overlay reports which window it was showing and
    /// what the pointer did. A click focuses that column, which also scrolls it
    /// into view; a drag focuses it and then hands the pointer to the window so
    /// Windows' own move loop takes over.
    PreviewGesture(leopardwm_platform_win32::PreviewClickEvent),
    /// Low-frequency tick that triggers a tab-strip refresh so background
    /// icon changes (notification badges, app icon swaps that don't
    /// accompany a title change) propagate without user interaction.
    /// Windows has no MSAA event for icon-only changes — `WM_SETICON` is
    /// a per-window message, not a global hook — so polling is the
    /// pragmatic alternative.
    TabStripIconPoll,
    /// User action from the overview overlay (activate a window, switch
    /// workspace, close a window, or dismiss). The overlay hit-tests its
    /// own model copy; the daemon routes the resulting intent here.
    Overview(OverviewEvent),
    /// Rename-dialog result delivered from the spawned modal thread.
    /// `new_title: None` means cancel; `Some("")` means clear the
    /// override and fall back to the live window title; `Some(name)`
    /// installs `name` as the override.
    TabRenameSubmitted {
        monitor: isize,
        workspace_idx: usize,
        column_idx: usize,
        tab_idx: usize,
        /// HWND captured when the popup was spawned. Authoritative
        /// rename target — independent of the (monitor/ws/col/tab)
        /// indices in case the column mutated while the popup was open.
        target_hwnd: u64,
        new_title: Option<String>,
    },
    /// Debounced persist trigger. Emitted by the background save task
    /// after a quiet period following one or more persisted-state
    /// changes. Handled on the main loop, which builds the snapshot JSON
    /// under its existing `AppState` lock and writes it off-loop. Routed
    /// through the event loop (rather than locking `AppState` directly in
    /// the spawned task) because `AppState` is not `Send`.
    PersistStateNow,
    /// Poll a retained timed-out placement worker. The timer keeps paused
    /// physical cleanup ownership alive even when no user input arrives.
    ReapPendingApplyWorkers,
    /// Shutdown signal.
    Shutdown,
}

/// Payload sent from the dialog thread back into the main loop via the
/// mpsc-to-tokio forwarder. Mirrors the strip's click-event pattern.
///
/// `target_hwnd` is captured at spawn time so the rename override lands
/// on the *original* window even if the column mutates (tab closed,
/// reordered, untabbed) while the popup is open. The monitor/ws/col/tab
/// indices are kept for diagnostics + strip refresh routing.
#[derive(Debug, Clone)]
pub(crate) struct TabRenameResult {
    pub(crate) monitor: isize,
    pub(crate) workspace_idx: usize,
    pub(crate) column_idx: usize,
    pub(crate) tab_idx: usize,
    pub(crate) target_hwnd: u64,
    pub(crate) new_title: Option<String>,
}
