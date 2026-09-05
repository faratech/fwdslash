//! One-shot per-user scheduled tasks: the only way this product can run
//! something *after* the process asking for it has gone.
//!
//! Two callers need that. The orphan self-clean has to delete the directory it
//! is executing from, and the self-updater has to survive the Store force-closing
//! the package in order to bring it back. Both were impossible with a detached
//! child: when the triggering process is itself inside a job object (WSL interop
//! is the case that exposed this) the whole tree is killed the moment the
//! launching command returns, and `CREATE_BREAKAWAY_FROM_JOB` does not even
//! spawn, because the job forbids breakaway. The Task Scheduler service starts
//! the script in its own session, outside any job we are in, so it always
//! survives.
//!
//! The shape is fixed and deliberately small:
//!
//! * a caller-owned task name per attempt, so concurrent updater attempts never
//!   overwrite a batch file another `cmd.exe` may already be executing;
//! * `/tr` is a **bare quoted path** to a `.cmd` in `%LOCALAPPDATA%\Temp` and
//!   never an embedded command line, so `schtasks`' own quoting rules cannot
//!   bite;
//! * the task is created with a backstop trigger one minute out *and* run
//!   immediately, so a machine that goes down before the script runs still
//!   cleans up at the trigger;
//! * every literal that reaches a command line is checked by
//!   [`is_safe_task_literal`] first.
//!
//! Everything here is best-effort and silent: a Task Scheduler that is absent or
//! refuses is a `None`, never a panic and never a message.

#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::process::Command;

/// `CREATE_NO_WINDOW`: no console flash for the `schtasks` children.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Whether `value` may be pasted into a `schtasks` argument or into the body of
/// the batch file a task runs.
///
/// The allowed set is ASCII letters, digits and `. _ - !` — enough for a task
/// name, a package family name, an `AppId` and a dotted version, and nothing
/// else. Everything with meaning to `cmd.exe` or to PowerShell is therefore
/// excluded by construction: no space, no quote, no `%`, no `&`, `|`, `^`, `<`,
/// `>`, `$`, `` ` ``, `(`, `)`, `;`, `,`, backslash or forward slash. Empty is
/// not safe either — an empty `/tn` is a `schtasks` syntax error, and an empty
/// literal spliced into a script silently changes what the script means.
#[must_use]
pub fn is_safe_task_literal(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'!'))
}

/// `HH:MM` one minute after `hour:minute`, wrapping at midnight — the `/st`
/// value for the backstop trigger.
///
/// One minute rather than zero because `schtasks` refuses a start time that has
/// already passed; wrapping rather than clamping because `24:00` is not a time
/// `schtasks` accepts.
#[must_use]
pub fn task_start_time(hour: u32, minute: u32) -> String {
    let next = (hour * 60 + minute + 1) % (24 * 60);
    format!("{:02}:{:02}", next / 60, next % 60)
}

/// The locale-independent `StartBoundary` used in a Task Scheduler XML
/// definition. Unlike `/st`, this carries the date as well as the time.
#[must_use]
pub fn task_start_boundary(year: u16, month: u16, day: u16, hour: u16, minute: u16) -> String {
    fn leap(year: u16) -> bool {
        year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
    }
    fn days(year: u16, month: u16) -> u16 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap(year) => 29,
            2 => 28,
            _ => 0,
        }
    }
    let (mut year, mut month, mut day, mut hour, mut minute) = (year, month, day, hour, minute + 1);
    if minute == 60 {
        minute = 0;
        hour += 1;
    }
    if hour == 24 {
        hour = 0;
        day += 1;
    }
    if day > days(year, month) {
        day = 1;
        month += 1;
    }
    if month == 13 {
        month = 1;
        year += 1;
    }
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[must_use]
pub fn task_xml(script_path: &str, start_boundary: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n  <Triggers><TimeTrigger><StartBoundary>{}</StartBoundary><Enabled>true</Enabled></TimeTrigger></Triggers>\r\n  <Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>\r\n  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><StartWhenAvailable>true</StartWhenAvailable><ExecutionTimeLimit>PT1H</ExecutionTimeLimit></Settings>\r\n  <Actions Context=\"Author\"><Exec><Command>{}</Command></Exec></Actions>\r\n</Task>\r\n",
        xml_escape(start_boundary),
        xml_escape(script_path),
    )
}

#[must_use]
pub fn task_xml_args(task_name: &str, xml_path: &str) -> Vec<String> {
    vec![
        "/create".to_string(),
        "/tn".to_string(),
        task_name.to_string(),
        "/xml".to_string(),
        xml_path.to_string(),
        "/f".to_string(),
    ]
}

/// The `schtasks /create` argument vector. `/tr` is a bare quoted script path —
/// no embedded command line — so `schtasks`' own quoting rules cannot bite.
#[must_use]
pub fn task_args(task_name: &str, script_path: &str, start_time: &str) -> Vec<String> {
    vec![
        "/create".to_string(),
        "/tn".to_string(),
        task_name.to_string(),
        "/sc".to_string(),
        "once".to_string(),
        "/st".to_string(),
        start_time.to_string(),
        "/f".to_string(),
        "/tr".to_string(),
        script_path.to_string(),
    ]
}

/// A one-shot task: a fixed name, and the batch file it runs.
///
/// The script is expected to remove the task and itself once its work is done
/// (`schtasks /delete` then `del "%~f0"`); [`delete_task`] is the inverse for
/// the case where it never got the chance.
pub struct OneShotTask {
    /// Task name, and the stem of the `.cmd`/`.xml` pair written into
    /// `%LOCALAPPDATA%\Temp`.
    /// Must satisfy [`is_safe_task_literal`].
    pub name: String,
    /// The batch file body, CRLF-terminated as `cmd.exe` expects.
    pub script: String,
}

impl OneShotTask {
    #[must_use]
    pub fn new(name: &str, script: String) -> Self {
        Self {
            name: name.to_string(),
            script,
        }
    }
}

/// The `.cmd` a task of this name runs, or `None` when `%LOCALAPPDATA%` is
/// unset or the name is not a safe literal.
#[cfg(windows)]
fn script_path(name: &str) -> Option<PathBuf> {
    if !is_safe_task_literal(name) {
        return None;
    }
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local_app_data)
            .join("Temp")
            .join(format!("{name}.cmd")),
    )
}

/// Writes the script, registers the task and fires it immediately.
///
/// `None` when anything at all refused — an unsafe name, no `%LOCALAPPDATA%`, an
/// unwritable directory, a missing or hardened Task Scheduler — so the caller
/// can fall back. The immediate `/run` is what makes this useful; the one-minute
/// trigger is only the backstop for a machine that stops first.
#[cfg(windows)]
#[must_use]
pub fn register_and_run(task: &OneShotTask) -> Option<()> {
    use std::os::windows::process::CommandExt;

    register(task)?;
    // Fire it now; the scheduled trigger is only the backstop.
    let started = Command::new("schtasks.exe")
        .args(["/run", "/tn", &task.name])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if started {
        Some(())
    } else {
        // Keep the one-minute trigger only if the immediate launch succeeded.
        // Otherwise an unowned updater could run after its caller gave up.
        let _ = delete_task(&task.name);
        None
    }
}

/// Writes the script and registers the task **without** running it: it fires at
/// the one-minute backstop trigger instead.
///
/// The self-updater needs this. Phase 1a of its install ladder registers the
/// relaunch watchdog before it calls the Store, and may then have to replace
/// that same script with a different one — which is only safe while no
/// `cmd.exe` has the file open. A minute is far longer than that decision takes.
#[cfg(windows)]
#[must_use]
pub fn register(task: &OneShotTask) -> Option<()> {
    register_after(task, 1)
}

/// Registers after a bounded number of minutes. The updater uses five minutes
/// for admission watchdogs because its WinRT calls can each take two minutes.
#[cfg(windows)]
#[must_use]
pub fn register_after(task: &OneShotTask, delay_minutes: u16) -> Option<()> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let script = script_path(&task.name)?;
    let xml = script.with_extension("xml");
    if let Some(parent) = script.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&script, &task.script).ok()?;

    // SAFETY: GetLocalTime only writes the SYSTEMTIME it is handed.
    let now = unsafe {
        let mut time = std::mem::zeroed();
        GetLocalTime(&raw mut time);
        time
    };
    let mut boundary = task_start_boundary(now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute);
    for _ in 1..delay_minutes {
        let bytes = boundary.as_bytes();
        let year = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
        let month = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
        let day = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
        let hour = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
        let minute = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
        boundary = task_start_boundary(year, month, day, hour, minute);
    }
    let definition = task_xml(&script.display().to_string(), &boundary);
    let mut utf16 = Vec::with_capacity(2 + definition.len() * 2);
    utf16.extend_from_slice(&[0xFF, 0xFE]);
    for unit in definition.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&xml, utf16).ok()?;
    let args = task_xml_args(&task.name, &xml.display().to_string());

    let created = Command::new("schtasks.exe")
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !created.success() {
        return None;
    }
    Some(())
}

/// Removes a task and the `.cmd` [`register_and_run`] wrote for it — the exact
/// inverse, for a task whose script never ran and so never deleted itself.
///
/// True only when `schtasks` reported the task gone; a task that was never there
/// is a failure by that measure, which is why callers treat this as best-effort.
#[cfg(windows)]
pub fn delete_task(name: &str) -> bool {
    use std::os::windows::process::CommandExt;

    if !is_safe_task_literal(name) {
        return false;
    }
    // Stop a script which Task Scheduler has already started before removing
    // its definition and releasing the updater's ownership lock.
    let _ = Command::new("schtasks.exe")
        .args(["/end", "/tn", name])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Some(script) = script_path(name) {
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(script.with_extension("xml"));
    }
    Command::new("schtasks.exe")
        .args(["/delete", "/tn", name, "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::{
        is_safe_task_literal, task_args, task_start_boundary, task_start_time, task_xml,
        task_xml_args,
    };

    #[test]
    fn safe_literals_are_names_versions_and_families_only() {
        assert!(is_safe_task_literal("fwdslash-update"));
        assert!(is_safe_task_literal("fwdslash-orphan-cleanup"));
        assert!(is_safe_task_literal("0.0.4.0"));
        // A package family name and an AppId, the two literals the update
        // watchdog splices into its script.
        assert!(is_safe_task_literal("32827MikeFara.fwdslash_t6j5qexy2jpp2"));
        assert!(is_safe_task_literal("App"));
    }

    #[test]
    fn every_shell_metacharacter_is_rejected() {
        // Empty is not "trivially safe": an empty /tn is a syntax error and an
        // empty splice silently changes what a script means.
        assert!(!is_safe_task_literal(""));
        // cmd.exe: expansion, redirection, chaining, escaping, grouping.
        for value in [
            "a b", "a%b", "a&b", "a|b", "a<b", "a>b", "a^b", "a(b", "a)b", "a;b", "a,b", "a=b",
            "a+b", "a@b", "a#b", "a*b", "a?b", "a[b", "a]b", "a{b", "a}b", "a~b", "a$b", "a'b",
            "a\"b", "a`b", "a\\b", "a/b", "a\tb", "a\nb", "a\rb",
        ] {
            assert!(!is_safe_task_literal(value), "accepted {value:?}");
        }
        // Non-ASCII never reaches a command line either: schtasks and cmd.exe
        // disagree about the console code page.
        assert!(!is_safe_task_literal("café"));
        assert!(!is_safe_task_literal("日本語"));
    }

    #[test]
    fn start_time_formats_and_wraps_at_midnight() {
        assert_eq!(task_start_time(9, 5), "09:06");
        assert_eq!(task_start_time(0, 0), "00:01");
        assert_eq!(task_start_time(13, 0), "13:01");
        assert_eq!(task_start_time(9, 59), "10:00");
        // Midnight wrap must stay a valid HH:MM rather than "24:00".
        assert_eq!(task_start_time(23, 59), "00:00");
    }

    #[test]
    fn start_boundary_carries_the_next_calendar_day() {
        assert_eq!(
            task_start_boundary(2026, 9, 5, 23, 59),
            "2026-09-06T00:00:00"
        );
        assert_eq!(
            task_start_boundary(2026, 1, 31, 23, 59),
            "2026-02-01T00:00:00"
        );
        assert_eq!(
            task_start_boundary(2026, 12, 31, 23, 59),
            "2027-01-01T00:00:00"
        );
        assert_eq!(
            task_start_boundary(2024, 2, 28, 23, 59),
            "2024-02-29T00:00:00"
        );
        assert_eq!(
            task_start_boundary(2025, 2, 28, 23, 59),
            "2025-03-01T00:00:00"
        );
    }

    #[test]
    fn task_xml_is_locale_independent_and_escapes_paths() {
        let xml = task_xml(r"C:\Users\a & b\Temp\x.cmd", "2027-01-01T00:00:00");
        assert!(xml.contains("<StartBoundary>2027-01-01T00:00:00</StartBoundary>"));
        assert!(xml.contains("C:\\Users\\a &amp; b\\Temp\\x.cmd"));
        assert!(!xml.contains("/st"));
        assert_eq!(
            task_xml_args("fwdslash-update", r"C:\Temp\fwdslash-update.xml"),
            vec![
                "/create",
                "/tn",
                "fwdslash-update",
                "/xml",
                r"C:\Temp\fwdslash-update.xml",
                "/f"
            ]
        );
    }

    #[test]
    fn task_args_pass_the_script_as_a_bare_path() {
        let args = task_args(
            "fwdslash-update",
            r"C:\Users\a b\AppData\Local\Temp\fwdslash-update.cmd",
            "09:06",
        );
        assert_eq!(
            args,
            vec![
                "/create",
                "/tn",
                "fwdslash-update",
                "/sc",
                "once",
                "/st",
                "09:06",
                "/f",
                "/tr",
                r"C:\Users\a b\AppData\Local\Temp\fwdslash-update.cmd",
            ]
        );
        // /f, so a second registration overwrites rather than accumulating.
        assert!(args.iter().any(|argument| argument == "/f"));
        // /tr carries no embedded command line: nothing to re-quote, nothing to
        // chain onto.
        assert!(!args.iter().any(|argument| argument.contains(" & ")));
        assert!(!args.iter().any(|argument| argument.contains('"')));
    }

    #[test]
    fn a_space_in_the_profile_name_survives_argument_passing() {
        // The script path is handed to schtasks as one argv element, so a
        // space in it never needs quoting by us -- the test that would have
        // caught quoting the path by hand.
        let args = task_args("t", r"C:\Users\a b\Temp\t.cmd", "00:01");
        assert!(args.contains(&r"C:\Users\a b\Temp\t.cmd".to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn xml_registration_runs_an_isolated_harmless_task() {
        use std::time::{Duration, Instant};

        let name = format!("fsw-task-test-{}", std::process::id());
        let marker = std::env::temp_dir().join(format!("{name}.marker"));
        let _ = std::fs::remove_file(&marker);
        let script = format!(
            "@echo off\r\necho scheduler-ok>\"{}\"\r\nschtasks /delete /tn \"{name}\" /f >nul 2>&1\r\ndel /q \"%~dpn0.xml\" >nul 2>&1\r\ndel /q \"%~f0\"\r\n",
            marker.display()
        );
        let task = super::OneShotTask::new(&name, script);
        assert!(super::register_and_run(&task).is_some());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = super::delete_task(&name);
        assert!(
            marker.is_file(),
            "Task Scheduler did not execute the XML task"
        );
        let _ = std::fs::remove_file(marker);
    }
}
