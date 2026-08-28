//! Workspace state persistence: save, load, and restore snapshots.

use crate::helpers::ScaledLayoutParams;
use crate::state::*;
use anyhow::Result;
use leopardwm_core_layout::Workspace;
use leopardwm_platform_win32::MonitorId;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tracing::{debug, info, warn};

/// Persisted workspace indices are user-writable JSON but the public command
/// surface exposes exactly workspaces 1 through 9 (zero-based 0 through 8).
const MAX_SAVED_WORKSPACE_INDEX: usize = 8;

/// A serialized state snapshot paired with the generation assigned while the
/// model lock was held. Writers must retain this value rather than extracting
/// only its JSON, otherwise a delayed older worker could be mistaken for a
/// newer snapshot when it reaches the filesystem.
#[derive(Debug)]
pub(crate) struct PreparedStateWrite {
    generation: u64,
    json: String,
}

/// Result of a generation-aware state-file write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateWriteOutcome {
    Written,
    SkippedStale,
}

/// Serializes state-file replacement and rejects a generation after a newer
/// generation has started its write. The mutex deliberately covers both the
/// freshness check and durable replacement.
#[derive(Debug)]
pub(crate) struct StateFileWriter {
    path: PathBuf,
    next_generation: AtomicU64,
    highest_started_generation: Mutex<u64>,
}

impl StateFileWriter {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            next_generation: AtomicU64::new(0),
            highest_started_generation: Mutex::new(0),
        }
    }

    fn prepare(&self, json: String) -> PreparedStateWrite {
        PreparedStateWrite {
            generation: self.next_generation.fetch_add(1, Ordering::SeqCst) + 1,
            json,
        }
    }

    fn write(&self, prepared: &PreparedStateWrite) -> Result<StateWriteOutcome> {
        let mut highest_started = self
            .highest_started_generation
            .lock()
            .unwrap_or_else(|poisoned| {
                warn!("State-file writer lock was poisoned; recovering ownership");
                poisoned.into_inner()
            });

        if prepared.generation <= *highest_started {
            return Ok(StateWriteOutcome::SkippedStale);
        }

        // Advance before I/O. If a newer write fails, allowing an older worker
        // to replace the file afterwards would still regress durable state.
        *highest_started = prepared.generation;
        crate::atomic_file::write(&self.path, &prepared.json)?;
        Ok(StateWriteOutcome::Written)
    }
}

fn state_file_writer() -> &'static StateFileWriter {
    static WRITER: OnceLock<StateFileWriter> = OnceLock::new();
    WRITER.get_or_init(|| StateFileWriter::new(AppState::state_file_path()))
}

/// Validate the portions of a deserialized core workspace which normally stay
/// true only when it is mutated through core-layout APIs. Core currently has
/// no public snapshot validator, so persistence fails closed before invoking
/// layout operations that assume these invariants.
fn validate_restored_workspace(workspace: &Workspace) -> std::result::Result<(), &'static str> {
    if !workspace.scroll_offset().is_finite() {
        return Err("scroll offset is not finite");
    }
    let (outer_left, outer_right, outer_top, outer_bottom) = workspace.outer_gaps();
    if workspace.gap() < 0
        || [outer_left, outer_right, outer_top, outer_bottom]
            .into_iter()
            .any(|gap| gap < 0)
    {
        return Err("gap is negative");
    }
    if workspace.default_column_width() < 100 {
        return Err("default column width is below the core minimum");
    }

    let columns = workspace.columns();
    let focused_column = workspace.focused_column_index();
    let focused_window = workspace.focused_window_index_in_column();
    if columns.is_empty() {
        if focused_column != 0 || focused_window != 0 {
            return Err("empty workspace has nonzero focus indices");
        }
    } else {
        let Some(focused) = columns.get(focused_column) else {
            return Err("focused column is out of bounds");
        };
        if focused_window >= focused.len() {
            return Err("focused window is out of bounds");
        }
    }

    let mut seen = HashSet::new();
    for (column_index, column) in columns.iter().enumerate() {
        if column.is_empty() {
            return Err("workspace contains an empty column");
        }
        if column.width() < 100 {
            return Err("column width is below the core minimum");
        }
        if let Some(active_tab) = column.active_tab_idx() {
            if column.len() < 2 || active_tab >= column.len() {
                return Err("tabbed column has an invalid active tab");
            }
            if column_index == focused_column && active_tab != focused_window {
                return Err("focused tabbed column has mismatched active tab and focus");
            }
        }

        let weights = column.height_weights();
        if !weights.is_empty() {
            if weights.len() != column.len()
                || weights.iter().any(|weight| !weight.is_finite() || *weight < 0.0)
            {
                return Err("column has invalid height weights");
            }
            let total: f64 = weights.iter().sum();
            if !total.is_finite() || total <= 0.0 || (total - 1.0).abs() > 1e-6 {
                return Err("column height weights are not normalized");
            }
        }

        for &hwnd in column.windows() {
            if !seen.insert(hwnd) {
                return Err("workspace contains a duplicate HWND");
            }
        }
    }

    for floating in workspace.floating_windows() {
        if floating.rect.width < 1 || floating.rect.height < 1 {
            return Err("floating window has a nonpositive size");
        }
        if !seen.insert(floating.id) {
            return Err("workspace contains a duplicate HWND");
        }
    }

    if let Some(fullscreen) = workspace.fullscreen_window_id() {
        if !seen.contains(&fullscreen) || workspace.is_minimized(fullscreen) {
            return Err("fullscreen window is absent or minimized");
        }
    }

    Ok(())
}

impl AppState {
    /// Save current workspace state to disk.
    pub(crate) fn save_state(&self) -> Result<()> {
        let prepared = self.build_state_json()?;
        match Self::write_state_file(&prepared)? {
            StateWriteOutcome::Written => {
                info!("Workspace state saved to {:?}", Self::state_file_path());
            }
            StateWriteOutcome::SkippedStale => {
                debug!("Skipped stale synchronous workspace-state save");
            }
        }
        Ok(())
    }

    /// Atomically write a state snapshot only if no newer prepared generation
    /// has started first. Callers must pass the opaque value returned by
    /// [`Self::build_state_json`] unchanged; this preserves the generation
    /// assigned while the model lock was held.
    pub(crate) fn write_state_file(
        prepared: &PreparedStateWrite,
    ) -> Result<StateWriteOutcome> {
        state_file_writer().write(prepared)
    }

    /// Serialize the persisted state and assign its write generation while the
    /// caller holds the `AppState` lock. The existing background-save caller
    /// can continue to build under the lock and write outside it, but it must
    /// retain this opaque prepared value rather than extracting only the JSON.
    pub(crate) fn build_state_json(&self) -> Result<PreparedStateWrite> {
        let mut snapshots: Vec<WorkspaceSnapshot> = Vec::new();
        for (monitor_id, ws_vec) in &self.workspaces {
            let active_idx = self.active_workspace_idx(*monitor_id);
            if let Some(monitor) = self.monitors.get(monitor_id) {
                for (idx, workspace) in ws_vec.iter().enumerate() {
                    // Save non-empty workspaces and the active workspace (even if empty)
                    if !workspace.all_window_ids().is_empty() || idx == active_idx {
                        snapshots.push(WorkspaceSnapshot {
                            monitor_device_name: monitor.device_name.clone(),
                            workspace_index: idx,
                            workspace: workspace.clone(),
                        });
                    }
                }
            }
        }

        let focused_name = self
            .monitors
            .get(&self.focused_monitor)
            .map(|m| m.device_name.clone())
            .unwrap_or_default();

        // Build active workspace map by device name
        let mut active_ws_map = HashMap::new();
        for (&monitor_id, &ws_idx) in &self.active_workspace {
            if let Some(monitor) = self.monitors.get(&monitor_id) {
                active_ws_map.insert(monitor.device_name.clone(), ws_idx);
            }
        }

        let saved_at = {
            let now = std::time::SystemTime::now();
            match now.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => format!("{}", d.as_secs()),
                Err(_) => "0".to_string(),
            }
        };

        let snapshot = StateSnapshot {
            saved_at,
            workspaces: snapshots,
            focused_monitor_name: focused_name,
            active_workspace: active_ws_map,
            tab_title_overrides: self.tab_title_overrides.clone(),
        };

        let json = serde_json::to_string_pretty(&snapshot)?;
        Ok(state_file_writer().prepare(json))
    }

    /// Cheap deterministic hash of the persisted workspace state. Used to
    /// deduplicate save requests so unchanged state does not enqueue a write.
    /// Runtime-only fields marked `serde(skip)` are intentionally excluded.
    pub(crate) fn persisted_signature(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.focused_monitor.hash(&mut hasher);
        self.monitors
            .get(&self.focused_monitor)
            .map(|monitor| monitor.device_name.as_str())
            .hash(&mut hasher);

        // Sort monitor ids for a deterministic traversal order.
        let mut monitor_ids: Vec<MonitorId> = self.workspaces.keys().copied().collect();
        monitor_ids.sort_unstable();

        for monitor_id in monitor_ids {
            monitor_id.hash(&mut hasher);
            self.monitors
                .get(&monitor_id)
                .map(|monitor| monitor.device_name.as_str())
                .hash(&mut hasher);
            self.active_workspace_idx(monitor_id).hash(&mut hasher);
            if let Some(ws_vec) = self.workspaces.get(&monitor_id) {
                ws_vec.len().hash(&mut hasher);
                for (workspace_idx, workspace) in ws_vec.iter().enumerate() {
                    workspace_idx.hash(&mut hasher);
                    workspace.focused_column_index().hash(&mut hasher);
                    workspace.focused_window_index_in_column().hash(&mut hasher);
                    workspace.fullscreen_window_id().hash(&mut hasher);

                    workspace.columns().len().hash(&mut hasher);
                    for column in workspace.columns() {
                        column.width().hash(&mut hasher);
                        column.windows().len().hash(&mut hasher);
                        for &wid in column.windows() {
                            wid.hash(&mut hasher);
                            workspace.is_minimized(wid).hash(&mut hasher);
                        }
                        column.height_weights().len().hash(&mut hasher);
                        for weight in column.height_weights() {
                            weight.to_bits().hash(&mut hasher);
                        }
                        match column.mode() {
                            leopardwm_core_layout::ColumnMode::Vertical => {
                                0u8.hash(&mut hasher);
                            }
                            leopardwm_core_layout::ColumnMode::Tabbed { active_idx } => {
                                1u8.hash(&mut hasher);
                                active_idx.hash(&mut hasher);
                            }
                        }
                    }

                    workspace.floating_windows().len().hash(&mut hasher);
                    for floating in workspace.floating_windows() {
                        floating.id.hash(&mut hasher);
                        floating.rect.x.hash(&mut hasher);
                        floating.rect.y.hash(&mut hasher);
                        floating.rect.width.hash(&mut hasher);
                        floating.rect.height.hash(&mut hasher);
                        floating.pinned.hash(&mut hasher);
                        workspace.is_minimized(floating.id).hash(&mut hasher);
                    }

                    // Keep animation-frame churn bounded while tracking the
                    // persisted scroll position at whole-pixel precision.
                    (workspace.scroll_offset().round() as i64).hash(&mut hasher);
                    workspace.gap().hash(&mut hasher);
                    workspace.outer_gaps().hash(&mut hasher);
                    workspace.default_column_width().hash(&mut hasher);
                    match workspace.centering_mode() {
                        leopardwm_core_layout::CenteringMode::Center => 0u8.hash(&mut hasher),
                        leopardwm_core_layout::CenteringMode::JustInView => 1u8.hash(&mut hasher),
                        leopardwm_core_layout::CenteringMode::OnOverflow => 2u8.hash(&mut hasher),
                    }
                }
            }
        }

        // Hash full title values, not just lengths: equal-length renames are
        // distinct persisted states.
        let mut overrides: Vec<(u64, &str)> = self
            .tab_title_overrides
            .iter()
            .map(|(&key, value)| (key, value.as_str()))
            .collect();
        overrides.sort_unstable_by_key(|(key, _)| *key);
        overrides.hash(&mut hasher);

        hasher.finish()
    }

    /// Request a debounced save iff the persisted state changed since the
    /// last request. Non-blocking: drops the request on a full channel
    /// (a queued request already covers the coalesced write). No-op when
    /// no sender is installed (cfg(test) / pre-wiring).
    pub(crate) fn request_save_if_changed(&mut self) {
        let sig = self.persisted_signature();
        if self.last_persisted_sig != Some(sig) {
            self.last_persisted_sig = Some(sig);
            if let Some(tx) = &self.save_request_tx {
                let _ = tx.try_send(());
            }
        }
    }

    /// Load saved workspace state from disk.
    pub(crate) fn load_state() -> Option<StateSnapshot> {
        let state_path = Self::state_file_path();
        match std::fs::read_to_string(&state_path) {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(snapshot) => Some(snapshot),
                Err(e) => {
                    warn!("Failed to parse saved state: {}", e);
                    None
                }
            },
            Err(_) => None,
        }
    }

    /// Get the path for the state file.
    pub(crate) fn state_file_path() -> std::path::PathBuf {
        directories::ProjectDirs::from("", "", "leopardwm")
            .map(|dirs| dirs.data_dir().join("workspace-state.json"))
            .unwrap_or_else(|| std::path::PathBuf::from("workspace-state.json"))
    }

    /// Restore the FULL saved workspace structure from a snapshot, BEFORE
    /// `enumerate_and_add_windows`. For each saved workspace whose monitor is
    /// still present, the cloned `Workspace` is pruned of dead windows (closed
    /// while the daemon was down), has its `#[serde(skip)]` runtime params
    /// re-applied, and is installed into `self.workspaces[monitor][ws_idx]`.
    /// This brings back column grouping, per-column widths, intra-column
    /// heights, and scroll offset — not just monitor+workspace+order.
    ///
    /// `enumerate_and_add_windows` then skips windows already managed (the
    /// restored ones) and only adds genuinely-new windows by current position.
    ///
    /// Returns the set of `(monitor, ws_idx)` slots that were restored, so the
    /// caller can skip startup width-normalization / scroll-reset for them
    /// (which would otherwise wipe the restored widths and scroll offset).
    pub(crate) fn restore_workspace_structure(
        &mut self,
        snapshot: &StateSnapshot,
    ) -> HashSet<(MonitorId, usize)> {
        self.restore_workspace_structure_with(snapshot, |hwnd| {
            // Keep a saved window only if it's still alive AND manageable. An
            // elevated window a non-elevated daemon can't reposition would
            // otherwise restore as a column we can never fill (a ghost column);
            // dropping it here lets enumerate re-see it, record it, and notify.
            leopardwm_platform_win32::is_valid_window(hwnd)
                && !leopardwm_platform_win32::window_manage_block(hwnd).is_blocked()
        })
    }

    /// Testable core of `restore_workspace_structure`: the `keep`
    /// predicate decides which HWNDs survive pruning. Production passes the
    /// real `is_valid_window` + elevation check; tests pass a fake so the
    /// structure-rebuild logic can be exercised without Win32.
    pub(crate) fn restore_workspace_structure_with(
        &mut self,
        snapshot: &StateSnapshot,
        keep: impl Fn(u64) -> bool,
    ) -> HashSet<(MonitorId, usize)> {
        let mut restored_slots = HashSet::new();
        // A snapshot is user-writable, so ownership must be unique across every
        // restored workspace, not merely within each individual workspace.
        let mut seen_hwnds = HashSet::new();

        for ws_snapshot in &snapshot.workspaces {
            let Some(monitor_id) = self
                .monitors
                .iter()
                .find(|(_, m)| m.device_name == ws_snapshot.monitor_device_name)
                .map(|(&id, _)| id)
            else {
                debug!(
                    "Skipping saved workspace for unknown monitor '{}'",
                    ws_snapshot.monitor_device_name
                );
                continue;
            };

            // Snapshots are user-writable JSON. Reject an out-of-contract
            // index instead of aliasing it onto workspace 9, where it could
            // overwrite a legitimate saved workspace.
            if ws_snapshot.workspace_index > MAX_SAVED_WORKSPACE_INDEX {
                warn!(
                    "Skipping saved workspace {} for monitor '{}': valid range is 0..={}",
                    ws_snapshot.workspace_index,
                    ws_snapshot.monitor_device_name,
                    MAX_SAVED_WORKSPACE_INDEX
                );
                continue;
            }
            let ws_idx = ws_snapshot.workspace_index;
            if restored_slots.contains(&(monitor_id, ws_idx)) {
                warn!(
                    "Skipping duplicate saved workspace slot {} for monitor '{}'",
                    ws_idx, ws_snapshot.monitor_device_name
                );
                continue;
            }

            // `Workspace` derives Deserialize for compatibility, but its
            // mutation APIs assume invariants that arbitrary JSON does not
            // establish. Reject malformed structures before any layout code
            // can observe their invalid focus, columns, or weights.
            if let Err(reason) = validate_restored_workspace(&ws_snapshot.workspace) {
                warn!(
                    "Skipping invalid saved workspace {} for monitor '{}': {}",
                    ws_idx, ws_snapshot.monitor_device_name, reason
                );
                continue;
            }

            // Clone the saved workspace and drop windows that should not be
            // restored (closed while the daemon was down, now unmanageable, or
            // already claimed by an earlier snapshot). Mirror reconcile/migration
            // pruning: use the type-preserving remove APIs.
            let mut ws = ws_snapshot.workspace.clone();
            let to_drop: Vec<u64> = ws
                .all_window_ids()
                .into_iter()
                .filter(|&w| !keep(w) || seen_hwnds.contains(&w))
                .collect();
            for wid in to_drop {
                if ws.is_floating(wid) {
                    ws.remove_floating(wid);
                } else {
                    let _ = ws.remove_window(wid);
                }
            }
            // Core removal APIs repair focus/mode state, but validate again to
            // keep the restore boundary fail-closed if their contract changes.
            if let Err(reason) = validate_restored_workspace(&ws) {
                warn!(
                    "Skipping invalid pruned saved workspace {} for monitor '{}': {}",
                    ws_idx, ws_snapshot.monitor_device_name, reason
                );
                continue;
            }
            seen_hwnds.extend(ws.all_window_ids());

            // The clone's #[serde(skip)] runtime fields deserialized to
            // defaults; re-apply them exactly like reconcile_monitors does.
            // apply_to sets gaps + default_column_width + tab_strip_reserve_px
            // WITHOUT touching per-column widths, so the saved widths survive.
            let scale = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.scale_factor)
                .unwrap_or(1.0);
            let vw = self
                .monitors
                .get(&monitor_id)
                .map(|m| m.work_area.width)
                .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
            let params = ScaledLayoutParams::from_config(
                &self.config.layout,
                &self.config.appearance,
                scale,
                vw,
            );
            params.apply_to(&mut ws);
            ws.set_centering_mode(self.config.layout.centering_mode.into());
            ws.set_center_past_edges(self.config.layout.center_past_edges);
            ws.set_reduce_motion(self.reduce_motion);
            ws.set_scroll_animation(
                self.config.animation.scroll_duration_ms,
                self.config.animation.easing,
            );
            // Preserve the saved scroll offset (it serializes, but set it
            // explicitly so a future skip on this field would not regress).
            ws.set_scroll_offset(ws_snapshot.workspace.scroll_offset());

            // Install into the per-monitor vec, extending with fresh empty
            // workspaces as needed.
            let entry = self.workspaces.entry(monitor_id).or_default();
            while entry.len() <= ws_idx {
                let mut empty = Workspace::with_directional_gaps(
                    params.gap,
                    params.outer_gap_left,
                    params.outer_gap_right,
                    params.outer_gap_top,
                    params.outer_gap_bottom,
                );
                empty.set_default_column_width(params.default_column_width);
                empty.set_tab_strip_reserve_px(params.tab_strip_reserve_px);
                empty.set_centering_mode(self.config.layout.centering_mode.into());
                empty.set_center_past_edges(self.config.layout.center_past_edges);
                empty.set_reduce_motion(self.reduce_motion);
                empty.set_scroll_animation(
                    self.config.animation.scroll_duration_ms,
                    self.config.animation.easing,
                );
                entry.push(empty);
            }
            entry[ws_idx] = ws;
            restored_slots.insert((monitor_id, ws_idx));
            info!(
                "Restored workspace structure for monitor '{}' workspace {}",
                ws_snapshot.monitor_device_name, ws_idx
            );
        }

        restored_slots
    }

    /// Restore workspace state from a saved snapshot.
    ///
    /// This should be called AFTER windows are enumerated so that scroll offsets
    /// are not clamped against empty workspaces. Sets the scroll offset directly
    /// (bypassing clamping) to preserve the saved value.
    ///
    /// Returns the set of monitor IDs whose scroll offsets were successfully
    /// restored. The caller should skip `ensure_focused_visible()` for these
    /// monitors to avoid overwriting the restored offset.
    pub(crate) fn restore_state(&mut self, snapshot: &StateSnapshot) -> HashSet<MonitorId> {
        let mut restored_monitors = HashSet::new();
        let mut restored_slots = HashSet::new();

        for ws_snapshot in &snapshot.workspaces {
            // Find matching monitor by device name
            let monitor_id = self
                .monitors
                .iter()
                .find(|(_, m)| m.device_name == ws_snapshot.monitor_device_name)
                .map(|(&id, _)| id);

            if let Some(id) = monitor_id {
                if ws_snapshot.workspace_index > MAX_SAVED_WORKSPACE_INDEX {
                    warn!(
                        "Skipping saved workspace state {} for monitor '{}': valid range is 0..={}",
                        ws_snapshot.workspace_index,
                        ws_snapshot.monitor_device_name,
                        MAX_SAVED_WORKSPACE_INDEX
                    );
                    continue;
                }
                let ws_idx = ws_snapshot.workspace_index;
                if restored_slots.contains(&(id, ws_idx)) {
                    warn!(
                        "Skipping duplicate saved workspace state {} for monitor '{}'",
                        ws_idx, ws_snapshot.monitor_device_name
                    );
                    continue;
                }
                if let Err(reason) = validate_restored_workspace(&ws_snapshot.workspace) {
                    warn!(
                        "Skipping invalid saved workspace state {} for monitor '{}': {}",
                        ws_idx, ws_snapshot.monitor_device_name, reason
                    );
                    continue;
                }
                restored_slots.insert((id, ws_idx));
                if let Some(ws_vec) = self.workspaces.get_mut(&id) {
                    // Extend the vec with empty workspaces if needed
                    let scale = self
                        .monitors
                        .get(&id)
                        .map(|m| m.scale_factor)
                        .unwrap_or(1.0);
                    let vw = self
                        .monitors
                        .get(&id)
                        .map(|m| m.work_area.width)
                        .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                    let params = ScaledLayoutParams::from_config(
                        &self.config.layout,
                        &self.config.appearance,
                        scale,
                        vw,
                    );
                    while ws_vec.len() <= ws_idx {
                        let mut ws = Workspace::with_directional_gaps(
                            params.gap,
                            params.outer_gap_left,
                            params.outer_gap_right,
                            params.outer_gap_top,
                            params.outer_gap_bottom,
                        );
                        ws.set_default_column_width(params.default_column_width);
                        ws.set_tab_strip_reserve_px(params.tab_strip_reserve_px);
                        ws.set_centering_mode(self.config.layout.centering_mode.into());
                        ws.set_center_past_edges(self.config.layout.center_past_edges);
                        ws.set_reduce_motion(self.reduce_motion);
                        ws.set_scroll_animation(
                            self.config.animation.scroll_duration_ms,
                            self.config.animation.easing,
                        );
                        ws_vec.push(ws);
                    }
                    // Restore scroll offset from saved workspace
                    let saved_offset = ws_snapshot.workspace.scroll_offset();
                    ws_vec[ws_idx].set_scroll_offset(saved_offset);
                    restored_monitors.insert(id);
                    info!(
                        "Restored workspace state for monitor '{}' workspace {}",
                        ws_snapshot.monitor_device_name, ws_idx
                    );
                }
            } else {
                debug!(
                    "Skipping saved workspace for unknown monitor '{}'",
                    ws_snapshot.monitor_device_name
                );
            }
        }

        // Restore active workspace indices (validate in range)
        for (device_name, &ws_idx) in &snapshot.active_workspace {
            if let Some((&id, _)) = self
                .monitors
                .iter()
                .find(|(_, m)| &m.device_name == device_name)
            {
                // Clamp to valid range — index must be within the workspace vec
                let max_idx = self
                    .workspaces
                    .get(&id)
                    .map(|v| v.len().saturating_sub(1))
                    .unwrap_or(0);
                self.active_workspace.insert(id, ws_idx.min(max_idx));
            }
        }

        // Restore focused monitor
        if let Some((&id, _)) = self
            .monitors
            .iter()
            .find(|(_, m)| m.device_name == snapshot.focused_monitor_name)
        {
            self.focused_monitor = id;
        }

        // Restore tab title overrides, pruning entries whose HWND is no
        // longer live. Guards against HWND reuse across daemon-offline
        // window closures: if the original window was destroyed while
        // the daemon was down, Windows can re-issue the same HWND to a
        // different process and the persisted override would silently
        // attach. The `IsWindow` check is cheap and catches the common
        // case; we don't bother with class/exe tagging.
        for (&hwnd, title) in &snapshot.tab_title_overrides {
            if leopardwm_platform_win32::is_valid_window(hwnd) {
                self.tab_title_overrides.insert(hwnd, title.clone());
            } else {
                debug!(
                    "Pruning stale tab title override for dead HWND {}: {:?}",
                    hwnd, title
                );
            }
        }

        restored_monitors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_writer_keeps_newer_prepared_snapshot_when_workers_finish_reordered() {
        let path = std::env::temp_dir().join(format!(
            "leopardwm-state-writer-order-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let writer = StateFileWriter::new(path.clone());
        let older = writer.prepare("older".to_string());
        let newer = writer.prepare("newer".to_string());

        assert_eq!(writer.write(&newer).unwrap(), StateWriteOutcome::Written);
        assert_eq!(
            writer.write(&older).unwrap(),
            StateWriteOutcome::SkippedStale
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer");

        std::fs::remove_file(path).unwrap();
    }
}
