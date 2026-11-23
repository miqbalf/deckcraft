//! [`DeckApp`]: DeckCraft's side of each step of a live session, made per call from the engine
//! session and the mapping's state.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use craft_live::{Landed, LiveApp, LocalError};
use deckcraft_engine::{EngineError, Session};
use deckcraft_model::{IdBlock, Presentation, ShapeId, SlideId};
use serde_json::{Value, json};

use crate::doc::{DeckChange, DeckDoc, TextTarget};
use crate::gate::NOT_YET_TEXT;
use crate::kind::{CaretPos, Deck, DeckCaret, DeckEdit, DeckPos};
use crate::local::plan;
use crate::net::unshared;
use crate::remote::{Targets, land, whole};
use crate::schema::{id_key, parse_key};
use crate::session::Notice;
use crate::split::plan_split;

/// The relay's frame: no update may pass it once encoded.
pub(crate) const FRAME: usize = 1024 * 1024;

/// Why a single change too large for one update is refused (L3 review M5).
pub const EDIT_TOO_LARGE_TEXT: &str = "This change is too large to share while editing live";

/// The most shapes of a caret kept.
const CARET_SHAPES_MAX: usize = 256;

/// One command as it went out: what it touched and how many updates it took (a large command is
/// cut into several updates that are still one command, one undo step).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SentGroup {
    pub slides: Vec<SlideId>,
    pub shapes: Vec<ShapeId>,
    pub updates: usize,
}

/// What the mapping keeps between calls.
pub(crate) struct Mapping {
    /// The engine document's uid.
    pub doc: u64,
    pub base: Arc<Presentation>,
    pub uid: String,
    pub space: u32,
    /// This peer's id block, once a space is granted.
    pub block: Option<IdBlock>,
    /// Remote changes waiting while a drag or a composition is open (Ruling D6).
    pub held: Targets,
    /// When the hold began (clock ms, or pumps without a clock).
    pub held_since: Option<f64>,
    /// A full rebuild could not land while busy: do it once free.
    pub resync_wanted: bool,
    /// The slides the last batch touched, rebuilt from the replica once it landed.
    pub touched: Targets,
    /// The last batch could not be written whole.
    pub partial: bool,
    /// Why the last batch was refused, for the gate's answer.
    pub refused: Option<String>,
    pub notices: Vec<Notice>,
    pub sent: Vec<SentGroup>,
}

/// The engine session and the mapping, for one call.
pub(crate) struct DeckApp<'a> {
    pub session: &'a mut Session,
    pub map: &'a mut Mapping,
    /// Now, for the hold's clock.
    pub now: f64,
}

impl DeckApp<'_> {
    /// Put `p` in place quietly, with this peer's block; a busy engine makes it wait.
    fn install(&mut self, mut p: Presentation) -> Result<(), String> {
        p.id_block = self.map.block.clone();
        match self.session.replace_presentation_quiet(self.map.doc, p) {
            Ok(()) => {
                self.map.resync_wanted = false;
                Ok(())
            }
            Err(EngineError::RemoteBusy) => {
                self.map.resync_wanted = true;
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn land_or_hold(&mut self, replica: &DeckDoc, t: Targets) -> Result<(), String> {
        let mut all = std::mem::take(&mut self.map.held);
        all.merge(t);
        if all.is_empty() {
            return Ok(());
        }
        if self.session.remote_busy(self.map.doc) {
            self.hold(all);
            return Ok(());
        }
        match land(self.session, self.map.doc, &self.map.base, replica, &all) {
            Ok(()) => {
                self.map.held_since = None;
                Ok(())
            }
            Err(EngineError::RemoteBusy) => {
                self.hold(all);
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn hold(&mut self, t: Targets) {
        self.map.held = t;
        if self.map.held_since.is_none() {
            self.map.held_since = Some(self.now);
        }
    }

    /// What was held for a drag or a composition, once it ended; call after each pump.
    pub(crate) fn retry(&mut self, replica: Option<&DeckDoc>) -> Result<(), String> {
        let Some(replica) = replica else { return Ok(()) };
        if self.map.resync_wanted && !self.session.remote_busy(self.map.doc) {
            return self.resync(replica);
        }
        if self.map.resync_wanted {
            return self.resync(replica);
        }
        // The slides the last local edits touched, as the replica has them (text runs merged,
        // the notes slide flag), so this window shows what every other one rebuilds.
        let touched = std::mem::take(&mut self.map.touched);
        if !self.map.held.is_empty() || !touched.is_empty() {
            return self.land_or_hold(replica, touched);
        }
        Ok(())
    }

    fn current(&self) -> Option<&deckcraft_engine::DocState> {
        self.session.documents().iter().find(|d| d.uid == self.map.doc)
    }
}

impl LiveApp<Deck> for DeckApp<'_> {
    fn local(&mut self, replica: &mut DeckDoc, edits: &[DeckEdit]) -> Result<Vec<Vec<u8>>, LocalError> {
        let m = &mut *self.map;
        let ours: Vec<&deckcraft_engine::Edited> =
            edits.iter().filter_map(|e| if let DeckEdit::Command(c) = e { Some(c.as_ref()) } else { None }).filter(|c| c.doc == m.doc).collect();
        // The safety net first, over the whole batch: nothing is written unless all of it is shared.
        for c in &ours {
            if let Some(why) = unshared(&c.before, &c.after) {
                m.refused = Some(why.to_owned());
                return Err(LocalError::Refused(why.to_owned()));
            }
        }
        if !ours.is_empty() && m.block.is_none() {
            // No id may be minted before a space is granted (D2 review Minor 2).
            m.refused = Some(craft_live::CONNECTING.to_owned());
            return Err(LocalError::Refused(craft_live::CONNECTING.to_owned()));
        }
        for e in edits {
            if matches!(e, DeckEdit::Undo | DeckEdit::Redo) {
                // Per-user undo comes with L6.
                m.notices.push(Notice { text: NOT_YET_TEXT.to_owned(), restore: None });
            }
        }
        let plans: Vec<_> = ours.iter().map(|c| plan(replica, c, m.space, &m.uid)).collect();
        let parts: Vec<Vec<Vec<crate::split::PlannedWrite>>> = plans.iter().map(|p| plan_split(&p.writes)).collect();
        // One entry's writes past what one frame holds would be refused by the relay after this
        // replica kept them: refuse the command before anything is written (L3 review M5). The
        // estimate counts twice the content's JSON.
        if parts.iter().flatten().any(|part| part.iter().map(crate::split::PlannedWrite::size).sum::<usize>() > 2 * FRAME) {
            m.refused = Some(EDIT_TOO_LARGE_TEXT.to_owned());
            return Err(LocalError::Refused(EDIT_TOO_LARGE_TEXT.to_owned()));
        }
        let mut updates = Vec::new();
        for (p, parts) in plans.iter().zip(parts) {
            if p.writes.is_empty() {
                m.touched.whole.extend(p.slides.iter().copied());
                continue;
            }
            let before = updates.len();
            for part in parts {
                let (texts, keys): (Vec<_>, Vec<_>) = part.into_iter().partition(|w| matches!(w, crate::split::PlannedWrite::Text { .. }));
                // Key writes untracked, the text in one tracked transaction (Ruling D4).
                for (tracked, ws) in [(false, keys), (true, texts)] {
                    if ws.is_empty() {
                        continue;
                    }
                    match replica.write(tracked, |t| {
                        for w in &ws {
                            w.write(t);
                        }
                        Ok(())
                    }) {
                        Ok(u) => updates.push(u),
                        Err(e) => {
                            // Whatever was written goes out; the engine is rebuilt from the replica.
                            if let Some(u) = e.update {
                                updates.push(u);
                            }
                            log::warn!("deckcraft-live: a local write was refused: {}", e.error);
                            if updates.is_empty() {
                                return Err(LocalError::Failed(e.error.to_string()));
                            }
                            m.partial = true;
                            return Ok(updates);
                        }
                    }
                }
            }
            for u in updates.iter().skip(before) {
                if u.len() > FRAME {
                    log::warn!("deckcraft-live: an update of {} bytes is over the relay's frame", u.len());
                }
            }
            let mut shapes = BTreeSet::new();
            for w in &p.writes {
                match w {
                    crate::split::PlannedWrite::PutShape { shape, .. } => {
                        shapes.insert(shape.id);
                    }
                    crate::split::PlannedWrite::SetShape { id, .. } => {
                        shapes.insert(*id);
                    }
                    crate::split::PlannedWrite::Text { target: TextTarget::Shape(id), .. } => {
                        shapes.insert(*id);
                    }
                    crate::split::PlannedWrite::Del { target: crate::doc::Tomb::Shape(id), .. } => {
                        shapes.insert(*id);
                    }
                    _ => {}
                }
            }
            m.sent.push(SentGroup {
                slides: p.slides.iter().copied().collect(),
                shapes: shapes.into_iter().collect(),
                updates: updates.len() - before,
            });
            m.touched.whole.extend(p.slides.iter().copied());
        }
        Ok(updates)
    }

    fn landed(&mut self) -> Landed {
        if std::mem::take(&mut self.map.partial) {
            self.map.touched = Targets::default();
            return Landed::Partial;
        }
        Landed::Done
    }

    fn put_back(&mut self, edits: Vec<DeckEdit>) -> Result<(), String> {
        let first = edits.iter().find_map(|e| if let DeckEdit::Command(c) = e { (c.doc == self.map.doc).then(|| c.before.clone()) } else { None });
        let Some(before) = first else { return Ok(()) };
        self.install((*before).clone())
    }

    fn received(&mut self, _replica: &mut DeckDoc, changes: Vec<DeckChange>) -> (Vec<DeckChange>, Vec<Vec<u8>>) {
        (changes, Vec::new())
    }

    fn apply(&mut self, replica: &DeckDoc, changes: &[DeckChange]) -> Result<(), String> {
        self.land_or_hold(replica, Targets::from_changes(changes))
    }

    fn resync(&mut self, replica: &DeckDoc) -> Result<(), String> {
        self.map.held = Targets::default();
        self.map.held_since = None;
        self.map.touched = Targets::default();
        let p = whole(&self.map.base, replica, self.map.block.as_ref());
        self.install(p)
    }

    fn reset(&mut self, space: u32) {
        self.map.held = Targets::default();
        self.map.held_since = None;
        self.map.touched = Targets::default();
        self.set_space(space);
    }

    fn set_space(&mut self, space: u32) {
        let m = &mut *self.map;
        if m.space == space && m.block.is_some() {
            return;
        }
        m.space = space;
        // A new space starts a new block; the old block's ids stay taken (Ruling D5).
        m.block = IdBlock::new(space);
        let block = m.block.clone();
        if let Some(st) = self.session.document_mut(m.doc) {
            // Allocation state, not content: decks compare equal whatever block they hold.
            Arc::make_mut(&mut st.doc).id_block = block;
        }
    }

    fn caret(&self, payload: &Value) -> Option<DeckCaret> {
        let slide = SlideId(parse_key(payload.get("s")?.as_str()?)?);
        let shapes: Vec<ShapeId> = payload
            .get("sh")
            .and_then(Value::as_array)
            .map(|a| a.iter().take(CARET_SHAPES_MAX).filter_map(|v| v.as_str().and_then(parse_key)).map(ShapeId).collect())
            .unwrap_or_default();
        let caret = payload.get("c").and_then(|c| {
            let id = parse_key(c.get("id")?.as_str()?)?;
            let target =
                if c.get("n").and_then(Value::as_bool) == Some(true) { TextTarget::Notes(SlideId(id)) } else { TextTarget::Shape(ShapeId(id)) };
            let pair = |k: &str| -> Option<(usize, usize)> {
                let a = c.get(k)?.as_array()?;
                Some((usize::try_from(a.first()?.as_u64()?).ok()?, usize::try_from(a.get(1)?.as_u64()?).ok()?))
            };
            Some(CaretPos { target, anchor: pair("a")?, head: pair("h")? })
        });
        let mut c = DeckCaret { slide, shapes, caret, pos: DeckPos { slide: None, shapes: Vec::new(), caret: None } };
        resolve(self.current().map(|d| d.doc.as_ref()), &mut c).then_some(c)
    }

    fn follow(&self, carets: &mut HashMap<String, DeckCaret>) {
        let p = self.current().map(|d| d.doc.clone());
        carets.retain(|_, c| resolve(p.as_deref(), c));
    }
}

/// Where a caret is now: its slide's index, its shapes that are still there, its text position
/// inside the text. False when its slide is gone.
fn resolve(p: Option<&Presentation>, c: &mut DeckCaret) -> bool {
    let Some(p) = p else { return false };
    let Some((i, s)) = p.slides.iter().enumerate().find(|(_, s)| s.id == c.slide) else { return false };
    let shapes: Vec<ShapeId> = c.shapes.iter().copied().filter(|id| s.shape(*id).is_some()).collect();
    let caret = c.caret.as_ref().and_then(|k| {
        let body = match &k.target {
            TextTarget::Notes(id) if *id == s.id => &s.notes,
            TextTarget::Notes(_) => return None,
            TextTarget::Shape(id) => s.shape(*id)?.text.as_ref()?,
        };
        let fit = |(pi, ci): (usize, usize)| {
            let pi = pi.min(body.paragraphs.len().saturating_sub(1));
            (pi, ci.min(body.paragraphs.get(pi).map_or(0, |x| x.char_len())))
        };
        Some(CaretPos { target: k.target.clone(), anchor: fit(k.anchor), head: fit(k.head) })
    });
    c.pos = DeckPos { slide: Some(i), shapes, caret };
    true
}

/// The caret payload for this window's selection, in stable ids.
pub(crate) fn caret_payload(st: &deckcraft_engine::DocState) -> Option<Value> {
    let slide = st.doc.slides.get(st.selection.slide)?;
    let shapes: Vec<String> = st.selection.shapes.iter().take(CARET_SHAPES_MAX).map(|id| id_key(id.0)).collect();
    let caret = st.selection.text.as_ref().filter(|t| t.cell.is_none()).map(|t| {
        let id = if t.notes { slide.id.0 } else { t.shape.0 };
        json!({"id": id_key(id), "n": t.notes, "a": [t.anchor.0, t.anchor.1], "h": [t.caret.0, t.caret.1]})
    });
    Some(json!({"s": id_key(slide.id.0), "sh": shapes, "c": caret}))
}
