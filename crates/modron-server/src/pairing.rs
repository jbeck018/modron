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
///
/// The token is written to a temp file created 0600 from the start (never
/// readable under the umask default), then renamed over the final name, so
/// there is no window where the secret sits with looser permissions.
pub fn grant(dir: &Path, name: &str) -> anyhow::Result<DeviceToken> {
    let path = token_path(dir, name)?;
    create_private_dir(dir)?;
    let record = DeviceToken {
        name: name.trim().to_string(),
        token: generate_token(),
        created_at: chrono_now(),
    };
    let json = serde_json::to_string_pretty(&record)?;
    write_private_atomic(&path, json.as_bytes())?;
    Ok(record)
}

/// Write `bytes` to `path` atomically with owner-only permissions: a temp
/// file in the same directory is created 0600 from the start (never readable
/// under the umask default), written, synced, then renamed over `path`. A
/// torn in-place write can therefore never replace a good file.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_dir(dir)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(".{file_name}.tmp-{}", Uuid::new_v4().simple()));
    let write = || -> std::io::Result<()> {
        let mut file = private_file_options()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("could not write {}: {e}", path.display())
    })
}

/// Create `dir` (and missing parents) owner-only: 0700 on Unix, where the
/// directory is created here. An existing directory is left as it is.
pub fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("could not create {}", dir.display()))
}

/// Open options for a secret file: created 0600 from the start on Unix.
#[cfg(unix)]
fn private_file_options() -> std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.mode(0o600);
    options
}

#[cfg(not(unix))]
fn private_file_options() -> std::fs::OpenOptions {
    std::fs::OpenOptions::new()
}

/// Best-effort owner-only permissions on an existing path (Unix). Used by
/// the listeners for their socket files.
pub fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(all(test, unix))]
mod mode_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn grant_creates_private_dir_and_0600_token_file() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("clients");
        let record = grant(&dir, "phone").unwrap();
        assert_eq!(mode(&dir), 0o700, "clients dir must be owner-only");
        assert_eq!(mode(&dir.join("phone.token")), 0o600);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "phone.token")
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
        assert_eq!(list(&dir).unwrap(), vec![record]);
    }
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
