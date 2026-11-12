//! Shared HTTP scaffolding for the provider crates: one `build_client`
//! (the three providers shipped byte-identical copies) and one POST helper
//! wrapping the status-check -> error -> parse sequence.

use modron_core::{Error, Result};
use reqwest::Client;
use std::time::Duration;

/// Per-request timeout shared by every provider.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Connect timeout shared by every provider.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The shared reqwest client: same timeouts, same construction everywhere.
pub fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|err| Error::Other(anyhow::anyhow!("failed to build HTTP client: {err}")))
}
