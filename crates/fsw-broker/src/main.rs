#![windows_subsystem = "windows"]

mod browser;

use fsw_core::navigation::NavigationAction;
use fsw_core::{
    BrokerState, CMD_ADAPTER_KEY, FSW_ADAPTER_SWEEP_MUTEX, FSW_BROKER_WINDOW_CLASS,
    FSW_FILTER_MAX_DISTRIBUTIONS, FSW_FILTER_PORT_NAME, FSW_FILTER_PROTOCOL_VERSION, FSW_VERSION,
    FSW_WM_QUERY_STATE, FSW_WM_SET_PAUSED, FSW_WM_SHOW_SETTINGS, POWERSHELL_ADAPTER_ROOT, Snapshot,
    adapter_outdated, executable_directory, has_package_identity, is_disabled, is_store_flavor,
    list_registered_distributions, package_version, persist_disabled, resolve_user_slash_path,
    resolve_user_target, state_changed_message, sync_settings_to_real_hive, update,
};
use fsw_path::{RenderBuf, eq_ignore_case};
use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
    CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationLegacyIAccessiblePattern,
    IUIAutomationValuePattern, UIA_ComboBoxControlTypeId, UIA_DocumentControlTypeId,
    UIA_EditControlTypeId, UIA_LegacyIAccessiblePatternId, UIA_ToolBarControlTypeId,
    UIA_ValuePatternId, UIA_ValueValuePropertyId,
};
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
use windows::core::{BOOL, BSTR, Interface};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, HWND,
    INVALID_HANDLE_VALUE, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenIntegrityLevel,
};
use windows_sys::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYEVENTF_KEYUP, SendInput, VK_ESCAPE, VK_RETURN,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    SEE_MASK_ASYNCOK, SEE_MASK_FLAG_NO_UI, SHELLEXECUTEINFOW, Shell_NotifyIconW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::QS_SENDMESSAGE;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, FindWindowExW, GA_ROOT, GetAncestor, GetClassNameW,
    GetCursorPos, GetDlgItem, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, HHOOK,
    HICON, HWND_MESSAGE, IDC_ARROW, KBDLLHOOKSTRUCT, KillTimer, LLKHF_LOWER_IL_INJECTED, LLKHF_UP,
    LoadCursorW, LoadIconW, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG,
    MsgWaitForMultipleObjectsEx, PostMessageW, PostQuitMessage, PostThreadMessageW,
    RegisterClassExW, RegisterWindowMessageW, SW_SHOWNORMAL, SetForegroundWindow,
    SetMenuDefaultItem, SetTimer, SetWindowsHookExW, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_KEYUP, WM_LBUTTONDBLCLK,
    WM_LBUTTONUP, WM_NULL, WM_QUERYENDSESSION, WM_QUIT, WM_RBUTTONUP, WM_SYSKEYUP, WM_TIMER,
    WNDCLASSEXW, WNDPROC, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

const MUTEX_NAME: &str = "Local\\ForwardSlashWindows.Broker";
/// Explicit opt-in for the isolated browser harness.  Normal package
/// activation never supplies this environment value.
#[cfg(debug_assertions)]
const INTEGRATION_TEST_ENV: &str = "FSW_INTEGRATION_TEST_MODE";
const INTEGRATION_TEST_TARGET_HWND_ENV: &str = "FSW_INTEGRATION_TEST_TARGET_HWND";
const INTEGRATION_TEST_TARGET_PID_ENV: &str = "FSW_INTEGRATION_TEST_TARGET_PID";
const INTEGRATION_TEST_MUTEX_NAME: &str = "Local\\ForwardSlashWindows.Broker.E2E";
const INTEGRATION_TEST_WINDOW_CLASS: &str = "ForwardSlashWindows.Broker.E2E";
/// Class of the worker window. Never discovered by anyone: the worker HWND
/// travels through `WORKER_WINDOW`, not `FindWindowW`.
const WORKER_WINDOW_CLASS: &str = "ForwardSlashWindows.BrokerWorker";
const INTEGRATION_TEST_WORKER_WINDOW_CLASS: &str = "ForwardSlashWindows.BrokerWorker.E2E";

/// Packaged Search and Start run at Low IL on supported Windows builds. They
/// are the sole exception to the broker's ordinary equal-integrity boundary.
/// `cw5n1h2txyewy` is the publisher ID for Windows system packages. Windows
/// has moved `SearchHost` between those packages before, so its package family
/// is bound to its canonical `SystemApps` directory rather than frozen to one
/// release's package name.
const WINDOWS_SYSTEM_PUBLISHER_ID: &str = "_cw5n1h2txyewy";
const LOW_INTEGRITY_LEVEL: u32 = 0x1000;
const MEDIUM_INTEGRITY_LEVEL: u32 = 0x2000;

/// Tray callback (broker window).
const TRAY_MESSAGE: u32 = WM_APP + 1;
/// Hook -> worker: `wParam` is the classified foreground HWND, `lParam` the
/// key generation. Attestation deliberately happens on the worker (issue
/// #121); the hook proves nothing but the window class.
const PROCESS_ENTER: u32 = WM_APP + 2;
/// UI thread -> worker: `lParam` owns a `Box<String>` with the path to open.
const WORKER_OPEN_PATH: u32 = WM_APP + 3;
/// Persist thread -> broker window: show the "could not be saved" balloon on
/// the thread that owns the icon.
const PERSIST_FAILED: u32 = WM_APP + 4;
/// `WinEvent` callback -> worker: re-attest and probe an engine-neutral browser
/// surface. The callback only posts this message and never touches UIA.
const BROWSER_DISCOVERY_PROBE: u32 = WM_APP + 5;
/// Hook -> worker: consume the worker-attested browser cache only after an
/// exact foreground HWND match made with atomic reads in the hook.
const PROCESS_CACHED_BROWSER_ENTER: u32 = WM_APP + 6;
/// Any thread -> worker: install (`wParam` = 1) or remove (`wParam` = 0) the
/// browser-discovery `WinEvent` hooks. They must be created and destroyed on
/// the thread that pumps their callbacks, which is the worker (issue #121),
/// and pausing must take them down (issue #122).
const WORKER_SET_DISCOVERY: u32 = WM_APP + 7;

const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_OBJECT_FOCUS: u32 = 0x8005;
const WINEVENT_OUTOFCONTEXT: u32 = 0;
const WINEVENT_SKIPOWNPROCESS: u32 = 2;

type WinEventProc = unsafe extern "system" fn(isize, u32, HWND, i32, i32, u32, u32);

#[link(name = "user32")]
unsafe extern "system" {
    #[link_name = "SetWinEventHook"]
    fn set_win_event_hook(
        event_min: u32,
        event_max: u32,
        module: isize,
        callback: Option<WinEventProc>,
        process_id: u32,
        thread_id: u32,
        flags: u32,
    ) -> isize;
    #[link_name = "UnhookWinEvent"]
    fn unhook_win_event(hook: isize) -> i32;
}

const TRAY_ID: usize = 1;
const HEALTH_TIMER: usize = 1;
/// Tick interval while a driver is on the other end of the filter port.
const HEALTH_INTERVAL_CONNECTED_MS: u32 = 5_000;
/// Tick interval with no driver — the shipping configuration. Nothing on the
/// tick is urgent: a reconnect probe, the tray-icon retry and the hook re-arm.
const HEALTH_INTERVAL_IDLE_MS: u32 = 60_000;
/// Minimum spacing of the tray-icon retry and the hook re-arm, independent of
/// the tick interval so a connected driver does not re-arm the hook every 5 s.
const MAINTENANCE_INTERVAL_MS: u64 = 60_000;
const REPLAY_MARKER: usize = 0x4653_572F;

/// Shutdown grace for the Enter worker: 50 polls, 10 ms apart.
const WORKER_STOP_ATTEMPTS: u32 = 50;
const WORKER_STOP_POLL_MS: u64 = 10;

/// Ceiling on one `fwdslash integration <id> enable` child. The transaction it
/// runs is a directory copy plus a handful of registry writes; anything past
/// this is a hang (a locked payload file, a wedged `reg.exe`), not slowness,
/// and the sweep must not leave a child of the resident broker running for the
/// rest of the session.
const ADAPTER_UPGRADE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
/// `try_wait` spacing while an upgrade child runs.
const ADAPTER_UPGRADE_POLL_MS: u64 = 250;
/// Balloon retry spacing and count (~10 s) for the sweep's one notification.
const ADAPTER_UPGRADE_NOTIFY_INTERVAL_MS: u64 = 500;
const ADAPTER_UPGRADE_NOTIFY_ATTEMPTS: u32 = 20;
/// Pause before the sweep's single retry of a failed adapter (issue #56).
///
/// The failures this sweep sees right after an MSIX update are timing: the old
/// payload tree still has a `fwdslash.exe` open in a console that just ran a
/// `doskey` macro, or the copy out of `WindowsApps` was still being staged.
/// Both clear in seconds, and a retry that lands is a balloon the user never
/// has to read.
const ADAPTER_RETRY_DELAY_MS: u64 = 5_000;

/// How often the broker even *considers* an update cycle. The real cadence is
/// the CLI's (`check_is_due`, 24 h): this only decides how often it is asked,
/// so a machine that is up for a week still checks daily and one that is up
/// for an hour costs nothing.
const UPDATE_CONSIDER_INTERVAL_MS: u64 = 6 * 60 * 60 * 1_000;
/// Nothing update-related happens for the first five minutes of a broker's
/// life. Logon is the busiest moment on the machine, the adapter sweep is
/// already running, and an update that force-closes the package seconds after
/// the user signed in is the worst possible moment for one.
const UPDATE_FIRST_DELAY_MS: u64 = 5 * 60 * 1_000;
/// Ceiling on `fwdslash update check`. A Store round trip on a bad network,
/// or `curl.exe` against GitHub, both answer or give up well inside this.
const UPDATE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
/// Ceiling on `fwdslash update install`. The child only *starts* the install —
/// the Store's own download runs in the Store's service, and the relaunch is a
/// scheduled task — so this bounds a handful of `WinRT` calls plus the CLI's
/// three-minute admission window, not a download. Killing it past that leaves
/// the queued item and the watchdog in place, so nothing is lost but the
/// exit code.
const UPDATE_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// Consecutive `update install` errors before the broker says so out loud.
/// One is noise (the Store was mid-something); two in a row is a state the
/// user's own click can get out of.
const UPDATE_FAILURES_BEFORE_BALLOON: u32 = 2;
/// `CREATE_NO_WINDOW`. `fwdslash.exe` is a console binary, and the sweep runs
/// unattended at logon: without this every outdated adapter flashes a console
/// window on the user's desktop.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const MENU_SETTINGS: u32 = 1001;
const MENU_PAUSE: u32 = 1002;
const MENU_EXIT: u32 = 1003;
const MENU_OPEN_ROOT: u32 = 1004;
const MENU_WINDOWS: u32 = 1005;
const MENU_CMD: u32 = 1006;
const MENU_WINDOWS_POWERSHELL: u32 = 1007;
const MENU_POWERSHELL: u32 = 1008;
const MENU_VERSION: u32 = 1009;
/// First id of the "Open distribution" submenu; item *i* is `BASE + i`.
const MENU_DISTRO_BASE: u32 = 1100;
/// The submenu is a convenience, not an inventory: cap it so the id range
/// stays private to the submenu no matter how many distributions exist.
const MENU_DISTRO_MAX: usize = 64;

/// `cmb13` and `edt1`: the path combo and file-name edit of the classic
/// common-item dialog. Their presence is what separates an Open/Save dialog
/// from every other `#32770` (a Find box, a property sheet, a message box).
const DIALOG_PATH_COMBO: i32 = 0x47C;
const DIALOG_FILE_NAME_EDIT: i32 = 0x480;

/// Icon resource id, kept in step with `app.rc` here and in `fsw-settings`.
const IDI_FSW_APP: u16 = 101;

// Port, protocol version and distribution capacity come from `fsw_core`
// (hand copies of `include/fsw_filter_protocol.h`).
const FSW_OPERATION_REPLACE_MAPPINGS: u32 = 1;
const FSW_MAX_DISTRIBUTION_NAME: usize = 128;

#[repr(C)]
struct FswMappingMessage {
    version: u32,
    size: u32,
    operation: u32,
    reserved: u32,
    generation: u64,
    distribution_count: u32,
    distributions: [[u16; FSW_MAX_DISTRIBUTION_NAME]; FSW_FILTER_MAX_DISTRIBUTIONS],
}

#[cfg(windows)]
#[link(name = "fltlib", kind = "raw-dylib")]
unsafe extern "system" {
    fn FilterConnectCommunicationPort(
        lpPortName: *const u16,
        dwOptions: u32,
        lpContext: *const std::ffi::c_void,
        wSizeOfContext: u16,
        lpSecurityAttributes: *mut std::ffi::c_void,
        hPort: *mut HANDLE,
    ) -> i32;

    fn FilterSendMessage(
        hPort: HANDLE,
        lpInBuffer: *const std::ffi::c_void,
        dwInBufferSize: u32,
        lpOutBuffer: *mut std::ffi::c_void,
        dwOutBufferSize: u32,
        lpBytesReturned: *mut u32,
    ) -> i32;
}

/// The supported UI flows. This is deliberately only one field of
/// [`TrustedSurface`], never an authorization decision on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceKind {
    Explorer,
    Run,
    Search,
    CommonDialog,
    Browser,
}

/// The immutable facts that bind an Enter request to the foreground process.
/// The worker repeats this attestation before touching UIA or navigating.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrustedSurface {
    kind: SurfaceKind,
    foreground: HWND,
    pid: u32,
    canonical_image: String,
    package_identity: Option<String>,
    session_id: u32,
    integrity_level: u32,
}

// SAFETY: this record owns no pointed-to memory. `foreground` is an opaque
// Win32 window identity, not a dereferenceable pointer; all consumers compare
// it or pass it back to Win32 and revalidate PID/session/integrity immediately
// before use. The remaining fields are owned values.
unsafe impl Send for TrustedSurface {}

static PAUSED: AtomicBool = AtomicBool::new(false);
/// Pause writes started but not yet finished, so [`reload_settings`] can tell
/// "the registry disagrees with us" from "the registry has not caught up with
/// us yet".
///
/// The tray toggle changes `PAUSED` in memory and persists it off-thread (see
/// [`request_persist_disabled`]), which leaves a window in which the stored
/// value is still the old one. A state-changed broadcast landing inside that
/// window — anyone's, including the one this broker's own write posts — would
/// otherwise be read as an external change and revert the toggle.
static PERSIST_IN_FLIGHT: AtomicU32 = AtomicU32::new(0);
static ENTER_DOWN: AtomicBool = AtomicBool::new(false);
static SUPPRESS_ENTER_UP: AtomicBool = AtomicBool::new(false);
static KEYDOWN_GENERATION: AtomicU64 = AtomicU64::new(0);

static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);
static BROKER_WINDOW: AtomicIsize = AtomicIsize::new(0);
static FILTER_PORT: AtomicIsize = AtomicIsize::new(-1);

/// The worker's message-only window and thread id. The hook reads the window
/// on every swallowed Enter, so it lives in an atomic rather than behind a
/// lock: a keyboard hook must never wait on anything.
static WORKER_WINDOW: AtomicIsize = AtomicIsize::new(0);
static WORKER_THREAD: AtomicU32 = AtomicU32::new(0);
static WORKER_STOPPED: AtomicBool = AtomicBool::new(false);
static WORKER_JOIN: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
/// Full browser attestation lives behind this worker-only cache. The keyboard
/// hook never locks it; it consults only [`CACHED_BROWSER_FOREGROUND`].
static DISCOVERED_BROWSER_SURFACE: Mutex<Option<TrustedSurface>> = Mutex::new(None);
static CACHED_BROWSER_FOREGROUND: AtomicIsize = AtomicIsize::new(0);
static FOREGROUND_EVENT_HOOK: AtomicIsize = AtomicIsize::new(0);
static FOCUS_EVENT_HOOK: AtomicIsize = AtomicIsize::new(0);
/// One FIFO writer prevents rapid pause/resume toggles from completing their
/// registry writes out of order. The sender is intentionally unbounded: a
/// tray command must never wait for a slow `reg.exe` child.
static PERSIST_QUEUE: Mutex<Option<mpsc::Sender<bool>>> = Mutex::new(None);

/// Whether `Shell_NotifyIconW(NIM_ADD)` has actually succeeded. It fails with
/// `ERROR_TIMEOUT` when the shell is busy — precisely the logon moment the
/// MSIX startup task launches us — and every later `NIM_MODIFY` (tooltip,
/// balloon) is silently discarded until an add lands.
static ICON_ADDED: AtomicBool = AtomicBool::new(false);

/// Whether the adapter-upgrade sweep has already been started. One pass per
/// process: the broker is restarted by the logon task and by every product
/// update, which is exactly when the payload can be stale.
static ADAPTER_UPGRADE_STARTED: AtomicBool = AtomicBool::new(false);

/// Current `SetTimer` interval, so the timer is only re-created when the
/// wanted interval actually changes.
static HEALTH_INTERVAL_MS: AtomicU32 = AtomicU32::new(HEALTH_INTERVAL_IDLE_MS);
/// `GetTickCount64` of the last tray/hook maintenance pass.
static LAST_MAINTENANCE_MS: AtomicU64 = AtomicU64::new(0);

/// `GetTickCount64` when this broker started, so the first update cycle can be
/// held off for [`UPDATE_FIRST_DELAY_MS`] without a second timer.
static BROKER_START_MS: AtomicU64 = AtomicU64::new(0);
/// `GetTickCount64` of the last update cycle's *start*; `0` means none has run
/// in this process.
static LAST_UPDATE_TICK_MS: AtomicU64 = AtomicU64::new(0);
/// An update cycle is on the `fsw-update` thread right now. Cleared by
/// [`UpdateCycleGuard`], so no early return can strand it.
static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);
/// The Enter worker is inside a request. An install that force-closes the
/// package while the worker is rewriting an address bar would take the user's
/// keystroke with it, so the update cycle never starts while this is set.
static WORKER_BUSY: AtomicBool = AtomicBool::new(false);
/// The version the last update balloon was about, so one available update
/// produces one balloon however many cycles see it.
/// The dedupe key of the offer the last balloon announced, or `None` when
/// nothing has been announced yet. Keyed rather than stored as the version
/// itself, because an offer with no version is still a distinct thing to
/// announce once (issue #97) — and conflating "nothing announced" with
/// "announced something nameless" made a nameless offer balloon every cycle.
static UPDATE_NOTIFIED_TAG: Mutex<Option<String>> = Mutex::new(None);
/// Consecutive `update install` failures; any other outcome resets it.
static UPDATE_INSTALL_FAILURES: AtomicU32 = AtomicU32::new(0);

/// The distribution list the "Open distribution" submenu was built from, so a
/// click resolves to the name that was on screen rather than to a re-read of
/// the registry.
static MENU_DISTRIBUTIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

thread_local! {
    /// Owned by the worker thread for its whole life. The UI thread never
    /// creates or releases it: a second STA is exactly what keeps UIA,
    /// `ShellExecuteExW` and `Navigate2` off the thread that owns the hook.
    static AUTOMATION: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
}

fn to_u16_vec(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = OsStr::new(s).encode_wide().collect();
    v.push(0);
    v
}

/// A development broker used by the visible browser harness must coexist with
/// the installed broker while remaining inert outside the hook/UIA path it is
/// measuring. Keep this an exact opt-in value so a non-empty inherited
/// environment cannot enable it.
#[must_use]
fn integration_test_mode() -> bool {
    #[cfg(debug_assertions)]
    {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| std::env::var(INTEGRATION_TEST_ENV).as_deref() == Ok("1"))
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[must_use]
fn broker_mutex_name() -> &'static str {
    if integration_test_mode() {
        INTEGRATION_TEST_MUTEX_NAME
    } else {
        MUTEX_NAME
    }
}

#[must_use]
fn broker_window_class() -> &'static str {
    if integration_test_mode() {
        INTEGRATION_TEST_WINDOW_CLASS
    } else {
        FSW_BROKER_WINDOW_CLASS
    }
}

#[must_use]
fn worker_window_class() -> &'static str {
    if integration_test_mode() {
        INTEGRATION_TEST_WORKER_WINDOW_CLASS
    } else {
        WORKER_WINDOW_CLASS
    }
}

#[derive(Clone, Copy)]
struct IntegrationTestTarget {
    foreground: isize,
    pid: u32,
}

/// Test mode is never a second desktop-wide hotkey broker. The harness starts
/// the isolated browser first, discovers its exact top-level HWND/PID, and
/// binds this process to that one window before the hook is installed.
#[must_use]
fn integration_test_target() -> Option<IntegrationTestTarget> {
    static TARGET: std::sync::OnceLock<Option<IntegrationTestTarget>> = std::sync::OnceLock::new();
    *TARGET.get_or_init(|| {
        let foreground = std::env::var(INTEGRATION_TEST_TARGET_HWND_ENV)
            .ok()?
            .parse::<isize>()
            .ok()?;
        let pid = std::env::var(INTEGRATION_TEST_TARGET_PID_ENV)
            .ok()?
            .parse::<u32>()
            .ok()?;
        (foreground != 0 && pid != 0).then_some(IntegrationTestTarget { foreground, pid })
    })
}

#[must_use]
fn integration_target_matches(foreground: HWND) -> bool {
    if !integration_test_mode() {
        return true;
    }
    let Some(target) = integration_test_target() else {
        return false;
    };
    if foreground as isize != target.foreground {
        return false;
    }
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(foreground, &raw mut pid);
    }
    pid == target.pid
}

fn from_u16_slice(slice: &[u16]) -> String {
    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    std::ffi::OsString::from_wide(slice.get(..len).unwrap_or_default())
        .to_string_lossy()
        .into_owned()
}

fn fixed_size_u32<T>() -> Option<u32> {
    u32::try_from(std::mem::size_of::<T>()).ok()
}

fn fixed_size_i32<T>() -> Option<i32> {
    i32::try_from(std::mem::size_of::<T>()).ok()
}

fn tray_id() -> Option<u32> {
    u32::try_from(TRAY_ID).ok()
}

/// Copies UTF-16 text into a zero-initialized Win32 fixed buffer, reserving
/// its final word for the mandatory terminator.
fn copy_wide_truncated(destination: &mut [u16], source: &[u16]) {
    let length = source.len().min(destination.len().saturating_sub(1));
    if let (Some(destination), Some(source)) = (destination.get_mut(..length), source.get(..length))
    {
        destination.copy_from_slice(source);
    }
}

fn log_diagnostic(msg: &str) {
    if let Ok(path) = std::env::var("FSW_DIAGNOSTIC_LOG")
        && !path.is_empty()
    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// Sign-extends one 16-bit word of a packed `WPARAM`/`LPARAM` coordinate pair,
/// the `GET_X_LPARAM`/`GET_Y_LPARAM` macros. A tray icon near the right or
/// bottom edge of a secondary monitor has negative coordinates, and a plain
/// mask would put the menu on the wrong screen.
fn signed_word(value: usize, shift: u32) -> i32 {
    let word = u16::try_from((value >> shift) & 0xFFFF).unwrap_or(0);
    i32::from(i16::from_ne_bytes(word.to_ne_bytes()))
}

fn process_image(process: HANDLE) -> Option<String> {
    unsafe {
        let mut image = [0u16; 1024];
        let mut length = u32::try_from(image.len()).ok()?;
        if QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &raw mut length) == 0 {
            return None;
        }
        let image = from_u16_slice(image.get(..usize::try_from(length).ok()?)?);
        // QueryFullProcessImageNameW is bound to the process handle. Resolve
        // it once more so comparisons are against the canonical on-disk image
        // rather than a caller-controlled spelling of the same path.
        Some(
            std::fs::canonicalize(&image)
                .ok()
                .map_or(image.clone(), |path| path.to_string_lossy().into_owned()),
        )
    }
}

fn package_identity(process: HANDLE) -> Option<String> {
    unsafe {
        let mut length = 0u32;
        if GetPackageFamilyName(process, &raw mut length, std::ptr::null_mut())
            != ERROR_INSUFFICIENT_BUFFER
            || length == 0
        {
            return None;
        }
        let mut name = vec![0u16; usize::try_from(length).ok()?];
        if GetPackageFamilyName(process, &raw mut length, name.as_mut_ptr()) != 0 {
            return None;
        }
        Some(from_u16_slice(&name))
    }
}

fn process_integrity_level(process: HANDLE) -> Option<u32> {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &raw mut token) == 0 {
            return None;
        }
        let mut bytes = 0u32;
        let probe = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::null_mut(),
            0,
            &raw mut bytes,
        );
        if probe != 0 || GetLastError() != ERROR_INSUFFICIENT_BUFFER || bytes == 0 {
            CloseHandle(token);
            return None;
        }
        let Ok(storage_bytes) = usize::try_from(bytes) else {
            CloseHandle(token);
            return None;
        };
        // Token information contains pointer-aligned fields. A `Vec<usize>`
        // gives the returned `TOKEN_MANDATORY_LABEL` its required alignment.
        let mut storage = vec![0usize; storage_bytes.div_ceil(std::mem::size_of::<usize>())];
        if GetTokenInformation(
            token,
            TokenIntegrityLevel,
            storage.as_mut_ptr().cast(),
            bytes,
            &raw mut bytes,
        ) == 0
        {
            CloseHandle(token);
            return None;
        }
        let label = &*storage.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let count = GetSidSubAuthorityCount(label.Label.Sid);
        let level = if count.is_null() || *count == 0 {
            None
        } else {
            let rid = GetSidSubAuthority(label.Label.Sid, u32::from(*count - 1));
            (!rid.is_null()).then(|| *rid)
        };
        CloseHandle(token);
        level
    }
}

fn canonical_image_ends_with(image: &str, suffix: &str) -> bool {
    image
        .replace('/', "\\")
        .to_ascii_lowercase()
        .ends_with(suffix)
}

/// Captures the process facts that make an HWND meaningful. Surface family is
/// deliberately decided separately: known Windows surfaces use class/product
/// recognition, while browser discovery proves the focused UIA control.
fn attest_foreground_identity(foreground: HWND) -> Option<(u32, String, Option<String>, u32, u32)> {
    if foreground.is_null() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(foreground, &raw mut pid) };
    if pid == 0 {
        return None;
    }
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let image = process_image(process);
        let package = package_identity(process);
        let integrity_level = process_integrity_level(process);
        CloseHandle(process);
        let image = image?;
        let integrity_level = integrity_level?;

        let mut session_id = 0u32;
        let mut own_session = 0u32;
        if ProcessIdToSessionId(pid, &raw mut session_id) == 0
            || ProcessIdToSessionId(std::process::id(), &raw mut own_session) == 0
            || session_id != own_session
        {
            log_diagnostic(&format!(
                "event=foreground_rejected pid={pid} reason=session"
            ));
            return None;
        }

        Some((pid, image, package, session_id, integrity_level))
    }
}

/// True only for OS-owned Search and Start executable locations. A package
/// family cannot be inferred from a process basename: it must be a Windows
/// system publisher family and agree with the canonical `SystemApps` directory
/// returned from the opened foreground PID. This permits a future Microsoft
/// repackage of `SearchHost` without accepting a user package.
fn trusted_windows_search_identity(image: &str, package: Option<&str>) -> bool {
    let Some(package) = package else {
        return false;
    };
    let package = package.to_ascii_lowercase();
    let image = image.replace('/', "\\").to_ascii_lowercase();
    let expected_directory = format!("\\windows\\systemapps\\{package}\\");
    package.ends_with(WINDOWS_SYSTEM_PUBLISHER_ID)
        && image.contains(&expected_directory)
        && (image.ends_with("\\searchhost.exe") || image.ends_with("\\startmenuexperiencehost.exe"))
}

/// Search and Start are `AppContainer` UI processes and legitimately use Low
/// IL. No other lower-integrity process is accepted. Their package and image
/// have already been bound by [`trusted_windows_search_identity`]; accepting
/// only Low or Medium prevents an unexpected elevated process from becoming a
/// privileged UIA target.
fn trusted_windows_search_integrity(level: u32) -> bool {
    matches!(level, LOW_INTEGRITY_LEVEL | MEDIUM_INTEGRITY_LEVEL)
}

fn equal_integrity(level: u32) -> bool {
    process_integrity_level(unsafe { GetCurrentProcess() }) == Some(level)
}

fn attest_foreground_surface(foreground: HWND) -> Option<TrustedSurface> {
    let (pid, image, package, session_id, integrity_level) =
        attest_foreground_identity(foreground)?;
    let class = window_class(foreground);
    let is_explorer_window =
        eq_ignore_case(&class, "CabinetWClass") || eq_ignore_case(&class, "ExploreWClass");
    let is_dialog = eq_ignore_case(&class, "#32770");
    let is_core_window = eq_ignore_case(&class, "Windows.UI.Core.CoreWindow");
    let kind = if is_explorer_window
        && canonical_image_ends_with(&image, "\\windows\\explorer.exe")
        && equal_integrity(integrity_level)
    {
        SurfaceKind::Explorer
    } else if is_dialog
        && canonical_image_ends_with(&image, "\\windows\\explorer.exe")
        && equal_integrity(integrity_level)
    {
        SurfaceKind::Run
    } else if is_dialog && equal_integrity(integrity_level) && dialog_has_path_control(foreground) {
        SurfaceKind::CommonDialog
    } else if is_core_window
        && trusted_windows_search_identity(&image, package.as_deref())
        && trusted_windows_search_integrity(integrity_level)
    {
        SurfaceKind::Search
    } else if browser_window_class(&class) && equal_integrity(integrity_level) {
        // Window family is a compatibility hint, not an identity claim.
        // Acceptance additionally requires the attested PID/session/IL
        // and an address control outside any web document.
        SurfaceKind::Browser
    } else {
        return None;
    };

    Some(TrustedSurface {
        kind,
        foreground,
        pid,
        canonical_image: image,
        package_identity: package,
        session_id,
        integrity_level,
    })
}

/// Browser engines do not expose a stable process or top-level window class
/// contract. Their address control, however, is discoverable through UIA.
/// This captures the same immutable process facts as known surfaces; the
/// caller must still prove an address bar before caching or navigating it.
fn attest_browser_surface(foreground: HWND) -> Option<TrustedSurface> {
    let (pid, canonical_image, package_identity, session_id, integrity_level) =
        attest_foreground_identity(foreground)?;
    if !equal_integrity(integrity_level) {
        log_diagnostic(&format!(
            "event=foreground_rejected pid={pid} reason=browser_integrity"
        ));
        return None;
    }
    Some(TrustedSurface {
        kind: SurfaceKind::Browser,
        foreground,
        pid,
        canonical_image,
        package_identity,
        session_id,
        integrity_level,
    })
}

fn window_class(window: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let Ok(capacity) = i32::try_from(buf.len()) else {
            return String::new();
        };
        let len = GetClassNameW(window, buf.as_mut_ptr(), capacity);
        if len <= 0 {
            return String::new();
        }
        usize::try_from(len)
            .ok()
            .and_then(|len| buf.get(..len))
            .map_or_else(String::new, from_u16_slice)
    }
}

/// Whether a `#32770` looks like a file-picker rather than any other dialog:
/// either the modern common-item dialog's `DirectUI` view, or one of the two
/// classic dialog controls that carry a path.
fn dialog_has_path_control(dialog: HWND) -> bool {
    unsafe {
        let dui = to_u16_vec("DUIViewWndClassName");
        if !FindWindowExW(dialog, std::ptr::null_mut(), dui.as_ptr(), std::ptr::null()).is_null() {
            return true;
        }
        !GetDlgItem(dialog, DIALOG_PATH_COMBO).is_null()
            || !GetDlgItem(dialog, DIALOG_FILE_NAME_EDIT).is_null()
    }
}

/// Runs inside the low-level hook on every Enter in every application, so the
/// window class — a fixed-size read of the calling process's own memory —
/// gates everything else. Only when the class is one of the four the product
/// supports is the process image worth an `OpenProcess`.
fn send_virtual_key(key: u16) -> bool {
    unsafe {
        let mut inputs: [INPUT; 2] = std::mem::zeroed();
        inputs[0].r#type = INPUT_KEYBOARD;
        inputs[0].Anonymous.ki.wVk = key;
        inputs[0].Anonymous.ki.dwExtraInfo = REPLAY_MARKER;

        inputs[1].r#type = INPUT_KEYBOARD;
        inputs[1].Anonymous.ki.wVk = key;
        inputs[1].Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
        inputs[1].Anonymous.ki.dwExtraInfo = REPLAY_MARKER;

        fixed_size_i32::<INPUT>().is_some_and(|size| SendInput(2, inputs.as_mut_ptr(), size) == 2)
    }
}

fn replay_enter() {
    #[cfg(test)]
    if CAPTURE_REPLAY_ENTER.load(Ordering::Acquire) {
        REPLAY_ENTER_CAPTURED.store(true, Ordering::Release);
        return;
    }
    if !send_virtual_key(VK_RETURN) {
        log_diagnostic("event=replay_enter_failed");
    }
}

#[cfg(test)]
static CAPTURE_REPLAY_ENTER: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static REPLAY_ENTER_CAPTURED: AtomicBool = AtomicBool::new(false);

fn read_focused_value(focused: &IUIAutomationElement) -> Option<String> {
    unsafe {
        if let Ok(variant) = focused.GetCurrentPropertyValue(UIA_ValueValuePropertyId)
            && variant.vt() == VT_BSTR
        {
            let s = variant.Anonymous.Anonymous.Anonymous.bstrVal.to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }

        if let Ok(legacy) = focused.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
            UIA_LegacyIAccessiblePatternId,
        ) && let Ok(bstr) = legacy.CurrentValue()
        {
            let s = bstr.to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
        None
    }
}

/// The focused element as a writable, non-password text field, or `None`.
///
/// Window-class detection cannot tell a file dialog's path box from a Find
/// box that happens to live in a `#32770`; this can. Requiring the pattern up
/// front also means the broker never reads text it could not have written
/// back, which is the promise PRIVACY.md makes. Applied to every surface —
/// Explorer, Run and Search all focus an edit control by construction, so the
/// gate costs three property reads and rejects nothing there.
fn editable_value_pattern(focused: &IUIAutomationElement) -> Option<IUIAutomationValuePattern> {
    unsafe {
        let control_type = focused.CurrentControlType().ok()?;
        if control_type != UIA_EditControlTypeId && control_type != UIA_ComboBoxControlTypeId {
            return None;
        }
        let Ok(password) = focused.CurrentIsPassword() else {
            return None;
        };
        if password.as_bool() {
            return None;
        }
        let pattern = focused
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()?;
        let Ok(read_only) = pattern.CurrentIsReadOnly() else {
            return None;
        };
        if read_only.as_bool() {
            return None;
        }
        Some(pattern)
    }
}

fn set_pattern_value(pattern: &IUIAutomationValuePattern, value: &str) -> bool {
    let bstr = BSTR::from(value);
    unsafe { pattern.SetValue(&bstr) }.is_ok()
}

/// Opens a resolved location. Always called from the worker: binding
/// `\\wsl.localhost\<distro>` boots a stopped distribution, which takes
/// seconds. `SEE_MASK_ASYNCOK` lets the shell finish the launch on its own
/// thread and `SEE_MASK_FLAG_NO_UI` keeps a failure from parking a modal
/// error box on a window nobody can see.
fn open_resolved_path(path: &str) -> bool {
    unsafe {
        let wide_verb = to_u16_vec("open");
        let wide_file = to_u16_vec(path);
        let mut exec: SHELLEXECUTEINFOW = std::mem::zeroed();
        let Some(size) = fixed_size_u32::<SHELLEXECUTEINFOW>() else {
            return false;
        };
        exec.cbSize = size;
        exec.fMask = SEE_MASK_ASYNCOK | SEE_MASK_FLAG_NO_UI;
        exec.lpVerb = wide_verb.as_ptr();
        exec.lpFile = wide_file.as_ptr();
        exec.nShow = SW_SHOWNORMAL;
        ShellExecuteExW(&raw mut exec) != 0
    }
}

fn surface_is_current(surface: &TrustedSurface) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    let current = if surface.kind == SurfaceKind::Browser {
        attest_browser_surface(foreground)
    } else {
        attest_foreground_surface(foreground)
    };
    current.is_some_and(|current| current == *surface)
}

fn browser_window_class(class: &str) -> bool {
    class.eq_ignore_ascii_case("Chrome_WidgetWin_1")
        || class.eq_ignore_ascii_case("MozillaWindowClass")
}

fn address_identity(name: &str, automation_id: &str, accelerator: &str) -> (bool, bool) {
    let id = automation_id.to_ascii_lowercase();
    let accelerator = accelerator.to_ascii_lowercase().replace(' ', "");
    let stable = matches!(
        id.as_str(),
        "addresseditbox" | "urlbar-input" | "urlbar" | "omnibox"
    ) || matches!(accelerator.as_str(), "ctrl+l" | "alt+d");
    let name = name.to_ascii_lowercase();
    (
        stable,
        name.contains("address") || name.contains("location") || name.contains("url"),
    )
}

fn focused_belongs_to_surface(focused: &IUIAutomationElement, surface: &TrustedSurface) -> bool {
    let Some(automation) = AUTOMATION.with_borrow(Option::clone) else {
        return false;
    };
    let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
        return false;
    };
    let mut node = focused.clone();
    // Chromium address bars are virtual UIA elements (HWND == 0). Walk to
    // their native ancestor while requiring the same attested PID throughout.
    for _ in 0..32 {
        if unsafe { node.CurrentProcessId() }
            .ok()
            .and_then(|pid| u32::try_from(pid).ok())
            != Some(surface.pid)
        {
            return false;
        }
        if surface.kind == SurfaceKind::Browser
            && unsafe { node.CurrentControlType() }.ok() == Some(UIA_DocumentControlTypeId)
        {
            return false;
        }
        let Ok(native) = (unsafe { node.CurrentNativeWindowHandle() }) else {
            return false;
        };
        if !native.0.is_null() {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(native.0, &raw mut pid) };
            return pid == surface.pid
                && unsafe { GetAncestor(native.0, GA_ROOT) } == surface.foreground;
        }
        let Ok(parent) = (unsafe { walker.GetParentElement(&node) }) else {
            return false;
        };
        node = parent;
    }
    false
}

fn is_browser_address_bar(focused: &IUIAutomationElement) -> bool {
    let name = unsafe { focused.CurrentName() }
        .map(|value| value.to_string())
        .unwrap_or_default();
    let id = unsafe { focused.CurrentAutomationId() }
        .map(|value| value.to_string())
        .unwrap_or_default();
    let accelerator = unsafe { focused.CurrentAcceleratorKey() }
        .map(|value| value.to_string())
        .unwrap_or_default();
    let (stable, named) = address_identity(&name, &id, &accelerator);
    if !stable && !named {
        return false;
    }
    let Some(automation) = AUTOMATION.with_borrow(Option::clone) else {
        return false;
    };
    let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
        return false;
    };
    let Ok(pid) = (unsafe { focused.CurrentProcessId() }) else {
        return false;
    };
    let mut node = focused.clone();
    let mut toolbar = false;
    for _ in 0..32 {
        if unsafe { node.CurrentProcessId() }.ok() != Some(pid) {
            return false;
        }
        let Ok(kind) = (unsafe { node.CurrentControlType() }) else {
            return false;
        };
        if kind == UIA_DocumentControlTypeId {
            return false;
        }
        toolbar |= kind == UIA_ToolBarControlTypeId;
        if let Ok(native) = unsafe { node.CurrentNativeWindowHandle() }
            && !native.0.is_null()
        {
            return stable || (named && toolbar);
        }
        let Ok(parent) = (unsafe { walker.GetParentElement(&node) }) else {
            return false;
        };
        node = parent;
    }
    false
}

fn trusted_editable_value_pattern(
    focused: &IUIAutomationElement,
    surface: &TrustedSurface,
) -> Option<IUIAutomationValuePattern> {
    if !focused_belongs_to_surface(focused, surface)
        || (surface.kind == SurfaceKind::Browser && !is_browser_address_bar(focused))
    {
        return None;
    }
    editable_value_pattern(focused)
}

fn clear_discovered_browser_surface() {
    // The mutex serializes against `cache_discovered_browser_surface`, so the
    // atomic must be zeroed while holding it: zeroing before the lock let a
    // concurrent cache repopulate the pair as (stale atomic, empty mutex),
    // which the hook read as a hit and the worker read as a miss.
    if let Ok(mut cached) = DISCOVERED_BROWSER_SURFACE.lock() {
        *cached = None;
        CACHED_BROWSER_FOREGROUND.store(0, Ordering::Release);
    }
}

fn cache_discovered_browser_surface(surface: TrustedSurface) {
    CACHED_BROWSER_FOREGROUND.store(0, Ordering::Release);
    if let Ok(mut cached) = DISCOVERED_BROWSER_SURFACE.lock() {
        let foreground = surface.foreground;
        *cached = Some(surface);
        CACHED_BROWSER_FOREGROUND.store(foreground as isize, Ordering::Release);
    }
}

/// Drop the fast-path publication, but only when it names this window: the
/// cache is a single slot and clearing it unconditionally would throw away a
/// different window's still-valid attestation.
fn invalidate_cached_browser_surface(window: HWND) {
    if !window.is_null() && CACHED_BROWSER_FOREGROUND.load(Ordering::Acquire) == window as isize {
        clear_discovered_browser_surface();
    }
}

fn cached_browser_surface(foreground: HWND) -> Option<TrustedSurface> {
    if CACHED_BROWSER_FOREGROUND.load(Ordering::Acquire) != foreground as isize {
        return None;
    }
    DISCOVERED_BROWSER_SURFACE.lock().ok().and_then(|cached| {
        cached
            .as_ref()
            .filter(|surface| surface.foreground == foreground)
            .cloned()
    })
}

/// Worker-only probe for arbitrary browser engines. The `WinEvent` callback is
/// intentionally limited to posting an HWND; all process/UIA calls happen
/// here on the existing STA worker.
fn probe_browser_surface(candidate: HWND) {
    let foreground = unsafe { GetForegroundWindow() };
    let root = unsafe { GetAncestor(candidate, GA_ROOT) };
    // Every rejection below also *invalidates* (issue #122): focus moving from
    // the omnibox to an ordinary page field is a focus event inside the very
    // window the cache names, and leaving the entry valid there is what made
    // the hook swallow an Enter meant for a web form.
    if root.is_null() || root != foreground {
        invalidate_cached_browser_surface(root);
        return;
    }
    let Some(surface) = attest_browser_surface(root) else {
        invalidate_cached_browser_surface(root);
        return;
    };
    let Some(automation) = AUTOMATION.with_borrow(Option::clone) else {
        invalidate_cached_browser_surface(root);
        return;
    };
    let Ok(focused) = (unsafe { automation.GetFocusedElement() }) else {
        invalidate_cached_browser_surface(root);
        return;
    };
    if trusted_editable_value_pattern(&focused, &surface).is_some()
        && request_control_is_current(&automation, &focused, &surface)
    {
        cache_discovered_browser_surface(surface);
    } else {
        invalidate_cached_browser_surface(root);
    }
}

fn process_cached_browser_enter(foreground: HWND, generation: u64) {
    // A focus event from any application can invalidate the cache between the
    // hook's cache hit and this read, swallowing the Enter into nothing. The
    // sink re-attests instead: when the same window is still foreground and
    // still attests, the transaction proceeds exactly as the cached path
    // would have; otherwise the drop is at least logged.
    let surface = cached_browser_surface(foreground).or_else(|| {
        let reattested = attest_foreground_surface(foreground)
            .filter(|surface| surface.kind == SurfaceKind::Browser);
        if reattested.is_none() {
            log_diagnostic("event=browser_enter_dropped_cache_miss");
        }
        reattested
    });
    let Some(surface) = surface else {
        replay_enter_for_window(foreground, generation);
        return;
    };
    process_enter_request(&surface, generation);
}

unsafe extern "system" fn browser_discovery_event_proc(
    _hook: isize,
    _event: u32,
    hwnd: HWND,
    _object_id: i32,
    _child_id: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if hwnd.is_null() {
        return;
    }
    // Only browser-engine windows can ever become a discovered surface. The
    // desktop-wide focus stream would otherwise wake the worker (and its
    // process/UIA probes) for every unrelated window focus change — a pure
    // energy and CPU cost. Class comparison runs on the stack, no allocation.
    let mut class_buf = [0u16; 64];
    let Ok(capacity) = i32::try_from(class_buf.len()) else {
        return;
    };
    let class_len = unsafe { GetClassNameW(hwnd, class_buf.as_mut_ptr(), capacity) };
    let class_len = usize::try_from(class_len).unwrap_or(0).min(class_buf.len());
    let is_browser_engine = {
        let chars = class_buf.get(..class_len).unwrap_or(&[]);
        let matches_ci = |expected: &str| {
            expected.len() == chars.len()
                && expected
                    .bytes()
                    .zip(chars)
                    .all(|(e, c)| u8::try_from(*c).is_ok_and(|c| e.eq_ignore_ascii_case(&c)))
        };
        matches_ci("Chrome_WidgetWin_1") || matches_ci("MozillaWindowClass")
    };
    if !is_browser_engine {
        return;
    }
    // Focus and foreground are asynchronous with physical input. An event from
    // *outside* the attested window invalidates immediately; one from inside it
    // cannot, because focus churn (caret, child element, tab switch within the
    // same frame) would tear down the cache the hook is mid-way through using.
    // Either way the worker re-proves the address bar, and `probe_browser_surface`
    // invalidates when the new focus is not one (issue #122).
    let cached = CACHED_BROWSER_FOREGROUND.load(Ordering::Acquire);
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if cached == 0 || root.is_null() || root as isize != cached {
        CACHED_BROWSER_FOREGROUND.store(0, Ordering::Release);
    }
    let worker = WORKER_WINDOW.load(Ordering::Acquire) as HWND;
    if !worker.is_null() {
        unsafe {
            PostMessageW(worker, BROWSER_DISCOVERY_PROBE, hwnd as WPARAM, 0);
        }
    }
}

/// Ask the worker to install or remove the `WinEvent` hooks. `SetWinEventHook`
/// binds the hook to the calling thread's message pump, so both calls have to
/// happen there: installing them from the thread that owns the keyboard hook
/// turned every desktop focus change into a callback on the hook thread
/// (issue #121).
fn request_browser_discovery(enabled: bool) {
    let worker = WORKER_WINDOW.load(Ordering::Acquire) as HWND;
    if !worker.is_null() {
        unsafe {
            PostMessageW(worker, WORKER_SET_DISCOVERY, usize::from(enabled), 0);
        }
    }
}

/// Worker thread only — see [`request_browser_discovery`].
fn install_browser_discovery() {
    if WORKER_WINDOW.load(Ordering::Acquire) == 0
        || FOREGROUND_EVENT_HOOK.load(Ordering::Acquire) != 0
        || FOCUS_EVENT_HOOK.load(Ordering::Acquire) != 0
    {
        return;
    }
    let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
    let foreground = unsafe {
        set_win_event_hook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            0,
            Some(browser_discovery_event_proc),
            0,
            0,
            flags,
        )
    };
    if foreground == 0 {
        log_diagnostic("event=browser_discovery_foreground_hook_failed");
        return;
    }
    let focus = unsafe {
        set_win_event_hook(
            EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_FOCUS,
            0,
            Some(browser_discovery_event_proc),
            0,
            0,
            flags,
        )
    };
    if focus == 0 {
        unsafe {
            unhook_win_event(foreground);
        }
        log_diagnostic("event=browser_discovery_focus_hook_failed");
        return;
    }
    FOREGROUND_EVENT_HOOK.store(foreground, Ordering::Release);
    FOCUS_EVENT_HOOK.store(focus, Ordering::Release);
}

/// Worker thread only — see [`request_browser_discovery`].
fn remove_browser_discovery() {
    let foreground = FOREGROUND_EVENT_HOOK.swap(0, Ordering::AcqRel);
    let focus = FOCUS_EVENT_HOOK.swap(0, Ordering::AcqRel);
    if foreground != 0 {
        unsafe {
            unhook_win_event(foreground);
        }
    }
    if focus != 0 {
        unsafe {
            unhook_win_event(focus);
        }
    }
    clear_discovered_browser_surface();
}

/// Opens only a directory or selects a file, never invoking an association.
fn perform_navigation(action: &NavigationAction) -> bool {
    let result = action.navigate_if(|| true);
    if result.is_err() {
        log_diagnostic("event=navigation_failed");
    }
    result.is_ok()
}

/// Post to the worker; a missing worker must never block the keyboard hook.
fn request_open_path(path: String) {
    let worker = WORKER_WINDOW.load(Ordering::Relaxed) as HWND;
    let owned = Box::into_raw(Box::new(path));
    if !worker.is_null()
        && unsafe { PostMessageW(worker, WORKER_OPEN_PATH, 0, owned as LPARAM) } != 0
    {
        return;
    }
    drop(unsafe { Box::from_raw(owned) });
    log_diagnostic("event=worker_open_path_dropped");
    show_notification("The location could not be opened right now.", NIIF_ERROR);
}

fn navigate_explorer_window(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    surface: &TrustedSurface,
    path: &str,
) -> bool {
    unsafe {
        let Ok(shell_windows) =
            CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_LOCAL_SERVER)
        else {
            return false;
        };
        let Ok(count) = shell_windows.Count() else {
            return false;
        };
        for i in 0..count {
            let item = VARIANT::from(i);
            if let Ok(dispatch) = shell_windows.Item(&item)
                && let Ok(browser) = dispatch.cast::<IWebBrowser2>()
                && let Ok(hwnd) = browser.HWND()
                && hwnd.0 == surface.foreground as isize
            {
                if !request_control_is_current(automation, focused, surface) {
                    return false;
                }
                let target = VARIANT::from(path);
                let empty = VARIANT::default();
                return browser
                    .Navigate2(
                        &raw const target,
                        Some(&raw const empty),
                        Some(&raw const empty),
                        Some(&raw const empty),
                        Some(&raw const empty),
                    )
                    .is_ok();
            }
        }
    }
    false
}

fn show_notification(message: &str, flags: u32) {
    let broker_wnd = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
    if broker_wnd.is_null() || !ICON_ADDED.load(Ordering::Relaxed) {
        // NIM_MODIFY against an icon the shell never accepted just fails; the
        // balloon would be lost either way.
        return;
    }
    unsafe {
        let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
        let Some(icon_size) = fixed_size_u32::<NOTIFYICONDATAW>() else {
            log_diagnostic("event=notification_data_size_unrepresentable");
            return;
        };
        let Some(tray_id) = tray_id() else {
            log_diagnostic("event=tray_id_unrepresentable");
            return;
        };
        icon.cbSize = icon_size;
        icon.hWnd = broker_wnd;
        icon.uID = tray_id;
        icon.uFlags = NIF_INFO;
        icon.dwInfoFlags = flags;

        let title = to_u16_vec("Forward Slash Windows");
        let msg_wide = to_u16_vec(message);

        copy_wide_truncated(&mut icon.szInfoTitle, &title);

        copy_wide_truncated(&mut icon.szInfo, &msg_wide);

        Shell_NotifyIconW(NIM_MODIFY, &raw const icon);
    }
}

#[must_use]
fn request_target_is_current(expected: HWND, current: HWND) -> bool {
    expected == current
}

#[must_use]
fn should_swallow_enter(worker_present: bool, post_succeeded: bool) -> bool {
    worker_present && post_succeeded
}

/// Revalidate both the exact focused element and the attested foreground
/// process after every potentially blocking UIA/native navigation preparation.
fn request_control_is_current(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    surface: &TrustedSurface,
) -> bool {
    if !request_target_is_current(surface.foreground, unsafe { GetForegroundWindow() })
        || !surface_is_current(surface)
    {
        return false;
    }
    let Ok(current) = (unsafe { automation.GetFocusedElement() }) else {
        return false;
    };
    if !unsafe { automation.CompareElements(focused, &current) }.is_ok_and(BOOL::as_bool)
        || !focused_belongs_to_surface(&current, surface)
    {
        log_diagnostic("event=enter_dropped_control_changed");
        return false;
    }
    // The same UIA element can change its role or writable state while a
    // path is being resolved. Browser navigation must re-prove the complete
    // address-bar predicate at each sink, not merely its PID/HWND ancestry.
    if surface.kind == SurfaceKind::Browser
        && trusted_editable_value_pattern(&current, surface).is_none()
    {
        log_diagnostic("event=enter_dropped_browser_control_rejected");
        return false;
    }
    surface_is_current(surface)
}

/// The hook swallowed the key-down before the worker ever saw it, so a worker
/// path that declines to translate must hand the Enter back rather than let it
/// vanish (issue #122). Replaying into a window the user has since left would
/// be worse than the drop, so both the foreground window and the key
/// generation must still be the ones the hook observed.
fn replay_enter_for_window(foreground: HWND, generation: u64) {
    if generation == KEYDOWN_GENERATION.load(Ordering::Acquire)
        && request_target_is_current(foreground, unsafe { GetForegroundWindow() })
    {
        replay_enter();
    }
}

fn replay_enter_if_current(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    surface: &TrustedSurface,
) {
    // Returning the user's Enter does not grant permission to rewrite text.
    // Webpage controls may belong to a renderer process and must retain their
    // ordinary Enter behavior even though they fail the address-bar gate.
    if !surface_is_current(surface) {
        return;
    }
    let Ok(current) = (unsafe { automation.GetFocusedElement() }) else {
        return;
    };
    if unsafe { automation.CompareElements(focused, &current) }.is_ok_and(BOOL::as_bool)
        && surface_is_current(surface)
    {
        replay_enter();
    }
}

const BROWSER_CAPTURE_BUDGET_MS: u64 = 250;
const BROWSER_CAPTURE_STABLE_MS: u64 = 60;
const BROWSER_CAPTURE_MIN_SAMPLES: u32 = 3;
const BROWSER_CAPTURE_INTERVAL_MS: u64 = 10;

#[derive(Debug, Default)]
struct StableInputCapture {
    value: Option<String>,
    stable_since_ms: u64,
    samples: u32,
}

impl StableInputCapture {
    fn observe(&mut self, value: String, now_ms: u64) -> bool {
        if self.value.as_deref() != Some(&value) {
            self.value = Some(value);
            self.stable_since_ms = now_ms;
            self.samples = 1;
            return false;
        }
        self.samples = self.samples.saturating_add(1);
        self.samples >= BROWSER_CAPTURE_MIN_SAMPLES
            && now_ms.saturating_sub(self.stable_since_ms) >= BROWSER_CAPTURE_STABLE_MS
    }
}

fn browser_capture_sample(
    automation: &IUIAutomation,
    surface: &TrustedSurface,
    expected_profile: &str,
    generation: u64,
) -> Option<String> {
    if generation != KEYDOWN_GENERATION.load(Ordering::Acquire) || !surface_is_current(surface) {
        return None;
    }
    let current = unsafe { automation.GetFocusedElement() }.ok()?;
    let process_id = u32::try_from(unsafe { current.CurrentProcessId() }.ok()?).ok()?;
    if process_id != surface.pid || editable_value_pattern(&current).is_none() {
        return None;
    }
    let browser::FocusEligibility::Eligible { profile, .. } =
        browser::focused_field_eligibility(automation, &current, surface.foreground)
    else {
        return None;
    };
    if profile != expected_profile || !browser::plain_browser_enter_is_safe(surface.foreground) {
        return None;
    }
    let value = read_focused_value(&current)?;
    (generation == KEYDOWN_GENERATION.load(Ordering::Acquire) && surface_is_current(surface))
        .then_some(value)
}

/// Wait without starving the worker STA (issue #126). A plain `Sleep` blocks
/// inbound cross-apartment COM calls — which is precisely the traffic the
/// browser's UIA provider makes back into this thread while the capture is
/// sampling it — because those arrive as *sent* messages. A `MsgWaitFor*` wait
/// keeps sent messages dispatched for the whole interval. It deliberately does
/// not pump *posted* messages: a nested `PROCESS_ENTER` dispatched from inside
/// a capture would re-enter this transaction.
fn wait_pumping_sent_messages(ms: u64) {
    let ms = u32::try_from(ms).unwrap_or(u32::MAX);
    unsafe {
        MsgWaitForMultipleObjectsEx(0, std::ptr::null(), ms, QS_SENDMESSAGE, 0);
    }
}

fn capture_stable_browser_input(
    automation: &IUIAutomation,
    surface: &TrustedSurface,
    profile: &str,
    generation: u64,
) -> Option<String> {
    let started = unsafe { GetTickCount64() };
    let mut capture = StableInputCapture::default();
    loop {
        let now = unsafe { GetTickCount64() };
        let value = browser_capture_sample(automation, surface, profile, generation)?;
        if capture.observe(value.clone(), now) {
            let final_value = browser_capture_sample(automation, surface, profile, generation)?;
            return (final_value == value).then_some(value);
        }
        if now.saturating_sub(started) >= BROWSER_CAPTURE_BUDGET_MS {
            return None;
        }
        // The loop's own `BROWSER_CAPTURE_BUDGET_MS` check above bounds the
        // total time spent here regardless of how the wait returns.
        wait_pumping_sent_messages(BROWSER_CAPTURE_INTERVAL_MS);
    }
}

fn replay_browser_enter_if_current(
    automation: &IUIAutomation,
    surface: &TrustedSurface,
    expected_profile: &str,
    expected_value: Option<&str>,
    generation: u64,
) {
    // `SetValue` is allowed to replace Chromium's virtual omnibox element.
    // Do not reuse the capture helper here: its pre-write modifier/IME gate
    // can observe transient post-Enter state and reject a legitimate
    // replacement. The tested transaction binding is the replacement's
    // profile, foreground HWND, PID, written value and key generation.
    if !surface_is_current(surface) {
        log_diagnostic("event=browser_enter_dropped_foreground_changed");
        return;
    }
    let Ok(current) = (unsafe { automation.GetFocusedElement() }) else {
        log_diagnostic("event=browser_enter_dropped_focus_unavailable");
        return;
    };
    let Ok(process_id) = (unsafe { current.CurrentProcessId() }) else {
        log_diagnostic("event=browser_enter_dropped_replacement_pid_unavailable");
        return;
    };
    if u32::try_from(process_id).ok() != Some(surface.pid)
        || editable_value_pattern(&current).is_none()
    {
        log_diagnostic("event=browser_enter_dropped_replacement_not_editable");
        return;
    }
    let browser::FocusEligibility::Eligible { profile, .. } =
        browser::focused_field_eligibility(automation, &current, surface.foreground)
    else {
        log_diagnostic("event=browser_enter_dropped_replacement_unverified");
        return;
    };
    let Some(value) = read_focused_value(&current) else {
        log_diagnostic("event=browser_enter_dropped_replacement_value_unavailable");
        return;
    };
    if profile != expected_profile
        || unsafe { GetForegroundWindow() } != surface.foreground
        || expected_value.is_some_and(|expected| expected != value)
        || generation != KEYDOWN_GENERATION.load(Ordering::Acquire)
        || !surface_is_current(surface)
    {
        log_diagnostic("event=browser_enter_dropped_transaction_changed");
        return;
    }
    replay_enter();
}

/// Browser address bars replace their UIA element after `SetValue`. This path
/// is separate from normal shell fields: it samples the replacement, requires
/// the same observed browser profile/PID/window, then replays the user's
/// original Enter only if the value still matches the transaction.
fn process_browser_enter_request(surface: &TrustedSurface, generation: u64) {
    if generation != KEYDOWN_GENERATION.load(Ordering::Acquire) || !surface_is_current(surface) {
        return;
    }
    let Some(automation) = AUTOMATION.with_borrow(Option::clone) else {
        replay_enter_for_window(surface.foreground, generation);
        return;
    };
    let Ok(focused) = (unsafe { automation.GetFocusedElement() }) else {
        log_diagnostic("event=browser_enter_replayed_focus_unavailable");
        replay_enter_for_window(surface.foreground, generation);
        return;
    };
    let Some(pattern) = editable_value_pattern(&focused) else {
        // A browser window can contain ordinary webpage controls (including
        // password fields). They are not navigation targets, but the broker
        // already swallowed the physical Enter, so return it to the unchanged
        // focused control rather than turning Enter into a dead key.
        replay_enter_if_current(&automation, &focused, surface);
        return;
    };
    let browser::FocusEligibility::Eligible { family, profile } =
        browser::focused_field_eligibility(&automation, &focused, surface.foreground)
    else {
        replay_enter_if_current(&automation, &focused, surface);
        return;
    };
    if !browser::plain_browser_enter_is_safe(surface.foreground) {
        replay_enter_if_current(&automation, &focused, surface);
        return;
    }
    let Some(input) = capture_stable_browser_input(&automation, surface, profile, generation)
    else {
        replay_browser_enter_if_current(&automation, surface, profile, None, generation);
        return;
    };
    if !input.starts_with('/') {
        replay_browser_enter_if_current(&automation, surface, profile, Some(&input), generation);
        return;
    }
    let snapshot = Snapshot::current();
    let target = match resolve_user_target(&input, &snapshot, None) {
        Ok(target) => target,
        Err(_error) => {
            // TargetError deliberately has no stable user-path diagnostic
            // spelling. Keep the broker log categorical. The Enter is not
            // replayed (replaying would make the browser search for the
            // untranslated text), but the user gets an explanation instead
            // of a dead key.
            log_diagnostic("event=path_rejected reason=target");
            show_notification(
                "That path could not be resolved to a location on this machine.",
                NIIF_WARNING,
            );
            return;
        }
    };
    let Ok(file_uri) = target.file_uri() else {
        log_diagnostic("event=browser_file_uri_rejected");
        show_notification(
            "That path resolves to a destination the browser cannot be given.",
            NIIF_WARNING,
        );
        return;
    };
    // Gecko's Windows file handler uses a complete UNC path after an empty
    // authority (RFC 8089 E.3.2). Preserve the server instead of letting it
    // interpret an authority-form URI as a local path. Chromium and drive
    // paths retain the resolver's existing representation and escaping.
    let file_uri =
        if family == browser::BrowserFamily::Gecko && target.native_path().starts_with(r"\\") {
            file_uri.replacen("file://", "file://///", 1)
        } else {
            file_uri
        };
    if browser_capture_sample(&automation, surface, profile, generation).as_deref() != Some(&input)
    {
        log_diagnostic("event=browser_enter_dropped_prewrite_value_changed");
        return;
    }
    if set_pattern_value(&pattern, &file_uri) {
        replay_browser_enter_if_current(&automation, surface, profile, Some(&file_uri), generation);
    } else {
        log_diagnostic("event=browser_value_set_failed");
        replay_browser_enter_if_current(&automation, surface, profile, Some(&input), generation);
    }
}

/// The worker's entry point for a hook-classified Enter. Attestation lives here
/// and not in the hook (issue #121): `OpenProcess`, `QueryFullProcessImageNameW`,
/// `fs::canonicalize`, `GetPackageFamilyName` and two token reads are all
/// capable of a disk stall, and a `WH_KEYBOARD_LL` callback that outruns
/// `LowLevelHooksTimeout` is removed by Windows without notice. The hook has
/// already swallowed the key-down, so a window that fails to attest gets its
/// Enter back untouched.
fn process_enter_hwnd(foreground: HWND, generation: u64) {
    let Some(surface) = attest_foreground_surface(foreground) else {
        log_diagnostic("event=enter_replayed_unattested");
        replay_enter_for_window(foreground, generation);
        return;
    };
    process_enter_request(&surface, generation);
}

/// Runs on the worker, keeping cross-process UIA and WSL I/O off the hook.
fn process_enter_request(surface: &TrustedSurface, generation: u64) {
    if surface.kind == SurfaceKind::Browser {
        process_browser_enter_request(surface, generation);
        return;
    }
    if !surface_is_current(surface) {
        log_diagnostic("event=enter_dropped_foreground_changed");
        return;
    }
    if PAUSED.load(Ordering::Relaxed) {
        // A pause that lands between the hook's check and this one must still
        // return the keystroke: the hook has already swallowed it (issue #122).
        log_diagnostic("event=enter_replayed_paused");
        replay_enter_for_window(surface.foreground, generation);
        return;
    }
    let Some(automation) = AUTOMATION.with_borrow(Option::clone) else {
        replay_enter_for_window(surface.foreground, generation);
        return;
    };
    let Ok(focused) = (unsafe { automation.GetFocusedElement() }) else {
        log_diagnostic("event=enter_replayed_focus_unavailable");
        replay_enter_for_window(surface.foreground, generation);
        return;
    };
    if !request_control_is_current(&automation, &focused, surface) {
        replay_enter_if_current(&automation, &focused, surface);
        return;
    }
    let Some(pattern) = trusted_editable_value_pattern(&focused, surface) else {
        log_diagnostic("event=surface_rejected");
        replay_enter_if_current(&automation, &focused, surface);
        return;
    };
    let Some(input) = read_focused_value(&focused) else {
        replay_enter_if_current(&automation, &focused, surface);
        return;
    };
    if !input.starts_with('/') {
        replay_enter_if_current(&automation, &focused, surface);
        return;
    }
    let snapshot = Snapshot::current();
    let mut buffer = RenderBuf::new();
    let resolved = match resolve_user_slash_path(&input, &snapshot, &mut buffer) {
        Ok(resolved) => resolved,
        Err(error) => {
            log_diagnostic(&format!("event=path_rejected reason={}", error.name()));
            show_notification(
                &format_resolve_error(error, &snapshot.distributions),
                NIIF_WARNING,
            );
            return;
        }
    };
    let path = resolved.unc_display();
    // Win32 normalization strips a trailing `.`/space on the final component
    // outside the `\\?\` namespace, silently opening a different file (ext4
    // allows `notes` and `notes.` side by side). A trailing separator is
    // preserved, so appending one keeps the component the user named: the
    // directory opens correctly and a file fails visibly instead of opening
    // its dotless twin. Browser sinks are unaffected — file URIs carry the
    // component percent-encoded to the provider.
    let mut translated = path.to_string();
    if resolved.has_win32_normalization_hazard() {
        translated.push('\\');
        log_diagnostic("event=win32_normalization_hazard");
    }
    // Windows Search and Start own their Enter: writing the UNC into the query
    // box and replaying Enter runs a *search* for the text. The sink is a shell
    // open plus an Escape to dismiss the flyout the user is done with.
    if surface.kind == SurfaceKind::Search {
        if !request_control_is_current(&automation, &focused, surface) {
            return;
        }
        if !open_resolved_path(&translated) {
            show_notification("Windows could not open the location.", NIIF_ERROR);
        }
        if request_control_is_current(&automation, &focused, surface) {
            send_virtual_key(VK_ESCAPE);
        }
        return;
    }
    // The WSL provider root is a virtual Explorer location. Navigating the
    // attested current window keeps its breadcrumb/address state synchronized
    // with the folder view; generic PIDL opening can reuse the view while
    // leaving the old filesystem address visible.
    if surface.kind == SurfaceKind::Explorer
        && resolved.is_provider_root()
        && navigate_explorer_window(&automation, &focused, surface, path)
    {
        return;
    }
    // Preserve the original surface's semantics. The broker translates the
    // physical user's text but does not decide whether the result is a file,
    // directory, link, executable, or provider object. Windows/the browser
    // receives the translated value and handles Enter exactly as it normally
    // would. Security comes from authenticating and revalidating the surface,
    // not from replacing native path traversal with a second policy engine.
    if !request_control_is_current(&automation, &focused, surface) {
        return;
    }
    if !set_pattern_value(&pattern, &translated) {
        log_diagnostic("event=value_write_failed");
    }
    // A failed write passes the original Enter through; there is no broker-side
    // ShellExecute fallback.
    replay_enter_if_current(&automation, &focused, surface);
}

#[must_use]
fn input_can_drive_broker(flags: u32) -> bool {
    flags & LLKHF_LOWER_IL_INJECTED == 0
}

/// Cheap gate inside the hook: only these window classes can ever attest to a
/// trusted surface, and reading a class is a fixed-size read of the calling
/// process's own memory. Everything else — Notepad, terminals, games — skips
/// the per-process identity work (an `OpenProcess`, `fs::canonicalize`, token
/// reads) that must not run per-Enter: a low-level hook past
/// `LowLevelHooksTimeout` is removed by Windows without notice.
fn is_candidate_foreground_class(foreground: HWND) -> bool {
    let class = window_class(foreground);
    eq_ignore_case(&class, "CabinetWClass")
        || eq_ignore_case(&class, "ExploreWClass")
        || eq_ignore_case(&class, "#32770")
        || eq_ignore_case(&class, "Windows.UI.Core.CoreWindow")
        || browser_window_class(&class)
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code != 0 || lparam == 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let key_up = wparam == WM_KEYUP as usize
        || wparam == WM_SYSKEYUP as usize
        || (key.flags & LLKHF_UP) != 0;
    // The browser worker waits briefly for an omnibox value to settle. Any
    // later ordinary/same-integrity injected key cancels that stale request;
    // our marked replay is excluded so it cannot cancel itself.
    if !key_up && key.dwExtraInfo != REPLAY_MARKER {
        KEYDOWN_GENERATION.fetch_add(1, Ordering::Release);
    }
    if key.vkCode != u32::from(VK_RETURN) || key.dwExtraInfo == REPLAY_MARKER {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    // Do not let a lower-integrity process drive this medium-integrity broker.
    // Equal-integrity accessibility tools, touch keyboards, macros, and test
    // automation retain normal Windows input semantics; the attested HWND,
    // PID, session, integrity and focused UIA control still gate every write.
    if !input_can_drive_broker(key.flags) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    if key_up {
        ENTER_DOWN.store(false, Ordering::Relaxed);
        if SUPPRESS_ENTER_UP.swap(false, Ordering::Relaxed) {
            return 1;
        }
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    if ENTER_DOWN.swap(true, Ordering::Relaxed) {
        return if SUPPRESS_ENTER_UP.load(Ordering::Relaxed) {
            1
        } else {
            unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
        };
    }

    let foreground = unsafe { GetForegroundWindow() };
    // The E2E instance may run beside the Store-installed broker. It must
    // never suppress or translate input outside the exact browser HWND/PID
    // that its harness supplied before startup.
    if !integration_target_matches(foreground) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    if PAUSED.load(Ordering::Relaxed) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    // Discovery owns the expensive process and UIA checks. The hook only
    // consumes its atomic exact-HWND publication; the worker rechecks the
    // complete cached TrustedSurface before it can reach a navigation sink.
    if CACHED_BROWSER_FOREGROUND.load(Ordering::Acquire) == foreground as isize {
        let worker = WORKER_WINDOW.load(Ordering::Relaxed) as HWND;
        let generation = KEYDOWN_GENERATION.load(Ordering::Acquire);
        let Ok(generation_lparam) = LPARAM::try_from(generation) else {
            return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
        };
        let posted = !worker.is_null()
            && unsafe {
                PostMessageW(
                    worker,
                    PROCESS_CACHED_BROWSER_ENTER,
                    foreground as WPARAM,
                    generation_lparam,
                )
            } != 0;
        if posted {
            SUPPRESS_ENTER_UP.store(true, Ordering::Relaxed);
            return 1;
        }
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    if !is_candidate_foreground_class(foreground) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    // Everything past the class check happens on the worker — attestation
    // included (issue #121): a low-level hook that takes longer than
    // LowLevelHooksTimeout is removed by Windows without notice, and the
    // removal is invisible to us. Nothing here allocates or blocks.
    let worker = WORKER_WINDOW.load(Ordering::Relaxed) as HWND;
    let generation = KEYDOWN_GENERATION.load(Ordering::Acquire);
    let Ok(generation_lparam) = LPARAM::try_from(generation) else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };
    let posted = !worker.is_null()
        && unsafe {
            PostMessageW(
                worker,
                PROCESS_ENTER,
                foreground as WPARAM,
                generation_lparam,
            )
        } != 0;
    if !should_swallow_enter(!worker.is_null(), posted) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    SUPPRESS_ENTER_UP.store(true, Ordering::Relaxed);
    1
}

fn install_hook() -> bool {
    let cur_hook = KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK;
    if !cur_hook.is_null() {
        return true;
    }

    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_keyboard_proc),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    KEYBOARD_HOOK.store(hook as isize, Ordering::Relaxed);
    if hook.is_null() {
        log_diagnostic(&format!("event=debug_hook_failed error={}", unsafe {
            GetLastError()
        }));
    }
    !hook.is_null()
}

fn remove_hook() {
    let hook = KEYBOARD_HOOK.swap(0, Ordering::Relaxed) as HHOOK;
    if !hook.is_null() {
        unsafe {
            UnhookWindowsHookEx(hook);
        }
    }
    ENTER_DOWN.store(false, Ordering::Relaxed);
    SUPPRESS_ENTER_UP.store(false, Ordering::Relaxed);
}

/// Replaces a live hook with a fresh one, keeping the old handle until the
/// replacement exists.
///
/// Windows silently unhooks a low-level hook whose owning thread exceeded
/// `LowLevelHooksTimeout`, and nothing tells the process: `fwdslash status`
/// keeps reporting `running (active)` while `/` does nothing anywhere. Since
/// there is no way to ask whether a hook handle is still live, the broker
/// re-arms on a slow timer instead.
fn rearm_hook(window: HWND) {
    if PAUSED.load(Ordering::Relaxed) {
        return;
    }
    let old = KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK;
    if old.is_null() {
        // Nothing to replace — the hook never installed, so the tooltip reads
        // "hook unavailable". A retry can clear that.
        if install_hook() {
            update_tray_tooltip(window);
        }
        return;
    }

    let fresh = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_keyboard_proc),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    if fresh.is_null() {
        // Keep the incumbent: a failed re-arm must never leave the product
        // with no hook at all.
        return;
    }
    KEYBOARD_HOOK.store(fresh as isize, Ordering::Relaxed);
    unsafe {
        UnhookWindowsHookEx(old);
    }
    log_diagnostic("event=hook_rearmed");
}

fn disconnect_filter() {
    // The next connection starts with an empty driver-side table, so drop the
    // cache to guarantee the following publish actually sends.
    if let Ok(mut published) = PUBLISHED_DISTRIBUTIONS.lock() {
        *published = None;
    }
    let port = FILTER_PORT.swap(-1, Ordering::Relaxed) as HANDLE;
    if port != INVALID_HANDLE_VALUE && !port.is_null() {
        unsafe {
            CloseHandle(port);
        }
    }
}

/// The distribution list most recently accepted by the driver.
/// Whether the driver-namespace preflight rejection has already been logged.
/// The health timer re-runs the check every tick and the verdict cannot change
/// without user action, so it is worth exactly one line per process.
static DRIVER_PREFLIGHT_REJECTED: AtomicBool = AtomicBool::new(false);

static PUBLISHED_DISTRIBUTIONS: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// The list most recently enumerated for publication, accepted or not.
///
/// `PUBLISHED_DISTRIBUTIONS` alone can never short-circuit anything while no
/// driver is loaded — the shipping configuration — because it is only written
/// after a successful `FilterSendMessage`. Recording the attempt is what makes
/// the compare-only path engage with the port absent.
static ATTEMPTED_DISTRIBUTIONS: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Opens the filter port if it is not open already. Returns whether a port is
/// available afterwards.
fn ensure_filter_port() -> bool {
    let port = FILTER_PORT.load(Ordering::Relaxed) as HANDLE;
    if port != INVALID_HANDLE_VALUE && !port.is_null() {
        return true;
    }

    let port_name = to_u16_vec(FSW_FILTER_PORT_NAME);
    let mut connected_port = INVALID_HANDLE_VALUE;
    let hr = unsafe {
        FilterConnectCommunicationPort(
            port_name.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            &raw mut connected_port,
        )
    };
    if hr < 0 || connected_port == INVALID_HANDLE_VALUE {
        return false;
    }
    if !verify_filter_protocol(connected_port) {
        unsafe {
            CloseHandle(connected_port);
        }
        return false;
    }
    FILTER_PORT.store(connected_port as isize, Ordering::Relaxed);
    true
}

/// The loaded driver reports the protocol version it speaks when a Ping
/// carries a `ULONG` output buffer (`include/fsw_filter_protocol.h`). A
/// mismatch means the message layout this broker compiled against is not the
/// one the driver interprets — a publish would still "succeed" while the
/// driver parsed different bytes, so the disagreement is logged and the
/// connection refused instead of silently misbehaving.
fn verify_filter_protocol(port: HANDLE) -> bool {
    /// `size_of::<u32>()` as the `u32` the filter API takes.
    const REPORTED_SIZE: u32 = 4;
    const FSW_OPERATION_PING: u32 = 3;
    let mut msg: FswMappingMessage = unsafe { std::mem::zeroed() };
    let Some(message_size) = fixed_size_u32::<FswMappingMessage>() else {
        log_diagnostic("event=filter_message_size_unrepresentable");
        return false;
    };
    msg.version = FSW_FILTER_PROTOCOL_VERSION;
    msg.size = message_size;
    msg.operation = FSW_OPERATION_PING;
    let mut reported = 0u32;
    let mut returned = 0u32;
    let sent = unsafe {
        FilterSendMessage(
            port,
            (&raw const msg).cast(),
            message_size,
            (&raw mut reported).cast(),
            REPORTED_SIZE,
            &raw mut returned,
        )
    };
    if sent >= 0 && returned == REPORTED_SIZE && reported == FSW_FILTER_PROTOCOL_VERSION {
        return true;
    }
    if sent < 0 {
        log_diagnostic("event=filter_protocol_ping_failed");
    } else {
        log_diagnostic(&format!(
            "event=filter_protocol_mismatch driver_reported={reported} returned_bytes={returned}"
        ));
    }
    false
}

/// Re-times the health timer. `SetTimer` with an existing id replaces the
/// interval in place, so there is only ever one timer.
fn set_health_interval(connected: bool) {
    let wanted = if connected {
        HEALTH_INTERVAL_CONNECTED_MS
    } else {
        HEALTH_INTERVAL_IDLE_MS
    };
    if HEALTH_INTERVAL_MS.swap(wanted, Ordering::Relaxed) == wanted {
        return;
    }
    let window = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
    if !window.is_null() {
        unsafe {
            SetTimer(window, HEALTH_TIMER, wanted, None);
        }
    }
}

/// Refuse to create a virtual namespace over a real filesystem object. The
/// exact-root `symlink_metadata` call does not follow its final component, so a
/// junction or symbolic link cannot redirect this inspection elsewhere.
fn validate_driver_namespace_root() -> Result<(), String> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    match std::fs::symlink_metadata(r"C:\fwdslash") {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) => {
            let attributes = std::os::windows::fs::MetadataExt::file_attributes(&metadata);
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                Err(
                    "C:\\fwdslash is a reparse point; refusing to activate driver mappings."
                        .to_owned(),
                )
            } else {
                Err(
                    "C:\\fwdslash already exists; refusing to shadow it with driver mappings."
                        .to_owned(),
                )
            }
        }
        Err(error) => Err(format!(
            "cannot inspect C:\\fwdslash without traversal ({error}); refusing to activate driver mappings."
        )),
    }
}

fn publish_filter_mappings(force: bool) {
    if let Err(_reason) = validate_driver_namespace_root() {
        // GUI subsystem: `eprintln!` went nowhere, and the health timer
        // repeated it every tick. One category-only line per process, and
        // never the path (issue #126).
        if !DRIVER_PREFLIGHT_REJECTED.swap(true, Ordering::Relaxed) {
            log_diagnostic("event=driver_namespace_rejected");
        }
        // A pre-existing port may still own prior mappings. Closing it invokes
        // the driver's disconnect cleanup, so the unsafe namespace cannot
        // remain active after a later health-timer check.
        disconnect_filter();
        set_health_interval(false);
        if let Ok(mut published) = PUBLISHED_DISTRIBUTIONS.lock() {
            *published = None;
        }
        return;
    }

    // The connect attempt comes first: whether anything is listening decides
    // both the tick interval and whether enumerating Lxss buys anything.
    let connected = ensure_filter_port();
    set_health_interval(connected);
    if !connected && !force {
        // Idle tick with no driver. The registry read, the sort and the
        // kernel round-trip would all be discarded; only the probe above is
        // worth doing.
        return;
    }

    let distributions = if PAUSED.load(Ordering::Relaxed) {
        Vec::new()
    } else {
        let mut distros = list_registered_distributions();
        // Ordinal case-insensitive sort. The driver receives this array in
        // order, so the comparison has to agree.
        distros.sort_by(|a, b| {
            let a_folded: Vec<char> = a.chars().flat_map(char::to_uppercase).collect();
            let b_folded: Vec<char> = b.chars().flat_map(char::to_uppercase).collect();
            a_folded.cmp(&b_folded)
        });
        distros
    };

    let unchanged = match ATTEMPTED_DISTRIBUTIONS.lock() {
        Ok(mut attempted) => {
            let same = attempted.as_ref() == Some(&distributions);
            if !same {
                *attempted = Some(distributions.clone());
            }
            same
        }
        Err(_) => false,
    };

    if !connected {
        // A forced publish with no driver: the attempt is recorded, and there
        // is nobody to send it to.
        return;
    }

    // Nothing changed and nobody asked for a resend, so skip the kernel
    // round-trip.
    if !force
        && unchanged
        && PUBLISHED_DISTRIBUTIONS
            .lock()
            .is_ok_and(|published| published.as_ref() == Some(&distributions))
    {
        return;
    }

    let port = FILTER_PORT.load(Ordering::Relaxed) as HANDLE;
    unsafe {
        let mut msg: FswMappingMessage = std::mem::zeroed();
        msg.version = FSW_FILTER_PROTOCOL_VERSION;
        let Some(message_size) = fixed_size_u32::<FswMappingMessage>() else {
            log_diagnostic("event=filter_message_size_unrepresentable");
            return;
        };
        msg.size = message_size;
        msg.operation = FSW_OPERATION_REPLACE_MAPPINGS;
        msg.reserved = 0; // the driver requires zero; explicit, not padding luck
        msg.generation = GetTickCount64();
        let count = distributions.len().min(FSW_FILTER_MAX_DISTRIBUTIONS);
        let Ok(distribution_count) = u32::try_from(count) else {
            log_diagnostic("event=distribution_count_unrepresentable");
            return;
        };
        msg.distribution_count = distribution_count;

        for (i, d) in distributions.iter().take(count).enumerate() {
            let wide = to_u16_vec(d);
            if let Some(destination) = msg.distributions.get_mut(i) {
                // A name the 128-unit slot cannot hold is published truncated,
                // which no longer equals the registry name and can never match.
                // Say so instead of failing silently.
                let truncated = wide.len() >= destination.len();
                copy_wide_truncated(destination, &wide);
                if truncated {
                    log_diagnostic("event=filter_distribution_name_truncated");
                }
            }
        }

        let mut returned = 0u32;
        let sent = FilterSendMessage(
            port,
            (&raw const msg).cast(),
            message_size,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
        );
        if sent < 0 {
            log_diagnostic("event=filter_publish_failed");
            disconnect_filter();
            return;
        }
    }

    if let Ok(mut published) = PUBLISHED_DISTRIBUTIONS.lock() {
        *published = Some(distributions);
    }
}

/// The tooltip is the only place the product reports its own health, so it
/// distinguishes a deliberate pause from a hook that failed to install.
fn tray_tip() -> &'static str {
    if PAUSED.load(Ordering::Relaxed) {
        "Forward Slash Windows \u{2014} paused"
    } else if (KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK).is_null()
        || (WORKER_WINDOW.load(Ordering::Acquire) as HWND).is_null()
    {
        "Forward Slash Windows \u{2014} processing unavailable"
    } else {
        "Forward Slash Windows \u{2014} active"
    }
}

fn tray_icon_data(window: HWND) -> Option<NOTIFYICONDATAW> {
    unsafe {
        let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
        icon.cbSize = fixed_size_u32::<NOTIFYICONDATAW>()?;
        icon.hWnd = window;
        icon.uID = tray_id()?;
        Some(icon)
    }
}

/// Adds the notification icon, reporting whether the shell took it.
///
/// `Shell_NotifyIcon` fails with `ERROR_TIMEOUT` while the shell is busy, and
/// the MSIX startup task launches the broker at exactly that moment. An
/// unchecked add costs the user the icon, the menu and every balloon for the
/// whole session, so the result drives `ICON_ADDED` and the health-timer retry.
fn add_tray_icon(window: HWND) -> bool {
    if integration_test_mode() {
        return false;
    }
    unsafe {
        let Some(mut icon) = tray_icon_data(window) else {
            log_diagnostic("event=tray_icon_data_unrepresentable");
            return false;
        };
        icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        icon.uCallbackMessage = TRAY_MESSAGE;
        icon.hIcon = LoadIconW(
            GetModuleHandleW(std::ptr::null()),
            IDI_FSW_APP as *const u16,
        );

        let tip = to_u16_vec(tray_tip());
        copy_wide_truncated(&mut icon.szTip, &tip);

        if Shell_NotifyIconW(NIM_ADD, &raw const icon) == 0 {
            ICON_ADDED.store(false, Ordering::Relaxed);
            log_diagnostic("event=tray_icon_add_failed");
            return false;
        }
        ICON_ADDED.store(true, Ordering::Relaxed);

        // Version 4 only after the icon exists: it is a property of an icon
        // the shell already knows about.
        icon.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &raw const icon);
        true
    }
}

fn remove_tray_icon(window: HWND) {
    if integration_test_mode() {
        return;
    }
    unsafe {
        let Some(icon) = tray_icon_data(window) else {
            log_diagnostic("event=tray_icon_data_unrepresentable");
            return;
        };
        Shell_NotifyIconW(NIM_DELETE, &raw const icon);
    }
    ICON_ADDED.store(false, Ordering::Relaxed);
}

/// Health-timer retry for an add the shell refused.
fn ensure_tray_icon(window: HWND) {
    if !ICON_ADDED.load(Ordering::Relaxed) {
        add_tray_icon(window);
    }
}

/// Re-announces the tooltip (`NIM_MODIFY`) without touching the icon itself.
fn update_tray_tooltip(window: HWND) {
    if integration_test_mode() {
        return;
    }
    if !ICON_ADDED.load(Ordering::Relaxed) {
        return;
    }
    unsafe {
        let Some(mut icon) = tray_icon_data(window) else {
            log_diagnostic("event=tray_icon_data_unrepresentable");
            return;
        };
        icon.uFlags = NIF_TIP;
        let tip = to_u16_vec(tray_tip());
        copy_wide_truncated(&mut icon.szTip, &tip);
        Shell_NotifyIconW(NIM_MODIFY, &raw const icon);
    }
}

/// `TaskbarCreated` is broadcast when the shell restarts; broadcasts never
/// reach a message-only window, which is why the broker window is a real
/// (never-shown) top-level window. Without the re-add, an explorer.exe
/// restart would leave the resident broker with no tray icon at all.
fn taskbar_created_message() -> u32 {
    static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *TASKBAR_CREATED
        .get_or_init(|| unsafe { RegisterWindowMessageW(to_u16_vec("TaskbarCreated").as_ptr()) })
}

/// Drains pause writes in submission order on one background thread.
///
/// `persist_disabled` shells out to `reg.exe` (see its doc comment for why a
/// child process and not a direct write). That is a process creation plus a
/// wait — unbounded under load — and the caller here is the thread that owns
/// the low-level keyboard hook, where any wait freezes every keystroke on the
/// machine. The pause therefore takes effect in memory immediately and the
/// persistence is reported asynchronously: `FSW_WM_SET_PAUSED` replies from
/// in-memory state plus the hook result, and a failed write surfaces later as
/// a balloon plus `event=persist_disabled_failed`.
fn report_persist_failure() {
    log_diagnostic("event=persist_disabled_failed");
    let window = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
    if !window.is_null() {
        unsafe {
            PostMessageW(window, PERSIST_FAILED, 0, 0);
        }
    }
}

fn drain_persist_queue<F>(receiver: &mpsc::Receiver<bool>, mut persist: F)
where
    F: FnMut(bool),
{
    while let Ok(disabled) = receiver.recv() {
        persist(disabled);
    }
}

fn persist_worker_main(receiver: &mpsc::Receiver<bool>) {
    drain_persist_queue(receiver, |disabled| {
        let failed = persist_disabled(disabled).is_err();
        PERSIST_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        if failed {
            report_persist_failure();
        }
    });
}

fn request_persist_disabled(disabled: bool) {
    PERSIST_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    let Ok(mut queue) = PERSIST_QUEUE.lock() else {
        PERSIST_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        log_diagnostic("event=persist_queue_unavailable");
        report_persist_failure();
        return;
    };
    if queue.is_none() {
        let (sender, receiver) = mpsc::channel();
        if let Ok(handle) = std::thread::Builder::new()
            .name("fsw-persist".to_owned())
            .spawn(move || persist_worker_main(&receiver))
        {
            drop(handle);
            *queue = Some(sender);
        } else {
            PERSIST_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
            log_diagnostic("event=persist_queue_start_failed");
            report_persist_failure();
            return;
        }
    }
    let Some(sender) = queue.as_ref() else { return };
    if sender.send(disabled).is_err() {
        *queue = None;
        PERSIST_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        log_diagnostic("event=persist_queue_unavailable");
        report_persist_failure();
    }
}

/// The shell adapters, in the order the tray "Integrations" submenu lists
/// them: the CLI verb id, the name a balloon may show, and the marker key
/// that records the payload version currently deployed.
fn adapter_upgrade_targets() -> [(&'static str, &'static str, String); 3] {
    [
        ("cmd", "Command Prompt", CMD_ADAPTER_KEY.to_owned()),
        (
            "windows-powershell",
            "Windows PowerShell",
            format!("{POWERSHELL_ADAPTER_ROOT}WindowsPowerShell"),
        ),
        (
            "powershell",
            "PowerShell 7",
            format!("{POWERSHELL_ADAPTER_ROOT}PowerShell"),
        ),
    ]
}

/// Runs one `fwdslash.exe` invocation to completion, bounded by `timeout`, and
/// reports the exit code it finished with.
///
/// `None` is deliberately *not* "failed": it means the child never got to
/// answer — it could not be spawned, the wait itself errored, or it blew the
/// deadline and was killed. Every caller here treats that differently from a
/// child that ran and refused, because the first retries itself for free (the
/// marker key still reads the old version, the update cadence comes round
/// again) and the second needs a person.
///
/// Never call this on the hook thread or the window thread: it parks the
/// calling thread for as long as the child takes.
fn run_cli_bounded(cli: &Path, args: &[&str], timeout: std::time::Duration) -> Option<i32> {
    let spawned = std::process::Command::new(cli)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let Ok(mut child) = spawned else {
        return None;
    };

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            // `code()` is `None` only for a signal, which Windows has no
            // concept of; the fallback keeps it total.
            Ok(Some(status)) => return Some(status.code().unwrap_or(1)),
            Ok(None) => {}
            Err(_) => return None,
        }
        if std::time::Instant::now() >= deadline {
            // Half-applied is the transaction's problem, not ours: the CLI
            // rolls its own snapshot back, and the marker key still reads the
            // old version, so the next launch tries again.
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(ADAPTER_UPGRADE_POLL_MS));
    }
}

/// Runs one `fwdslash integration <id> enable`, bounded by
/// [`ADAPTER_UPGRADE_TIMEOUT`].
///
/// The CLI does the transactional uninstall+install itself and is idempotent
/// once the recorded version already matches, so racing a manual enable from
/// the settings app costs at worst a redundant reinstall.
fn run_adapter_upgrade(cli: &Path, id: &str) -> Option<i32> {
    // `--background`: this sweep may swap the %LOCALAPPDATA% payload, which is
    // the whole upgrade now, but must never write the user's PowerShell profile
    // under Documents — Controlled Folder Access guards it, and a blocked write
    // used to fail silently (#127).
    run_cli_bounded(
        cli,
        &["integration", id, "enable", "--background"],
        ADAPTER_UPGRADE_TIMEOUT,
    )
}

/// `fwdslash integration <id> enable` exit codes the sweep treats specially,
/// mirrored from `crates/fsw-cli/src/adapters/mod.rs` (#127).
const ADAPTER_EXIT_NEEDS_CONFIRMATION: i32 = 4;
const ADAPTER_EXIT_BLOCKED: i32 = 5;

/// Runs one `fwdslash repair-adapters` to completion, bounded by
/// [`ADAPTER_UPGRADE_TIMEOUT`]. Fire-and-forget: a repair that fails or times
/// out just retries next launch, and the guarded profile block means an
/// un-repaired orphan is silent, never a red shell error (#37).
fn run_adapter_repair(cli: &Path) {
    let _ = run_cli_bounded(cli, &["repair-adapters"], ADAPTER_UPGRADE_TIMEOUT);
}

/// `show_notification` silently drops a balloon while the shell has not
/// accepted `NIM_ADD`, and at logon the add can be waiting on the 60 s
/// health tick to retry. Give the icon ~10 s to appear before spending the
/// one balloon this sweep is allowed.
fn notify_when_icon_ready(message: &str, flags: u32) {
    for _ in 0..ADAPTER_UPGRADE_NOTIFY_ATTEMPTS {
        if ICON_ADDED.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(
            ADAPTER_UPGRADE_NOTIFY_INTERVAL_MS,
        ));
    }
    show_notification(message, flags);
}

/// What the sweep decided about one adapter, after up to two attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdapterOutcome {
    /// The payload is now on this build's version.
    Upgraded,
    /// Neither attempt got an answer out of the CLI — the child could not
    /// start, or blew its budget and was killed. The marker key still reads
    /// the old version, so the next broker start or settings launch tries
    /// again. Silent by design: nothing the user does changes this.
    Deferred,
    /// The CLI ran and refused. A third-party-modified profile, a missing
    /// `pwsh.exe` — the failures that stay failed until somebody acts.
    NeedsUser,
    /// The payload is current, but the profile carries a block only the user
    /// may authorise rewriting (#127). Nothing is broken: the deployed block
    /// keeps working until they confirm.
    NeedsConfirmation,
    /// A profile write was refused — Controlled Folder Access is the usual
    /// cause, and naming it is the whole point (#127).
    Blocked,
}

/// Classifies one adapter from its two attempts' exit codes (issue #56).
///
/// `None` means the attempt never produced an exit code (see
/// [`run_cli_bounded`]); `Some(0)` is success and any other `Some` is a
/// refusal. The retry is only consulted when the first attempt did not
/// succeed, which is also why the caller may pass `None` for it unconditionally
/// when the first attempt already won.
#[must_use]
fn adapter_outcome(first: Option<i32>, retry: Option<i32>) -> AdapterOutcome {
    if first == Some(0) {
        return AdapterOutcome::Upgraded;
    }
    // A refusal a person has to act on is the same on both attempts; retrying
    // it changes nothing, so the first answer stands.
    if let Some(ADAPTER_EXIT_NEEDS_CONFIRMATION) = first {
        return AdapterOutcome::NeedsConfirmation;
    }
    if let Some(ADAPTER_EXIT_BLOCKED) = first {
        return AdapterOutcome::Blocked;
    }
    match retry {
        Some(0) => AdapterOutcome::Upgraded,
        // Two attempts, neither of which the CLI answered: transient by every
        // available signal. Balloon nothing.
        None => AdapterOutcome::Deferred,
        Some(ADAPTER_EXIT_NEEDS_CONFIRMATION) => AdapterOutcome::NeedsConfirmation,
        Some(ADAPTER_EXIT_BLOCKED) => AdapterOutcome::Blocked,
        Some(_) => AdapterOutcome::NeedsUser,
    }
}

/// Holds [`FSW_ADAPTER_SWEEP_MUTEX`] for the length of a sweep.
///
/// `None` from [`SweepLock::acquire`] means the settings window is already
/// sweeping (issue #56): skip rather than fight it for the payload tree, since
/// whoever holds it is running the identical work.
struct SweepLock(HANDLE);

impl SweepLock {
    fn acquire() -> Option<Self> {
        let name = to_u16_vec(FSW_ADAPTER_SWEEP_MUTEX);
        // SAFETY: a named mutex with a static, NUL-terminated name; the handle
        // is closed exactly once, in `Drop`.
        unsafe {
            let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
            if handle.is_null() {
                // Nobody can tell whether a sweep is running; err toward doing
                // the work, which is idempotent, rather than never doing it.
                return Some(Self(std::ptr::null_mut()));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return None;
            }
            Some(Self(handle))
        }
    }
}

impl Drop for SweepLock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from `CreateMutexW` above and is closed
            // once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Brings every installed shell adapter whose recorded payload version is not
/// this build's back up to date, with no click from the user.
///
/// An adapter's payload — the scripts plus a private copy of `fwdslash.exe` —
/// is copied into `%LOCALAPPDATA%` at install time, so after a product update
/// the previous copy keeps serving every console until someone re-runs
/// `fwdslash integration <id> enable`. The broker is the one component that
/// starts at every logon and after every update, so the check belongs here.
fn adapter_upgrade_sweep() {
    // One sweeper at a time (issue #56). The settings window runs the same
    // work at launch, and an update that restarts the app starts both within
    // seconds of each other; the loser of that race deletes a payload
    // directory the winner's child is running out of and reports a failure
    // that was never real.
    let Some(_lock) = SweepLock::acquire() else {
        log_diagnostic("event=adapter_sweep_busy");
        return;
    };

    // Before anything else: mirror the settings this packaged process holds
    // into the real hive, because the adapters this sweep is about to look
    // after read them from an unpackaged shell and would otherwise still see
    // whatever the install started with (issue #52). A no-op unpackaged, and
    // a no-op packaged once the hives agree. It logs `event=settings_synced`
    // itself when it repairs something, so nothing is logged here.
    let _ = sync_settings_to_real_hive();

    // Beside the broker, never from PATH: an appExecutionAlias or a stale
    // directory on PATH could resolve to a different install entirely.
    let Ok(directory) = executable_directory() else {
        log_diagnostic("event=adapter_upgrade_skipped");
        return;
    };
    let cli = directory.join("fwdslash.exe");
    if !cli.is_file() {
        log_diagnostic("event=adapter_upgrade_skipped");
        return;
    }

    // The version-bump upgrade first, so its "integrations were updated"
    // balloon still fires. Repair runs afterward regardless, cleaning any
    // orphaned or duplicated block the version match alone would miss (#37).
    let targets = adapter_upgrade_targets();
    let outdated: Vec<(&'static str, &'static str)> = targets
        .iter()
        .filter(|(_, _, marker_key)| adapter_outdated(marker_key, FSW_VERSION))
        .map(|&(id, label, _)| (id, label))
        .collect();
    if !outdated.is_empty() {
        let mut upgraded: Vec<&'static str> = Vec::new();
        let mut needs_user = false;
        let mut needs_confirmation = false;
        let mut blocked = false;
        for (id, label) in outdated {
            let first = run_adapter_upgrade(&cli, id);
            // One retry, after a pause (issue #56). Right after an MSIX update
            // the first attempt fails for reasons that are gone seconds later:
            // a console still holding the old payload's `fwdslash.exe`, a copy
            // out of `WindowsApps` competing with the package still being
            // staged.
            let retry = if first == Some(0)
                || first == Some(ADAPTER_EXIT_NEEDS_CONFIRMATION)
                || first == Some(ADAPTER_EXIT_BLOCKED)
            {
                None
            } else {
                log_diagnostic("event=adapter_upgrade_retry");
                std::thread::sleep(std::time::Duration::from_millis(ADAPTER_RETRY_DELAY_MS));
                run_adapter_upgrade(&cli, id)
            };
            match adapter_outcome(first, retry) {
                AdapterOutcome::Upgraded => {
                    log_diagnostic("event=adapter_upgraded");
                    upgraded.push(label);
                }
                AdapterOutcome::Deferred => log_diagnostic("event=adapter_upgrade_deferred"),
                AdapterOutcome::NeedsUser => {
                    log_diagnostic("event=adapter_upgrade_failed");
                    needs_user = true;
                }
                AdapterOutcome::NeedsConfirmation => {
                    log_diagnostic("event=adapter_upgrade_needs_confirmation");
                    needs_confirmation = true;
                }
                AdapterOutcome::Blocked => {
                    log_diagnostic("event=adapter_upgrade_blocked");
                    blocked = true;
                }
            }
        }

        // Exactly one balloon, whatever the mix: a per-adapter notification
        // would stack three toasts on top of a logon the user did not ask
        // about. A deferral alone is silent — it retries itself.
        if blocked {
            notify_when_icon_ready(
                "Windows Controlled Folder Access blocked an update to your PowerShell profile. \
                 Allow Forward Slash Windows through it under Windows Security, or turn the \
                 PowerShell integration off.",
                NIIF_WARNING,
            );
        } else if needs_user {
            notify_when_icon_ready(
                "Some terminal integrations could not be updated automatically. \
                 Open Settings and choose Repair integrations.",
                NIIF_WARNING,
            );
        } else if needs_confirmation {
            // Not a failure: the deployed block still works. It is a change to
            // a file under Documents, which only the user may authorise (#127).
            notify_when_icon_ready(
                "The PowerShell integration needs a change you must confirm. Open Settings and \
                 turn the PowerShell integration off and on again to apply it.",
                NIIF_INFO,
            );
        } else if !upgraded.is_empty() {
            notify_when_icon_ready(
                &format!(
                    "Terminal integrations were updated to {FSW_VERSION}: {}.",
                    upgraded.join(", ")
                ),
                NIIF_INFO,
            );
        }
    }

    // Detect-and-repair every adapter's profile/AutoRun hygiene: an orphaned or
    // duplicated block self-heals even when the recorded version already
    // matches and nothing was "outdated" (#37). Silent — the guarded block
    // means an un-repaired orphan is never a red shell error anyway.
    run_adapter_repair(&cli);
}

/// Starts the adapter-upgrade sweep on a thread of its own, once per process.
///
/// Fire-and-forget by design. The work is process creation plus a wait, both
/// unbounded under load, so it may touch neither the hook/UI thread nor the
/// Enter worker; and `WM_DESTROY` deliberately does not join it, because a
/// child `fwdslash.exe` already mid-transaction owns its own rollback and
/// finishes whether the broker is still there or not.
fn start_adapter_upgrade() {
    if ADAPTER_UPGRADE_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    if std::thread::Builder::new()
        .name("fsw-adapter-upgrade".to_owned())
        .spawn(adapter_upgrade_sweep)
        .is_err()
    {
        // Out of threads. Running it inline would park the thread that owns
        // the keyboard hook for as long as three CLI transactions take, so the
        // upgrade waits for the next launch instead.
        log_diagnostic("event=adapter_upgrade_skipped");
    }
}

// ---------------------------------------------------------------------------
// The update cycle
// ---------------------------------------------------------------------------

/// Exit codes of `fwdslash update`, mirrored from `crates/fsw-cli/src/update`.
/// Named here too, because a bare `10` in a match arm reads as nothing.
const UPDATE_EXIT_AVAILABLE: i32 = 10;
const UPDATE_EXIT_NEEDS_USER: i32 = 11;
const UPDATE_EXIT_ERROR: i32 = 1;

/// Clears [`UPDATE_RUNNING`] however [`update_cycle`] ends.
struct UpdateCycleGuard;

impl Drop for UpdateCycleGuard {
    fn drop(&mut self) {
        UPDATE_RUNNING.store(false, Ordering::Release);
    }
}

/// How old the update cycle is at `now`, in milliseconds.
///
/// Before the first cycle of this process (`last == 0`) the age is a step
/// function of uptime rather than a duration: nothing until the broker has been
/// up [`UPDATE_FIRST_DELAY_MS`], a full interval after that. It keeps the two
/// thresholds — the first delay and the recurring interval — from needing two
/// timers or a signed clock.
#[must_use]
fn update_cycle_age_ms(now: u64, start: u64, last: u64) -> u64 {
    if last == 0 {
        if now.saturating_sub(start) >= UPDATE_FIRST_DELAY_MS {
            UPDATE_CONSIDER_INTERVAL_MS
        } else {
            0
        }
    } else {
        now.saturating_sub(last)
    }
}

/// Whether an update cycle may start. Pure, so the four gates are one truth
/// table instead of four early returns spread through a side-effecting
/// function.
///
/// `allowed` is `fsw_core::update::update_check_allowed(packaged, auto_update)`:
/// an unpackaged build has nothing it could install, and the Automatic updates
/// switch is the user's answer for both flavors.
#[must_use]
fn update_cycle_due(running: bool, age_ms: u64, worker_busy: bool, allowed: bool) -> bool {
    !running && age_ms >= UPDATE_CONSIDER_INTERVAL_MS && !worker_busy && allowed
}

/// Whether a balloon about `tag` is new information, given the version the last
/// balloon was about.
///
/// One available update produces one balloon, however many six-hour cycles see
/// it. A cycle that cannot name a version at all (the CLI answered but the
/// registry had no tag) is announced once and then stays quiet, rather than
/// once per cycle forever.
#[must_use]
fn should_balloon_update(key: &str, notified: Option<&str>) -> bool {
    // Nothing announced yet, or a different offer than the one announced.
    notified != Some(key)
}

/// The dedupe key for one offer. Total, so "no offer" and "an offer with no
/// version" are distinct keys and neither collides with "nothing announced".
#[must_use]
fn offer_key(tag: Option<&str>) -> String {
    tag.map_or_else(|| "(unnamed)".to_string(), str::to_owned)
}

/// Whether repeated install failures have earned the warning balloon.
///
/// Store flavor only: the GitHub flavor's failed install leaves the downloaded
/// bundle in place and applies it at the next logon on its own, so telling the
/// user about it would be asking for a click that changes nothing.
#[must_use]
fn should_balloon_install_failure(consecutive_failures: u32, store_flavor: bool) -> bool {
    store_flavor && consecutive_failures >= UPDATE_FAILURES_BEFORE_BALLOON
}

/// Shows one update balloon, at most once per version.
///
/// The dedupe key is whatever `cached_update_tag()` holds *now* — the value the
/// CLI just wrote — so a second update replacing the first is announced again.
fn notify_update_once(message: &str, flags: u32) {
    let tag = update::cached_update_tag();
    let Ok(mut notified) = UPDATE_NOTIFIED_TAG.lock() else {
        return;
    };
    let key = offer_key(tag.as_deref());
    if !should_balloon_update(&key, notified.as_deref()) {
        return;
    }
    *notified = Some(key);
    // The lock is held across a call that can park for ~10 s waiting for the
    // shell to accept the icon. Nothing else ever takes it except another
    // cycle, and `UPDATE_RUNNING` already makes those mutually exclusive.
    drop(notified);
    notify_when_icon_ready(message, flags);
}

/// One check, and — when the CLI says there is something to install — one
/// install. Runs on the `fsw-update` thread and nowhere else: every step is a
/// child process wait.
fn update_cycle() {
    let _guard = UpdateCycleGuard;
    log_diagnostic("event=update_cycle_started");

    // Beside the broker, never from PATH: an appExecutionAlias or a stale
    // directory on PATH could resolve to a different install entirely.
    let Ok(directory) = executable_directory() else {
        log_diagnostic("event=update_cycle_failed");
        return;
    };
    let cli = directory.join("fwdslash.exe");
    if !cli.is_file() {
        log_diagnostic("event=update_cycle_failed");
        return;
    }

    let Some(code) = run_cli_bounded(&cli, &["update", "check", "--json"], UPDATE_CHECK_TIMEOUT)
    else {
        log_diagnostic("event=update_cycle_failed");
        return;
    };
    if code != UPDATE_EXIT_AVAILABLE {
        // Up to date, not due, disabled, or a check that could not run: all
        // silent, all retried at the next cycle.
        return;
    }
    log_diagnostic("event=update_available");

    // `--relaunch broker` and no `--force`: the CLI's own moment gate declines
    // while a settings window is open, and what has to come back afterwards is
    // the resident broker, not a window nobody asked for.
    log_diagnostic("event=update_installing");
    let Some(code) = run_cli_bounded(
        &cli,
        &["update", "install", "--relaunch", "broker", "--json"],
        UPDATE_INSTALL_TIMEOUT,
    ) else {
        log_diagnostic("event=update_cycle_failed");
        return;
    };

    match code {
        UPDATE_EXIT_NEEDS_USER => {
            UPDATE_INSTALL_FAILURES.store(0, Ordering::Relaxed);
            notify_update_once(
                "An update to fwdslash is available in the Microsoft Store. \
                 Open Settings to install it.",
                NIIF_INFO,
            );
        }
        UPDATE_EXIT_ERROR => {
            let failures = UPDATE_INSTALL_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            log_diagnostic("event=update_cycle_failed");
            if should_balloon_install_failure(failures, is_store_flavor()) {
                notify_update_once(
                    "fwdslash could not update itself automatically.",
                    NIIF_WARNING,
                );
            }
        }
        // 0 (install started), 10 (deferred — a settings window is open, or
        // the download is still coming) and 12 (nothing to install) are all
        // silent: the next cycle picks the story up.
        _ => UPDATE_INSTALL_FAILURES.store(0, Ordering::Relaxed),
    }
}

/// Starts an update cycle if this is a moment to have one. Called from the
/// health tick, on the window thread, so it may do nothing but read atomics,
/// read two registry values and spawn.
fn maybe_start_update_cycle() {
    // SAFETY: no preconditions.
    let now = unsafe { GetTickCount64() };
    let age = update_cycle_age_ms(
        now,
        BROKER_START_MS.load(Ordering::Relaxed),
        LAST_UPDATE_TICK_MS.load(Ordering::Relaxed),
    );
    let allowed =
        update::update_check_allowed(has_package_identity(), update::read_auto_update_enabled());
    if !update_cycle_due(
        UPDATE_RUNNING.load(Ordering::Acquire),
        age,
        WORKER_BUSY.load(Ordering::Acquire),
        allowed,
    ) {
        return;
    }

    // Claim the slot before spawning, so a tick that arrives while the thread
    // is still starting cannot start a second one.
    if UPDATE_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    LAST_UPDATE_TICK_MS.store(now, Ordering::Relaxed);
    if std::thread::Builder::new()
        .name("fsw-update".to_owned())
        .spawn(update_cycle)
        .is_err()
    {
        // Never inline: this thread owns the low-level keyboard hook, and
        // Windows silently removes a hook whose thread stops pumping.
        UPDATE_RUNNING.store(false, Ordering::Release);
        log_diagnostic("event=update_cycle_skipped");
    }
}

/// Applies a pause/resume to the *running* broker and reports whether the
/// keyboard hook ended up armed as the new state requires.
///
/// Persistence is deliberately not part of it: this is also the path a
/// state-changed broadcast takes, where the value has already been written by
/// somebody else and writing it again would be a loop.
fn apply_paused(paused: bool) -> bool {
    PAUSED.store(paused, Ordering::Relaxed);
    // A paused broker must be inert: leaving discovery armed kept two
    // desktop-wide WinEvent hooks running and kept calling GetFocusedElement
    // on the user's browser (issue #122).
    request_browser_discovery(!paused);
    if paused {
        remove_hook();
        true
    } else {
        install_hook()
    }
}

/// Re-reads the settings another component just changed and catches the
/// running broker up (issue #55).
///
/// The tray tooltip, the keyboard hook and the mapping published to the driver
/// are all derived from state that `fwdslash pause`, the settings window or a
/// shell adapter can change from another process. Before this they caught up
/// at the next health tick — a minute — or, for the pause flag, not at all.
///
/// Runs on the broker's window thread, which also owns the low-level keyboard
/// hook, so it stays a registry read plus, at most, exactly the work a tray
/// pause already does. The menus need nothing: they are built from live state
/// when the tray menu opens.
fn reload_settings(window: HWND) {
    // A pause of our own is mid-flight: `PAUSED` is ahead of the stored value
    // on purpose, and re-reading now would undo it.
    if PERSIST_IN_FLIGHT.load(Ordering::Acquire) != 0 {
        return;
    }
    let disabled = is_disabled();
    if disabled == PAUSED.load(Ordering::Relaxed) {
        // Nothing the tray shows changed. The distribution list still might
        // have, and this is how the driver hears about it; with no driver
        // connected it costs one failed connect attempt.
        publish_filter_mappings(false);
        return;
    }
    log_diagnostic("event=state_changed");
    let hook_ok = apply_paused(disabled);
    update_tray_tooltip(window);
    publish_filter_mappings(true);
    if !hook_ok {
        // No balloon: the user did not ask *this* process for anything, and
        // the tooltip already reads "hook unavailable". The health timer
        // re-arms on its own.
        log_diagnostic("event=hook_unavailable");
    }
}

/// Applies a pause/resume and reports the state the broker ended up in.
///
/// `Err` means the resume could not arm the keyboard hook, i.e. the broker is
/// `Unavailable`. The persistence result is deliberately *not* part of it —
/// see [`request_persist_disabled`].
///
/// No broadcast from here: the write this schedules broadcasts when it lands
/// (`fsw_core::settings_write`), and announcing the change before the value is
/// stored would have every listener — this broker included — re-read the old
/// one.
fn set_paused(paused: bool) -> Result<BrokerState, ()> {
    // Unhook before persisting: the write is off-thread now, but the ordering
    // is what guarantees a pause stops swallowing Enter immediately.
    let hook_ok = apply_paused(paused);

    request_persist_disabled(paused);

    let window = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
    if !window.is_null() {
        update_tray_tooltip(window);
    }
    publish_filter_mappings(true);

    if !hook_ok {
        show_notification(
            "The shell keyboard hook could not be installed.",
            NIIF_ERROR,
        );
        return Err(());
    }

    Ok(if paused {
        BrokerState::Paused
    } else {
        BrokerState::Active
    })
}

fn open_settings_section(section: &str) {
    let Ok(dir) = executable_directory() else {
        show_notification("The settings application could not be located.", NIIF_ERROR);
        return;
    };
    let exe = dir.join("fswsettings.exe");
    let arg = format!("fwdslash://settings/{section}");

    unsafe {
        let wide_verb = to_u16_vec("open");
        let wide_file = to_u16_vec(&exe.to_string_lossy());
        let wide_arg = to_u16_vec(&arg);

        let mut exec: SHELLEXECUTEINFOW = std::mem::zeroed();
        let Some(exec_size) = fixed_size_u32::<SHELLEXECUTEINFOW>() else {
            show_notification(
                "The WinUI settings application could not be opened.",
                NIIF_ERROR,
            );
            return;
        };
        exec.cbSize = exec_size;
        exec.fMask = SEE_MASK_FLAG_NO_UI;
        exec.lpVerb = wide_verb.as_ptr();
        exec.lpFile = wide_file.as_ptr();
        exec.lpParameters = wide_arg.as_ptr();
        exec.nShow = SW_SHOWNORMAL;

        if ShellExecuteExW(&raw mut exec) == 0 {
            show_notification(
                "The WinUI settings application could not be opened.",
                NIIF_ERROR,
            );
        }
    }
}

/// Builds the "Open distribution" submenu and records the list it was built
/// from, so a click resolves to the name that was on screen.
fn build_distributions_menu() -> windows_sys::Win32::UI::WindowsAndMessaging::HMENU {
    let submenu = unsafe { CreatePopupMenu() };
    let distributions = list_registered_distributions();
    let listed = distributions.len().min(MENU_DISTRO_MAX);
    if listed == 0 {
        let s_none = to_u16_vec("No distributions registered");
        unsafe {
            AppendMenuW(submenu, MF_STRING | MF_GRAYED, 0, s_none.as_ptr());
        }
    } else {
        for (index, name) in distributions.iter().take(listed).enumerate() {
            let label = to_u16_vec(name);
            let id = MENU_DISTRO_BASE as usize + index;
            unsafe {
                AppendMenuW(submenu, MF_STRING, id, label.as_ptr());
            }
        }
    }
    if let Ok(mut cached) = MENU_DISTRIBUTIONS.lock() {
        *cached = distributions;
    }
    submenu
}

fn build_integrations_menu() -> windows_sys::Win32::UI::WindowsAndMessaging::HMENU {
    let s_windows = to_u16_vec("Windows surfaces");
    let s_cmd = to_u16_vec("Command Prompt");
    let s_win_ps = to_u16_vec("Windows PowerShell");
    let s_ps7 = to_u16_vec("PowerShell 7");
    unsafe {
        let submenu = CreatePopupMenu();
        AppendMenuW(
            submenu,
            MF_STRING,
            MENU_WINDOWS as usize,
            s_windows.as_ptr(),
        );
        AppendMenuW(submenu, MF_STRING, MENU_CMD as usize, s_cmd.as_ptr());
        AppendMenuW(
            submenu,
            MF_STRING,
            MENU_WINDOWS_POWERSHELL as usize,
            s_win_ps.as_ptr(),
        );
        AppendMenuW(submenu, MF_STRING, MENU_POWERSHELL as usize, s_ps7.as_ptr());
        submenu
    }
}

fn show_tray_menu(window: HWND, anchor: POINT) {
    unsafe {
        let menu = CreatePopupMenu();

        let s_settings = to_u16_vec("Open settings");
        let s_enabled = to_u16_vec("Enabled");
        let s_open_root = to_u16_vec("Open WSL root");
        let s_open_distro = to_u16_vec("Open distribution");
        let s_integrations = to_u16_vec("Integrations");
        let version = package_version().unwrap_or_else(|| FSW_VERSION.to_owned());
        let s_version = to_u16_vec(&format!("Forward Slash Windows {version}"));
        let s_exit = to_u16_vec("Exit");

        AppendMenuW(menu, MF_STRING, MENU_SETTINGS as usize, s_settings.as_ptr());
        // Left click and Enter both land on this one.
        SetMenuDefaultItem(menu, MENU_SETTINGS, 0);
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());

        let enabled_flags = if PAUSED.load(Ordering::Relaxed) {
            MF_STRING
        } else {
            MF_STRING | MF_CHECKED
        };
        AppendMenuW(menu, enabled_flags, MENU_PAUSE as usize, s_enabled.as_ptr());
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_OPEN_ROOT as usize,
            s_open_root.as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_POPUP,
            build_distributions_menu() as usize,
            s_open_distro.as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_POPUP,
            build_integrations_menu() as usize,
            s_integrations.as_ptr(),
        );

        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(
            menu,
            MF_STRING | MF_GRAYED,
            MENU_VERSION as usize,
            s_version.as_ptr(),
        );
        AppendMenuW(menu, MF_STRING, MENU_EXIT as usize, s_exit.as_ptr());

        SetForegroundWindow(window);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            anchor.x,
            anchor.y,
            0,
            window,
            std::ptr::null(),
        );
        // Documented Shell_NotifyIcon requirement: without it the menu owner
        // keeps a stale foreground state and the *next* right-click flashes a
        // menu that dismisses itself.
        PostMessageW(window, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

fn open_menu_distribution(index: usize) {
    let name = MENU_DISTRIBUTIONS
        .lock()
        .ok()
        .and_then(|names| names.get(index).cloned());
    if let Some(name) = name {
        request_open_path(format!("\\\\wsl.localhost\\{name}"));
    }
}

fn handle_menu_command(window: HWND, id: u32) {
    match id {
        MENU_SETTINGS => open_settings_section("general"),
        MENU_WINDOWS => open_settings_section("windows"),
        MENU_CMD => open_settings_section("cmd"),
        MENU_WINDOWS_POWERSHELL => open_settings_section("windows-powershell"),
        MENU_POWERSHELL => open_settings_section("powershell"),
        MENU_OPEN_ROOT => request_open_path("\\\\wsl.localhost".to_owned()),
        MENU_PAUSE => {
            // The item is checked while enabled, so clicking it toggles.
            let _ = set_paused(!PAUSED.load(Ordering::Relaxed));
        }
        MENU_EXIT => unsafe {
            DestroyWindow(window);
        },
        _ => {
            if let Some(index) = id.checked_sub(MENU_DISTRO_BASE)
                && let Ok(index) = usize::try_from(index)
                && index < MENU_DISTRO_MAX
            {
                open_menu_distribution(index);
            }
        }
    }
}

/// Tray-icon retry and hook re-arm, spaced by `MAINTENANCE_INTERVAL_MS` so
/// they run once a minute whatever the tick interval is.
fn health_tick(window: HWND) {
    publish_filter_mappings(false);

    let now = unsafe { GetTickCount64() };
    let last = LAST_MAINTENANCE_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < MAINTENANCE_INTERVAL_MS {
        return;
    }
    LAST_MAINTENANCE_MS.store(now, Ordering::Relaxed);
    ensure_tray_icon(window);
    rearm_hook(window);
    // Last, and only ever a spawn: the cycle itself waits on child processes.
    maybe_start_update_cycle();
}

/// Asks the worker to quit, waits ~500 ms for it to signal that it has, and
/// joins it only if it made that deadline.
///
/// The worker may be parked inside a multi-second `ShellExecuteExW` — binding
/// `\\wsl.localhost\<distro>` boots a stopped distribution — and this runs
/// inside `WM_DESTROY`, ahead of the tray-icon removal and the process exit.
/// An unbounded join would keep a ghost icon on screen for as long as the bind
/// takes, so a worker that misses the deadline is detached on purpose.
fn stop_worker() {
    // The worker unhooks its own WinEvent hooks as it leaves `pump_messages`.
    let thread_id = WORKER_THREAD.swap(0, Ordering::Relaxed);
    if thread_id == 0 {
        return;
    }
    unsafe {
        PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
    }

    let mut stopped = false;
    for _ in 0..WORKER_STOP_ATTEMPTS {
        if WORKER_STOPPED.load(Ordering::Acquire) {
            stopped = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(WORKER_STOP_POLL_MS));
    }

    // Take the handle either way: leaving it parked in the static is what
    // makes the timeout path look like a leak.
    let handle = WORKER_JOIN.lock().ok().and_then(|mut slot| slot.take());
    let Some(handle) = handle else {
        return;
    };

    if stopped {
        // It is out of its message loop and past `CoUninitialize`, so the join
        // is immediate and reaps the thread properly.
        let _ = handle.join();
        return;
    }

    // Deliberate detach, not a dropped error: the worker is still inside
    // something that cannot be interrupted, and the process is exiting anyway.
    // The handle goes out of scope here, which detaches the thread and lets
    // process teardown reap it — `WM_DESTROY` continues on to remove the icon.
    log_diagnostic("event=worker_detached");
    drop(handle);
}

/// Raises [`WORKER_BUSY`] for the length of one worker request and lowers it
/// again however the handler returns.
struct WorkerBusy;

impl WorkerBusy {
    fn mark() -> Self {
        WORKER_BUSY.store(true, Ordering::Release);
        Self
    }
}

impl Drop for WorkerBusy {
    fn drop(&mut self) {
        WORKER_BUSY.store(false, Ordering::Release);
    }
}

unsafe extern "system" fn worker_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        PROCESS_ENTER => {
            // `WORKER_BUSY` is the update cycle's veto: an install that
            // force-closes the package while this handler is mid-rewrite would
            // take the user's keystroke down with it.
            let _busy = WorkerBusy::mark();
            if let Ok(generation) = u64::try_from(lparam) {
                process_enter_hwnd(wparam as HWND, generation);
            }
            0
        }
        PROCESS_CACHED_BROWSER_ENTER => {
            let _busy = WorkerBusy::mark();
            if let Ok(generation) = u64::try_from(lparam) {
                process_cached_browser_enter(wparam as HWND, generation);
            }
            0
        }
        BROWSER_DISCOVERY_PROBE => {
            probe_browser_surface(wparam as HWND);
            0
        }
        WORKER_SET_DISCOVERY => {
            if wparam == 0 {
                remove_browser_discovery();
            } else {
                install_browser_discovery();
            }
            0
        }
        WORKER_OPEN_PATH => {
            // The tail of the same request: `request_open_path` posts this from
            // inside `process_enter_request`, and the shell navigation it runs
            // is exactly as bad a moment to be terminated in.
            let _busy = WorkerBusy::mark();
            if lparam != 0 {
                // Ownership was handed over by `request_open_path`.
                let path = unsafe { Box::from_raw(lparam as *mut String) };
                if !perform_navigation(&NavigationAction::OpenDirectory((*path).clone())) {
                    show_notification("Windows could not open the location.", NIIF_ERROR);
                }
            }
            0
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

#[allow(clippy::too_many_lines)]
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        FSW_WM_QUERY_STATE => {
            if PAUSED.load(Ordering::Relaxed) {
                BrokerState::Paused as isize
            } else if (KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK).is_null()
                || (WORKER_WINDOW.load(Ordering::Acquire) as HWND).is_null()
            {
                BrokerState::Unavailable as isize
            } else {
                BrokerState::Active as isize
            }
        }
        // Replies with the resulting `BrokerState` (Active=1 / Paused=2), or 0
        // when the request could not be honoured. The old unconditional 1 made
        // a failed resume indistinguishable from a successful one.
        FSW_WM_SET_PAUSED if integration_test_mode() => BrokerState::Active as isize,
        FSW_WM_SET_PAUSED => match set_paused(wparam != 0) {
            Ok(state) => state as isize,
            Err(()) => 0,
        },
        FSW_WM_SHOW_SETTINGS => {
            if integration_test_mode() {
                0
            } else {
                open_settings_section("general");
                1
            }
        }
        PERSIST_FAILED => {
            if !integration_test_mode() {
                show_notification("The pause setting could not be saved.", NIIF_ERROR);
            }
            0
        }
        WM_TIMER => {
            if !integration_test_mode() && wparam == HEALTH_TIMER {
                health_tick(window);
            }
            0
        }
        message if message == taskbar_created_message() => {
            if !integration_test_mode() {
                ICON_ADDED.store(false, Ordering::Relaxed);
                add_tray_icon(window);
            }
            0
        }
        // Somebody changed shared state (issue #55). It is a broadcast, so it
        // arrives whoever the writer was — the CLI, a shell adapter's staged
        // copy of it, the settings window — and including this process's own
        // writes, which `reload_settings` recognizes as already applied.
        // `message != 0` because a failed registration answers 0, and 0 is
        // WM_NULL — which arrives whenever anything probes this window.
        message if message != 0 && message == state_changed_message() => {
            if !integration_test_mode() {
                reload_settings(window);
            }
            0
        }
        // Session end: Windows destroys the window without WM_DESTROY running
        // our cleanup, so remove the icon now or it lingers as a ghost.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            remove_tray_icon(window);
            0
        }
        WM_COMMAND => {
            if !integration_test_mode() {
                handle_menu_command(window, u32::try_from(wparam & 0xFFFF).unwrap_or(0));
            }
            0
        }
        TRAY_MESSAGE => {
            if integration_test_mode() {
                return 0;
            }
            // NOTIFYICON_VERSION_4: the notification is the low word of
            // lParam and the anchor point rides in wParam.
            let event = u32::try_from(lparam & 0xFFFF).unwrap_or(0);
            match event {
                WM_CONTEXTMENU => {
                    let anchor = POINT {
                        x: signed_word(wparam, 0),
                        y: signed_word(wparam, 16),
                    };
                    show_tray_menu(window, anchor);
                }
                WM_RBUTTONUP => {
                    // Legacy path: reached only if NIM_SETVERSION never took,
                    // where wParam is the icon id and not a point.
                    let mut cursor: POINT = unsafe { std::mem::zeroed() };
                    unsafe {
                        GetCursorPos(&raw mut cursor);
                    }
                    show_tray_menu(window, cursor);
                }
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => open_settings_section("general"),
                _ => {}
            }
            0
        }
        WM_CLOSE => unsafe {
            DestroyWindow(window);
            0
        },
        WM_DESTROY => {
            unsafe {
                KillTimer(window, HEALTH_TIMER);
            }
            remove_tray_icon(window);
            remove_hook();
            stop_worker();
            disconnect_filter();
            BROKER_WINDOW.store(0, Ordering::Relaxed);
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

fn format_resolve_error(err: fsw_path::ResolveError, distributions: &[String]) -> String {
    let mut message = err.message().to_string();
    if err.hint_lists_distributions() && !distributions.is_empty() {
        message.push_str(" Try ");
        let count = distributions.len().min(3);
        for (idx, d) in distributions.iter().take(count).enumerate() {
            if idx != 0 {
                message.push_str(", ");
            }
            message.push('/');
            message.push_str(d);
        }
        message.push('.');
    }
    message
}

/// Registers a window class. Both windows of the process go through this,
/// so the `WNDCLASSEXW` layout is described once.
fn register_window_class(name: &[u16], proc: WNDPROC, icon: HICON) -> bool {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        let Some(class_size) = fixed_size_u32::<WNDCLASSEXW>() else {
            log_diagnostic("event=window_class_size_unrepresentable");
            return false;
        };
        wc.cbSize = class_size;
        wc.lpfnWndProc = proc;
        wc.hInstance = instance;
        wc.hIcon = icon;
        wc.hIconSm = icon;
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        wc.lpszClassName = name.as_ptr();
        RegisterClassExW(&raw const wc) != 0
    }
}

fn pump_messages() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

/// The worker thread: a second STA that owns the UI Automation object and a
/// message-only window, and does every piece of Enter handling that can block.
///
/// `HWND_MESSAGE` is right here precisely because it receives no broadcasts —
/// unlike the broker window, which needs `TaskbarCreated`.
fn worker_thread_main(ready: &std::sync::mpsc::SyncSender<()>) {
    unsafe {
        if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
            let _ = ready.send(());
            return;
        }

        let class_name = to_u16_vec(worker_window_class());
        if !register_window_class(&class_name, Some(worker_proc), std::ptr::null_mut()) {
            CoUninitialize();
            let _ = ready.send(());
            return;
        }

        let title = to_u16_vec("fwdslash broker worker");
        let worker_wnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null_mut(),
        );
        if worker_wnd.is_null() {
            CoUninitialize();
            let _ = ready.send(());
            return;
        }

        // Created once and kept for the thread's life. A failure here is not
        // fatal: every request then falls through to a plain Enter replay.
        match CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            Ok(automation) => AUTOMATION.with_borrow_mut(|slot| *slot = Some(automation)),
            Err(err) => log_diagnostic(&format!("event=debug_uia_failed code={}", err.code().0)),
        }

        WORKER_THREAD.store(GetCurrentThreadId(), Ordering::Relaxed);
        WORKER_WINDOW.store(worker_wnd as isize, Ordering::Release);
        // Installed here, not from the UI thread: `SetWinEventHook` delivers
        // its out-of-context callbacks on the installing thread's pump, and
        // that must not be the thread that owns the keyboard hook (issue #121).
        if !PAUSED.load(Ordering::Relaxed) {
            install_browser_discovery();
        }
        let _ = ready.send(());

        pump_messages();

        // `UnhookWinEvent` is only valid from the installing thread.
        remove_browser_discovery();
        WORKER_WINDOW.store(0, Ordering::Release);
        AUTOMATION.with_borrow_mut(|slot| *slot = None);
        DestroyWindow(worker_wnd);
        CoUninitialize();
        WORKER_STOPPED.store(true, Ordering::Release);
    }
}

/// Starts the worker and waits for it to publish its window, so the very first
/// Enter after startup already has somewhere to go.
fn start_worker() {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let Ok(handle) = std::thread::Builder::new()
        .name("fsw-worker".to_owned())
        .spawn(move || worker_thread_main(&ready_tx))
    else {
        log_diagnostic("event=worker_start_failed");
        return;
    };
    let _ = ready_rx.recv_timeout(std::time::Duration::from_secs(5));
    if let Ok(mut slot) = WORKER_JOIN.lock() {
        *slot = Some(handle);
    }
}

fn main() {
    unsafe {
        let integration_test = integration_test_mode();
        // A malformed test invocation must be inert rather than installing a
        // second desktop-wide hook.
        if integration_test && integration_test_target().is_none() {
            log_diagnostic("event=integration_test_target_missing");
            return;
        }
        let wide_mutex = to_u16_vec(broker_mutex_name());
        let mutex = CreateMutexW(std::ptr::null_mut(), 0, wide_mutex.as_ptr());
        if mutex.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            if !mutex.is_null() {
                CloseHandle(mutex);
            }
            return;
        }

        if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
            CloseHandle(mutex);
            return;
        }

        // No EcoQoS here (issue #121). This process owns a WH_KEYBOARD_LL
        // hook, and a hook callback that outruns `LowLevelHooksTimeout` is
        // removed by Windows silently: asking the scheduler to prefer
        // efficiency cores for the thread that runs it trades a latency budget
        // we do not control for power we do not measure.
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = to_u16_vec(broker_window_class());
        let icon = LoadIconW(instance, IDI_FSW_APP as *const u16);
        if !register_window_class(&class_name, Some(window_proc), icon) {
            CoUninitialize();
            CloseHandle(mutex);
            return;
        }

        // Not "Forward Slash Windows": the settings window carried that title
        // too, and a title-based raise could match this one instead.
        let title = to_u16_vec("fwdslash broker");
        // A top-level never-shown tool window, not a message-only one:
        // message-only windows are skipped by HWND_BROADCAST, so
        // TaskbarCreated and WM_ENDSESSION would never reach the icon
        // lifecycle below. Discovery via FindWindowW on the class is
        // unaffected by the window not being message-only.
        let broker_wnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
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

        if broker_wnd.is_null() {
            CoUninitialize();
            CloseHandle(mutex);
            return;
        }

        BROKER_WINDOW.store(broker_wnd as isize, Ordering::Relaxed);
        // Paused state first: the tray tooltip reflects it at NIM_ADD time.
        PAUSED.store(
            if integration_test {
                false
            } else {
                is_disabled()
            },
            Ordering::Relaxed,
        );
        if !integration_test {
            add_tray_icon(broker_wnd);
        }
        // The worker has to exist before the hook does, or the first Enter is
        // classified with nowhere to post it.
        // The worker installs browser discovery itself, on its own pump.
        start_worker();
        let hook_installed = PAUSED.load(Ordering::Relaxed) || install_hook();
        if !integration_test {
            update_tray_tooltip(broker_wnd);
            let start_tick = GetTickCount64();
            LAST_MAINTENANCE_MS.store(start_tick, Ordering::Relaxed);
            // The update cycle measures its first delay from here, not from boot.
            BROKER_START_MS.store(start_tick, Ordering::Relaxed);
            SetTimer(broker_wnd, HEALTH_TIMER, HEALTH_INTERVAL_IDLE_MS, None);
            // Switches the timer to 5 s if a driver actually answers.
            publish_filter_mappings(true);
        }

        if !hook_installed && !integration_test {
            show_notification(
                "The shell keyboard hook could not be installed.",
                NIIF_ERROR,
            );
        }

        // Last, and off this thread: the icon and the hook are what the user
        // notices missing, and the sweep may take minutes.
        if !integration_test {
            start_adapter_upgrade();
        }

        pump_messages();

        CoUninitialize();
        CloseHandle(mutex);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Only the pure decisions are covered here: everything else in this binary is
// a Win32 call, a child process or a thread, and none of those has a meaning
// outside a running broker. The cost of getting one of these three wrong is a
// balloon at every logon, an update that never runs, or one that runs while
// the user is typing in an address bar.
#[cfg(test)]
mod tests {
    #[test]
    fn browser_identity_is_independent_of_localized_labels() {
        assert_eq!(super::address_identity("", "", "Ctrl+L"), (true, false));
        assert_eq!(
            super::address_identity("", "urlbar-input", ""),
            (true, false)
        );
        assert_eq!(
            super::address_identity("Search this page", "", ""),
            (false, false)
        );
        assert!(super::browser_window_class("Chrome_WidgetWin_1"));
        assert!(super::browser_window_class("MozillaWindowClass"));
        assert!(!super::browser_window_class("WindowsForms10.Window"));
    }

    #[test]
    fn only_exact_system_search_or_start_identities_may_cross_integrity() {
        let search_image =
            r"\\?\C:\Windows\SystemApps\Microsoft.Windows.Search_cw5n1h2txyewy\SearchHost.exe";
        let current_search_image =
            r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\SearchHost.exe";
        let start_image = r"C:\Windows\SystemApps\Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy\StartMenuExperienceHost.exe";

        assert!(super::trusted_windows_search_identity(
            search_image,
            Some("Microsoft.Windows.Search_cw5n1h2txyewy")
        ));
        assert!(super::trusted_windows_search_identity(
            start_image,
            Some("MICROSOFT.WINDOWS.STARTMENUEXPERIENCEHOST_CW5N1H2TXYEWY")
        ));
        assert!(super::trusted_windows_search_identity(
            current_search_image,
            Some("MicrosoftWindows.Client.CBS_cw5n1h2txyewy")
        ));
        assert!(!super::trusted_windows_search_identity(
            r"C:\Users\attacker\SearchHost.exe",
            Some("Microsoft.Windows.Search_cw5n1h2txyewy")
        ));
        assert!(!super::trusted_windows_search_identity(
            search_image,
            Some("Contoso.Windows.Search_cw5n1h2txyewy")
        ));
        assert!(!super::trusted_windows_search_identity(search_image, None));
        assert!(super::trusted_windows_search_integrity(0x1000));
        assert!(super::trusted_windows_search_integrity(0x2000));
        assert!(!super::trusted_windows_search_integrity(0x3000));
    }

    #[test]
    #[ignore = "requires FSW_SEARCH_TEST_HWND for a real Windows Search or Start surface"]
    fn real_search_surface_is_attested() -> Result<(), Box<dyn std::error::Error>> {
        let hwnd = std::env::var("FSW_SEARCH_TEST_HWND")?.parse::<isize>()? as super::HWND;
        let surface = super::attest_foreground_surface(hwnd)
            .ok_or("Search/Start surface was not attested")?;
        if surface.kind != super::SurfaceKind::Search {
            return Err("foreground surface was not classified as Windows Search or Start".into());
        }
        if !super::trusted_windows_search_identity(
            &surface.canonical_image,
            surface.package_identity.as_deref(),
        ) || !super::trusted_windows_search_integrity(surface.integrity_level)
        {
            return Err("Search/Start surface lost its package/image/integrity binding".into());
        }
        println!(
            "PASS: trusted Search/Start PID={} package={:?} image={} IL={}",
            surface.pid, surface.package_identity, surface.canonical_image, surface.integrity_level
        );
        Ok(())
    }

    #[test]
    fn equal_integrity_automation_is_allowed_but_lower_integrity_input_is_not() {
        const INJECTED: u32 = 0x10;
        assert!(super::input_can_drive_broker(0));
        assert!(super::input_can_drive_broker(INJECTED));
        assert!(!super::input_can_drive_broker(
            super::LLKHF_LOWER_IL_INJECTED
        ));
        assert!(!super::input_can_drive_broker(
            INJECTED | super::LLKHF_LOWER_IL_INJECTED
        ));
    }

    #[test]
    #[ignore = "requires a real foreground browser window"]
    #[allow(clippy::too_many_lines)]
    fn browser_worker_translates_into_real_address_bar() -> Result<(), Box<dyn std::error::Error>> {
        use windows::Win32::UI::Accessibility::TreeScope_Descendants;

        unsafe { super::CoInitializeEx(None, super::COINIT_APARTMENTTHREADED).ok()? };
        let automation: super::IUIAutomation = unsafe {
            super::CoCreateInstance(&super::CUIAutomation, None, super::CLSCTX_INPROC_SERVER)?
        };
        super::AUTOMATION.with_borrow_mut(|slot| *slot = Some(automation.clone()));
        if let Ok(delay) = std::env::var("FSW_BROWSER_TEST_FOCUS_DELAY_MS") {
            std::thread::sleep(std::time::Duration::from_millis(delay.parse()?));
        }
        let foreground = unsafe { super::GetForegroundWindow() };
        let surface = super::attest_browser_surface(foreground)
            .ok_or("foreground browser identity was not attested")?;
        if let Ok(suffix) = std::env::var("FSW_BROWSER_TEST_IMAGE_SUFFIX")
            && !super::canonical_image_ends_with(&surface.canonical_image, &suffix)
        {
            return Err(format!(
                "wrong foreground browser: expected image suffix {suffix:?}, got {:?}",
                surface.canonical_image
            )
            .into());
        }
        let root =
            unsafe { automation.ElementFromHandle(windows::Win32::Foundation::HWND(foreground))? };
        let condition = unsafe { automation.CreateTrueCondition()? };
        let descendants = unsafe { root.FindAll(TreeScope_Descendants, &condition)? };
        let mut address_bar = None;
        let mut candidates = Vec::new();
        for index in 0..unsafe { descendants.Length()? } {
            let element = unsafe { descendants.GetElement(index)? };
            if super::trusted_editable_value_pattern(&element, &surface).is_some() {
                address_bar = Some(element);
                break;
            }
            let control_type = unsafe { element.CurrentControlType()? };
            if (control_type == super::UIA_EditControlTypeId
                || control_type == super::UIA_ComboBoxControlTypeId)
                && candidates.len() < 24
            {
                candidates.push(format!(
                    "name={:?} id={:?} accelerator={:?} pid={} belongs={} address={} editable={}",
                    unsafe { element.CurrentName()? }.to_string(),
                    unsafe { element.CurrentAutomationId()? }.to_string(),
                    unsafe { element.CurrentAcceleratorKey()? }.to_string(),
                    unsafe { element.CurrentProcessId()? },
                    super::focused_belongs_to_surface(&element, &surface),
                    super::is_browser_address_bar(&element),
                    super::editable_value_pattern(&element).is_some(),
                ));
            }
        }
        let focused = address_bar.ok_or_else(|| {
            std::io::Error::other(format!(
                "no trusted browser address bar was found; edit candidates: {}",
                candidates.join(" | ")
            ))
        })?;
        unsafe { focused.SetFocus()? };
        let pattern = super::trusted_editable_value_pattern(&focused, &surface)
            .ok_or("discovered address bar lost its trust binding")?;
        let original = unsafe { pattern.CurrentValue()? }.to_string();
        let snapshot = fsw_core::Snapshot::current();
        let distribution = snapshot
            .distributions
            .first()
            .ok_or("browser test requires one registered WSL distribution")?;
        let input = format!("/{distribution}/");
        let expected = fsw_core::resolve_user_target(&input, &snapshot, None)
            .map_err(|error| std::io::Error::other(format!("{error:?}")))?
            .file_uri()
            .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        if !super::set_pattern_value(&pattern, &input) {
            return Err("could not seed the slash path into the address bar".into());
        }
        super::REPLAY_ENTER_CAPTURED.store(false, std::sync::atomic::Ordering::Release);
        super::CAPTURE_REPLAY_ENTER.store(true, std::sync::atomic::Ordering::Release);
        super::process_enter_request(
            &surface,
            super::KEYDOWN_GENERATION.load(std::sync::atomic::Ordering::Acquire),
        );
        super::CAPTURE_REPLAY_ENTER.store(false, std::sync::atomic::Ordering::Release);
        let written = unsafe { pattern.CurrentValue()? }.to_string();
        let restored = super::set_pattern_value(&pattern, &original);
        if written != expected {
            return Err(format!(
                "production worker wrote the wrong value: expected {expected:?}, got {written:?}"
            )
            .into());
        }
        if !super::REPLAY_ENTER_CAPTURED.load(std::sync::atomic::Ordering::Acquire) {
            return Err("production worker did not request the original Enter replay".into());
        }
        if !restored {
            return Err("browser address-bar value could not be restored".into());
        }
        println!(
            "PASS: production worker translated {input:?} to {expected:?} in PID {} ({})",
            surface.pid, surface.canonical_image
        );
        super::AUTOMATION.with_borrow_mut(|slot| *slot = None);
        unsafe { super::CoUninitialize() };
        Ok(())
    }

    #[test]
    #[ignore = "requires the foreground low-integrity Search-class/UIA fake from the security harness"]
    #[allow(clippy::too_many_lines)]
    fn reject_low_integrity_foreground() -> Result<(), Box<dyn std::error::Error>> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let hwnd: usize = std::env::var("FSW_ATTESTATION_TEST_HWND")?.parse()?;
        let pid: u32 = std::env::var("FSW_ATTESTATION_TEST_PID")?.parse()?;
        let edit: isize = std::env::var("FSW_ATTESTATION_TEST_EDIT_HWND")?.parse()?;
        let marker = std::path::PathBuf::from(std::env::var("FSW_ATTESTATION_MARKER")?);
        let input = std::env::var("FSW_ATTESTATION_INPUT")?;
        assert_eq!(
            std::env::var("FSW_ATTESTATION_EXPECTED_VERSION")?,
            env!("CARGO_PKG_VERSION"),
            "attestation binary must match the running broker version"
        );
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        assert_eq!(
            unsafe { super::GetForegroundWindow() },
            hwnd,
            "fake must actually be foreground"
        );
        let mut actual_pid = 0;
        unsafe { super::GetWindowThreadProcessId(hwnd, &raw mut actual_pid) };
        assert_eq!(actual_pid, pid);
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert!(!process.is_null());
        let integrity = super::process_integrity_level(process);
        unsafe { super::CloseHandle(process) };
        assert_eq!(integrity, Some(4096));
        assert_eq!(
            super::process_integrity_level(unsafe { super::GetCurrentProcess() }),
            Some(8192)
        );
        assert_eq!(
            super::window_class(hwnd),
            "Windows.UI.Core.CoreWindow",
            "the fake must exercise Search's top-level window class"
        );
        let identity = super::attest_foreground_identity(hwnd)
            .ok_or("the Search-class fake did not yield foreground process facts")?;
        assert_eq!(identity.0, pid, "foreground identity was not PID-bound");
        assert_eq!(identity.4, 4096, "foreground identity lost Low IL");
        assert!(
            !super::trusted_windows_search_identity(&identity.1, identity.2.as_deref()),
            "a private executable must not satisfy Windows Search package/image identity"
        );

        unsafe { super::CoInitializeEx(None, super::COINIT_APARTMENTTHREADED).ok()? };
        let automation: super::IUIAutomation = unsafe {
            super::CoCreateInstance(&super::CUIAutomation, None, super::CLSCTX_INPROC_SERVER)?
        };
        let edit = unsafe {
            automation.ElementFromHandle(windows::Win32::Foundation::HWND(edit as *mut _))?
        };
        assert_eq!(
            unsafe { edit.CurrentProcessId()? },
            pid.cast_signed(),
            "the focused writable UIA element must belong to the low-IL fake PID"
        );
        let pattern = unsafe {
            edit.GetCurrentPatternAs::<super::IUIAutomationValuePattern>(super::UIA_ValuePatternId)?
        };
        assert!(
            !unsafe { pattern.CurrentIsReadOnly()? }.as_bool(),
            "the fake must expose a writable native UIA ValuePattern"
        );
        assert_eq!(unsafe { pattern.CurrentValue()? }.to_string(), input);
        assert!(
            super::attest_foreground_surface(hwnd).is_none(),
            "production attestation must reject the Low-IL Search-class impersonator"
        );
        assert!(
            !marker.exists(),
            "the canary was already launched before the attestation boundary"
        );

        // This is the same physical-Enter path the low-level hook takes. It
        // must stop at the production foreground attestation boundary: no
        // worker message, UIA value mutation, or canary launch is permitted.
        let previous_down = super::ENTER_DOWN.swap(false, std::sync::atomic::Ordering::AcqRel);
        let previous_suppress =
            super::SUPPRESS_ENTER_UP.swap(false, std::sync::atomic::Ordering::AcqRel);
        let mut key: super::KBDLLHOOKSTRUCT = unsafe { std::mem::zeroed() };
        key.vkCode = u32::from(super::VK_RETURN);
        let _ = unsafe {
            super::low_level_keyboard_proc(
                0,
                0x0100,
                (&raw mut key).cast::<std::ffi::c_void>() as super::LPARAM,
            )
        };
        super::ENTER_DOWN.store(previous_down, std::sync::atomic::Ordering::Release);
        super::SUPPRESS_ENTER_UP.store(previous_suppress, std::sync::atomic::Ordering::Release);
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert_eq!(
            unsafe { pattern.CurrentValue()? }.to_string(),
            input,
            "the rejected UIA edit was mutated by the keyboard handling boundary"
        );
        assert!(
            !marker.exists(),
            "the rejected UIA edit was mutated and launched its marker"
        );
        unsafe { super::CoUninitialize() };
        println!(
            "PASS: production physical-Enter attestation rejected low-integrity Search-class PID {pid} without UIA mutation or marker launch"
        );
        Ok(())
    }
    use super::{
        AdapterOutcome, UPDATE_CONSIDER_INTERVAL_MS, UPDATE_FIRST_DELAY_MS, adapter_outcome,
        drain_persist_queue, offer_key, request_target_is_current, should_balloon_install_failure,
        should_balloon_update, should_swallow_enter, update_cycle_age_ms, update_cycle_due,
    };
    use windows_sys::Win32::Foundation::HWND;

    // -- the cycle gate ----------------------------------------------------

    #[test]
    fn a_due_idle_allowed_broker_starts_a_cycle() {
        assert!(update_cycle_due(
            false,
            UPDATE_CONSIDER_INTERVAL_MS,
            false,
            true
        ));
    }

    #[test]
    fn each_gate_alone_stops_the_cycle() {
        // Already running.
        assert!(!update_cycle_due(
            true,
            UPDATE_CONSIDER_INTERVAL_MS,
            false,
            true
        ));
        // Not due yet.
        assert!(!update_cycle_due(
            false,
            UPDATE_CONSIDER_INTERVAL_MS - 1,
            false,
            true
        ));
        // The Enter worker is mid-request.
        assert!(!update_cycle_due(
            false,
            UPDATE_CONSIDER_INTERVAL_MS,
            true,
            true
        ));
        // Unpackaged, or Automatic updates off.
        assert!(!update_cycle_due(
            false,
            UPDATE_CONSIDER_INTERVAL_MS,
            false,
            false
        ));
    }

    #[test]
    fn the_first_cycle_waits_out_the_startup_delay() {
        // Ticks are ms since boot; a broker started an hour in.
        const START: u64 = 3_600_000;
        assert_eq!(update_cycle_age_ms(START, START, 0), 0);
        assert_eq!(
            update_cycle_age_ms(START + UPDATE_FIRST_DELAY_MS - 1, START, 0),
            0
        );
        assert_eq!(
            update_cycle_age_ms(START + UPDATE_FIRST_DELAY_MS, START, 0),
            UPDATE_CONSIDER_INTERVAL_MS
        );
    }

    #[test]
    fn later_cycles_are_a_full_interval_apart() {
        const START: u64 = 3_600_000;
        let last = START + UPDATE_FIRST_DELAY_MS;
        assert_eq!(update_cycle_age_ms(last, START, last), 0);
        assert_eq!(
            update_cycle_age_ms(last + UPDATE_CONSIDER_INTERVAL_MS, START, last),
            UPDATE_CONSIDER_INTERVAL_MS
        );
        // A tick count that went backwards must not read as an enormous age.
        assert_eq!(update_cycle_age_ms(last - 1_000, START, last), 0);
    }

    // -- balloon dedupe ----------------------------------------------------

    #[test]
    fn one_version_produces_one_balloon() {
        assert!(should_balloon_update(&offer_key(Some("0.0.5")), None));
        assert!(!should_balloon_update(
            &offer_key(Some("0.0.5")),
            Some("0.0.5")
        ));
        // A second update replacing the first is news again.
        assert!(should_balloon_update(
            &offer_key(Some("0.0.6")),
            Some("0.0.5")
        ));
    }

    #[test]
    fn a_nameless_update_is_announced_once() {
        let nameless = offer_key(None);
        // Nothing announced yet: balloon, named or not.
        assert!(should_balloon_update(&nameless, None));
        // ...and then stay quiet, which is what the doc comment always
        // promised and the old match did not deliver: a nameless offer
        // re-announced itself on every cycle forever, because "nothing
        // announced" and "announced something nameless" were the same state.
        assert!(!should_balloon_update(&nameless, Some(&nameless)));
        // A named offer replacing it is news again, and the reverse too.
        assert!(should_balloon_update(
            &offer_key(Some("0.0.6")),
            Some(&nameless)
        ));
        assert!(should_balloon_update(&nameless, Some("0.0.5")));
        // The key for a nameless offer can never collide with a real version.
        assert_ne!(nameless, offer_key(Some("0.0.5")));
    }

    #[test]
    fn only_the_store_flavor_reports_a_failed_install_and_only_on_the_second() {
        assert!(!should_balloon_install_failure(1, true));
        assert!(should_balloon_install_failure(2, true));
        assert!(should_balloon_install_failure(7, true));
        // The GitHub flavor's bundle applies itself at the next logon.
        assert!(!should_balloon_install_failure(2, false));
    }

    // -- #56: retry and deferral ------------------------------------------

    #[test]
    fn a_first_pass_success_never_retries() {
        // The caller passes `None` for the retry it did not run.
        assert_eq!(adapter_outcome(Some(0), None), AdapterOutcome::Upgraded);
    }

    #[test]
    fn a_retry_that_lands_is_silent_success() {
        assert_eq!(adapter_outcome(Some(1), Some(0)), AdapterOutcome::Upgraded);
        assert_eq!(adapter_outcome(None, Some(0)), AdapterOutcome::Upgraded);
    }

    #[test]
    fn two_unanswered_attempts_defer_instead_of_ballooning() {
        // Killed at the deadline, or never spawned: the marker key still reads
        // the old version, so the next launch tries again.
        assert_eq!(adapter_outcome(None, None), AdapterOutcome::Deferred);
        assert_eq!(adapter_outcome(Some(1), None), AdapterOutcome::Deferred);
    }

    #[test]
    fn a_second_refusal_is_the_one_the_user_must_see() {
        assert_eq!(adapter_outcome(Some(1), Some(1)), AdapterOutcome::NeedsUser);
        assert_eq!(adapter_outcome(None, Some(2)), AdapterOutcome::NeedsUser);
    }

    /// The two refusals that are about the user's own Documents folder, not
    /// about a broken install, get their own outcomes on the first answer —
    /// retrying them changes nothing (#127).
    #[test]
    fn confirmation_and_cfa_refusals_are_reported_separately() {
        assert_eq!(
            adapter_outcome(Some(4), None),
            AdapterOutcome::NeedsConfirmation
        );
        assert_eq!(
            adapter_outcome(Some(4), Some(4)),
            AdapterOutcome::NeedsConfirmation
        );
        assert_eq!(adapter_outcome(Some(5), None), AdapterOutcome::Blocked);
        assert_eq!(adapter_outcome(None, Some(5)), AdapterOutcome::Blocked);
        assert_eq!(
            adapter_outcome(None, Some(4)),
            AdapterOutcome::NeedsConfirmation
        );
    }

    // -- #65: pause persistence ordering ----------------------------------

    #[test]
    fn persistence_queue_keeps_rapid_toggles_in_order() {
        let (sender, receiver) = std::sync::mpsc::channel();
        assert!(sender.send(true).is_ok());
        assert!(sender.send(false).is_ok());
        assert!(sender.send(true).is_ok());
        drop(sender);

        let mut persisted = Vec::new();
        drain_persist_queue(&receiver, |disabled| persisted.push(disabled));
        assert_eq!(persisted, [true, false, true]);
    }

    // -- #7/#64: hook admission and stale replay --------------------------

    #[test]
    fn null_worker_never_admits_an_enter_for_suppression() {
        // PostMessageW(NULL, ...) may report success, but it is not a worker.
        assert!(!should_swallow_enter(false, true));
        assert!(!should_swallow_enter(false, false));
        assert!(!should_swallow_enter(true, false));
        assert!(should_swallow_enter(true, true));
    }

    #[test]
    fn stale_request_never_replays_into_a_new_foreground_window() {
        let original = 101isize as HWND;
        assert!(request_target_is_current(original, original));
        assert!(!request_target_is_current(original, 202isize as HWND));
    }
}
