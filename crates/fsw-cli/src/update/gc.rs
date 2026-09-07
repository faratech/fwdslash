//! Garbage collection for what an update attempt leaves behind.
//!
//! Every attempt registers a one-shot task, writes its `.cmd`/`.xml` sidecars
//! into `%LOCALAPPDATA%\Temp` and takes the attempt lock; the script deletes
//! all of that when it finishes. An attempt that is killed between
//! registering and running, a task cancelled by the scheduler, or a build old
//! enough to have used the fixed `fwdslash-update` name, leaves them all
//! behind — and the leftovers used to stay until uninstall (issue #140).
//!
//! The rule is age, not status. Every task's XML carries a one-hour
//! `ExecutionTimeLimit` and its trigger is at most five minutes out, so an
//! attempt whose sidecar is older than [`STALE_AFTER`] cannot still be a live
//! install, whatever the scheduler's (localized) status column says. A task
//! with no sidecar at all can never run usefully and is removed at once.

use super::relaunch::WATCHDOG_TASK_NAME;
use crate::scheduled_task::is_safe_task_literal;
use std::time::Duration;

/// Older than this, a task and its sidecars are leftovers: the scheduler's
/// one-hour limit plus the longest trigger delay, with a margin.
pub const STALE_AFTER: Duration = Duration::from_mins(70);

/// A scheduler task from a current or legacy updater attempt. Matching the
/// complete generated grammar, rather than a broad prefix, keeps the sweep
/// from touching another product's task with a similar name.
#[must_use]
pub fn is_owned_update_task_name(name: &str) -> bool {
    let name = name.strip_prefix('\\').unwrap_or(name);
    if name == WATCHDOG_TASK_NAME {
        return true;
    }
    if !is_safe_task_literal(name) {
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
#[must_use]
pub fn owned_update_task_inventory(csv: &str) -> Vec<String> {
    csv.lines()
        .filter_map(|line| {
            let field = line.trim().strip_prefix('"')?;
            let name = field.split('"').next()?;
            is_owned_update_task_name(name).then(|| name.trim_start_matches('\\').to_string())
        })
        .collect()
}

/// Whether a task whose sidecar has this age (or none) is a leftover.
#[must_use]
pub fn is_stale(sidecar_age: Option<Duration>, stale_after: Duration) -> bool {
    sidecar_age.is_none_or(|age| age > stale_after)
}

/// What one sweep removed. Diagnostic only; the sweep never fails a verb.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Collected {
    pub tasks: usize,
    pub files: usize,
    pub lock: bool,
}

/// The names of every owned update task the scheduler currently lists.
#[cfg(windows)]
#[must_use]
pub fn registered_update_tasks() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    fsw_core::SystemBinary::Schtasks
        .path()
        .and_then(|schtasks| {
            Command::new(schtasks)
                .args(["/query", "/fo", "csv", "/nh"])
                .creation_flags(crate::scheduled_task::CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()
        })
        .map_or_else(Vec::new, |output| {
            owned_update_task_inventory(&String::from_utf8_lossy(&output.stdout))
        })
}

fn file_age(path: &std::path::Path) -> Option<Duration> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
}

/// Removes every owned task, sidecar and lock token old enough to be a
/// leftover. Best effort throughout: a sweep that cannot query the scheduler
/// simply removes nothing.
#[cfg(windows)]
pub fn collect() -> Collected {
    let mut collected = Collected::default();
    for name in registered_update_tasks() {
        let age = crate::scheduled_task::script_path(&name)
            .as_deref()
            .and_then(file_age);
        if is_stale(age, STALE_AFTER) && crate::scheduled_task::delete_task(&name) {
            collected.tasks += 1;
        }
    }
    // Sidecars whose task is already gone — a task that self-deleted but
    // could not remove its own files, or one the scheduler dropped.
    if let Some(temp) =
        std::env::var_os("LOCALAPPDATA").map(|dir| std::path::PathBuf::from(dir).join("Temp"))
        && let Ok(entries) = std::fs::read_dir(temp)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let stem = file_name
                .strip_suffix(".cmd")
                .or_else(|| file_name.strip_suffix(".xml"));
            if stem.is_some_and(is_owned_update_task_name)
                && is_stale(file_age(&path), STALE_AFTER)
                && std::fs::remove_file(&path).is_ok()
            {
                collected.files += 1;
            }
        }
    }
    collected.lock = super::relaunch::reclaim_stale_attempt_lock();
    collected
}

#[cfg(not(windows))]
pub fn collect() -> Collected {
    Collected::default()
}

#[cfg(test)]
mod tests {
    use super::{STALE_AFTER, is_owned_update_task_name, is_stale, owned_update_task_inventory};
    use std::time::Duration;

    #[test]
    fn only_the_generated_grammar_and_the_legacy_name_are_owned() {
        for name in [
            "fwdslash-update",
            "\\fwdslash-update",
            "fwdslash-update-watchdog-18784-1",
            "fwdslash-update-apply-4-12",
        ] {
            assert!(is_owned_update_task_name(name), "{name}");
        }
        for name in [
            "fwdslash-update-",
            "fwdslash-updates",
            "fwdslash-update-watchdog",
            "fwdslash-update-watchdog-a-1",
            "fwdslash-update-repair-1-1",
            "fwdslash-update-watchdog-1-1-1",
            "OtherProduct-update",
        ] {
            assert!(!is_owned_update_task_name(name), "{name}");
        }
    }

    #[test]
    fn the_inventory_reads_only_the_first_csv_field() {
        let csv = "\"\\fwdslash-update\",\"N/A\",\"Ready\"\r\n\
                   \"\\Microsoft\\Windows\\Something\",\"N/A\",\"Ready\"\r\n\
                   \"\\fwdslash-update-apply-77-3\",\"9/5/2026 7:59:00 PM\",\"Running\"\r\n";
        assert_eq!(
            owned_update_task_inventory(csv),
            vec![
                "fwdslash-update".to_string(),
                "fwdslash-update-apply-77-3".to_string()
            ]
        );
    }

    #[test]
    fn a_missing_or_old_sidecar_is_stale_and_a_fresh_one_is_not() {
        assert!(is_stale(None, STALE_AFTER));
        assert!(is_stale(
            Some(STALE_AFTER + Duration::from_secs(1)),
            STALE_AFTER
        ));
        assert!(!is_stale(Some(STALE_AFTER), STALE_AFTER));
        assert!(!is_stale(Some(Duration::from_secs(30)), STALE_AFTER));
        // Well past the scheduler's one-hour execution limit plus the
        // five-minute trigger delay, so a stale task is never a live one.
        assert!(STALE_AFTER > Duration::from_mins(65));
    }
}
