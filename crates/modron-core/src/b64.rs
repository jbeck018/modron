//! Minimal base64 (standard alphabet, padded) — Phase-1b C1. Hand-rolled to
//! honor the no-new-dependencies rule; used only for checkpoint file
//! snapshots, never for wire formats.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as padded base64.
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
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

/// Decode padded base64. Whitespace is ignored. Invalid characters or
/// misplaced padding are errors (the model/CLI must not silently restore
/// garbage).
pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    let body: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !body.len().is_multiple_of(4) {
        return Err(format!(
            "base64: length {} is not a multiple of 4",
            body.len()
        ));
    }
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(body.len() / 4 * 3);
    let mut pad_seen = false;
    for ch in body.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' => {
                if pad_seen {
                    return Err("base64: data after padding".into());
                }
                let v = match ch {
                    'A'..='Z' => ch as u32 - 'A' as u32,
                    'a'..='z' => ch as u32 - 'a' as u32 + 26,
                    '0'..='9' => ch as u32 - '0' as u32 + 52,
                    '+' => 62,
                    _ => 63,
                };
                acc = (acc << 6) | v;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    out.push((acc >> bits) as u8);
                }
            }
            '=' => pad_seen = true,
            other => return Err(format!("base64: invalid character {other:?}")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        for (plain, enc) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
        ] {
            assert_eq!(decode(enc).unwrap(), plain);
        }
    }

    #[test]
    fn all_bytes_roundtrip() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
        // Chunked offsets too.
        for len in 0..=all.len() {
            assert_eq!(decode(&encode(&all[..len])).unwrap(), &all[..len]);
        }
    }

    #[test]
    fn invalid_input_is_an_error() {
        assert!(decode("A").is_err()); // impossible length
        assert!(decode("Zm9v!").is_err()); // invalid character
        assert!(decode("Zg=Z").is_err()); // data after padding
        assert!(decode("Zg==\n").unwrap() == b"f"); // trailing whitespace ok
    }
}
