mod adapters;

use fsw_core::*;
use fsw_path::{BareSlashMode, RenderBuf, ResolveError, Resolved, is_valid_windows_root};
use std::env;
use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

#[cfg(windows)]
#[link(name = "fltlib", kind = "raw-dylib")]
unsafe extern "system" {
    fn FilterConnectCommunicationPort(
        lpPortName: *const u16,
        dwOptions: u32,
        lpContext: *const std::ffi::c_void,
        wSizeOfContext: u16,
        lpSecurityAttributes: *mut std::ffi::c_void,
        hPort: *mut *mut std::ffi::c_void,
    ) -> i32;
}

const SYNCHRONIZE: u32 = 0x0010_0000;
/// HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND); what `remove_value` reports for an
/// absent value, which is the desired end state rather than a failure.
const ERROR_FILE_NOT_FOUND_HRESULT: u32 = 0x8007_0002;

fn to_u16_vec(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = OsStr::new(s).encode_wide().collect();
    v.push(0);
    v
}

fn from_u16_slice(slice: &[u16]) -> String {
    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    std::ffi::OsString::from_wide(&slice[..len])
        .to_string_lossy()
        .into_owned()
}

fn is_driver_available() -> bool {
    #[cfg(windows)]
    unsafe {
        let port_name = to_u16_vec(FSW_FILTER_PORT_NAME);
        let mut handle = std::ptr::null_mut();
        let hr = FilterConnectCommunicationPort(
            port_name.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            &mut handle,
        );
        if hr >= 0 && !handle.is_null() {
            windows_sys::Win32::Foundation::CloseHandle(handle);
            true
        } else {
            false
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Asks the running broker to unpause (`FSW_WM_SET_PAUSED` with 0). True only
/// when the broker accepted the message.
#[cfg(windows)]
fn send_resume() -> bool {
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
        };

        let class = to_u16_vec(FSW_BROKER_WINDOW_CLASS);
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if hwnd.is_null() {
            return false;
        }
        let mut result: usize = 0;
        let sent = SendMessageTimeoutW(
            hwnd,
            FSW_WM_SET_PAUSED,
            0,
            0,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            2000,
            &mut result,
        );
        sent != 0 && result != 0
    }
}

/// An honest explanation for a rejected `FSW_WM_SET_PAUSED` reply. The broker
/// answers 0 when it could not apply the change completely — a failed
/// `persist_disabled` on pause, a failed `install_hook` on resume — so ask
/// what state it actually reached instead of blaming the message.
#[cfg(windows)]
fn state_change_failure(paused: bool) -> &'static str {
    if !broker_window_exists() {
        return "The broker did not accept the state change.";
    }
    match (paused, broker_state(1000)) {
        (true, BrokerState::Paused) => "Resolution paused, but the setting could not be saved.",
        (false, BrokerState::Unavailable) => {
            "Resolution enabled, but the keyboard hook could not be installed."
        }
        _ => "The broker did not accept the state change.",
    }
}

fn start_broker() -> i32 {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, WAIT_OBJECT_0};
        use windows_sys::Win32::System::SystemInformation::GetTickCount64;
        use windows_sys::Win32::System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW, Sleep,
            WaitForSingleObject,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
        };

        if broker_window_exists() {
            let state = broker_state(1000);
            if state == BrokerState::Active {
                println!("Forward Slash Windows broker is already active.");
                return 0;
            }
            if state == BrokerState::Paused {
                if send_resume() {
                    if broker_state(1000) == BrokerState::Active {
                        println!("Forward Slash Windows broker resumed and is active.");
                        return 0;
                    }
                    eprintln!("Broker resumed but its keyboard hook is unavailable.");
                    return 1;
                }
                eprintln!("{}", state_change_failure(false));
                return 1;
            }
            eprintln!("Broker is running but its keyboard hook is unavailable.");
            return 1;
        }

        let dir = executable_directory().unwrap_or_else(|_| PathBuf::from("."));
        let broker_path = dir.join("fswbroker.exe");
        let broker_str = format!("\"{}\"", broker_path.display());
        let mut cmd_wide = to_u16_vec(&broker_str);
        let app_wide = to_u16_vec(&broker_path.to_string_lossy());
        let dir_wide = to_u16_vec(&dir.to_string_lossy());

        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut process: PROCESS_INFORMATION = std::mem::zeroed();

        let ok = CreateProcessW(
            app_wide.as_ptr(),
            cmd_wide.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            CREATE_NEW_PROCESS_GROUP | 0x0800_0000,
            std::ptr::null(),
            dir_wide.as_ptr(),
            &startup,
            &mut process,
        );

        if ok == 0 {
            eprintln!("Unable to start broker. Win32 error {}.", GetLastError());
            return 1;
        }

        CloseHandle(process.hThread);
        let deadline = GetTickCount64() + 5000;
        let mut resume_attempted = false;
        loop {
            match broker_state(1000) {
                BrokerState::Active => {
                    CloseHandle(process.hProcess);
                    println!("Forward Slash Windows broker started and is active.");
                    return 0;
                }
                BrokerState::Paused if !resume_attempted => {
                    // A fresh spawn comes up paused when Disabled=1; resume it
                    // the same way the already-running branch does.
                    resume_attempted = true;
                    send_resume();
                }
                _ => {}
            }
            if WaitForSingleObject(process.hProcess, 0) == WAIT_OBJECT_0 {
                break;
            }
            Sleep(50);
            if GetTickCount64() >= deadline {
                break;
            }
        }

        // The probe failed. Something with the broker class exists, but it is
        // not necessarily the process we spawned -- a losing spawn exits at
        // the mutex and the class window belongs to some other instance. Only
        // ever close the window our own process owns.
        let state = broker_state(1000);
        if state == BrokerState::Paused {
            // A paused broker (ours or not) is healthy; surface the pause.
            CloseHandle(process.hProcess);
            eprintln!("Resolution is paused; run \"fwdslash enable\" to activate.");
            return 1;
        }
        let class = to_u16_vec(FSW_BROKER_WINDOW_CLASS);
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if !hwnd.is_null() {
            let mut owner = 0u32;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == process.dwProcessId {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
                WaitForSingleObject(process.hProcess, 2000);
            }
        }
        CloseHandle(process.hProcess);
        eprintln!("Broker started but its keyboard hook is unavailable.");
        1
    }
    #[cfg(not(windows))]
    {
        1
    }
}

fn stop_broker() -> i32 {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::SystemInformation::GetTickCount64;
        use windows_sys::Win32::System::Threading::{OpenProcess, Sleep, WaitForSingleObject};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
        };

        let class = to_u16_vec(FSW_BROKER_WINDOW_CLASS);
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if hwnd.is_null() {
            println!("Forward Slash Windows broker is not running.");
            return 0;
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let process = OpenProcess(SYNCHRONIZE, 0, pid);
        PostMessageW(hwnd, WM_CLOSE, 0, 0);

        if !process.is_null() {
            let wait = WaitForSingleObject(process, 5000);
            CloseHandle(process);
            if wait != WAIT_OBJECT_0 {
                eprintln!("Broker did not stop within five seconds.");
                return 1;
            }
        } else {
            let deadline = GetTickCount64() + 5000;
            while !FindWindowW(class.as_ptr(), std::ptr::null()).is_null()
                && GetTickCount64() < deadline
            {
                Sleep(50);
            }
            if !FindWindowW(class.as_ptr(), std::ptr::null()).is_null() {
                eprintln!("Broker did not stop within five seconds.");
                return 1;
            }
        }
        println!("Forward Slash Windows broker stopped.");
        0
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn set_paused(paused: bool) -> i32 {
    if persist_disabled(paused).is_err() {
        eprintln!("The pause setting could not be saved.");
        return 1;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW,
        };

        let class = to_u16_vec(FSW_BROKER_WINDOW_CLASS);
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if hwnd.is_null() {
            if paused {
                println!("Forward-slash resolution disabled.");
            } else {
                println!("Forward-slash resolution enabled.");
            }
            return 0;
        }

        let mut result: usize = 0;
        let success = SendMessageTimeoutW(
            hwnd,
            FSW_WM_SET_PAUSED,
            if paused { 1 } else { 0 },
            0,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            2000,
            &mut result,
        );

        if success == 0 || result == 0 {
            eprintln!("{}", state_change_failure(paused));
            return 1;
        }

        if paused {
            println!("Forward-slash resolution disabled.");
        } else {
            println!("Forward-slash resolution enabled.");
        }
        0
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn set_startup(enabled: bool) -> i32 {
    if has_package_identity() {
        if !enabled {
            eprintln!(
                "Startup for the packaged app is controlled by Windows. Turn it off under Settings > Apps > Startup."
            );
        }
        return 0;
    }
    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;
        let key = match CURRENT_USER.create(RUN_KEY) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("Unable to open the per-user startup key. Error {:?}.", e);
                return 1;
            }
        };

        if enabled {
            let broker_path = match executable_directory() {
                Ok(dir) => dir.join("fswbroker.exe"),
                Err(_) => PathBuf::from("fswbroker.exe"),
            };
            let val = format!("\"{}\"", broker_path.display());
            if let Err(e) = key.set_string(RUN_VALUE, val) {
                eprintln!("Unable to update startup registration. Error {:?}.", e);
                return 1;
            }
        } else if let Err(error) = key.remove_value(RUN_VALUE) {
            // An already-absent value is the goal state, not a failure.
            if error.code().0 as u32 != ERROR_FILE_NOT_FOUND_HRESULT {
                eprintln!("Unable to remove startup registration. Error {error:?}.");
                return 1;
            }
        }
        0
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        0
    }
}

fn set_settings_protocol(enabled: bool) -> i32 {
    if has_package_identity() {
        return 0;
    }
    let settings = match executable_directory() {
        Ok(dir) => dir.join("fswsettings.exe"),
        Err(_) => PathBuf::from("fswsettings.exe"),
    };
    let command = format!("\"{}\" \"%1\"", settings.display());
    let command_key = format!(r"{}\shell\open\command", PROTOCOL_KEY);

    #[cfg(windows)]
    {
        use windows_registry::CURRENT_USER;
        if enabled {
            if !settings.exists() {
                eprintln!(
                    "The WinUI settings application was not found: {}",
                    settings.display()
                );
                return 1;
            }
            if let Ok(existing) = CURRENT_USER.open(&command_key) {
                if let Ok(val) = existing.get_string("") {
                    if val == command {
                        return 0;
                    }
                }
                eprintln!(
                    "The fwdslash URI scheme is already owned by another application. No protocol registration was changed."
                );
                return 1;
            }

            let root = match CURRENT_USER.create(PROTOCOL_KEY) {
                Ok(k) => k,
                Err(e) => {
                    eprintln!(
                        "Unable to create the fwdslash URI registration. Error {:?}.",
                        e
                    );
                    return 1;
                }
            };
            let _ = root.set_string("", "URL:Forward Slash Windows");
            let _ = root.set_string("URL Protocol", "");

            let cmd_k = match CURRENT_USER.create(&command_key) {
                Ok(k) => k,
                Err(e) => {
                    let _ = CURRENT_USER.remove_tree(PROTOCOL_KEY);
                    eprintln!(
                        "Unable to complete the fwdslash URI registration. Error {:?}.",
                        e
                    );
                    return 1;
                }
            };
            if let Err(e) = cmd_k.set_string("", &command) {
                let _ = CURRENT_USER.remove_tree(PROTOCOL_KEY);
                eprintln!(
                    "Unable to complete the fwdslash URI registration. Error {:?}.",
                    e
                );
                return 1;
            }
            0
        } else {
            if let Ok(existing) = CURRENT_USER.open(&command_key) {
                if let Ok(val) = existing.get_string("") {
                    if val != command {
                        eprintln!(
                            "The fwdslash URI handler changed after registration. Refusing to remove another application's value."
                        );
                        return 1;
                    }
                }
            }
            let _ = CURRENT_USER.remove_tree(PROTOCOL_KEY);
            0
        }
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        0
    }
}

fn set_windows_integration(enabled: bool) -> i32 {
    if enabled {
        if set_settings_protocol(true) != 0 || set_startup(true) != 0 {
            return 1;
        }
        let started = start_broker();
        if started != 0 {
            set_startup(false);
        }
        started
    } else {
        let stopped = stop_broker();
        let unregistered = set_startup(false);
        if stopped != 0 { stopped } else { unregistered }
    }
}

fn show_bare_slash_state() -> i32 {
    let snap = Snapshot::current();
    let mut buf = RenderBuf::new();
    let bare = resolve_user_slash_path("/", &snap, &mut buf);

    let mode_str = match snap.bare_slash_mode {
        BareSlashMode::DefaultDistribution => "default distribution",
        BareSlashMode::DistributionList => "distribution list",
    };
    println!("bare slash mode: {}", mode_str);

    match &snap.bare_slash_root {
        Some(root) => println!("custom root: {}", root),
        None => println!("custom root: none"),
    }
    if let Some(pinned) = &snap.bare_slash_pinned {
        println!("pinned distribution: /{}", pinned);
    }
    match &snap.default_distribution {
        Some(def) => println!("WSL default distribution: /{}", def),
        None => println!("WSL default distribution: none"),
    }

    match bare {
        Ok(resolved) => println!("/ resolves to: {}", resolved.unc_display()),
        Err(err) => println!(
            "/ is blocked: {}",
            format_resolve_error(err, &snap.distributions)
        ),
    }
    0
}

fn set_bare_slash(default_mode: bool, pinned: &str, root: Option<&str>) -> i32 {
    if default_mode && !pinned.is_empty() && !is_registered_distribution(pinned) {
        eprintln!("That WSL distribution is not registered.");
        return 1;
    }
    if let Some(path) = root {
        if !is_valid_windows_root(path) {
            eprintln!(
                "That is not a usable folder root. Use an absolute path like \
                 C:\\code or \\\\wsl.localhost\\Ubuntu\\home\\me."
            );
            return 1;
        }
    }
    // The dispatcher passes root=None for every non-root mutation, so the
    // radios and a configured folder can never disagree about what `/` means.
    if write_bare_slash_settings(default_mode, pinned, root).is_err() {
        return 1;
    }
    show_bare_slash_state()
}

fn format_resolve_error(err: ResolveError, distributions: &[String]) -> String {
    let mut message = err.message().to_string();
    if err.hint_lists_distributions() && !distributions.is_empty() {
        message.push_str(" Try ");
        let count = distributions.len().min(3);
        for (idx, d) in distributions[..count].iter().enumerate() {
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

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

/// What a shell verb should do with a slash path. The targets are owned
/// strings because a borrowed `Resolved` cannot outlive the `RenderBuf` that
/// rendered it.
enum ShellTarget {
    /// Bare `/` in distribution-list mode: there is no single target.
    Root { distributions: Vec<String> },
    /// A path inside one registered distribution.
    Distribution { target: String },
    /// A path under the configured custom folder root.
    Folder { target: String },
}

/// Why a shell verb has no target to offer.
enum ShellExit {
    /// The shell must run its own command unchanged (exit 3).
    Native,
    /// The resolver rejected the input; the message is for the user (exit 1).
    Rejected(String),
}

/// The single resolution funnel behind `cmd-cd`, `shell-resolve` and
/// `cmd-list`. It reads `Snapshot::current()` and nothing else — no broker
/// round trip, no filter-port probe — because a shell adapter runs it on
/// every `cd` and `dir` the user types.
fn shell_target(input: &str) -> Result<ShellTarget, ShellExit> {
    if !input.starts_with('/') {
        return Err(ShellExit::Native);
    }
    let snap = Snapshot::current();
    // The same value `is_disabled()` reads, out of the settings key the
    // snapshot has already opened.
    if snap.disabled {
        return Err(ShellExit::Native);
    }
    let mut buf = RenderBuf::new();
    let outcome = match resolve_user_slash_path(input, &snap, &mut buf) {
        Ok(Resolved::WslRoot) => Ok(None),
        Ok(resolved) => Ok(Some((
            resolved.distribution().is_some(),
            resolved.unc_display().to_string(),
        ))),
        Err(err) => Err(ShellExit::Rejected(format_resolve_error(
            err,
            &snap.distributions,
        ))),
    };
    match outcome {
        Ok(None) => Ok(ShellTarget::Root {
            distributions: snap.distributions,
        }),
        Ok(Some((true, target))) => Ok(ShellTarget::Distribution { target }),
        Ok(Some((false, target))) => Ok(ShellTarget::Folder { target }),
        Err(exit) => Err(exit),
    }
}

/// What every surface says when a bare `/` cannot become one directory.
fn root_distribution_hint(distributions: &[String]) -> String {
    if distributions.is_empty() {
        return "No WSL distributions are registered.".to_string();
    }
    let mut list = String::new();
    for (index, distro) in distributions.iter().enumerate() {
        if index != 0 {
            list.push_str(", ");
        }
        list.push('/');
        list.push_str(distro);
    }
    format!(
        "/ lists your WSL distributions: {list}. Use cd /<Distro>, or run \
         \"fwdslash bare-slash default\" so / opens your default distribution."
    )
}

/// `fwdslash cmd-cd <input>` — the target for the cmd adapter's CD/PUSHD
/// macros. Stdout carries the Win32 path and nothing else, so the batch file
/// can capture it verbatim; exit 3 means "run your own CD".
fn cmd_shell_cd(input: &str) -> i32 {
    match shell_target(input) {
        Ok(ShellTarget::Distribution { target } | ShellTarget::Folder { target }) => {
            println!("{target}");
            0
        }
        Ok(ShellTarget::Root { distributions }) => {
            eprintln!("{}", root_distribution_hint(&distributions));
            1
        }
        Err(ShellExit::Rejected(message)) => {
            eprintln!("{message}");
            1
        }
        Err(ShellExit::Native) => 3,
    }
}

/// `fwdslash shell-resolve <input>` — one JSON line for the PowerShell
/// module, which needs the kind, the target and the distribution list from a
/// single spawn.
fn cmd_shell_resolve(input: &str) -> i32 {
    match shell_target(input) {
        Ok(ShellTarget::Root { distributions }) => {
            let list: Vec<String> = distributions
                .iter()
                .map(|d| format!("\"{}\"", json_escape(d)))
                .collect();
            println!(
                "{{\"kind\":\"root\",\"target\":null,\"distributions\":[{}]}}",
                list.join(",")
            );
            0
        }
        Ok(ShellTarget::Distribution { target }) => {
            println!(
                "{{\"kind\":\"distribution\",\"target\":\"{}\",\"distributions\":[]}}",
                json_escape(&target)
            );
            0
        }
        Ok(ShellTarget::Folder { target }) => {
            println!(
                "{{\"kind\":\"folder\",\"target\":\"{}\",\"distributions\":[]}}",
                json_escape(&target)
            );
            0
        }
        Err(ShellExit::Rejected(message)) => {
            println!("{{\"error\":\"{}\"}}", json_escape(&message));
            1
        }
        Err(ShellExit::Native) => {
            println!("{{\"kind\":\"native\"}}");
            3
        }
    }
}

fn cmd_status(json: bool) -> i32 {
    let snap = Snapshot::current();
    // Window check first: a stopped broker costs one FindWindowW, and a
    // wedged one only waits the short informational timeout.
    let broker_status = if !broker_window_exists() {
        "stopped"
    } else {
        match broker_state(200) {
            BrokerState::Active => "running (active)",
            BrokerState::Paused => "running (paused)",
            BrokerState::Unavailable => "running (hook unavailable)",
        }
    };
    let driver_conn = is_driver_available();

    if json {
        let mut buf = RenderBuf::new();
        let bare = resolve_user_slash_path("/", &snap, &mut buf);
        let bare_target = match bare {
            Ok(r) => format!("\"{}\"", json_escape(r.unc_display())),
            Err(_) => "null".to_string(),
        };
        let mode_str = match snap.bare_slash_mode {
            BareSlashMode::DefaultDistribution => "default",
            BareSlashMode::DistributionList => "list",
        };
        let distro_list: Vec<String> = snap
            .distributions
            .iter()
            .map(|d| format!("\"{}\"", json_escape(d)))
            .collect();

        println!(
            "{{\"broker\":\"{}\",\"driverConnected\":{},\"disabled\":{},\"bareSlashMode\":\"{}\",\"bareSlashTarget\":{},\"bareSlashRoot\":{},\"wslRoot\":\"\\\\\\\\wsl.localhost\",\"distributions\":[{}]}}",
            broker_status,
            driver_conn,
            snap.disabled,
            mode_str,
            bare_target,
            snap
                .bare_slash_root
                .as_deref()
                .map(|root| format!("\"{}\"", json_escape(root)))
                .unwrap_or_else(|| "null".to_string()),
            distro_list.join(",")
        );
        return 0;
    }

    println!("broker: {}", broker_status);
    println!(
        "global state: {}",
        if snap.disabled { "disabled" } else { "enabled" }
    );
    println!(
        "filesystem driver: {}",
        if driver_conn {
            "connected"
        } else {
            "not connected"
        }
    );
    println!("registered distributions: {}", snap.distributions.len());

    let mut buf = RenderBuf::new();
    match resolve_user_slash_path("/", &snap, &mut buf) {
        Ok(resolved) => {
            let note = if resolved.is_provider_root() {
                " (distribution list)"
            } else if resolved.distribution().is_none() {
                " (custom folder root)"
            } else {
                " (default distribution)"
            };
            println!("  / -> {}{}", resolved.unc_display(), note);
        }
        Err(err) => {
            println!(
                "  / -> blocked. {}",
                format_resolve_error(err, &snap.distributions)
            );
        }
    }

    for d in &snap.distributions {
        println!("  /{}/ -> \\\\wsl.localhost\\{}\\", d, d);
    }
    0
}

fn cmd_resolve(path: &str) -> i32 {
    let snap = Snapshot::current();
    let mut buf = RenderBuf::new();
    match resolve_user_slash_path(path, &snap, &mut buf) {
        Ok(resolved) => {
            println!("{}", resolved.unc_display());
            0
        }
        Err(err) => {
            eprintln!("{}", format_resolve_error(err, &snap.distributions));
            1
        }
    }
}

fn cmd_open(path: &str) -> i32 {
    let snap = Snapshot::current();
    let mut buf = RenderBuf::new();
    let resolved = match resolve_user_slash_path(path, &snap, &mut buf) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("{}", format_resolve_error(err, &snap.distributions));
            return 1;
        }
    };

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW};
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let wide_verb = to_u16_vec("open");
        let wide_target = to_u16_vec(resolved.unc_display());

        let mut exec: SHELLEXECUTEINFOW = std::mem::zeroed();
        exec.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        exec.lpVerb = wide_verb.as_ptr();
        exec.lpFile = wide_target.as_ptr();
        exec.nShow = SW_SHOWNORMAL;

        if ShellExecuteExW(&mut exec) != 0 {
            0
        } else {
            eprintln!(
                "Windows could not open the target. Error {}.",
                GetLastError()
            );
            1
        }
    }
    #[cfg(not(windows))]
    {
        let _ = resolved;
        0
    }
}

/// Enumerates one already-resolved directory. `Err` carries the Win32 error
/// from `FindFirstFileW` so the caller decides between reporting it and
/// handing the command back to the shell.
fn list_directory(target: &str) -> Result<i32, u32> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{
            ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW,
        };

        let mut pattern = target.to_string();
        if !pattern.ends_with('\\') {
            pattern.push('\\');
        }
        pattern.push('*');

        let wide_pattern = to_u16_vec(&pattern);
        let mut entry: WIN32_FIND_DATAW = std::mem::zeroed();
        let search = FindFirstFileW(wide_pattern.as_ptr(), &mut entry);

        if search == INVALID_HANDLE_VALUE {
            return Err(GetLastError());
        }

        loop {
            let name = from_u16_slice(&entry.cFileName);
            if name != "." && name != ".." {
                let is_dir = (entry.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                println!("{}{}", if is_dir { "[dir]  " } else { "       " }, name);
            }
            if FindNextFileW(search, &mut entry) == 0 {
                break;
            }
        }
        let final_err = GetLastError();
        FindClose(search);
        if final_err == ERROR_NO_MORE_FILES {
            Ok(0)
        } else {
            Ok(1)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        Ok(0)
    }
}

fn cmd_list(path: &str) -> i32 {
    let snap = Snapshot::current();
    let mut buf = RenderBuf::new();
    let resolved = match resolve_user_slash_path(path, &snap, &mut buf) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("{}", format_resolve_error(err, &snap.distributions));
            return 1;
        }
    };

    if resolved.is_provider_root() {
        for d in &snap.distributions {
            println!("[distro] /{}", d);
        }
        if snap.distributions.is_empty() {
            println!("No registered WSL distributions were found.");
        }
        return 0;
    }

    match list_directory(resolved.unc_display()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!(
                "Unable to enumerate {}. Error {}.",
                resolved.unc_display(),
                error
            );
            1
        }
    }
}

/// `fwdslash cmd-list <input>` — DIR for the cmd adapter. Exit 3 hands the
/// command back to the shell's own DIR: resolution is disabled, the input is
/// not a slash path, the resolver rejected it, or the target does not exist
/// (a DIR switch the batch file's shape test let through, or a stale path).
/// Resolution happens once, here, and the target is passed to the listing.
fn cmd_cmd_list(path: &str) -> i32 {
    let Ok(target) = shell_target(path) else {
        return 3;
    };
    match target {
        ShellTarget::Root { distributions } => {
            for d in &distributions {
                println!("[distro] /{d}");
            }
            if distributions.is_empty() {
                println!("No registered WSL distributions were found.");
            }
            0
        }
        ShellTarget::Distribution { target } | ShellTarget::Folder { target } => {
            match list_directory(&target) {
                Ok(code) => code,
                // ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND: nothing is
                // there, so native DIR is the better answer than an error.
                Err(2 | 3) => 3,
                Err(error) => {
                    eprintln!("Unable to enumerate {target}. Error {error}.");
                    1
                }
            }
        }
    }
}

fn cmd_doctor_single(path: &str, snap: &Snapshot) -> i32 {
    let mut buf = RenderBuf::new();
    let resolved = match resolve_user_slash_path(path, snap, &mut buf) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("resolver: {}", err.name());
            return 1;
        }
    };

    let is_root = resolved.is_provider_root();
    // The same three-way match `cmd_status` makes: a custom folder root is
    // neither the provider root nor a distribution path, and calling it one
    // printed an empty distribution and a Linux path that was really the
    // tail under a Win32 folder.
    let kind = match resolved {
        Resolved::WslRoot => "WSL distribution list",
        Resolved::Distribution(_) => "distribution path",
        Resolved::Folder(_) if resolved.linux_path() == "/" => "custom folder root",
        Resolved::Folder(_) => "path under custom root",
    };
    println!("target kind: {kind}");
    println!("distribution: {}", resolved.distribution().unwrap_or("none"));
    println!("linux path: {}", resolved.linux_path());
    println!("windows target: {}", resolved.unc_display());

    if is_root {
        println!(
            "Shell namespace: {}",
            if snap.distributions.is_empty() {
                "no registered distributions"
            } else {
                "available"
            }
        );
        return if snap.distributions.is_empty() { 2 } else { 0 };
    }

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
        };

        let wide_path = to_u16_vec(resolved.unc_display());
        let attrs = GetFileAttributesW(wide_path.as_ptr());
        if attrs == INVALID_FILE_ATTRIBUTES {
            println!(
                "target access: unavailable (Win32 error {})",
                GetLastError()
            );
            2
        } else {
            println!("target access: available");
            0
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn cmd_doctor_all() -> i32 {
    let snap = Snapshot::current();
    let mut outcome = cmd_doctor_single("/", &snap);
    for d in &snap.distributions {
        let path = format!("/{}", d);
        let res = cmd_doctor_single(&path, &snap);
        if res > outcome {
            outcome = res;
        }
    }
    outcome
}

fn cmd_settings(section: &str) -> i32 {
    let dir = executable_directory().unwrap_or_else(|_| PathBuf::from("."));
    let settings = dir.join("fswsettings.exe");
    if !settings.exists() {
        eprintln!(
            "The WinUI settings application was not found: {}",
            settings.display()
        );
        return 1;
    }
    let sec = if section.is_empty() {
        "general"
    } else {
        section
    };
    let arg = format!("fwdslash://settings/{}", sec);

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW};
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let wide_verb = to_u16_vec("open");
        let wide_file = to_u16_vec(&settings.to_string_lossy());
        let wide_arg = to_u16_vec(&arg);

        let mut exec: SHELLEXECUTEINFOW = std::mem::zeroed();
        exec.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        exec.lpVerb = wide_verb.as_ptr();
        exec.lpFile = wide_file.as_ptr();
        exec.lpParameters = wide_arg.as_ptr();
        exec.nShow = SW_SHOWNORMAL;

        if ShellExecuteExW(&mut exec) != 0 {
            0
        } else {
            1
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// The install state of one shell adapter, including whether its deployed
/// payload predates this build (#13).
fn integration_label(installed: bool, outdated: bool) -> &'static str {
    if !installed {
        "not installed"
    } else if outdated {
        "installed (update available)"
    } else {
        "installed"
    }
}

fn cmd_integrations(json: bool) -> i32 {
    let disabled = is_disabled();
    let windows = windows_integration_installed();
    let win_ps_key = format!("{}WindowsPowerShell", POWERSHELL_ADAPTER_ROOT);
    let ps7_key = format!("{}PowerShell", POWERSHELL_ADAPTER_ROOT);
    let cmd = adapter_installed(CMD_ADAPTER_KEY);
    let win_ps = adapter_installed(&win_ps_key);
    let ps7 = adapter_installed(&ps7_key);
    let cmd_outdated = adapter_outdated(CMD_ADAPTER_KEY, adapters::PAYLOAD_VERSION);
    let win_ps_outdated = adapter_outdated(&win_ps_key, adapters::PAYLOAD_VERSION);
    let ps7_outdated = adapter_outdated(&ps7_key, adapters::PAYLOAD_VERSION);
    let ps7_avail = executable_available("pwsh.exe");

    if json {
        println!(
            "{{\"disabled\":{},\"windows\":{},\"cmd\":{},\"windowsPowerShell\":{},\"powerShell7\":{},\"powerShell7Available\":{},\"cmdOutdated\":{},\"windowsPowerShellOutdated\":{},\"powerShell7Outdated\":{}}}",
            disabled,
            windows,
            cmd,
            win_ps,
            ps7,
            ps7_avail,
            cmd_outdated,
            win_ps_outdated,
            ps7_outdated
        );
        return 0;
    }

    println!(
        "resolution: {}",
        if disabled { "disabled" } else { "enabled" }
    );
    println!(
        "Windows surfaces: {}",
        if windows {
            "installed"
        } else {
            "not installed"
        }
    );
    println!("Command Prompt: {}", integration_label(cmd, cmd_outdated));
    println!(
        "Windows PowerShell: {}",
        integration_label(win_ps, win_ps_outdated)
    );
    print!("PowerShell 7: {}", integration_label(ps7, ps7_outdated));
    if !ps7_avail {
        print!(" (PowerShell 7 unavailable)");
    }
    println!();
    0
}

/// `fwdslash version`. The packaged four-part identity version when there is
/// one, so a Store or GitHub install reports what the shell and `Get-AppxPackage`
/// report; otherwise the crate version compiled in as `FSW_VERSION`, which is
/// the same string the `.rc` VERSIONINFO is generated from.
fn cmd_version() -> i32 {
    println!(
        "fwdslash {}",
        package_version().unwrap_or_else(|| FSW_VERSION.to_owned())
    );
    0
}

fn usage() {
    print!(
        "Forward Slash Windows controller\n\n\
         \x20 fwdslash status [--json]\n\
         \x20 fwdslash resolve /Distro/path\n\
         \x20 fwdslash open /Distro/path\n\
         \x20 fwdslash list /Distro/path\n\
         \x20 fwdslash cmd-list /Distro/path    Shell adapter DIR; exit 3 means run native DIR\n\
         \x20 fwdslash cmd-cd /Distro/path      Prints the directory for the cmd CD macro\n\
         \x20 fwdslash shell-resolve /Distro/path   One JSON line for the PowerShell module\n\
         \x20 fwdslash doctor /Distro/path | --all\n\
         \x20 fwdslash settings [general|windows|cmd|windows-powershell|powershell]\n\
         \x20 fwdslash integrations [--json]\n\
         \x20 fwdslash integration <name> enable|disable\n\
         \x20 fwdslash bare-slash\n\
         \x20 fwdslash bare-slash list | default [Distro]\n\
         \x20 fwdslash bare-slash root <WindowsPath>\n\
         \x20 fwdslash disable | enable\n\
         \x20 fwdslash pause | resume       Aliases for disable and enable\n\
         \x20 fwdslash driver status\n\
         \x20 fwdslash start | stop\n\
         \x20 fwdslash install       Register and start the per-user broker\n\
         \x20 fwdslash uninstall     Stop and unregister the per-user broker\n\
         \x20 fwdslash version | --version | -V\n\n\
         The optional filesystem driver is production-gated and is never installed by these per-user commands.\n"
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }

    let command = args[1].as_str();
    // The single operand the shell verbs take, without a panicking index.
    let operand = args.get(2).map_or("", String::as_str);
    let exit_code = match command {
        "version" | "--version" | "-V" if args.len() == 2 => cmd_version(),
        "status" => {
            if args.len() > 3 || (args.len() == 3 && args[2] != "--json") {
                usage();
                2
            } else {
                cmd_status(args.len() == 3)
            }
        }
        "resolve" if args.len() == 3 => cmd_resolve(&args[2]),
        "open" if args.len() == 3 => cmd_open(&args[2]),
        "list" if args.len() == 3 => cmd_list(&args[2]),
        "cmd-list" if args.len() == 3 => cmd_cmd_list(&args[2]),
        "cmd-cd" if args.len() == 3 => cmd_shell_cd(operand),
        "shell-resolve" if args.len() == 3 => cmd_shell_resolve(operand),
        "doctor" => {
            if args.len() != 3 {
                usage();
                2
            } else if args[2] == "--all" {
                cmd_doctor_all()
            } else {
                let snap = Snapshot::current();
                cmd_doctor_single(&args[2], &snap)
            }
        }
        "settings" if args.len() == 2 || args.len() == 3 => {
            cmd_settings(if args.len() == 3 { &args[2] } else { "general" })
        }
        "integrations" => {
            if args.len() == 2 || (args.len() == 3 && args[2] == "--json") {
                cmd_integrations(args.len() == 3)
            } else {
                usage();
                2
            }
        }
        "bare-slash" => {
            if args.len() == 2 {
                show_bare_slash_state()
            } else {
                let op = args[2].as_str();
                if args.len() == 3 && op == "list" {
                    set_bare_slash(false, "", None)
                } else if args.len() == 3 && op == "default" {
                    set_bare_slash(true, "", None)
                } else if args.len() == 4 && op == "default" {
                    set_bare_slash(true, &args[3], None)
                } else if args.len() == 3 && op == "root" {
                    // Root set/clear is orthogonal to the mode and the pin:
                    // preserve both, the way every other mutation preserves
                    // the root.
                    let snap = Snapshot::current();
                    let default_mode = snap.bare_slash_mode == BareSlashMode::DefaultDistribution;
                    let pinned = snap.bare_slash_pinned.as_deref().unwrap_or("");
                    set_bare_slash(default_mode, pinned, None)
                } else if args.len() == 4 && op == "root" {
                    let snap = Snapshot::current();
                    let default_mode = snap.bare_slash_mode == BareSlashMode::DefaultDistribution;
                    let pinned = snap.bare_slash_pinned.as_deref().unwrap_or("");
                    set_bare_slash(default_mode, pinned, Some(&args[3]))
                } else {
                    usage();
                    2
                }
            }
        }
        "integration" if args.len() == 4 => {
            let name = args[2].as_str();
            let op = args[3].as_str();
            if op != "enable" && op != "disable" {
                usage();
                2
            } else {
                let enabled = op == "enable";
                if name == "windows" {
                    set_windows_integration(enabled)
                } else {
                    let result = adapters::set_integration(name, enabled);
                    if result == 2 {
                        usage();
                    }
                    result
                }
            }
        }
        "pause" | "disable" if args.len() == 2 => set_paused(true),
        "resume" | "enable" if args.len() == 2 => set_paused(false),
        "driver" if args.len() == 3 && args[2] == "status" => {
            let conn = is_driver_available();
            println!("{}", if conn { "connected" } else { "not connected" });
            if conn { 0 } else { 1 }
        }
        "start" => start_broker(),
        "stop" => stop_broker(),
        "install" => set_windows_integration(true),
        "uninstall" => {
            // Sweep the shell adapters first so their helper state (payload,
            // markers, profile blocks) goes with the rest of the product.
            let sweep = adapters::sweep_uninstall();
            // Downloaded update bundles go with the product; best effort,
            // since a leftover bundle must not fail the uninstall.
            let _ = fsw_core::update::sweep_update_directory();
            let win = set_windows_integration(false);
            let proto = set_settings_protocol(false);
            if win != 0 { win } else if proto != 0 { proto } else { sweep }
        }
        _ => {
            usage();
            2
        }
    };

    std::process::exit(exit_code);
}
