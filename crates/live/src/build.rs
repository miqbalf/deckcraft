//! The deck a replica describes: the base plus every entry, tombstoned shapes removed through
//! the one delete routine (Ruling D7). The oracle of the tests and the worker's save.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use craft_collab::{LiveError, Replica};
use deckcraft_model::text::Paragraph;
use deckcraft_model::{Presentation, Shape, ShapeId, ShapeKind, Slide, SlideId};

use crate::doc::{DeckDoc, TextTarget, apply_shape_field, apply_slide_field};

/// The deck for `state` over `base`: equal for equal (base, state) on every target.
pub fn deck_from_state(base: &Presentation, state: &[u8]) -> Result<Presentation, LiveError> {
    let doc = DeckDoc::from_state(state)?;
    Ok(deck_from_doc(base, &doc))
}

/// The deck a live replica describes, with no encode. Values a peer wrote as junk read as the
/// earlier good value this replica saw (as the base's when it saw none).
///
/// `next_id` stays the base's: ids minted live sit at or above `LIVE_ID_BASE` and are never
/// counted into it, so a block-less session over the result keeps minting below the live range.
#[must_use]
pub fn deck_from_doc(base: &Presentation, doc: &DeckDoc) -> Presentation {
    let mut out = base.clone();
    let base_slides: HashMap<SlideId, &Arc<Slide>> = base.slides.iter().map(|s| (s.id, s)).collect();
    let mut base_shapes: HashMap<ShapeId, &Shape> = HashMap::new();
    for s in &base.slides {
        deckcraft_model::walk(&s.shapes, &mut |sh, _| {
            base_shapes.entry(sh.id).or_insert(sh);
        });
    }
    let mut slides = Vec::new();
    let mut tombs = Vec::new();
    for id in doc.slide_order() {
        let Some(e) = doc.slide(id) else { continue };
        let earlier = base_slides.get(&id).map(|s| s.as_ref());
        let mut slide = earlier.cloned().unwrap_or_else(|| Slide { id, ..Default::default() });
        slide.layout = e.layout;
        for (k, v) in &e.fields {
            apply_slide_field(&mut slide, k, v);
        }
        if let Some(ps) = doc.text(TextTarget::Notes(id)) {
            slide.notes.paragraphs = keep_base(&slide.notes.paragraphs, ps);
        }
        let (shapes, dead) = build_tree(doc, id, &base_shapes);
        slide.shapes = shapes;
        tombs.push(dead);
        let slide = match base_slides.get(&id) {
            Some(b) if b.as_ref() == &slide => Arc::clone(b),
            _ => Arc::new(slide),
        };
        slides.push(slide);
    }
    out.slides = slides;
    for (i, dead) in tombs.into_iter().enumerate() {
        if !dead.is_empty() {
            delete_shapes_in(&mut out, i, &dead);
        }
    }
    out
}

/// The base's own paragraphs when the replica holds them unchanged (a YText keeps only the
/// normalized form), else the replica's.
fn keep_base(base: &[Paragraph], live: Vec<Paragraph>) -> Vec<Paragraph> {
    let same = base.len() == live.len()
        && base.iter().zip(&live).all(|(b, l)| {
            let mut b = b.clone();
            b.normalize();
            &b == l
        });
    if same { base.to_vec() } else { live }
}

/// The slide's shape tree, tombstones included, and the tombstoned ids.
fn build_tree(doc: &DeckDoc, slide: SlideId, base: &HashMap<ShapeId, &Shape>) -> (Vec<Shape>, Vec<ShapeId>) {
    let mut kids: BTreeMap<Option<ShapeId>, Vec<(ShapeId, String)>> = BTreeMap::new();
    let mut dead = Vec::new();
    for (id, parent) in doc.tree_of(slide) {
        if let Some(e) = doc.shape(id) {
            if e.del {
                dead.push(id);
            }
            kids.entry(parent).or_default().push((id, e.z));
        }
    }
    let shapes = build_level(doc, &kids, None, base, 0);
    (shapes, dead)
}

fn build_level(
    doc: &DeckDoc,
    kids: &BTreeMap<Option<ShapeId>, Vec<(ShapeId, String)>>,
    at: Option<ShapeId>,
    base: &HashMap<ShapeId, &Shape>,
    depth: usize,
) -> Vec<Shape> {
    let Some(list) = kids.get(&at) else { return Vec::new() };
    crate::order::order(list.iter().cloned()).into_iter().filter_map(|id| build_shape(doc, kids, id, base, depth)).collect()
}

fn build_shape(
    doc: &DeckDoc,
    kids: &BTreeMap<Option<ShapeId>, Vec<(ShapeId, String)>>,
    id: ShapeId,
    base: &HashMap<ShapeId, &Shape>,
    depth: usize,
) -> Option<Shape> {
    let e = doc.shape(id)?;
    let earlier = base.get(&id).copied();
    let mut s = earlier.cloned().unwrap_or_else(|| Shape { id, ..Default::default() });
    if e.has_text {
        let ps = doc.text(TextTarget::Shape(id)).unwrap_or_default();
        let mut t = s.text.take().unwrap_or_default();
        t.paragraphs = keep_base(&t.paragraphs, ps);
        s.text = Some(t);
    } else if !doc.text_is_junk(id) {
        s.text = None;
    }
    // Every value in the index already read as its type; `body` lands only in a text body.
    for (k, v) in &e.fields {
        apply_shape_field(&mut s, k, v);
    }
    if let ShapeKind::Group { children, .. } = &mut s.kind {
        *children = if depth < 64 { build_level(doc, kids, Some(id), base, depth + 1) } else { Vec::new() };
    }
    Some(s)
}

/// A copy of D4's `delete_shapes_in` (frozen signature) until it lands in the engine; L4 removes
/// it at its merge. It removes `ids` from slide `slide` as `edit.delete` does: empty groups go too;
/// animations of a gone shape go and effects it triggered fall back to the main sequence;
/// connector ends glued to a gone shape come loose. It does not refit groups or reroute
/// connectors (the engine's routine does). Returns every id that is gone.
pub fn delete_shapes_in(doc: &mut Presentation, slide: usize, ids: &[ShapeId]) -> Vec<ShapeId> {
    let Some(sl) = doc.slides.get_mut(slide) else { return Vec::new() };
    let sl = Arc::make_mut(sl);
    let mut before = Vec::new();
    deckcraft_model::walk(&sl.shapes, &mut |s, _| before.push(s.id));
    remove(&mut sl.shapes, ids, 0);
    let mut left = std::collections::HashSet::new();
    deckcraft_model::walk(&sl.shapes, &mut |s, _| {
        left.insert(s.id);
    });
    let gone: Vec<ShapeId> = before.into_iter().filter(|i| !left.contains(i)).collect();
    sl.animations.retain(|a| !gone.contains(&a.shape));
    for a in sl.animations.iter_mut().filter(|a| a.trigger.is_some_and(|t| gone.contains(&t))) {
        a.trigger = None;
    }
    loosen(&mut sl.shapes, &gone, 0);
    gone
}

fn remove(v: &mut Vec<Shape>, ids: &[ShapeId], depth: usize) {
    v.retain(|s| !ids.contains(&s.id));
    if depth > 64 {
        return;
    }
    for s in v.iter_mut() {
        if let Some(ch) = s.children_mut() {
            remove(ch, ids, depth + 1);
        }
    }
    v.retain(|s| !s.is_group() || !s.children().is_empty());
}

fn loosen(v: &mut [Shape], gone: &[ShapeId], depth: usize) {
    for s in v.iter_mut() {
        if let ShapeKind::Connector { start, end } = &mut s.kind {
            for e in [start, end] {
                if e.is_some_and(|(id, _)| gone.contains(&id)) {
                    *e = None;
                }
            }
        }
        if depth < 64
            && let Some(ch) = s.children_mut()
        {
            loosen(ch, gone, depth + 1);
        }
    }
}
