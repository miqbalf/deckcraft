//! One command's writes cut into updates under [`UPDATE_BUDGET`] (Ruling D10).
//!
//! Writes are grouped by the entry they touch: a shape's keys, its text and its tombstone travel
//! in one update, as do a slide's, so a receiver never sees half a shape. Groups keep the order of
//! their first write and each keeps its own order. Sizes are upper bounds of what each write adds
//! to an update ([`PlannedWrite::size`]); the gate turns them into wire bytes against the
//! relay's frame before anything is written.

use std::collections::HashMap;
use std::sync::Arc;

use deckcraft_model::{Shape, ShapeId, Slide, SlideId};
use serde_json::Value;

use crate::doc::{DeckTxn, TextTarget, Tomb};
use crate::schema::UPDATE_BUDGET;
use crate::text::{Chunk, TextOp};

/// One write a command plans, applied through [`DeckTxn`].
#[derive(Clone, Debug, PartialEq)]
pub enum PlannedWrite {
    PutSlide {
        slide: Arc<Slide>,
        pos: String,
    },
    SetSlide {
        id: SlideId,
        key: String,
        value: Value,
    },
    /// One shape entry. A group's children are not written with it (its `kind` goes without
    /// them): each descendant needs its own `PutShape` with `parent` set to its group.
    PutShape {
        slide: SlideId,
        parent: Option<ShapeId>,
        z: String,
        shape: Box<Shape>,
    },
    SetShape {
        id: ShapeId,
        key: String,
        value: Value,
    },
    Del {
        target: Tomb,
        del: bool,
        by: String,
    },
    Text {
        target: TextTarget,
        ops: Vec<TextOp>,
    },
}

/// The entry a write belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Owner {
    Slide(SlideId),
    Shape(ShapeId),
}

impl PlannedWrite {
    /// Make the write in `t`.
    pub fn write(&self, t: &mut DeckTxn) {
        match self {
            PlannedWrite::PutSlide { slide, pos } => t.put_slide(slide, pos),
            PlannedWrite::SetSlide { id, key, value } => t.set_slide(*id, key, value.clone()),
            PlannedWrite::PutShape { slide, parent, z, shape } => t.put_shape(*slide, *parent, z, shape),
            PlannedWrite::SetShape { id, key, value } => t.set_shape(*id, key, value.clone()),
            PlannedWrite::Del { target, del, by } => t.set_del(*target, *del, by),
            PlannedWrite::Text { target, ops } => t.text_ops(target.clone(), ops),
        }
    }

    fn owner(&self) -> Owner {
        match self {
            PlannedWrite::PutSlide { slide, .. } => Owner::Slide(slide.id),
            PlannedWrite::SetSlide { id, .. } | PlannedWrite::Text { target: TextTarget::Notes(id), .. } => Owner::Slide(*id),
            PlannedWrite::PutShape { shape, .. } => Owner::Shape(shape.id),
            PlannedWrite::SetShape { id, .. } | PlannedWrite::Text { target: TextTarget::Shape(id), .. } => Owner::Shape(*id),
            PlannedWrite::Del { target: Tomb::Slide(id), .. } => Owner::Slide(*id),
            PlannedWrite::Del { target: Tomb::Shape(id), .. } => Owner::Shape(*id),
        }
    }

    /// An upper bound of the bytes this write adds to a yrs update (v1): every item counted
    /// with the largest header yrs gives one, every value at its exact lib0 encoding, every
    /// string at its UTF-8 length, and two format items per attribute per stretch of text.
    pub(crate) fn size(&self) -> usize {
        match self {
            // What `put_slide` writes: the slide's own keys, `pos`, `del`, `layout` and its notes;
            // its shapes are `PutShape` writes of their own.
            PlannedWrite::PutSlide { slide, pos } => {
                let keys: usize = crate::doc::slide_values(slide).iter().map(|(k, v)| key_size(k, v)).sum();
                ITEM * 5 + pos.len() + keys + body_size(&slide.notes.paragraphs)
            }
            PlannedWrite::SetSlide { key, value, .. } | PlannedWrite::SetShape { key, value, .. } => key_size(key, value),
            // What `put_shape` writes: the entry's own keys (a group's without its children), its
            // `slide`, `parent`, `z`, `del` and the text body.
            PlannedWrite::PutShape { z, shape, .. } => {
                let keys: usize = crate::doc::shape_values(shape).iter().map(|(k, v)| key_size(k, v)).sum();
                ITEM * 7 + z.len() + keys + shape.text.as_ref().map_or(0, |t| body_size(&t.paragraphs))
            }
            PlannedWrite::Del { by, .. } => ITEM * 2 + by.len(),
            PlannedWrite::Text { ops, .. } => ops.iter().map(op_size).sum(),
        }
    }
}

/// The most bytes one item's header takes in an update: info, both origins (client and clock
/// varints), the parent, the content tag and the length prefixes.
const ITEM: usize = 64;

/// The exact lib0 encoding of `v` as yrs stores it.
pub(crate) fn any_size(v: &Value) -> usize {
    let mut out: Vec<u8> = Vec::new();
    craft_collab::replica::to_any(v).encode(&mut out);
    out.len()
}

fn key_size(key: &str, v: &Value) -> usize {
    ITEM + key.len() + any_size(v)
}

/// A text body written into a new YText: its content and every attribute stretch.
fn body_size(paragraphs: &[deckcraft_model::text::Paragraph]) -> usize {
    crate::text::edit_ops(&[], paragraphs).iter().map(op_size).sum()
}

/// Two format items (open and close) for each attribute.
fn attrs_size<'a>(a: impl Iterator<Item = (&'a String, Option<&'a Value>)>) -> usize {
    a.map(|(k, v)| 2 * (ITEM + k.len() + v.map_or(1, any_size))).sum()
}

fn op_size(op: &TextOp) -> usize {
    match op {
        TextOp::Insert { chunks, .. } => chunks
            .iter()
            .map(|c| match c {
                Chunk::Text { text, attrs: a } => ITEM + text.len() + attrs_size(a.iter().map(|(k, v)| (k, Some(v)))),
                Chunk::Embed { value, attrs: a } => ITEM + any_size(value) + attrs_size(a.iter().map(|(k, v)| (k, Some(v)))),
            })
            .sum(),
        // A delete splits at most two items and adds one delete-set range.
        TextOp::Delete { .. } => ITEM * 3,
        TextOp::Format { attrs: a, .. } => ITEM * 2 + attrs_size(a.iter().map(|(k, v)| (k, v.as_ref()))),
    }
}
/// Cut one command's writes into updates under [`UPDATE_BUDGET`]: a shape's (or a slide's) keys
/// and text never split. An entry whose writes alone pass the budget goes in an update of its own.
#[must_use]
pub fn plan_split(writes: &[PlannedWrite]) -> Vec<Vec<PlannedWrite>> {
    let mut groups: Vec<(usize, Vec<PlannedWrite>)> = Vec::new();
    let mut at: HashMap<Owner, usize> = HashMap::new();
    for w in writes {
        let i = *at.entry(w.owner()).or_insert_with(|| {
            groups.push((0, Vec::new()));
            groups.len() - 1
        });
        if let Some(g) = groups.get_mut(i) {
            g.0 += w.size();
            g.1.push(w.clone());
        }
    }
    let mut out: Vec<Vec<PlannedWrite>> = Vec::new();
    let mut used = 0;
    for (size, g) in groups {
        if out.is_empty() || (used > 0 && used + size > UPDATE_BUDGET) {
            out.push(Vec::new());
            used = 0;
        }
        used += size;
        if let Some(last) = out.last_mut() {
            last.extend(g);
        }
    }
    out
}
