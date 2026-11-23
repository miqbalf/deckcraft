//! [`DeckApp`]: DeckCraft's side of each step of a live session, made per call from the engine
//! session and the mapping's state.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use craft_live::{Landed, LiveApp, LocalError};
use deckcraft_engine::{EngineError, Session};
use deckcraft_model::{IdBlock, Presentation, ShapeId, SlideId};
use serde_json::Value;

use crate::doc::{DeckChange, DeckDoc, TextTarget, Tomb};
use crate::kind::{Deck, DeckCaret, DeckEdit};
use crate::local::plan;
use crate::net::unshared;
use crate::presence::{Mine, Scene};
use crate::remote::{Targets, land, whole};
use crate::session::{Notice, UNMERGED_TEXT};
use crate::split::{PlannedWrite, plan_split};
use crate::tomb::{Tombs, inside, line_for, restore_writes, slide_of, slide_tomb, tomb_over, tombstoned};
use crate::undo::{AFTER_UNDO_TEXT, Group, UNDO_SKIPPED_TEXT, UndoLog};
use crate::unmerged::Unmerged;

/// The frame one update may fill on the wire (base64 inside its submit), whatever larger cap
/// a relay advertises (L3 review M5). craft-live does not expose the relay's own cap at this pin;
/// the relay's default is twice this.
pub const FRAME: usize = 1024 * 1024;

/// What the gate lets one part of a command fill: the frame less a margin.
pub const WIRE_LIMIT: usize = FRAME - FRAME / 16;

/// The submit's JSON around the base64 of one update.
const ENVELOPE: usize = 256;

/// An update of at most `raw` bytes, as the relay measures it: base64 in its submit.
pub(crate) fn wire_bound(raw: usize) -> usize {
    raw.div_ceil(3) * 4 + ENVELOPE
}

/// The bytes the relay measures for `update` (what craft-collab checks against the relay's cap).
fn wire_of(update: &[u8]) -> usize {
    serde_json::to_vec(&craft_collab::TextPayload::new(update)).map_or(usize::MAX, |v| v.len())
}

/// Why a single change too large for one update is refused (L3 review M5).
pub const EDIT_TOO_LARGE_TEXT: &str = "This change is too large to share while editing live";

/// One command as it went out: what it touched and how many updates it took (a large command is
/// cut into several updates that are still one command, one undo step).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SentGroup {
    pub slides: Vec<SlideId>,
    pub shapes: Vec<ShapeId>,
    pub updates: usize,
    /// The largest of its updates on the wire.
    pub wire: usize,
    /// The gate's bound for its largest part, which `wire` never passes.
    pub bound: usize,
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
    /// When the hold began, or the gesture holding it last moved (clock ms, or pumps without a
    /// clock).
    pub held_since: Option<f64>,
    /// The engine document's revision at the last pump, to see a gesture still moving.
    pub seen_revision: u64,
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
    /// The base file's name, for the unmerged copy's.
    pub name: String,
    /// Tombstones seen and told, my latest groups and the roster's names (L5).
    pub tombs: Tombs,
    /// Local edits craft-live was about to drop, for [`crate::DeckLive::take_unmerged`].
    pub unmerged: Option<Unmerged>,
    /// What the other editors' carets resolve against (L7).
    pub scene: Scene,
    /// This window's text caret as sticky indexes, for remote text edits (L7, spec H2).
    pub mine: Option<Mine>,
    /// My commands, for Undo and Redo (L6).
    pub undo: UndoLog,
    /// Commands made after an Undo in the same batch were dropped: the engine is rebuilt from
    /// the replica once the batch landed.
    pub resync_after: bool,
}

impl Mapping {
    /// The texts whose remote change waits (for the carets' scene, L7 review M2).
    pub(crate) fn held_texts(&self) -> crate::presence::Held {
        crate::presence::Held {
            shapes: self.held.shapes.clone(),
            slides: self.held.slides.clone(),
            whole: self.held.whole.clone(),
            all: self.resync_wanted,
        }
    }

    /// Keep `state` (a replica's state with local edits the room lacks) for the unmerged copy.
    /// A copy still waiting is merged with it, never replaced: after a reset the replica no longer
    /// holds the earlier edits (L5 review I1). The person is told when a copy starts waiting.
    pub(crate) fn keep_unmerged(&mut self, state: Vec<u8>) {
        match self.unmerged.as_mut() {
            Some(u) => u.states.push(state),
            None => {
                self.unmerged = Some(Unmerged { states: vec![state], at: None });
                self.notices.push(Notice { text: UNMERGED_TEXT.to_owned(), restore: None });
            }
        }
    }
}

/// Every tombstone `doc` holds.
fn tombs_in(doc: &DeckDoc) -> BTreeSet<Tomb> {
    let slides = doc.slide_entries().iter().filter(|(_, e)| e.del).map(|(id, _)| Tomb::Slide(*id));
    slides.chain(doc.shape_entries().iter().filter(|(_, e)| e.del).map(|(id, _)| Tomb::Shape(*id))).collect()
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
        p.id_block = Some(self.map.block.clone().unwrap_or_default());
        match self.session.replace_presentation_quiet(self.map.doc, p) {
            Ok(()) => {
                self.map.resync_wanted = false;
                Ok(())
            }
            Err(EngineError::RemoteBusy) => {
                self.map.resync_wanted = true;
                self.start_hold();
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn start_hold(&mut self) {
        if self.map.held_since.is_none() {
            self.map.held_since = Some(self.now);
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
        match land(self.session, self.map.doc, &self.map.base, replica, &all, &mut self.map.mine) {
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
        self.start_hold();
    }

    /// What was held for a drag or a composition, once it ended; call after each pump.
    pub(crate) fn retry(&mut self, replica: Option<&DeckDoc>) -> Result<(), String> {
        let Some(replica) = replica else { return Ok(()) };
        if self.map.resync_wanted {
            // `resync` waits again, without building anything, while the engine is busy.
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

    /// The carets' scene gets the engine's deck and what is held as they are now.
    fn see(&self) {
        self.map.scene.set_deck(self.current().map(|d| d.doc.clone()));
        self.map.scene.set_held(self.map.held_texts());
    }

    /// Whether this window's selection or caret is in `t`: a selected shape or the text caret
    /// inside a deleted shape, or on a deleted slide.
    fn selected_in(&self, replica: &DeckDoc, t: Tomb) -> bool {
        let Some(st) = self.current() else { return false };
        let mut held: Vec<ShapeId> = st.selection.shapes.clone();
        if let Some(c) = st.selection.text.as_ref().filter(|c| !c.notes) {
            held.push(c.shape);
        }
        match t {
            Tomb::Shape(g) => held.iter().any(|id| inside(replica, *id, g)),
            Tomb::Slide(s) => {
                let on = st.doc.slides.get(st.selection.slide).map(|x| x.id) == Some(s);
                (on && (!held.is_empty() || st.selection.text.is_some())) || held.iter().any(|id| slide_of(replica, *id) == Some(s))
            }
        }
    }

    /// Remote changes that tombstoned a slide or shape: the person is told, with Restore, when
    /// their latest work or their selection is in it (Ruling D8). One brought back is forgotten.
    fn note_deletes(&mut self, replica: &DeckDoc, changes: &[DeckChange]) {
        let mut targets = BTreeSet::new();
        for c in changes {
            match c {
                DeckChange::Slides(ids) => targets.extend(ids.iter().map(|id| Tomb::Slide(*id))),
                DeckChange::Shapes(ids) => targets.extend(ids.iter().map(|id| Tomb::Shape(*id))),
                DeckChange::Text(id) => {
                    targets.insert(Tomb::Shape(*id));
                }
                DeckChange::Notes(id) => {
                    targets.insert(Tomb::Slide(*id));
                }
                DeckChange::Meta => {}
            }
        }
        for t in targets {
            match tombstoned(replica, t) {
                Some(by) => {
                    if self.map.tombs.dead.insert(t) && by != self.map.uid && (self.map.tombs.mine(replica, t) || self.selected_in(replica, t)) {
                        self.map.tombs.tell(t, &by);
                    }
                }
                None => {
                    if self.map.tombs.dead.contains(&t) {
                        self.map.tombs.revived(t);
                    }
                }
            }
        }
    }

    /// Restores asked for in this batch, each a group of its own.
    fn restores(&mut self, replica: &mut DeckDoc, edits: &[DeckEdit], updates: &mut Vec<Vec<u8>>) -> Result<(), LocalError> {
        for e in edits {
            let DeckEdit::Restore(t) = e else { continue };
            let writes = restore_writes(replica, *t, &self.map.uid);
            if writes.is_empty() {
                continue;
            }
            let bound = wire_bound(writes.iter().map(PlannedWrite::size).sum());
            let mut g = Group::default();
            let u = replica.write(false, |txn| {
                for w in &writes {
                    g.write_noted(txn, w);
                }
                Ok(())
            });
            let u = match u {
                Ok(u) => u,
                Err(e) => {
                    log::warn!("deckcraft-live: a restore was refused: {}", e.error);
                    // Whatever was written goes out; the engine is rebuilt from the replica.
                    // As for a command: earlier updates of this batch went out, so this is partial.
                    match e.update {
                        Some(u) => updates.push(u),
                        None if updates.is_empty() => return Err(LocalError::Failed(e.error.to_string())),
                        None => {}
                    }
                    self.map.partial = true;
                    return Ok(());
                }
            };
            let (mut slides, mut shapes) = (BTreeSet::new(), BTreeSet::new());
            for w in &writes {
                if let PlannedWrite::Del { target, .. } = w {
                    self.map.tombs.revived(*target);
                    match target {
                        Tomb::Slide(id) => {
                            slides.insert(*id);
                        }
                        Tomb::Shape(id) => {
                            shapes.insert(*id);
                            slides.extend(slide_of(replica, *id));
                        }
                    }
                }
            }
            self.map.sent.push(SentGroup {
                slides: slides.iter().copied().collect(),
                shapes: shapes.iter().copied().collect(),
                updates: 1,
                wire: wire_of(&u),
                bound,
            });
            self.map.tombs.wrote(slides.iter().copied(), shapes.iter().copied());
            // A Restore is a command of mine: Undo deletes it again.
            g.slides.extend(slides.iter().copied());
            self.map.undo.record(g);
            self.map.touched.whole.extend(slides);
            updates.push(u);
        }
        Ok(())
    }

    /// The mapping of a batch of local edits (see [`LiveApp::local`]).
    fn local_inner(&mut self, replica: &mut DeckDoc, edits: &[DeckEdit]) -> Result<Vec<Vec<u8>>, LocalError> {
        // Commands made after an Undo or a Redo in this batch were made on the deck before the
        // undo landed in the engine: they are not shared, and the engine is rebuilt from the
        // replica (craft-live does the same for its own history steps).
        let first = edits.iter().position(|e| matches!(e, DeckEdit::Undo | DeckEdit::Redo)).unwrap_or(edits.len());
        let (now, later) = edits.split_at(first);
        let mut updates = self.commands(replica, now)?;
        if self.map.partial {
            return Ok(updates);
        }
        self.restores(replica, edits, &mut updates)?;
        let doc = self.map.doc;
        if later.iter().any(|e| matches!(e, DeckEdit::Command(c) if c.doc == doc)) {
            self.map.resync_after = true;
            self.map.notices.push(Notice { text: AFTER_UNDO_TEXT.to_owned(), restore: None });
        }
        for e in later {
            let redo = match e {
                DeckEdit::Undo => false,
                DeckEdit::Redo => true,
                _ => continue,
            };
            self.history(replica, redo, &mut updates)?;
            if self.map.partial {
                break;
            }
        }
        Ok(updates)
    }

    /// Undo (`redo` false) or redo my latest command: one new group of its own.
    fn history(&mut self, replica: &mut DeckDoc, redo: bool, updates: &mut Vec<Vec<u8>>) -> Result<(), LocalError> {
        let m = &mut *self.map;
        let Some(g) = m.undo.take(redo) else { return Ok(()) };
        let ran = match crate::undo::run(replica, g, redo, &m.uid) {
            Ok(r) => r,
            Err(e) => {
                if updates.is_empty() {
                    return Err(LocalError::Failed(e.error.to_string()));
                }
                m.partial = true;
                return Ok(());
            }
        };
        if ran.skipped > 0 {
            m.notices.push(Notice { text: UNDO_SKIPPED_TEXT.to_owned(), restore: None });
        }
        m.partial |= ran.partial;
        for (t, del) in &ran.dels {
            if *del {
                m.tombs.dead.insert(*t);
            } else {
                m.tombs.revived(*t);
            }
        }
        let mut shapes = BTreeSet::new();
        for w in &ran.inverse.writes {
            if let crate::doc::KeyRef::Shape(id, _) = &w.key {
                shapes.insert(*id);
            }
        }
        let slides = ran.inverse.slides.clone();
        if !ran.updates.is_empty() {
            m.sent.push(SentGroup {
                slides: slides.iter().copied().collect(),
                shapes: shapes.iter().copied().collect(),
                updates: ran.updates.len(),
                wire: ran.updates.iter().map(|u| wire_of(u)).max().unwrap_or(0),
                bound: ran.bound,
            });
            m.tombs.wrote(slides.iter().copied(), shapes.iter().copied());
        }
        m.touched.whole.extend(slides);
        m.undo.done(redo, ran.inverse);
        updates.extend(ran.updates);
        Ok(())
    }

    /// The shared commands of a batch (no Undo or Redo among them).
    fn commands(&mut self, replica: &mut DeckDoc, edits: &[DeckEdit]) -> Result<Vec<Vec<u8>>, LocalError> {
        let m = &mut *self.map;
        let ours: Vec<&deckcraft_engine::Edited> =
            edits.iter().filter_map(|e| if let DeckEdit::Command(c) = e { Some(c.as_ref()) } else { None }).filter(|c| c.doc == m.doc).collect();
        if !ours.is_empty() && m.space == 0 {
            // No id may be minted before a space is granted (D2 review Minor 2); checked before
            // the safety net, so the reason is the connection, not the minted id.
            m.refused = Some(craft_live::CONNECTING.to_owned());
            return Err(LocalError::Refused(craft_live::CONNECTING.to_owned()));
        }
        // The safety net, over the whole batch: nothing is written unless all of it is shared.
        for c in &ours {
            if let Some(why) = unshared(&c.before, &c.after) {
                m.refused = Some(why.to_owned());
                return Err(LocalError::Refused(why.to_owned()));
            }
        }
        let plans: Vec<_> = ours.iter().map(|c| plan(replica, c, m.space, &m.uid)).collect();
        // A drop on a shape a peer deleted is refused with the deleter's name (Ruling D6); typed
        // text still lands in the tombstone (spec "delete beats edit").
        for (c, p) in ours.iter().zip(&plans) {
            // The engine labels a pointer gesture's report `pointer:<label>` (`Edited::command`);
            // only a drop is refused here, typing into a tombstone lands in it.
            if !c.command.starts_with("pointer:") {
                continue;
            }
            let dead = p.writes.iter().find_map(|w| match w {
                PlannedWrite::SetShape { id, .. } | PlannedWrite::Text { target: TextTarget::Shape(id), .. } => tomb_over(replica, *id),
                _ => None,
            });
            if let Some((t, by)) = dead {
                let why = line_for(t, &m.tombs.name(&by));
                m.refused = Some(why.clone());
                return Err(LocalError::Refused(why));
            }
        }
        let parts: Vec<Vec<Vec<crate::split::PlannedWrite>>> = plans.iter().map(|p| plan_split(&p.writes)).collect();
        // A part past the frame would be refused by the relay after this replica kept it, which
        // breaks the link: refuse the command before anything is written (L3 review M5). Each
        // part's size is an upper bound of its updates, measured as the relay measures them.
        let bounds: Vec<usize> = parts
            .iter()
            .map(|ps| ps.iter().map(|part| wire_bound(part.iter().map(crate::split::PlannedWrite::size).sum())).max().unwrap_or(0))
            .collect();
        if bounds.iter().any(|b| *b > WIRE_LIMIT) {
            m.refused = Some(EDIT_TOO_LARGE_TEXT.to_owned());
            return Err(LocalError::Refused(EDIT_TOO_LARGE_TEXT.to_owned()));
        }
        let mut updates = Vec::new();
        for ((p, parts), bound) in plans.iter().zip(parts).zip(bounds) {
            if p.writes.is_empty() {
                m.touched.whole.extend(p.slides.iter().copied());
                continue;
            }
            let before = updates.len();
            // One command, one undo step, however many updates carry it (L4 review M3).
            let mut g = Group { slides: p.slides.clone(), ..Group::default() };
            for part in parts {
                let (texts, keys): (Vec<_>, Vec<_>) = part.into_iter().partition(|w| matches!(w, crate::split::PlannedWrite::Text { .. }));
                // Key writes untracked, the text in one tracked transaction (Ruling D4).
                for (tracked, ws) in [(false, keys), (true, texts)] {
                    if ws.is_empty() {
                        continue;
                    }
                    match replica.write(tracked, |t| {
                        for w in &ws {
                            g.write_noted(t, w);
                        }
                        Ok(())
                    }) {
                        Ok(u) => {
                            updates.push(u);
                            if tracked {
                                g.text_items += 1;
                            }
                        }
                        Err(e) => {
                            // Whatever was written goes out; the engine is rebuilt from the replica.
                            if let Some(u) = e.update {
                                updates.push(u);
                                if tracked {
                                    g.text_items += 1;
                                }
                            }
                            log::warn!("deckcraft-live: a local write was refused: {}", e.error);
                            if updates.is_empty() {
                                return Err(LocalError::Failed(e.error.to_string()));
                            }
                            m.undo.record(g);
                            m.partial = true;
                            return Ok(updates);
                        }
                    }
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
            // My own deletes are known; work of mine that landed in a peer's tombstone (held
            // behind a drag or a composition) is told, with Restore.
            for w in &p.writes {
                if let PlannedWrite::Del { target, del: true, .. } = w {
                    m.tombs.dead.insert(*target);
                }
            }
            let landed_in = shapes.iter().filter_map(|id| tomb_over(replica, *id)).chain(p.slides.iter().filter_map(|id| slide_tomb(replica, *id)));
            let landed_in: Vec<(Tomb, String)> = landed_in.filter(|(_, by)| *by != m.uid).collect();
            for (t, by) in landed_in {
                m.tombs.dead.insert(t);
                m.tombs.tell(t, &by);
            }
            m.tombs.wrote(p.slides.iter().copied(), shapes.iter().copied());
            m.sent.push(SentGroup {
                slides: p.slides.iter().copied().collect(),
                shapes: shapes.into_iter().collect(),
                updates: updates.len() - before,
                wire: updates.iter().skip(before).map(|u| wire_of(u)).max().unwrap_or(0),
                bound,
            });
            m.undo.record(g);
            m.touched.whole.extend(p.slides.iter().copied());
        }
        Ok(updates)
    }
}

impl LiveApp<Deck> for DeckApp<'_> {
    fn local(&mut self, replica: &mut DeckDoc, edits: &[DeckEdit]) -> Result<Vec<Vec<u8>>, LocalError> {
        self.map.scene.set_view(Some(replica.view()));
        let updates = self.local_inner(replica, edits)?;
        // The engine's text and the replica's agree again: my caret's sticky indexes, for a remote
        // change landing before the next pump ends.
        if let Some(st) = self.current() {
            self.map.mine = crate::presence::mine(st, replica);
        }
        Ok(updates)
    }

    fn landed(&mut self) -> Landed {
        let resync = std::mem::take(&mut self.map.resync_after);
        if std::mem::take(&mut self.map.partial) {
            self.map.touched = Targets::default();
            return Landed::Partial;
        }
        if resync {
            self.map.touched = Targets::default();
            return Landed::Resync;
        }
        Landed::Done
    }

    fn put_back(&mut self, edits: Vec<DeckEdit>) -> Result<(), String> {
        let first = edits.iter().find_map(|e| if let DeckEdit::Command(c) = e { (c.doc == self.map.doc).then(|| c.before.clone()) } else { None });
        let Some(before) = first else { return Ok(()) };
        self.install((*before).clone())
    }

    fn received(&mut self, replica: &mut DeckDoc, changes: Vec<DeckChange>) -> (Vec<DeckChange>, Vec<Vec<u8>>) {
        self.map.scene.set_view(Some(replica.view()));
        (changes, Vec::new())
    }

    fn apply(&mut self, replica: &DeckDoc, changes: &[DeckChange]) -> Result<(), String> {
        self.map.scene.set_view(Some(replica.view()));
        self.note_deletes(replica, changes);
        self.land_or_hold(replica, Targets::from_changes(changes))
    }

    fn resync(&mut self, replica: &DeckDoc) -> Result<(), String> {
        self.map.scene.set_view(Some(replica.view()));
        // The whole deck replaces what was held and touched.
        self.map.held = Targets::default();
        self.map.touched = Targets::default();
        if self.session.remote_busy(self.map.doc) {
            // Wait for the gesture without rebuilding every frame; the hold's clock keeps
            // running, so a stale twist is still ended.
            self.map.resync_wanted = true;
            self.start_hold();
            return Ok(());
        }
        if std::mem::take(&mut self.map.tombs.reseed) {
            // After a reset: the tombstones the room's document holds are known, not news.
            self.map.tombs.dead = tombs_in(replica);
        }
        let p = whole(&self.map.base, replica, self.map.block.as_ref());
        self.install(p)?;
        if !self.map.resync_wanted {
            self.map.held_since = None;
        }
        Ok(())
    }

    fn reset(&mut self, space: u32) {
        // The replica was replaced: what it holds now is the room's, nothing to tell about. My
        // groups before it are gone from it too (L5 review M6); the resync that follows reads
        // the room's tombstones as known.
        self.map.tombs.dead.clear();
        self.map.tombs.told.clear();
        self.map.tombs.pending.clear();
        self.map.tombs.recent.clear();
        self.map.tombs.reseed = true;
        // The replaced replica's undo items are gone with it.
        self.map.undo.clear();
        self.map.held = Targets::default();
        self.map.held_since = None;
        self.map.touched = Targets::default();
        // The old replica's sticky indexes name nothing in the new one; a resync follows.
        self.map.scene.set_view(None);
        self.map.mine = None;
        self.set_space(space);
    }

    fn set_space(&mut self, space: u32) {
        let m = &mut *self.map;
        if m.space == space && m.block.is_some() {
            return;
        }
        m.space = space;
        // A new space starts a new block; the old block's ids stay taken (Ruling D5). Until a
        // valid space comes, the engine holds an empty block, which mints nothing.
        m.block = IdBlock::new(space);
        if m.block.is_none() && space != 0 {
            // A space past the block range (the relay's counter ran past it): edits go on, and a
            // minting command is refused with NO_SPACE_TEXT, not CONNECTING (L4 re-review N2).
            log::warn!("deckcraft-live: the relay granted space {space}, outside the id block range");
        }
        let block = Some(m.block.clone().unwrap_or_default());
        if let Some(st) = self.session.document_mut(m.doc) {
            // Allocation state, not content: decks compare equal whatever block they hold.
            Arc::make_mut(&mut st.doc).id_block = block;
        }
    }

    fn caret(&self, payload: &Value) -> Option<DeckCaret> {
        self.see();
        self.map.scene.caret(payload)
    }

    fn unmerged(&mut self, state: &[u8], edits: usize) {
        log::warn!("deckcraft-live: {edits} local edits the room does not have are kept as an unmerged copy");
        self.map.keep_unmerged(state.to_vec());
    }

    fn follow(&self, carets: &mut HashMap<String, DeckCaret>) {
        self.see();
        carets.retain(|_, c| self.map.scene.follow(c));
    }
}
