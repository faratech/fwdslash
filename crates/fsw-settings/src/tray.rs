//! Tray icon + minimize/close-to-tray for the settings window.
//!
//! Ported from the wfdiag reactor-spike `window_support.rs` solution: reactor
//! models no tray or lifecycle surface, so the window is subclassed with
//! `SetWindowSubclass` (comctl32) and every intercepted message either ends in
//! this file or chains to the original reactor procedure via `DefSubclassProc`.
//!
//! - `WM_CLOSE` hides the window to the tray; the tray menu's Exit sets
//!   `FORCE_CLOSE` so the next close is real.
//! - Minimizing hides the window as well -- the taskbar button disappears and
//!   the notification-area icon becomes the restore affordance.
//! - `WM_APP_TRAY` is the tray callback: left click restores, right click
//!   opens the Show/Exit menu.
//! - `TaskbarCreated` (shell restart) re-adds the icon so the window can never
//!   end up hidden with no way back.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    RemoveWindowSubclass, SetWindowSubclass, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetSystemMetrics,
    GetWindowThreadProcessId, LoadImageW, MF_STRING, PostMessageW, PostQuitMessage,
    RegisterWindowMessageW, SetForegroundWindow, ShowWindow, SIZE_MINIMIZED, SM_CXSMICON,
    SM_CYSMICON, SW_HIDE, SW_RESTORE, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD,
    TrackPopupMenu, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_ENDSESSION, WM_NCDESTROY,
    WM_SIZE,
};

use crate::{IDI_FSW_APP, WINDOW_TITLE};

/// Win32 app-defined message used for the tray icon callback.
const TRAY_MESSAGE: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const SUBCLASS_ID: usize = 1;

const MENU_SHOW: i32 = 1;
const MENU_EXIT: i32 = 2;

/// Tray tooltip, distinct from the broker's so the two icons are
/// identifiable at a glance. The window title itself stays `"Forward Slash
/// Windows"` for C++ parity.
pub const TRAY_TIP: &str = "fwdslash settings";

const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;

/// Set just before `DestroyWindow` from the tray Exit item, so the resulting
/// `WM_CLOSE` closes instead of hiding.
static FORCE_CLOSE: AtomicBool = AtomicBool::new(false);

/// Discovers this process's settings window. Enumerates top-level windows of
/// the current process only and matches the title exactly: a bare
/// `FindWindowW(NULL, title)` can return *another* instance's window (the
/// single-instance mutex keeps us from racing a live one, but a leftover dev
/// build would still be matched), and `SetWindowSubclass` fails cross-process.
pub fn discover_window() -> isize {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::System::Threading::GetCurrentProcessId;
        use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextLengthW, GetWindowTextW};

        struct Match {
            process_id: u32,
            title: Vec<u16>,
            found: isize,
        }
        unsafe extern "system" fn on_window(window: HWND, lparam: isize) -> i32 {
            unsafe {
                let state = &mut *(lparam as *mut Match);
                let mut owner = 0u32;
                GetWindowThreadProcessId(window, &mut owner);
                if owner != state.process_id {
                    return 1;
                }
                let length = GetWindowTextLengthW(window);
                if length <= 0 {
                    return 1;
                }
                let mut text = vec![0u16; (length as usize) + 1];
                GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32);
                if text[..length as usize] == state.title[..state.title.len() - 1] {
                    state.found = window as isize;
                    return 0;
                }
                1
            }
        }

        let mut state = Match {
            process_id: GetCurrentProcessId(),
            title: crate::to_wide(WINDOW_TITLE),
            found: 0,
        };
        EnumWindows(Some(on_window), &mut state as *mut Match as isize);
        state.found
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Subclasses the window and adds the tray icon. Must run on the window's own
/// thread; a static guard makes repeat calls a no-op and a *changed* handle
/// (a reactor-recreated HWND) reinstall rather than double-subclass. The
/// returned string is the Win32 error the failure mapped to, for the crash log.
pub fn install(window: isize) -> Result<(), u32> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::GetLastError;
        static INSTALLED_HWND: AtomicIsize = AtomicIsize::new(0);
        if INSTALLED_HWND.load(Ordering::SeqCst) == window {
            return Ok(());
        }
        if SetWindowSubclass(window as _, Some(tray_proc), SUBCLASS_ID, 0) == 0 {
            return Err(GetLastError());
        }
        if !add_tray_icon(window as _) {
            return Err(GetLastError());
        }
        INSTALLED_HWND.store(window, Ordering::SeqCst);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        Err(0)
    }
}

#[cfg(windows)]
fn tray_icon() -> *mut core::ffi::c_void {
    static TRAY_ICON: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *TRAY_ICON.get_or_init(|| unsafe {
        let instance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(
            std::ptr::null(),
        );
        LoadImageW(
            instance,
            IDI_FSW_APP as *const u16,
            1, // IMAGE_ICON
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            0, // LR_DEFAULTCOLOR
        ) as usize
    }) as *mut core::ffi::c_void
}

#[cfg(windows)]
fn add_tray_icon(window: windows_sys::Win32::Foundation::HWND) -> bool {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = window;
    data.uID = TRAY_ID;
    // NIF_ICON is required for `hIcon` to be shown at all; without it the
    // notification area reserved a blank slot (wfdiag's finding).
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = TRAY_MESSAGE;
    data.hIcon = tray_icon();
    let tip = crate::to_wide(TRAY_TIP);
    let tip_len = tip.len().min(data.szTip.len() - 1);
    data.szTip[..tip_len].copy_from_slice(&tip[..tip_len]);
    unsafe { Shell_NotifyIconW(NIM_ADD, &data) != 0 }
}

#[cfg(windows)]
fn remove_tray_icon(window: windows_sys::Win32::Foundation::HWND) {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = window;
    data.uID = TRAY_ID;
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

/// `TaskbarCreated` is broadcast when the shell restarts; without re-adding
/// the icon, hide-to-tray would leave the window with no restore affordance.
#[cfg(windows)]
fn taskbar_created_message() -> u32 {
    static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *TASKBAR_CREATED.get_or_init(|| unsafe {
        RegisterWindowMessageW(crate::to_wide("TaskbarCreated").as_ptr())
    })
}

#[cfg(windows)]
fn restore(window: windows_sys::Win32::Foundation::HWND) {
    unsafe {
        ShowWindow(window, SW_RESTORE);
        SetForegroundWindow(window);
    }
}

#[cfg(windows)]
fn hide(window: windows_sys::Win32::Foundation::HWND) {
    unsafe {
        ShowWindow(window, SW_HIDE);
    }
}

/// Real exit: arms `FORCE_CLOSE`, then posts `WM_CLOSE` instead of calling
/// `DestroyWindow` directly. The reactor's process exit lives behind WinUI's
/// `Window.Closed` event, which only the close pipeline raises -- a direct
/// `DestroyWindow` skips it and leaves a windowless process holding the
/// single-instance mutex (the "won't start again" zombie). The subclass
/// removes the icon on the resulting `WM_CLOSE`; `WM_DESTROY` posts the quit.
#[cfg(windows)]
fn exit_via_close(window: windows_sys::Win32::Foundation::HWND) {
    FORCE_CLOSE.store(true, Ordering::SeqCst);
    unsafe { PostMessageW(window, WM_CLOSE, 0, 0) };
}

#[cfg(windows)]
fn show_tray_menu(window: windows_sys::Win32::Foundation::HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING, MENU_SHOW as usize, crate::to_wide("&Show").as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_EXIT as usize, crate::to_wide("E&xit").as_ptr());
        let mut cursor = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        SetForegroundWindow(window);
        let command = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD,
            cursor.x,
            cursor.y,
            0,
            window,
            std::ptr::null(),
        );
        DestroyMenu(menu);
        match command {
            MENU_SHOW => restore(window),
            MENU_EXIT => exit_via_close(window),
            _ => {}
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn tray_proc(
    window: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _context: usize,
) -> LRESULT {
    match message {
        WM_CLOSE => {
            if FORCE_CLOSE.load(Ordering::SeqCst) {
                remove_tray_icon(window);
            } else {
                hide(window);
                return 0;
            }
        }
        WM_SIZE => {
            if wparam as u32 == SIZE_MINIMIZED {
                // Minimize to tray: hide removes the taskbar button, and the
                // notification-area icon takes over as the restore affordance.
                hide(window);
                return 0;
            }
        }
        WM_COMMAND => {
            match (wparam & 0xFFFF) as i32 {
                MENU_SHOW => restore(window),
                MENU_EXIT => exit_via_close(window),
                _ => {}
            }
            return 0;
        }
        // Belt and braces: the reactor's only exit path is WinUI's
        // `Window.Closed` event, and any destroy that skips the close
        // pipeline would otherwise leave the process running with no
        // window (the settings-app "won't start again" zombie).
        WM_DESTROY => {
            crate::watchdog::note_exit_requested();
            unsafe { PostQuitMessage(0) };
        }
        WM_NCDESTROY => {
            remove_tray_icon(window);
            unsafe { RemoveWindowSubclass(window, Some(tray_proc), SUBCLASS_ID) };
        }
        // Session end destroys the window without the close pipeline; make
        // sure the icon does not outlive the session as a ghost.
        WM_ENDSESSION if wparam != 0 => {
            remove_tray_icon(window);
        }
        message if message == taskbar_created_message() => {
            remove_tray_icon(window);
            add_tray_icon(window);
        }
        message if message == TRAY_MESSAGE => {
            match lparam as u32 {
                WM_LBUTTONUP => restore(window),
                WM_RBUTTONUP => show_tray_menu(window),
                _ => {}
            }
            return 0;
        }
        _ => {}
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}
