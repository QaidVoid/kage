//! Turn image bytes (a file, a drag-drop path, or the OS clipboard)
//! into a [`kage_core::Content::Image`]-ready attachment.
//!
//! The model/provider/session layers already handle image content;
//! this module is just the acquisition+encoding step the TUI input
//! needs. It deliberately uses no image crate: the format is sniffed
//! from magic bytes (PNG/JPEG/GIF/WebP - what the providers accept)
//! and the payload is base64 of the original bytes, unmodified.

use std::path::Path;

use base64::Engine as _;
use kage_core::ImageSource;

/// Largest image accepted, in bytes. Beyond this the base64 payload
/// bloats the request and providers reject it anyway; refuse early
/// with a clear message instead of failing mid-turn.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// One image queued on the prompt, ready to become a
/// [`kage_core::Content::Image`] block on submit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachedImage {
    /// Inline base64 source handed to the provider.
    pub source: ImageSource,
    /// Sniffed MIME type (e.g. `image/png`).
    pub mime: String,
    /// Short human label for the input/conversation placeholder
    /// (a filename, or `clipboard`).
    pub label: String,
    /// Decoded byte length, shown in the placeholder.
    pub bytes: usize,
}

impl AttachedImage {
    /// `[image: image/png, 42 KB (shot.png)]`-style one-liner shown
    /// in the input chrome and the conversation so an attachment is
    /// never silent.
    #[must_use]
    pub fn placeholder(&self) -> String {
        format!(
            "[image: {}, {} ({})]",
            self.mime,
            human_bytes(self.bytes),
            self.label
        )
    }
}

/// Sniff a provider-supported image MIME from the leading bytes.
/// `None` for anything else (callers reject it rather than guess).
#[must_use]
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Build an attachment from raw image bytes (clipboard or file).
///
/// # Errors
///
/// Returns a user-facing message when the data is empty, larger than
/// [`MAX_IMAGE_BYTES`], or not a recognised image format.
pub fn from_bytes(bytes: &[u8], label: impl Into<String>) -> Result<AttachedImage, String> {
    if bytes.is_empty() {
        return Err("image is empty".to_owned());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image is {} (max {})",
            human_bytes(bytes.len()),
            human_bytes(MAX_IMAGE_BYTES)
        ));
    }
    let mime = sniff_mime(bytes)
        .ok_or_else(|| "unrecognised image format (need png/jpeg/gif/webp)".to_owned())?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(AttachedImage {
        source: ImageSource::Base64 { data },
        mime: mime.to_owned(),
        label: label.into(),
        bytes: bytes.len(),
    })
}

/// Read an image file and build an attachment, labelled by file name.
///
/// # Errors
///
/// Returns a user-facing message when the file cannot be read or
/// [`from_bytes`] rejects its contents.
pub fn load_path(path: &Path) -> Result<AttachedImage, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let label = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image")
        .to_owned();
    from_bytes(&bytes, label)
}

/// Whether `text` is a plausible single filesystem path to an
/// existing image (used to treat a pasted/dragged path as an image
/// rather than literal prompt text). Tolerates surrounding quotes
/// and a leading `file://`, and backslash-escaped spaces from
/// drag-drop.
#[must_use]
pub fn path_if_image(text: &str) -> Option<std::path::PathBuf> {
    let t = text.trim();
    if t.is_empty() || t.contains('\n') {
        return None;
    }
    let unquoted = t
        .trim_matches(['"', '\''])
        .strip_prefix("file://")
        .unwrap_or_else(|| t.trim_matches(['"', '\'']));
    let cleaned = unquoted.replace("\\ ", " ");
    let path = std::path::PathBuf::from(&cleaned);
    let is_image_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"));
    if is_image_ext && path.is_file() {
        Some(path)
    } else {
        None
    }
}

/// One clipboard-image command: program + args, expected to emit the
/// raw image bytes on stdout.
struct ClipCmd {
    program: &'static str,
    args: &'static [&'static str],
}

/// Pick the clipboard-image reader for the current platform, given
/// which Linux display servers look available. Returns `None` when
/// no probe makes sense (e.g. headless Linux), so a normal text
/// paste never pays for a doomed subprocess. Split out from
/// [`clipboard_image`] so the selection logic is unit-testable
/// without a real clipboard.
fn clip_cmd(wayland: bool, x11: bool) -> Option<ClipCmd> {
    if cfg!(target_os = "macos") {
        // `pngpaste -` writes the clipboard PNG to stdout. Absent ->
        // spawn fails -> fall through to path/text (documented).
        Some(ClipCmd {
            program: "pngpaste",
            args: &["-"],
        })
    } else if cfg!(target_os = "windows") {
        Some(ClipCmd {
            program: "powershell",
            args: &[
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
                 $i=[Windows.Forms.Clipboard]::GetImage(); \
                 if($i){ $o=[Console]::OpenStandardOutput(); \
                 $i.Save($o,[Drawing.Imaging.ImageFormat]::Png); $o.Close() }",
            ],
        })
    } else if wayland {
        Some(ClipCmd {
            program: "wl-paste",
            args: &["--no-newline", "--type", "image/png"],
        })
    } else if x11 {
        Some(ClipCmd {
            program: "xclip",
            args: &["-selection", "clipboard", "-t", "image/png", "-o"],
        })
    } else {
        None
    }
}

/// Best-effort read of an image from the OS clipboard via a platform
/// CLI helper (no extra crates). Returns the raw bytes if a helper
/// produced any; the caller still runs them through [`from_bytes`],
/// so a helper that emits non-image data (text clipboard) is
/// naturally rejected and the caller falls back to path/text. Any
/// failure - missing tool, error exit, empty output - is `None`,
/// never a hard error: clipboard image paste is a convenience, not a
/// guarantee, and must not break a normal text paste.
#[must_use]
pub fn clipboard_image() -> Option<Vec<u8>> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let cmd = clip_cmd(wayland, x11)?;
    let out = std::process::Command::new(cmd.program)
        .args(cmd.args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
}

fn human_bytes(n: usize) -> String {
    if n >= 1024 * 1024 {
        #[allow(clippy::cast_precision_loss)]
        let mb = n as f64 / (1024.0 * 1024.0);
        format!("{mb:.1} MB")
    } else if n >= 1024 {
        format!("{} KB", n / 1024)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];

    #[test]
    fn sniffs_known_formats_and_rejects_others() {
        assert_eq!(sniff_mime(PNG), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0x00]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a....."), Some("image/gif"));
        let webp = b"RIFF\0\0\0\0WEBPVP8 ";
        assert_eq!(sniff_mime(webp), Some("image/webp"));
        assert_eq!(sniff_mime(b"not an image"), None);
        assert_eq!(sniff_mime(b""), None);
    }

    #[test]
    fn from_bytes_encodes_base64_and_sets_mime() {
        let att = from_bytes(PNG, "shot.png").unwrap();
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.label, "shot.png");
        assert_eq!(att.bytes, PNG.len());
        match &att.source {
            ImageSource::Base64 { data } => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap();
                assert_eq!(decoded, PNG);
            }
            ImageSource::Url { .. } => panic!("expected base64"),
        }
        assert!(att.placeholder().contains("image/png"));
        assert!(att.placeholder().contains("shot.png"));
    }

    #[test]
    fn from_bytes_rejects_empty_oversize_and_unknown() {
        assert!(from_bytes(&[], "x").is_err());
        assert!(
            from_bytes(b"plain text data", "x")
                .unwrap_err()
                .contains("unrecognised")
        );
        let big = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert!(from_bytes(&big, "x").unwrap_err().contains("max"));
    }

    #[test]
    fn load_path_round_trips_a_real_file() {
        let dir = std::env::temp_dir().join(format!("kage-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("pic.png");
        std::fs::write(&p, PNG).unwrap();
        let att = load_path(&p).unwrap();
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.label, "pic.png");
        assert_eq!(path_if_image(p.to_str().unwrap()), Some(p.clone()));
        assert_eq!(
            path_if_image(&format!("\"{}\"", p.display())),
            Some(p.clone())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn clip_cmd_picks_linux_tool_by_display_and_skips_headless() {
        assert_eq!(clip_cmd(true, false).map(|c| c.program), Some("wl-paste"));
        assert_eq!(clip_cmd(false, true).map(|c| c.program), Some("xclip"));
        // Wayland wins when both are set.
        assert_eq!(clip_cmd(true, true).map(|c| c.program), Some("wl-paste"));
        // Headless: no probe, so a text paste pays no subprocess.
        assert!(clip_cmd(false, false).is_none());
    }

    #[test]
    fn path_if_image_rejects_non_images_and_text() {
        assert_eq!(path_if_image("just a sentence"), None);
        assert_eq!(path_if_image("/nope/missing.png"), None);
        assert_eq!(path_if_image("line one\nline two"), None);
    }
}
