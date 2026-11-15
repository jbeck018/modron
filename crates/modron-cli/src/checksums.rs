//! SHA-256 verification of release archives against the release's
//! `SHA256SUMS` manifest. The release workflow writes it with
//! `shasum -a 256 modron-*.tar.gz`, one `<hex>  <file name>` line per archive.
//!
//! What this detects: a corrupted or truncated download, and an archive that
//! was replaced in the release while `SHA256SUMS` was left unchanged.
//!
//! What this does NOT detect: someone who can replace both the archive and
//! `SHA256SUMS` in the release. Stopping that needs signed releases, which
//! this project does not have yet. Do not read a matching digest as proof of
//! origin.
//!
//! Format rules (strict, lowercase only): each non-blank line is 64 lowercase
//! hex characters, then two spaces or a space and `*` (binary marker), then a
//! file name with no control characters, `/` or `\`. Any other line makes the
//! whole manifest malformed. Uppercase hex is rejected on purpose: the
//! workflow writes lowercase, and rejecting the other case keeps one spelling.

use anyhow::{anyhow, bail};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Largest `SHA256SUMS` body accepted. A release has a handful of lines.
pub const MAX_SUMS_BYTES: usize = 64 * 1024;

/// A raw SHA-256 digest.
pub type Digest32 = [u8; 32];

/// Parse one manifest line, or `None` when it does not match the strict format.
fn parse_line(line: &str) -> Option<(String, Digest32)> {
    let hex = line.get(..64)?;
    if !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let sep = line.get(64..66)?;
    if sep != "  " && sep != " *" {
        return None;
    }
    let name = &line[66..];
    if name.is_empty()
        || name.starts_with(' ')
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return None;
    }
    let mut digest = [0u8; 32];
    for (slot, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *slot = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Some((name.to_string(), digest))
}

/// Hex digit value; callers have already checked the character class.
fn nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        _ => b - b'a' + 10,
    }
}

/// Parse a whole manifest. Blank lines are ignored; any other malformed line
/// fails the whole manifest, so a corrupt file is never partly trusted.
pub fn parse_sha256sums(text: &str) -> anyhow::Result<Vec<(String, Digest32)>> {
    let mut entries = Vec::new();
    for (idx, line) in text.split('\n').enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry = parse_line(line).ok_or_else(|| {
            anyhow!(
                "malformed SHA256SUMS line {}: expected `<64 lowercase hex>  <file name>`",
                idx + 1
            )
        })?;
        entries.push(entry);
    }
    if entries.is_empty() {
        bail!("SHA256SUMS has no entries - refusing to install an unverified archive");
    }
    Ok(entries)
}

/// The one digest the manifest lists for `file`. Unrelated entries are fine.
/// An unlisted file or a duplicate entry is an error.
pub fn expected_digest(text: &str, file: &str) -> anyhow::Result<Digest32> {
    let entries = parse_sha256sums(text)?;
    let mut matches = entries.iter().filter(|(name, _)| name == file);
    let first = matches.next().ok_or_else(|| {
        anyhow!("SHA256SUMS does not list `{file}` - refusing to install an unverified archive")
    })?;
    if matches.next().is_some() {
        bail!("SHA256SUMS lists `{file}` more than once - refusing to choose a digest");
    }
    Ok(first.1)
}

/// Compare two digests without an early exit on the first differing byte.
pub fn digests_equal(a: &Digest32, b: &Digest32) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Lowercase hex form of a digest.
pub fn to_hex(digest: &Digest32) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a file, read in fixed-size blocks (the archive is never held
/// in memory whole).
pub fn sha256_file(path: &Path) -> std::io::Result<Digest32> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut block = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        hasher.update(&block[..n]);
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&hasher.finalize());
    Ok(digest)
}

/// Check the archive at `archive` against `sums`, for the asset named `file`.
/// Returns Err on any failure, and the caller must not extract or swap then.
pub fn verify_archive(archive: &Path, file: &str, sums: &str) -> anyhow::Result<()> {
    let expected = expected_digest(sums, file)?;
    let actual = sha256_file(archive)?;
    if digests_equal(&expected, &actual) {
        return Ok(());
    }
    bail!(
        "checksum mismatch for {file}\n  expected sha256: {}\n  actual sha256:   {}\n\
         refusing to install",
        to_hex(&expected),
        to_hex(&actual)
    )
}

/// Append a downloaded chunk to the manifest buffer, refusing to grow past
/// [`MAX_SUMS_BYTES`].
pub fn push_sums_chunk(buf: &mut Vec<u8>, chunk: &[u8]) -> anyhow::Result<()> {
    if buf.len() + chunk.len() > MAX_SUMS_BYTES {
        bail!("SHA256SUMS is larger than {MAX_SUMS_BYTES} bytes - refusing to parse it");
    }
    buf.extend_from_slice(chunk);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX_A: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const HEX_B: &str = "486ea46224d1bb4fb680f34f7c9dad96a8f24ec88be73ea8e5a6c65260e9cb8a";

    #[test]
    fn parses_both_separator_forms_and_skips_blank_lines() {
        let text = format!(
            "{HEX_A}  modron-x86_64-unknown-linux-gnu.tar.gz\n\n   \n{HEX_B} *modron-aarch64-apple-darwin.tar.gz\n"
        );
        let entries = parse_sha256sums(&text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "modron-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(entries[1].0, "modron-aarch64-apple-darwin.tar.gz");
        assert_eq!(to_hex(&entries[1].1), HEX_B);
    }

    #[test]
    fn malformed_lines_are_rejected_table_driven() {
        let cases: &[(&str, &str)] = &[
            ("short hex", "2cf24dba  modron.tar.gz"),
            ("63 hex chars", &format!("{}  modron.tar.gz", &HEX_A[..63])),
            ("65 hex chars", &format!("{HEX_A}a  modron.tar.gz")),
            (
                "uppercase hex",
                &format!("{}  modron.tar.gz", HEX_A.to_uppercase()),
            ),
            ("non-hex char", &format!("{}g  modron.tar.gz", &HEX_A[..63])),
            ("single space", &format!("{HEX_A} modron.tar.gz")),
            ("tab separator", &format!("{HEX_A}\tmodron.tar.gz")),
            ("three spaces", &format!("{HEX_A}   modron.tar.gz")),
            ("no name", &format!("{HEX_A}  ")),
            ("name with slash", &format!("{HEX_A}  dir/modron.tar.gz")),
            ("CRLF ending", &format!("{HEX_A}  modron.tar.gz\r")),
            ("bare hash", HEX_A),
        ];
        for (label, line) in cases {
            assert!(parse_sha256sums(line).is_err(), "accepted: {label}");
        }
    }

    #[test]
    fn one_bad_line_fails_the_whole_manifest() {
        let text = format!("{HEX_A}  good.tar.gz\nnot a checksum line\n");
        let err = parse_sha256sums(&text).unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn empty_manifest_is_rejected() {
        assert!(parse_sha256sums("").is_err());
        assert!(parse_sha256sums("\n  \n").is_err());
    }

    #[test]
    fn extra_unrelated_entries_still_find_the_archive() {
        let text = format!(
            "{HEX_B}  modron-other.tar.gz\n{HEX_A}  modron-x86_64-unknown-linux-gnu.tar.gz\n"
        );
        let digest = expected_digest(&text, "modron-x86_64-unknown-linux-gnu.tar.gz").unwrap();
        assert_eq!(to_hex(&digest), HEX_A);
    }

    #[test]
    fn unlisted_and_duplicate_entries_are_errors() {
        let one = format!("{HEX_A}  modron-a.tar.gz\n");
        let err = expected_digest(&one, "modron-b.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not list"), "{err}");

        let dup = format!("{HEX_A}  modron-a.tar.gz\n{HEX_B}  modron-a.tar.gz\n");
        let err = expected_digest(&dup, "modron-a.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn name_match_is_exact() {
        let text = format!("{HEX_A}  modron-x86_64-unknown-linux-gnu.tar.gz.sig\n");
        assert!(expected_digest(&text, "modron-x86_64-unknown-linux-gnu.tar.gz").is_err());
    }

    #[test]
    fn digest_comparison() {
        let a = [7u8; 32];
        let mut b = a;
        assert!(digests_equal(&a, &b));
        b[31] ^= 1;
        assert!(!digests_equal(&a, &b));
        b = a;
        b[0] ^= 0x80;
        assert!(!digests_equal(&a, &b));
    }

    #[test]
    fn verify_archive_accepts_a_match_and_reports_both_digests_on_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("modron-test.tar.gz");
        std::fs::write(&archive, b"archive bytes").unwrap();
        let actual = to_hex(&sha256_file(&archive).unwrap());
        let good = format!("{actual}  modron-test.tar.gz\n");
        verify_archive(&archive, "modron-test.tar.gz", &good).unwrap();

        let bad = format!("{HEX_A}  modron-test.tar.gz\n");
        let err = verify_archive(&archive, "modron-test.tar.gz", &bad)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("checksum mismatch for modron-test.tar.gz"),
            "{err}"
        );
        assert!(err.contains(HEX_A), "{err}");
        assert!(err.contains(&actual), "{err}");
    }

    #[test]
    fn oversize_manifest_is_rejected_and_the_limit_is_inclusive() {
        let mut buf = Vec::new();
        push_sums_chunk(&mut buf, &vec![b'a'; MAX_SUMS_BYTES]).unwrap();
        assert!(push_sums_chunk(&mut buf, b"x").is_err());

        let mut buf = Vec::new();
        assert!(push_sums_chunk(&mut buf, &vec![b'a'; MAX_SUMS_BYTES + 1]).is_err());
    }
}
