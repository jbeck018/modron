//! Clipboard harvest for image paste (Phase 14, ctrl+V in the TUI).
//!
//! Terminals do not deliver clipboard IMAGES on ctrl+V — so modron asks
//! the OS itself: on macOS a zero-dependency JXA (AppleScript-JavaScript)
//! bridge reads the pasteboard (PNG first, TIFF converted to PNG); on
//! Linux `wl-paste` then `xclip` serve Wayland and X. Text fallback:
//! when the clipboard holds no image, ctrl+V pastes the TEXT into the
//! input instead (never a submit).
//!
//! Everything is a bounded subprocess (2s) — a hung clipboard service
//! must never hang the TUI.

use std::path::PathBuf;
use std::time::Duration;

/// What ctrl+V found.
#[derive(Debug)]
pub enum ClipboardContent {
    Image(PathBuf),
    Text(String),
    Empty,
}

/// Read the clipboard: image first, then text.
pub fn harvest() -> ClipboardContent {
    if let Some(path) = harvest_image() {
        return ClipboardContent::Image(path);
    }
    match harvest_text() {
        Some(text) if !text.is_empty() => ClipboardContent::Text(text),
        _ => ClipboardContent::Empty,
    }
}

/// Try to write the clipboard image to `$TMPDIR/modron-paste-<ts>.png`.
fn harvest_image() -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "modron-paste-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    if run_bounded(Duration::from_secs(2), image_command(&path)).is_some() {
        let meta = std::fs::metadata(&path).ok()?;
        if meta.len() > 64 {
            return Some(path);
        }
    }
    None
}

/// macOS: JXA reads the pasteboard — PNG directly, TIFF converted via
/// NSBitmapImageRep. Linux: wl-paste, then xclip.
fn image_command(out: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c");
    if cfg!(target_os = "macos") {
        let script = format!(
            "osascript -l JavaScript -e ' \
             ObjC.import(\"AppKit\"); ObjC.import(\"stdlib\"); \
             const pb = $.NSPasteboard.generalPasteboard; \
             const out = \"{}\"; \
             let png = pb.dataForType($.NSPasteboardTypePNG); \
             if (!png) {{ \
               const tiff = pb.dataForType($.NSPasteboardTypeTIFF); \
               if (tiff) {{ \
                 const rep = $.NSBitmapImageRep.imageRepWithData(tiff); \
                 if (rep) {{ png = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $()); }} \
               }} \
             }} \
             if (png && png.writeToFileAtomically(out, true)) {{ $.exit(0) }} else {{ $.exit(1) }}'",
            out.display()
        );
        cmd.arg(script);
    } else {
        cmd.arg(format!(
            "wl-paste --type image/png > \"{p}\" 2>/dev/null || \
             xclip -selection clipboard -t image/png -o > \"{p}\" 2>/dev/null",
            p = out.display()
        ));
    }
    cmd
}

fn harvest_text() -> Option<String> {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c");
    if cfg!(target_os = "macos") {
        cmd.arg("pbpaste 2>/dev/null");
    } else {
        cmd.arg("wl-paste 2>/dev/null || xclip -selection clipboard -o 2>/dev/null");
    }
    let out = run_bounded(Duration::from_secs(1), cmd)?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run with a hard timeout; None on failure or timeout (status is ignored —
/// callers check side effects).
fn run_bounded(timeout: Duration, mut cmd: std::process::Command) -> Option<std::process::Output> {
    // std::process has no timeout; approximate with wait_timeout-free spawn
    // + poll via try_wait (the commands are local and fast; 2s is the wall).
    use std::io::Read as _;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let mut stdout = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut stdout);
    }
    let _ = child.wait();
    Some(std::process::Output {
        status: std::process::ExitStatus::default(),
        stdout,
        stderr: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn harvest_on_macos_never_panics() {
        // Whatever is (or is not) on the pasteboard, this must return fast
        // and not hang. With a text clipboard this is Text; with an image,
        // Image; empty clipboard, Empty.
        let started = std::time::Instant::now();
        let content = harvest();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "harvest must be bounded"
        );
        match content {
            ClipboardContent::Text(t) => assert!(!t.is_empty()),
            ClipboardContent::Image(p) => assert!(p.is_file(), "{p:?}"),
            ClipboardContent::Empty => {}
        }
    }

    #[test]
    fn image_command_targets_the_requested_path() {
        let out = std::path::Path::new("/tmp/probe.png");
        let cmd = image_command(out);
        let arg = cmd
            .get_args()
            .last()
            .and_then(|a| a.to_str())
            .unwrap_or_default();
        assert!(arg.contains("probe.png"), "{arg}");
    }
}

