//! Filename resolution and sanitisation.
//!
//! This module owns the guarantee behind exit criterion **S1-C3**: [`sanitise`] returns a
//! name that is *exactly one normal path component*. Joining such a name to the target
//! directory cannot leave that directory, whatever the server put in `Content-Disposition`.
//!
//! `attachment; filename="../../etc/passwd"` is an ordinary occurrence on the open web, not
//! an exotic attack, and it is handled by construction rather than by inspection.
//!
//! References: RFC 6266 (`Content-Disposition`), RFC 8187 (`filename*` encoding).

use percent_encoding::percent_decode_str;
use url::Url;

/// Used when nothing usable survives resolution. Deliberately extensionless: guessing an
/// extension from a content type would be guessing at the file's identity.
const DEFAULT_NAME: &str = "download";

/// Byte budget for a name. 255 is the limit on ext4, APFS, NTFS and most other filesystems
/// we will meet, and it is a *byte* limit, not a character one.
const MAX_NAME_BYTES: usize = 255;

/// Bytes the storage layer appends while a download is in progress: `.dppart`.
///
/// A name that fits exactly at the limit is still unusable, because the *working* file is
/// `<name>.dppart` and that is what gets created first. Reserving the suffix here rather than
/// at the creation site keeps the guarantee where the budget is: a name this module returns can
/// always be written to disk, in progress and finished.
///
/// Found by `local/suggested-filename-exceeds-the-path-limit`, which truncated to exactly 255
/// bytes and then failed with `ENAMETOOLONG` at 262.
const PART_SUFFIX_BYTES: usize = ".dppart".len();

/// Truncation leaves room for a reserved-name prefix and for the in-progress suffix, so that
/// neither needs a second truncation pass afterwards. That headroom is what keeps [`sanitise`]
/// a single pass, and therefore idempotent.
const MAX_TRUNCATED_BYTES: usize = MAX_NAME_BYTES - 1 - PART_SUFFIX_BYTES;

/// Windows device names. These are unusable as filenames on Windows *even with an extension*
/// — `con.txt` resolves to the console, not a file. A cross-platform download manager must
/// never produce one, including on Linux, because the file may be on a shared volume.
const RESERVED_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters that are illegal in a filename on Windows, plus both path separators. Replaced
/// rather than removed, so two names that differ only in these characters stay different.
const ILLEGAL: [char; 9] = ['<', '>', ':', '"', '|', '?', '*', '/', '\\'];

/// Decide what to call the file, from the server's suggestion and the final URL.
///
/// Precedence, following RFC 6266 §4.3:
///
/// 1. `Content-Disposition`'s `filename*` parameter (percent-encoded, charset-tagged),
/// 2. its `filename` parameter,
/// 3. the last non-empty path segment of the final URL, percent-decoded,
/// 4. `download`.
///
/// A suggestion that sanitises away to nothing falls through to the next source rather than
/// short-circuiting to the default, so `filename=".."` still yields the URL's name.
///
/// ```
/// use downpour_types::filename;
/// let url: url::Url = "https://example.com/dl/file.zip?token=abc".parse()?;
///
/// // No suggestion: the URL path, with the query discarded.
/// assert_eq!(filename::resolve(None, &url), "file.zip");
///
/// // A traversal attempt cannot escape, whatever the header says.
/// let cd = Some(r#"attachment; filename="../../../etc/passwd""#);
/// assert_eq!(filename::resolve(cd, &url), "passwd");
/// # Ok::<(), url::ParseError>(())
/// ```
#[must_use]
pub fn resolve(content_disposition: Option<&str>, url: &Url) -> String {
    if let Some(header) = content_disposition
        && let Some(suggested) = from_content_disposition(header)
        && let Some(name) = clean(&suggested)
    {
        return name;
    }
    if let Some(segment) = from_url_path(url)
        && let Some(name) = clean(&segment)
    {
        return name;
    }
    DEFAULT_NAME.to_owned()
}

/// Reduce an arbitrary string to a single safe path component.
///
/// Guarantees, all asserted by `tests/filename_prop.rs`:
///
/// - the result is exactly one [`std::path::Component::Normal`], so it cannot escape a
///   directory it is joined to;
/// - it is never empty, never `.`, never `..`;
/// - it contains no path separator, no NUL, and no control character;
/// - it is at most 255 bytes and is always valid UTF-8;
/// - it is not a Windows device name;
/// - `sanitise(sanitise(x)) == sanitise(x)`.
///
/// Idempotence matters more than it looks: the name is verified before the `.dppart` file is
/// renamed (I-4), and a name that drifted between the check and the rename would defeat that.
#[must_use]
pub fn sanitise(raw: &str) -> String {
    clean(raw).unwrap_or_else(|| DEFAULT_NAME.to_owned())
}

/// The body of [`sanitise`], returning `None` when nothing usable survives so that
/// [`resolve`] can fall through to its next source.
fn clean(raw: &str) -> Option<String> {
    // 1. Keep only the last path component. Both separators count regardless of platform: a
    //    Windows-style path arriving on Linux is still a path, and stripping only `/` here is
    //    the bug that lets `..\..\x` through.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);

    // 2. Neutralise control characters and everything Windows forbids.
    let replaced: String = base
        .chars()
        .map(|c| {
            if c.is_control() || ILLEGAL.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();

    // 3. Windows silently strips trailing dots and spaces, which turns the name we verified
    //    into a different name on disk. Strip them here so the two always agree. Leading dots
    //    are kept — `.bashrc` is a legitimate filename.
    let trimmed = replaced
        .trim_start_matches(' ')
        .trim_end_matches(['.', ' ']);

    // 4. Fit the byte budget, keeping the extension, before the reserved-name check — so that
    //    a device name exposed *by* truncation is still caught in step 5.
    let sized = if trimmed.len() > MAX_NAME_BYTES {
        truncate_keeping_extension(trimmed)
    } else {
        trimmed.to_owned()
    };

    if sized.is_empty() {
        return None;
    }

    // 5. Reserved device names. Prefixing is enough and preserves the original name, which
    //    matters when the user goes looking for the file.
    Some(if is_reserved_stem(&sized) {
        format!("_{sized}")
    } else {
        sized
    })
}

/// Shorten a name to the byte budget while keeping its extension, because the extension is
/// what tells the user and the operating system what the file is.
fn truncate_keeping_extension(name: &str) -> String {
    let extension = extension_to_keep(name);
    let head_budget = MAX_TRUNCATED_BYTES.saturating_sub(extension.len());
    let head = name.get(..name.len() - extension.len()).unwrap_or("");
    // Truncation can expose a trailing dot or space that step 3 had no reason to remove.
    // Trimming again here is what makes the whole function idempotent.
    let head = truncate_at_char_boundary(head, head_budget).trim_end_matches(['.', ' ']);
    format!("{head}{extension}")
}

/// The trailing extension worth preserving: up to two dot-separated groups, so that
/// `.tar.gz` survives, each short enough to actually be an extension. Anything longer is
/// part of the name, not a suffix, and is truncated with the rest.
fn extension_to_keep(name: &str) -> &str {
    const MAX_GROUP_BYTES: usize = 8;
    let mut start = name.len();
    for _ in 0..2 {
        let head = name.get(..start).unwrap_or("");
        let Some(dot) = head.rfind('.') else { break };
        // A leading dot is the whole name (`.bashrc`), not an extension.
        if dot == 0 {
            break;
        }
        let group = name.get(dot + 1..start).unwrap_or("");
        if group.is_empty() || group.len() > MAX_GROUP_BYTES || group.contains(' ') {
            break;
        }
        start = dot;
    }
    name.get(start..).unwrap_or("")
}

/// Truncate to at most `max_bytes` without splitting a UTF-8 character.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("")
}

/// Whether the part before the first dot is a Windows device name.
fn is_reserved_stem(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    RESERVED_STEMS
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

/// Extract a filename from a `Content-Disposition` header, preferring `filename*`.
fn from_content_disposition(header: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;

    for parameter in split_parameters(header) {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = unquote(value.trim());

        if name.eq_ignore_ascii_case("filename*") {
            if extended.is_none() {
                extended = decode_extended_value(&value);
            }
        } else if name.eq_ignore_ascii_case("filename") && plain.is_none() {
            plain = Some(value);
        }
    }

    extended.or(plain)
}

/// Split on `;` at the top level only — a semicolon inside a quoted string is part of the
/// filename, not a separator.
fn split_parameters(header: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;

    for (index, byte) in header.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_quotes => escaped = true,
            b'"' => in_quotes = !in_quotes,
            b';' if !in_quotes => {
                if let Some(part) = header.get(start..index) {
                    parts.push(part);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if let Some(part) = header.get(start..) {
        parts.push(part);
    }
    parts
}

/// Remove surrounding quotes and resolve `\X` escapes (RFC 9110 §5.6.4 quoted-string).
fn unquote(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_owned();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(escaped) = chars.next() {
                out.push(escaped);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Decode an RFC 8187 `ext-value`: `charset'language'percent-encoded`.
///
/// Only the two charsets the RFC permits are accepted. An unknown charset yields `None`
/// rather than a mojibake filename.
fn decode_extended_value(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?;
    let _language = parts.next()?;
    let encoded = parts.next()?;

    let bytes = percent_decode_str(encoded).collect::<Vec<u8>>();
    if charset.eq_ignore_ascii_case("utf-8") {
        String::from_utf8(bytes).ok()
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        Some(bytes.iter().map(|&b| char::from(b)).collect())
    } else {
        None
    }
}

/// The last non-empty path segment of a URL, percent-decoded. The query string is not part
/// of the name — signed URLs carry tokens there and they are not filenames.
fn from_url_path(url: &Url) -> Option<String> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    let last = segments
        .last()
        .copied()
        .filter(|segment| !segment.is_empty())?;
    Some(percent_decode_str(last).decode_utf8().ok()?.into_owned())
}
