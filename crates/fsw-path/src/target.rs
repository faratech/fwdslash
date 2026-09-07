//! Owned, typed navigation targets for the shell and broker boundary.
//!
//! The historical resolver returns borrows into [`RenderBuf`], which is ideal
//! for the hot WSL-only path but cannot represent a local drive. This module
//! deliberately owns its output and never performs filesystem, network, or
//! existence checks.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{
    BareSlashMode, Context, Registry, RenderBuf, ResolveError, Resolved, WSL_ROOT_UNC,
    is_valid_windows_root, resolve, resolve_under_root,
};

/// The destination family a caller must hand to the Windows shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    LocalDrive,
    WslDistribution,
    NativeUNC,
    NavigationRoot,
}

/// An explicit base for a relative user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetBase<'a> {
    /// An absolute, non-device Windows path.
    NativePath(&'a str),
    /// A path inside a known WSL distribution.
    WslDistribution {
        distribution: &'a str,
        linux_path: &'a str,
    },
}

/// A concrete reason an otherwise path-like input was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetRejection {
    InvalidPath,
    InvalidFileUri,
    InvalidPercentEncoding,
    DevicePath,
    SlashPath(ResolveError),
}

/// Why input could not become a navigation target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// The input belongs to another handler, such as an HTTP URL or shell
    /// expansion. It was intentionally not interpreted as a path.
    NotHandled,
    /// Resolution needs a WSL context or an explicit relative base.
    NeedsContext,
    /// The input was path-like but malformed or disallowed.
    Rejected(TargetRejection),
}

/// An owned destination safe to pass across the CLI/broker boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTarget {
    kind: TargetKind,
    native_path: String,
    wsl_context: Option<(String, String)>,
}

impl UserTarget {
    #[must_use]
    pub const fn kind(&self) -> TargetKind {
        self.kind
    }

    /// Absolute Windows path suitable for the native shell APIs.
    #[must_use]
    pub fn native_path(&self) -> &str {
        &self.native_path
    }

    /// RFC 3986-style `file:` URI for this target.
    ///
    /// This returns a `Result` so callers have one stable failure channel if a
    /// future target kind cannot be represented as a file URI.
    pub fn file_uri(&self) -> Result<String, TargetError> {
        Ok(file_uri_from_native(&self.native_path))
    }

    /// Returns the actual Linux path only for a target resolved inside the
    /// requested WSL distribution. A native drive spelling intentionally has
    /// no implicit Linux mapping.
    #[must_use]
    pub fn to_wsl_path(&self, distribution: &str) -> Option<String> {
        self.wsl_context
            .as_ref()
            .and_then(|(resolved_distribution, linux_path)| {
                super::eq_ignore_case(resolved_distribution, distribution)
                    .then(|| linux_path.clone())
            })
    }
}

/// Resolves Windows paths, `file:` URIs, slash aliases, and relative inputs.
///
/// `context` supplies the existing registered-distribution semantics. It is
/// required for slash aliases other than `/mnt/<drive>`, and for relative WSL
/// bases. `custom_root` has exactly the same precedence as the core legacy
/// slash resolver. No branch probes the filesystem or expands shell variables.
pub fn resolve_user_target<R: Registry + ?Sized>(
    input: &str,
    context: Option<&Context<'_, R>>,
    custom_root: Option<&str>,
    base: Option<TargetBase<'_>>,
) -> Result<UserTarget, TargetError> {
    if input.is_empty() {
        return Err(TargetError::Rejected(TargetRejection::InvalidPath));
    }
    if is_device_path(input) {
        return Err(TargetError::Rejected(TargetRejection::DevicePath));
    }
    if has_file_scheme(input) {
        return resolve_file_uri(input);
    }
    if has_other_uri_scheme(input) || looks_like_shell_expansion(input) {
        return Err(TargetError::NotHandled);
    }
    if is_absolute_native_path(input) {
        return native_target(input, None);
    }
    if input.starts_with('/') {
        return resolve_slash_target(input, context, custom_root);
    }
    resolve_relative(input, context, custom_root, base)
}

fn resolve_slash_target<R: Registry + ?Sized>(
    input: &str,
    context: Option<&Context<'_, R>>,
    custom_root: Option<&str>,
) -> Result<UserTarget, TargetError> {
    if let Some(target) = resolve_mnt_drive(input)? {
        return Ok(target);
    }
    if input.as_bytes().get(1) == Some(&b'/') {
        return Err(rejected_slash(ResolveError::DoubleLeadingSlash));
    }
    if input.contains('\0') {
        return Err(rejected_slash(ResolveError::EmbeddedNul));
    }
    if input.contains('\\') {
        return Err(rejected_slash(ResolveError::BackslashNotAllowed));
    }
    let Some(context) = context else {
        return Err(TargetError::NeedsContext);
    };

    let first_segment = input[1..].split('/').next().unwrap_or_default();
    let root = custom_root.filter(|root| is_valid_windows_root(root));
    // The first segment is a distribution selector only when the bare slash
    // opens the provider root that lists distributions. When it opens a
    // distribution or a user-chosen folder root, every segment is filesystem
    // content of that root: `/ubuntu` must resolve under the root the user
    // pinned even when a distribution shares the folder's name.
    let explicit_distro = root.is_none()
        && context.mode == BareSlashMode::DistributionList
        && !first_segment.is_empty()
        && context.registry.is_registered(first_segment);

    let mut render_buf = RenderBuf::new();
    let resolved = if explicit_distro {
        resolve(input, context, &mut render_buf)
    } else if let Some(root) = root {
        resolve_under_root(input, root, &mut render_buf)
    } else {
        resolve(input, context, &mut render_buf)
    }
    .map_err(rejected_slash)?;
    target_from_resolved(resolved)
}

fn resolve_relative<R: Registry + ?Sized>(
    input: &str,
    context: Option<&Context<'_, R>>,
    custom_root: Option<&str>,
    base: Option<TargetBase<'_>>,
) -> Result<UserTarget, TargetError> {
    let Some(base) = base else {
        return Err(TargetError::NeedsContext);
    };
    if input.contains('\0') || is_device_path(input) {
        return Err(TargetError::Rejected(TargetRejection::InvalidPath));
    }
    match base {
        TargetBase::NativePath(base) => {
            let base = native_target(base, None)?;
            let mut joined = base.native_path;
            if !joined.ends_with('\\') {
                joined.push('\\');
            }
            joined.push_str(&input.replace('/', "\\"));
            native_target(&joined, None)
        }
        TargetBase::WslDistribution {
            distribution,
            linux_path,
        } => {
            if !linux_path.starts_with('/') || linux_path.contains('\\') {
                return Err(TargetError::Rejected(TargetRejection::InvalidPath));
            }
            let suffix = linux_path.trim_end_matches('/');
            let full_input = if suffix.is_empty() {
                format!("/{distribution}/{input}")
            } else {
                format!("/{distribution}{suffix}/{input}")
            };
            resolve_slash_target(&full_input, context, custom_root)
        }
    }
}

fn resolve_mnt_drive(input: &str) -> Result<Option<UserTarget>, TargetError> {
    let mut render_buf = RenderBuf::new();
    match resolve_mnt_drive_resolved(input, &mut render_buf).map_err(rejected_slash)? {
        Some(resolved) => {
            let mut target = target_from_resolved(resolved)?;
            target.kind = TargetKind::LocalDrive;
            Ok(Some(target))
        }
        None => Ok(None),
    }
}

/// `/mnt/<letter>[/tail]` — the WSL drive alias. Public so the shell/Explorer
/// funnel resolves the alias identically to the browser funnel: the same input
/// must not become `C:\temp` in a browser and "unregistered distribution" in
/// Explorer. `None` for inputs the alias does not claim (callers fall through
/// to distribution matching); `Err` for claimed inputs that fail shape checks.
pub fn resolve_mnt_drive_resolved<'r>(
    input: &str,
    render_buf: &'r mut RenderBuf,
) -> Result<Option<Resolved<'r>>, ResolveError> {
    let bytes = input.as_bytes();
    if !input.starts_with("/mnt/") {
        return Ok(None);
    }
    let Some(&drive_byte) = bytes.get(5) else {
        return Ok(None);
    };
    if !drive_byte.is_ascii_alphabetic() {
        return Ok(None);
    }
    if bytes.get(6).is_some_and(|byte| *byte != b'/') {
        return Ok(None);
    }
    if input.contains('\0') {
        return Err(ResolveError::EmbeddedNul);
    }
    if input.contains('\\') {
        return Err(ResolveError::BackslashNotAllowed);
    }
    let drive = (drive_byte as char).to_ascii_uppercase();
    let tail = input
        .get(6..)
        .filter(|tail| !tail.is_empty())
        .unwrap_or("/");
    let resolved = resolve_under_root(tail, &format!("{drive}:\\"), render_buf)?;
    Ok(Some(resolved))
}

fn target_from_resolved(resolved: Resolved<'_>) -> Result<UserTarget, TargetError> {
    match resolved {
        Resolved::WslRoot => Ok(UserTarget {
            kind: TargetKind::NavigationRoot,
            native_path: WSL_ROOT_UNC.to_string(),
            wsl_context: None,
        }),
        Resolved::Distribution(path) => Ok(UserTarget {
            kind: TargetKind::WslDistribution,
            native_path: path.unc_display().to_string(),
            wsl_context: Some((
                path.distribution().to_string(),
                path.linux_path().to_string(),
            )),
        }),
        Resolved::Folder(path) => native_target(path.display(), None),
    }
}

fn native_target(
    input: &str,
    wsl_context: Option<(String, String)>,
) -> Result<UserTarget, TargetError> {
    // This boundary produces a Win32 path. Unlike the legacy WSL resolver,
    // every C0 code point is invalid in a native Windows path, including one
    // introduced by percent-decoding a file URI.
    if input.bytes().any(|byte| byte <= 0x1f) {
        return Err(TargetError::Rejected(TargetRejection::InvalidPath));
    }
    if is_device_path(input) {
        return Err(TargetError::Rejected(TargetRejection::DevicePath));
    }
    let native_path = normalize_native_path(input)?;
    let kind = if native_path.starts_with(r"\\") {
        TargetKind::NativeUNC
    } else {
        TargetKind::LocalDrive
    };
    Ok(UserTarget {
        kind,
        native_path,
        wsl_context,
    })
}

fn normalize_native_path(input: &str) -> Result<String, TargetError> {
    let mut native = input.replace('/', "\\");
    if let Some(rest) = native.strip_prefix(r"\\") {
        let mut parts = rest.split('\\');
        let server = parts.next().unwrap_or_default();
        let share = parts.next().unwrap_or_default();
        if server.is_empty() || share.is_empty() || server.contains(':') || share.contains(':') {
            return Err(TargetError::Rejected(TargetRejection::InvalidPath));
        }
        if native.starts_with(r"\\\\") {
            return Err(TargetError::Rejected(TargetRejection::InvalidPath));
        }
        return Ok(native);
    }
    let bytes = native.as_bytes();
    if !matches!(
        (bytes.first(), bytes.get(1), bytes.get(2)),
        (Some(letter), Some(b':'), Some(b'\\')) if letter.is_ascii_alphabetic()
    ) {
        return Err(TargetError::Rejected(TargetRejection::InvalidPath));
    }
    native.replace_range(0..1, &native[0..1].to_ascii_uppercase());
    Ok(native)
}

fn resolve_file_uri(input: &str) -> Result<UserTarget, TargetError> {
    let rest = input.get(5..).unwrap_or_default();
    if rest.contains('?') || rest.contains('#') {
        return Err(TargetError::Rejected(TargetRejection::InvalidFileUri));
    }
    if let Some(authority_and_path) = rest.strip_prefix("//") {
        let (authority, raw_path) = match authority_and_path.find('/') {
            Some(index) => (&authority_and_path[..index], &authority_and_path[index..]),
            None => (authority_and_path, ""),
        };
        let authority = percent_decode(authority)?;
        if has_encoded_separator(raw_path) {
            return Err(TargetError::Rejected(TargetRejection::InvalidFileUri));
        }
        let path = percent_decode(raw_path)?;
        if authority.is_empty() || authority.eq_ignore_ascii_case("localhost") {
            return native_target(&path.trim_start_matches('/').replace('/', "\\"), None);
        }
        if authority.contains(['/', '\\', '@', ':']) || path.is_empty() {
            return Err(TargetError::Rejected(TargetRejection::InvalidFileUri));
        }
        return native_target(&format!(r"\\{authority}{}", path.replace('/', "\\")), None);
    }
    if has_encoded_separator(rest) {
        return Err(TargetError::Rejected(TargetRejection::InvalidFileUri));
    }
    let path = percent_decode(rest)?;
    if path.starts_with('/') {
        return native_target(&path.trim_start_matches('/').replace('/', "\\"), None);
    }
    Err(TargetError::NeedsContext)
}

fn file_uri_from_native(native_path: &str) -> String {
    if let Some(rest) = native_path.strip_prefix(r"\\") {
        let (server, path) = rest.split_once('\\').unwrap_or((rest, ""));
        let encoded_path = percent_encode(&path.replace('\\', "/"));
        return if encoded_path.is_empty() {
            format!("file://{}", percent_encode(server))
        } else {
            format!("file://{}/{encoded_path}", percent_encode(server))
        };
    }
    format!(
        "file:///{}",
        percent_encode(&native_path.replace('\\', "/"))
    )
}

fn percent_decode(input: &str) -> Result<String, TargetError> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let Some(high) = bytes.get(index + 1).and_then(|byte| hex_value(*byte)) else {
                return Err(TargetError::Rejected(
                    TargetRejection::InvalidPercentEncoding,
                ));
            };
            let Some(low) = bytes.get(index + 2).and_then(|byte| hex_value(*byte)) else {
                return Err(TargetError::Rejected(
                    TargetRejection::InvalidPercentEncoding,
                ));
            };
            output.push((high << 4) | low);
            index += 3;
        } else {
            let Some(character) = input
                .get(index..)
                .and_then(|remainder| remainder.chars().next())
            else {
                return Err(TargetError::Rejected(
                    TargetRejection::InvalidPercentEncoding,
                ));
            };
            let mut utf8 = [0; 4];
            output.extend_from_slice(character.encode_utf8(&mut utf8).as_bytes());
            index += character.len_utf8();
        }
    }
    String::from_utf8(output)
        .map_err(|_| TargetError::Rejected(TargetRejection::InvalidPercentEncoding))
}

fn percent_encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(input.len());
    for byte in input.bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':')
        {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(char::from(
                HEX.get((byte >> 4) as usize).copied().unwrap_or_default(),
            ));
            encoded.push(char::from(
                HEX.get((byte & 0x0f) as usize).copied().unwrap_or_default(),
            ));
        }
    }
    encoded
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn has_encoded_separator(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.windows(3).any(|window| {
        matches!(
            window,
            [b'%', first, second]
                if matches!(
                    (first.to_ascii_uppercase(), second.to_ascii_uppercase()),
                    (b'2', b'F') | (b'5', b'C')
                )
        )
    })
}

fn has_file_scheme(input: &str) -> bool {
    input
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
}

fn has_other_uri_scheme(input: &str) -> bool {
    let Some(colon) = input.find(':') else {
        return false;
    };
    colon > 1
        && input.as_bytes().get(..colon).is_some_and(|scheme| {
            scheme.iter().enumerate().all(|(index, byte)| {
                byte.is_ascii_alphanumeric()
                    || *byte == b'+'
                    || *byte == b'-'
                    || *byte == b'.'
                    || (index > 0 && *byte == b'_')
            })
        })
}

fn looks_like_shell_expansion(input: &str) -> bool {
    input.starts_with('%') || input.starts_with('$') || input.starts_with('~')
}

fn is_device_path(input: &str) -> bool {
    input.starts_with("\\\\?\\") || input.starts_with("\\\\.\\") || input.starts_with("\\??\\")
}

fn is_absolute_native_path(input: &str) -> bool {
    input.starts_with(r"\\")
        || matches!(
            (input.as_bytes().first(), input.as_bytes().get(1), input.as_bytes().get(2)),
            (Some(letter), Some(b':'), Some(b'\\' | b'/')) if letter.is_ascii_alphabetic()
        )
}

fn rejected_slash(error: ResolveError) -> TargetError {
    TargetError::Rejected(TargetRejection::SlashPath(error))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{BareSlashMode, Context};

    #[test]
    fn mnt_drive_is_local_and_keeps_mount_provenance() {
        let target =
            resolve_user_target("/mnt/c/work/a#b", None::<&Context<'_, [&str]>>, None, None)
                .expect("mount target");
        assert_eq!(target.kind(), TargetKind::LocalDrive);
        assert_eq!(target.native_path(), r"C:\work\a#b");
        assert_eq!(target.file_uri().expect("URI"), "file:///C:/work/a%23b");
        assert_eq!(target.to_wsl_path("Ubuntu"), None);
    }

    #[test]
    fn explicit_distribution_mount_stays_wsl() {
        let distributions: &[&str] = &["Ubuntu"];
        let context = Context {
            registry: distributions,
            mode: BareSlashMode::DistributionList,
            preferred: None,
            wsl_default: None,
        };
        let target = resolve_user_target("/Ubuntu/mnt/c/work", Some(&context), None, None)
            .expect("WSL target");
        assert_eq!(target.kind(), TargetKind::WslDistribution);
        assert_eq!(target.native_path(), r"\\wsl.localhost\Ubuntu\mnt\c\work");
    }

    #[test]
    fn native_and_file_uri_round_trip_unicode_and_reserved_characters() {
        let target =
            resolve_user_target(r"C:\café\100%#", None::<&Context<'_, [&str]>>, None, None)
                .expect("native target");
        let uri = target.file_uri().expect("URI");
        assert_eq!(uri, "file:///C:/caf%C3%A9/100%25%23");
        let reparsed = resolve_user_target(&uri, None::<&Context<'_, [&str]>>, None, None)
            .expect("reparsed target");
        assert_eq!(reparsed.native_path(), target.native_path());
    }

    #[test]
    fn relative_requires_base_and_device_paths_are_rejected() {
        assert_eq!(
            resolve_user_target("new-folder", None::<&Context<'_, [&str]>>, None, None),
            Err(TargetError::NeedsContext)
        );
        assert_eq!(
            resolve_user_target(r"\\?\\C:\\secret", None::<&Context<'_, [&str]>>, None, None),
            Err(TargetError::Rejected(TargetRejection::DevicePath))
        );
    }

    #[test]
    fn http_and_shell_forms_are_not_paths() {
        assert_eq!(
            resolve_user_target(
                "https://example.test/a",
                None::<&Context<'_, [&str]>>,
                None,
                None
            ),
            Err(TargetError::NotHandled)
        );
        assert_eq!(
            resolve_user_target("%USERPROFILE%", None::<&Context<'_, [&str]>>, None, None),
            Err(TargetError::NotHandled)
        );
    }
}
