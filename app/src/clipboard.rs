//! Clipboard read for image paste (PARITY item 10, Ctrl+V). The same approach
//! as the old terminal (`crates/modron-tui/src/clipboard.rs`): terminals do
//! not carry clipboard images, so the OS tools are asked. Linux: `wl-paste`,
//! then `xclip`. Text: the same tools, and `pbpaste` on macOS. Every call is
//! a bounded subprocess (2 s), so a hung clipboard service cannot hang the
//! app. The commands run through [`Runner`], so tests inject their output.
//!
//! macOS: the image comes from a JavaScript-for-Automation script run by
//! `osascript`, which writes the pasteboard's PNG (TIFF converted) to stdout;
//! text comes from `pbpaste`. The old terminal's bridge
//! (crates/modron-tui/src/clipboard.rs) wrote the same PNG to a file.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Bound on one clipboard tool call.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// What the clipboard held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Harvest {
    /// An image: its file-style name, its media type and the raw bytes.
    Image { name: String, mime: &'static str, bytes: Vec<u8> },
    /// No image; the clipboard's text.
    Text(String),
    /// Neither.
    Empty,
}

/// Runs a clipboard tool and returns its stdout, or None when it fails,
/// is missing, or exceeds [`TIMEOUT`].
pub trait Runner {
    fn stdout(&self, program: &str, args: &[&str]) -> Option<Vec<u8>>;
}

/// The real runner: a child process, killed after [`TIMEOUT`].
pub struct Process;

impl Runner for Process {
    fn stdout(&self, program: &str, args: &[&str]) -> Option<Vec<u8>> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        });
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let buf = reader.join().ok()?;
                    return status.success().then_some(buf);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return None;
                }
            }
        }
    }
}

/// The media type of image bytes, by their signature (PNG, JPEG, GIF, WebP).
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The macOS image read: JXA (`osascript -l JavaScript`) takes the PNG from
/// the pasteboard, or the TIFF converted to PNG, and writes the raw bytes to
/// stdout. No image writes nothing, and the signature check rejects it.
const MACOS_IMAGE_JXA: &str = "ObjC.import('AppKit'); \
    const pb = $.NSPasteboard.generalPasteboard; \
    let png = pb.dataForType('public.png'); \
    if (!png) { \
        const tiff = pb.dataForType('public.tiff'); \
        if (tiff) { \
            const rep = $.NSBitmapImageRep.imageRepWithData(tiff); \
            if (rep) { png = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $()); } \
        } \
    } \
    if (png) { $.NSFileHandle.fileHandleWithStandardOutput.writeData(png); }";

/// The image tools for a platform: macOS asks the JXA bridge, Linux asks
/// wl-paste, then xclip.
fn image_tools(macos: bool) -> &'static [(&'static str, &'static [&'static str])] {
    if macos {
        &[("osascript", &["-l", "JavaScript", "-e", MACOS_IMAGE_JXA])]
    } else {
        &[
            ("wl-paste", &["--type", "image/png"]),
            ("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"]),
        ]
    }
}

fn text_tools(macos: bool) -> &'static [(&'static str, &'static [&'static str])] {
    if macos {
        &[("pbpaste", &[])]
    } else {
        &[("wl-paste", &["--no-newline"]), ("xclip", &["-selection", "clipboard", "-o"])]
    }
}

/// Reads the clipboard through `runner`: an image first (only bytes with an
/// image signature count), then text. Never a submit, never a write.
pub fn harvest_with(runner: &dyn Runner) -> Harvest {
    harvest_with_os(runner, cfg!(target_os = "macos"))
}

/// [`harvest_with`] for a named platform, so both platforms' tool choices
/// are testable on any host.
pub fn harvest_with_os(runner: &dyn Runner, macos: bool) -> Harvest {
    for (program, args) in image_tools(macos) {
        if let Some(bytes) = runner.stdout(program, args) {
            if let Some(mime) = sniff(&bytes) {
                let ext = mime.trim_start_matches("image/");
                return Harvest::Image { name: format!("clipboard.{ext}"), mime, bytes };
            }
        }
    }
    for (program, args) in text_tools(macos) {
        if let Some(bytes) = runner.stdout(program, args) {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if !text.is_empty() {
                return Harvest::Text(text);
            }
        }
    }
    Harvest::Empty
}

/// [`harvest_with`] over the real tools.
pub fn harvest() -> Harvest {
    harvest_with(&Process)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-image";

    /// Answers each tool call from `answer`, keyed on the program name and
    /// its first argument.
    struct Fake(fn(&str, &[&str]) -> Option<Vec<u8>>);

    impl Runner for Fake {
        fn stdout(&self, program: &str, args: &[&str]) -> Option<Vec<u8>> {
            (self.0)(program, args)
        }
    }

    #[test]
    fn an_image_on_the_clipboard_is_harvested_before_text() {
        let fake = Fake(|program, args| match (program, args.first()) {
            ("wl-paste", Some(&"--type")) => Some(PNG.to_vec()),
            ("wl-paste", _) => Some(b"text".to_vec()),
            _ => None,
        });
        match harvest_with(&fake) {
            Harvest::Image { name, mime, bytes } => {
                assert_eq!(name, "clipboard.png");
                assert_eq!(mime, "image/png");
                assert_eq!(bytes, PNG);
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn bytes_without_an_image_signature_fall_back_to_text() {
        let fake = Fake(|program, args| match (program, args.first()) {
            (_, Some(&"--type")) | (_, Some(&"-t")) => Some(b"not an image".to_vec()),
            ("wl-paste", _) => Some(b"hello".to_vec()),
            _ => None,
        });
        assert_eq!(harvest_with(&fake), Harvest::Text("hello".into()));
    }

    #[test]
    fn nothing_on_the_clipboard_is_empty() {
        assert_eq!(harvest_with(&Fake(|_, _| None)), Harvest::Empty);
        assert_eq!(harvest_with(&Fake(|_, _| Some(Vec::new()))), Harvest::Empty);
    }

    #[test]
    fn on_macos_the_image_comes_from_osascript_and_linux_tools_are_not_asked() {
        let fake = Fake(|program, _| match program {
            "osascript" => Some(PNG.to_vec()),
            _ => panic!("{program} must not run on macOS for an image"),
        });
        match harvest_with_os(&fake, true) {
            Harvest::Image { name, mime, bytes } => {
                assert_eq!(name, "clipboard.png");
                assert_eq!(mime, "image/png");
                assert_eq!(bytes, PNG);
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn on_macos_text_without_an_image_comes_from_pbpaste() {
        let fake = Fake(|program, args| match program {
            "osascript" => Some(Vec::new()),
            "pbpaste" => {
                assert!(args.is_empty());
                Some(b"copied words".to_vec())
            }
            _ => None,
        });
        assert_eq!(harvest_with_os(&fake, true), Harvest::Text("copied words".into()));
    }

    #[test]
    fn on_macos_a_failed_image_read_with_no_text_is_empty() {
        assert_eq!(harvest_with_os(&Fake(|_, _| None), true), Harvest::Empty);
    }

    #[test]
    fn signatures_name_the_four_image_types() {
        assert_eq!(sniff(PNG), Some("image/png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"plain text"), None);
    }
}
