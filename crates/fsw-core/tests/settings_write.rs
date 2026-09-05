//! The pure half of the settings writer (issue #52): which hives a write has
//! to reach, how `reg query` output parses, and which values a sync mirrors.
//!
//! The registry itself is `#[cfg(windows)]`; everything pinned here is
//! platform-independent decision-making, so the whole file runs on Linux.

use fsw_core::settings_write::{
    RawSetting, SettingValue, WritePlan, parse_reg_number, parse_reg_query, sync_plan, write_plan,
};
use fsw_core::{
    FSW_BARE_SLASH_DISTRIBUTION_VALUE, FSW_BARE_SLASH_MODE_VALUE, FSW_DISABLED_VALUE,
    sync_settings_to_real_hive,
};

// ---------------------------------------------------------------------------
// write_plan
// ---------------------------------------------------------------------------

#[test]
fn packaged_writes_both_hives() {
    // The package hive too, or its stale copy shadows the real one for every
    // packaged reader and the split just flips direction.
    assert_eq!(write_plan(true), WritePlan::Both);
}

#[test]
fn unpackaged_writes_the_real_hive_only() {
    // Without package identity the in-process API *is* the real hive; a
    // reg.exe child would only write the same value twice.
    assert_eq!(write_plan(false), WritePlan::RealOnly);
}

#[test]
fn write_plan_is_decided_by_identity_alone() {
    assert_ne!(write_plan(true), write_plan(false));
}

// ---------------------------------------------------------------------------
// reg query parsing
// ---------------------------------------------------------------------------

fn raw(kind: &str, data: &str) -> RawSetting {
    RawSetting {
        kind: kind.to_string(),
        data: data.to_string(),
    }
}

/// Verbatim from the reporting machine, `reg query
/// HKCU\Software\ForwardSlashWindows\Settings` (issue #52).
const REAL_HIVE_SAMPLE: &str = "\r\nHKEY_CURRENT_USER\\Software\\ForwardSlashWindows\\Settings\r\n    Disabled    REG_DWORD    0x0\r\n\r\n";

#[test]
fn parses_the_measured_real_hive_output() {
    let values = parse_reg_query(REAL_HIVE_SAMPLE);
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].0, FSW_DISABLED_VALUE);
    assert_eq!(values[0].1, raw("REG_DWORD", "0x0"));
}

#[test]
fn parses_every_type_this_key_uses() {
    let output = "\r\nHKEY_CURRENT_USER\\Software\\ForwardSlashWindows\\Settings\r\n\
        \x20   Disabled    REG_DWORD    0x1\r\n\
        \x20   BareSlashMode    REG_DWORD    0x1\r\n\
        \x20   BareSlashDistribution    REG_SZ    Ubuntu\r\n\
        \x20   LastUpdateCheck    REG_QWORD    0x68b8c0f1\r\n";
    let values = parse_reg_query(output);
    assert_eq!(values.len(), 4);
    assert_eq!(values[2].0, FSW_BARE_SLASH_DISTRIBUTION_VALUE);
    assert_eq!(values[2].1, raw("REG_SZ", "Ubuntu"));
    assert_eq!(values[3].1, raw("REG_QWORD", "0x68b8c0f1"));
}

#[test]
fn a_string_value_keeps_its_spaces_and_quotes() {
    let output = "    BareSlashRoot    REG_SZ    C:\\Program Files\\My \"Code\"\r\n";
    let values = parse_reg_query(output);
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].1.data, "C:\\Program Files\\My \"Code\"");
}

#[test]
fn a_value_name_with_spaces_still_splits_on_the_type() {
    let output = "    Bare Slash Root    REG_SZ    C:\\code\r\n";
    let values = parse_reg_query(output);
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].0, "Bare Slash Root");
    assert_eq!(values[0].1.data, "C:\\code");
}

#[test]
fn empty_data_parses_as_an_empty_string() {
    let values = parse_reg_query("    AvailableUpdate    REG_SZ    \r\n");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].1, raw("REG_SZ", ""));
}

#[test]
fn a_missing_key_parses_as_nothing() {
    // What reg.exe prints (on stderr, and nothing on stdout) for a key that
    // is not there; the parser must not invent a value from the error text.
    let output = "ERROR: The system was unable to find the specified registry key or value.\r\n";
    assert!(parse_reg_query(output).is_empty());
    assert!(parse_reg_query("").is_empty());
}

#[test]
fn key_and_subkey_lines_are_not_values() {
    let output = "\r\nHKEY_CURRENT_USER\\Software\\ForwardSlashWindows\\Settings\r\n\
        HKEY_CURRENT_USER\\Software\\ForwardSlashWindows\\Settings\\Sub\r\n";
    assert!(parse_reg_query(output).is_empty());
}

#[test]
fn foreign_types_on_the_key_are_parsed_not_skipped() {
    // A REG_EXPAND_SZ left by something else must compare as a mismatch, not
    // disappear and read as "the real hive has nothing".
    let values = parse_reg_query("    BareSlashRoot    REG_EXPAND_SZ    %USERPROFILE%\\code\r\n");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].1.kind, "REG_EXPAND_SZ");
}

#[test]
fn numbers_parse_from_hex_and_decimal() {
    assert_eq!(parse_reg_number("0x1"), Some(1));
    assert_eq!(parse_reg_number("0x0"), Some(0));
    assert_eq!(parse_reg_number("0X1f"), Some(31));
    assert_eq!(parse_reg_number("42"), Some(42));
    assert_eq!(parse_reg_number(" 0x2 "), Some(2));
    assert_eq!(parse_reg_number("nonsense"), None);
    assert_eq!(parse_reg_number(""), None);
}

// ---------------------------------------------------------------------------
// value comparison
// ---------------------------------------------------------------------------

#[test]
fn a_dword_matches_its_hex_rendering() {
    assert!(SettingValue::Dword(1).matches_raw(&raw("REG_DWORD", "0x1")));
    assert!(SettingValue::Dword(0).matches_raw(&raw("REG_DWORD", "0x0")));
    assert!(!SettingValue::Dword(1).matches_raw(&raw("REG_DWORD", "0x0")));
    // Same number, wrong type: still a mismatch, so the sync rewrites it.
    assert!(!SettingValue::Dword(1).matches_raw(&raw("REG_SZ", "1")));
}

#[test]
fn a_qword_matches_its_hex_rendering() {
    assert!(SettingValue::Qword(0x68b8_c0f1).matches_raw(&raw("REG_QWORD", "0x68b8c0f1")));
    assert!(!SettingValue::Qword(0x68b8_c0f1).matches_raw(&raw("REG_DWORD", "0x68b8c0f1")));
}

#[test]
fn a_string_matches_exactly() {
    assert!(SettingValue::Sz("Ubuntu".into()).matches_raw(&raw("REG_SZ", "Ubuntu")));
    // Registry *names* are case-insensitive; the data is not.
    assert!(!SettingValue::Sz("Ubuntu".into()).matches_raw(&raw("REG_SZ", "ubuntu")));
    assert!(!SettingValue::Sz("Ubuntu".into()).matches_raw(&raw("REG_SZ", "Debian")));
}

#[test]
fn reg_arguments_render_numbers_in_decimal() {
    assert_eq!(SettingValue::Dword(1).reg_type(), "REG_DWORD");
    assert_eq!(SettingValue::Dword(1).reg_data(), "1");
    assert_eq!(SettingValue::Qword(9).reg_type(), "REG_QWORD");
    assert_eq!(SettingValue::Qword(9).reg_data(), "9");
    assert_eq!(SettingValue::Sz("Ubuntu".into()).reg_type(), "REG_SZ");
    assert_eq!(SettingValue::Sz("Ubuntu".into()).reg_data(), "Ubuntu");
}

// ---------------------------------------------------------------------------
// sync decision table
// ---------------------------------------------------------------------------

#[test]
fn the_reported_split_mirrors_both_bare_slash_values() {
    // Exactly the measured machine: the merged view says "default
    // distribution, Ubuntu" and the real hive holds only Disabled.
    let merged = vec![
        (FSW_DISABLED_VALUE, SettingValue::Dword(0)),
        (FSW_BARE_SLASH_MODE_VALUE, SettingValue::Dword(1)),
        (
            FSW_BARE_SLASH_DISTRIBUTION_VALUE,
            SettingValue::Sz("Ubuntu".into()),
        ),
    ];
    let real = parse_reg_query(REAL_HIVE_SAMPLE);
    assert_eq!(
        sync_plan(&merged, &real),
        vec![
            FSW_BARE_SLASH_MODE_VALUE,
            FSW_BARE_SLASH_DISTRIBUTION_VALUE
        ]
    );
}

#[test]
fn agreeing_hives_need_no_writes() {
    let merged = vec![
        (FSW_DISABLED_VALUE, SettingValue::Dword(0)),
        (FSW_BARE_SLASH_MODE_VALUE, SettingValue::Dword(1)),
    ];
    let real = parse_reg_query(
        "    Disabled    REG_DWORD    0x0\r\n    BareSlashMode    REG_DWORD    0x1\r\n",
    );
    assert!(sync_plan(&merged, &real).is_empty());
}

#[test]
fn a_differing_value_is_rewritten() {
    let merged = vec![(FSW_BARE_SLASH_MODE_VALUE, SettingValue::Dword(0))];
    let real = parse_reg_query("    BareSlashMode    REG_DWORD    0x1\r\n");
    assert_eq!(sync_plan(&merged, &real), vec![FSW_BARE_SLASH_MODE_VALUE]);
}

#[test]
fn value_names_compare_case_insensitively() {
    // The registry's own rule: `bareslashmode` is the same value.
    let merged = vec![(FSW_BARE_SLASH_MODE_VALUE, SettingValue::Dword(1))];
    let real = parse_reg_query("    bareslashmode    REG_DWORD    0x1\r\n");
    assert!(sync_plan(&merged, &real).is_empty());
}

#[test]
fn a_real_hive_only_value_is_never_deleted() {
    // The sync mirrors; it never prunes. A value the packaged app does not
    // hold (an unpackaged fwdslash wrote it) stays exactly where it is.
    let merged: Vec<(&str, SettingValue)> = vec![(FSW_DISABLED_VALUE, SettingValue::Dword(0))];
    let real = parse_reg_query(
        "    Disabled    REG_DWORD    0x0\r\n    BareSlashRoot    REG_SZ    C:\\code\r\n",
    );
    assert!(sync_plan(&merged, &real).is_empty());
}

#[test]
fn an_empty_real_hive_mirrors_everything() {
    let merged = vec![
        (FSW_DISABLED_VALUE, SettingValue::Dword(1)),
        (FSW_BARE_SLASH_MODE_VALUE, SettingValue::Dword(1)),
    ];
    assert_eq!(
        sync_plan(&merged, &[]),
        vec![FSW_DISABLED_VALUE, FSW_BARE_SLASH_MODE_VALUE]
    );
}

#[test]
fn nothing_merged_means_nothing_to_do() {
    let real = parse_reg_query(REAL_HIVE_SAMPLE);
    assert!(sync_plan(&[], &real).is_empty());
}

// ---------------------------------------------------------------------------
// The cross-platform contract of the entry points.
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
#[test]
fn the_writers_are_no_ops_without_a_registry() {
    use fsw_core::{delete_setting, set_setting_string, set_setting_u32, set_setting_u64};

    assert!(set_setting_u32(FSW_BARE_SLASH_MODE_VALUE, 1).is_ok());
    assert!(set_setting_u64("LastUpdateCheck", 7).is_ok());
    assert!(set_setting_string(FSW_BARE_SLASH_DISTRIBUTION_VALUE, "Ubuntu").is_ok());
    assert!(delete_setting(FSW_BARE_SLASH_DISTRIBUTION_VALUE).is_ok());
}

#[test]
fn syncing_without_package_identity_does_nothing() {
    // Unpackaged there is one hive and nothing to mirror — and this test
    // binary never has package identity, on any platform.
    assert!(!sync_settings_to_real_hive());
}
