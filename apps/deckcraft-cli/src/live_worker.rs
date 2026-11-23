//! `deckcraft-cli live-worker`: DeckCraft's document service for live co-editing.
//!
//! The worker loop (the notify endpoint, room tasks, service tickets, checkpoint-then-write saves
//! with `If-Match`, conflict copies on `412`, heartbeats, closing empty rooms) is
//! craft-live-worker's, shared by every Craft app. What is DeckCraft's is here: only `.pptx` files
//! are served (Ruling D13), a room opens from the base the Nextcloud app pinned for the grant, and
//! a checkpoint's state is written back as the file through the engine's own `.pptx` writer, with
//! everything the live schema does not carry (pictures, animations, comments, masters, kept
//! parts) taken from that base.
//!
//! Tickets, tokens and secrets are never logged.

use std::sync::Arc;

use craft_collab::Hub;
use craft_live::{Access, LiveEditing, SERVICE_UID, uid_of};
use craft_live_worker::{DocumentService, LiveDocument};
use deckcraft_engine::Session;
use deckcraft_live::{BaseFile, Deck, DeckLive, deck_from_state};
use deckcraft_model::Slide;

/// DeckCraft's document service: PowerPoint presentations (`.pptx`) only. A file is written back
/// in its own format, and for templates, slide shows or older formats that would lose or change
/// what the file is.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeckService;

/// A presentation open in a live session in the worker: the engine session holding it and the
/// live link over the room. The worker never edits; it only follows the room and saves it.
pub struct DeckWorkerDoc<H: Hub> {
    live: DeckLive<H>,
    session: Session,
}

impl DocumentService for DeckService {
    type Doc<H: Hub> = DeckWorkerDoc<H>;

    fn accepts(&self, file_name: &str) -> bool {
        is_pptx(file_name)
    }

    fn refusal(&self, file_name: &str) -> String {
        format!("only .pptx presentations are edited live, not {file_name}")
    }

    fn open<H: Hub>(&self, hub: H, room: &str, file_name: &str, bytes: &[u8]) -> Result<DeckWorkerDoc<H>, String> {
        // `RoomFiles::base` already checked the pinned base against the hash the app gave.
        let base = BaseFile { bytes, name: file_name, expected_sha256: None };
        let mut session = Session::new();
        let live = DeckLive::start(hub, room, &base, Access::Edit, SERVICE_UID, &mut session).map_err(|e| e.to_string())?;
        Ok(DeckWorkerDoc { live, session })
    }

    /// Every editor seeds from the base the app pinned when the room opened, so the worker does too:
    /// the file may have been saved since, and a different base would build a different deck.
    fn pinned_base(&self) -> bool {
        true
    }
}

impl<H: Hub> DeckWorkerDoc<H> {
    /// The live presentation's slides as the engine holds them now.
    fn slides(&self) -> Vec<Arc<Slide>> {
        let uid = self.live.doc();
        self.session.documents().iter().find(|d| d.uid == uid).map(|d| d.doc.slides.clone()).unwrap_or_default()
    }
}

impl<H: Hub> LiveDocument<H> for DeckWorkerDoc<H> {
    type Kind = Deck;
    /// The schema shares slides and their shapes, so the slides are what is compared.
    type Snapshot = Vec<Arc<Slide>>;

    fn live(&self) -> &LiveEditing<H, Deck> {
        self.live.core()
    }

    fn live_mut(&mut self) -> &mut LiveEditing<H, Deck> {
        self.live.core_mut()
    }

    fn pump(&mut self) -> usize {
        self.live.pump(&mut self.session).iter().filter(|p| uid_of(&p.id) != SERVICE_UID).count()
    }

    fn snapshot(&self) -> Vec<Arc<Slide>> {
        self.slides()
    }

    /// The very same slides, by address: a remote change rebuilds only the slides it touched.
    fn unchanged(&self, a: &Vec<Arc<Slide>>, b: &Vec<Arc<Slide>>) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| Arc::ptr_eq(x, y))
    }

    /// Equal slides: by address first, by value where that differs.
    fn same(&self, a: &Vec<Arc<Slide>>, b: &Vec<Arc<Slide>>) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| Arc::ptr_eq(x, y) || x == y)
    }

    /// The deck the checkpoint's state describes over the base (tombstones removed with the
    /// engine's delete routine, Ruling D7), written as `.pptx`. The state is never compared or
    /// hashed: its bytes are not canonical (L3 M6).
    fn save(&self, state: &[u8], name: &str) -> Result<Vec<u8>, String> {
        log::warn!("live-worker: saving a presentation; the room's state is {} bytes", state.len());
        if !is_pptx(name) {
            return Err(DeckService.refusal(name));
        }
        let deck = deck_from_state(self.live.base(), state).map_err(|e| e.to_string())?;
        deckcraft_engine::cmd::file::save_bytes(&deck, "pptx").map_err(|e| e.to_string())
    }
}

/// Whether `name` is a PowerPoint presentation (`.pptx`), the only format edited live.
fn is_pptx(name: &str) -> bool {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("pptx"))
}
