//! DeckCraft live co-editing (layer 6).
//!
//! One yrs document per room holds the pinned base deck's slides and shapes and what peers changed
//! since. This crate holds the yrs schema ([`schema`], [`doc`]), its seed and rebuild ([`seed`],
//! [`build`]), the split of a command into updates ([`split`]), fractional order keys and text
//! bodies; the mapping between engine edits and the document ([`local`], [`remote`]), the safety
//! net ([`net`]), the command gate ([`gate`]) and the session ([`DeckLive`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

mod app;
pub mod build;
pub mod doc;
pub mod gate;
pub mod kind;
mod local;
pub mod net;
pub mod order;
mod remote;
pub mod schema;
pub mod seed;
pub mod session;
pub mod split;
pub mod text;
pub mod tomb;
pub mod unmerged;

pub use app::{EDIT_TOO_LARGE_TEXT, FRAME, SentGroup, WIRE_LIMIT};
pub use build::{deck_from_doc, deck_from_state};
pub use doc::{DeckChange, DeckDoc, DeckSeed, DeckTxn, KeyRef, ShapeEntry, SlideEntry, TextTarget, Tomb, WriteError};
pub use gate::{MEDIA_TEXT, NOT_YET_TEXT};
pub use kind::{CaretPos, Deck, DeckCaret, DeckEdit, DeckPos};
pub use net::{COPY_UNSHARED_TEXT, SECTIONS_TEXT, unshared};
pub use schema::{MAX_SEED_BYTES, UPDATE_BUDGET, schema};
pub use session::{
    BaseFile, CLOSED_TEXT, DeckLive, LiveFail, NO_SPACE_TEXT, Notice, PAUSED_TEXT, Peer, READ_ONLY_TEXT, TOO_LARGE_TEXT, UNMERGED_TEXT,
};
pub use split::{PlannedWrite, plan_split};
pub use tomb::{RESTORE_LABEL, deleted_slide_text, deleted_text};
pub use unmerged::unmerged_name;
