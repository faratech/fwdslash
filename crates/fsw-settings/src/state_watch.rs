//! Hearing about state this window did not change (issue #55).
//!
//! `State::read()` is a snapshot. Everything in it — the pause flag, the
//! bare-slash mode, the adapter markers, whether a broker is running — belongs
//! to the machine, not to this process, and four other writers touch it: the
//! CLI (any `fwdslash` verb, including the staged unpackaged copies the shell
//! adapters run), the broker (the tray's Enabled toggle, the logon adapter
//! sweep), the update path, and a person with `reg.exe`. Before this the
//! window noticed none of them: it re-read only after its own controller calls,
//! so `fwdslash bare-slash default` in a terminal left the radio buttons lying
//! until the window was closed and reopened.
//!
//! Two mechanisms, deliberately:
//!
//! 1. **The broadcast.** Every component posts
//!    [`fsw_core::FSW_STATE_CHANGED_MESSAGE`] after a mutation that landed.
//!    This module owns a hidden top-level window whose only job is to receive
//!    it. It has to be a real top-level window — `HWND_BROADCAST` skips
//!    message-only ones, the same reason the broker's window is one — and it
//!    lives on a thread of its own so its message loop can never be held up by
//!    the XAML one.
//! 2. **The poll.** Not every writer broadcasts: a staged `fwdslash.exe` from
//!    an older version does not, and neither does `reg.exe`. So a wake also
//!    happens every [`POLL_INTERVAL_MS`], and the window compares what it
//!    reads against what it holds.
//!
//! Nothing here touches the UI thread, and the message carries no payload —
//! the reader re-reads, so nothing about what changed travels between
//! processes (PRIVACY.md).

/// How long a wake waits before giving up and polling instead.
///
/// The poll is the belt to the broadcast's braces, and it only runs while the
/// window is on screen, so 5 s of latency for a writer that says nothing is
/// the whole cost of covering writers that cannot be changed.
pub(crate) const POLL_INTERVAL_MS: u32 = 5_000;

/// How long a broadcast waits for its friends before the read starts.
///
/// One `fwdslash bare-slash default` writes three values, so it broadcasts
/// three times; the settings window's own controller calls can produce a
/// handful more. Reading once at the end of the burst is both cheaper and
/// less likely to catch a multi-value write half-applied.
pub(crate) const COALESCE_MS: u32 = 250;

/// What woke the watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// A component broadcast that it changed something.
    Broadcast,
    /// Nobody said anything; this is the safety poll.
    Poll,
}

/// Whether a wake should cost a `State::read()`.
///
/// A broadcast always does, minimized or not: it is rare, it is cheap, and a
/// window restored from the taskbar has to be right the moment it appears. The
/// poll is the one that repeats forever, so it stops while nobody can see the
/// result.
pub(crate) const fn should_read(wake: Wake, window_visible: bool) -> bool {
    match wake {
        Wake::Broadcast => true,
        Wake::Poll => window_visible,
    }
}

/// Keeps external notifications down to one state read at a time.
///
/// The read is not free — it opens the settings key, enumerates Lxss, asks the
/// broker for its state and probes the filter port — and bursts are the normal
/// case, not the exception. So a wake that arrives while a read is running is
/// not dropped and not run: it is remembered, and one more read starts when
/// the running one lands. However many wakes arrive, at most one read is in
/// flight and at most one is owed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReadCoalescer {
    in_flight: bool,
    owed: bool,
}

impl ReadCoalescer {
    /// Records a wake, reporting whether a read should start now.
    pub(crate) const fn wake(&mut self) -> bool {
        if self.in_flight {
            self.owed = true;
            return false;
        }
        self.in_flight = true;
        true
    }

    /// Records a completed read, reporting whether another should start.
    pub(crate) const fn finished(&mut self) -> bool {
        self.in_flight = false;
        if self.owed {
            self.owed = false;
            return self.wake();
        }
        false
    }

    /// Whether a read is running right now. Diagnostics and tests only.
    #[cfg(test)]
    pub(crate) const fn is_reading(self) -> bool {
        self.in_flight
    }
}

// ---------------------------------------------------------------------------
// The watcher window
// ---------------------------------------------------------------------------

/// Class of the hidden window that receives the broadcast. Never discovered by
/// anyone — nothing looks this window up, it only listens.
#[cfg(windows)]
const WATCHER_WINDOW_CLASS: &str = "ForwardSlashWindows.SettingsWatcher";

/// The window's caption. Anything but [`crate::WINDOW_TITLE`]: the
/// single-instance raise and the folder picker's owner lookup both find a
/// window *by that title in this process*, and would find this one.
#[cfg(windows)]
const WATCHER_WINDOW_TITLE: &str = "fwdslash settings watcher";

/// The manual-reset event the watcher window signals and [`wait`] drains, as a
/// `usize` so it can live in a `static`. Zero until [`start`] succeeds.
#[cfg(windows)]
static SIGNAL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Creates the event and the listener thread, once per process.
///
/// Returns whether the broadcast half is live. `false` degrades to the poll
/// alone, which is a slower window, not a broken one — so nothing here is
/// worth failing a launch over.
#[cfg(windows)]
pub(crate) fn start() -> bool {
    *SIGNAL.get_or_init(|| {
        use windows_sys::Win32::System::Threading::CreateEventW;

        // Manual reset, initially clear: `wait` resets it itself, immediately
        // before the read, so every signal raised up to that moment is
        // absorbed by the one read that follows.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return 0;
        }
        let handle = event as usize;
        if std::thread::Builder::new()
            .name("fsw-state-watch".to_owned())
            .spawn(move || watcher_thread(handle))
            .is_err()
        {
            // No thread, no listener. Leave the handle open (it is a
            // process-lifetime object) and report the poll-only fallback.
            return 0;
        }
        handle
    }) != 0
}

#[cfg(not(windows))]
pub(crate) fn start() -> bool {
    false
}

/// Owns the hidden window and pumps its messages for the life of the process.
#[cfg(windows)]
fn watcher_thread(signal: usize) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, RegisterClassW,
        TranslateMessage, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
    };

    // The signalling half of the pair, reachable from the window procedure.
    static WATCHER_SIGNAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "system" fn watcher_proc(
        window: HWND,
        message: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        use windows_sys::Win32::System::Threading::SetEvent;

        // `message != 0` because a failed registration answers 0, which is
        // WM_NULL — a message this window is sent by anything probing it.
        if message != 0 && message == fsw_core::state_changed_message() {
            let signal = WATCHER_SIGNAL.load(std::sync::atomic::Ordering::Acquire);
            if signal != 0 {
                unsafe { SetEvent(signal as _) };
            }
            return 0;
        }
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    WATCHER_SIGNAL.store(signal, std::sync::atomic::Ordering::Release);
    // Registering the message here means the window procedure's comparison is
    // a load from the first broadcast onward.
    let _ = fsw_core::state_changed_message();

    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = crate::to_wide(WATCHER_WINDOW_CLASS);
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.lpfnWndProc = Some(watcher_proc);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&raw const class) == 0 {
            return;
        }
        // Top-level and never shown, exactly like the broker window:
        // `HWND_BROADCAST` does not reach message-only windows. `WS_EX_TOOLWINDOW`
        // keeps it out of the taskbar and Alt+Tab.
        let window = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            crate::to_wide(WATCHER_WINDOW_TITLE).as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        if window.is_null() {
            return;
        }
        let mut message: MSG = std::mem::zeroed();
        // Ends when the process does: there is nothing to tear down, and the
        // window must keep listening for as long as one can be rendered.
        while GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

/// Blocks until something changes or the poll interval elapses.
///
/// **Runs on a background thread only.** It is one turn of the watch: the
/// caller re-arms the next turn when the message it returns is handled, so the
/// loop is driven by the component and stops when the component does.
#[cfg(windows)]
pub(crate) fn wait() -> Wake {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{ResetEvent, Sleep, WaitForSingleObject};

    let Some(&signal) = SIGNAL.get().filter(|signal| **signal != 0) else {
        // Poll-only fallback: no event was ever created.
        unsafe { Sleep(POLL_INTERVAL_MS) };
        return Wake::Poll;
    };
    if unsafe { WaitForSingleObject(signal as _, POLL_INTERVAL_MS) } != WAIT_OBJECT_0 {
        return Wake::Poll;
    }
    // Let the rest of a multi-value write arrive, then clear the event *before*
    // reading: a signal raised after this point belongs to a write this read
    // may not see, and has to wake the next turn.
    unsafe {
        Sleep(COALESCE_MS);
        ResetEvent(signal as _);
    }
    Wake::Broadcast
}

#[cfg(not(windows))]
pub(crate) fn wait() -> Wake {
    std::thread::sleep(std::time::Duration::from_millis(u64::from(POLL_INTERVAL_MS)));
    Wake::Poll
}

/// Whether the settings window is on screen right now.
///
/// A window that has not been created yet counts as visible: the first poll
/// can land before WinUI has materialized it, and skipping the read then would
/// leave the first frame's state unrefreshed for another interval.
#[cfg(windows)]
pub(crate) fn window_visible() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};

    let window = crate::folder_picker::current_process_window();
    if window == 0 {
        return true;
    }
    unsafe { IsWindowVisible(window as _) != 0 && IsIconic(window as _) == 0 }
}

#[cfg(not(windows))]
pub(crate) fn window_visible() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{POLL_INTERVAL_MS, ReadCoalescer, Wake, should_read};

    #[test]
    fn a_broadcast_is_always_read() {
        assert!(should_read(Wake::Broadcast, true));
        assert!(should_read(Wake::Broadcast, false));
    }

    #[test]
    fn the_poll_stops_while_the_window_is_not_on_screen() {
        assert!(should_read(Wake::Poll, true));
        assert!(!should_read(Wake::Poll, false));
    }

    #[test]
    fn the_first_wake_reads() {
        let mut coalescer = ReadCoalescer::default();
        assert!(coalescer.wake());
        assert!(coalescer.is_reading());
    }

    #[test]
    fn a_burst_during_a_read_becomes_one_more_read() {
        let mut coalescer = ReadCoalescer::default();
        assert!(coalescer.wake());
        // Three values written in one `bare-slash default`, three broadcasts.
        assert!(!coalescer.wake());
        assert!(!coalescer.wake());
        assert!(!coalescer.wake());
        // Exactly one more read, and then the burst is spent.
        assert!(coalescer.finished());
        assert!(!coalescer.finished());
        assert!(!coalescer.is_reading());
    }

    #[test]
    fn a_quiet_read_owes_nothing() {
        let mut coalescer = ReadCoalescer::default();
        assert!(coalescer.wake());
        assert!(!coalescer.finished());
        assert_eq!(coalescer, ReadCoalescer::default());
    }

    #[test]
    fn wakes_after_a_read_start_a_new_one() {
        let mut coalescer = ReadCoalescer::default();
        assert!(coalescer.wake());
        assert!(!coalescer.finished());
        assert!(coalescer.wake());
    }

    /// The safety poll is a background cost that runs for as long as the
    /// window is open; keep it visibly slow.
    #[test]
    fn the_poll_stays_infrequent() {
        assert!(POLL_INTERVAL_MS >= 5_000);
    }
}
