//! Unit tests for the pure half of `fwdslash update`.
//!
//! Everything with a Store, a network or a Task Scheduler behind it is
//! exercised by the manual matrix in `docs/compatibility.md`; what is tested
//! here is the part that decides *what happens* — the ladder, the state map,
//! the JSON shape, and above all the generated watchdog script, which is the
//! one artifact in this product that is executed by two different interpreters
//! and cannot be inspected once it has run.

// The workspace denies `expect` because `panic = "abort"` makes one in a window
// proc or a COM callback an instant process death. A test binary has neither,
// and a failed `expect` here is exactly the report wanted.
#![allow(clippy::expect_used, clippy::panic)]

use super::appinstall::{Poll, code_for, result_for, severity};
use super::install_control::AppInstallState;
use super::relaunch::{
    RelaunchMode, WATCHDOG_TASK_NAME, apply_script, watchdog_powershell, watchdog_script,
    winget_command_for,
};
use super::{
    Availability, EXIT_AVAILABLE, EXIT_ERROR, EXIT_NEEDS_USER, EXIT_NOTHING, EXIT_OK, Fold,
    HelperResult, InstallAnswer, Options, Precheck, Route, UpdateJson, Verb, fold_helper_result,
    install_answer, install_moment_ok, install_precheck, parse_args, parse_helper_result,
    render_json, route_for, state_for_code,
};
use crate::scheduled_task::is_safe_task_literal;
use fsw_core::update::Offer;
use std::path::Path;

const FAMILY: &str = "32827MikeFara.fwdslash_t6j5qexy2jpp2";
const IDENTITY: &str = "32827MikeFara.fwdslash";
const PREVIOUS: &str = "0.0.4.0";
const POWERSHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

// ---------------------------------------------------------------------------
// The ladder
// ---------------------------------------------------------------------------

#[test]
fn route_for_is_exhaustive_over_the_ladder() {
    // No override: the ladder in order, over all eight input rows.
    for silent in [false, true] {
        for winget in [false, true] {
            for metered in [false, true] {
                let expected = if silent {
                    Route::Store
                } else if winget && !metered {
                    Route::Winget
                } else {
                    Route::Notify
                };
                assert_eq!(
                    route_for(None, silent, winget, metered),
                    expected,
                    "silent={silent} winget={winget} metered={metered}"
                );
            }
        }
    }
}

#[test]
fn the_private_api_route_is_never_selected_automatically() {
    // Issue #98: `AppInstallManager` is documented as gated by a private
    // capability restricted to Microsoft's own apps, so nothing the product
    // decides on its own may reach it. It stays available as a hand-set
    // diagnostic escape hatch, and only that.
    for silent in [false, true] {
        for winget in [false, true] {
            for metered in [false, true] {
                assert_ne!(
                    route_for(None, silent, winget, metered),
                    Route::AppInstall,
                    "silent={silent} winget={winget} metered={metered}"
                );
            }
        }
    }
    // Reachable only by asking for it by name.
    assert_eq!(
        route_for(Some(Route::AppInstall), true, true, false),
        Route::AppInstall
    );
}

#[test]
fn a_metered_network_suppresses_only_winget() {
    // winget downloads regardless of the user's data settings, so it is the
    // one rung the cost probe can veto...
    assert_eq!(
        route_for(None, false, true, true),
        Route::Notify,
        "metered must not reach winget"
    );
    assert_eq!(route_for(None, false, true, false), Route::Winget);
    // ...and the rung above it is unaffected, because the Store makes its
    // own metered decision (`CanSilentlyDownloadStorePackageUpdates`).
    assert_eq!(route_for(None, true, false, true), Route::Store);
}

#[test]
fn an_override_wins_over_every_probe() {
    // The `UpdateRoute` escape hatch has to work when nothing is available,
    // including forcing a route that will then fail: that is what makes it
    // useful for diagnosis.
    for route in [
        Route::AppInstall,
        Route::Store,
        Route::Winget,
        Route::Notify,
    ] {
        assert_eq!(route_for(Some(route), false, false, true), route);
        assert_eq!(route_for(Some(route), true, true, false), route);
    }
}

#[test]
fn route_names_round_trip() {
    for route in [
        Route::AppInstall,
        Route::Store,
        Route::Winget,
        Route::Notify,
    ] {
        assert_eq!(Route::parse(route.name()), Some(Some(route)));
    }
    // `auto` is "no override", not an unknown name.
    assert_eq!(Route::parse("auto"), Some(None));
    assert_eq!(Route::parse("nonsense"), None);
    assert_eq!(Route::parse(""), None);
}

#[test]
fn nothing_to_install_outranks_the_moment_gate_and_the_route() {
    // The shipped bug: `update install --route notify` on an up-to-date Store
    // package with the settings window open answered `deferred`, exit 10 --
    // which tells the broker "there is an update, retry later" about an update
    // that does not exist, and it would have retried forever.
    //
    // `install_precheck` takes no route at all, which is the fix stated as a
    // type: a forced route says *how* to install, never *whether* there is
    // anything to.
    assert_eq!(install_precheck(false, false), Precheck::Nothing);
    assert_eq!(install_precheck(false, true), Precheck::Nothing);
    // With something to install, the moment gate decides.
    assert_eq!(install_precheck(true, false), Precheck::Defer);
    assert_eq!(install_precheck(true, true), Precheck::Proceed);
}

#[test]
fn the_precheck_verdicts_carry_the_exit_codes_the_broker_keys_on() {
    // 12 is "nothing to install" and is silent; 10 is "an update exists, come
    // back later"; 11 is "an update exists and needs the user". Confusing the
    // first two is what made an up-to-date install look like a pending one.
    assert_eq!(state_for_code(EXIT_NOTHING), "upToDate");
    assert_eq!(state_for_code(EXIT_AVAILABLE), "deferred");
    assert_eq!(state_for_code(EXIT_NEEDS_USER), "needsUser");
    assert_eq!(state_for_code(EXIT_OK), "installed");
    assert_eq!(state_for_code(EXIT_ERROR), "error");
    // An exit code no route produces is still an error, never a success.
    assert_eq!(state_for_code(99), "error");
}

#[test]
fn every_route_reports_nothing_to_install_the_same_way() {
    // The route is chosen only after the precheck says Proceed, so all four
    // rungs -- including a forced `--route notify`, which is the one that
    // reported 10 -- share the single `Nothing` verdict above.
    for route in [
        Route::AppInstall,
        Route::Store,
        Route::Winget,
        Route::Notify,
    ] {
        // `route_for` still answers, because picking a rung is a separate
        // question from whether one will ever be walked.
        assert_eq!(route_for(Some(route), false, false, true), route);
        // ...and the precheck that gates it never sees the route.
        assert_eq!(install_precheck(false, true), Precheck::Nothing);
    }
}

#[test]
fn install_moment_needs_an_idle_desktop_or_an_explicit_ask() {
    // forced wins over everything: the user pressed the button.
    assert!(install_moment_ok(true, true, true));
    assert!(install_moment_ok(true, false, false));
    // Unforced: both vetoes hold independently.
    assert!(install_moment_ok(false, false, false));
    assert!(!install_moment_ok(false, true, false));
    assert!(!install_moment_ok(false, false, true));
    assert!(!install_moment_ok(false, true, true));
}

// ---------------------------------------------------------------------------
// AppInstallState
// ---------------------------------------------------------------------------

#[test]
fn code_for_keeps_polling_through_every_progressing_state() {
    for state in [
        AppInstallState::Pending,
        AppInstallState::Starting,
        AppInstallState::AcquiringLicense,
        AppInstallState::Downloading,
        AppInstallState::RestoringData,
        AppInstallState::Installing,
        // The fourteenth value, added after the docs were written and the one
        // a naive match drops into the error arm.
        AppInstallState::ReadyToDownload,
    ] {
        assert_eq!(code_for(state), Poll::Continue, "state {}", state.0);
    }
}

#[test]
fn every_paused_state_is_retry_later_and_never_an_error() {
    // A pause is the Store deferring for battery or Wi-Fi; it resumes on its
    // own. Reporting it as a failure would make the broker balloon an error
    // the user cannot act on, and drop the update on the floor.
    for state in [
        AppInstallState::Paused,
        AppInstallState::PausedLowBattery,
        AppInstallState::PausedWiFiRecommended,
        AppInstallState::PausedWiFiRequired,
    ] {
        assert_eq!(
            code_for(state),
            Poll::Finished(EXIT_AVAILABLE),
            "state {}",
            state.0
        );
    }
}

#[test]
fn terminal_states_map_to_success_and_failure() {
    assert_eq!(
        code_for(AppInstallState::Completed),
        Poll::Finished(EXIT_OK)
    );
    assert_eq!(
        code_for(AppInstallState::Canceled),
        Poll::Finished(EXIT_ERROR)
    );
    assert_eq!(code_for(AppInstallState::Error), Poll::Finished(EXIT_ERROR));
    // Anything a future Windows adds is terminal, not a 45-minute poll.
    assert_eq!(
        code_for(AppInstallState(999)),
        Poll::Finished(EXIT_ERROR),
        "an unknown state must not spin"
    );
}

#[test]
fn queued_success_and_pauses_keep_the_watchdog_but_terminal_refusals_do_not() {
    assert!(super::appinstall_keeps_watchdog(EXIT_OK));
    assert!(super::appinstall_keeps_watchdog(EXIT_AVAILABLE));
    assert!(!super::appinstall_keeps_watchdog(EXIT_NOTHING));
    assert!(!super::appinstall_keeps_watchdog(EXIT_ERROR));
    assert!(super::store_keeps_watchdog(EXIT_OK));
    assert!(super::store_keeps_watchdog(EXIT_AVAILABLE));
    assert!(!super::store_keeps_watchdog(EXIT_NOTHING));
    assert!(!super::store_keeps_watchdog(EXIT_ERROR));
}

#[test]
fn all_fourteen_documented_states_are_classified() {
    // A guard against the enum growing a value the loop above forgets: every
    // value 0..=13 must have an opinion, and exactly one must be Completed.
    let completed = (0..14)
        .filter(|value| code_for(AppInstallState(*value)) == Poll::Finished(EXIT_OK))
        .count();
    assert_eq!(completed, 1);
    let progressing = (0..14)
        .filter(|value| code_for(AppInstallState(*value)) == Poll::Continue)
        .count();
    assert_eq!(progressing, 7);
}

#[test]
fn a_foreground_wait_hands_off_on_progress_or_at_the_admission_window() {
    use super::{Verdict, WaitPolicy, verdict};
    use std::time::Duration;
    let policy = WaitPolicy::Foreground {
        admission: Duration::from_secs(180),
    };
    assert_eq!(verdict(policy, Duration::ZERO, false), Verdict::Continue);
    assert_eq!(
        verdict(policy, Duration::from_secs(179), false),
        Verdict::Continue
    );
    assert_eq!(
        verdict(policy, Duration::from_secs(180), false),
        Verdict::HandOff
    );
    // The Store started moving: leave now, before it force-closes us.
    assert_eq!(
        verdict(policy, Duration::from_secs(1), true),
        Verdict::HandOff
    );
    // A foreground wait never reports a timeout: the item is the Store's now.
    assert_ne!(
        verdict(policy, Duration::from_hours(2), false),
        Verdict::TimedOut
    );
}

#[test]
fn a_background_wait_polls_to_the_ceiling_whatever_the_progress() {
    use super::{Verdict, WaitPolicy, verdict};
    use std::time::Duration;
    let policy = WaitPolicy::Background {
        ceiling: Duration::from_mins(45),
    };
    for progressed in [false, true] {
        assert_eq!(
            verdict(policy, Duration::from_mins(44), progressed),
            Verdict::Continue
        );
        assert_eq!(
            verdict(policy, Duration::from_mins(45), progressed),
            Verdict::TimedOut
        );
    }
}

#[test]
fn waiting_its_turn_is_not_progress_but_a_byte_is() {
    use super::appinstall::shows_progress;
    for state in [
        AppInstallState::Pending,
        AppInstallState::Starting,
        AppInstallState::AcquiringLicense,
        AppInstallState::ReadyToDownload,
    ] {
        assert!(!shows_progress(state, 0.0, 0), "state {}", state.0);
        assert!(shows_progress(state, 0.5, 0), "state {}", state.0);
        assert!(shows_progress(state, 0.0, 1), "state {}", state.0);
    }
    for state in [
        AppInstallState::Downloading,
        AppInstallState::RestoringData,
        AppInstallState::Installing,
        AppInstallState::Completed,
    ] {
        assert!(shows_progress(state, 0.0, 0), "state {}", state.0);
    }
}

#[test]
fn a_live_queue_item_is_adopted_and_a_dead_one_is_cleared() {
    use super::appinstall::{Existing, existing_item};
    for state in [
        AppInstallState::Pending,
        AppInstallState::Downloading,
        AppInstallState::Installing,
    ] {
        assert_eq!(existing_item(state), Existing::Adopt, "state {}", state.0);
    }
    for state in [
        AppInstallState::Completed,
        AppInstallState::Canceled,
        AppInstallState::Error,
        AppInstallState::Paused,
        AppInstallState::PausedWiFiRequired,
    ] {
        assert_eq!(existing_item(state), Existing::Clear, "state {}", state.0);
    }
}

#[test]
fn an_empty_answer_inside_the_scan_cooldown_keeps_the_cached_offer() {
    use super::{STORE_SCAN_COOLDOWN_SECS, keep_cached_offer};
    let now = 1_800_000_000;
    assert!(keep_cached_offer(Some(now - 5), now));
    assert!(keep_cached_offer(
        Some(now - STORE_SCAN_COOLDOWN_SECS + 1),
        now
    ));
    assert!(!keep_cached_offer(
        Some(now - STORE_SCAN_COOLDOWN_SECS),
        now
    ));
    assert!(!keep_cached_offer(None, now));
    // A clock that went backwards reads as "just checked", which errs on the
    // side of keeping the offer for one more cycle.
    assert!(keep_cached_offer(Some(now + 60), now));
}

#[test]
fn severity_orders_error_over_pause_over_success() {
    assert!(severity(EXIT_ERROR) > severity(EXIT_AVAILABLE));
    assert!(severity(EXIT_AVAILABLE) > severity(EXIT_OK));
    assert_eq!(severity(EXIT_NOTHING), severity(EXIT_OK));
}

#[test]
fn result_for_writes_the_word_the_folder_reads() {
    // Round trip through the file format: whatever the helper records must
    // parse back to the same verdict.
    for (code, error) in [
        (EXIT_OK, None),
        (EXIT_NOTHING, None),
        (EXIT_AVAILABLE, None),
        (EXIT_ERROR, Some("0x80070005")),
    ] {
        let result = result_for(code, error);
        let text = match &result {
            HelperResult::Completed => "completed".to_string(),
            HelperResult::Paused => "paused".to_string(),
            HelperResult::Error(code) => format!("error:{code}"),
        };
        assert_eq!(parse_helper_result(&text).as_ref(), Some(&result));
    }
}

// ---------------------------------------------------------------------------
// The result file
// ---------------------------------------------------------------------------

#[test]
fn helper_results_parse_and_fold() {
    assert_eq!(
        parse_helper_result("completed"),
        Some(HelperResult::Completed)
    );
    assert_eq!(parse_helper_result("paused"), Some(HelperResult::Paused));
    assert_eq!(
        parse_helper_result("error:0x80070005"),
        Some(HelperResult::Error("0x80070005".to_string()))
    );
    // The helper writes with no trailing newline, but a text editor or a
    // `>` redirect will add one; trimming is not optional.
    assert_eq!(
        parse_helper_result("  completed\r\n"),
        Some(HelperResult::Completed)
    );
    assert_eq!(
        parse_helper_result("ERROR: 0x8007000E"),
        Some(HelperResult::Error("0x8007000E".to_string()))
    );

    // Only a completed install proves the cached notice stale.
    assert_eq!(
        fold_helper_result(&HelperResult::Completed),
        Fold::ClearAvailable
    );
    assert_eq!(
        fold_helper_result(&HelperResult::Paused),
        Fold::KeepAvailable
    );
    assert_eq!(
        fold_helper_result(&HelperResult::Error("0x1".to_string())),
        Fold::KeepAvailable
    );
}

#[test]
fn a_truncated_or_foreign_result_file_is_no_verdict() {
    // Half a word, an empty file, someone else's file: none of these may be
    // mistaken for "the install finished".
    for text in ["", "   ", "complet", "error:", "error", "0x80070005", "ok"] {
        assert_eq!(parse_helper_result(text), None, "{text:?}");
    }
}

// ---------------------------------------------------------------------------
// Argument parsing and the helper argv
// ---------------------------------------------------------------------------

#[test]
fn verbs_and_flags_parse() {
    let options = parse_args(&argv(&["check", "--json", "--force"]));
    assert_eq!(
        options,
        Some(Options {
            verb: Verb::Check,
            json: true,
            force: true,
            relaunch: RelaunchMode::Broker,
            route: None,
            product: None,
            bundle: None,
            previous: None,
        })
    );

    let options = parse_args(&argv(&[
        "install",
        "--relaunch",
        "app",
        "--route",
        "store",
        "--json",
    ]));
    let options = options.expect("install with a route parses");
    assert_eq!(options.relaunch, RelaunchMode::App);
    assert_eq!(options.route, Some(Route::Store));
    assert!(options.json);
    assert!(!options.force);

    // Default relaunch is the broker: it is what the product is with no window
    // open, and the settings app is the only caller that wants the app back.
    let options = parse_args(&argv(&["install"])).expect("bare install parses");
    assert_eq!(options.relaunch, RelaunchMode::Broker);
}

#[test]
fn malformed_invocations_are_rejected_rather_than_defaulted() {
    // No verb, unknown verb, unknown flag, a valued flag with no value, and a
    // bad value for a valued flag. Each must be a usage error, because each
    // would otherwise silently do something other than what was asked.
    for arguments in [
        vec![],
        argv(&["nonsense"]),
        argv(&["check", "--nonsense"]),
        argv(&["install", "--relaunch"]),
        argv(&["install", "--relaunch", "sideways"]),
        argv(&["install", "--route"]),
        argv(&["install", "--route", "carrier-pigeon"]),
        argv(&["apply-store", "--product"]),
    ] {
        assert_eq!(parse_args(&arguments), None, "{arguments:?}");
    }
}

#[test]
fn the_helper_argv_round_trips() {
    // The command line the scheduled task hands the staged helper is built by
    // one function and parsed by another, in two different processes, with a
    // package shutdown in between. This is the only place the two meet.
    let command = super::helper::apply_store_command(
        std::path::Path::new(
            r"C:\Users\a b\AppData\Local\ForwardSlashWindows\update\fwdslash-helper.exe",
        ),
        "9P51CM0MTMK2",
        PREVIOUS,
    );
    let tail: Vec<String> = command
        .split(" update ")
        .nth(1)
        .expect("the command carries an update verb")
        .split(' ')
        .map(str::to_string)
        .collect();
    let options = parse_args(&tail).expect("the helper argv parses");
    assert_eq!(options.verb, Verb::ApplyStore);
    assert_eq!(options.product.as_deref(), Some("9P51CM0MTMK2"));
    assert_eq!(options.previous.as_deref(), Some(PREVIOUS));

    let command = super::helper::apply_bundle_command(
        std::path::Path::new(r"C:\u\fwdslash-helper.exe"),
        std::path::Path::new(r"C:\u\update\fwdslash-v0.0.5.msixbundle"),
        PREVIOUS,
    );
    assert!(command.contains("update apply-bundle --bundle"));
    // Both paths are quoted: either may contain a space.
    assert_eq!(command.matches('"').count(), 4);
}

#[test]
fn only_check_sweeps_the_updaters_leftovers() {
    // `status` is the read-only verb by contract: asking what the updater
    // thinks must never delete a scheduled task out from under a live
    // install. `collect_for` is the single call site of `gc::collect`, so
    // this table is the behaviour and not a description of it.
    assert!(Verb::Check.collects_garbage());
    for verb in [
        Verb::Status,
        Verb::Install,
        Verb::ApplyStore,
        Verb::ApplyBundle,
    ] {
        assert!(
            !verb.collects_garbage(),
            "{} must have no side effect on the updater's own files",
            verb.name()
        );
    }
}

#[test]
fn only_the_apply_verbs_are_helper_only() {
    assert!(Verb::ApplyStore.is_helper_only());
    assert!(Verb::ApplyBundle.is_helper_only());
    assert!(!Verb::Check.is_helper_only());
    assert!(!Verb::Install.is_helper_only());
    assert!(!Verb::Status.is_helper_only());
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

#[test]
fn json_golden_full_row() {
    let json = render_json(&UpdateJson {
        flavor: "store",
        state: "available",
        available: Some("0.0.5.0"),
        auto_update: true,
        last_check: Some(1_700_000_000),
        route: Some("appinstall"),
        action: Some("scheduled"),
        detail: Some("In-process install unavailable (0x80070005)."),
    });
    assert_eq!(
        json,
        r#"{"flavor":"store","state":"available","available":"0.0.5.0","autoUpdate":true,"lastUpdateCheck":1700000000,"route":"appinstall","action":"scheduled","detail":"In-process install unavailable (0x80070005)."}"#
    );
}

#[test]
fn json_golden_empty_row_uses_null_not_empty_string() {
    let json = render_json(&UpdateJson {
        flavor: "unpackaged",
        state: "disabled",
        available: None,
        auto_update: false,
        last_check: None,
        route: None,
        action: None,
        detail: None,
    });
    assert_eq!(
        json,
        r#"{"flavor":"unpackaged","state":"disabled","available":null,"autoUpdate":false,"lastUpdateCheck":null,"route":null,"action":null,"detail":null}"#
    );
}

#[test]
fn json_escapes_a_detail_that_carries_quotes() {
    // Details come from HRESULT formatting today, but an InfoBar renders them
    // verbatim, so the line must stay parseable whatever lands in there.
    let json = render_json(&UpdateJson {
        flavor: "github",
        state: "error",
        available: None,
        auto_update: true,
        last_check: Some(0),
        route: None,
        action: None,
        detail: Some("said \"no\"\\ and\nstopped"),
    });
    assert!(
        json.contains(r#""detail":"said \"no\"\\ and\nstopped""#),
        "{json}"
    );
}

// ---------------------------------------------------------------------------
// The watchdog script
// ---------------------------------------------------------------------------

/// The PowerShell text out of a generated script: everything after the
/// `-Command` on the one system PowerShell line.
pub(super) fn powershell_line(script: &str) -> Option<&str> {
    script
        .lines()
        .find(|line| line.contains(" -Command "))?
        .split_once(" -Command ")
        .map(|(_, text)| text)
}

pub(super) fn assert_batch_safe(text: &str) {
    // `%` would be expanded by cmd.exe as a variable, and `%` is legal inside
    // a PowerShell string, so the corruption would be silent.
    assert!(!text.contains('%'), "PowerShell text contains %: {text}");
    // A quote terminates the argument cmd.exe is building.
    assert!(
        !text.contains('"'),
        "PowerShell text contains a quote: {text}"
    );
    // The operators that would otherwise be redirection or chaining.
    for character in ['<', '>', '&', '|', '^'] {
        assert!(
            !text.contains(character),
            "PowerShell text contains {character}: {text}"
        );
    }
}

#[test]
fn watchdog_script_golden_broker() {
    let script = watchdog_script(
        Some(Path::new(POWERSHELL)),
        Some(Path::new(r"C:\Windows\System32\schtasks.exe")),
        FAMILY,
        IDENTITY,
        PREVIOUS,
        RelaunchMode::Broker,
    )
    .expect("safe literals");
    assert!(
        script.contains("$package.PackageFamilyName -eq '32827MikeFara.fwdslash_t6j5qexy2jpp2'")
    );
    assert!(script.contains("error:0x800705B4"));
    assert!(script.contains("del /q \"%~dpn0.xml\" >nul 2>&1"));
    assert!(
        script
            .contains("\"C:\\Windows\\System32\\schtasks.exe\" /delete /tn \"fwdslash-update\" /f")
    );
    assert!(script.starts_with("@echo off\r\n\"C:\\Windows\\System32\\WindowsPowerShell"));
    assert_batch_safe(powershell_line(&script).expect("a powershell line"));
    // The relaunch goes through the app-execution alias, never the App entry
    // point: the package's App is the settings window, and the broker is a
    // startup task that only fires at logon.
    assert!(script.contains("fwdslash.exe') -ArgumentList 'start'"));
    assert!(!script.contains("AppsFolder"));
    // ...and only when no broker is already running, so a double launch is
    // impossible rather than merely harmless.
    assert!(script.contains("if (-not (Get-Process -Name fswbroker"));
}

#[test]
fn watchdog_does_not_relaunch_an_unready_or_wrong_family_package() {
    let script =
        watchdog_powershell(FAMILY, IDENTITY, PREVIOUS, RelaunchMode::App).expect("safe literals");
    assert!(script.contains("-Name '32827MikeFara.fwdslash'"));
    assert!(
        script.contains("$package.PackageFamilyName -eq '32827MikeFara.fwdslash_t6j5qexy2jpp2'")
    );
    assert!(script.contains("if ($ready) { Start-Process"));
    assert!(script.contains(fsw_core::update::UPDATE_RESULT_FILE));
    assert!(script.contains("error:0x800705B4"));
    // The timeout notice never overwrites a verdict the helper already wrote:
    // a `paused` result must survive the watchdog giving up.
    assert!(script.contains("if (-not (Test-Path -LiteralPath $result))"));
}

#[cfg(windows)]
#[test]
fn generated_watchdog_powershell_runs_with_mocked_appx_commands() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    struct FixtureDirectory(std::path::PathBuf);
    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let fixture = FixtureDirectory(std::env::temp_dir().join(format!(
        "fsw-watchdog-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    )));
    std::fs::create_dir_all(&fixture.0).expect("fixture directory");
    let local_app_data = fixture.0.join("local-app-data");
    std::fs::create_dir_all(&local_app_data).expect("isolated local app data");
    let marker_path = fixture.0.join("started.txt");
    let marker = marker_path.to_string_lossy().replace('\'', "''");
    let watchdog =
        watchdog_powershell(FAMILY, IDENTITY, PREVIOUS, RelaunchMode::App).expect("safe literals");
    let harness = format!(
        "function Get-AppxPackage {{ param($Name) [pscustomobject]@{{ PackageFamilyName = '{FAMILY}'; Version = '0.0.5.0' }} }}; function Get-Process {{ param($Name) $null }}; function Start-Process {{ param($FilePath) Set-Content -LiteralPath '{marker}' -Value $FilePath }}; {watchdog}"
    );
    let powershell = fsw_core::SystemBinary::PowerShell
        .path()
        .expect("system Windows PowerShell");
    let mut child = Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-Command", &harness])
        .env("LOCALAPPDATA", &local_app_data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Windows PowerShell starts");
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("watchdog child state") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("mocked watchdog exceeded its five-second fixture deadline");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(status.success());
    assert!(marker_path.is_file());
    assert!(
        !local_app_data
            .join("ForwardSlashWindows")
            .join("update")
            .join(fsw_core::update::UPDATE_RESULT_FILE)
            .exists()
    );
}

#[test]
fn watchdog_script_golden_app() {
    let script = watchdog_script(
        Some(Path::new(POWERSHELL)),
        Some(Path::new(r"C:\Windows\System32\schtasks.exe")),
        FAMILY,
        IDENTITY,
        PREVIOUS,
        RelaunchMode::App,
    )
    .expect("safe literals");
    assert_batch_safe(powershell_line(&script).expect("a powershell line"));
    // The app mode is the settings window coming back, which is the App entry
    // point and therefore the AppsFolder moniker.
    assert!(script.contains(
        "Start-Process -FilePath 'shell:AppsFolder\\32827MikeFara.fwdslash_t6j5qexy2jpp2!App'"
    ));
    // Same wait as every other mode: the relaunch is the only difference.
    assert!(script.contains("while ((Get-Date) -lt $deadline)"));
    // ...and the same give-up: after the ceiling, the broker comes back
    // whatever the mode, because the old package is still the product.
    let (ready, timed_out) = script
        .split_once("} else {")
        .expect("a ready branch and a timeout branch");
    assert!(!ready.contains("Get-Process -Name fswbroker"));
    assert!(timed_out.contains("if (-not (Get-Process -Name fswbroker"));
    assert!(timed_out.contains("fwdslash.exe') -ArgumentList 'start'"));
    assert!(script.ends_with("del /q \"%~f0\"\r\n"));
}

#[test]
fn watchdog_script_golden_none() {
    // `none` still produces a script, because the task is what the caller was
    // going to register anyway -- but it carries no watchdog at all, so it
    // self-cleans immediately instead of holding a task for 45 minutes.
    let script = watchdog_script(
        Some(Path::new(POWERSHELL)),
        Some(Path::new(r"C:\Windows\System32\schtasks.exe")),
        FAMILY,
        IDENTITY,
        PREVIOUS,
        RelaunchMode::None,
    )
    .expect("safe literals");
    assert_eq!(
        script,
        "@echo off\r\n\
         \"C:\\Windows\\System32\\schtasks.exe\" /delete /tn \"fwdslash-update\" /f >nul 2>&1\r\n\
         del /q \"%~dpn0.xml\" >nul 2>&1\r\n\
         del /q \"%~f0\"\r\n"
    );
    assert!(powershell_line(&script).is_none());
    assert_eq!(
        watchdog_powershell(FAMILY, IDENTITY, PREVIOUS, RelaunchMode::None),
        None
    );
}

#[test]
fn the_apply_script_runs_the_lead_command_before_the_watchdog() {
    let command = r#""C:\u\fwdslash-helper.exe" update apply-store --product 9P51CM0MTMK2"#;
    let script = apply_script(
        command,
        Some(Path::new(POWERSHELL)),
        Some(Path::new(r"C:\Windows\System32\schtasks.exe")),
        FAMILY,
        IDENTITY,
        PREVIOUS,
        RelaunchMode::Broker,
    )
    .expect("safe literals");
    let lines: Vec<&str> = script.lines().collect();
    assert_eq!(lines.first().copied(), Some("@echo off"));
    assert_eq!(lines.get(1).copied(), Some(command));
    assert!(lines.get(2).is_some_and(|line| line.starts_with('"')));
    // The install and the comeback are one task, so a package shutdown cannot
    // land between them.
    assert!(
        script.contains("\"C:\\Windows\\System32\\schtasks.exe\" /delete /tn \"fwdslash-update\"")
    );
    assert_batch_safe(powershell_line(&script).expect("a powershell line"));
}

#[test]
fn an_unsafe_literal_produces_no_script_at_all() {
    // A refusal, never a mangled script: these values reach two interpreters.
    for (family, identity, previous) in [
        ("a b", IDENTITY, PREVIOUS),
        (FAMILY, "a&b", PREVIOUS),
        (FAMILY, IDENTITY, "0.0.4.0'; Remove-Item"),
        ("", IDENTITY, PREVIOUS),
        (FAMILY, "", PREVIOUS),
        (FAMILY, IDENTITY, ""),
    ] {
        assert_eq!(
            watchdog_script(
                Some(Path::new(POWERSHELL)),
                Some(Path::new(r"C:\Windows\System32\schtasks.exe")),
                family,
                identity,
                previous,
                RelaunchMode::Broker,
            ),
            None,
            "{family:?} {identity:?} {previous:?}"
        );
        assert_eq!(
            apply_script(
                "cmd",
                Some(Path::new(POWERSHELL)),
                Some(Path::new(r"C:\Windows\System32\schtasks.exe")),
                family,
                identity,
                previous,
                RelaunchMode::App,
            ),
            None
        );
    }
}

#[test]
fn an_unsafe_powershell_path_produces_no_script_at_all() {
    // The resolver supplies this path in production, but it still crosses a
    // batch parser. A metacharacter must refuse scheduling, not become part of
    // the command line.
    assert_eq!(
        watchdog_script(
            Some(Path::new(r"C:\Windows&evil\powershell.exe")),
            Some(Path::new(r"C:\Windows\System32\schtasks.exe")),
            FAMILY,
            IDENTITY,
            PREVIOUS,
            RelaunchMode::Broker,
        ),
        None
    );
}

#[test]
fn the_literals_the_watchdog_splices_are_safe_by_construction() {
    // The three values that reach the script, and the task name itself.
    assert!(is_safe_task_literal(FAMILY));
    assert!(is_safe_task_literal(IDENTITY));
    assert!(is_safe_task_literal(PREVIOUS));
    assert!(is_safe_task_literal(WATCHDOG_TASK_NAME));
    assert!(is_safe_task_literal(fsw_core::STORE_PACKAGE_FAMILY));
    assert!(is_safe_task_literal(fsw_core::STORE_IDENTITY_NAME));
    assert!(is_safe_task_literal(fsw_core::STORE_PRODUCT_ID));
}

#[test]
fn relaunch_modes_round_trip() {
    for mode in [RelaunchMode::App, RelaunchMode::Broker, RelaunchMode::None] {
        assert_eq!(RelaunchMode::parse(mode.name()), Some(mode));
    }
    assert_eq!(RelaunchMode::parse("Broker"), None, "case is exact");
    assert_eq!(RelaunchMode::parse(""), None);
}

#[test]
fn the_winget_command_answers_every_prompt_in_advance() {
    let command = winget_command_for(
        fsw_core::STORE_PRODUCT_ID,
        Path::new(r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\winget.exe"),
    )
    .unwrap_or_else(|| panic!("safe alias path"));
    assert_eq!(
        command,
        "\"C:\\Users\\me\\AppData\\Local\\Microsoft\\WindowsApps\\winget.exe\" upgrade --id 9P51CM0MTMK2 --source msstore --exact --silent --force \
         --accept-package-agreements --accept-source-agreements --disable-interactivity"
    );
    // It runs from a scheduled task with no console: anything that could ask a
    // question would hang until the 45-minute ceiling.
    for flag in [
        "--silent",
        "--disable-interactivity",
        "--accept-package-agreements",
        "--accept-source-agreements",
    ] {
        assert!(command.contains(flag), "missing {flag}");
    }
    // The executable path is the only quoted field; every remaining token is
    // a fixed literal and no batch chaining/expansion metacharacter survives.
    assert_eq!(command.matches('"').count(), 2);
    for character in ['%', '<', '>', '&', '|', '^'] {
        assert!(
            !command.contains(character),
            "batch command contains {character}"
        );
    }
}

// ---------------------------------------------------------------------------
// The install gate (issues #90 and #97)
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_availability_never_reaches_a_route() {
    // The property issue #90 is about, and the one a refactor is most likely
    // to break: a Store query that failed must not license an install, and
    // must not claim there is nothing to install either.
    for actionable in [false, true] {
        for moment_ok in [false, true] {
            assert_eq!(
                install_answer(&Availability::Unknown, actionable, moment_ok),
                InstallAnswer::Unknown,
                "actionable {actionable}, moment {moment_ok}"
            );
        }
    }
}

#[test]
fn nothing_to_install_still_outranks_the_moment() {
    for moment_ok in [false, true] {
        assert_eq!(
            install_answer(&Availability::Nothing, true, moment_ok),
            InstallAnswer::Nothing
        );
    }
}

#[test]
fn a_named_offer_is_never_subject_to_the_unnamed_backoff() {
    // Its version is the proof that something will change, so it is retried
    // as often as the caller likes.
    let named = Availability::Offer(Offer::Named("0.1.0.0".to_string()));
    for actionable in [false, true] {
        assert_eq!(
            install_answer(&named, actionable, true),
            InstallAnswer::Proceed
        );
        assert_eq!(
            install_answer(&named, actionable, false),
            InstallAnswer::Defer
        );
    }
}

#[test]
fn an_unnamed_offer_backs_off_before_the_moment_is_even_asked() {
    let unnamed = Availability::Offer(Offer::Unnamed);
    for moment_ok in [false, true] {
        assert_eq!(
            install_answer(&unnamed, false, moment_ok),
            InstallAnswer::BackedOff,
            "a spent unnamed offer must not retry every cycle"
        );
    }
    assert_eq!(install_answer(&unnamed, true, true), InstallAnswer::Proceed);
    assert_eq!(install_answer(&unnamed, true, false), InstallAnswer::Defer);
}

#[test]
fn the_availability_label_never_invents_a_version() {
    assert_eq!(
        Availability::Offer(Offer::Named("0.1.0.0".to_string())).label(),
        Some("0.1.0.0".to_string())
    );
    assert_eq!(Availability::Offer(Offer::Unnamed).label(), None);
    assert_eq!(Availability::Nothing.label(), None);
    assert_eq!(Availability::Unknown.label(), None);
}

#[test]
fn each_install_answer_carries_the_exit_code_its_state_names() {
    // Pins the state/exit pairs `cmd_install` emits, so the two cannot drift.
    // `Unknown` is 11 and not 12: twelve claims there is nothing to install,
    // which is the whole of issue #90. It is not 0 either — the settings
    // window reads exit 0 without `action: "queued"` as "about to be
    // force-closed", shows nothing and leaves the broker down.
    assert_eq!(state_for_code(EXIT_NOTHING), "upToDate");
    assert_eq!(state_for_code(EXIT_NEEDS_USER), "needsUser");
    assert_eq!(state_for_code(EXIT_AVAILABLE), "deferred");
    assert_ne!(EXIT_NEEDS_USER, EXIT_NOTHING);
    assert_ne!(EXIT_NEEDS_USER, EXIT_OK);
}
