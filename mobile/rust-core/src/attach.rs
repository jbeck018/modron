//! Composer attachments (PARITY item 10): turn picked image bytes into the
//! wire [`Attachment`] a submit carries, applying the same type rules as the
//! old terminal client and the CLI, and the size bound the server enforces.
//!
//! Sources of the limits (no client or server code checks anything else):
//! - Types: `crates/modron-tui/src/lib.rs` `attach_image` maps the extension
//!   to png, jpeg, gif or webp; `crates/modron-cli/src/main.rs`
//!   `load_attachment` does the same and also checks the magic bytes. The
//!   bytes must be non-empty.
//! - Size: the server reads each websocket message whole, up to 256 MiB
//!   (`crates/modron-server/src/ws.rs`, `max_message_size`). The image is
//!   base64 inside the submit JSON, so its encoded length must fit that
//!   message with room left for the envelope and the task text. Neither the
//!   old TUI nor the server caps an image lower; provider-side limits are
//!   applied by the provider, not here.

use modron_types::Attachment;

/// Image media types a submit may carry.
pub const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Largest whole websocket message the server accepts (ws.rs).
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;

/// Room kept inside that message for the JSON envelope and the task text.
pub const ENVELOPE_HEADROOM_BYTES: usize = 64 * 1024;

/// Largest base64 payload one attachment may carry in a submit.
pub const MAX_ATTACHMENT_ENCODED_BYTES: usize = MAX_MESSAGE_BYTES - ENVELOPE_HEADROOM_BYTES;

/// Build a wire image attachment from picked bytes.
///
/// `mime` is the declared type (parameters and case are ignored, and
/// `image/jpg` is read as `image/jpeg`). The bytes must start with the
/// signature of that type. Errors are short, user-facing sentences.
pub fn attachment_from_bytes(name: &str, mime: &str, bytes: &[u8]) -> Result<Attachment, String> {
    let base_type = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let media_type = match base_type.as_str() {
        "image/jpg" => "image/jpeg",
        other => other,
    };
    if !IMAGE_MEDIA_TYPES.contains(&media_type) {
        return Err(format!(
            "{name} is {mime}; attach a png, jpeg, gif or webp image"
        ));
    }
    if bytes.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if !has_signature(media_type, bytes) {
        return Err(format!("{name} does not contain {media_type} data"));
    }
    // Checked from the raw length, before any encoding work.
    let encoded_len = bytes.len().div_ceil(3) * 4;
    if encoded_len > MAX_ATTACHMENT_ENCODED_BYTES {
        return Err(format!(
            "{name} is too large: {} bytes encode to {encoded_len}, the limit is {MAX_ATTACHMENT_ENCODED_BYTES} encoded bytes",
            bytes.len()
        ));
    }
    Ok(Attachment::Image {
        media_type: media_type.to_string(),
        data: base64_encode(bytes),
    })
}

/// The magic bytes each accepted image type starts with (the CLI's
/// `load_attachment` check, per type).
fn has_signature(media_type: &str, bytes: &[u8]) -> bool {
    match media_type {
        "image/png" => bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => bytes.starts_with(b"GIF8"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

/// Standard base64 with padding (RFC 4648), the form `Attachment::Image`
/// carries (no data: prefix).
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        bytes[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        bytes
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn accepts_each_image_type_with_its_signature() {
        let cases: [(&str, &[u8]); 4] = [
            ("image/png", &[0x89, b'P', b'N', b'G', 1]),
            ("image/jpeg", &[0xFF, 0xD8, 0xFF, 0xE0]),
            ("image/gif", b"GIF89a"),
            ("image/webp", b"RIFF\x00\x00\x00\x00WEBPVP8 "),
        ];
        for (mime, bytes) in cases {
            match attachment_from_bytes("pic", mime, bytes).expect(mime) {
                Attachment::Image { media_type, data } => {
                    assert_eq!(media_type, mime);
                    assert_eq!(data, base64_encode(bytes));
                }
                other => panic!("expected an image, got {other:?}"),
            }
        }
    }

    #[test]
    fn normalizes_the_declared_type() {
        let attachment =
            attachment_from_bytes("pic", "IMAGE/JPG; charset=binary", &[0xFF, 0xD8, 0xFF])
                .expect("jpg alias");
        assert!(matches!(
            attachment,
            Attachment::Image { ref media_type, .. } if media_type == "image/jpeg"
        ));
    }

    #[test]
    fn refuses_unsupported_types_empty_and_mislabelled_bytes() {
        let unsupported = attachment_from_bytes("notes.txt", "text/plain", b"hi").unwrap_err();
        assert!(
            unsupported.contains("png, jpeg, gif or webp"),
            "{unsupported}"
        );
        assert!(attachment_from_bytes("pic", "image/png", b"").is_err());
        let mislabelled = attachment_from_bytes("pic", "image/png", b"GIF89a").unwrap_err();
        assert_eq!(mislabelled, "pic does not contain image/png data");
    }

    #[test]
    fn size_limit_is_checked_before_encoding() {
        // The largest raw size whose encoding fits the limit is accepted;
        // one byte more is refused with the limit named.
        let max_raw = MAX_ATTACHMENT_ENCODED_BYTES / 4 * 3;
        assert!(attachment_from_bytes("big", "image/png", &png(max_raw)).is_ok());
        let err = attachment_from_bytes("big", "image/png", &png(max_raw + 4)).unwrap_err();
        assert!(err.starts_with("big is too large"), "{err}");
        assert!(
            err.contains(&MAX_ATTACHMENT_ENCODED_BYTES.to_string()),
            "{err}"
        );
    }
}
