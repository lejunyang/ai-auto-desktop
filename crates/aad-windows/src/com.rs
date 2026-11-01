//! COM, which UI Automation and the WinRT APIs need on every thread.
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Initialise COM on the calling thread.
///
/// COM apartments are per-thread, not per-process, so this must run on every
/// thread that touches UI Automation.  Using a process-wide guard here would
/// leave later threads using COM in an uninitialised apartment, which fails in
/// ways that surface as access violations rather than errors.
pub fn ensure() {
    thread_local! {
        static INITIALISED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    INITIALISED.with(|initialised| {
        if !initialised.get() {
            unsafe {
                // An "already initialised" result is a success for our
                // purposes: the host may own the apartment.
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            initialised.set(true);
        }
    });
}
