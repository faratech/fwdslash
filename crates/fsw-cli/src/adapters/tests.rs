//! Tests for the pure adapter decision and byte-format logic (state.rs,
//! profile.rs). The Win32 I/O in cmd.rs/powershell.rs is exercised by the
//! manual verification matrix; everything decided here is what that I/O
//! feeds on.

use super::profile;
use super::state;

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

#[test]
fn block_text_matches_the_script_layout() {
    // The payload version follows the crate version, so the layout assertions
    // are built from PAYLOAD_VERSION rather than a literal that goes stale
    // at every release.
    let version = super::PAYLOAD_VERSION;
    let module = format!(
        r"C:\Users\me\AppData\Local\ForwardSlashWindows\PowerShell\{version}\ForwardSlashWindows.psm1"
    );
    let block = profile::block_text(version, "cafe", &module, false);
    assert!(block.starts_with(&format!(
        "# >>> Forward Slash Windows {version} cafe >>>\r\n"
    )));
    assert!(block.contains(&format!(
        "Import-Module -Name 'C:\\Users\\me\\AppData\\Local\\ForwardSlashWindows\\PowerShell\\{version}\\ForwardSlashWindows.psm1' -Global -Force\r\n"
    )));
    assert!(block.ends_with(&format!(
        "# <<< Forward Slash Windows {version} cafe <<<\r\n"
    )));
}

#[test]
fn block_text_escapes_quotes_and_prefixes_nonempty_originals() {
    let block = profile::block_text(super::PAYLOAD_VERSION, "t", "C:\\it's", true);
    assert!(block.starts_with("\r\n"));
    assert!(block.contains("'C:\\it''s'"));
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
