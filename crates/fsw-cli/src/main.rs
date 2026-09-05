mod adapters;
mod scheduled_task;
mod update;

use fsw_core::*;
use fsw_path::{BareSlashMode, RenderBuf, ResolveError, Resolved, is_valid_windows_root};
use std::env;
use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

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

/// Whether the minifilter's communication port answers. The probe itself lives
/// in `fsw-core` so the settings window runs exactly the same one.
fn is_driver_available() -> bool {
    filter_port_available()
}

/// The optional filesystem driver, as one bool plus one of the four words
/// `fwdslash driver status` prints. A port that answers outranks whatever the
/// SCM says; otherwise the service decides. The settings window mirrors this
/// mapping (`DriverStatus` in `crates/fsw-settings/src/main.rs`) — keep the two
/// in step.
fn driver_state() -> (bool, &'static str) {
    let connected = is_driver_available();
    if connected {
        return (true, "connected");
    }
    let label = match filter_service_state() {
        FilterServiceState::NotInstalled => "not installed",
        FilterServiceState::Stopped => "installed, not loaded",
        FilterServiceState::Running => "loaded, not connected",
    };
    (false, label)
}

/// Asks the running broker to unpause (`FSW_WM_SET_PAUSED` with 0). True only
/// when the broker accepted the message.
#[cfg(windows)]
fn send_resume() -> bool {
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW,
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
    let (driver_conn, driver_state_label) = driver_state();

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

        // The four update fields are APPENDED. Everything already in this line
        // keeps its name and its position: the settings window, the shell
        // adapters and the packaging scripts all read it, and a reordering
        // would be a silent break.
        println!(
            "{{\"broker\":\"{}\",\"driverConnected\":{},\"driverState\":\"{}\",\"disabled\":{},\"bareSlashMode\":\"{}\",\"bareSlashTarget\":{},\"bareSlashRoot\":{},\"wslRoot\":\"\\\\\\\\wsl.localhost\",\"distributions\":[{}],\"flavor\":\"{}\",\"autoUpdate\":{},\"availableUpdate\":{},\"lastUpdateCheck\":{}}}",
            broker_status,
            driver_conn,
            driver_state_label,
            snap.disabled,
            mode_str,
            bare_target,
            snap.bare_slash_root
                .as_deref()
                .map(|root| format!("\"{}\"", json_escape(root)))
                .unwrap_or_else(|| "null".to_string()),
            distro_list.join(","),
            update::flavor_name(),
            fsw_core::update::read_auto_update_enabled(),
            fsw_core::update::cached_update_tag()
                .map(|tag| format!("\"{}\"", json_escape(&tag)))
                .unwrap_or_else(|| "null".to_string()),
            fsw_core::update::last_update_check()
                .map_or_else(|| "null".to_string(), |value| value.to_string()),
        );
        return 0;
    }

    println!("broker: {}", broker_status);
    println!(
        "global state: {}",
        if snap.disabled { "disabled" } else { "enabled" }
    );
    println!("filesystem driver: {driver_state_label}");
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
    for (label, value) in doctor_target_fields(resolved, snap) {
        println!("{label}: {value}");
    }

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

/// The diagnostic fields for one resolved target. Folder roots are Win32
/// locations, not distributions, so their normalized tail is never labelled a
/// Linux path.
fn doctor_target_fields(resolved: Resolved<'_>, snap: &Snapshot) -> Vec<(&'static str, String)> {
    match resolved {
        Resolved::WslRoot => vec![
            ("target kind", "WSL distribution list".to_string()),
            ("distribution", "none".to_string()),
            ("linux path", "/".to_string()),
            ("windows target", resolved.unc_display().to_string()),
        ],
        Resolved::Distribution(path) => vec![
            ("target kind", "distribution path".to_string()),
            ("distribution", path.distribution().to_string()),
            ("linux path", path.linux_path().to_string()),
            ("windows target", path.unc_display().to_string()),
        ],
        Resolved::Folder(path) => vec![
            (
                "target kind",
                if path.under_root() == "/" {
                    "custom folder root"
                } else {
                    "path under custom root"
                }
                .to_string(),
            ),
            (
                "custom root",
                snap.bare_slash_root
                    .as_deref()
                    .unwrap_or(path.display())
                    .to_string(),
            ),
            ("path under root", path.under_root().to_string()),
            ("windows target", path.display().to_string()),
        ],
    }
}

/// A scheduler task from a current or legacy updater attempt. Matching the
/// complete generated grammar, rather than a broad prefix, keeps uninstall
/// from touching another product's task with a similar name.
fn is_owned_update_task_name(name: &str) -> bool {
    let name = name.strip_prefix('\\').unwrap_or(name);
    if name == update::relaunch::WATCHDOG_TASK_NAME {
        return true;
    }
    if !scheduled_task::is_safe_task_literal(name) {
        return false;
    }
    let mut parts = name.split('-');
    matches!(
        (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ),
        (Some("fwdslash"), Some("update"), Some("watchdog" | "apply"), Some(pid), Some(sequence), None)
            if !pid.is_empty() && !sequence.is_empty()
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
    )
}

/// Task Scheduler CSV is locale-independent in its first (task-name) field.
/// Our names cannot contain a quote, so this intentionally small CSV reader is
/// safer than interpreting localized headers or command text.
fn owned_update_task_inventory(csv: &str) -> Vec<String> {
    csv.lines()
        .filter_map(|line| {
            let field = line.trim().strip_prefix('"')?;
            let name = field.split('"').next()?;
            is_owned_update_task_name(name).then(|| name.trim_start_matches('\\').to_string())
        })
        .collect()
}

fn cleanup_update_tasks_for_uninstall() -> bool {
    use std::process::{Command, Stdio};

    // Do not sweep a freshly acquired attempt token from another session. The
    // guard is intentionally held only across this bounded uninstall cleanup.
    let Some(_guard) = update::relaunch::lock_update_storage_for_uninstall() else {
        return false;
    };
    let mut names = Command::new("schtasks.exe")
        .args(["/query", "/fo", "csv", "/nh"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .map_or_else(Vec::new, |output| {
            owned_update_task_inventory(&String::from_utf8_lossy(&output.stdout))
        });
    // Query failure must not leave the legacy fixed watchdog behind.
    if !names
        .iter()
        .any(|name| name == update::relaunch::WATCHDOG_TASK_NAME)
    {
        names.push(update::relaunch::WATCHDOG_TASK_NAME.to_string());
    }
    // Stop/delete every task before deleting either its sidecar or update
    // storage. `delete_task` issues `/end` before releasing its definition.
    for name in &names {
        let _ = scheduled_task::delete_task(name);
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let temp = PathBuf::from(local_app_data).join("Temp");
        if let Ok(entries) = std::fs::read_dir(temp) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let stem = file_name
                    .strip_suffix(".cmd")
                    .or_else(|| file_name.strip_suffix(".xml"));
                if stem.is_some_and(is_owned_update_task_name) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
    fsw_core::update::sweep_update_directory().is_ok()
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

/// One edition's execution-policy fields for `integrations --json` (#45).
/// Appended to the existing object, never renamed into it: an unknown edition
/// reports an empty policy string and `false`, so a consumer that only reads
/// the 0.0.3 fields is unaffected.
fn policy_json_fields(key: &str, reported: &str, blocked: bool, remedy: &str) -> String {
    format!(
        ",\"{key}ExecutionPolicy\":\"{}\",\"{key}PolicyBlocked\":{},\"{key}PolicyRemedy\":\"{}\"",
        json_escape(reported),
        blocked,
        json_escape(remedy)
    )
}

/// The execution-policy fields for both PowerShell editions, in the order the
/// text output reports them.
fn execution_policy_json() -> String {
    #[cfg(windows)]
    {
        let statuses = adapters::execution_policy_statuses();
        let mut out = String::new();
        for (key, edition) in [
            ("windowsPowerShell", adapters::Edition::WindowsPowerShell),
            ("powerShell7", adapters::Edition::PowerShell),
        ] {
            match statuses.iter().find(|status| status.edition == edition) {
                Some(status) => out.push_str(&policy_json_fields(
                    key,
                    &status.reported,
                    status.blocked,
                    &status.remedy,
                )),
                None => out.push_str(&policy_json_fields(key, "", false, "")),
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        String::new()
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
        // Additive only: every field 0.0.3 emitted keeps its name and meaning,
        // and the execution-policy fields (#45) are appended. An edition whose
        // shell could not be asked reports an empty policy and `false`.
        println!(
            "{{\"disabled\":{},\"windows\":{},\"cmd\":{},\"windowsPowerShell\":{},\"powerShell7\":{},\"powerShell7Available\":{},\"cmdOutdated\":{},\"windowsPowerShellOutdated\":{},\"powerShell7Outdated\":{}{}}}",
            disabled,
            windows,
            cmd,
            win_ps,
            ps7,
            ps7_avail,
            cmd_outdated,
            win_ps_outdated,
            ps7_outdated,
            execution_policy_json()
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
    print_shell_integration_health();
    0
}

/// `fwdslash repair-adapters`: the settings mirror first, then the adapters'
/// profile/AutoRun hygiene.
///
/// The mirror comes first because it is what the adapters *read* (issue #52).
/// Run through the packaged identity — which is how the broker's startup sweep
/// and the settings window's launch sweep invoke this — it copies the settings
/// out of the package's private hive into the real one, where the unpackaged
/// staged `fwdslash.exe` behind `cd /` can see them. Unpackaged it is a no-op,
/// so the exit code stays the repair's.
fn repair_adapters() -> i32 {
    let _ = fsw_core::sync_settings_to_real_hive();
    adapters::repair_all()
}

/// Prints each shell adapter's integration-hygiene line (#37): `healthy`, or a
/// named problem such as `orphaned profile block for 0.0.1`. Read-only — the
/// broker/settings `repair-adapters` sweep is what fixes them.
fn print_shell_integration_health() {
    let lines = adapters::health_report();
    if lines.is_empty() {
        return;
    }
    println!("shell integration health:");
    for (label, status) in lines {
        println!("  {label}: {status}");
    }
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
         \x20 fwdslash integration <name> enable|disable|repair\n\
         \x20 fwdslash bare-slash\n\
         \x20 fwdslash bare-slash list | default [Distro]\n\
         \x20 fwdslash bare-slash root <WindowsPath>\n\
         \x20 fwdslash disable | enable\n\
         \x20 fwdslash pause | resume       Aliases for disable and enable\n\
         \x20 fwdslash update check [--json] [--force]\n\
         \x20 fwdslash update install [--json] [--force] [--relaunch app|broker|none]\n\
         \x20 fwdslash update status --json\n\
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
            } else {
                let code = if args[2] == "--all" {
                    cmd_doctor_all()
                } else {
                    let snap = Snapshot::current();
                    cmd_doctor_single(&args[2], &snap)
                };
                print_shell_integration_health();
                code
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
            if op == "repair" {
                // Detect-and-repair a single adapter's shell-integration
                // hygiene (#37). `windows` is registry-only, nothing to repair.
                if name == "windows" {
                    0
                } else {
                    let result = adapters::repair_integration(name);
                    if result == 2 {
                        usage();
                    }
                    result
                }
            } else if op != "enable" && op != "disable" {
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
        // Repairs every shell adapter's hygiene. The broker startup sweep and
        // the settings launch sweep invoke this so orphaned/duplicate profile
        // blocks self-heal on the next run (#37).
        "repair-adapters" if args.len() == 2 => repair_adapters(),
        "pause" | "disable" if args.len() == 2 => set_paused(true),
        "resume" | "enable" if args.len() == 2 => set_paused(false),
        "driver" if args.len() == 3 && args[2] == "status" => {
            let (connected, label) = driver_state();
            println!("{label}");
            if connected { 0 } else { 1 }
        }
        // Self-update. Every route, both flavors, and the two helper-only
        // apply verbs live behind this one arm -- and so does the only
        // CoInitializeEx in the binary.
        "update" if args.len() >= 3 => update::run(args.get(2..).unwrap_or_default()),
        "start" => start_broker(),
        "stop" => stop_broker(),
        "install" => set_windows_integration(true),
        // The orphan self-clean a leftover shell hook runs on the next shell
        // start after the product was removed without running code, e.g. an
        // MSIX uninstall (#37). It confirms the product is gone, then removes
        // every trace including its own directory.
        "uninstall" if args.get(2).map(String::as_str) == Some("--orphaned") => {
            adapters::cleanup_orphaned()
        }
        "uninstall" => {
            // Sweep the shell adapters first so their helper state (payload,
            // markers, profile blocks) goes with the rest of the product.
            let sweep = adapters::sweep_uninstall();
            // Downloaded update bundles, the staged helper and the helper's
            // result file all live in the update directory and go with the
            // product; best effort, since a leftover must not fail the
            // uninstall. The relaunch watchdog is a scheduled task rather than
            // a file, so it needs its own sweep -- an update task that fired
            // after an uninstall would relaunch a product that is gone.
            let updates_cleaned = cleanup_update_tasks_for_uninstall();
            let win = set_windows_integration(false);
            let proto = set_settings_protocol(false);
            if win != 0 {
                win
            } else if proto != 0 {
                proto
            } else if !updates_cleaned {
                1
            } else {
                sweep
            }
        }
        _ => {
            usage();
            2
        }
    };

    // One notification for the whole invocation, and only for a verb that
    // finished cleanly (issue #55). A settings window that is open right now
    // re-reads on this; without it it keeps rendering what it read at launch
    // until it is closed and reopened.
    if exit_code == 0 && broadcasts_state_change(command, args.len()) {
        broadcast_state_changed();
    }

    std::process::exit(exit_code);
}

/// Whether a verb that just succeeded changed state some other component
/// renders, and so has to be broadcast (issue #55).
///
/// Deliberately a whitelist of the mutating verbs rather than "everything that
/// exited 0": `resolve`, `status`, `list` and the three shell-adapter verbs run
/// on every prompt and every `cd`, and a broadcast per `cd` would have the
/// settings window re-reading the registry all day.
///
/// `argc` is the whole `argv` length, matching the dispatch above, and is what
/// separates `bare-slash` (prints the current mode, changes nothing) from
/// `bare-slash default` (changes it).
///
/// The settings-key writes already broadcast from `fsw_core::settings_write`,
/// so `bare-slash` and `pause`/`resume` are covered twice on the happy path;
/// they stay listed because the verbs are the contract, and a write that is
/// skipped as redundant is not.
fn broadcasts_state_change(command: &str, argc: usize) -> bool {
    match command {
        // Adapter payload, profile blocks and marker keys — none of them under
        // the settings key, so nothing else announces them.
        "integration" => argc == 4,
        "repair-adapters" => argc == 2,
        // Run key, protocol registration, the adapter sweep.
        "install" | "uninstall" => true,
        // The broker's presence is the status line's "broker" column.
        "start" | "stop" => true,
        // Global pause, via the broker when one is running and the registry
        // when none is.
        "pause" | "disable" | "resume" | "enable" => argc == 2,
        // `bare-slash` alone is a read; every longer form writes.
        "bare-slash" => argc > 2,
        _ => false,
    }
}

#[cfg(test)]
mod doctor_and_update_cleanup_tests {
    use super::{doctor_target_fields, is_owned_update_task_name, owned_update_task_inventory};
    use fsw_core::Snapshot;
    use fsw_path::{BareSlashMode, RenderBuf};

    #[test]
    fn updater_inventory_only_selects_the_generated_task_grammar() {
        for name in [
            "fwdslash-update",
            "\\fwdslash-update-watchdog-123-1",
            "fwdslash-update-apply-456-99",
        ] {
            assert!(is_owned_update_task_name(name), "{name}");
        }
        for name in [
            "fwdslash-update-watchdog-x-1",
            "fwdslash-update-apply-1-2-extra",
            "fwdslash-update-other-1-2",
            "fwdslash-update-watchdog-1-2 & whoami",
            "someone-elses-update-watchdog-1-2",
        ] {
            assert!(!is_owned_update_task_name(name), "{name}");
        }
        let inventory = owned_update_task_inventory(
            "\"\\fwdslash-update-watchdog-123-1\",\"Task\"\r\n\"\\unrelated\",\"Task\"\r\n\"\\fwdslash-update\",\"Task\"\r\n",
        );
        assert_eq!(
            inventory,
            ["fwdslash-update-watchdog-123-1", "fwdslash-update"]
        );
    }

    #[test]
    fn folder_doctor_fields_describe_the_custom_root_not_a_fake_distro() {
        let root = r"C:\source";
        let snap = Snapshot {
            distributions: Vec::new(),
            default_distribution: None,
            bare_slash_mode: BareSlashMode::DistributionList,
            bare_slash_pinned: None,
            bare_slash_root: Some(root.to_string()),
            disabled: false,
        };
        let mut buffer = RenderBuf::new();
        let resolved =
            fsw_path::resolve_under_root("/tools", root, &mut buffer).expect("folder path");
        let fields = doctor_target_fields(resolved, &snap);
        assert!(fields.contains(&("custom root", root.to_string())));
        assert!(fields.contains(&("path under root", "/tools".to_string())));
        assert!(
            !fields
                .iter()
                .any(|(label, _)| *label == "distribution" || *label == "linux path")
        );
    }
}

#[cfg(test)]
mod broadcast_tests {
    use super::broadcasts_state_change;

    #[test]
    fn mutating_verbs_broadcast() {
        assert!(broadcasts_state_change("bare-slash", 3));
        assert!(broadcasts_state_change("bare-slash", 4));
        assert!(broadcasts_state_change("integration", 4));
        assert!(broadcasts_state_change("repair-adapters", 2));
        assert!(broadcasts_state_change("install", 2));
        assert!(broadcasts_state_change("uninstall", 2));
        assert!(broadcasts_state_change("uninstall", 3));
        assert!(broadcasts_state_change("start", 2));
        assert!(broadcasts_state_change("stop", 2));
        for verb in ["pause", "resume", "enable", "disable"] {
            assert!(broadcasts_state_change(verb, 2), "{verb}");
        }
    }

    /// The read-only verbs, including the two that run on every shell prompt.
    #[test]
    fn read_only_verbs_stay_quiet() {
        for verb in [
            "status",
            "resolve",
            "open",
            "list",
            "cmd-list",
            "cmd-cd",
            "shell-resolve",
            "doctor",
            "settings",
            "integrations",
            "version",
            "driver",
        ] {
            assert!(!broadcasts_state_change(verb, 3), "{verb}");
        }
    }

    /// `bare-slash` with no operand prints the mode; `integration` with the
    /// wrong arity never reaches an adapter. Neither changed anything.
    #[test]
    fn arity_separates_reads_from_writes() {
        assert!(!broadcasts_state_change("bare-slash", 2));
        assert!(!broadcasts_state_change("integration", 3));
        assert!(!broadcasts_state_change("integration", 5));
    }

    /// `enable`/`disable` are the pause verbs only in their bare form — the
    /// operand forms are `integration <id> enable`, dispatched elsewhere.
    #[test]
    fn pause_aliases_need_their_bare_form() {
        assert!(!broadcasts_state_change("enable", 3));
        assert!(!broadcasts_state_change("disable", 3));
    }
}
