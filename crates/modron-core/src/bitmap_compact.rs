//! Bitmap context compaction — the "oh-my-pi pattern".
//!
//! When the engine compacts a transcript, the pruned older conversation is
//! normally preserved only as the summarizer's compact-state text. For
//! vision-capable models we ALSO render the full pruned transcript into one
//! compact pixel-font PNG and attach it to the compact-state message: the
//! model reads the whole pruned history back from a single image attachment
//! (~1-2k tokens) instead of losing it behind a lossy re-summary. The text
//! summary always stays (bitmap is additive), so non-vision turns and old
//! session logs are unaffected.
//!
//! Everything here is pure and deterministic: same text + same width →
//! byte-identical PNG (asserted in tests), which keeps compact states
//! reproducible and cache-friendly.

use crate::Attachment;

// ---------------------------------------------------------------------------
// 5x7 pixel font (ASCII 32-126)
// ---------------------------------------------------------------------------

/// Glyph cell: 5 px wide, 7 px tall.
const GLYPH_W: usize = 5;
const GLYPH_H: usize = 7;
/// Horizontal advance: glyph + 1 px letter spacing.
const ADVANCE: usize = GLYPH_W + 1;
/// Line height: glyph + 1 px descender row.
const LINE_H: usize = GLYPH_H + 1;

/// Hard height cap: a taller transcript does not fit any provider's image
/// budget usefully (and decode cost grows quadratically with area) — return
/// None and let the caller fall back to text-only compaction.
const MAX_HEIGHT_PX: usize = 4096;
/// Total-pixel cap at the engine's bitmap width: bounds encode/decode work
/// even if a caller passes an extreme `width_px`. 1024×4096 = 4 Mi pixels.
const MAX_PIXELS: usize = 1024 * MAX_HEIGHT_PX;

/// The width the engine renders compact-state bitmaps at: 170 chars/line at
/// 6 px advance — dense enough that a ~50 KB transcript stays under the
/// height cap, wide enough that vision OCR reads it comfortably.
pub const COMPACT_BITMAP_WIDTH_PX: u16 = 1024;

/// Built-in 5x7 font for ASCII 32 (`' '`)..=126 (`'~'`), one row of five
/// column bytes per glyph: bit 0 = top pixel, bit 6 = bottom pixel. The
/// classic terminal-glyph layout — hand-rolled to honor the
/// no-new-dependencies rule (`png` aside, which has no font crate anyway).
const FONT: [[u8; GLYPH_W]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00], // ' '
    [0x00, 0x00, 0x5F, 0x00, 0x00], // '!'
    [0x00, 0x07, 0x00, 0x07, 0x00], // '"'
    [0x14, 0x7F, 0x14, 0x7F, 0x14], // '#'
    [0x24, 0x2A, 0x7F, 0x2A, 0x12], // '$'
    [0x23, 0x13, 0x08, 0x64, 0x62], // '%'
    [0x36, 0x49, 0x55, 0x22, 0x50], // '&'
    [0x00, 0x05, 0x03, 0x00, 0x00], // '\''
    [0x00, 0x1C, 0x22, 0x41, 0x00], // '('
    [0x00, 0x41, 0x22, 0x1C, 0x00], // ')'
    [0x14, 0x08, 0x3E, 0x08, 0x14], // '*'
    [0x08, 0x08, 0x3E, 0x08, 0x08], // '+'
    [0x00, 0x50, 0x30, 0x00, 0x00], // ','
    [0x08, 0x08, 0x08, 0x08, 0x08], // '-'
    [0x00, 0x60, 0x60, 0x00, 0x00], // '.'
    [0x20, 0x10, 0x08, 0x04, 0x02], // '/'
    [0x3E, 0x51, 0x49, 0x45, 0x3E], // '0'
    [0x00, 0x42, 0x7F, 0x40, 0x00], // '1'
    [0x42, 0x61, 0x51, 0x49, 0x46], // '2'
    [0x21, 0x41, 0x45, 0x4B, 0x31], // '3'
    [0x18, 0x14, 0x12, 0x7F, 0x10], // '4'
    [0x27, 0x45, 0x45, 0x45, 0x39], // '5'
    [0x3C, 0x4A, 0x49, 0x49, 0x30], // '6'
    [0x01, 0x71, 0x09, 0x05, 0x03], // '7'
    [0x36, 0x49, 0x49, 0x49, 0x36], // '8'
    [0x06, 0x49, 0x49, 0x29, 0x1E], // '9'
    [0x00, 0x36, 0x36, 0x00, 0x00], // ':'
    [0x00, 0x56, 0x36, 0x00, 0x00], // ';'
    [0x08, 0x14, 0x22, 0x41, 0x00], // '<'
    [0x14, 0x14, 0x14, 0x14, 0x14], // '='
    [0x00, 0x41, 0x22, 0x14, 0x08], // '>'
    [0x02, 0x01, 0x51, 0x09, 0x06], // '?'
    [0x32, 0x49, 0x79, 0x41, 0x3E], // '@'
    [0x7E, 0x11, 0x11, 0x11, 0x7E], // 'A'
    [0x7F, 0x49, 0x49, 0x49, 0x36], // 'B'
    [0x3E, 0x41, 0x41, 0x41, 0x22], // 'C'
    [0x7F, 0x41, 0x41, 0x22, 0x1C], // 'D'
    [0x7F, 0x49, 0x49, 0x49, 0x41], // 'E'
    [0x7F, 0x09, 0x09, 0x09, 0x01], // 'F'
    [0x3E, 0x41, 0x49, 0x49, 0x7A], // 'G'
    [0x7F, 0x08, 0x08, 0x08, 0x7F], // 'H'
    [0x00, 0x41, 0x7F, 0x41, 0x00], // 'I'
    [0x20, 0x40, 0x41, 0x3F, 0x01], // 'J'
    [0x7F, 0x08, 0x14, 0x22, 0x41], // 'K'
    [0x7F, 0x40, 0x40, 0x40, 0x40], // 'L'
    [0x7F, 0x02, 0x0C, 0x02, 0x7F], // 'M'
    [0x7F, 0x04, 0x08, 0x10, 0x7F], // 'N'
    [0x3E, 0x41, 0x41, 0x41, 0x3E], // 'O'
    [0x7F, 0x09, 0x09, 0x09, 0x06], // 'P'
    [0x3E, 0x41, 0x51, 0x21, 0x5E], // 'Q'
    [0x7F, 0x09, 0x19, 0x29, 0x46], // 'R'
    [0x46, 0x49, 0x49, 0x49, 0x31], // 'S'
    [0x01, 0x01, 0x7F, 0x01, 0x01], // 'T'
    [0x3F, 0x40, 0x40, 0x40, 0x3F], // 'U'
    [0x1F, 0x20, 0x40, 0x20, 0x1F], // 'V'
    [0x3F, 0x40, 0x38, 0x40, 0x3F], // 'W'
    [0x63, 0x14, 0x08, 0x14, 0x63], // 'X'
    [0x07, 0x08, 0x70, 0x08, 0x07], // 'Y'
    [0x61, 0x51, 0x49, 0x45, 0x43], // 'Z'
    [0x00, 0x7F, 0x41, 0x41, 0x00], // '['
    [0x02, 0x04, 0x08, 0x10, 0x20], // '\\'
    [0x00, 0x41, 0x41, 0x7F, 0x00], // ']'
    [0x04, 0x02, 0x01, 0x02, 0x04], // '^'
    [0x40, 0x40, 0x40, 0x40, 0x40], // '_'
    [0x00, 0x01, 0x02, 0x04, 0x00], // '`'
    [0x20, 0x54, 0x54, 0x54, 0x78], // 'a'
    [0x7F, 0x48, 0x44, 0x44, 0x38], // 'b'
    [0x38, 0x44, 0x44, 0x44, 0x20], // 'c'
    [0x38, 0x44, 0x44, 0x48, 0x7F], // 'd'
    [0x38, 0x54, 0x54, 0x54, 0x18], // 'e'
    [0x08, 0x7E, 0x09, 0x01, 0x02], // 'f'
    [0x0C, 0x52, 0x52, 0x52, 0x3E], // 'g'
    [0x7F, 0x08, 0x04, 0x04, 0x78], // 'h'
    [0x00, 0x44, 0x7D, 0x40, 0x00], // 'i'
    [0x20, 0x40, 0x44, 0x3D, 0x00], // 'j'
    [0x7F, 0x10, 0x28, 0x44, 0x00], // 'k'
    [0x00, 0x41, 0x7F, 0x40, 0x00], // 'l'
    [0x7C, 0x04, 0x18, 0x04, 0x78], // 'm'
    [0x7C, 0x08, 0x04, 0x04, 0x78], // 'n'
    [0x38, 0x44, 0x44, 0x44, 0x38], // 'o'
    [0x7C, 0x14, 0x14, 0x14, 0x08], // 'p'
    [0x08, 0x14, 0x14, 0x18, 0x7C], // 'q'
    [0x7C, 0x08, 0x04, 0x04, 0x08], // 'r'
    [0x48, 0x54, 0x54, 0x54, 0x20], // 's'
    [0x04, 0x3F, 0x44, 0x40, 0x20], // 't'
    [0x3C, 0x40, 0x40, 0x20, 0x7C], // 'u'
    [0x1C, 0x20, 0x40, 0x20, 0x1C], // 'v'
    [0x3C, 0x40, 0x30, 0x40, 0x3C], // 'w'
    [0x44, 0x28, 0x10, 0x28, 0x44], // 'x'
    [0x0C, 0x50, 0x50, 0x50, 0x3C], // 'y'
    [0x44, 0x64, 0x54, 0x4C, 0x44], // 'z'
    [0x00, 0x08, 0x36, 0x41, 0x00], // '{'
    [0x00, 0x00, 0x7F, 0x00, 0x00], // '|'
    [0x00, 0x41, 0x36, 0x08, 0x00], // '}'
    [0x02, 0x01, 0x02, 0x04, 0x02], // '~'
];

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render `text` as a black-on-white 5x7 pixel-font PNG (`image/png`
/// attachment, base64 body — the same shape `load_attachment` emits for
/// user-attached images). Non-ASCII and control characters map to `'?'`;
/// `\n` starts a new line; everything else wraps at
/// `width_px / 6` chars per line. Font is 1x only — density is capped by
/// refusing the render, not by shrinking glyphs (a half-scale 5x7 font is
/// unreadable to vision OCR, which defeats the point).
///
/// Returns `None` when the text is empty, the width cannot fit one glyph,
/// the rendered height would exceed ~4096 px, or the total pixel count
/// exceeds the cap — callers fall back to text-only compaction.
pub fn render_text_bitmap(text: &str, width_px: u16) -> Option<Attachment> {
    if text.is_empty() {
        return None;
    }
    let width = width_px as usize;
    let chars_per_line = width / ADVANCE;
    if chars_per_line == 0 {
        return None; // narrower than one glyph cell: nothing renderable
    }
    let lines = wrap_ascii(text, chars_per_line);
    // Whitespace-only text (spaces, newlines) rasterizes to a blank sheet —
    // no glyph can draw ink, so refuse it like empty text.
    if !lines.iter().any(|line| line.iter().any(|&b| b != b' ')) {
        return None;
    }
    let height = lines.len() * LINE_H;
    if height > MAX_HEIGHT_PX || width.saturating_mul(height) > MAX_PIXELS {
        return None; // oversize: refuse rather than truncate history
    }

    // Grayscale canvas: 255 = paper, 0 = ink. Flat white compresses to
    // almost nothing, so the PNG stays small even at 1024x4096.
    let mut bitmap = vec![255u8; width * height];
    for (line_idx, line) in lines.iter().enumerate() {
        for (char_idx, &byte) in line.iter().enumerate() {
            let glyph = &FONT[(byte - b' ') as usize];
            for (col, &column) in glyph.iter().enumerate() {
                for row in 0..GLYPH_H {
                    if (column & (1 << row)) != 0 {
                        let x = char_idx * ADVANCE + col;
                        let y = line_idx * LINE_H + row;
                        bitmap[y * width + x] = 0;
                    }
                }
            }
        }
    }

    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, width as u32, height as u32);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        // Fixed settings (no adaptive thread/count nondeterminism): the same
        // input must always encode to the same bytes for reproducible
        // compact states.
        encoder.set_compression(png::Compression::Default);
        encoder.set_filter(png::FilterType::Sub);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&bitmap).ok()?;
    }
    Some(Attachment::Image {
        media_type: "image/png".to_string(),
        data: crate::b64::encode(&png_bytes),
    })
}

/// Split ASCII-mapped `text` into lines of at most `budget` glyph cells:
/// `\n` forces a line break, longer runs hard-wrap at the budget (the older
/// transcript is already line-oriented `[TAG] …` text; fancy word wrapping
/// would only shuffle where the breaks land, never save lines).
fn wrap_ascii(text: &str, budget: usize) -> Vec<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = vec![Vec::new()];
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(Vec::new());
            continue;
        }
        // ASCII printable survives verbatim; everything else (non-ASCII,
        // control chars) becomes '?' so the bitmap stays honest about what
        // the font cannot draw.
        let byte = if ch.is_ascii_graphic() || ch == ' ' {
            ch as u8
        } else {
            b'?'
        };
        let current = lines.last_mut().expect("lines starts with one entry");
        if current.len() >= budget {
            lines.push(vec![byte]);
        } else {
            current.push(byte);
        }
    }
    // Drop the empty line a trailing '\n' leaves — it would only buy 8 px
    // of blank height.
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

// ---------------------------------------------------------------------------
// Vision gate
// ---------------------------------------------------------------------------

/// Whether `model` is vision-capable AND its provider encodes message
/// attachments on the wire (anthropic, openai-compat, and — since this
/// follow-up — the chatgpt Responses path). Deliberately a small explicit
/// list, not catalog metadata: unknown models return `false`, so a typo
/// costs a bitmap, never a broken turn. Case-insensitive prefix match
/// covers tier/size suffixes (`glm-5.3-air`). Known limitation: the gate
/// checks the model at compaction time — a later routed non-vision model
/// receives the PNG as an ordinary (ignored) attachment.
pub fn vision_capable(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    ["claude-", "glm-5.3", "gpt-6", "gpt-5.6"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode an attachment's PNG body back to raw pixels (test oracle:
    /// verify ink actually landed, not just that encoding succeeded).
    fn decode_png(attachment: &Attachment) -> (u32, u32, Vec<u8>) {
        let Attachment::Image { media_type, data } = attachment else {
            panic!("expected an image attachment");
        };
        assert_eq!(media_type, "image/png");
        let bytes = crate::b64::decode(data).expect("attachment body is valid base64");
        let decoder = png::Decoder::new(bytes.as_slice());
        let mut reader = decoder.read_info().expect("valid PNG header");
        let mut pixels = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels).expect("decodable PNG frame");
        (info.width, info.height, pixels)
    }

    fn ink_pixels(attachment: &Attachment) -> usize {
        decode_png(attachment).2.iter().filter(|&&p| p == 0).count()
    }

    #[test]
    fn known_string_renders_byte_identical_png() {
        // Determinism is the whole contract: two renders of the same input
        // must produce byte-identical attachments (reproducible compact
        // states, comparable across runs).
        let text = "[TASK] fix the login module\n[USER] please\n";
        let first = render_text_bitmap(text, COMPACT_BITMAP_WIDTH_PX).unwrap();
        let second = render_text_bitmap(text, COMPACT_BITMAP_WIDTH_PX).unwrap();
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap(),
            "same input must encode to the same PNG bytes"
        );
        // And the render carries the expected geometry: 2 text lines × 8 px.
        let (w, h, _) = decode_png(&first);
        assert_eq!(
            (w, h),
            (COMPACT_BITMAP_WIDTH_PX as u32, (2 * LINE_H) as u32)
        );
    }

    #[test]
    fn empty_text_renders_none() {
        assert!(render_text_bitmap("", COMPACT_BITMAP_WIDTH_PX).is_none());
        // Whitespace-only text has no glyph rows either — nothing to read.
        assert!(render_text_bitmap(" \n \n", COMPACT_BITMAP_WIDTH_PX).is_none());
        // A width narrower than one glyph cell (6 px) cannot fit anything.
        assert!(render_text_bitmap("abc", 5).is_none());
    }

    #[test]
    fn oversize_input_renders_none() {
        // 600 wrapped lines × 8 px = 4800 px > the 4096 px cap.
        let line = "a".repeat(COMPACT_BITMAP_WIDTH_PX as usize / ADVANCE);
        let oversize = vec![line.as_str(); 600].join("\n");
        assert!(render_text_bitmap(&oversize, COMPACT_BITMAP_WIDTH_PX).is_none());
        // Just under the cap still renders: 512 lines × 8 px = 4096 px.
        let fitting = vec![line.as_str(); MAX_HEIGHT_PX / LINE_H].join("\n");
        assert!(render_text_bitmap(&fitting, COMPACT_BITMAP_WIDTH_PX).is_some());
        // The total-pixel cap refuses an extreme width even at legal height:
        // 512 lines × 65535 px ≈ 33.5 M pixels > the 4 Mi cap.
        let tall = vec!["a"; MAX_HEIGHT_PX / LINE_H].join("\n");
        assert!(render_text_bitmap(&tall, u16::MAX).is_none());
    }

    #[test]
    fn every_renderable_ascii_byte_lands_ink() {
        // All-ASCII mapping coverage: each printable byte 33..=126 must
        // appear in SOME output, i.e. its glyph draws at least one ink
        // pixel. Space is the one glyph allowed to draw none.
        for byte in b'!'..=b'~' {
            let attachment =
                render_text_bitmap(&(byte as char).to_string(), COMPACT_BITMAP_WIDTH_PX)
                    .unwrap_or_else(|| panic!("glyph {:#04x} must render", byte));
            assert!(
                ink_pixels(&attachment) > 0,
                "glyph {:#04x} ({}) drew no ink",
                byte,
                byte as char
            );
        }
        // The space glyph is the exception — it draws none, proven by ink
        // counts: "a a" (with a space cell) inks exactly as much as "aa".
        assert!(render_text_bitmap(" ", 64).is_none());
        assert_eq!(
            ink_pixels(&render_text_bitmap("a a", 64).unwrap()),
            ink_pixels(&render_text_bitmap("aa", 64).unwrap())
        );
    }

    #[test]
    fn non_ascii_maps_to_question_mark() {
        // The '?' substitution must be exact: a non-ASCII string and its
        // '?'-masked ASCII twin encode to identical bytes.
        let non_ascii = render_text_bitmap("héllo wörld", 64).unwrap();
        let masked = render_text_bitmap("h?llo w?rld", 64).unwrap();
        assert_eq!(
            serde_json::to_string(&non_ascii).unwrap(),
            serde_json::to_string(&masked).unwrap()
        );
    }

    #[test]
    fn wrapping_respects_the_char_budget() {
        // 64 px / 6 px = 10 chars per line: 25 'a's become 3 lines of
        // 10/10/5, i.e. 3 × 8 = 24 px tall (vs 8 px unwrapped).
        let text = "a".repeat(25);
        let (_, h, _) = decode_png(&render_text_bitmap(&text, 64).unwrap());
        assert_eq!(h, 24);
        let (_, h2, _) = decode_png(&render_text_bitmap("aaaaaaaaaa", 64).unwrap());
        assert_eq!(h2, 8, "exactly one budgeted line stays one line");
    }

    #[test]
    fn vision_gate_matches_attachment_encoding_flagships_only() {
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "glm-5.3",
            "glm-5.3-air",
            "GLM-5.3",
            "gpt-6",
            "gpt-6-mini",
            "gpt-5.6-turbo",
        ] {
            assert!(vision_capable(model), "{model} should be vision-capable");
        }
        for model in ["nonvision-model", "mock-model", "small-window-model", ""] {
            assert!(!vision_capable(model), "{model} must not gate a bitmap in");
        }
    }
}
