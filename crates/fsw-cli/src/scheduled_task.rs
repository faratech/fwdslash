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
//! * one fixed task name per purpose, created with `/f`, so repeated runs
//!   overwrite one task instead of accumulating one per run;
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
    /// Task name, and the stem of the `.cmd` written into `%LOCALAPPDATA%\Temp`.
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
    let _ = Command::new("schtasks.exe")
        .args(["/run", "/tn", &task.name])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Some(())
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
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let script = script_path(&task.name)?;
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
    let start = task_start_time(u32::from(now.wHour), u32::from(now.wMinute));
    let args = task_args(&task.name, &script.display().to_string(), &start);

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
    if let Some(script) = script_path(name) {
        let _ = std::fs::remove_file(script);
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
    use super::{is_safe_task_literal, task_args, task_start_time};

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
}
