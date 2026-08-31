//! System-level queries: DPI scaling, power state, accessibility settings.

/// Scale a pixel value by the given DPI scale factor.
///
/// Config values are in logical pixels (96 DPI). This function converts them
/// to physical pixels for a specific monitor's DPI.
pub fn scale_px(value: i32, scale_factor: f64) -> i32 {
    (value as f64 * scale_factor).round() as i32
}

/// Check if the system is running on battery power or Windows power saver is active.
/// Returns `true` when either condition is met, signalling that animations should be disabled.
pub fn is_on_battery_or_power_saver() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut status = SYSTEM_POWER_STATUS::default();
    unsafe {
        if GetSystemPowerStatus(&mut status).is_ok() {
            // ACLineStatus: 0 = offline (battery), 1 = online (AC), 255 = unknown
            let on_battery = status.ACLineStatus == 0;
            // SystemStatusFlag bit 0: Windows power saver is active
            let power_saver = (status.SystemStatusFlag & 1) != 0;
            on_battery || power_saver
        } else {
            false // Assume AC if the API fails
        }
    }
}

fn process_session_id(process_id: u32) -> Result<u32, String> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;

    let mut session_id = 0;
    unsafe { ProcessIdToSessionId(process_id, &mut session_id) }
        .map_err(|error| format!("session query failed for process {process_id}: {error}"))?;
    Ok(session_id)
}

/// Authoritative Windows session ID for this process.
pub fn current_session_id() -> Result<u32, String> {
    use windows::Win32::System::Threading::GetCurrentProcessId;

    process_session_id(unsafe { GetCurrentProcessId() })
}

fn session_ids_match(current: u32, peer: u32) -> bool {
    current == peer
}

fn same_session_as_current(process_id: u32) -> Result<bool, String> {
    Ok(session_ids_match(
        current_session_id()?,
        process_session_id(process_id)?,
    ))
}

fn named_pipe_peer_in_current_session(raw_handle: isize, client: bool) -> Result<bool, String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};

    let pipe = HANDLE(raw_handle as *mut core::ffi::c_void);
    let mut process_id = 0;
    let result = unsafe {
        if client {
            GetNamedPipeClientProcessId(pipe, &mut process_id)
        } else {
            GetNamedPipeServerProcessId(pipe, &mut process_id)
        }
    };
    result.map_err(|error| format!("named-pipe peer query failed: {error}"))?;
    same_session_as_current(process_id)
}

/// Validate the client connected to a server-side named-pipe handle.
pub fn named_pipe_client_in_current_session(raw_handle: isize) -> Result<bool, String> {
    named_pipe_peer_in_current_session(raw_handle, true)
}

/// Validate the server behind a client-side named-pipe handle.
pub fn named_pipe_server_in_current_session(raw_handle: isize) -> Result<bool, String> {
    named_pipe_peer_in_current_session(raw_handle, false)
}

/// Whether another process with `executable_name` runs in this interactive
/// session.
pub fn other_process_in_current_session(executable_name: &str) -> Result<bool, String> {
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NO_MORE_FILES, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;

    struct Snapshot(HANDLE);
    impl Drop for Snapshot {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    unsafe {
        let current_pid = GetCurrentProcessId();
        let current_session = current_session_id()?;
        let snapshot = Snapshot(
            CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
                .map_err(|error| format!("process snapshot failed: {error}"))?,
        );
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        Process32FirstW(snapshot.0, &mut entry)
            .map_err(|error| format!("first process enumeration failed: {error}"))?;
        loop {
            if entry.th32ProcessID != current_pid {
                let name_end = entry
                    .szExeFile
                    .iter()
                    .position(|character| *character == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..name_end]);
                if name.eq_ignore_ascii_case(executable_name)
                    && process_session_id(entry.th32ProcessID)? == current_session
                {
                    return Ok(true);
                }
            }
            if let Err(error) = Process32NextW(snapshot.0, &mut entry) {
                if GetLastError() == ERROR_NO_MORE_FILES {
                    break;
                }
                return Err(format!("process enumeration failed: {error}"));
            }
        }
        Ok(false)
    }
}

/// Check if Windows "Show animations" accessibility setting is enabled.
/// Returns `false` when the user has disabled client-area animations
/// (Settings > Accessibility > Visual effects > Animation effects).
pub fn are_animations_enabled() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW;
    use windows::Win32::UI::WindowsAndMessaging::SPI_GETCLIENTAREAANIMATION;
    use windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS;

    let mut enabled: i32 = 1;
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut i32 as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    enabled != 0
}

/// Check if Windows High Contrast mode is enabled.
/// Returns `true` when the user has activated a high contrast theme
/// (Settings > Accessibility > Contrast themes).
pub fn is_high_contrast_enabled() -> bool {
    use windows::Win32::UI::Accessibility::{
        HCF_HIGHCONTRASTON, HIGHCONTRASTW, HIGHCONTRASTW_FLAGS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    // SPI_GETHIGHCONTRAST = 0x0042
    const SPI_GETHIGHCONTRAST: u32 = 0x0042;

    let mut hc = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = SystemParametersInfoW(
            windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_ACTION(
                SPI_GETHIGHCONTRAST,
            ),
            hc.cbSize,
            Some(&mut hc as *mut HIGHCONTRASTW as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    (hc.dwFlags & HCF_HIGHCONTRASTON) != HIGHCONTRASTW_FLAGS(0)
}

/// Get the system highlight color as a BGR COLORREF value.
/// Used in high contrast mode to override the border color with the
/// system-defined highlight color, matching native Windows behavior.
pub fn get_system_highlight_color_bgr() -> u32 {
    use windows::Win32::Graphics::Gdi::GetSysColor;

    // COLOR_HIGHLIGHT = 13
    unsafe { GetSysColor(windows::Win32::Graphics::Gdi::SYS_COLOR_INDEX(13)) }
}

/// Set the process DPI awareness to Per-Monitor Aware V2.
///
/// This must be called as early as possible in `main()`, before any
/// window or GDI operations. Returns `true` if the call succeeded.
pub fn set_dpi_awareness() -> bool {
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_px_identity_at_100_percent() {
        assert_eq!(scale_px(10, 1.0), 10);
        assert_eq!(scale_px(0, 1.0), 0);
        assert_eq!(scale_px(-5, 1.0), -5);
    }

    #[test]
    fn test_scale_px_200_percent() {
        assert_eq!(scale_px(10, 2.0), 20);
        assert_eq!(scale_px(3, 2.0), 6);
    }

    #[test]
    fn test_scale_px_150_percent_rounds() {
        assert_eq!(scale_px(3, 1.5), 5); // 4.5 rounds to 5
        assert_eq!(scale_px(10, 1.5), 15);
        assert_eq!(scale_px(1, 1.5), 2); // 1.5 rounds to 2
    }

    #[test]
    fn test_scale_px_125_percent() {
        assert_eq!(scale_px(10, 1.25), 13); // 12.5 rounds to 13
        assert_eq!(scale_px(8, 1.25), 10);
    }

    #[test]
    fn pipe_peer_policy_rejects_other_windows_session() {
        assert!(session_ids_match(7, 7));
        assert!(!session_ids_match(7, 8));
    }

    #[test]
    fn current_process_has_a_queryable_session() {
        assert!(current_session_id().is_ok());
    }

    #[test]
    fn current_session_process_scan_excludes_nonexistent_name() {
        assert!(!other_process_in_current_session("LeopardWM.Does.Not.Exist.exe").unwrap());
    }

    #[test]
    #[ignore = "Requires display hardware - run with: cargo test -- --ignored"]
    fn test_set_dpi_awareness_no_panic() {
        // On CI/test environments this may return false (already set), but must not panic
        let _result = set_dpi_awareness();
    }
}
