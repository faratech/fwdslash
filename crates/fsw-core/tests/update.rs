//! Tests for the pure half of the update machinery: full-name parsing,
//! version comparison, release-JSON field extraction, and the check gate.
//! The `#[cfg(windows)]` HTTP/spawn layer is exercised by the packaged
//! verification matrix.

use fsw_core::update::{
    check_is_due, extract_bundle_url, extract_tag_name, is_newer_version, normalize_running_version,
    parse_version, update_check_allowed, UpdateOutcome,
};
use fsw_core::{
    package_family_from_full_name, package_version_from_full_name,
};

#[test]
fn family_from_full_name_handles_the_empty_resource_id() {
    // The shipped shape: no ResourceId means an empty group (double underscore).
    assert_eq!(
        package_family_from_full_name("32827MikeFara.fwdslash_0.0.2.0_x64__t6j5qexy2jpp2")
            .as_deref(),
        Some("32827MikeFara.fwdslash_t6j5qexy2jpp2")
    );
}

#[test]
fn family_ignores_an_underscore_in_the_identity_name() {
    assert_eq!(
        package_family_from_full_name("a.b_c_1.2.3.0_x64__h").as_deref(),
        Some("a.b_c_h")
    );
}

#[test]
fn family_rejects_short_or_malformed_names() {
    for full in ["", "x", "a_b_c", "32827MikeFara.fwdslash_notaversion_x64__h"] {
        assert_eq!(package_family_from_full_name(full), None, "{full:?}");
    }
}

#[test]
fn version_from_full_name_returns_the_four_part_version() {
    assert_eq!(
        package_version_from_full_name("32827MikeFara.fwdslash_0.0.2.0_arm64__t6j5qexy2jpp2")
            .as_deref(),
        Some("0.0.2.0")
    );
}

#[test]
fn parse_version_accepts_tag_shapes() {
    assert!(parse_version("v0.0.3").is_some());
    assert!(parse_version("0.1.10").is_some());
    assert!(parse_version("V1.0.0").is_some());
}

#[test]
fn parse_version_rejects_non_releases() {
    for text in ["v1.2", "v1.2.3-rc1", "v1.2.3.4", "v1.2.x", ""] {
        assert!(parse_version(text).is_none(), "{text:?} should not parse");
    }
}

#[test]
fn is_newer_version_compares_numerically() {
    assert!(!is_newer_version("0.0.2", "0.0.2"));
    assert!(!is_newer_version("0.0.3", "0.0.2"));
    assert!(is_newer_version("0.0.2", "0.0.3"));
    assert!(is_newer_version("0.0.9", "0.0.10"), "numeric, not lexical");
    assert!(is_newer_version("0.0.2", "0.1.0"));
    assert!(is_newer_version("0.0.2", "1.0.0"));
}

#[test]
fn normalize_running_version_drops_the_msix_fourth_group() {
    assert_eq!(normalize_running_version("0.0.2.0"), "0.0.2");
    assert_eq!(normalize_running_version("1.2.3.4"), "1.2.3");
    // Already three parts: unchanged.
    assert_eq!(normalize_running_version("0.0.2"), "0.0.2");
    // Anything else is left alone for `parse_version` to reject.
    assert_eq!(normalize_running_version("1.2"), "1.2");
    assert_eq!(normalize_running_version("1.2.3.4.5"), "1.2.3.4.5");
    assert_eq!(normalize_running_version(""), "");
    assert_eq!(normalize_running_version("not-a-version"), "not-a-version");
}

#[test]
fn the_packaged_four_part_version_can_see_a_release() {
    // The shipped bug: `package_version()` reports the four-part MSIX version,
    // `parse_version` rejects four groups, so every packaged GitHub install
    // answered `false` here and never updated.
    assert!(!is_newer_version("0.0.2.0", "v0.0.3"), "the raw shape never compares");

    assert!(is_newer_version(&normalize_running_version("0.0.2.0"), "v0.0.3"));
    assert!(!is_newer_version(&normalize_running_version("0.0.3.0"), "v0.0.3"));
    assert!(!is_newer_version(&normalize_running_version("0.0.3.0"), "v0.0.2"));
    assert!(is_newer_version(&normalize_running_version("0.0.9.0"), "v0.0.10"));
}

#[test]
fn is_newer_version_never_triggers_on_pre_release_or_garbage() {
    assert!(!is_newer_version("0.0.2", "0.0.3-rc1"));
    assert!(!is_newer_version("0.0.2", "not-a-version"));
    assert!(!is_newer_version("also-garbage", "0.0.3"));
}

// The asset list a release actually carries. The unsigned Store submission
// bundle is deliberately listed FIRST: the resolver scans in document order,
// and the GitHub API does not promise which asset comes back first.
const RELEASE_JSON: &str = r#"{
  "tag_name": "v0.0.3",
  "assets": [
    { "name": "fwdslash-0.0.3.0-arm64.msix", "browser_download_url": "https://github.com/faratech/fwdslash/releases/download/v0.0.3/fwdslash-0.0.3.0-arm64.msix" },
    { "name": "fwdslash-0.0.3.0-store-unsigned.msixbundle", "browser_download_url": "https://github.com/faratech/fwdslash/releases/download/v0.0.3/fwdslash-0.0.3.0-store-unsigned.msixbundle" },
    { "name": "fwdslash-0.0.3.0.msixbundle", "browser_download_url": "https://github.com/faratech/fwdslash/releases/download/v0.0.3/fwdslash-0.0.3.0.msixbundle" },
    { "name": "forward-slash-windows-0.0.3-arm64.zip", "browser_download_url": "https://github.com/faratech/fwdslash/releases/download/v0.0.3/forward-slash-windows-0.0.3-arm64.zip" }
  ]
}"#;

#[test]
fn extract_tag_name_reads_compact_and_spaced_json() {
    assert_eq!(extract_tag_name(r#"{"tag_name":"v0.0.3"}"#), Some("v0.0.3"));
    assert_eq!(
        extract_tag_name(r#"{ "tag_name": "v0.0.3" }"#),
        Some("v0.0.3")
    );
    assert_eq!(extract_tag_name(r#"{"other":1}"#), None);
}

#[test]
fn extract_tag_name_takes_the_first_occurrence() {
    // A release body may quote other tags; tag_name precedes it.
    let json = r#"{"tag_name":"v0.0.3","body":"fixes a regression introduced in v0.0.2"}"#;
    assert_eq!(extract_tag_name(json), Some("v0.0.3"));
}

#[test]
fn extract_bundle_url_picks_the_msixbundle() {
    let url = extract_bundle_url(RELEASE_JSON);
    assert_eq!(
        url,
        Some("https://github.com/faratech/fwdslash/releases/download/v0.0.3/fwdslash-0.0.3.0.msixbundle")
    );
}

#[test]
fn extract_bundle_url_ignores_zips() {
    let json = r#"{"browser_download_url":"https://example.com/tool.zip"}"#;
    assert_eq!(extract_bundle_url(json), None);
}

#[test]
fn extract_bundle_url_skips_the_unsigned_store_bundle() {
    // Every release since 0.0.4 carries the Microsoft Store submission
    // artifact alongside the signed one. It has the Partner Center identity
    // and no signature at all, so Add-AppxPackage would reject it — the
    // updater must never pick it, even when it is the only bundle present.
    let json = r#"{"browser_download_url":"https://example.com/fwdslash-0.0.4.0-store-unsigned.msixbundle"}"#;
    assert_eq!(extract_bundle_url(json), None);
}

#[test]
fn check_is_due_respects_the_daily_cadence() {
    assert!(check_is_due(None, 1_000));
    assert!(check_is_due(Some(1_000), 1_000 + fsw_core::update::CHECK_CADENCE_SECS));
    assert!(!check_is_due(Some(1_000), 1_000 + fsw_core::update::CHECK_CADENCE_SECS - 1));
}

#[test]
fn update_check_allowed_truth_table() {
    // GitHub flavor, packaged, auto-update on.
    assert!(update_check_allowed(true, false, true));
    // Never: unpackaged.
    assert!(!update_check_allowed(false, false, true));
    // Never: the Store flavor.
    assert!(!update_check_allowed(true, true, true));
    // Never: auto-update switched off.
    assert!(!update_check_allowed(true, false, false));
}

#[test]
fn update_outcome_defaults_to_silence() {
    // The NotDue/Unavailable outcomes must exist so failures never surface
    // user-visible errors from a background check.
    let outcomes = [
        UpdateOutcome::NotDue,
        UpdateOutcome::Unavailable,
        UpdateOutcome::UpToDate,
        UpdateOutcome::Ready("v0.0.3".to_string()),
    ];
    assert_eq!(outcomes.len(), 4);
}
