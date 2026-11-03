//! What the hub accepts as a chat attachment and how it serves one back.
//!
//! `POST /api/v1/sessions/:id/prompt` saves each attachment in the attachments directory and
//! `GET /api/v1/attachments/:filename` serves it from the hub's own origin. The Owner's session
//! token lives on that origin, so a served file must never run as a page there. The rules:
//!
//! - **Upload.** A prompt carries at most [`MAX_ATTACHMENTS_PER_PROMPT`] files, each at most
//!   [`MAX_ATTACHMENT_BYTES`] decoded, all together at most [`MAX_ATTACHMENTS_TOTAL_BYTES`]. A
//!   file whose extension is in [`BLOCKED_UPLOAD_EXTENSIONS`] (markup and script a browser runs,
//!   native executables and installers) is refused. Every check passes before the first byte is
//!   written, so a refused prompt leaves nothing on disk.
//! - **Stored media type.** The hub derives `media_type` from the file extension and the file
//!   content ([`stored_media_type`]); the type the client sent is never stored. Anything the hub
//!   does not recognise is `application/octet-stream`.
//! - **Serving.** Only [`serve_attachment`] answers the route. Every response carries
//!   `X-Content-Type-Options: nosniff` and `Content-Security-Policy: default-src 'none';
//!   sandbox`. Four image types, plain text and JSON are served inline with their own type;
//!   everything else (PDF, HTML, SVG, script, XML, unknown) is `application/octet-stream` with
//!   `Content-Disposition: attachment`. An image is served inline only when its first bytes
//!   match its extension.
//! - **Path.** The file that is read is the canonical path of the requested name, and it must be
//!   a file directly inside the canonical attachments directory: a symlink that leaves the
//!   directory is refused. [`read_static_file`] holds the static UI bundle to the same rule and
//!   caps its file size.

use std::fs;
use std::io::Read;
use std::path::Path;

use serde_json::json;

use crate::api::HttpResponse;

/// The largest single attachment, decoded. A base64 string of this file is about 21.3 MiB, so
/// one maximal file still fits in the HTTP request cap ([`crate::ws::MAX_HTTP_REQUEST_BYTES`]).
pub(crate) const MAX_ATTACHMENT_BYTES: usize = 16 * 1024 * 1024;

/// The most decoded bytes one prompt may attach. This is what fits in the HTTP request cap
/// after base64 (4 characters carry 3 bytes), so the limit holds for a caller that reaches the
/// handler without the HTTP reader.
pub(crate) const MAX_ATTACHMENTS_TOTAL_BYTES: usize = crate::ws::MAX_HTTP_REQUEST_BYTES / 4 * 3;

/// The most files one prompt may attach.
pub(crate) const MAX_ATTACHMENTS_PER_PROMPT: usize = 16;

/// The largest static UI file the hub reads into memory to serve.
pub(crate) const MAX_STATIC_FILE_BYTES: u64 = 32 * 1024 * 1024;

const _: () = assert!(MAX_ATTACHMENT_BYTES <= MAX_ATTACHMENTS_TOTAL_BYTES);

/// Extensions (lower case, no dot) the hub refuses to store as an attachment.
///
/// - Markup and script a browser executes when it renders the file: `html htm xhtml xht shtml
///   mht mhtml svg svgz xml xsl xslt js mjs cjs`.
/// - Native executables, scripts and installers a person may run by opening the file: `exe dll
///   com scr bat cmd msi msp sh bash ps1 psm1 vbs vbe jse wsf hta jar app apk lnk cpl reg pif
///   swf`.
///
/// Source files in these formats cannot be attached to a chat prompt; paste them in the prompt
/// or put them in the workspace.
pub(crate) const BLOCKED_UPLOAD_EXTENSIONS: &[&str] = &[
    "html", "htm", "xhtml", "xht", "shtml", "mht", "mhtml", "svg", "svgz", "xml", "xsl", "xslt",
    "js", "mjs", "cjs", "exe", "dll", "com", "scr", "bat", "cmd", "msi", "msp", "sh", "bash",
    "ps1", "psm1", "vbs", "vbe", "jse", "wsf", "hta", "jar", "app", "apk", "lnk", "cpl", "reg",
    "pif", "swf",
];

/// The media type of an unrecognised or active attachment.
pub(crate) const OCTET_STREAM: &str = "application/octet-stream";

/// The `Content-Security-Policy` of every served attachment: no sub-resource loads, no script,
/// and an opaque origin even if a browser were to render the file.
const ATTACHMENT_CSP: &str = "default-src 'none'; sandbox";

/// The extension of `name` in lower case, without trailing dots or spaces: `"A.PNG "` is `png`.
pub(crate) fn extension_of(name: &str) -> Option<String> {
    let trimmed = name.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let (stem, ext) = trimmed.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// The blocked extension of an upload name, if it has one.
pub(crate) fn blocked_upload_extension(name: &str) -> Option<String> {
    let ext = extension_of(name)?;
    BLOCKED_UPLOAD_EXTENSIONS
        .contains(&ext.as_str())
        .then_some(ext)
}

/// The media type an extension stands for. `None` for any extension the hub does not recognise.
fn type_for_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "md" | "log" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "csv" => "text/csv; charset=utf-8",
        "pdf" => "application/pdf",
        _ => return None,
    })
}

/// Whether the first bytes of an image agree with its type.
fn image_magic_matches(media_type: &str, bytes: &[u8]) -> bool {
    match media_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => true,
    }
}

/// The media type for a file: from its extension, with an image's content checked against it.
/// `None` when the extension is unknown or an image's content does not match.
fn recognised_type(name: &str, bytes: &[u8]) -> Option<&'static str> {
    let media_type = type_for_extension(&extension_of(name)?)?;
    image_magic_matches(media_type, bytes).then_some(media_type)
}

/// The `media_type` stored with an upload. The client's own value is not an input: it is
/// whatever the extension and content say, or `application/octet-stream`.
pub(crate) fn stored_media_type(saved_name: &str, bytes: &[u8]) -> String {
    recognised_type(saved_name, bytes)
        .unwrap_or(OCTET_STREAM)
        .to_string()
}

/// Whether a recognised type is safe to show inside the page that fetched it. Images, plain text
/// and JSON are inert. PDF is not inline: a browser's PDF viewer does not render under the
/// sandbox CSP, and a PDF viewer is itself active content.
fn is_inline_type(media_type: &str) -> bool {
    media_type.starts_with("image/")
        || media_type.starts_with("text/plain")
        || media_type == "application/json"
}

/// A file name that is safe inside a quoted `Content-Disposition` value: ASCII letters, digits,
/// `.`, `-` and `_` only (a quote, backslash, CR, LF or non-ASCII character becomes `_`).
pub(crate) fn header_safe_filename(name: &str) -> String {
    let safe: String = name
        .chars()
        .take(120)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "attachment".to_string()
    } else {
        safe
    }
}

/// The response for the bytes of the attachment `file_name`: the inert types inline, the rest as
/// a download. See the module docs.
pub(crate) fn attachment_response(file_name: &str, bytes: Vec<u8>) -> HttpResponse {
    let safe_name = header_safe_filename(file_name);
    let response = match recognised_type(file_name, &bytes).filter(|t| is_inline_type(t)) {
        Some(media_type) => HttpResponse::bytes(200, media_type, bytes).with_header(
            "Content-Disposition",
            format!("inline; filename=\"{safe_name}\""),
        ),
        None => HttpResponse::bytes(200, OCTET_STREAM, bytes).with_header(
            "Content-Disposition",
            format!("attachment; filename=\"{safe_name}\""),
        ),
    };
    response
        .with_header("X-Content-Type-Options", "nosniff")
        .with_header("Content-Security-Policy", ATTACHMENT_CSP)
}

/// Read the regular file `candidate` if it is directly inside `dir` once symlinks are resolved,
/// and at most `max_bytes` long.
///
/// - `Ok(None)`: `dir` or `candidate` does not exist or `candidate` is not a regular file.
/// - `Err(response)`: the file resolves outside `dir` (403), or is larger than `max_bytes`
///   (413), or cannot be read (500).
fn read_confined(
    dir: &Path,
    candidate: &Path,
    max_bytes: u64,
    nested: bool,
) -> Result<Option<(std::path::PathBuf, Vec<u8>)>, HttpResponse> {
    let Ok(root) = fs::canonicalize(dir) else {
        return Ok(None);
    };
    let Ok(canonical) = fs::canonicalize(candidate) else {
        return Ok(None);
    };
    let inside = if nested {
        canonical.starts_with(&root)
    } else {
        canonical.parent() == Some(root.as_path())
    };
    if !inside {
        return Err(HttpResponse::json(
            403,
            json!({ "error": "file resolves outside its directory" }),
        ));
    }
    let Ok(file) = fs::File::open(&canonical) else {
        return Ok(None);
    };
    let Ok(meta) = file.metadata() else {
        return Ok(None);
    };
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() > max_bytes {
        return Err(HttpResponse::json(
            413,
            json!({
                "error": format!("file is larger than the {max_bytes}-byte serving limit"),
                "limit_bytes": max_bytes,
            }),
        ));
    }
    // Read one byte past the limit so a file that grew after the stat is still caught.
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    if let Err(e) = file.take(max_bytes + 1).read_to_end(&mut bytes) {
        return Err(HttpResponse::json(
            500,
            json!({ "error": format!("failed to read file: {e}") }),
        ));
    }
    if bytes.len() as u64 > max_bytes {
        return Err(HttpResponse::json(
            413,
            json!({
                "error": format!("file is larger than the {max_bytes}-byte serving limit"),
                "limit_bytes": max_bytes,
            }),
        ));
    }
    Ok(Some((canonical, bytes)))
}

/// The response of `GET /api/v1/attachments/:filename` for the (already validated) bare
/// `filename` in the attachments directory `dir`.
pub(crate) fn serve_attachment(dir: &Path, filename: &str) -> HttpResponse {
    match read_confined(dir, &dir.join(filename), MAX_ATTACHMENT_BYTES as u64, false) {
        Ok(Some((canonical, bytes))) => {
            let name = canonical
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(filename);
            attachment_response(name, bytes)
        }
        Ok(None) => HttpResponse::json(404, json!({ "error": "attachment not found" })),
        Err(refusal) => refusal,
    }
}

/// Read the static UI file `candidate` for the bundle directory `ui_dir`: `Ok(None)` when it is
/// not a file there (the caller falls back to `index.html`), `Err(response)` when it resolves
/// outside `ui_dir` (403) or is larger than [`MAX_STATIC_FILE_BYTES`] (413).
pub(crate) fn read_static_file(
    ui_dir: &Path,
    candidate: &Path,
) -> Result<Option<Vec<u8>>, HttpResponse> {
    read_confined(ui_dir, candidate, MAX_STATIC_FILE_BYTES, true)
        .map(|found| found.map(|(_, bytes)| bytes))
}

/// The ways a prompt's attachments can be refused before anything is written.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttachmentRefusal {
    TooMany { count: usize },
    BlockedExtension { name: String, ext: String },
    FileTooLarge { name: String },
    TotalTooLarge,
}

impl AttachmentRefusal {
    /// The HTTP response for this refusal.
    pub(crate) fn into_response(self) -> HttpResponse {
        match self {
            Self::TooMany { count } => HttpResponse::json(
                413,
                json!({
                    "error": format!(
                        "too many attachments ({count}): at most {MAX_ATTACHMENTS_PER_PROMPT} per prompt"
                    ),
                    "limit_count": MAX_ATTACHMENTS_PER_PROMPT,
                }),
            ),
            Self::BlockedExtension { name, ext } => HttpResponse::json(
                415,
                json!({
                    "error": format!(
                        "attachment `{name}` has a blocked file type (.{ext}): the hub does not accept markup, script or executable files"
                    ),
                    "name": name,
                }),
            ),
            Self::FileTooLarge { name } => HttpResponse::json(
                413,
                json!({
                    "error": format!(
                        "attachment `{name}` is larger than the {MAX_ATTACHMENT_BYTES}-byte limit per file"
                    ),
                    "limit_bytes": MAX_ATTACHMENT_BYTES,
                    "name": name,
                }),
            ),
            Self::TotalTooLarge => HttpResponse::json(
                413,
                json!({
                    "error": format!(
                        "attachments are larger than the {MAX_ATTACHMENTS_TOTAL_BYTES}-byte limit per prompt"
                    ),
                    "limit_bytes": MAX_ATTACHMENTS_TOTAL_BYTES,
                }),
            ),
        }
    }
}

/// The most base64 characters (padding included) a decoded file of `MAX_ATTACHMENT_BYTES` can
/// take; a longer string is refused before it is decoded.
pub(crate) const MAX_ATTACHMENT_BASE64_CHARS: usize = MAX_ATTACHMENT_BYTES.div_ceil(3) * 4;

/// Check the number of attachments and each name before any decoding.
pub(crate) fn check_attachment_names<'a>(
    names: impl ExactSizeIterator<Item = &'a str>,
) -> Result<(), AttachmentRefusal> {
    if names.len() > MAX_ATTACHMENTS_PER_PROMPT {
        return Err(AttachmentRefusal::TooMany { count: names.len() });
    }
    for name in names {
        if let Some(ext) = blocked_upload_extension(name) {
            return Err(AttachmentRefusal::BlockedExtension {
                name: name.to_string(),
                ext,
            });
        }
    }
    Ok(())
}

/// Add one decoded file's size to the running total and check both limits.
pub(crate) fn check_attachment_size(
    name: &str,
    len: usize,
    total: &mut usize,
) -> Result<(), AttachmentRefusal> {
    if len > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentRefusal::FileTooLarge {
            name: name.to_string(),
        });
    }
    *total = total.saturating_add(len);
    if *total > MAX_ATTACHMENTS_TOTAL_BYTES {
        return Err(AttachmentRefusal::TotalTooLarge);
    }
    Ok(())
}
