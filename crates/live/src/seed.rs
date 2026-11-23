//! What a room is seeded from: the base bytes' sha256 and the deck they import to.
//!
//! The seed itself is [`DeckDoc::seeded`](craft_collab::Replica::seeded): client 0, one
//! transaction, every slide and shape of the base with its keys sorted, one call per key. The
//! import is deterministic (D1), so peers opening the same bytes reach the same item ids.

use std::sync::Arc;

use deckcraft_model::Presentation;
use sha2::{Digest, Sha256};

use crate::doc::DeckSeed;

/// sha256 of the base bytes.
#[must_use]
pub fn base_sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

impl DeckSeed {
    /// The seed for `deck`, imported from `bytes`.
    #[must_use]
    pub fn new(bytes: &[u8], deck: Arc<Presentation>) -> DeckSeed {
        DeckSeed { base_sha256: base_sha256(bytes), deck }
    }
}
