//! Tombstones as the person meets them (spec "delete beats edit", Ruling D8): a slide or shape a
//! peer deleted stays in the document with `del` and `delBy`, so an edit made to it concurrently
//! lands in it. The person whose work was there is told who deleted it, with a Restore action;
//! Restore writes `del = false` as a new group. A drop on a deleted shape is refused with the
//! deleter's name.

use std::collections::{BTreeSet, HashMap, VecDeque};

use deckcraft_model::{ShapeId, SlideId};

use crate::doc::{DeckDoc, Tomb};
use crate::split::PlannedWrite;

/// The label of a notice's Restore action.
pub const RESTORE_LABEL: &str = "Restore";

/// The line of a notice about a deleted shape: "{name} deleted this shape".
#[must_use]
pub fn deleted_text(name: &str) -> String {
    format!("{name} deleted this shape")
}

/// The line of a notice about a deleted slide: "{name} deleted this slide".
#[must_use]
pub fn deleted_slide_text(name: &str) -> String {
    format!("{name} deleted this slide")
}

/// The line for `t`, deleted by `name`.
pub(crate) fn line_for(t: Tomb, name: &str) -> String {
    match t {
        Tomb::Slide(_) => deleted_slide_text(name),
        Tomb::Shape(_) => deleted_text(name),
    }
}

/// How many of my latest groups count as "my work" in a deleted slide or shape (L5 rule 1).
pub(crate) const RECENT_GROUPS: usize = 20;

/// The longest parent chain walked (the model walks 64 levels).
const DEPTH: usize = 64;

/// What the mapping keeps about tombstones between calls.
#[derive(Default)]
pub(crate) struct Tombs {
    /// The tombstones this peer has seen or made; a remote change naming one not in here is a
    /// new delete.
    pub dead: BTreeSet<Tomb>,
    /// Tombstones the person was told about; told once until restored.
    pub told: BTreeSet<Tomb>,
    /// The slides and shapes of my latest [`RECENT_GROUPS`] groups, newest last.
    pub recent: VecDeque<(BTreeSet<SlideId>, BTreeSet<ShapeId>)>,
    /// Display names by uid, from every roster seen (a deleter may have left since).
    pub names: HashMap<String, String>,
    /// Deletes to tell the person about, with the deleter's uid; named after the pump.
    pub pending: Vec<(Tomb, String)>,
}

impl Tombs {
    /// One of my groups went out.
    pub(crate) fn wrote(&mut self, slides: impl IntoIterator<Item = SlideId>, shapes: impl IntoIterator<Item = ShapeId>) {
        self.recent.push_back((slides.into_iter().collect(), shapes.into_iter().collect()));
        while self.recent.len() > RECENT_GROUPS {
            self.recent.pop_front();
        }
    }

    /// The display name of `uid`, or the uid itself when no roster named it.
    pub(crate) fn name(&self, uid: &str) -> String {
        self.names.get(uid).cloned().unwrap_or_else(|| uid.to_owned())
    }

    /// Tell the person about `t` (deleted by `by`) unless they were told already.
    pub(crate) fn tell(&mut self, t: Tomb, by: &str) {
        if self.told.insert(t) {
            self.pending.push((t, by.to_owned()));
        }
    }

    /// `t` is live again.
    pub(crate) fn revived(&mut self, t: Tomb) {
        self.dead.remove(&t);
        self.told.remove(&t);
        self.pending.retain(|(p, _)| *p != t);
    }

    /// Whether my latest groups wrote to `t` or to something inside it.
    pub(crate) fn mine(&self, doc: &DeckDoc, t: Tomb) -> bool {
        self.recent.iter().any(|(slides, shapes)| match t {
            Tomb::Slide(s) => slides.contains(&s) || shapes.iter().any(|id| slide_of(doc, *id) == Some(s)),
            Tomb::Shape(g) => shapes.iter().any(|id| inside(doc, *id, g)),
        })
    }
}

/// The slide shape `id` is on, by its entry.
pub(crate) fn slide_of(doc: &DeckDoc, id: ShapeId) -> Option<SlideId> {
    doc.shape_entries().get(&id).map(|e| e.slide)
}

/// Whether shape `id` is `g` or sits in group `g`.
pub(crate) fn inside(doc: &DeckDoc, id: ShapeId, g: ShapeId) -> bool {
    let mut at = Some(id);
    for _ in 0..DEPTH {
        let Some(cur) = at else { return false };
        if cur == g {
            return true;
        }
        at = doc.shape_entries().get(&cur).and_then(|e| e.parent);
    }
    false
}

/// The tombstone shape `id` is under, nearest first: itself, a group above it, or its slide;
/// with the deleter's uid (empty when the entry names none).
pub(crate) fn tomb_over(doc: &DeckDoc, id: ShapeId) -> Option<(Tomb, String)> {
    let first = doc.shape(id)?;
    let slide = first.slide;
    let mut at = Some(id);
    for _ in 0..DEPTH {
        let Some(cur) = at else { break };
        let Some(e) = doc.shape(cur) else { break };
        if e.del {
            return Some((Tomb::Shape(cur), e.del_by.unwrap_or_default()));
        }
        at = e.parent;
    }
    slide_tomb(doc, slide)
}

/// The tombstone of slide `id`, with the deleter's uid.
pub(crate) fn slide_tomb(doc: &DeckDoc, id: SlideId) -> Option<(Tomb, String)> {
    let e = doc.slide(id)?;
    e.del.then(|| (Tomb::Slide(id), e.del_by.unwrap_or_default()))
}

/// Whether `t` is a tombstone in `doc`, and its deleter.
pub(crate) fn tombstoned(doc: &DeckDoc, t: Tomb) -> Option<String> {
    match t {
        Tomb::Slide(id) => slide_tomb(doc, id).map(|(_, by)| by),
        Tomb::Shape(id) => doc.shape(id).filter(|e| e.del).map(|e| e.del_by.unwrap_or_default()),
    }
}

/// The writes that bring `t` back: `del = false` for it, for the groups above a shape that are
/// tombstoned and for its slide when that is tombstoned too. Empty when nothing is deleted.
pub(crate) fn restore_writes(doc: &DeckDoc, t: Tomb, uid: &str) -> Vec<PlannedWrite> {
    let mut targets = Vec::new();
    match t {
        Tomb::Slide(id) => {
            if slide_tomb(doc, id).is_some() {
                targets.push(t);
            }
        }
        Tomb::Shape(id) => {
            let Some(first) = doc.shape(id) else { return Vec::new() };
            let mut at = Some(id);
            for _ in 0..DEPTH {
                let Some(cur) = at else { break };
                let Some(e) = doc.shape(cur) else { break };
                if e.del {
                    targets.push(Tomb::Shape(cur));
                }
                at = e.parent;
            }
            if slide_tomb(doc, first.slide).is_some() {
                targets.push(Tomb::Slide(first.slide));
            }
        }
    }
    targets.into_iter().map(|target| PlannedWrite::Del { target, del: false, by: uid.to_owned() }).collect()
}
