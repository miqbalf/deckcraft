//! DeckCraft live co-editing (layer 6).
//!
//! One yrs document per room holds the pinned base deck's slides and shapes and what peers changed
//! since. This crate holds the yrs schema ([`schema`], [`doc`]), its seed and rebuild ([`seed`],
//! [`build`]), the split of a command into updates ([`split`]), fractional order keys and text
//! bodies; the mapping to engine commands, the gate, per-user undo and presence follow in M1.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

pub mod build;
pub mod doc;
pub mod order;
pub mod schema;
pub mod seed;
pub mod split;
pub mod text;

pub use build::{deck_from_doc, deck_from_state, delete_shapes_in};
pub use doc::{DeckChange, DeckDoc, DeckSeed, DeckTxn, KeyRef, ShapeEntry, SlideEntry, TextTarget, Tomb};
pub use schema::{MAX_SEED_BYTES, UPDATE_BUDGET, schema};
pub use split::{PlannedWrite, plan_split};
