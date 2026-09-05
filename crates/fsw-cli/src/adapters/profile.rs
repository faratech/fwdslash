//! Profile byte-format logic for the PowerShell adapters, ported from the
//! retired `tools/Install-PowerShellAdapter.ps1` /
//! `Uninstall-PowerShellAdapter.ps1`. Pure functions — no I/O.
//!
//! The block the adapter writes is a **fenced region** delimited by two marker
//! comment lines (`# >>> Forward Slash Windows <ver> <id> >>>` … `# <<< … <<<`).
//! Everything here that has to survive an upgrade — the idempotent strip, the
//! block parser, the health classifier — keys off those fence lines, never off
//! an exact byte-for-byte copy of a specific version's block (#37).

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

/// The BOM byte length `detect_encoding` implies for `encoding`. UTF-8 is
/// treated as BOM-less: a UTF-8 BOM, if any, stays in the decoded text and
/// round-trips through `encode`.
fn bom_len(encoding: ProfileEncoding) -> usize {
    match encoding {
        ProfileEncoding::Utf8 => 0,
        ProfileEncoding::Utf16Le | ProfileEncoding::Utf16Be => 2,
        ProfileEncoding::Utf32Le | ProfileEncoding::Utf32Be => 4,
    }
}

/// Decodes `bytes` (already BOM-stripped) in `encoding`. `None` when the bytes
/// are not valid text in that encoding — the caller then leaves the buffer
/// untouched rather than mangling it.
fn decode(bytes: &[u8], encoding: ProfileEncoding) -> Option<String> {
    match encoding {
        ProfileEncoding::Utf8 => std::str::from_utf8(bytes).ok().map(str::to_owned),
        ProfileEncoding::Utf16Le | ProfileEncoding::Utf16Be => {
            if bytes.len() % 2 != 0 {
                return None;
            }
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| match encoding {
                    ProfileEncoding::Utf16Be => u16::from_be_bytes([pair[0], pair[1]]),
                    _ => u16::from_le_bytes([pair[0], pair[1]]),
                })
                .collect();
            String::from_utf16(&units).ok()
        }
        ProfileEncoding::Utf32Le | ProfileEncoding::Utf32Be => {
            if bytes.len() % 4 != 0 {
                return None;
            }
            let mut text = String::with_capacity(bytes.len() / 4);
            for quad in bytes.chunks_exact(4) {
                let value = match encoding {
                    ProfileEncoding::Utf32Be => u32::from_be_bytes([quad[0], quad[1], quad[2], quad[3]]),
                    _ => u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]),
                };
                text.push(char::from_u32(value)?);
            }
            Some(text)
        }
    }
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

/// PowerShell single-quote literal escaping: a `'` doubles to `''`.
fn escape_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

/// The open/close fence marker prefixes. A fence line is recognised by these
/// plus the matching `>>>`/`<<<` suffix, so any version and any transaction id
/// are stripped, not only this build's (#37).
const FENCE_OPEN_PREFIX: &str = "# >>> Forward Slash Windows";
const FENCE_CLOSE_PREFIX: &str = "# <<< Forward Slash Windows";

/// Everything the guarded profile block needs. Grouped into a struct rather
/// than a long positional argument list because the block now carries the
/// product-presence probe and the staged-controller path for the self-clean
/// branch (#37 addendum).
pub struct BlockParams<'a> {
    pub version: &'a str,
    pub transaction_id: &'a str,
    /// The deployed `ForwardSlashWindows.psm1`.
    pub module_path: &'a str,
    /// A path that exists while the product is installed: the package's own
    /// app-data folder (packaged) or the controller's directory (unpackaged).
    /// Its absence is the cheap "product gone" signal that arms the self-clean.
    pub probe_path: &'a str,
    /// The app-execution alias, checked as a second "present" signal. It is
    /// only ever an OR: a user can disable the alias under Settings > Apps,
    /// which must not look like an uninstall.
    pub alias_path: &'a str,
    /// The staged `fwdslash.exe` next to the module, invoked as the self-clean
    /// entry point (`uninstall --orphaned`) — it survives an MSIX uninstall
    /// because `%LOCALAPPDATA%` is not virtualized.
    pub controller_path: &'a str,
    /// Prefix the block with a blank CRLF line only when the original profile
    /// was non-empty.
    pub original_non_empty: bool,
}

/// The guarded profile block.
///
/// Guarded three ways (#37): `Import-Module` runs only when the module file is
/// present, so a pruned version directory can never throw the red
/// `no valid module file` error; when the product-presence probe is gone the
/// block hands off to the staged controller's self-clean; and the whole region
/// is fenced so it can be found and replaced by marker, not by exact bytes.
/// The block is CRLF-terminated and prefixed with a blank CRLF line only when
/// the original profile was non-empty.
#[must_use]
pub fn block_text(params: &BlockParams) -> String {
    let module = escape_single_quoted(params.module_path);
    let probe = escape_single_quoted(params.probe_path);
    let alias = escape_single_quoted(params.alias_path);
    let controller = escape_single_quoted(params.controller_path);
    let prefix = if params.original_non_empty { "\r\n" } else { "" };
    let version = params.version;
    let id = params.transaction_id;
    format!(
        "{prefix}# >>> Forward Slash Windows {version} {id} >>>\r\n\
         $m = '{module}'\r\n\
         $p = '{probe}'\r\n\
         $a = '{alias}'\r\n\
         $c = '{controller}'\r\n\
         if ((Test-Path -LiteralPath $p) -or ($a -and (Test-Path -LiteralPath $a))) {{ if (Test-Path -LiteralPath $m) {{ Import-Module -Name $m -Global -Force }} }} elseif (Test-Path -LiteralPath $c) {{ Start-Process -FilePath $c -ArgumentList 'uninstall','--orphaned' -WindowStyle Hidden -ErrorAction SilentlyContinue }}\r\n\
         # <<< Forward Slash Windows {version} {id} <<<\r\n"
    )
}

#[must_use]
pub fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&start| &haystack[start..start + needle.len()] == needle)
}

/// Removes one occurrence of `block` from `current`. `None` when absent. Kept
/// as the exact-match fast path; `strip_fwdslash_blocks` is the marker-based
/// superset uninstall also runs (#37).
#[must_use]
pub fn remove_block(current: &[u8], block: &[u8]) -> Option<Vec<u8>> {
    let index = find_subslice(current, block)?;
    let mut remaining = Vec::with_capacity(current.len() - block.len());
    remaining.extend_from_slice(&current[..index]);
    remaining.extend_from_slice(&current[index + block.len()..]);
    Some(remaining)
}

/// An emptied profile is deleted only when there was no genuine (non-fwdslash)
/// original before the adapter wrote its block.
pub fn should_delete_profile(remaining_len: usize, original_present: bool) -> bool {
    remaining_len == 0 && !original_present
}

/// Keep empty originals, but discard files consisting only of orphaned blocks.
pub fn original_profile_present(existed: bool, original: &[u8], cleaned: &[u8]) -> bool {
    existed && (original.is_empty() || !cleaned.is_empty())
}

fn line_trim(content: &str) -> &str {
    content.trim()
}

fn is_open_fence(content: &str) -> bool {
    let t = line_trim(content);
    t.starts_with(FENCE_OPEN_PREFIX) && t.ends_with(">>>")
}

fn is_close_fence(content: &str) -> bool {
    let t = line_trim(content);
    t.starts_with(FENCE_CLOSE_PREFIX) && t.ends_with("<<<")
}

/// One physical line: `[start, content_end)` is the text without its line
/// terminator, `[content_end, term_end)` is the terminator (`\r\n`, `\r`, or
/// `\n`; empty at EOF).
struct LineSpan {
    start: usize,
    content_end: usize,
    term_end: usize,
}

fn split_lines(text: &str) -> Vec<LineSpan> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let mut j = i;
        while j < bytes.len() && bytes[j] != b'\n' && bytes[j] != b'\r' {
            j += 1;
        }
        let content_end = j;
        let mut k = j;
        if k < bytes.len() && bytes[k] == b'\r' {
            k += 1;
        }
        if k < bytes.len() && bytes[k] == b'\n' {
            k += 1;
        }
        // A lone '\r' or '\n' still advances so the loop terminates.
        if k == start {
            k += 1;
        }
        lines.push(LineSpan {
            start,
            content_end,
            term_end: k,
        });
        i = k;
    }
    lines
}

/// Removes every fwdslash fenced region from `text`, collapsing the single
/// preceding line terminator that `block_text` inserts as its blank-line
/// prefix. Content outside the fences is preserved exactly.
fn strip_fence_regions(text: &str) -> String {
    let lines = split_lines(text);
    let mut removals: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let content = &text[lines[i].start..lines[i].content_end];
        let region = if is_open_fence(content) {
            // Match forward to the next close fence; stop at another open
            // (a truncated region) and drop just the lone open line.
            let mut j = i + 1;
            let mut close = None;
            while j < lines.len() {
                let cj = &text[lines[j].start..lines[j].content_end];
                if is_close_fence(cj) {
                    close = Some(j);
                    break;
                }
                if is_open_fence(cj) {
                    break;
                }
                j += 1;
            }
            Some(close.unwrap_or(i))
        } else if is_close_fence(content) {
            // An unmatched close line is our own debris too.
            Some(i)
        } else {
            None
        };
        match region {
            Some(last) => {
                // Swallow the terminator of the preceding line (keeping its
                // content), which is exactly the blank-line prefix block_text
                // added — or, for a no-trailing-newline original, the newline
                // it introduced.
                let start = if i > 0 {
                    lines[i - 1].content_end
                } else {
                    lines[i].start
                };
                removals.push((start, lines[last].term_end));
                i = last + 1;
            }
            None => i += 1,
        }
    }
    if removals.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in removals {
        let start = start.max(cursor);
        if start > cursor {
            out.push_str(&text[cursor..start]);
        }
        cursor = cursor.max(end);
    }
    if cursor < text.len() {
        out.push_str(&text[cursor..]);
    }
    out
}

/// The idempotent, encoding-aware strip: returns `bytes` with every fwdslash
/// fenced region removed. A buffer with no fence, or one whose bytes are not
/// valid text in its detected encoding, is returned unchanged so the true
/// original stays byte-exact (#37).
#[must_use]
pub fn strip_fwdslash_blocks(bytes: &[u8]) -> Vec<u8> {
    let encoding = detect_encoding(bytes);
    let split = bom_len(encoding).min(bytes.len());
    let (bom, body) = bytes.split_at(split);
    let Some(text) = decode(body, encoding) else {
        return bytes.to_vec();
    };
    if !text.contains(FENCE_OPEN_PREFIX) && !text.contains(FENCE_CLOSE_PREFIX) {
        return bytes.to_vec();
    }
    let stripped = strip_fence_regions(&text);
    if stripped == text {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bom.len() + stripped.len());
    out.extend_from_slice(bom);
    out.extend_from_slice(&encode(&stripped, encoding));
    out
}

/// A fwdslash block parsed out of a profile: the version and transaction id
/// from its open fence, and the first single-quoted literal in its body — the
/// module path, from either the new `$m = '…'` line or an old one-line
/// `Import-Module -Name '…'` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBlock {
    pub version: String,
    pub transaction_id: String,
    pub module_path: Option<String>,
}

/// Parses every fwdslash block in `bytes` (encoding-aware).
#[must_use]
pub fn parse_blocks(bytes: &[u8]) -> Vec<ParsedBlock> {
    let encoding = detect_encoding(bytes);
    let split = bom_len(encoding).min(bytes.len());
    let (_, body) = bytes.split_at(split);
    match decode(body, encoding) {
        Some(text) => parse_blocks_text(&text),
        None => Vec::new(),
    }
}

fn parse_blocks_text(text: &str) -> Vec<ParsedBlock> {
    let lines = split_lines(text);
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let content = &text[lines[i].start..lines[i].content_end];
        if !is_open_fence(content) {
            i += 1;
            continue;
        }
        let (version, transaction_id) = parse_open_fence(content);
        // Body: lines after the open fence up to the close (or the next open).
        let mut j = i + 1;
        let mut module_path = None;
        while j < lines.len() {
            let cj = &text[lines[j].start..lines[j].content_end];
            if is_close_fence(cj) || is_open_fence(cj) {
                break;
            }
            if module_path.is_none() {
                module_path = first_single_quoted(cj);
            }
            j += 1;
        }
        blocks.push(ParsedBlock {
            version,
            transaction_id,
            module_path,
        });
        // Continue after the close fence when there is one.
        i = if j < lines.len() && is_close_fence(&text[lines[j].start..lines[j].content_end]) {
            j + 1
        } else {
            j
        };
    }
    blocks
}

/// `# >>> Forward Slash Windows <version> <id> >>>` → `(version, id)`. Missing
/// fields come back empty rather than failing the parse.
fn parse_open_fence(content: &str) -> (String, String) {
    let trimmed = line_trim(content);
    let inner = trimmed
        .strip_prefix(FENCE_OPEN_PREFIX)
        .and_then(|rest| rest.trim().strip_suffix(">>>"))
        .map(str::trim)
        .unwrap_or("");
    let mut parts = inner.split_whitespace();
    let version = parts.next().unwrap_or("").to_string();
    let id = parts.collect::<Vec<_>>().join(" ");
    (version, id)
}

/// The first single-quoted PowerShell literal on `line`, with `''` un-doubled.
fn first_single_quoted(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let open = line.find('\'')?;
    let mut out = String::new();
    let mut i = open + 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                out.push('\'');
                i += 2;
                continue;
            }
            return Some(out);
        }
        // Push the char starting at byte i (handles multi-byte UTF-8).
        if let Some(chunk) = line.get(i..) {
            if let Some(ch) = chunk.chars().next() {
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
        }
        break;
    }
    // Unterminated literal: return what we have rather than nothing.
    Some(out)
}

/// One profile block plus whether its module file is present on disk. The I/O
/// caller fills `module_present`; the classifier stays pure.
#[derive(Debug, Clone)]
pub struct BlockPresence {
    pub version: String,
    pub module_present: bool,
}

/// The health of a single edition's profile with respect to its fwdslash
/// block(s). Reported verbatim by `fwdslash doctor` / `integrations` and
/// consumed by `decide_profile_repair`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileHealth {
    /// No fwdslash block at all.
    Clean,
    /// Exactly one block, current version, module present.
    Healthy,
    /// A block whose module file is missing — the red-error orphan. Carries
    /// the offending block's version.
    Orphaned(String),
    /// A single block whose version differs from the current payload. Carries
    /// that version.
    Stale(String),
    /// More than one fwdslash block.
    Duplicated,
}

/// Classifies a profile from its blocks. Orphan (missing module) outranks the
/// rest because it is the only state that throws a visible error.
#[must_use]
pub fn classify_profile(blocks: &[BlockPresence], current_version: &str) -> ProfileHealth {
    if blocks.is_empty() {
        return ProfileHealth::Clean;
    }
    if let Some(missing) = blocks.iter().find(|b| !b.module_present) {
        return ProfileHealth::Orphaned(missing.version.clone());
    }
    if blocks.len() > 1 {
        return ProfileHealth::Duplicated;
    }
    let only = &blocks[0];
    if only.version != current_version {
        return ProfileHealth::Stale(only.version.clone());
    }
    ProfileHealth::Healthy
}

/// What a repair should do to a profile, decided purely from its health plus
/// the marker/module facts the I/O layer supplies (#37).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileAction {
    /// Leave the profile untouched.
    Nothing,
    /// Strip every fwdslash block; restore the true original (or delete an
    /// emptied file).
    RemoveBlocks,
    /// Strip every block and write the true original plus one current guarded
    /// block.
    WriteCurrentBlock,
    /// The adapter should be installed but its current module is missing: a
    /// full redeploy is required.
    Reinstall,
}

/// The repair verdict. `marker_installed` is whether the edition's marker says
/// `installed`; `current_module_present` is whether the *current* payload's
/// module file exists on disk.
#[must_use]
pub fn decide_profile_repair(
    health: &ProfileHealth,
    marker_installed: bool,
    current_module_present: bool,
) -> ProfileAction {
    let restore_installed = || {
        if current_module_present {
            ProfileAction::WriteCurrentBlock
        } else {
            ProfileAction::Reinstall
        }
    };
    match health {
        // No block, but the marker still claims installed: put one back.
        ProfileHealth::Clean => {
            if marker_installed {
                restore_installed()
            } else {
                ProfileAction::Nothing
            }
        }
        // One good current block: leave it, unless the marker is gone — then
        // it is a dangling leftover to remove.
        ProfileHealth::Healthy => {
            if marker_installed {
                ProfileAction::Nothing
            } else {
                ProfileAction::RemoveBlocks
            }
        }
        // Orphan / stale / duplicate: normalise to one current block when the
        // adapter should be installed, otherwise strip it out entirely.
        ProfileHealth::Orphaned(_) | ProfileHealth::Stale(_) | ProfileHealth::Duplicated => {
            if marker_installed {
                restore_installed()
            } else {
                ProfileAction::RemoveBlocks
            }
        }
    }
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
