//! modron-router — model selection.
//!
//! LEDGER items: R1 (heuristic `ModelSelector`: conservative tier mapping,
//! always-populated `reason`), R2 (embedded model catalog: ~12 models with
//! context windows + $/1M pricing + tier, plus lookup and pricing math).
//! Phase 3a adds the learned router (hashing-vectorizer + centroids) and
//! the eval harness; the Jev adapter lands when its API is documented.
//!
//! - [`catalog`] — R2: static model data, `lookup`, cost math.
//! - [`selector`] — R1: `HeuristicSelector` implementing
//!   `modron_core::ModelSelector`.
//!
//! Approximate pricing, reviewed quarterly — see the catalog module docs.

pub mod catalog;
pub mod learned;
pub mod selector;

pub use catalog::{catalog, lookup, ModelInfo, Tier};
pub use selector::HeuristicSelector;
