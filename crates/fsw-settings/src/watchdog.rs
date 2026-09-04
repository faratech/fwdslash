//! Process-exit watchdog for the settings window.
//!
//! The vendored reactor's only exit path is WinUI's `Window.Closed` event
//! (`app.rs` `finalize_closed_window`, reached only through the `window.Closed`
//! subscription). Any way the HWND can die without that event -- a direct
//! `DestroyWindow`, an external destroy, session end -- leaves the process
//! pumping forever with no window, and every future launch then exits silently
//! against the single-instance mutex. The tray subclass posts `WM_QUIT` from
//! `WM_DESTROY` as the first line of defense; this watchdog is the second: a
//! 1 Hz off-thread tick that quits the UI thread (or the process, last resort)
//! when the window it saw disappears.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32};
use std::sync::atomic::Ordering::SeqCst;
use std::thread::sleep;
use std::time::Duration;

/// Thread id of the UI thread, stored by `main` before the reactor starts.
static UI_THREAD: AtomicU32 = AtomicU32::new(0);
/// Set once `WindowHookReady` has delivered a non-zero window.
static WINDOW_SEEN: AtomicBool = AtomicBool::new(false);
/// The window handle the discovery poller handed back.
static KNOWN_HWND: AtomicIsize = AtomicIsize::new(0);
/// Set by the exit paths when the app means to exit; silences the watchdog.
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Startup grace: window materialization plus the second-instance activation
/// poll (10 s) must never trip the watchdog.
const STARTUP_GRACE_TICKS: u32 = 20;
/// Ticks between the first `WM_QUIT` and the last-resort `process::exit`.
const EXIT_GRACE_TICKS: u32 = 3;

pub fn note_ui_thread() {
    #[cfg(windows)]
    unsafe {
        UI_THREAD.store(windows_sys::Win32::System::Threading::GetCurrentThreadId(), SeqCst);
    }
}

pub fn note_window(window: isize) {
    WINDOW_SEEN.store(true, SeqCst);
    KNOWN_HWND.store(window, SeqCst);
}

pub fn note_exit_requested() {
    EXIT_REQUESTED.store(true, SeqCst);
}

/// Posts `WM_QUIT` to the UI thread from any thread.
pub fn quit_ui_thread() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
        PostThreadMessageW(UI_THREAD.load(SeqCst), WM_QUIT, 0, 0);
    }
}

/// Spawns the 1 Hz watchdog thread. `PostQuitMessage` only reaches the calling
/// thread's queue, so a non-UI thread must use `PostThreadMessageW`.
pub fn spawn() {
    #[cfg(windows)]
    std::thread::spawn(|| unsafe {
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::UI::WindowsAndMessaging::{IsWindow, PostThreadMessageW, WM_QUIT};

        let mut ticks = 0u32;
        let mut dead_ticks = 0u32;
        loop {
            sleep(Duration::from_secs(1));
            ticks += 1;
            if ticks <= STARTUP_GRACE_TICKS || EXIT_REQUESTED.load(SeqCst) || !WINDOW_SEEN.load(SeqCst)
            {
                continue;
            }
            let known = KNOWN_HWND.load(SeqCst);
            if known != 0 && IsWindow(known as HWND) != 0 {
                dead_ticks = 0;
                continue;
            }
            dead_ticks += 1;
            if dead_ticks >= EXIT_GRACE_TICKS {
                crate::log_crash("watchdog: process outlived its window; exiting");
                std::process::exit(4);
            }
            if dead_ticks == 1 {
                PostThreadMessageW(UI_THREAD.load(SeqCst), WM_QUIT, 0, 0);
            }
        }
    });
}
