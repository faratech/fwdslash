//! Profile byte-format logic for the PowerShell adapters, ported verbatim
//! from the retired `tools/Install-PowerShellAdapter.ps1` /
//! `Uninstall-PowerShellAdapter.ps1`. Pure functions — no I/O.

/// The text encodings a profile may legally use. BOM detection order is a
/// preserved quirk of the original script: UTF-32 is tested before UTF-16, so
/// a UTF-16LE file whose first payload unit decodes through a zero byte is
/// still classified UTF-32LE. Default is UTF-8 *without* a BOM — a UTF-8 BOM
/// in the original is simply part of the untouched prefix bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

pub fn detect_encoding(bytes: &[u8]) -> ProfileEncoding {
    if bytes.len() >= 4 && bytes[0] == 0x00 && bytes[1] == 0x00 && bytes[2] == 0xFE && bytes[3] == 0xFF
    {
        return ProfileEncoding::Utf32Be;
    }
    if bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xFE && bytes[2] == 0x00 && bytes[3] == 0x00
    {
        return ProfileEncoding::Utf32Le;
    }
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        return ProfileEncoding::Utf16Be;
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        return ProfileEncoding::Utf16Le;
    }
    ProfileEncoding::Utf8
}

/// Encodes `text` in the profile's encoding. Never prepends a BOM: the
/// original script constructed every encoder without one, and the BOM (if
/// any) lives in the untouched prefix bytes.
#[must_use]
pub fn encode(text: &str, encoding: ProfileEncoding) -> Vec<u8> {
    match encoding {
        ProfileEncoding::Utf16Le => text
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect(),
        ProfileEncoding::Utf16Be => text
            .encode_utf16()
            .flat_map(|unit| unit.to_be_bytes())
            .collect(),
        ProfileEncoding::Utf32Le => text.chars().flat_map(|c| u32::from(c).to_le_bytes()).collect(),
        ProfileEncoding::Utf32Be => text.chars().flat_map(|c| u32::from(c).to_be_bytes()).collect(),
        ProfileEncoding::Utf8 => text.bytes().collect(),
    }
}

/// The guarded profile block. `module_path` quotes are doubled, per the
/// script; the block is CRLF-terminated and prefixed with a blank CRLF line
/// only when the original profile was non-empty.
#[must_use]
pub fn block_text(version: &str, transaction_id: &str, module_path: &str, original_non_empty: bool) -> String {
    let escaped_module = module_path.replace('\'', "''");
    let prefix = if original_non_empty { "\r\n" } else { "" };
    format!(
        "{prefix}# >>> Forward Slash Windows {version} {transaction_id} >>>\r\n\
         Import-Module -Name '{escaped_module}' -Global -Force\r\n\
         # <<< Forward Slash Windows {version} {transaction_id} <<<\r\n"
    )
}

#[must_use]
pub fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&start| &haystack[start..start + needle.len()] == needle)
}

/// Removes one occurrence of `block` from `current`. `None` when absent.
#[must_use]
pub fn remove_block(current: &[u8], block: &[u8]) -> Option<Vec<u8>> {
    let index = find_subslice(current, block)?;
    let mut remaining = Vec::with_capacity(current.len() - block.len());
    remaining.extend_from_slice(&current[..index]);
    remaining.extend_from_slice(&current[index + block.len()..]);
    Some(remaining)
}

/// An emptied profile is deleted only when there was no original file before
/// the adapter added its block.
pub fn should_delete_profile(remaining_len: usize, original_present: bool) -> bool {
    remaining_len == 0 && !original_present
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 of the UTF-16LE encoding — the format PowerShell's
/// `-EncodedCommand` expects. Hand-rolled to keep the crate dependency-free.
#[must_use]
pub fn base64_utf16le(text: &str) -> String {
    let bytes: Vec<u8> = text
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64[(triple >> 18) as usize & 0x3F] as char);
        out.push(BASE64[(triple >> 12) as usize & 0x3F] as char);
        if chunk.len() > 1 {
            out.push(BASE64[(triple >> 6) as usize & 0x3F] as char);
        }
        if chunk.len() > 2 {
            out.push(BASE64[triple as usize & 0x3F] as char);
        }
    }
    for _ in 0..(3 - bytes.len() % 3) % 3 {
        out.push('=');
    }
    out
}

/// The in-shell verification the installer runs after writing the profile:
/// both aliases must resolve to the adapter function. Exit 41 = wrong
/// definition, 42 = an alias is missing.
pub const VERIFY_SCRIPT: &str = concat!(
    "$ErrorActionPreference = 'Stop'\n",
    "try {\n",
    "    $dirAlias = Get-Alias -Name dir\n",
    "    $lsAlias = Get-Alias -Name ls\n",
    "    if ($dirAlias.Definition -ne 'Invoke-ForwardSlashWindowsChildItem' -or\n",
    "        $lsAlias.Definition -ne 'Invoke-ForwardSlashWindowsChildItem') {\n",
    "        exit 41\n",
    "    }\n",
    "    exit 0\n",
    "} catch {\n",
    "    exit 42\n",
    "}\n",
);
