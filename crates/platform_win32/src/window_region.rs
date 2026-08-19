//! Owned `SetWindowRgn` clipping for tiled windows that cross monitor bounds.
//!
//! LeopardWM intersects its monitor clip with an application's existing region
//! instead of hiding that window. The original region is retained in memory and,
//! for custom-shaped windows, persisted once so a restarted daemon can restore
//! it after an abnormal exit.

use crate::{recover_poisoned_mutex, window_id_to_hwnd};
use leopardwm_core_layout::{Rect, Visibility, WindowId};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, EqualRgn, ExtCreateRegion, GetRegionData,
    GetWindowRgn, SetWindowRgn, HGDIOBJ, HRGN, RGNDATA, RGNDATAHEADER, RGN_AND, RGN_COPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetPropW, GetWindowThreadProcessId, IsWindow, RemovePropW, SetPropW,
};

const ERROR_REGION_KIND: i32 = 0;
const NULL_REGION_KIND: i32 = 1;
const OWNER_MAGIC_V3: usize = 0x4c57_4d33; // "LWM3"
const OWNER_MAGIC_V2: usize = 0x4c57_4d32; // "LWM2"
const MAX_REGION_DATA_BYTES: usize = 4 * 1024 * 1024;
const BACKUP_MAGIC: &[u8; 8] = b"LWMRGN3\0";
const BACKUP_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRegionClip {
    pub window_id: WindowId,
    /// Screen-coordinate output rectangle that the HWND may paint into.
    pub clip_bounds: Rect,
    /// Safe placement used only when GDI region capture or application fails.
    pub fallback_rect: Rect,
    pub fallback_visibility: Visibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionClipResult {
    Applied,
    Unchanged,
    Unsupported,
    Failed,
}

impl RegionClipResult {
    pub(crate) fn succeeded(self) -> bool {
        matches!(self, Self::Applied | Self::Unchanged)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowIdentity {
    process_id: u32,
    thread_id: u32,
    class_name: String,
}

#[derive(Debug)]
struct OwnedRegion(usize);

impl OwnedRegion {
    fn from_handle(region: HRGN) -> Option<Self> {
        (!region.0.is_null()).then_some(Self(region.0 as usize))
    }

    fn as_handle(&self) -> HRGN {
        HRGN(self.0 as *mut c_void)
    }

    fn release(mut self) -> HRGN {
        let region = self.as_handle();
        self.0 = 0;
        region
    }
}

impl Drop for OwnedRegion {
    fn drop(&mut self) {
        if self.0 == 0 {
            return;
        }
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.0 as *mut c_void));
        }
    }
}

#[derive(Debug, Clone)]
struct RegionSnapshot {
    words: Vec<u32>,
    len: u32,
}

impl RegionSnapshot {
    fn capture(region: &OwnedRegion) -> Option<Self> {
        let len = unsafe { GetRegionData(region.as_handle(), 0, None) };
        if len == 0 || len as usize > MAX_REGION_DATA_BYTES {
            return None;
        }

        let mut words = vec![0u32; (len as usize).div_ceil(std::mem::size_of::<u32>())];
        let written = unsafe {
            GetRegionData(
                region.as_handle(),
                len,
                Some(words.as_mut_ptr().cast::<RGNDATA>()),
            )
        };
        (written == len).then_some(Self { words, len })
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < std::mem::size_of::<RGNDATAHEADER>()
            || bytes.len() > MAX_REGION_DATA_BYTES
        {
            return None;
        }
        let mut words = vec![0u32; bytes.len().div_ceil(std::mem::size_of::<u32>())];
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                words.as_mut_ptr().cast::<u8>(),
                bytes.len(),
            );
        }
        Some(Self {
            words,
            len: bytes.len() as u32,
        })
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.len as usize)
        }
    }

    fn to_region(&self) -> Option<OwnedRegion> {
        let region = unsafe {
            ExtCreateRegion(
                None,
                self.len,
                self.words.as_ptr().cast::<RGNDATA>(),
            )
        };
        OwnedRegion::from_handle(region)
    }
}

#[derive(Debug)]
enum BaseRegion {
    None,
    Custom {
        template: OwnedRegion,
        snapshot: RegionSnapshot,
    },
}

impl BaseRegion {
    fn capture(hwnd: HWND) -> Result<Self, ()> {
        let region = create_empty_region().ok_or(())?;
        let kind = unsafe { GetWindowRgn(hwnd, region.as_handle()) }.0;
        match kind {
            ERROR_REGION_KIND => Err(()),
            NULL_REGION_KIND => Ok(Self::None),
            _ => {
                let snapshot = RegionSnapshot::capture(&region).ok_or(())?;
                Ok(Self::Custom {
                    template: region,
                    snapshot,
                })
            }
        }
    }

    fn from_snapshot(snapshot: RegionSnapshot) -> Option<Self> {
        let template = snapshot.to_region()?;
        Some(Self::Custom { template, snapshot })
    }

    fn expected_region(&self, clip: Rect) -> Option<OwnedRegion> {
        match self {
            Self::None => create_rect_region(clip),
            Self::Custom { template, .. } => intersect_region(template, clip),
        }
    }

    fn restore(&self, hwnd: HWND, redraw: bool) -> bool {
        match self {
            Self::None => clear_region(hwnd, redraw),
            Self::Custom { template, .. } => {
                let Some(region) = clone_region(template) else {
                    return false;
                };
                set_owned_region(hwnd, region, redraw)
            }
        }
    }

    fn snapshot(&self) -> Option<&RegionSnapshot> {
        match self {
            Self::None => None,
            Self::Custom { snapshot, .. } => Some(snapshot),
        }
    }
}

#[derive(Debug)]
struct RegionState {
    identity: WindowIdentity,
    base: BaseRegion,
    expected: Option<OwnedRegion>,
    expected_clip: Option<Rect>,
    backup_token: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct RegionMetadata {
    clip: Rect,
    backup_token: Option<u64>,
}

static REGION_STATES: OnceLock<Mutex<HashMap<WindowId, RegionState>>> = OnceLock::new();
static REGION_COMMIT: Mutex<()> = Mutex::new(());
static NEXT_BACKUP_TOKEN: AtomicU64 = AtomicU64::new(1);

fn states() -> &'static Mutex<HashMap<WindowId, RegionState>> {
    REGION_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_states() -> std::sync::MutexGuard<'static, HashMap<WindowId, RegionState>> {
    states().lock().unwrap_or_else(recover_poisoned_mutex)
}

fn lock_commit() -> std::sync::MutexGuard<'static, ()> {
    REGION_COMMIT.lock().unwrap_or_else(recover_poisoned_mutex)
}

fn identity(window_id: WindowId) -> Option<WindowIdentity> {
    let hwnd = window_id_to_hwnd(window_id).ok()?;
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return None;
    }

    let mut process_id = 0u32;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if thread_id == 0 || process_id == 0 {
        return None;
    }

    let mut class = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut class) };
    if len <= 0 {
        return None;
    }

    Some(WindowIdentity {
        process_id,
        thread_id,
        class_name: String::from_utf16_lossy(&class[..len as usize]),
    })
}

fn create_rect_region(rect: Rect) -> Option<OwnedRegion> {
    let region = unsafe { CreateRectRgn(rect.x, rect.y, rect.right(), rect.bottom()) };
    OwnedRegion::from_handle(region)
}

fn create_empty_region() -> Option<OwnedRegion> {
    create_rect_region(Rect::new(0, 0, 0, 0))
}

fn clone_region(source: &OwnedRegion) -> Option<OwnedRegion> {
    let destination = create_empty_region()?;
    let kind = unsafe {
        CombineRgn(
            Some(destination.as_handle()),
            Some(source.as_handle()),
            Some(source.as_handle()),
            RGN_COPY,
        )
    }
    .0;
    (kind != ERROR_REGION_KIND).then_some(destination)
}

fn intersect_region(source: &OwnedRegion, clip: Rect) -> Option<OwnedRegion> {
    let clip_region = create_rect_region(clip)?;
    let destination = create_empty_region()?;
    let kind = unsafe {
        CombineRgn(
            Some(destination.as_handle()),
            Some(source.as_handle()),
            Some(clip_region.as_handle()),
            RGN_AND,
        )
    }
    .0;
    (kind > NULL_REGION_KIND).then_some(destination)
}

fn current_region_matches(hwnd: HWND, expected: &OwnedRegion) -> Result<bool, ()> {
    let actual = create_empty_region().ok_or(())?;
    let kind = unsafe { GetWindowRgn(hwnd, actual.as_handle()) }.0;
    if kind == ERROR_REGION_KIND {
        return Err(());
    }
    if kind == NULL_REGION_KIND {
        return Ok(false);
    }
    Ok(unsafe { EqualRgn(actual.as_handle(), expected.as_handle()).as_bool() })
}

fn set_owned_region(hwnd: HWND, region: OwnedRegion, redraw: bool) -> bool {
    let handle = region.as_handle();
    if unsafe { SetWindowRgn(hwnd, Some(handle), redraw) } == 0 {
        return false;
    }
    let _ = region.release();
    true
}

fn clear_region(hwnd: HWND, redraw: bool) -> bool {
    unsafe { SetWindowRgn(hwnd, None, redraw) != 0 }
}

fn handle_from_usize(value: usize) -> HANDLE {
    HANDLE(value as *mut c_void)
}

fn usize_from_handle(value: HANDLE) -> usize {
    value.0 as usize
}

fn encode_coordinate(value: i32) -> HANDLE {
    let biased = (i64::from(value) - i64::from(i32::MIN) + 1) as u64;
    handle_from_usize(biased as usize)
}

fn decode_coordinate(value: HANDLE) -> Option<i32> {
    let raw = usize_from_handle(value) as u64;
    if raw == 0 || raw > u64::from(u32::MAX) + 1 {
        return None;
    }
    let decoded = raw as i64 - 1 + i64::from(i32::MIN);
    i32::try_from(decoded).ok()
}

fn has_owner_marker_v3(hwnd: HWND) -> bool {
    usize_from_handle(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Owner")) })
        == OWNER_MAGIC_V3
}

fn remove_metadata_v3(hwnd: HWND) {
    unsafe {
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Owner"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Left"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Top"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Right"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Bottom"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Backup"));
    }
}

fn write_metadata_v3(hwnd: HWND, metadata: RegionMetadata) -> bool {
    unsafe {
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Owner"));
    }

    let payload_ok = unsafe {
        SetPropW(
            hwnd,
            w!("LeopardWM.RegionClip.v3.Left"),
            Some(encode_coordinate(metadata.clip.x)),
        )
        .is_ok()
            && SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v3.Top"),
                Some(encode_coordinate(metadata.clip.y)),
            )
            .is_ok()
            && SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v3.Right"),
                Some(encode_coordinate(metadata.clip.right())),
            )
            .is_ok()
            && SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v3.Bottom"),
                Some(encode_coordinate(metadata.clip.bottom())),
            )
            .is_ok()
    };
    if !payload_ok {
        remove_metadata_v3(hwnd);
        return false;
    }

    unsafe {
        if let Some(token) = metadata.backup_token {
            if SetPropW(
                hwnd,
                w!("LeopardWM.RegionClip.v3.Backup"),
                Some(handle_from_usize(token as usize)),
            )
            .is_err()
            {
                remove_metadata_v3(hwnd);
                return false;
            }
        } else {
            let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v3.Backup"));
        }

        if SetPropW(
            hwnd,
            w!("LeopardWM.RegionClip.v3.Owner"),
            Some(handle_from_usize(OWNER_MAGIC_V3)),
        )
        .is_err()
        {
            remove_metadata_v3(hwnd);
            return false;
        }
    }
    true
}

fn read_metadata_v3(hwnd: HWND) -> Option<RegionMetadata> {
    if !has_owner_marker_v3(hwnd) {
        return None;
    }
    let left = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Left")) })?;
    let top = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Top")) })?;
    let right =
        decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Right")) })?;
    let bottom =
        decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Bottom")) })?;
    if right <= left || bottom <= top {
        return None;
    }
    let token =
        usize_from_handle(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v3.Backup")) }) as u64;
    Some(RegionMetadata {
        clip: Rect::new(left, top, right - left, bottom - top),
        backup_token: (token != 0).then_some(token),
    })
}

fn metadata_matches(hwnd: HWND, metadata: RegionMetadata) -> bool {
    read_metadata_v3(hwnd).is_some_and(|current| {
        current.clip == metadata.clip && current.backup_token == metadata.backup_token
    })
}

fn class_hash(class_name: &str) -> u64 {
    class_name.as_bytes().iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

fn next_backup_token() -> u64 {
    let counter = NEXT_BACKUP_TOKEN.fetch_add(1, Ordering::Relaxed);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    let token = time.rotate_left(17)
        ^ counter.rotate_left(7)
        ^ (u64::from(std::process::id()) << 32);
    token.max(1)
}

fn backup_path(window_id: WindowId, token: u64) -> PathBuf {
    std::env::temp_dir()
        .join("LeopardWM")
        .join("region-recovery-v3")
        .join(format!("{window_id:016x}-{token:016x}.bin"))
}

fn encode_backup(identity: &WindowIdentity, snapshot: &RegionSnapshot) -> Vec<u8> {
    let data = snapshot.as_bytes();
    let mut encoded = Vec::with_capacity(40 + data.len());
    encoded.extend_from_slice(BACKUP_MAGIC);
    encoded.extend_from_slice(&BACKUP_VERSION.to_le_bytes());
    encoded.extend_from_slice(&identity.process_id.to_le_bytes());
    encoded.extend_from_slice(&identity.thread_id.to_le_bytes());
    encoded.extend_from_slice(&class_hash(&identity.class_name).to_le_bytes());
    encoded.extend_from_slice(&(data.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&checksum(data).to_le_bytes());
    encoded.extend_from_slice(data);
    encoded
}

fn decode_backup(identity: &WindowIdentity, encoded: &[u8]) -> Option<RegionSnapshot> {
    const HEADER_LEN: usize = 40;
    if encoded.len() < HEADER_LEN || &encoded[..8] != BACKUP_MAGIC {
        return None;
    }
    let read_u32 = |offset: usize| -> Option<u32> {
        let bytes: [u8; 4] = encoded.get(offset..offset + 4)?.try_into().ok()?;
        Some(u32::from_le_bytes(bytes))
    };
    let read_u64 = |offset: usize| -> Option<u64> {
        let bytes: [u8; 8] = encoded.get(offset..offset + 8)?.try_into().ok()?;
        Some(u64::from_le_bytes(bytes))
    };

    let version: u32 = read_u32(8)?;
    let process_id: u32 = read_u32(12)?;
    let thread_id: u32 = read_u32(16)?;
    let stored_class_hash: u64 = read_u64(20)?;
    let data_len = read_u32(28)? as usize;
    let stored_checksum: u64 = read_u64(32)?;
    if version != BACKUP_VERSION
        || process_id != identity.process_id
        || thread_id != identity.thread_id
        || stored_class_hash != class_hash(&identity.class_name)
        || data_len > MAX_REGION_DATA_BYTES
        || encoded.len() != HEADER_LEN + data_len
    {
        return None;
    }
    let data = &encoded[HEADER_LEN..];
    (checksum(data) == stored_checksum)
        .then(|| RegionSnapshot::from_bytes(data))
        .flatten()
}

fn write_backup(
    window_id: WindowId,
    token: u64,
    identity: &WindowIdentity,
    snapshot: &RegionSnapshot,
) -> io::Result<()> {
    let path = backup_path(window_id, token);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, encode_backup(identity, snapshot))?;
    match fs::rename(&temporary, &path) {
        Ok(()) => Ok(()),
        Err(error) if path.exists() => {
            let _ = fs::remove_file(&temporary);
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn read_backup(
    window_id: WindowId,
    token: u64,
    identity: &WindowIdentity,
) -> Option<RegionSnapshot> {
    let encoded = fs::read(backup_path(window_id, token)).ok()?;
    decode_backup(identity, &encoded)
}

fn delete_backup(window_id: WindowId, token: Option<u64>) {
    if let Some(token) = token {
        let _ = fs::remove_file(backup_path(window_id, token));
    }
}

fn ensure_backup(window_id: WindowId, state: &mut RegionState) -> bool {
    let Some(snapshot) = state.base.snapshot() else {
        state.backup_token = None;
        return true;
    };
    if state.backup_token.is_some() {
        return true;
    }

    let token = next_backup_token();
    if write_backup(window_id, token, &state.identity, snapshot).is_err() {
        return false;
    }
    state.backup_token = Some(token);
    true
}

fn cleanup_state(window_id: WindowId, state: &RegionState) {
    delete_backup(window_id, state.backup_token);
}

fn remove_legacy_v2_metadata(hwnd: HWND) {
    unsafe {
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Owner"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Left"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Top"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Right"));
        let _ = RemovePropW(hwnd, w!("LeopardWM.RegionClip.v2.Bottom"));
    }
}

fn recover_legacy_v2(hwnd: HWND, redraw: bool) -> bool {
    let owner =
        usize_from_handle(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Owner")) });
    if owner != OWNER_MAGIC_V2 {
        return true;
    }

    let expected = (|| {
        let left =
            decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Left")) })?;
        let top = decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Top")) })?;
        let right =
            decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Right")) })?;
        let bottom =
            decode_coordinate(unsafe { GetPropW(hwnd, w!("LeopardWM.RegionClip.v2.Bottom")) })?;
        (right > left && bottom > top).then_some(Rect::new(left, top, right - left, bottom - top))
    })();

    let recovered = expected
        .and_then(create_rect_region)
        .is_none_or(|region| {
            current_region_matches(hwnd, &region).map_or(false, |matches| {
                !matches || clear_region(hwnd, redraw)
            })
        });
    if recovered {
        remove_legacy_v2_metadata(hwnd);
    }
    recovered
}

fn recover_stale_metadata(
    hwnd: HWND,
    window_id: WindowId,
    identity: &WindowIdentity,
    redraw: bool,
) -> bool {
    if !recover_legacy_v2(hwnd, redraw) {
        return false;
    }

    if !has_owner_marker_v3(hwnd) {
        remove_metadata_v3(hwnd);
        return true;
    }
    let Some(metadata) = read_metadata_v3(hwnd) else {
        return false;
    };

    let recovered = if let Some(token) = metadata.backup_token {
        let Some(snapshot) = read_backup(window_id, token, identity) else {
            warn!(
                window_id,
                "Cannot recover a stale custom window region: backup is missing or invalid"
            );
            return false;
        };
        let Some(base) = BaseRegion::from_snapshot(snapshot) else {
            return false;
        };
        let Some(expected) = base.expected_region(metadata.clip) else {
            return false;
        };
        match current_region_matches(hwnd, &expected) {
            Ok(true) => base.restore(hwnd, redraw),
            Ok(false) => true, // The application replaced the stale region.
            Err(()) => false,
        }
    } else {
        let Some(expected) = create_rect_region(metadata.clip) else {
            return false;
        };
        match current_region_matches(hwnd, &expected) {
            Ok(true) => clear_region(hwnd, redraw),
            Ok(false) => true,
            Err(()) => false,
        }
    };

    if recovered {
        remove_metadata_v3(hwnd);
        delete_backup(window_id, metadata.backup_token);
    }
    recovered
}

/// Compute the HWND-local rectangle that exposes only the part of the visible
/// DWM frame inside `clip_bounds`.
pub(crate) fn relative_clip_region(
    outer_rect: Rect,
    visible_rect: Rect,
    clip_bounds: Rect,
) -> Option<Rect> {
    let intersection_left = visible_rect.x.max(clip_bounds.x);
    let intersection_top = visible_rect.y.max(clip_bounds.y);
    let intersection_right = visible_rect.right().min(clip_bounds.right());
    let intersection_bottom = visible_rect.bottom().min(clip_bounds.bottom());
    if intersection_right <= intersection_left || intersection_bottom <= intersection_top {
        return None;
    }

    let outer_width = outer_rect.width.max(1);
    let outer_height = outer_rect.height.max(1);
    let left = if visible_rect.x >= clip_bounds.x {
        0
    } else {
        intersection_left
            .saturating_sub(outer_rect.x)
            .clamp(0, outer_width)
    };
    let top = if visible_rect.y >= clip_bounds.y {
        0
    } else {
        intersection_top
            .saturating_sub(outer_rect.y)
            .clamp(0, outer_height)
    };
    let right = if visible_rect.right() <= clip_bounds.right() {
        outer_width
    } else {
        intersection_right
            .saturating_sub(outer_rect.x)
            .clamp(left, outer_width)
    };
    let bottom = if visible_rect.bottom() <= clip_bounds.bottom() {
        outer_height
    } else {
        intersection_bottom
            .saturating_sub(outer_rect.y)
            .clamp(top, outer_height)
    };
    (right > left && bottom > top).then_some(Rect::new(left, top, right - left, bottom - top))
}

/// Preflight the region path. Application-owned simple and complex regions are
/// supported; only an invalid HWND, a failed region query, or unrecoverable
/// stale metadata forces the whole-window fallback.
pub(crate) fn can_clip_window_region(window_id: WindowId) -> bool {
    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        if let Some(state) = lock_states().remove(&window_id) {
            cleanup_state(window_id, &state);
        }
        return false;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return false;
    };

    if let Some(state) = lock_states().get(&window_id) {
        if state.identity == current_identity {
            return true;
        }
    }
    if let Some(state) = lock_states().remove(&window_id) {
        cleanup_state(window_id, &state);
    }

    recover_stale_metadata(hwnd, window_id, &current_identity, false)
        && BaseRegion::capture(hwnd).is_ok()
}

fn metadata_for(state: &RegionState, clip: Rect) -> RegionMetadata {
    RegionMetadata {
        clip,
        backup_token: state.backup_token,
    }
}

fn rollback_or_discard(
    hwnd: HWND,
    window_id: WindowId,
    state: RegionState,
) -> RegionClipResult {
    if let Some(old_clip) = state.expected_clip {
        let _ = write_metadata_v3(hwnd, metadata_for(&state, old_clip));
        lock_states().insert(window_id, state);
    } else {
        remove_metadata_v3(hwnd);
        cleanup_state(window_id, &state);
    }
    RegionClipResult::Failed
}

/// Install or update a LeopardWM region. An existing application region is
/// intersected with the monitor clip and restored when clipping ends.
pub(crate) fn apply_window_region_clip(
    window_id: WindowId,
    outer_rect: Rect,
    visible_rect: Rect,
    clip_bounds: Rect,
    redraw: bool,
) -> RegionClipResult {
    let Some(clip) = relative_clip_region(outer_rect, visible_rect, clip_bounds) else {
        return RegionClipResult::Unsupported;
    };

    let _commit = lock_commit();
    let Some(current_identity) = identity(window_id) else {
        if let Some(state) = lock_states().remove(&window_id) {
            cleanup_state(window_id, &state);
        }
        return RegionClipResult::Failed;
    };
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return RegionClipResult::Failed;
    };

    let mut state = match lock_states().remove(&window_id) {
        Some(state) if state.identity == current_identity => {
            match state
                .expected
                .as_ref()
                .map(|expected| current_region_matches(hwnd, expected))
            {
                Some(Ok(true)) => state,
                Some(Err(())) => {
                    lock_states().insert(window_id, state);
                    return RegionClipResult::Failed;
                }
                _ => {
                    remove_metadata_v3(hwnd);
                    cleanup_state(window_id, &state);
                    let Ok(base) = BaseRegion::capture(hwnd) else {
                        return RegionClipResult::Failed;
                    };
                    RegionState {
                        identity: current_identity.clone(),
                        base,
                        expected: None,
                        expected_clip: None,
                        backup_token: None,
                    }
                }
            }
        }
        Some(state) => {
            cleanup_state(window_id, &state);
            remove_metadata_v3(hwnd);
            let Ok(base) = BaseRegion::capture(hwnd) else {
                return RegionClipResult::Failed;
            };
            RegionState {
                identity: current_identity.clone(),
                base,
                expected: None,
                expected_clip: None,
                backup_token: None,
            }
        }
        None => {
            if !recover_stale_metadata(hwnd, window_id, &current_identity, false) {
                return RegionClipResult::Failed;
            }
            let Ok(base) = BaseRegion::capture(hwnd) else {
                return RegionClipResult::Failed;
            };
            RegionState {
                identity: current_identity,
                base,
                expected: None,
                expected_clip: None,
                backup_token: None,
            }
        }
    };

    if !ensure_backup(window_id, &mut state) {
        return rollback_or_discard(hwnd, window_id, state);
    }
    let metadata = metadata_for(&state, clip);

    if state.expected_clip == Some(clip)
        && state
            .expected
            .as_ref()
            .is_some_and(|expected| current_region_matches(hwnd, expected) == Ok(true))
    {
        if !metadata_matches(hwnd, metadata) && !write_metadata_v3(hwnd, metadata) {
            return rollback_or_discard(hwnd, window_id, state);
        }
        lock_states().insert(window_id, state);
        return RegionClipResult::Unchanged;
    }

    let Some(expected) = state.base.expected_region(clip) else {
        if state.expected.is_some() {
            lock_states().insert(window_id, state);
        } else {
            cleanup_state(window_id, &state);
        }
        return RegionClipResult::Unsupported;
    };
    let Some(transfer) = clone_region(&expected) else {
        return rollback_or_discard(hwnd, window_id, state);
    };
    if !write_metadata_v3(hwnd, metadata) {
        return rollback_or_discard(hwnd, window_id, state);
    }
    if !set_owned_region(hwnd, transfer, redraw) {
        return rollback_or_discard(hwnd, window_id, state);
    }

    state.expected = Some(expected);
    state.expected_clip = Some(clip);
    lock_states().insert(window_id, state);
    RegionClipResult::Applied
}

/// Restore the application's exact original region when LeopardWM still owns
/// the live shape. If the application replaced it, leave that replacement
/// untouched and relinquish ownership.
pub(crate) fn restore_window_region(window_id: WindowId, redraw: bool) -> bool {
    let _commit = lock_commit();
    let state = lock_states().remove(&window_id);
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        if let Some(state) = state {
            cleanup_state(window_id, &state);
        }
        return true;
    };
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        if let Some(state) = state {
            cleanup_state(window_id, &state);
        }
        return true;
    }

    let Some(state) = state else {
        let Some(current_identity) = identity(window_id) else {
            return true;
        };
        return recover_stale_metadata(hwnd, window_id, &current_identity, redraw);
    };
    if identity(window_id).as_ref() != Some(&state.identity) {
        cleanup_state(window_id, &state);
        return true;
    }

    let still_owned = match state
        .expected
        .as_ref()
        .map(|expected| current_region_matches(hwnd, expected))
    {
        Some(Ok(matches)) => matches,
        Some(Err(())) => {
            lock_states().insert(window_id, state);
            return false;
        }
        None => false,
    };
    if still_owned && !state.base.restore(hwnd, redraw) {
        lock_states().insert(window_id, state);
        return false;
    }

    remove_metadata_v3(hwnd);
    cleanup_state(window_id, &state);
    true
}

pub(crate) fn reconcile_window_regions(
    managed_window_ids: &HashSet<WindowId>,
    clipped_window_ids: &HashSet<WindowId>,
    redraw: bool,
) {
    for window_id in managed_window_ids.difference(clipped_window_ids) {
        let _ = restore_window_region(*window_id, redraw);
    }
    let stale: Vec<WindowId> = lock_states()
        .keys()
        .filter(|window_id| !managed_window_ids.contains(window_id))
        .copied()
        .collect();
    for window_id in stale {
        let _ = restore_window_region(window_id, redraw);
    }
}

pub fn restore_all_window_regions() {
    let window_ids: Vec<WindowId> = lock_states().keys().copied().collect();
    for window_id in window_ids {
        let _ = restore_window_region(window_id, true);
    }
}

pub fn forget_window_region(window_id: WindowId) {
    if let Some(state) = lock_states().remove(&window_id) {
        cleanup_state(window_id, &state);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_window_region_clip, backup_path, can_clip_window_region, clone_region,
        create_empty_region, create_rect_region, current_region_matches, decode_backup,
        encode_backup, encode_coordinate, intersect_region, lock_states, relative_clip_region,
        restore_window_region, set_owned_region, RegionSnapshot,
    };
    use leopardwm_core_layout::Rect;
    use std::sync::OnceLock;
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{CombineRgn, RGN_OR};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, WNDCLASSEXW,
        WINDOW_EX_STYLE, WS_OVERLAPPED,
    };

    unsafe extern "system" fn test_wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn test_window() -> HWND {
        static REGISTERED: OnceLock<()> = OnceLock::new();
        let instance = unsafe { GetModuleHandleW(None).unwrap() };
        REGISTERED.get_or_init(|| {
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(test_wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("LeopardWMRegionTest"),
                ..Default::default()
            };
            unsafe {
                RegisterClassExW(&class);
            }
        });
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("LeopardWMRegionTest"),
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                1000,
                800,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap()
        }
    }

    fn window_id(hwnd: HWND) -> u64 {
        hwnd.0 as usize as u64
    }

    struct TestWindow(HWND);

    impl TestWindow {
        fn new() -> Self {
            Self(test_window())
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = restore_window_region(window_id(self.0), false);
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    #[test]
    fn coordinate_property_encoding_round_trips_extremes() {
        for value in [i32::MIN, -100_000, -1, 0, 1, 100_000, i32::MAX] {
            assert_eq!(super::decode_coordinate(encode_coordinate(value)), Some(value));
        }
    }

    #[test]
    fn right_edge_clip_preserves_the_unclipped_outer_frame() {
        let region = relative_clip_region(
            Rect::new(1792, 90, 616, 916),
            Rect::new(1800, 100, 600, 900),
            Rect::new(0, 0, 1920, 1080),
        )
        .unwrap();
        assert_eq!(region, Rect::new(0, 0, 128, 916));
    }

    #[test]
    fn left_edge_clip_preserves_the_unclipped_outer_frame() {
        let region = relative_clip_region(
            Rect::new(-208, 90, 616, 916),
            Rect::new(-200, 100, 600, 900),
            Rect::new(0, 0, 1920, 1080),
        )
        .unwrap();
        assert_eq!(region, Rect::new(208, 0, 408, 916));
    }

    #[test]
    fn clips_vertical_neighbors_and_negative_virtual_coordinates() {
        let region = relative_clip_region(
            Rect::new(-1930, -110, 820, 1200),
            Rect::new(-1920, -100, 800, 1180),
            Rect::new(-1920, 0, 1920, 1080),
        )
        .unwrap();
        assert_eq!(region, Rect::new(0, 110, 820, 1090));
    }

    #[test]
    fn rejects_a_window_with_no_visible_intersection() {
        assert!(relative_clip_region(
            Rect::new(2100, 0, 400, 800),
            Rect::new(2100, 0, 400, 800),
            Rect::new(0, 0, 1920, 1080),
        )
        .is_none());
    }

    #[test]
    fn symmetric_neighbor_previews_are_preserved() {
        for (column_width, expected_preview) in [(1000, 500), (1500, 250)] {
            let center_x = (2000 - column_width) / 2;
            let left = relative_clip_region(
                Rect::new(center_x - column_width, 0, column_width, 800),
                Rect::new(center_x - column_width, 0, column_width, 800),
                Rect::new(0, 0, 2000, 1000),
            )
            .unwrap();
            let right = relative_clip_region(
                Rect::new(center_x + column_width, 0, column_width, 800),
                Rect::new(center_x + column_width, 0, column_width, 800),
                Rect::new(0, 0, 2000, 1000),
            )
            .unwrap();
            assert_eq!(left.width, expected_preview);
            assert_eq!(right.width, expected_preview);
        }
    }

    #[test]
    fn custom_region_intersection_preserves_complex_shape() {
        let left = create_rect_region(Rect::new(0, 0, 400, 800)).unwrap();
        let right = create_rect_region(Rect::new(600, 0, 400, 800)).unwrap();
        let base = create_empty_region().unwrap();
        let kind = unsafe {
            CombineRgn(
                Some(base.as_handle()),
                Some(left.as_handle()),
                Some(right.as_handle()),
                RGN_OR,
            )
        };
        assert!(kind.0 > 1);

        let clipped = intersect_region(&base, Rect::new(500, 0, 500, 800)).unwrap();
        let expected = create_rect_region(Rect::new(600, 0, 400, 800)).unwrap();
        assert!(unsafe {
            windows::Win32::Graphics::Gdi::EqualRgn(
                clipped.as_handle(),
                expected.as_handle(),
            )
            .as_bool()
        });
    }

    #[test]
    fn region_snapshot_and_backup_round_trip() {
        let region = create_rect_region(Rect::new(10, 20, 900, 700)).unwrap();
        let snapshot = RegionSnapshot::capture(&region).unwrap();
        let identity = super::WindowIdentity {
            process_id: 42,
            thread_id: 7,
            class_name: "SnapshotTest".to_string(),
        };
        let encoded = encode_backup(&identity, &snapshot);
        let decoded = decode_backup(&identity, &encoded).unwrap();
        let recreated = decoded.to_region().unwrap();
        assert!(unsafe {
            windows::Win32::Graphics::Gdi::EqualRgn(
                region.as_handle(),
                recreated.as_handle(),
            )
            .as_bool()
        });
    }

    #[test]
    fn application_custom_region_is_clipped_and_restored() {
        let window = TestWindow::new();
        let id = window_id(window.0);
        let original = create_rect_region(Rect::new(40, 20, 920, 740)).unwrap();
        assert!(set_owned_region(window.0, clone_region(&original).unwrap(), false));

        assert!(can_clip_window_region(id));
        let result = apply_window_region_clip(
            id,
            Rect::new(-500, 0, 1000, 800),
            Rect::new(-500, 0, 1000, 800),
            Rect::new(0, 0, 2000, 1000),
            false,
        );
        assert!(result.succeeded());
        let expected = create_rect_region(Rect::new(500, 20, 460, 740)).unwrap();
        assert_eq!(current_region_matches(window.0, &expected), Ok(true));

        assert!(restore_window_region(id, false));
        assert_eq!(current_region_matches(window.0, &original), Ok(true));
    }

    #[test]
    fn application_region_takeover_is_not_overwritten_on_restore() {
        let window = TestWindow::new();
        let id = window_id(window.0);
        let original = create_rect_region(Rect::new(0, 0, 1000, 800)).unwrap();
        assert!(set_owned_region(window.0, clone_region(&original).unwrap(), false));
        assert!(apply_window_region_clip(
            id,
            Rect::new(-500, 0, 1000, 800),
            Rect::new(-500, 0, 1000, 800),
            Rect::new(0, 0, 2000, 1000),
            false,
        )
        .succeeded());

        let takeover = create_rect_region(Rect::new(100, 100, 700, 500)).unwrap();
        assert!(set_owned_region(window.0, clone_region(&takeover).unwrap(), false));
        assert!(restore_window_region(id, false));
        assert_eq!(current_region_matches(window.0, &takeover), Ok(true));
    }

    #[test]
    fn custom_region_recovers_after_in_memory_state_loss() {
        let window = TestWindow::new();
        let id = window_id(window.0);
        let original = create_rect_region(Rect::new(40, 20, 920, 740)).unwrap();
        assert!(set_owned_region(window.0, clone_region(&original).unwrap(), false));
        assert!(apply_window_region_clip(
            id,
            Rect::new(-500, 0, 1000, 800),
            Rect::new(-500, 0, 1000, 800),
            Rect::new(0, 0, 2000, 1000),
            false,
        )
        .succeeded());

        let state = lock_states().remove(&id).unwrap();
        let backup = backup_path(id, state.backup_token.unwrap());
        drop(state);
        assert!(backup.exists());

        assert!(can_clip_window_region(id));
        assert_eq!(current_region_matches(window.0, &original), Ok(true));
        assert!(!backup.exists());
    }
}
