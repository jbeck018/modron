//! Pairing tests (Phase 3): token file write/list/revoke round-trip and
//! the hello auth gate (non-loopback without a token rejected, with a
//! valid token accepted, loopback always trusted).

use modron_server::{check_token, generate_token, grant, list, revoke};

fn temp_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

/// `grant` writes a 0600 JSON record; `list` finds it; `revoke` removes it.
#[test]
fn grant_list_revoke_round_trip() {
    let (_guard, dir) = temp_dir();
    let record = grant(&dir, "phone").unwrap();
    assert_eq!(record.name, "phone");
    assert_eq!(record.token.len(), 32, "token is 32 base64url chars");

    // File is 0600 (owner-only).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("phone.token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "token files are owner-only");
    }

    let devices = list(&dir).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0], record);

    assert!(revoke(&dir, "phone").unwrap());
    assert!(list(&dir).unwrap().is_empty());
    assert!(!revoke(&dir, "phone").unwrap(), "second revoke = false");
}

/// Tokens are url-safe base64 of fresh randomness: 32 chars from the
/// base64url alphabet, no padding.
#[test]
fn tokens_are_32_base64url_chars() {
    let alphabet: Vec<char> = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        .chars()
        .collect();
    for _ in 0..8 {
        let token = generate_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| alphabet.contains(&c)));
    }
    assert_ne!(generate_token(), generate_token(), "tokens are fresh");
}

/// Path safety: device names are plain filenames.
#[test]
fn device_names_are_filename_safe() {
    let (_guard, dir) = temp_dir();
    assert!(grant(&dir, "../escape").is_err());
    assert!(grant(&dir, "a/b").is_err());
    assert!(grant(&dir, "").is_err());
    assert!(grant(&dir, "  ").is_err());
    assert!(grant(&dir, "ok-name_1").is_ok());
}

/// The auth gate: remote (untrusted) connections need a stored token;
/// trusted (loopback / unix) connections never do.
#[test]
fn auth_gate_trust_matrix() {
    let (_guard, dir) = temp_dir();
    let record = grant(&dir, "tablet").unwrap();

    // Untrusted + no token -> rejected.
    assert!(check_token(false, None, &dir).is_err());
    // Untrusted + wrong token -> rejected.
    assert!(check_token(false, Some("nope"), &dir).is_err());
    // Untrusted + valid token -> accepted.
    assert!(check_token(false, Some(&record.token), &dir).is_ok());
    // Empty token string is no token.
    assert!(check_token(false, Some(""), &dir).is_err());
    // Trusted peer: anything (even nothing) is accepted.
    assert!(check_token(true, None, &dir).is_ok());
    assert!(check_token(true, Some("whatever"), &dir).is_ok());

    // After revocation the token stops authenticating.
    revoke(&dir, "tablet").unwrap();
    assert!(check_token(false, Some(&record.token), &dir).is_err());
}
