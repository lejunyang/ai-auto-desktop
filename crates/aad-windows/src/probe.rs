//! The Windows part of `aad probe`: read-only checks of what desktop
//! automation needs here. Nothing is asked for, injected or captured.
use aad_platform::probe::{Check, State};
use serde_json::json;
use windows_sys::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC, DESKTOPHORZRES, HORZRES};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenElevation,
    TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::SystemServices::{
    SECURITY_MANDATORY_HIGH_RID, SECURITY_MANDATORY_LOW_RID, SECURITY_MANDATORY_MEDIUM_RID,
    SECURITY_MANDATORY_SYSTEM_RID,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::HiDpi::{
    GetProcessDpiAwareness, PROCESS_DPI_AWARENESS, PROCESS_DPI_UNAWARE,
    PROCESS_PER_MONITOR_DPI_AWARE, PROCESS_SYSTEM_DPI_AWARE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CMONITORS, SM_CXSCREEN, SM_CYSCREEN, SM_REMOTESESSION,
};

pub fn run() -> Vec<Check> {
    vec![
        display(),
        session(),
        input_desktop(),
        integrity(),
        scaling(),
        uia(),
    ]
}

/// Whether an addressable desktop surface exists at all.
fn display() -> Check {
    let (width, height, monitors) = unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
            GetSystemMetrics(SM_CMONITORS),
        )
    };
    let evidence = json!({
        "screen_width": width,
        "screen_height": height,
        "monitor_count": monitors,
    });

    if width > 0 && height > 0 {
        Check::new(
            "windows.display",
            State::Available,
            "A desktop surface with a non-zero extent is advertised.",
            evidence,
        )
    } else {
        Check::new(
            "windows.display",
            State::Unavailable,
            "No desktop surface with a usable extent is advertised to this process.",
            evidence,
        )
    }
}

/// A remote session can work, but input and capture behave differently.
fn session() -> Check {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    let remote = unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0;
    let mut session_id = 0u32;
    let session_known =
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) } != 0;
    let session_zero = session_known.then_some(session_id == 0);
    let evidence = json!({
        "session_id_available": session_known,
        "session_zero": session_zero,
        "interactive_session": session_known.then_some(session_id != 0),
        "remote_session": remote,
    });

    if session_zero == Some(true) {
        Check::new(
            "windows.session",
            State::Unavailable,
            "This process runs in Session 0, which has no interactive desktop.",
            evidence,
        )
    } else if !session_known {
        Check::new(
            "windows.session",
            State::Unknown,
            "The process session could not be determined.",
            evidence,
        )
    } else if remote {
        Check::new(
            "windows.session",
            State::Degraded,
            "This is a remote desktop session; input and capture can differ after disconnect.",
            evidence,
        )
    } else {
        Check::new(
            "windows.session",
            State::Available,
            "This is a local console session.",
            evidence,
        )
    }
}

fn input_desktop() -> Check {
    use windows_sys::Win32::Foundation::FALSE;
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW,
        OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
    };
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    unsafe fn object_name(handle: *mut std::ffi::c_void) -> Option<String> {
        if handle.is_null() {
            return None;
        }
        let mut needed = 0u32;
        GetUserObjectInformationW(handle, UOI_NAME, std::ptr::null_mut(), 0, &mut needed);
        if !(2..=4096).contains(&needed) {
            return None;
        }
        let mut buffer = vec![0u16; needed.div_ceil(2) as usize];
        if GetUserObjectInformationW(
            handle,
            UOI_NAME,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            return None;
        }
        let end = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..end]))
    }

    let station = unsafe { object_name(GetProcessWindowStation().cast()) };
    let thread_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    let thread_name = unsafe { object_name(thread_desktop) };
    let input = unsafe { OpenInputDesktop(0, FALSE, DESKTOP_READOBJECTS) };
    if input.is_null() {
        return Check::new("windows.input_desktop", State::Degraded, "The current input desktop cannot be opened; a secure desktop or another session may own input.", json!({"interactive_window_station": station.as_deref().is_some_and(|name| name.eq_ignore_ascii_case("WinSta0")), "input_desktop_readable": false, "thread_desktop_is_input_desktop": null}));
    }
    let input_name = unsafe { object_name(input) };
    unsafe { CloseDesktop(input) };
    let interactive = station
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case("WinSta0"));
    let same = thread_name
        .as_ref()
        .zip(input_name.as_ref())
        .map(|(left, right)| left == right);
    let (state, summary) = if !interactive {
        (
            State::Unavailable,
            "This process is not on the interactive window station.",
        )
    } else if same == Some(true) {
        (
            State::Available,
            "This thread desktop is the current input desktop; no input was injected.",
        )
    } else {
        (
            State::Degraded,
            "This thread desktop could not be confirmed as the current input desktop.",
        )
    };
    Check::new(
        "windows.input_desktop",
        state,
        summary,
        json!({"interactive_window_station": interactive, "input_desktop_readable": true, "thread_desktop_is_input_desktop": same}),
    )
}

/// DPI virtualisation makes reported coordinates disagree with real pixels.
fn scaling() -> Check {
    let mut awareness: PROCESS_DPI_AWARENESS = PROCESS_DPI_UNAWARE;
    let awareness_readable =
        unsafe { GetProcessDpiAwareness(std::ptr::null_mut(), &mut awareness) >= 0 };
    let (logical, physical) = unsafe {
        let dc = GetDC(std::ptr::null_mut());
        if dc.is_null() {
            (0, 0)
        } else {
            let logical = GetDeviceCaps(dc, HORZRES as i32);
            let physical = GetDeviceCaps(dc, DESKTOPHORZRES as i32);
            ReleaseDC(std::ptr::null_mut(), dc);
            (logical, physical)
        }
    };
    let quantisation = (logical > 0 && physical > 0)
        .then(|| (physical as f64 / logical as f64 * 10_000.0).round() / 10_000.0);
    let awareness_name = awareness_readable.then_some(match awareness {
        PROCESS_DPI_UNAWARE => "unaware",
        PROCESS_SYSTEM_DPI_AWARE => "system",
        PROCESS_PER_MONITOR_DPI_AWARE => "per_monitor",
        _ => "unknown",
    });
    let evidence = json!({
        "awareness_readable": awareness_readable,
        "awareness": awareness_name,
        "logical_width": logical,
        "physical_width": physical,
        "scaled_display": logical > 0 && physical > 0 && logical != physical,
        "pointer_quantisation": quantisation,
    });

    if logical <= 0 || physical <= 0 {
        return Check::new(
            "windows.dpi",
            State::Unknown,
            "Display scaling could not be determined.",
            evidence,
        );
    }
    if logical != physical {
        return Check::new(
            "windows.dpi",
            State::Degraded,
            "The process is not fully DPI aware on a scaled display, so pointer coordinates quantise physical pixels.",
            evidence,
        );
    }
    Check::new(
        "windows.dpi",
        State::Available,
        "Reported coordinates match physical pixels.",
        evidence,
    )
}

/// Whether the UI Automation client stack can be instantiated.
fn uia() -> Check {
    // This is the one prerequisite worth confirming directly, because the
    // whole driver depends on it and creating a client object is a purely
    // read-only operation that changes nothing on the desktop.
    if check_uia_available() {
        Check::new(
            "windows.uia",
            State::Available,
            "The UI Automation client stack is available to this process.",
            json!({"client_created": true}),
        )
    } else {
        Check::new(
            "windows.uia",
            State::Unavailable,
            "The UI Automation client stack could not be created.",
            json!({"client_created": false}),
        )
    }
}

fn check_uia_available() -> bool {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};

    unsafe {
        // COM apartments are per-thread; initialise the one we are on.
        // "Already initialised" is a success for a read-only probe.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // The reference is dropped at the end of this scope, so the probe
        // owns nothing once it has answered.
        CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER).is_ok()
    }
}

/// Integrity level governs which windows this process may automate.
fn integrity() -> Check {
    use windows_sys::Win32::Foundation::CloseHandle;

    let (elevated, level) = unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            (None, None)
        } else {
            let mut elevation: u32 = 0;
            let mut returned: u32 = 0;
            let elevation_ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut u32 as *mut _,
                std::mem::size_of::<u32>() as u32,
                &mut returned,
            );
            let mut required = 0u32;
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                std::ptr::null_mut(),
                0,
                &mut required,
            );
            let level = if required == 0 || required > 4096 {
                None
            } else {
                let mut buffer = vec![0u8; required as usize];
                if GetTokenInformation(
                    token,
                    TokenIntegrityLevel,
                    buffer.as_mut_ptr().cast(),
                    required,
                    &mut returned,
                ) == 0
                {
                    None
                } else {
                    let label = &*(buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
                    let count = *GetSidSubAuthorityCount(label.Label.Sid) as u32;
                    if count == 0 {
                        None
                    } else {
                        let rid = *GetSidSubAuthority(label.Label.Sid, count - 1) as i32;
                        Some(if rid >= SECURITY_MANDATORY_SYSTEM_RID {
                            "system"
                        } else if rid >= SECURITY_MANDATORY_HIGH_RID {
                            "high"
                        } else if rid >= SECURITY_MANDATORY_MEDIUM_RID {
                            "medium"
                        } else if rid >= SECURITY_MANDATORY_LOW_RID {
                            "low"
                        } else {
                            "untrusted"
                        })
                    }
                }
            };
            CloseHandle(token);
            ((elevation_ok != 0).then_some(elevation != 0), level)
        }
    };

    let evidence =
        json!({"token_readable": level.is_some(), "elevated": elevated, "integrity_level": level});
    match level {
        Some("untrusted" | "low") => Check::new("windows.integrity", State::Unavailable, "This process has low integrity, so UIPI blocks ordinary applications.", evidence),
        Some(level) => Check::new("windows.integrity", State::Available, &format!("This process runs at {level} integrity; UIPI still blocks higher-integrity applications."), evidence),
        None => Check::new("windows.integrity", State::Unknown, "The process integrity level could not be determined.", evidence),
    }
}
