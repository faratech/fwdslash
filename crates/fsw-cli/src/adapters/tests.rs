//! Tests for the pure adapter decision and byte-format logic (state.rs,
//! profile.rs). The Win32 I/O in cmd.rs/powershell.rs is exercised by the
//! manual verification matrix; everything decided here is what that I/O
//! feeds on.

use super::profile;
use super::state;

#[test]
fn empty_originals_remain_present_but_orphan_hooks_do_not() {
    assert!(profile::original_profile_present(true, b"", b""));
    assert!(!profile::original_profile_present(false, b"", b""));
    assert!(!profile::original_profile_present(true, b"old block", b""));
    assert!(profile::original_profile_present(true, b"user", b"user"));
    assert!(!profile::should_delete_profile(0, true));
    assert!(state::original_autorun_present(true, "", ""));
    assert!(!state::original_autorun_present(false, "", ""));
    assert!(!state::original_autorun_present(true, "call old-hook", ""));
    assert!(state::original_autorun_present(true, "%SystemRoot%", "%SystemRoot%"));
}

#[cfg(windows)]
#[test]
fn orphan_cleanup_preserves_updater_and_unrelated_data() {
    let root = std::env::temp_dir().join(format!("fsw-prune-{}", super::new_transaction_id()));
    for child in ["cmd", "PowerShell", ".cmd-staging-test", "update", "unrelated"] {
        std::fs::create_dir_all(root.join(child)).unwrap();
        std::fs::write(root.join(child).join("sentinel"), b"keep").unwrap();
    }
    super::prune_adapter_directories(&root);
    for child in ["cmd", "PowerShell", ".cmd-staging-test"] {
        assert!(!root.join(child).exists());
    }
    for child in ["update", "unrelated"] {
        assert_eq!(std::fs::read(root.join(child).join("sentinel")).unwrap(), b"keep");
    }
    std::fs::remove_dir_all(root).unwrap();
}

// ---------------------------------------------------------------------------
// state.rs — marker classification and decisions
// ---------------------------------------------------------------------------

#[test]
fn classify_knows_the_three_transaction_states() {
    assert_eq!(state::classify("prepared"), state::MarkerState::Prepared);
    assert_eq!(state::classify("installed"), state::MarkerState::Installed);
    assert_eq!(state::classify("removing"), state::MarkerState::Removing);
}

#[test]
fn classify_rejects_everything_else() {
    for text in ["", "Installed", "INSTALL", "garbage"] {
        assert_eq!(state::classify(text), state::MarkerState::Unknown, "{text:?}");
    }
}

#[test]
fn cmd_install_refuses_installed_and_recovers_interrupted() {
    assert_eq!(
        state::decide_cmd_install(false, state::MarkerState::Unknown),
        state::InstallDecision::Proceed
    );
    assert_eq!(
        state::decide_cmd_install(true, state::MarkerState::Installed),
        state::InstallDecision::AlreadyInstalled
    );
    for marker_state in [state::MarkerState::Prepared, state::MarkerState::Removing] {
        assert_eq!(
            state::decide_cmd_install(true, marker_state),
            state::InstallDecision::RecoverRequired,
            "{marker_state:?}"
        );
    }
}

#[test]
fn ps_install_treats_installed_as_a_friendly_noop() {
    assert_eq!(
        state::decide_ps_install(true, state::MarkerState::Installed),
        state::InstallDecision::AlreadyInstalled
    );
    assert_eq!(
        state::decide_ps_install(false, state::MarkerState::Unknown),
        state::InstallDecision::Proceed
    );
    assert_eq!(
        state::decide_ps_install(true, state::MarkerState::Prepared),
        state::InstallDecision::RecoverRequired
    );
}

#[test]
fn cmd_uninstall_not_installed_is_a_success_noop() {
    assert_eq!(
        state::decide_cmd_uninstall(
            false,
            state::MarkerState::Unknown,
            state::AutorunVerdict::Changed
        ),
        state::UninstallDecision::NotInstalled
    );
}

#[test]
fn cmd_uninstall_refuses_unknown_state_and_foreign_autorun() {
    assert_eq!(
        state::decide_cmd_uninstall(
            true,
            state::MarkerState::Unknown,
            state::AutorunVerdict::Matches
        ),
        state::UninstallDecision::UnknownState
    );
    assert_eq!(
        state::decide_cmd_uninstall(
            true,
            state::MarkerState::Installed,
            state::AutorunVerdict::Changed
        ),
        state::UninstallDecision::AutoRunChanged
    );
}

#[test]
fn cmd_uninstall_proceeds_when_autorun_matches_either_snapshot() {
    assert_eq!(
        state::decide_cmd_uninstall(
            true,
            state::MarkerState::Installed,
            state::AutorunVerdict::Matches
        ),
        state::UninstallDecision::Proceed
    );
}

#[test]
fn ps_uninstall_refuses_unknown_state() {
    assert_eq!(
        state::decide_ps_uninstall(true, state::MarkerState::Unknown),
        state::UninstallDecision::UnknownState
    );
    for marker_state in [
        state::MarkerState::Prepared,
        state::MarkerState::Installed,
        state::MarkerState::Removing,
    ] {
        assert_eq!(
            state::decide_ps_uninstall(true, marker_state),
            state::UninstallDecision::Proceed,
            "{marker_state:?}"
        );
    }
}

#[test]
fn installed_autorun_appends_verbatim_with_ampersand() {
    let marker = "call \"C:\\fsw\\fsw-autorun.cmd\"";
    assert_eq!(
        state::installed_autorun("", marker),
        marker,
        "empty original keeps only the marker"
    );
    assert_eq!(
        state::installed_autorun("   ", marker),
        marker,
        "whitespace-only original counts as empty"
    );
    assert_eq!(
        state::installed_autorun("echo hi", marker),
        "echo hi & call \"C:\\fsw\\fsw-autorun.cmd\""
    );
    // Trailing space is preserved verbatim — the restore must be byte-exact.
    assert_eq!(
        state::installed_autorun("echo hi ", marker),
        "echo hi  & call \"C:\\fsw\\fsw-autorun.cmd\""
    );
}

#[test]
fn judge_autorun_accepts_installed_or_original() {
    let installed = "call \"x\"";
    let original = "echo hi";
    assert_eq!(
        state::judge_autorun(false, "", installed, original),
        state::AutorunVerdict::Matches,
        "absent value matches (nothing to restore)"
    );
    assert_eq!(
        state::judge_autorun(true, installed, installed, original),
        state::AutorunVerdict::Matches
    );
    assert_eq!(
        state::judge_autorun(true, original, installed, original),
        state::AutorunVerdict::Matches
    );
    assert_eq!(
        state::judge_autorun(true, "something else", installed, original),
        state::AutorunVerdict::Changed
    );
}

#[test]
fn judge_autorun_tolerates_the_0_0_2_one_character_truncation() {
    // 0.0.2 truncation compatibility rule: `installed` may be `current` plus
    // exactly one trailing character, because that is the damage 0.0.2's
    // byte-first NUL strip did to the value it read back.
    let installed = "call \"C:\\fsw\\fsw-autorun.cmd\"";
    let truncated = "call \"C:\\fsw\\fsw-autorun.cmd";
    let original = "echo hi";
    assert_eq!(
        state::judge_autorun(true, truncated, installed, original),
        state::AutorunVerdict::Matches,
        "one missing trailing character is the known 0.0.2 damage"
    );
    // And no further. Two characters short is a real third-party edit.
    assert_eq!(
        state::judge_autorun(true, "call \"C:\\fsw\\fsw-autorun.cm", installed, original),
        state::AutorunVerdict::Changed
    );
    // A one-character difference that is not a prefix is still Changed.
    assert_eq!(
        state::judge_autorun(true, "call \"C:\\fsw\\fsw-autorun.cmdX", installed, original),
        state::AutorunVerdict::Changed
    );
    // An empty current value never counts as a truncation of anything.
    assert_eq!(
        state::judge_autorun(true, "", "x", original),
        state::AutorunVerdict::Changed
    );
    // The tolerance is one-sided: `original` is still compared exactly.
    assert_eq!(
        state::judge_autorun(true, "echo h", installed, original),
        state::AutorunVerdict::Changed
    );
}

#[test]
fn other_edition_swaps() {
    assert_eq!(
        state::other_edition(state::Edition::WindowsPowerShell),
        state::Edition::PowerShell
    );
    assert_eq!(
        state::other_edition(state::Edition::PowerShell),
        state::Edition::WindowsPowerShell
    );
}

#[test]
fn shared_module_removed_unless_the_other_edition_pins_the_same_version() {
    assert!(
        state::remove_shared_module(None, "0.0.3"),
        "the other edition has no marker: this directory is the last reference"
    );
    assert!(
        !state::remove_shared_module(Some("0.0.3"), "0.0.3"),
        "the other edition still loads this exact directory"
    );
    assert!(
        state::remove_shared_module(Some("0.0.2"), "0.0.3"),
        "the other edition names a different directory, so it cannot pin this one"
    );
    // The 0.0.2 bug in one line: keying on marker presence alone stranded
    // every version directory whenever both editions were installed.
    assert!(state::remove_shared_module(Some("0.0.1"), "0.0.2"));
}

// ---------------------------------------------------------------------------
// reg.rs — REG_SZ / REG_EXPAND_SZ decoding (the 0.0.2 truncation)
// ---------------------------------------------------------------------------

/// A `RegGetValueW` buffer: UTF-16LE plus `terminators` trailing NUL units.
#[cfg(windows)]
fn reg_bytes(text: &str, terminators: usize) -> Vec<u8> {
    let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    for _ in 0..terminators {
        bytes.extend_from_slice(&[0x00, 0x00]);
    }
    bytes
}

#[cfg(windows)]
#[test]
fn decode_reg_string_keeps_a_trailing_ascii_character() {
    // The exact shape that blocked the upgrade: 0.0.2 popped the zero high
    // byte of the closing quote and then dropped the orphaned low byte.
    let value = r#"call "C:\Users\me\AppData\Local\ForwardSlashWindows\cmd\fsw-autorun.cmd""#;
    assert_eq!(super::reg::decode_reg_string(&reg_bytes(value, 1)), value);
    assert!(super::reg::decode_reg_string(&reg_bytes(value, 1)).ends_with('"'));
}

#[cfg(windows)]
#[test]
fn decode_reg_string_handles_every_terminator_count() {
    for terminators in [0, 1, 2, 3] {
        assert_eq!(
            super::reg::decode_reg_string(&reg_bytes("echo hi", terminators)),
            "echo hi",
            "{terminators} terminator(s)"
        );
    }
}

#[cfg(windows)]
#[test]
fn decode_reg_string_decodes_an_empty_value() {
    assert_eq!(super::reg::decode_reg_string(&[]), "");
    assert_eq!(super::reg::decode_reg_string(&[0x00, 0x00]), "");
    assert_eq!(super::reg::decode_reg_string(&[0x00, 0x00, 0x00, 0x00]), "");
}

#[cfg(windows)]
#[test]
fn decode_reg_string_preserves_a_non_ascii_last_character() {
    // U+00E9 is E9 00 in UTF-16LE — the same zero high byte, same 0.0.2 loss.
    assert_eq!(super::reg::decode_reg_string(&reg_bytes("caf\u{e9}", 1)), "caf\u{e9}");
    // A code unit with a non-zero high byte was never affected; still exact.
    assert_eq!(super::reg::decode_reg_string(&reg_bytes("\u{65e5}", 1)), "\u{65e5}");
}

#[cfg(windows)]
#[test]
fn decode_reg_string_leaves_expand_sz_references_verbatim() {
    // RRF_NOEXPAND data: the %VAR% must survive into the AutoRun snapshot.
    let value = r"%SystemRoot%\System32\x.cmd";
    assert_eq!(super::reg::decode_reg_string(&reg_bytes(value, 1)), value);
}

#[cfg(windows)]
#[test]
fn decode_reg_string_ignores_a_trailing_odd_byte() {
    // A malformed size can leave half a code unit; drop it rather than pad it.
    let mut bytes = reg_bytes("hi", 0);
    bytes.push(0x41);
    assert_eq!(super::reg::decode_reg_string(&bytes), "hi");
}

// ---------------------------------------------------------------------------
// profile.rs — byte formats
// ---------------------------------------------------------------------------

#[test]
fn detect_encoding_reads_every_bom() {
    assert_eq!(profile::detect_encoding(&[0x00, 0x00, 0xFE, 0xFF]), profile::ProfileEncoding::Utf32Be);
    assert_eq!(profile::detect_encoding(&[0xFF, 0xFE, 0x00, 0x00]), profile::ProfileEncoding::Utf32Le);
    assert_eq!(profile::detect_encoding(&[0xFE, 0xFF]), profile::ProfileEncoding::Utf16Be);
    assert_eq!(profile::detect_encoding(&[0xFF, 0xFE]), profile::ProfileEncoding::Utf16Le);
    assert_eq!(profile::detect_encoding(b"plain"), profile::ProfileEncoding::Utf8);
    assert_eq!(profile::detect_encoding(&[]), profile::ProfileEncoding::Utf8);
    // Preserved quirk: a UTF-16LE payload whose next unit starts with 0x00
    // matches the UTF-32LE probe first.
    assert_eq!(
        profile::detect_encoding(&[0xFF, 0xFE, 0x00, 0x00, 0x41, 0x00]),
        profile::ProfileEncoding::Utf32Le
    );
}

#[test]
fn encode_never_prepends_a_bom() {
    // UTF-16LE of "abc" must start with 'a' (0x61 0x00), not a BOM.
    assert_eq!(profile::encode("abc", profile::ProfileEncoding::Utf16Le), vec![0x61, 0x00, 0x62, 0x00, 0x63, 0x00]);
    assert_eq!(profile::encode("abc", profile::ProfileEncoding::Utf8), b"abc".to_vec());
    assert_eq!(
        profile::encode("a", profile::ProfileEncoding::Utf16Be),
        vec![0x00, 0x61]
    );
    assert_eq!(
        profile::encode("a", profile::ProfileEncoding::Utf32Le),
        vec![0x61, 0x00, 0x00, 0x00]
    );
}

#[test]
fn encode_round_trips_through_utf16() {
    let text = "dir /etc ← 日本語";
    let bytes = profile::encode(text, profile::ProfileEncoding::Utf16Le);
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    assert_eq!(String::from_utf16(&units).ok().as_deref(), Some(text));
}

/// A guarded block for the tests: the fixed probe/controller keep the layout
/// assertions focused on the parts #37 changed.
fn ps_block(version: &str, id: &str, module: &str, original_non_empty: bool) -> String {
    profile::block_text(&profile::BlockParams {
        version,
        transaction_id: id,
        module_path: module,
        probe_path: "C:\\Local\\Packages\\Fam_abc",
        alias_path: "C:\\Local\\Microsoft\\WindowsApps\\fwdslash.exe",
        controller_path: "C:\\ctl\\fwdslash.exe",
        original_non_empty,
    })
}

#[test]
fn block_text_renders_the_guarded_form() {
    // The payload version follows the crate version, so the layout assertions
    // are built from PAYLOAD_VERSION rather than a literal that goes stale
    // at every release.
    let version = super::PAYLOAD_VERSION;
    let module = format!(
        r"C:\Users\me\AppData\Local\ForwardSlashWindows\PowerShell\{version}\ForwardSlashWindows.psm1"
    );
    let block = ps_block(version, "cafe", &module, false);
    assert!(block.starts_with(&format!(
        "# >>> Forward Slash Windows {version} cafe >>>\r\n"
    )));
    assert!(block.contains(&format!("$m = '{module}'\r\n")));
    // The import is guarded by Test-Path, never a bare Import-Module (#37); the
    // product counts as present when EITHER the probe folder or the
    // app-execution alias is there, so switching the alias off in Settings
    // cannot look like an uninstall; and the self-clean is launched detached so
    // a shell never blocks on it.
    assert!(block.contains("$p = 'C:\\Local\\Packages\\Fam_abc'\r\n"));
    assert!(block.contains("$a = 'C:\\Local\\Microsoft\\WindowsApps\\fwdslash.exe'\r\n"));
    // `$a -and` guards the alias: an unresolvable %LOCALAPPDATA% renders it
    // empty, and a bare `Test-Path -LiteralPath ''` throws on every shell start.
    assert!(block.contains(
        "if ((Test-Path -LiteralPath $p) -or ($a -and (Test-Path -LiteralPath $a))) { if (Test-Path -LiteralPath $m) { Import-Module -Name $m -Global -Force } } elseif (Test-Path -LiteralPath $c) { Start-Process -FilePath $c -ArgumentList 'uninstall','--orphaned' -WindowStyle Hidden -ErrorAction SilentlyContinue }\r\n"
    ));
    assert!(
        !block.contains("Import-Module -Name 'C:"),
        "no unguarded literal-path import"
    );
    assert!(block.ends_with(&format!(
        "# <<< Forward Slash Windows {version} cafe <<<\r\n"
    )));
    // No blank-line prefix when the original was empty.
    assert!(!block.starts_with("\r\n"));
}

#[test]
fn block_text_escapes_quotes_and_prefixes_nonempty_originals() {
    let block = ps_block(super::PAYLOAD_VERSION, "t", "C:\\it's", true);
    assert!(block.starts_with("\r\n"));
    assert!(block.contains("$m = 'C:\\it''s'\r\n"));
}

#[test]
fn strip_restores_the_true_original_utf8() {
    let original = b"Write-Host hi\r\n".to_vec();
    let block = ps_block(super::PAYLOAD_VERSION, "id1", "C:\\m.psm1", true);
    let mut installed = original.clone();
    installed.extend_from_slice(block.as_bytes());
    assert_eq!(profile::strip_fwdslash_blocks(&installed), original);
}

#[test]
fn strip_restores_empty_when_the_original_was_absent() {
    let block = ps_block(super::PAYLOAD_VERSION, "id1", "C:\\m.psm1", false);
    assert!(profile::strip_fwdslash_blocks(block.as_bytes()).is_empty());
}

#[test]
fn strip_removes_every_block_on_a_multi_version_upgrade() {
    // The append-not-replace bug (#37): a profile carrying an old block and a
    // new one must strip back to the genuine original, and both are detected.
    let original = b"# my profile\r\n".to_vec();
    let old = ps_block("0.0.1", "old", "C:\\old\\m.psm1", true);
    let new = ps_block("0.0.3", "new", "C:\\new\\m.psm1", true);
    let mut polluted = original.clone();
    polluted.extend_from_slice(old.as_bytes());
    polluted.extend_from_slice(new.as_bytes());
    assert_eq!(profile::strip_fwdslash_blocks(&polluted), original);
    assert_eq!(profile::parse_blocks(&polluted).len(), 2);
}

#[test]
fn strip_leaves_a_fenceless_profile_byte_exact() {
    let original = b"Set-StrictMode -Version 2\r\nWrite-Host hi\r\n";
    assert_eq!(profile::strip_fwdslash_blocks(original), original.to_vec());
}

#[test]
fn strip_restores_the_true_original_utf16le_with_bom() {
    let text = "Write-Host hi\r\n";
    let block = ps_block(super::PAYLOAD_VERSION, "id", "C:\\m.psm1", true);
    let mut installed = vec![0xFF, 0xFE];
    installed.extend_from_slice(&profile::encode(text, profile::ProfileEncoding::Utf16Le));
    installed.extend_from_slice(&profile::encode(&block, profile::ProfileEncoding::Utf16Le));
    let mut expected = vec![0xFF, 0xFE];
    expected.extend_from_slice(&profile::encode(text, profile::ProfileEncoding::Utf16Le));
    assert_eq!(profile::strip_fwdslash_blocks(&installed), expected);
}

#[test]
fn parse_blocks_extracts_version_and_module() {
    let block = ps_block("0.0.1", "abc123", "C:\\FSW\\PowerShell\\0.0.1\\ForwardSlashWindows.psm1", true);
    let blocks = profile::parse_blocks(block.as_bytes());
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].version, "0.0.1");
    assert_eq!(blocks[0].transaction_id, "abc123");
    assert_eq!(
        blocks[0].module_path.as_deref(),
        Some("C:\\FSW\\PowerShell\\0.0.1\\ForwardSlashWindows.psm1")
    );
}

#[test]
fn parse_and_strip_understand_the_old_one_line_import_format() {
    // Pre-#37 blocks carried a bare `Import-Module -Name '…'` line; the parser
    // still finds the module and the strip still removes them cleanly.
    let legacy = "# >>> Forward Slash Windows 0.0.1 old >>>\r\nImport-Module -Name 'C:\\old\\ForwardSlashWindows.psm1' -Global -Force\r\n# <<< Forward Slash Windows 0.0.1 old <<<\r\n";
    let blocks = profile::parse_blocks(legacy.as_bytes());
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        blocks[0].module_path.as_deref(),
        Some("C:\\old\\ForwardSlashWindows.psm1")
    );
    assert!(profile::strip_fwdslash_blocks(legacy.as_bytes()).is_empty());
}

#[test]
fn classify_profile_ranks_orphan_over_duplicate_and_stale() {
    use profile::{BlockPresence, ProfileHealth};
    let present = BlockPresence {
        version: super::PAYLOAD_VERSION.to_string(),
        module_present: true,
    };
    let missing = BlockPresence {
        version: "0.0.1".to_string(),
        module_present: false,
    };
    let stale = BlockPresence {
        version: "0.0.1".to_string(),
        module_present: true,
    };
    assert_eq!(
        profile::classify_profile(&[], super::PAYLOAD_VERSION),
        ProfileHealth::Clean
    );
    assert_eq!(
        profile::classify_profile(std::slice::from_ref(&present), super::PAYLOAD_VERSION),
        ProfileHealth::Healthy
    );
    assert_eq!(
        profile::classify_profile(&[present.clone(), missing], super::PAYLOAD_VERSION),
        ProfileHealth::Orphaned("0.0.1".to_string())
    );
    assert_eq!(
        profile::classify_profile(&[present.clone(), present.clone()], super::PAYLOAD_VERSION),
        ProfileHealth::Duplicated
    );
    assert_eq!(
        profile::classify_profile(std::slice::from_ref(&stale), super::PAYLOAD_VERSION),
        ProfileHealth::Stale("0.0.1".to_string())
    );
}

#[test]
fn decide_profile_repair_covers_the_matrix() {
    use profile::{ProfileAction, ProfileHealth};
    // Healthy: nothing when installed; a dangling leftover to remove otherwise.
    assert_eq!(
        profile::decide_profile_repair(&ProfileHealth::Healthy, true, true),
        ProfileAction::Nothing
    );
    assert_eq!(
        profile::decide_profile_repair(&ProfileHealth::Healthy, false, true),
        ProfileAction::RemoveBlocks
    );
    // Orphan: write one current block when the module is there, reinstall when
    // it is missing, strip entirely when the marker is gone.
    let orphan = ProfileHealth::Orphaned("x".to_string());
    assert_eq!(
        profile::decide_profile_repair(&orphan, true, true),
        ProfileAction::WriteCurrentBlock
    );
    assert_eq!(
        profile::decide_profile_repair(&orphan, true, false),
        ProfileAction::Reinstall
    );
    assert_eq!(
        profile::decide_profile_repair(&orphan, false, true),
        ProfileAction::RemoveBlocks
    );
    assert_eq!(
        profile::decide_profile_repair(&ProfileHealth::Clean, false, false),
        ProfileAction::Nothing
    );
}

// ---------------------------------------------------------------------------
// state.rs — cmd AutoRun own-hook strip + product-presence probe (#37)
// ---------------------------------------------------------------------------

#[test]
fn strip_fwdslash_autorun_recovers_the_true_third_party_value() {
    let hook =
        "call \"C:\\Users\\me\\AppData\\Local\\ForwardSlashWindows\\cmd\\fsw-autorun.cmd\"";
    // The MSIX-leftover case: an AutoRun that is purely our own hook.
    assert_eq!(state::strip_fwdslash_autorun(hook), "");
    assert_eq!(state::strip_fwdslash_autorun(&format!("echo hi & {hook}")), "echo hi");
    assert_eq!(state::strip_fwdslash_autorun(&format!("{hook} & echo hi")), "echo hi");
    // A doubled install (`call fsw & call fsw`) strips to nothing.
    assert_eq!(state::strip_fwdslash_autorun(&format!("{hook} & {hook}")), "");
    // A third-party value that itself contains ` & ` survives intact.
    assert_eq!(
        state::strip_fwdslash_autorun(&format!("echo a & echo b & {hook}")),
        "echo a & echo b"
    );
    assert_eq!(state::strip_fwdslash_autorun("echo hi"), "echo hi");
}

#[test]
fn a_refused_uninstall_still_strips_only_our_own_autorun_segment() {
    // The bug this closes (#37): the transactional uninstall REFUSES when a
    // third party edited AutoRun after we installed, which used to leave our
    // `call "…fsw-autorun.cmd"` pointing at a script the self-clean then
    // deleted — a "system cannot find the path specified" on every cmd start.
    let hook =
        "call \"C:\\Users\\me\\AppData\\Local\\ForwardSlashWindows\\cmd\\fsw-autorun.cmd\"";
    let original = "echo hi";
    let installed = state::installed_autorun(original, hook);
    let tampered = format!("{installed} & echo later");

    // The refusal still stands...
    assert_eq!(
        state::judge_autorun(true, &tampered, &installed, original),
        state::AutorunVerdict::Changed
    );
    // ...but stripping removes only our segment and keeps both third-party
    // parts byte-for-byte, leaving nothing that references the deleted script.
    let stripped = state::strip_fwdslash_autorun(&tampered);
    assert_eq!(stripped, "echo hi & echo later");
    assert!(!state::autorun_references_fwdslash(&stripped));
    // Idempotent: stripping an already-clean value changes nothing.
    assert_eq!(state::strip_fwdslash_autorun(&stripped), stripped);
}

#[test]
fn autorun_reference_and_path_helpers() {
    let hook = "call \"C:\\x\\ForwardSlashWindows\\cmd\\fsw-autorun.cmd\"";
    assert!(state::autorun_references_fwdslash(&format!("echo hi & {hook}")));
    assert!(!state::autorun_references_fwdslash("echo hi"));
    assert_eq!(
        state::fwdslash_autorun_path(hook).as_deref(),
        Some("C:\\x\\ForwardSlashWindows\\cmd\\fsw-autorun.cmd")
    );
    assert_eq!(state::fwdslash_autorun_path("echo hi"), None);
}

#[test]
fn product_confirmed_gone_requires_both_checks_absent() {
    assert!(state::product_confirmed_gone(false, false));
    assert!(!state::product_confirmed_gone(true, false));
    assert!(!state::product_confirmed_gone(false, true));
    assert!(!state::product_confirmed_gone(true, true));
}

// ---------------------------------------------------------------------------
// mod.rs — deferred payload delete + blocked-write classification (#37)
// ---------------------------------------------------------------------------

#[test]
fn task_start_time_formats_and_wraps_at_midnight() {
    assert_eq!(super::task_start_time(9, 5), "09:06");
    assert_eq!(super::task_start_time(0, 0), "00:01");
    // Two-digit padding on both fields.
    assert_eq!(super::task_start_time(13, 0), "13:01");
    // Minute rollover into the next hour.
    assert_eq!(super::task_start_time(9, 59), "10:00");
    // Midnight wrap must stay a valid HH:MM rather than "24:00".
    assert_eq!(super::task_start_time(23, 59), "00:00");
}

#[test]
fn cleanup_script_quotes_every_path_and_self_destructs() {
    let dir = r"C:\Users\a b\AppData\Local\ForwardSlashWindows";
    let body = super::cleanup_script_body(dir, super::CLEANUP_TASK_NAME);
    // The payload path is quoted, so a space in the profile name is safe.
    assert!(!body.contains(&format!("rd /s /q \"{dir}\"\r\n")));
    assert!(body.contains(&format!("rd /s /q \"{dir}\\cmd\"\r\n")));
    assert!(body.contains(&format!("rd /s /q \"{dir}\\PowerShell\"\r\n")));
    // It waits for the launching process to exit before deleting.
    assert!(body.contains("ping -n 3 127.0.0.1 >nul\r\n"));
    // ...then removes the task and itself, so nothing accumulates.
    assert!(body.contains(&format!(
        "schtasks /delete /tn \"{}\" /f",
        super::CLEANUP_TASK_NAME
    )));
    assert!(body.contains("del /q \"%~f0\""));
    assert!(body.starts_with("@echo off\r\n"));
}

#[test]
fn cleanup_task_args_pass_the_script_as_a_bare_quoted_path() {
    let args = super::cleanup_task_args(
        super::CLEANUP_TASK_NAME,
        r"C:\Users\a b\AppData\Local\Temp\fwdslash-orphan-cleanup.cmd",
        "09:06",
    );
    assert_eq!(
        args,
        vec![
            "/create",
            "/tn",
            "fwdslash-orphan-cleanup",
            "/sc",
            "once",
            "/st",
            "09:06",
            "/f",
            "/tr",
            r"C:\Users\a b\AppData\Local\Temp\fwdslash-orphan-cleanup.cmd",
        ]
    );
    // A fixed task name, so repeated self-cleans overwrite one task under /f
    // rather than accumulating one per run.
    assert!(!super::CLEANUP_TASK_NAME.is_empty());
    // /tr carries no embedded command line, so schtasks' quoting cannot bite.
    assert!(!args.iter().any(|argument| argument.contains(" & ")));
}

#[cfg(windows)]
#[test]
fn only_the_payload_tree_is_ever_deletable() {
    use std::path::Path;

    let local = Path::new(r"C:\Users\me\AppData\Local");
    assert!(super::is_payload_tree(
        &local.join("ForwardSlashWindows"),
        local
    ));
    // Anything else — a parent, a sibling, a subdirectory — is refused.
    assert!(!super::is_payload_tree(local, local));
    assert!(!super::is_payload_tree(&local.join("Packages"), local));
    assert!(!super::is_payload_tree(
        &local.join("ForwardSlashWindows").join("cmd"),
        local
    ));
    assert!(!super::is_payload_tree(Path::new(r"C:\"), local));
}

#[test]
fn blocked_write_classification_covers_the_cfa_not_found_case() {
    // Access denied is a block whether or not the folder is there.
    assert!(super::looks_like_blocked_write(
        "file operation failed (Access is denied. (os error 5))",
        false
    ));
    assert!(super::looks_like_blocked_write("os error 5", true));
    // The #37 case: Controlled Folder Access surfaced the blocked profile
    // write as ERROR_FILE_NOT_FOUND, and the folder does exist.
    assert!(super::looks_like_blocked_write(
        "file operation failed (The system cannot find the file specified. (os error 2))",
        true
    ));
    // ...but a genuinely missing folder is not a CFA block.
    assert!(!super::looks_like_blocked_write(
        "file operation failed (The system cannot find the file specified. (os error 2))",
        false
    ));
    // Unrelated failures stay unexplained.
    assert!(!super::looks_like_blocked_write(
        "file operation failed (The disk is full. (os error 112))",
        true
    ));
}

// ---------------------------------------------------------------------------
// state.rs — execution policy (#45)
// ---------------------------------------------------------------------------

#[test]
fn execution_policy_parses_every_documented_name_case_insensitively() {
    use state::ExecutionPolicy as Policy;
    for (text, expected) in [
        ("Restricted", Policy::Restricted),
        ("restricted", Policy::Restricted),
        ("  RESTRICTED\r\n", Policy::Restricted),
        ("Undefined", Policy::Undefined),
        ("AllSigned", Policy::AllSigned),
        ("allsigned", Policy::AllSigned),
        ("RemoteSigned", Policy::RemoteSigned),
        ("remotesigned ", Policy::RemoteSigned),
        ("Unrestricted", Policy::Unrestricted),
        ("Bypass", Policy::Bypass),
        ("", Policy::Unrecognized),
        ("Eingeschränkt", Policy::Unrecognized),
        ("MachineSomethingNew", Policy::Unrecognized),
    ] {
        assert_eq!(state::parse_execution_policy(text), expected, "{text:?}");
    }
}

#[test]
fn only_restricted_undefined_and_allsigned_block() {
    for edition in [state::Edition::WindowsPowerShell, state::Edition::PowerShell] {
        for blocking in ["Restricted", "Undefined", "AllSigned"] {
            assert!(
                state::classify_execution_policy(edition, blocking).is_blocked(),
                "{edition:?} {blocking}"
            );
        }
        for allowed in ["RemoteSigned", "Unrestricted", "Bypass"] {
            assert_eq!(
                state::classify_execution_policy(edition, allowed),
                state::PolicyVerdict::Allowed { note: None },
                "{edition:?} {allowed}"
            );
        }
    }
}

#[test]
fn an_unknown_policy_never_blocks_but_is_noted() {
    let verdict = state::classify_execution_policy(state::Edition::WindowsPowerShell, "Whatever\r\n");
    assert!(!verdict.is_blocked());
    let note = match &verdict {
        state::PolicyVerdict::Allowed { note } => note.clone().unwrap_or_default(),
        state::PolicyVerdict::Blocked(_) => String::new(),
    };
    assert!(note.contains("unrecognized execution policy 'Whatever'"), "{note}");
}

#[test]
fn the_remedy_names_the_edition_that_owns_the_policy() {
    let windows = state::classify_execution_policy(state::Edition::WindowsPowerShell, "Restricted");
    let seven = state::classify_execution_policy(state::Edition::PowerShell, "Restricted");
    let windows = windows.blocked().map(|block| block.remedy.clone()).unwrap_or_default();
    let seven = seven.blocked().map(|block| block.remedy.clone()).unwrap_or_default();
    assert!(windows.contains("Run this in Windows PowerShell,"), "{windows}");
    assert!(seven.contains("Run this in PowerShell 7 (pwsh),"), "{seven}");
    // Both point at the same one-line fix, ending in a copyable command.
    for remedy in [&windows, &seven] {
        assert!(remedy.ends_with(state::REMOTE_SIGNED_COMMAND), "{remedy}");
    }
}

#[test]
fn the_preflight_error_names_edition_policy_remedy_and_that_nothing_changed() {
    let verdict = state::classify_execution_policy(state::Edition::WindowsPowerShell, "Restricted");
    let block = verdict.blocked().cloned().unwrap_or(state::PolicyBlock {
        reason: String::new(),
        remedy: String::new(),
    });
    assert_eq!(
        state::policy_install_error(&block),
        "Windows PowerShell's execution policy is Restricted, so the profile the adapter installs \
         can never load. Nothing was changed. Run this in Windows PowerShell, then enable the \
         adapter again: Set-ExecutionPolicy -Scope CurrentUser RemoteSigned"
    );
}

#[test]
fn undefined_explains_that_it_means_restricted() {
    let verdict = state::classify_execution_policy(state::Edition::WindowsPowerShell, "Undefined");
    let text = verdict
        .blocked()
        .map(state::policy_install_error)
        .unwrap_or_default();
    assert!(text.contains("is Undefined, which is treated as Restricted"), "{text}");
}

#[test]
fn allsigned_says_the_adapter_is_unsupported_and_why() {
    let verdict = state::classify_execution_policy(state::Edition::PowerShell, "AllSigned");
    let text = verdict
        .blocked()
        .map(state::policy_install_error)
        .unwrap_or_default();
    assert!(text.contains("does not support"), "{text}");
    // Signing the shipped module cannot help: the user's own profile is the
    // file that would need a signature.
    assert!(text.contains("your own profile.ps1"), "{text}");
}

#[test]
fn the_exit_42_rewrite_keeps_the_rollback_clause() {
    let verdict = state::classify_execution_policy(state::Edition::WindowsPowerShell, "Restricted");
    let block = verdict.blocked().cloned().unwrap_or(state::PolicyBlock {
        reason: String::new(),
        remedy: String::new(),
    });
    let text = state::policy_verify_error(&block);
    assert!(text.starts_with("Windows PowerShell's execution policy is Restricted,"), "{text}");
    assert!(text.contains("The installation was rolled back."), "{text}");
    assert!(text.ends_with(state::REMOTE_SIGNED_COMMAND), "{text}");
    // The generic "did not load the ... adapter" wording is replaced, not
    // appended to.
    assert!(!text.contains("did not load"), "{text}");
}

#[test]
fn the_health_line_says_blocked_by_execution_policy() {
    let verdict = state::classify_execution_policy(state::Edition::WindowsPowerShell, "Restricted");
    let line = state::policy_health_status("Restricted\r\n", &verdict);
    assert!(line.starts_with("Restricted — blocked by execution policy: "), "{line}");
    let ok = state::classify_execution_policy(state::Edition::WindowsPowerShell, "RemoteSigned");
    assert_eq!(state::policy_health_status("RemoteSigned\r\n", &ok), "RemoteSigned");
}

#[test]
fn integrations_json_appends_policy_fields_without_renaming_any() {
    assert_eq!(
        crate::policy_json_fields(
            "windowsPowerShell",
            "Restricted",
            true,
            "Run this in Windows PowerShell, then enable the adapter again: Set-ExecutionPolicy -Scope CurrentUser RemoteSigned"
        ),
        ",\"windowsPowerShellExecutionPolicy\":\"Restricted\",\"windowsPowerShellPolicyBlocked\":true,\
         \"windowsPowerShellPolicyRemedy\":\"Run this in Windows PowerShell, then enable the adapter again: Set-ExecutionPolicy -Scope CurrentUser RemoteSigned\""
    );
    // An edition that could not be asked reports empty, never absent.
    assert_eq!(
        crate::policy_json_fields("powerShell7", "", false, ""),
        ",\"powerShell7ExecutionPolicy\":\"\",\"powerShell7PolicyBlocked\":false,\"powerShell7PolicyRemedy\":\"\""
    );
    // Anything a shell could print stays valid JSON.
    assert_eq!(
        crate::policy_json_fields("powerShell7", "we\"ird\\", false, ""),
        ",\"powerShell7ExecutionPolicy\":\"we\\\"ird\\\\\",\"powerShell7PolicyBlocked\":false,\"powerShell7PolicyRemedy\":\"\""
    );
}

#[test]
fn find_subslice_locations() {
    assert_eq!(profile::find_subslice(b"abcdef", b"cd"), Some(2));
    assert_eq!(profile::find_subslice(b"abcdef", b"ab"), Some(0));
    assert_eq!(profile::find_subslice(b"abcdef", b"ef"), Some(4));
    assert_eq!(profile::find_subslice(b"abc", b"abcd"), None);
    assert_eq!(profile::find_subslice(b"abc", b""), None);
    assert_eq!(profile::find_subslice(b"aaa", b"aa"), Some(0));
}

#[test]
fn remove_block_excises_exactly_one_occurrence() {
    assert_eq!(
        profile::remove_block(b"head BLOCK tail", b"BLOCK").as_deref(),
        Some(b"head  tail".as_slice())
    );
    assert_eq!(profile::remove_block(b"BLOCK", b"BLOCK").as_deref(), Some(&[][..]));
    assert_eq!(profile::remove_block(b"no match", b"BLOCK"), None);
}

#[test]
fn emptied_profile_deleted_only_without_an_original() {
    assert!(profile::should_delete_profile(0, false));
    assert!(!profile::should_delete_profile(0, true));
    assert!(!profile::should_delete_profile(10, false));
}

#[test]
fn base64_utf16le_matches_known_vectors() {
    assert_eq!(profile::base64_utf16le(""), "");
    assert_eq!(
        profile::base64_utf16le("foobar"),
        "ZgBvAG8AYgBhAHIA", // "foobar" as UTF-16LE: 12 bytes, no padding
        "sanity vector for the hand-rolled encoder"
    );
    assert!(profile::base64_utf16le("a").ends_with('='));
    assert!(profile::base64_utf16le("ab").ends_with('='));
    // Must decode back to the original UTF-16LE bytes.
    let text = "Get-Alias -Name dir";
    let encoded = profile::base64_utf16le(text);
    let decoded: Vec<u8> = {
        // Minimal decoder for verification only.
        fn value_of(byte: u8) -> Option<u32> {
            match byte {
                b'A'..=b'Z' => Some(u32::from(byte - b'A')),
                b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
                b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        }
        let mut bytes = Vec::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for byte in encoded.bytes() {
            if byte == b'=' {
                break;
            }
            let Some(value) = value_of(byte) else { continue };
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                bytes.push((buffer >> bits) as u8);
            }
        }
        bytes
    };
    let expected: Vec<u8> = text
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    assert_eq!(decoded, expected);
}
