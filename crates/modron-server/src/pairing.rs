//! Device pairing (Phase 3): `modron pair` mints a bearer token per
//! device, stored under `~/.modron/clients/<name>.token` (mode 0600) as a
//! small JSON record. Remote (non-loopback) WebSocket connections must
//! present a token that matches a stored record in their `hello` frame;
//! loopback and Unix-socket peers are always trusted.

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// One paired device's record (the token file's JSON body).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceToken {
    pub name: String,
    pub token: String,
    pub created_at: String,
}

/// The pairing directory (default `~/.modron/clients`).
pub fn default_clients_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".modron").join("clients"))
}

/// The `modron serve` socket + pid live here too.
pub fn default_modron_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".modron"))
}

fn token_path(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("device name must not be empty");
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        anyhow::bail!("device name must be a plain filename (got {name:?})");
    }
    Ok(dir.join(format!("{name}.token")))
}

/// Generate a 32-char base64url token from 24 random bytes (two fresh
/// UUIDv4s). Exactly 24 bytes -> 8 base64 groups -> 32 chars, no padding.
pub fn generate_token() -> String {
    let uuid_a = Uuid::new_v4();
    let uuid_b = Uuid::new_v4();
    let a = uuid_a.as_bytes();
    let b = uuid_b.as_bytes();
    let mut bytes = [0u8; 24];
    bytes[..16].copy_from_slice(a);
    bytes[16..].copy_from_slice(&b[..8]);
    base64url_encode(&bytes)
}

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Standard base64 (RFC 4648 §5, URL-safe) without padding.
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64URL_ALPHABET[(n >> 18 & 0x3F) as usize] as char);
        out.push(B64URL_ALPHABET[(n >> 12 & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[(n >> 6 & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

/// Mint a token for `name`, persist it (0600), and return the record.
pub fn grant(dir: &Path, name: &str) -> anyhow::Result<DeviceToken> {
    let path = token_path(dir, name)?;
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let record = DeviceToken {
        name: name.trim().to_string(),
        token: generate_token(),
        created_at: chrono_now(),
    };
    let json = serde_json::to_string_pretty(&record)?;
    std::fs::write(&path, json).with_context(|| format!("could not write {}", path.display()))?;
    restrict_to_owner(&path);
    Ok(record)
}

/// Best-effort 0600 (Unix); a failed chmod is not fatal, the directory is
/// already user-private under `~/.modron`.
fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Every stored device record (sorted by name).
pub fn list(dir: &Path) -> anyhow::Result<Vec<DeviceToken>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(anyhow!("could not read {}: {e}", dir.display())),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("token") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str::<DeviceToken>(&data).ok())
        {
            Some(record) => out.push(record),
            None => tracing::warn!(
                file = %path.display(),
                "skipping unreadable device token file"
            ),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Delete a device's token; returns whether it existed.
pub fn revoke(dir: &Path, name: &str) -> anyhow::Result<bool> {
    let path = token_path(dir, name)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow!("could not remove {}: {e}", path.display())),
    }
}

/// Does `token` match any stored device record?
pub fn verify(dir: &Path, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    list(dir)
        .unwrap_or_default()
        .iter()
        .any(|record| constant_time_eq(record.token.as_bytes(), token.as_bytes()))
}

/// The auth gate for one connection. `trusted` peers (loopback WS, the
/// Unix socket) skip the token; remote peers MUST present a stored token.
pub fn check_token(trusted: bool, provided: Option<&str>, dir: &Path) -> Result<(), String> {
    if trusted {
        return Ok(());
    }
    match provided {
        Some(token) if verify(dir, token) => Ok(()),
        Some(_) => Err("unknown device token — pair with `modron pair <name>` first".to_string()),
        None => Err(
            "this endpoint requires a device token — pair with `modron pair <name>` \
             and send it in the hello frame"
                .to_string(),
        ),
    }
}

/// Length-checked byte comparison; early-exit on length only.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn chrono_now() -> String {
    // RFC 3339 without pulling chrono into this crate: the store already
    // owns timestamps; this is a human-readable record field.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}
