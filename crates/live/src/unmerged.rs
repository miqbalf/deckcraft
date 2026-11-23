//! The unmerged copy (Ruling D18): when craft-live is about to drop local edits the room does not
//! have (a reset, a replica given up), the replica's state with them is kept, and the host saves
//! it beside the file as `"{stem} (unmerged YYYY-MM-DD HHMM).pptx"`.

use chrono::NaiveDateTime;
use deckcraft_model::Presentation;

use crate::build::deck_from_state;

/// The name of the unmerged copy of `file_name` made at `at`:
/// `"Deck (unmerged 2026-11-22 1404).pptx"` for `"Deck.pptx"`.
#[must_use]
pub fn unmerged_name(file_name: &str, at: NaiveDateTime) -> String {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => base,
    };
    format!("{stem} (unmerged {}).pptx", at.format("%Y-%m-%d %H%M"))
}

/// The replica's state with the local edits the room lacks, waiting for the host.
pub(crate) struct Unmerged {
    pub state: Vec<u8>,
    /// When it was handed over (stamped after the pump, on the wall clock).
    pub at: Option<NaiveDateTime>,
}

/// The `.pptx` bytes of `base` + `state`.
pub(crate) fn copy_bytes(base: &Presentation, state: &[u8]) -> Result<Vec<u8>, String> {
    let deck = deck_from_state(base, state).map_err(|e| e.to_string())?;
    deckcraft_engine::cmd::file::save_bytes(&deck, "pptx").map_err(|e| e.to_string())
}

/// The wall clock on this target, when it has one.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn local_now() -> Option<NaiveDateTime> {
    Some(chrono::Local::now().naive_local())
}

/// `std::time` has no clock on the web: the host gives one ([`crate::DeckLive::set_wall_clock`]).
#[cfg(target_arch = "wasm32")]
pub(crate) fn local_now() -> Option<NaiveDateTime> {
    None
}
