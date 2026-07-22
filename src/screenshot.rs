//! Screen capture for "look at this" — hand a frame to the running session so
//! Joi can see what's on screen.
//!
//! Selection is delegated to KDE's Spectacle rather than reimplemented. The
//! capture itself is trivial (~0.34s measured, and `xcap` would do it
//! in-process), but the *rectangle selection UI* is not: overlay window, drag
//! handles, live dimensions, magnifier, escape-to-cancel, HiDPI, multi-monitor.
//! Spectacle already does all of that, already handles Wayland via portals
//! (which a hand-rolled X11 overlay would not), and is the selection UI the
//! user already has muscle memory for.
//!
//! Everything after selection is ours: downscale and JPEG-encode, because the
//! Live API takes JPEG and a native-resolution screenshot is far more bytes
//! than the model needs to read a dialog.

use std::path::Path;

use anyhow::{Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

/// Longest edge of the frame we send. A 2560x1440 screenshot is ~600KB of PNG
/// and far more detail than is needed to read an error dialog or comment on a
/// layout; downscaling cuts tokens, upload time, and latency with no practical
/// loss. Raised via config if it ever proves too coarse for fine print.
pub const DEFAULT_MAX_EDGE: u32 = 1024;
pub const DEFAULT_QUALITY: u8 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Drag out a rectangle. Blocks until a selection or cancel.
    Region,
    /// Whatever window has focus — no interaction, and narrower than the whole
    /// desktop so less of the screen leaves the machine.
    Window,
    /// The entire desktop.
    Full,
}

impl Mode {
    fn spectacle_flag(self) -> &'static str {
        match self {
            Mode::Region => "--region",
            Mode::Window => "--activewindow",
            Mode::Full => "--fullscreen",
        }
    }
}

/// Capture via Spectacle and return JPEG bytes ready for `send_video`.
///
/// `Ok(None)` means the user cancelled (escaped out of the region selector) —
/// a normal outcome, not an error, and the caller should quietly do nothing
/// rather than sending a stale frame.
pub fn capture(mode: Mode, max_edge: u32, quality: u8) -> Result<Option<Vec<u8>>> {
    let dir = tempfile::tempdir().context("creating temp dir for screenshot")?;
    let shot = dir.path().join("shot.png");

    let status = std::process::Command::new("spectacle")
        .args([
            mode.spectacle_flag(),
            "--background",   // no GUI, just capture and exit
            "--nonotify",     // we do our own notification
            "--no-decoration",
            "--output",
        ])
        .arg(&shot)
        .status()
        .context("running spectacle (is it installed?)")?;

    // Cancelling the region selector exits non-zero and/or writes nothing.
    // Treat both as "user changed their mind".
    if !status.success() || !shot.exists() {
        return Ok(None);
    }
    let png = std::fs::read(&shot).context("reading captured screenshot")?;
    if png.is_empty() {
        return Ok(None);
    }
    Ok(Some(to_jpeg(&png, max_edge, quality)?))
}

/// Decode an image, downscale so its longest edge is at most `max_edge`, and
/// re-encode as JPEG. Split out from `capture` so it's testable without a
/// display server or Spectacle.
pub fn to_jpeg(input: &[u8], max_edge: u32, quality: u8) -> Result<Vec<u8>> {
    let img = image::load_from_memory(input).context("decoding screenshot")?;
    let (w, h) = (img.width(), img.height());

    // Only ever shrink. Upscaling a small selection would add bytes without
    // adding readable detail.
    let img = if w.max(h) > max_edge {
        img.resize(max_edge, max_edge, FilterType::Lanczos3)
    } else {
        img
    };

    // JPEG has no alpha; a screenshot with transparency would otherwise
    // encode with a black background.
    let rgb = img.to_rgb8();

    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode(&rgb, rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
        .context("encoding JPEG")?;
    Ok(out)
}

/// Where a captured frame is parked for the session process to pick up.
/// Signals can't carry a payload, so the frame goes through the filesystem
/// and `SIGUSR2` just says "there's one waiting".
pub fn frame_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(base).join("gemini-assistant-frame.jpg")
}

/// Write atomically so the session can never read a half-written frame.
pub fn write_frame(path: &Path, jpeg: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, jpeg).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).context("renaming frame into place")?;
    Ok(())
}

/// Take the pending frame, if any, removing it so it can't be sent twice.
pub fn take_frame(path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    let _ = std::fs::remove_file(path);
    (!bytes.is_empty()).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn downscales_large_captures_to_max_edge() {
        let jpeg = to_jpeg(&png_of(2560, 1440), 1024, 80).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!(decoded.width(), 1024, "longest edge should be capped");
        assert_eq!(decoded.height(), 576, "aspect ratio should be preserved");
    }

    #[test]
    fn leaves_small_captures_alone() {
        // A tight region selection shouldn't be upscaled — that adds bytes
        // without adding detail.
        let jpeg = to_jpeg(&png_of(300, 200), 1024, 80).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (300, 200));
    }

    /// Deliberately compared against *raw* pixel bytes, not the source PNG:
    /// the synthetic gradient here is unnaturally PNG-friendly, so a
    /// PNG-relative assertion would fail on a purely synthetic artifact while
    /// telling us nothing about real screenshots. (Measured on a real 2560x1440
    /// capture: 594 KB PNG -> 123 KB JPEG at max_edge 1024.)
    #[test]
    fn output_is_a_compressed_jpeg() {
        let jpeg = to_jpeg(&png_of(2560, 1440), 1024, 80).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG SOI marker");

        let decoded = image::load_from_memory(&jpeg).unwrap();
        let raw_rgb_bytes = (decoded.width() * decoded.height() * 3) as usize;
        assert!(
            jpeg.len() < raw_rgb_bytes,
            "JPEG ({}) should be smaller than raw RGB ({raw_rgb_bytes})",
            jpeg.len()
        );
    }

    #[test]
    fn take_frame_consumes_so_a_frame_is_never_sent_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.jpg");
        write_frame(&path, b"fake-jpeg").unwrap();
        assert_eq!(take_frame(&path).as_deref(), Some(&b"fake-jpeg"[..]));
        assert_eq!(take_frame(&path), None, "second take must find nothing");
    }

    #[test]
    fn take_frame_is_none_when_absent() {
        assert_eq!(take_frame(Path::new("/nonexistent/frame.jpg")), None);
    }
}
