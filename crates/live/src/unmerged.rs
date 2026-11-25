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

/// The replica's states with the local edits the room lacks, waiting for the host: one per
/// hand-over, oldest first, merged into one copy when taken.
pub(crate) struct Unmerged {
    pub states: Vec<Vec<u8>>,
    /// When it was handed over (stamped after the pump, on the wall clock).
    pub at: Option<NaiveDateTime>,
    /// Every state in it was kept because the offline buffer filled (none after a reset or a
    /// replica given up): the copy is offered as waiting edits, not as unmerged ones.
    pub full: bool,
}

/// The `.pptx` bytes of `base` + every state in `states` (yrs updates of one document, so their
/// merge holds each one's edits).
pub(crate) fn copy_bytes(base: &Presentation, states: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let merged;
    let state = match states {
        [one] => one.as_slice(),
        _ => {
            let parts: Vec<&[u8]> = states.iter().map(Vec::as_slice).collect();
            merged = craft_collab::yrs::merge_updates_v1(&parts).map_err(|e| e.to_string())?;
            merged.as_slice()
        }
    };
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
