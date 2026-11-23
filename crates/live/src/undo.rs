//! Per-user undo and redo (Ruling D4).
//!
//! DeckCraft keeps its own log of command groups. The engine's live undo hook queues
//! [`crate::DeckEdit::Undo`] / [`crate::DeckEdit::Redo`]; the mapping runs them here at the next flush.
//! One command is one [`Group`], however many updates carried it (L4 review M3: grouped by the
//! command's `SentGroup`, never by transaction):
//! - its text, as the yrs undo items of its tracked transactions (`text_items`), so an undo takes
//!   back my characters and only mine;
//! - its latest-wins keys, as guarded inverses ([`KeyWrite`]): a key goes back to `before` only
//!   while it still holds what the command wrote (`after`); a key someone changed since is left
//!   alone and the person is told once ([`UNDO_SKIPPED_TEXT`]).
//!
//! A new slide or shape is undone by a tombstone (`del` true), never by removing its entry; a
//! delete is undone through the Restore writes (L5), unless a group above or the slide was deleted
//! by someone else since: then the step is skipped and told. The undo itself is one new group, sent in
//! parts under the update budget, and its inverse goes on the other stack, so Redo mirrors Undo.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use deckcraft_model::SlideId;
use serde_json::Value;

use crate::doc::{DeckDoc, DeckTxn, KeyRef, TextTarget, Tomb, WriteError};
use crate::schema::{DEL, UPDATE_BUDGET};
use crate::split::PlannedWrite;
use crate::tomb::{restore_writes, slide_of, tombstoned};

/// Why part of an undo or redo was not done: someone else changed those properties since.
pub const UNDO_SKIPPED_TEXT: &str = "Someone else changed this since; not undone";

/// Why commands made right after Undo or Redo, before the next pump, were not shared: they were
/// made on the deck as it was before the undo landed. The deck shows the room's again.
pub const AFTER_UNDO_TEXT: &str = "An edit made right after Undo was not kept. Please make it again.";

/// How many commands the log keeps; older ones can no longer be undone.
pub const UNDO_DEPTH: usize = 500;

/// One latest-wins key a command wrote: what it held before (None: absent, or the entry did not
/// exist yet) and what the command left in it, both as the replica reads them back.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyWrite {
    pub key: KeyRef,
    pub before: Option<Value>,
    pub after: Value,
}

/// One command of mine: its tracked text transactions and its key writes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Group {
    pub text_items: u32,
    pub writes: Vec<KeyWrite>,
    /// The slides it touched, rebuilt in the engine once its undo or redo is written.
    pub(crate) slides: BTreeSet<SlideId>,
    /// The texts its tracked transactions wrote: a text undo that changes another one reached
    /// an older step of mine (L6 review I2).
    pub(crate) texts: Vec<TextTarget>,
}

impl Group {
    pub(crate) fn is_empty(&self) -> bool {
        self.text_items == 0 && self.writes.is_empty()
    }

    /// Note a key write made in a command: the first `before` and the last `after` of a key count.
    pub(crate) fn note(&mut self, key: KeyRef, before: Option<Value>, after: Value) {
        match self.writes.iter_mut().find(|w| w.key == key) {
            Some(w) => w.after = after,
            None => self.writes.push(KeyWrite { key, before, after }),
        }
    }

    /// Make `w` in `t`, noting the key it writes ([`key_of`]) before and after.
    pub(crate) fn write_noted(&mut self, t: &mut DeckTxn, w: &PlannedWrite) {
        let Some((key, new)) = key_of(w) else {
            if let PlannedWrite::Text { target, .. } = w
                && !self.texts.contains(target)
            {
                self.texts.push(target.clone());
            }
            w.write(t);
            return;
        };
        let before = if new { None } else { t.read(key.clone()) };
        w.write(t);
        if let Some(after) = t.read(key.clone()) {
            self.note(key, before, after);
        }
    }
}

/// The latest-wins key a planned write sets, and whether it makes a new entry (whose undo is a
/// tombstone). Text writes have none: they are yrs undo items.
pub(crate) fn key_of(w: &PlannedWrite) -> Option<(KeyRef, bool)> {
    match w {
        PlannedWrite::PutSlide { slide, .. } => Some((KeyRef::Slide(slide.id, DEL.to_owned()), true)),
        PlannedWrite::PutShape { shape, .. } => Some((KeyRef::Shape(shape.id, DEL.to_owned()), true)),
        PlannedWrite::SetSlide { id, key, .. } => Some((KeyRef::Slide(*id, key.clone()), false)),
        PlannedWrite::SetShape { id, key, .. } => Some((KeyRef::Shape(*id, key.clone()), false)),
        PlannedWrite::Del { target: Tomb::Slide(id), .. } => Some((KeyRef::Slide(*id, DEL.to_owned()), false)),
        PlannedWrite::Del { target: Tomb::Shape(id), .. } => Some((KeyRef::Shape(*id, DEL.to_owned()), false)),
        PlannedWrite::Text { .. } => None,
    }
}

/// What the Undo and Redo menu items may offer, shared with the engine's live undo hook so its
/// answer (`{"live": true, "taken": bool}`) is honest.
#[derive(Debug, Default)]
pub(crate) struct Offer {
    undo: AtomicBool,
    redo: AtomicBool,
}

impl Offer {
    pub(crate) fn has(&self, redo: bool) -> bool {
        if redo { self.redo.load(Ordering::Relaxed) } else { self.undo.load(Ordering::Relaxed) }
    }
}

/// This person's undo and redo stacks of command groups.
#[derive(Debug, Default)]
pub struct UndoLog {
    undo: Vec<Group>,
    redo: Vec<Group>,
    offer: Arc<Offer>,
}

impl UndoLog {
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub(crate) fn offer(&self) -> Arc<Offer> {
        Arc::clone(&self.offer)
    }

    fn publish(&self) {
        self.offer.undo.store(self.can_undo(), Ordering::Relaxed);
        self.offer.redo.store(self.can_redo(), Ordering::Relaxed);
    }

    /// A new local command: it can be undone, and nothing undone before it can be redone.
    pub(crate) fn record(&mut self, g: Group) {
        if g.is_empty() {
            return;
        }
        self.undo.push(g);
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.publish();
    }

    /// The group Undo (`redo` false) or Redo takes next.
    pub(crate) fn take(&mut self, redo: bool) -> Option<Group> {
        let g = if redo { self.redo.pop() } else { self.undo.pop() };
        self.publish();
        g
    }

    /// A group taken whose undo or redo wrote nothing goes back where it was.
    pub(crate) fn untake(&mut self, redo: bool, g: Group) {
        if redo {
            self.redo.push(g)
        } else {
            self.undo.push(g)
        }
        self.publish();
    }

    /// The inverse of what was just undone (`redo` false) or redone goes on the other stack.
    /// An inverse that would do nothing is not offered (L6 review M1).
    pub(crate) fn done(&mut self, redo: bool, inverse: Group) {
        if inverse.is_empty() {
            self.publish();
            return;
        }
        if redo {
            self.undo.push(inverse)
        } else {
            self.redo.push(inverse)
        }
        self.publish();
    }

    /// The replica was replaced: its undo items are gone, and so is what the log names.
    pub(crate) fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.publish();
    }
}

/// One guarded inverse to make: the key write it reverts and the writes that do it (None: remove
/// the key).
struct Step {
    kw: KeyWrite,
    writes: Option<Vec<PlannedWrite>>,
}

impl Step {
    fn size(&self) -> usize {
        self.writes.as_ref().map_or(64, |ws| ws.iter().map(PlannedWrite::size).sum())
    }
}

fn target_of(key: &KeyRef) -> Tomb {
    match key {
        KeyRef::Slide(id, _) => Tomb::Slide(*id),
        KeyRef::Shape(id, _) => Tomb::Shape(*id),
    }
}

/// The writes that put `kw.before` back; None when the step is skipped (counted and told).
fn step_for(replica: &DeckDoc, kw: KeyWrite, uid: &str) -> Option<Step> {
    let (KeyRef::Slide(_, k) | KeyRef::Shape(_, k)) = &kw.key;
    let writes = if k == DEL {
        let target = target_of(&kw.key);
        match &kw.before {
            // A new entry (None) or a restored one goes back to a tombstone.
            None | Some(Value::Bool(true)) => Some(vec![PlannedWrite::Del { target, del: true, by: uid.to_owned() }]),
            // A delete is undone through the Restore writes (L5): the groups above a shape and
            // its slide come back with it when they are tombstoned too.
            Some(Value::Bool(false)) => {
                let ws = restore_writes(replica, target, uid);
                // A group above or the slide a peer deleted since stays deleted: Ctrl+Z takes back
                // only my own delete (L6 review I1). The peer's delete notice offers Restore.
                let theirs = ws.iter().any(|w| match w {
                    PlannedWrite::Del { target: t, .. } if *t != target => tombstoned(replica, *t).is_some_and(|by| by != uid),
                    _ => false,
                });
                if theirs {
                    return None;
                }
                Some(if ws.is_empty() { vec![PlannedWrite::Del { target, del: false, by: uid.to_owned() }] } else { ws })
            }
            Some(_) => {
                log::warn!("deckcraft-live: a logged del value does not read; that step is skipped");
                return None;
            }
        }
    } else {
        kw.before.as_ref().map(|v| {
            vec![match &kw.key {
                KeyRef::Slide(id, key) => PlannedWrite::SetSlide { id: *id, key: key.clone(), value: v.clone() },
                KeyRef::Shape(id, key) => PlannedWrite::SetShape { id: *id, key: key.clone(), value: v.clone() },
            }]
        })
    };
    Some(Step { kw, writes })
}

/// What an undo or redo did.
#[derive(Default)]
pub(crate) struct Ran {
    pub updates: Vec<Vec<u8>>,
    /// The group that takes it back, for the other stack.
    pub inverse: Group,
    /// Keys left alone because someone changed them since.
    pub skipped: usize,
    /// The gate's bound for its largest key part.
    pub bound: usize,
    /// A write failed part way: what was written is in `updates`.
    pub partial: bool,
    /// Tombstones it wrote (`true`) or lifted (`false`).
    pub dels: Vec<(Tomb, bool)>,
    /// The texts its text undo or redo changed.
    pub texts: Vec<TextTarget>,
}

/// Undo (`redo` false) or redo `g` on `replica`: every key still holding what `g` left gets its
/// earlier value back (untracked), the others are skipped; then `g.text_items` yrs undo (or redo)
/// items. The key writes go in parts under [`UPDATE_BUDGET`].
///
/// # Errors
/// A write that failed before anything was written.
pub(crate) fn run(replica: &mut DeckDoc, g: Group, redo: bool, uid: &str) -> Result<Ran, WriteError> {
    let mut ran = Ran { inverse: Group { slides: g.slides.clone(), texts: g.texts.clone(), ..Group::default() }, ..Ran::default() };
    let mut steps: Vec<Step> = Vec::new();
    for kw in g.writes.into_iter().rev() {
        match step_for(replica, kw, uid) {
            Some(s) => steps.push(s),
            None => ran.skipped += 1,
        }
    }
    // Parts under the budget, in order.
    let mut parts: Vec<Vec<Step>> = Vec::new();
    let mut used = 0;
    for s in steps {
        let n = s.size();
        if parts.is_empty() || (used > 0 && used + n > UPDATE_BUDGET) {
            parts.push(Vec::new());
            used = 0;
        }
        used += n;
        if let Some(p) = parts.last_mut() {
            p.push(s);
        }
    }
    for part in parts {
        let bound = crate::app::wire_bound(part.iter().map(Step::size).sum());
        ran.bound = ran.bound.max(bound);
        let (mut inverse, mut skipped, mut dels) = (Vec::new(), 0, Vec::new());
        let r = replica.write(false, |t| {
            for s in &part {
                let now = t.read(s.kw.key.clone()).unwrap_or(Value::Null);
                if now != s.kw.after {
                    skipped += 1;
                    continue;
                }
                match &s.writes {
                    Some(ws) => {
                        for w in ws {
                            w.write(t);
                            if let PlannedWrite::Del { target, del, .. } = w {
                                dels.push((*target, *del));
                            }
                        }
                    }
                    None => t.unset(&s.kw.key),
                }
                let after = t.read(s.kw.key.clone()).unwrap_or(Value::Null);
                inverse.push(KeyWrite { key: s.kw.key.clone(), before: Some(now), after });
            }
            Ok(())
        });
        // An unset key reads as null: its inverse puts the old value back over "absent".
        ran.skipped += skipped;
        ran.dels.extend(dels);
        ran.inverse.writes.extend(inverse);
        match r {
            Ok(u) => {
                if u.as_slice() != craft_collab::yrs::Update::EMPTY_V1 {
                    ran.updates.push(u);
                }
            }
            Err(e) => {
                log::warn!("deckcraft-live: an undo write was refused: {}", e.error);
                match e.update {
                    Some(u) => ran.updates.push(u),
                    None if ran.updates.is_empty() => return Err(e),
                    None => {}
                }
                ran.partial = true;
                return Ok(ran);
            }
        }
    }
    for _ in 0..g.text_items {
        let Some((u, touched)) = (if redo { replica.redo_text() } else { replica.undo_text() }) else {
            // The replica's undo items are gone (a new client id rebuilt it, L6 review M6): the
            // person is told the text was not undone.
            ran.skipped += 1;
            break;
        };
        ran.updates.push(u);
        let inside = touched.iter().all(|t| g.texts.contains(t));
        add_texts(&mut ran.texts, touched);
        if inside {
            ran.inverse.text_items += 1;
            continue;
        }
        // yrs skips an item that changes nothing (a peer deleted all of those characters) and
        // takes the next one, an older step of mine in another text: put that one back at
        // once, so the older text stays and the log and the yrs stacks agree again (L6 review I2).
        if let Some((u, back)) = if redo { replica.undo_text() } else { replica.redo_text() } {
            ran.updates.push(u);
            add_texts(&mut ran.texts, back);
        }
        ran.skipped += 1;
        break;
    }
    // The inverse is undone in reverse again: its keys in the order `g` wrote them.
    ran.inverse.writes.reverse();
    // Shapes written: their slides are rebuilt too.
    let more: Vec<SlideId> = ran
        .inverse
        .writes
        .iter()
        .filter_map(|w| match &w.key {
            KeyRef::Slide(id, _) => Some(*id),
            KeyRef::Shape(id, _) => slide_of(replica, *id),
        })
        .collect();
    ran.inverse.slides.extend(more);
    let texts: Vec<SlideId> = ran
        .texts
        .iter()
        .filter_map(|t| match t {
            TextTarget::Shape(id) => slide_of(replica, *id),
            TextTarget::Notes(id) => Some(*id),
        })
        .collect();
    ran.inverse.slides.extend(texts);
    Ok(ran)
}

fn add_texts(into: &mut Vec<TextTarget>, more: Vec<TextTarget>) {
    for t in more {
        if !into.contains(&t) {
            into.push(t);
        }
    }
}
