//! Touchpad gesture detection via low-level mouse hook.

use crate::{recover_poisoned_mutex, Win32Error, WM_QUIT_LLHOOK_THREAD};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_MOUSE_LL,
};

/// Gesture events detected from touchpad/pointer input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureEvent {
    /// Three-finger swipe left
    SwipeLeft,
    /// Three-finger swipe right
    SwipeRight,
    /// Three-finger swipe up
    SwipeUp,
    /// Three-finger swipe down
    SwipeDown,
    /// Modifier + mouse wheel scroll up
    ScrollUp,
    /// Modifier + mouse wheel scroll down
    ScrollDown,
}

/// Wheel message constants (not all exposed by windows-rs).
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_MOUSEHWHEEL: u32 = 0x020E;
const LLMHF_INJECTED: u32 = 0x01;

/// Threshold for accumulated wheel delta before firing a swipe gesture.
/// 3 * WHEEL_DELTA (120) = 360.
const GESTURE_SCROLL_THRESHOLD: i32 = 360;
const WHEEL_DELTA: i32 = 120;
const STREAM_END_MS: u128 = 80;
/// Coarse wall-clock milliseconds for throttling one diagnostic.
fn coarse_now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

static LAST_MODIFIER_MISS_LOG_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

const NAVIGATION_COOLDOWN_MS: u128 = 150;

/// Timeout in milliseconds: if no swipe event arrives within this window,
/// partial accumulators are reset.
const GESTURE_TIMEOUT_MS: u128 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelMode {
    Discrete,
    Stream,
}

#[derive(Debug, Clone, Copy)]
struct WheelGestureInput {
    now_ms: u128,
    axis: WheelAxis,
    delta: i32,
    flags: u32,
    mods_held: bool,
    swipe_candidate: bool,
}

#[derive(Debug, Clone, Copy)]
struct StreamSummary {
    emitted: u32,
    suppressed: u32,
    duration_ms: u128,
    flags_seen: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct EngineTrace {
    mode_transition: Option<(WheelMode, WheelMode)>,
    stream_end: Option<StreamSummary>,
    cooldown_suppressed: bool,
    mode: Option<WheelMode>,
    accumulation: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
struct WheelGestureResult {
    event: Option<GestureEvent>,
    consume: bool,
    trace: EngineTrace,
}

struct WheelGestureEngine {
    navigation_mode: WheelMode,
    navigation_burst_count: u32,
    navigation_last_event_ms: Option<u128>,
    navigation_stream_started_ms: Option<u128>,
    navigation_accum: i32,
    navigation_cooldown_until_ms: Option<u128>,
    navigation_stream_emitted: u32,
    navigation_stream_suppressed: u32,
    navigation_stream_flags: u32,
    swipe_accum_x: i32,
    swipe_accum_y: i32,
    swipe_last_event_ms: Option<u128>,
}

impl WheelGestureEngine {
    fn new() -> Self {
        Self {
            navigation_mode: WheelMode::Discrete,
            navigation_burst_count: 0,
            navigation_last_event_ms: None,
            navigation_stream_started_ms: None,
            navigation_accum: 0,
            navigation_cooldown_until_ms: None,
            navigation_stream_emitted: 0,
            navigation_stream_suppressed: 0,
            navigation_stream_flags: 0,
            swipe_accum_x: 0,
            swipe_accum_y: 0,
            swipe_last_event_ms: None,
        }
    }

    fn process(&mut self, input: WheelGestureInput) -> WheelGestureResult {
        if input.axis == WheelAxis::Vertical && input.mods_held {
            return self.process_navigation(input);
        }
        if input.swipe_candidate {
            return self.process_swipe(input);
        }
        WheelGestureResult {
            event: None,
            consume: false,
            trace: EngineTrace::default(),
        }
    }

    fn process_navigation(&mut self, input: WheelGestureInput) -> WheelGestureResult {
        if input.delta == 0 {
            return WheelGestureResult {
                event: None,
                consume: false,
                trace: EngineTrace::default(),
            };
        }

        let mut trace = EngineTrace::default();
        if let Some(last_event_ms) = self.navigation_last_event_ms {
            let gap_ms = input.now_ms.saturating_sub(last_event_ms);
            if gap_ms > STREAM_END_MS {
                if self.navigation_mode == WheelMode::Stream {
                    trace.stream_end = Some(StreamSummary {
                        emitted: self.navigation_stream_emitted,
                        suppressed: self.navigation_stream_suppressed,
                        duration_ms: input.now_ms.saturating_sub(
                            self.navigation_stream_started_ms.unwrap_or(last_event_ms),
                        ),
                        flags_seen: self.navigation_stream_flags,
                    });
                    trace.mode_transition = Some((WheelMode::Stream, WheelMode::Discrete));
                }
                self.reset_navigation_stream();
                self.navigation_burst_count = 1;
            } else {
                self.navigation_burst_count += 1;
            }
        } else {
            self.navigation_burst_count = 1;
        }
        self.navigation_last_event_ms = Some(input.now_ms);
        self.navigation_stream_flags |= input.flags;

        if self.navigation_mode == WheelMode::Discrete && self.navigation_burst_count >= 2 {
            self.navigation_mode = WheelMode::Stream;
            self.navigation_stream_started_ms = Some(input.now_ms);
            trace.mode_transition = Some((WheelMode::Discrete, WheelMode::Stream));
        }

        let event = if self
            .navigation_cooldown_until_ms
            .is_some_and(|until_ms| input.now_ms < until_ms)
        {
            if self.navigation_mode == WheelMode::Stream {
                self.navigation_stream_suppressed += 1;
                trace.cooldown_suppressed = true;
            }
            None
        } else {
            self.navigation_accum += input.delta;
            if self.navigation_accum.abs() < WHEEL_DELTA {
                None
            } else {
                let event = scroll_event(self.navigation_accum);
                self.navigation_accum %= WHEEL_DELTA;
                self.navigation_cooldown_until_ms = Some(input.now_ms + NAVIGATION_COOLDOWN_MS);
                if self.navigation_mode == WheelMode::Stream {
                    self.navigation_stream_emitted += 1;
                }
                Some(event)
            }
        };

        trace.mode = Some(self.navigation_mode);
        trace.accumulation = Some(self.navigation_accum);
        WheelGestureResult {
            event,
            consume: true,
            trace,
        }
    }

    fn process_swipe(&mut self, input: WheelGestureInput) -> WheelGestureResult {
        let mut trace = EngineTrace::default();
        if self.swipe_last_event_ms.is_some_and(|last_event_ms| {
            input.now_ms.saturating_sub(last_event_ms) > GESTURE_TIMEOUT_MS
        }) {
            self.swipe_accum_x = 0;
            self.swipe_accum_y = 0;
        }
        self.swipe_last_event_ms = Some(input.now_ms);

        let accum = match input.axis {
            WheelAxis::Horizontal => &mut self.swipe_accum_x,
            WheelAxis::Vertical => &mut self.swipe_accum_y,
        };
        *accum += input.delta;

        let event = if accum.abs() >= GESTURE_SCROLL_THRESHOLD {
            let event = match input.axis {
                WheelAxis::Horizontal if *accum > 0 => GestureEvent::SwipeRight,
                WheelAxis::Horizontal => GestureEvent::SwipeLeft,
                WheelAxis::Vertical if *accum > 0 => GestureEvent::SwipeDown,
                WheelAxis::Vertical => GestureEvent::SwipeUp,
            };
            *accum = 0;
            Some(event)
        } else {
            None
        };

        trace.accumulation = Some(*accum);
        WheelGestureResult {
            event,
            consume: false,
            trace,
        }
    }

    fn retry_navigation_after_failed_delivery(&mut self) {
        self.navigation_cooldown_until_ms = None;
        if self.navigation_mode == WheelMode::Stream {
            self.navigation_stream_emitted = self.navigation_stream_emitted.saturating_sub(1);
        }
    }

    fn reset_navigation_stream(&mut self) {
        self.navigation_mode = WheelMode::Discrete;
        self.navigation_accum = 0;
        self.navigation_cooldown_until_ms = None;
        self.navigation_stream_started_ms = None;
        self.navigation_stream_emitted = 0;
        self.navigation_stream_suppressed = 0;
        self.navigation_stream_flags = 0;
    }
}

fn scroll_event(delta: i32) -> GestureEvent {
    if delta > 0 {
        GestureEvent::ScrollUp
    } else {
        GestureEvent::ScrollDown
    }
}

/// Gesture accumulator state for the low-level mouse hook.
struct GestureAccumState {
    engine: WheelGestureEngine,
    started_at: std::time::Instant,
}

/// Modifier flags for scroll wheel navigation, stored as a bitmask.
/// Bit 0 = Ctrl, Bit 1 = Alt, Bit 2 = Shift, Bit 3 = Win.
static SCROLL_MODIFIER_FLAGS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0x03); // default: Ctrl + Alt

/// Global sender for gesture events.
static GESTURE_SENDER: std::sync::Mutex<Option<mpsc::SyncSender<GestureEvent>>> =
    std::sync::Mutex::new(None);

/// Global gesture accumulator state.
/// Initialized to `None`; `register_gestures()` sets it to `Some(...)`.
static GESTURE_STATE: std::sync::Mutex<Option<GestureAccumState>> = std::sync::Mutex::new(None);
static ACTIVE_GESTURE_THREAD: AtomicU32 = AtomicU32::new(0);
static GESTURE_DELIVERY_CONNECTED: AtomicBool = AtomicBool::new(false);

fn clear_gesture_globals() {
    *GESTURE_SENDER.lock().unwrap_or_else(recover_poisoned_mutex) = None;
    *GESTURE_STATE.lock().unwrap_or_else(recover_poisoned_mutex) = None;
    GESTURE_DELIVERY_CONNECTED.store(false, Ordering::Release);
}

fn retire_gesture_thread(thread_id: u32) {
    if ACTIVE_GESTURE_THREAD
        .compare_exchange(thread_id, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        clear_gesture_globals();
    }
}

/// Handle for gesture detection.
///
/// Dropping this handle will signal the dedicated message-pump thread to
/// unhook the low-level mouse hook and exit.
pub struct GestureHandle {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for GestureHandle {
    fn drop(&mut self) {
        let posted = unsafe {
            PostThreadMessageW(
                self.thread_id,
                WM_QUIT_LLHOOK_THREAD,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
            .is_ok()
        };
        if let Some(thread) = self.thread.take() {
            if posted {
                for _ in 0..30 {
                    if thread.is_finished() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            if thread.is_finished() {
                let _ = thread.join();
                tracing::debug!("Gesture detection stopped");
            } else {
                tracing::warn!(
                    "Gesture hook did not acknowledge shutdown; retaining singleton globals until unhook"
                );
            }
        }
    }
}

/// Set the modifier keys required for scroll wheel navigation.
///
/// Parses a modifier string like "Ctrl+Alt", "Ctrl+Shift", etc.
/// Unrecognized tokens are ignored.
pub fn set_scroll_modifier(modifier_str: &str) {
    let mut flags: u8 = 0;
    for part in modifier_str.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "control" => flags |= 0x01,
            "alt" | "menu" => flags |= 0x02,
            "shift" => flags |= 0x04,
            "win" | "super" => flags |= 0x08,
            _ => {}
        }
    }
    if flags == 0 {
        // Fallback to Ctrl+Alt if nothing valid was parsed
        flags = 0x03;
    }
    SCROLL_MODIFIER_FLAGS.store(flags, std::sync::atomic::Ordering::Relaxed);
    tracing::debug!(
        "Scroll modifier set to: {} (flags=0x{:02x})",
        modifier_str,
        flags
    );
}

/// Register a low-level mouse hook for gesture detection via wheel events.
///
/// Spawns a dedicated thread with a Win32 message pump so that `WH_MOUSE_LL`
/// callbacks are dispatched promptly (low-level hooks require the installing
/// thread to pump messages).
///
/// Returns a handle that must be kept alive to receive gesture events,
/// and a channel receiver for gesture events.
pub fn register_gestures() -> Result<(GestureHandle, mpsc::Receiver<GestureEvent>), Win32Error> {
    // Bound low-level callback ingress; a full queue passes the wheel through.
    let (tx, rx) = mpsc::sync_channel(64);

    // Store sender globally
    {
        let mut sender = GESTURE_SENDER.lock().map_err(|_| {
            Win32Error::HookInstallFailed("Gesture sender mutex poisoned".to_string())
        })?;
        if sender.is_some() {
            return Err(Win32Error::HookInstallFailed(
                "Gesture sender already initialized - drop existing GestureHandle first"
                    .to_string(),
            ));
        }
        *sender = Some(tx);
        GESTURE_DELIVERY_CONNECTED.store(true, Ordering::Release);
    }

    // Initialize accumulator state. If any setup after sender publication
    // fails, roll every singleton back before returning.
    let setup_result = GESTURE_STATE
        .lock()
        .map_err(|_| Win32Error::HookInstallFailed("Gesture state mutex poisoned".to_string()))
        .map(|mut state| {
            *state = Some(GestureAccumState {
                engine: WheelGestureEngine::new(),
                started_at: std::time::Instant::now(),
            });
        });
    if let Err(error) = setup_result {
        clear_gesture_globals();
        return Err(error);
    }

    // Channel to receive init result from the dedicated thread
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32, Win32Error>>();

    let thread = std::thread::Builder::new()
        .name("gesture-hook".into())
        .spawn(move || {
            unsafe {
                let thread_id = GetCurrentThreadId();
                ACTIVE_GESTURE_THREAD.store(thread_id, Ordering::Release);

                // Ensure message queue exists before signalling init
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);

                // Install the low-level mouse hook on this thread
                let hook =
                    match SetWindowsHookExW(WH_MOUSE_LL, Some(gesture_mouse_hook_proc), None, 0) {
                        Ok(h) => h,
                        Err(e) => {
                            let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                                "SetWindowsHookExW for gesture hook failed: {}",
                                e
                            ))));
                            retire_gesture_thread(thread_id);
                            return;
                        }
                    };

                let _ = init_tx.send(Ok(thread_id));

                // Message pump — required for WH_MOUSE_LL callbacks
                loop {
                    let ret = GetMessageW(&mut msg, None, 0, 0).0;
                    if ret <= 0 {
                        break;
                    }
                    if msg.message == WM_QUIT_LLHOOK_THREAD {
                        break;
                    }
                    let _ = DispatchMessageW(&msg);
                }

                let _ = UnhookWindowsHookEx(hook);
                retire_gesture_thread(thread_id);
            }
        })
        .map_err(|e| {
            clear_gesture_globals();
            Win32Error::HookInstallFailed(format!("Failed to spawn gesture thread: {}", e))
        })?;

    // Wait for initialization
    let thread_id = match init_rx.recv() {
        Ok(Ok(thread_id)) => thread_id,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(_) => {
            if thread.is_finished() {
                let _ = thread.join();
            }
            return Err(Win32Error::HookInstallFailed(
                "Gesture thread initialization failed".to_string(),
            ));
        }
    };

    tracing::info!("Gesture detection registered (low-level mouse hook)");

    Ok((
        GestureHandle {
            thread_id,
            thread: Some(thread),
        },
        rx,
    ))
}

fn scroll_modifiers_held(flags: u8) -> bool {
    const VK_CONTROL: i32 = 0x11;
    const VK_MENU: i32 = 0x12;
    const VK_SHIFT: i32 = 0x10;
    const VK_LWIN: i32 = 0x5B;
    const VK_RWIN: i32 = 0x5C;

    flags != 0
        && (flags & 0x01 == 0 || unsafe { GetAsyncKeyState(VK_CONTROL) } < 0)
        && (flags & 0x02 == 0 || unsafe { GetAsyncKeyState(VK_MENU) } < 0)
        && (flags & 0x04 == 0 || unsafe { GetAsyncKeyState(VK_SHIFT) } < 0)
        && (flags & 0x08 == 0
            || unsafe { GetAsyncKeyState(VK_LWIN) } < 0
            || unsafe { GetAsyncKeyState(VK_RWIN) } < 0)
}

fn send_gesture_event(event: GestureEvent) -> bool {
    let result = match GESTURE_SENDER.try_lock() {
        Ok(sender) => sender.as_ref().map(|sender| sender.try_send(event)),
        Err(std::sync::TryLockError::Poisoned(error)) => error
            .into_inner()
            .as_ref()
            .map(|sender| sender.try_send(event)),
        Err(std::sync::TryLockError::WouldBlock) => return false,
    };
    match result {
        Some(Ok(())) => true,
        Some(Err(mpsc::TrySendError::Full(_))) => false,
        Some(Err(mpsc::TrySendError::Disconnected(_))) | None => {
            GESTURE_DELIVERY_CONNECTED.store(false, Ordering::Release);
            false
        }
    }
}

/// Low-level mouse hook callback for gesture detection.
///
/// Handles WM_MOUSEWHEEL and WM_MOUSEHWHEEL to normalize modifier navigation
/// and fire swipe gesture events when the threshold is exceeded.
unsafe extern "system" fn gesture_mouse_hook_proc(
    ncode: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    if ncode >= 0 && ACTIVE_GESTURE_THREAD.load(Ordering::Acquire) == GetCurrentThreadId() {
        let msg = wparam.0 as u32;
        let axis = match msg {
            WM_MOUSEHWHEEL => Some(WheelAxis::Horizontal),
            WM_MOUSEWHEEL => Some(WheelAxis::Vertical),
            _ => None,
        };
        if let Some(axis) = axis {
            let mouse_struct = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let delta = (mouse_struct.mouseData >> 16) as i16 as i32;
            let modifier_flags = SCROLL_MODIFIER_FLAGS.load(std::sync::atomic::Ordering::Relaxed);
            let mods_held = axis == WheelAxis::Vertical && scroll_modifiers_held(modifier_flags);
            let swipe_candidate = mouse_struct.flags & LLMHF_INJECTED != 0;

            let mut state_guard = GESTURE_STATE.lock().unwrap_or_else(recover_poisoned_mutex);
            if let Some(state) = state_guard.as_mut() {
                let result = state.engine.process(WheelGestureInput {
                    now_ms: state.started_at.elapsed().as_millis(),
                    axis,
                    delta,
                    flags: mouse_struct.flags,
                    mods_held,
                    swipe_candidate,
                });

                // A wheel notch that fails the modifier check is the one outcome
                // with no visible trace at debug level, because every other
                // navigation line is downstream of mods_held. Some windows take
                // the keyboard below user mode - VMware's enhanced virtual
                // keyboard is one - so Ctrl and Alt never appear in the async
                // key state and navigation is silently inert over them. Report
                // it once a second so the cause is one log line instead of an
                // investigation, without one line per notch.
                if axis == WheelAxis::Vertical && delta != 0 && !mods_held {
                    let now = coarse_now_millis();
                    let last = LAST_MODIFIER_MISS_LOG_MS.load(Ordering::Acquire);
                    if now.saturating_sub(last) >= 1000
                        && LAST_MODIFIER_MISS_LOG_MS
                            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                    {
                        tracing::debug!(
                            delta,
                            modifier_flags,
                            "Wheel navigation ignored: the configured scroll modifiers are not held \
                             in this process's key state; a window that takes the keyboard below \
                             user mode starves them"
                        );
                    }
                }

                tracing::trace!(
                    ?axis,
                    delta,
                    hook_flags = mouse_struct.flags,
                    modifier_flags,
                    mods_held,
                    swipe_candidate,
                    mode = ?result.trace.mode,
                    accumulation = ?result.trace.accumulation,
                    consume = result.consume,
                    "Wheel hook decision"
                );
                if let Some((from, to)) = result.trace.mode_transition {
                    tracing::debug!(
                        ?from,
                        ?to,
                        delta,
                        hook_flags = mouse_struct.flags,
                        "Wheel navigation mode changed"
                    );
                }
                if let Some(summary) = result.trace.stream_end {
                    tracing::debug!(
                        emitted = summary.emitted,
                        suppressed = summary.suppressed,
                        duration_ms = summary.duration_ms,
                        flags_seen = summary.flags_seen,
                        "Wheel navigation stream ended"
                    );
                }
                if result.trace.cooldown_suppressed {
                    tracing::trace!(
                        ?axis,
                        delta,
                        hook_flags = mouse_struct.flags,
                        "Wheel gesture suppressed during cooldown"
                    );
                }
                let mut delivery_available = GESTURE_DELIVERY_CONNECTED.load(Ordering::Acquire);
                if let Some(event) = result.event {
                    tracing::debug!(
                        ?event,
                        ?axis,
                        delta,
                        hook_flags = mouse_struct.flags,
                        "Wheel gesture emitted"
                    );
                    delivery_available = send_gesture_event(event);
                    if !delivery_available && mods_held {
                        state.engine.retry_navigation_after_failed_delivery();
                    }
                }
                if result.consume && delivery_available {
                    return windows::Win32::Foundation::LRESULT(1);
                }
            }
        }
    }

    CallNextHookEx(None, ncode, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    static GESTURE_GLOBAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn input(
        now_ms: u128,
        axis: WheelAxis,
        delta: i32,
        mods_held: bool,
        flags: u32,
    ) -> WheelGestureInput {
        WheelGestureInput {
            now_ms,
            axis,
            delta,
            flags,
            mods_held,
            swipe_candidate: flags & LLMHF_INJECTED != 0,
        }
    }

    #[test]
    fn modifier_notches_remain_one_to_one() {
        let mut engine = WheelGestureEngine::new();

        let up = engine.process(input(0, WheelAxis::Vertical, 120, true, 0));
        let down = engine.process(input(100, WheelAxis::Vertical, -120, true, 0));
        let passthrough = engine.process(input(200, WheelAxis::Vertical, 120, false, 0));

        assert_eq!(up.event, Some(GestureEvent::ScrollUp));
        assert!(up.consume);
        assert_eq!(down.event, Some(GestureEvent::ScrollDown));
        assert!(down.consume);
        assert_eq!(passthrough.event, None);
        assert!(!passthrough.consume);
    }

    #[test]
    fn modifier_stream_at_40ms_is_cooldown_bound() {
        let mut engine = WheelGestureEngine::new();
        let mut emitted = 0;

        for index in 0..9 {
            let result = engine.process(input(index * 40, WheelAxis::Vertical, 120, true, 0));
            assert!(result.consume);
            emitted += usize::from(result.event.is_some());
        }

        assert_eq!(emitted, 3);
    }

    #[test]
    fn partial_ticks_do_not_emit_before_reaching_a_notch() {
        let mut engine = WheelGestureEngine::new();

        let first = engine.process(input(0, WheelAxis::Vertical, 10, true, 0));
        let second = engine.process(input(10, WheelAxis::Vertical, 10, true, 0));
        let third = engine.process(input(20, WheelAxis::Vertical, 10, true, 0));

        assert_eq!(first.event, None);
        assert_eq!(second.event, None);
        assert_eq!(third.event, None);
        assert_eq!(engine.navigation_mode, WheelMode::Stream);
        assert_eq!(engine.navigation_accum, 30);
    }

    #[test]
    fn navigation_stream_retains_only_partial_remainder_during_cooldown() {
        let mut engine = WheelGestureEngine::new();

        let first = engine.process(input(0, WheelAxis::Vertical, 240, true, 0));
        assert_eq!(first.event, Some(GestureEvent::ScrollUp));
        assert_eq!(engine.navigation_accum, 0);

        for now_ms in [40, 80, 120] {
            let suppressed = engine.process(input(now_ms, WheelAxis::Vertical, 1, true, 0));
            assert_eq!(suppressed.event, None);
            assert!(suppressed.trace.cooldown_suppressed);
            assert_eq!(engine.navigation_accum, 0);
        }
        let residual = engine.process(input(160, WheelAxis::Vertical, 1, true, 0));
        assert_eq!(residual.event, None);
        assert_eq!(engine.navigation_accum, 1);

        let mut multi_notch_engine = WheelGestureEngine::new();
        let multi_notch = multi_notch_engine.process(input(0, WheelAxis::Vertical, 250, true, 0));
        assert_eq!(multi_notch.event, Some(GestureEvent::ScrollUp));
        assert_eq!(multi_notch_engine.navigation_accum, 10);

        let partial_suppressed =
            multi_notch_engine.process(input(40, WheelAxis::Vertical, 1, true, 0));
        assert_eq!(partial_suppressed.event, None);
        assert!(partial_suppressed.trace.cooldown_suppressed);
        assert_eq!(multi_notch_engine.navigation_accum, 10);
    }

    #[test]
    fn navigation_sign_reversal_cancels_partial_accumulation() {
        let mut engine = WheelGestureEngine::new();

        engine.process(input(0, WheelAxis::Vertical, 80, true, 0));
        let result = engine.process(input(40, WheelAxis::Vertical, -80, true, 0));

        assert_eq!(result.event, None);
        assert_eq!(engine.navigation_accum, 0);
    }

    #[test]
    fn zero_delta_navigation_passes_through() {
        let mut engine = WheelGestureEngine::new();

        let result = engine.process(input(0, WheelAxis::Vertical, 0, true, 0));

        assert_eq!(result.event, None);
        assert!(!result.consume);
        assert_eq!(engine.navigation_last_event_ms, None);
    }

    #[test]
    fn navigation_stream_resets_after_pause() {
        let mut engine = WheelGestureEngine::new();
        for index in 0..3 {
            engine.process(input(index * 10, WheelAxis::Vertical, 40, true, 0));
        }

        let result = engine.process(input(200, WheelAxis::Vertical, 120, true, 0));

        assert_eq!(result.event, Some(GestureEvent::ScrollUp));
        assert_eq!(result.trace.stream_end.unwrap().emitted, 1);
    }

    #[test]
    fn rapid_swipes_remain_independent() {
        let mut engine = WheelGestureEngine::new();

        let first = engine.process(input(0, WheelAxis::Vertical, 360, false, LLMHF_INJECTED));
        let second = engine.process(input(10, WheelAxis::Vertical, 360, false, LLMHF_INJECTED));

        assert_eq!(first.event, Some(GestureEvent::SwipeDown));
        assert_eq!(second.event, Some(GestureEvent::SwipeDown));
    }

    #[test]
    fn horizontal_swipe_accumulates_independently() {
        let mut engine = WheelGestureEngine::new();

        engine.process(input(0, WheelAxis::Vertical, 240, false, LLMHF_INJECTED));
        let result = engine.process(input(
            10,
            WheelAxis::Horizontal,
            -360,
            false,
            LLMHF_INJECTED,
        ));

        assert_eq!(result.event, Some(GestureEvent::SwipeLeft));
        assert_eq!(engine.swipe_accum_y, 240);
    }

    #[test]
    fn non_candidate_unmodified_wheel_passes_through() {
        let mut engine = WheelGestureEngine::new();

        let result = engine.process(input(0, WheelAxis::Vertical, 120, false, 0));

        assert_eq!(result.event, None);
        assert!(!result.consume);
    }

    #[test]
    fn swipe_timeout_clears_partial_accumulation() {
        let mut engine = WheelGestureEngine::new();

        engine.process(input(0, WheelAxis::Vertical, 240, false, LLMHF_INJECTED));
        let result = engine.process(input(
            GESTURE_TIMEOUT_MS + 1,
            WheelAxis::Vertical,
            120,
            false,
            LLMHF_INJECTED,
        ));

        assert_eq!(result.event, None);
    }

    #[test]
    fn injection_flag_does_not_change_navigation_stream_mode() {
        let mut unflagged = WheelGestureEngine::new();
        let mut injected = WheelGestureEngine::new();
        let mut unflagged_events = Vec::new();
        let mut injected_events = Vec::new();

        for index in 0..6 {
            unflagged_events.push(
                unflagged
                    .process(input(index * 40, WheelAxis::Vertical, 120, true, 0))
                    .event,
            );
            injected_events.push(
                injected
                    .process(input(
                        index * 40,
                        WheelAxis::Vertical,
                        120,
                        true,
                        LLMHF_INJECTED,
                    ))
                    .event,
            );
        }

        assert_eq!(unflagged_events, injected_events);
        assert_eq!(unflagged.navigation_mode, injected.navigation_mode);
    }

    #[test]
    fn disconnected_delivery_disarms_wheel_consumption_without_releasing_singleton() {
        let _guard = GESTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        *GESTURE_SENDER.lock().unwrap_or_else(recover_poisoned_mutex) = Some(tx);
        GESTURE_DELIVERY_CONNECTED.store(true, Ordering::Release);

        assert!(!send_gesture_event(GestureEvent::ScrollUp));
        assert!(!GESTURE_DELIVERY_CONNECTED.load(Ordering::Acquire));
        assert!(GESTURE_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .is_some());
        clear_gesture_globals();
    }

    #[test]
    fn failed_navigation_delivery_rearms_the_next_notch() {
        let mut engine = WheelGestureEngine::new();
        let first = engine.process(input(0, WheelAxis::Vertical, 120, true, 0));
        assert_eq!(first.event, Some(GestureEvent::ScrollUp));
        engine.retry_navigation_after_failed_delivery();
        let retry = engine.process(input(10, WheelAxis::Vertical, 120, true, 0));
        assert_eq!(retry.event, Some(GestureEvent::ScrollUp));
    }

    #[test]
    fn full_delivery_queue_passes_through_without_disabling_generation() {
        let _guard = GESTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.try_send(GestureEvent::SwipeLeft).unwrap();
        *GESTURE_SENDER.lock().unwrap_or_else(recover_poisoned_mutex) = Some(tx);
        GESTURE_DELIVERY_CONNECTED.store(true, Ordering::Release);

        assert!(!send_gesture_event(GestureEvent::ScrollUp));
        assert!(GESTURE_DELIVERY_CONNECTED.load(Ordering::Acquire));
        clear_gesture_globals();
    }

    #[test]
    fn stale_generation_cannot_clear_active_gesture_state() {
        let _guard = GESTURE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        let (tx, _rx) = mpsc::sync_channel(1);
        *GESTURE_SENDER.lock().unwrap_or_else(recover_poisoned_mutex) = Some(tx);
        ACTIVE_GESTURE_THREAD.store(32, Ordering::Release);

        retire_gesture_thread(31);
        assert!(GESTURE_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .is_some());
        retire_gesture_thread(32);
        assert!(GESTURE_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .is_none());
    }
}
