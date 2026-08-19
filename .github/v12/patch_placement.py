from pathlib import Path

path = Path("crates/platform_win32/src/placement.rs")
text = path.read_text(encoding="utf-8")


def replace(old: str, new: str, count: int = 1) -> None:
    global text
    actual = text.count(old)
    if actual != count:
        raise RuntimeError(f"expected {count}, found {actual}: {old[:120]!r}")
    text = text.replace(old, new)


replace(
    "use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error};",
    "use crate::types::{AnimationPlacementPolicy, PlatformConfig, Win32Error, WindowRegionClipSpec};",
)

apply_marker = "pub fn apply_placements(\n"
apply_at = text.find(apply_marker)
if apply_at < 0:
    raise RuntimeError("apply_placements marker missing")
helpers = r'''fn clip_spec_for(
    specs: &[WindowRegionClipSpec],
    window_id: WindowId,
) -> Option<WindowRegionClipSpec> {
    specs
        .iter()
        .rev()
        .find(|spec| spec.window_id == window_id)
        .copied()
}

fn actual_outer_rect(hwnd: HWND, fallback: Rect) -> Rect {
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
        Rect::new(
            rect.left,
            rect.top,
            rect.right.saturating_sub(rect.left),
            rect.bottom.saturating_sub(rect.top),
        )
    } else {
        fallback
    }
}

fn prepare_region_clipped_placements(
    placements: &[WindowPlacement],
    specs: &[WindowRegionClipSpec],
) -> (
    Vec<WindowPlacement>,
    HashSet<WindowId>,
    HashSet<WindowId>,
) {
    let mut effective = Vec::with_capacity(placements.len());
    let mut clip_candidates = HashSet::with_capacity(specs.len());
    let mut managed = HashSet::with_capacity(specs.len());

    for placement in placements {
        let Some(spec) = clip_spec_for(specs, placement.window_id) else {
            let _ = crate::window_region::restore_window_region(placement.window_id, false);
            effective.push(placement.clone());
            continue;
        };
        if placement.visibility != Visibility::Visible || placement.column_index == usize::MAX {
            let _ = crate::window_region::restore_window_region(placement.window_id, false);
            effective.push(placement.clone());
            continue;
        }

        managed.insert(placement.window_id);
        if crate::window_region::can_clip_window_region(placement.window_id) {
            clip_candidates.insert(placement.window_id);
            effective.push(placement.clone());
        } else {
            let mut fallback = placement.clone();
            fallback.rect = spec.fallback_rect;
            fallback.visibility = spec.fallback_visibility;
            effective.push(fallback);
        }
    }

    (effective, clip_candidates, managed)
}

fn fallback_entry(
    original: &WindowPlacement,
    spec: WindowRegionClipSpec,
    safe: bool,
    high_contrast: bool,
) -> Option<(WindowPlacement, DeferEntry)> {
    let mut placement = original.clone();
    if safe {
        placement.rect = spec.safe_fallback_rect;
        placement.visibility = spec.safe_fallback_visibility;
    } else {
        placement.rect = spec.fallback_rect;
        placement.visibility = spec.fallback_visibility;
    }

    let hwnd = window_id_to_hwnd(placement.window_id).ok()?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
    }
    let (left, top, right, bottom) = if high_contrast {
        (0, 0, 0, 0)
    } else {
        invisible_border_insets(hwnd)
    };
    let frame_w = placement
        .rect
        .width
        .saturating_add(left)
        .saturating_add(right);
    let frame_h = placement
        .rect
        .height
        .saturating_add(top)
        .saturating_add(bottom);

    let (x, y, w, h, flags) = if placement.visibility == Visibility::Visible {
        (
            placement.rect.x.saturating_sub(left),
            placement.rect.y.saturating_sub(top),
            frame_w,
            frame_h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
    } else {
        let (x, y) = offscreen_position(&placement, left, top);
        (
            x,
            y,
            frame_w,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };

    Some((
        placement.clone(),
        DeferEntry {
            hwnd,
            window_id: placement.window_id,
            x,
            y,
            w,
            h,
            layout_rect: placement.rect,
            used_insets: (left, top, right, bottom),
            validate_insets: !high_contrast,
            visibility: placement.visibility,
            flags,
            column_index: placement.column_index,
        },
    ))
}

fn position_single_entry(entry: &DeferEntry) -> bool {
    unsafe {
        SetWindowPos(
            entry.hwnd,
            None,
            entry.x,
            entry.y,
            entry.w,
            entry.h,
            entry.flags,
        )
        .is_ok()
    }
}

fn preferred_fallback_is_contained(entry: &DeferEntry, clip_bounds: Rect) -> bool {
    if entry.visibility != Visibility::Visible {
        return true;
    }
    let actual = actual_outer_rect(
        entry.hwnd,
        Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1)),
    );
    let (left, _, right, _) = entry.used_insets;
    let allowed_left = clip_bounds.x.saturating_sub(left).saturating_sub(EDGE_EPSILON_PX);
    let allowed_right = clip_bounds
        .right()
        .saturating_add(right)
        .saturating_add(EDGE_EPSILON_PX);
    actual.x >= allowed_left && actual.right() <= allowed_right
}

fn upsert_entry(entries: &mut Vec<DeferEntry>, replacement: DeferEntry) {
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.window_id == replacement.window_id)
    {
        *entry = replacement;
    } else {
        entries.push(replacement);
    }
}

fn install_fallback(
    placement: &mut WindowPlacement,
    entries: &mut Vec<DeferEntry>,
    spec: WindowRegionClipSpec,
    high_contrast: bool,
) -> bool {
    for safe in [false, true] {
        let Some((candidate, entry)) = fallback_entry(placement, spec, safe, high_contrast) else {
            continue;
        };
        if !position_single_entry(&entry) {
            continue;
        }
        if !safe && !preferred_fallback_is_contained(&entry, spec.clip_bounds) {
            continue;
        }
        *placement = candidate;
        upsert_entry(entries, entry);
        return true;
    }
    // Final fail-safe: use the existing global off-screen move helper. This is
    // intentionally best-effort; a failure is returned to the daemon so it can
    // pause and run its established recovery path instead of caching bad state.
    if crate::move_window_offscreen(placement.window_id).is_ok() {
        placement.rect = spec.safe_fallback_rect;
        placement.visibility = spec.safe_fallback_visibility;
        return true;
    }
    false
}

fn reconcile_window_regions(
    placements: &mut [WindowPlacement],
    entries: &mut Vec<DeferEntry>,
    specs: &[WindowRegionClipSpec],
    clip_candidates: &HashSet<WindowId>,
    failed_window_ids: &mut HashSet<WindowId>,
    high_contrast: bool,
) -> Result<HashSet<WindowId>, Win32Error> {
    let mut active = HashSet::with_capacity(clip_candidates.len());
    let mut unrecoverable = Vec::new();

    for spec in specs.iter().copied() {
        let Some(placement) = placements
            .iter_mut()
            .find(|placement| placement.window_id == spec.window_id)
        else {
            continue;
        };

        if clip_candidates.contains(&spec.window_id)
            && !failed_window_ids.contains(&spec.window_id)
        {
            let outer = entries
                .iter()
                .find(|entry| entry.window_id == spec.window_id)
                .map(|entry| {
                    actual_outer_rect(
                        entry.hwnd,
                        Rect::new(entry.x, entry.y, entry.w.max(1), entry.h.max(1)),
                    )
                });
            if outer.is_some_and(|outer| {
                crate::window_region::apply_window_region_clip(
                    spec.window_id,
                    outer,
                    spec.clip_bounds,
                    false,
                )
            }) {
                active.insert(spec.window_id);
                continue;
            }
            let _ = crate::window_region::restore_window_region(spec.window_id, false);
        }

        if install_fallback(placement, entries, spec, high_contrast) {
            failed_window_ids.remove(&spec.window_id);
        } else {
            failed_window_ids.insert(spec.window_id);
            unrecoverable.push(spec.window_id);
        }
    }

    crate::window_region::restore_window_regions_not_in(&active, false);
    if unrecoverable.is_empty() {
        Ok(active)
    } else {
        Err(Win32Error::SetPositionFailed(format!(
            "failed to clip or safely isolate {} window(s): {:?}",
            unrecoverable.len(),
            unrecoverable
        )))
    }
}

'''
text = text[:apply_at] + helpers + text[apply_at:]

replace(
    """    if placements.is_empty() {
        if let Some(cache) = cache {
""",
    """    if placements.is_empty() {
        crate::window_region::restore_all_window_regions();
        if let Some(cache) = cache {
""",
)
replace(
    """    // Cache presence identifies an intermediate animation frame. The exact
    // landing pass has no cache and remains fully synchronous. Intermediate
""",
    """    let high_contrast = crate::is_high_contrast_enabled();
    let (mut effective_placements, clip_candidates, region_managed_ids) =
        prepare_region_clipped_placements(placements, &config.region_clips);
    if !clip_candidates.is_empty() {
        // A region is computed from the post-position HWND geometry. Force only
        // those HWNDs through the adaptive synchronous path so an async move
        // cannot leave the region one frame behind the window.
        if let Some(ref mut cache) = cache {
            for window_id in &clip_candidates {
                cache.compositor_sensitive.insert(*window_id, true);
            }
        }
    }
    let placements = effective_placements.as_mut_slice();

    // Cache presence identifies an intermediate animation frame. The exact
    // landing pass has no cache and remains fully synchronous. Intermediate
""",
)
replace(
    """    // In high contrast mode, DWM paints a visible border in the normally-invisible
    // frame area.  If we expand by the usual insets, adjacent windows' visible borders
    // overlap and the layout gaps disappear.  Zero the insets to keep correct spacing.
    let high_contrast = crate::is_high_contrast_enabled();

    let (entries, skipped) = build_defer_entries(
        placements,
        &mut cache,
        animation_frame,
        config.animation_placement_policy,
        high_contrast,
    );
""",
    """    // In high contrast mode, the pre-pass and entry builder both use zero
    // invisible-frame insets so region and position geometry agree.
    let effective_policy = if animation_frame && !clip_candidates.is_empty() {
        AnimationPlacementPolicy::AdaptiveCompositorSafe
    } else {
        config.animation_placement_policy
    };
    let (mut entries, skipped) = build_defer_entries(
        placements,
        &mut cache,
        animation_frame,
        effective_policy,
        high_contrast,
    );
""",
)
replace(
    "    let (applied, failed_window_ids) = position_entries(&entries);\n",
    """    let (mut applied, mut failed_window_ids) = position_entries(&entries);
    let active_region_ids = match reconcile_window_regions(
        placements,
        &mut entries,
        &config.region_clips,
        &clip_candidates,
        &mut failed_window_ids,
        high_contrast,
    ) {
        Ok(active) => active,
        Err(error) => {
            crate::window_region::restore_window_regions_not_in(&HashSet::new(), true);
            return Err(error);
        }
    };
    applied = applied.saturating_add(
        entries
            .iter()
            .filter(|entry| !failed_window_ids.contains(&entry.window_id))
            .count() as u32
            .saturating_sub(applied),
    );
""",
)
replace(
    """        detect_size_violations(&entries, &failed_window_ids, &mut cache)
""",
    """        detect_size_violations(
            &entries,
            &failed_window_ids,
            &region_managed_ids,
            &mut cache,
        )
""",
)
replace(
    """                e.visibility == Visibility::Visible
                    && e.w > 1
""",
    """                e.visibility == Visibility::Visible
                    && !region_managed_ids.contains(&e.window_id)
                    && e.w > 1
""",
)
replace(
    """fn detect_size_violations(
    entries: &[DeferEntry],
    failed_window_ids: &HashSet<u64>,
    cache: &mut Option<&mut PlacementCache>,
""",
    """fn detect_size_violations(
    entries: &[DeferEntry],
    failed_window_ids: &HashSet<u64>,
    region_managed_ids: &HashSet<u64>,
    cache: &mut Option<&mut PlacementCache>,
""",
)
replace(
    """            || failed_window_ids.contains(&entry.window_id)
        {
""",
    """            || failed_window_ids.contains(&entry.window_id)
            || region_managed_ids.contains(&entry.window_id)
        {
""",
)
replace(
    """pub fn clear_suspected_oversize(window_id: WindowId) {
    let mut guard = lock_suspected_oversize();
""",
    """pub fn clear_suspected_oversize(window_id: WindowId) {
    crate::window_region::forget_window_region(window_id);
    let mut guard = lock_suspected_oversize();
""",
)
replace(
    """pub fn dwm_uncloak_all() {
    let _commit = lock_cloak_commit();
""",
    """pub fn dwm_uncloak_all() {
    crate::window_region::restore_all_window_regions();
    let _commit = lock_cloak_commit();
""",
)
replace(
    """fn uncloak_all_tracked() {
    let _commit = lock_cloak_commit();
""",
    """fn uncloak_all_tracked() {
    crate::window_region::restore_all_window_regions();
    let _commit = lock_cloak_commit();
""",
)

path.write_text(text, encoding="utf-8", newline="\n")
print("placement pipeline patched for post-position region ownership and verified fallback")
