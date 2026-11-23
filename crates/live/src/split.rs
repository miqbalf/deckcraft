//! One command's writes cut into updates under [`UPDATE_BUDGET`] (Ruling D10).
//!
//! Writes are grouped by the entry they touch: a shape's keys, its text and its tombstone travel
//! in one update, as do a slide's, so a receiver never sees half a shape. Groups keep the order of
//! their first write and each keeps its own order. Sizes are estimated from the content (twice
//! its JSON length plus a fixed cost per key), leaving room for yrs's framing under the relay's
//! 1 MiB frame; a test checks real updates stay under it.

use std::collections::HashMap;
use std::sync::Arc;

use deckcraft_model::{Shape, ShapeId, Slide, SlideId};
use serde_json::Value;

use crate::doc::{DeckTxn, TextTarget, Tomb};
use crate::schema::UPDATE_BUDGET;
use crate::text::{Chunk, TextOp};

/// Estimated bytes of one key and its framing.
const KEY_COST: usize = 48;

/// One write a command plans, applied through [`DeckTxn`].
#[derive(Clone, Debug, PartialEq)]
pub enum PlannedWrite {
    PutSlide { slide: Arc<Slide>, pos: String },
    SetSlide { id: SlideId, key: String, value: Value },
    PutShape { slide: SlideId, parent: Option<ShapeId>, z: String, shape: Box<Shape> },
    SetShape { id: ShapeId, key: String, value: Value },
    Del { target: Tomb, del: bool, by: String },
    Text { target: TextTarget, ops: Vec<TextOp> },
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

    /// Estimated encoded size.
    fn size(&self) -> usize {
        let json = |v: &dyn erased::Json| v.len() * 2;
        match self {
            PlannedWrite::PutSlide { slide, pos } => KEY_COST * 12 + pos.len() + json(slide.as_ref()),
            PlannedWrite::SetSlide { key, value, .. } | PlannedWrite::SetShape { key, value, .. } => KEY_COST + key.len() + json(value),
            PlannedWrite::PutShape { z, shape, .. } => KEY_COST * 26 + z.len() + json(shape.as_ref()),
            PlannedWrite::Del { by, .. } => KEY_COST * 2 + by.len(),
            PlannedWrite::Text { ops, .. } => ops.iter().map(op_size).sum(),
        }
    }
}

fn op_size(op: &TextOp) -> usize {
    let attrs = |a: &[(String, Value)]| a.iter().map(|(k, v)| KEY_COST + k.len() + v.to_string().len() * 2).sum::<usize>();
    match op {
        TextOp::Insert { chunks, .. } => chunks
            .iter()
            .map(|c| match c {
                Chunk::Text { text, attrs: a } => KEY_COST + text.len() * 2 + attrs(a),
                Chunk::Embed { value, attrs: a } => KEY_COST + value.to_string().len() * 2 + attrs(a),
            })
            .sum(),
        TextOp::Delete { .. } => KEY_COST,
        TextOp::Format { attrs: a, .. } => {
            KEY_COST + a.iter().map(|(k, v)| KEY_COST + k.len() + v.as_ref().map_or(4, |v| v.to_string().len() * 2)).sum::<usize>()
        }
    }
}

mod erased {
    /// JSON length of anything serializable (0 when it does not encode).
    pub trait Json {
        fn len(&self) -> usize;
    }
    impl<T: serde::Serialize> Json for T {
        fn len(&self) -> usize {
            serde_json::to_string(self).map_or(0, |s| s.len())
        }
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
