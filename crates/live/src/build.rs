//! The deck a replica describes: the base plus every entry, tombstoned shapes removed through
//! the engine's one delete routine (Ruling D7). The oracle of the tests and the worker's save.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use craft_collab::{LiveError, Replica};
use deckcraft_model::text::{Paragraph, TextBody};
use deckcraft_model::{LayoutId, Presentation, Shape, ShapeId, ShapeKind, Slide, SlideId};

use crate::doc::{DeckDoc, ShapeEntry, TextTarget, apply_shape_field, apply_slide_field, tree_in};
use crate::schema::{BODY, DEL, LAYOUT, PARENT, POS, SLIDE, Z};

/// The deck for `state` over `base`: equal for equal (base, state) on every target.
pub fn deck_from_state(base: &Presentation, state: &[u8]) -> Result<Presentation, LiveError> {
    let doc = DeckDoc::from_state(state)?;
    Ok(deck_from_doc(base, &doc))
}

/// Where the seed put a base shape: its slide, its parent and its `z`.
type Placed = (SlideId, Option<ShapeId>, String);

/// The base's slides and shapes, and where the seed put them (`pos` and `z` from `seed_keys`).
struct Base<'a> {
    slides: HashMap<SlideId, (&'a Arc<Slide>, String)>,
    shapes: HashMap<ShapeId, (&'a Shape, Placed)>,
}

impl<'a> Base<'a> {
    fn of(p: &'a Presentation) -> Base<'a> {
        fn rec<'a>(out: &mut HashMap<ShapeId, (&'a Shape, Placed)>, slide: SlideId, parent: Option<ShapeId>, shapes: &'a [Shape], depth: usize) {
            if depth > 64 {
                return;
            }
            for (s, z) in shapes.iter().zip(crate::order::seed_keys(shapes.len())) {
                out.entry(s.id).or_insert((s, (slide, parent, z)));
                rec(out, slide, Some(s.id), s.children(), depth + 1);
            }
        }
        let pos = crate::order::seed_keys(p.slides.len());
        let mut shapes = HashMap::new();
        for s in &p.slides {
            rec(&mut shapes, s.id, None, &s.shapes, 0);
        }
        Base { slides: p.slides.iter().zip(pos).map(|(s, pos)| (s.id, (s, pos))).collect(), shapes }
    }
}

/// The deck a replica describes, with no encode. A key holding a peer's junk reads as the base's
/// value (the default for a new entry), so the result is a function of (base, state):
/// `deck_from_doc(base, doc) == deck_from_state(base, &doc.state())`.
///
/// `next_id` stays the base's: ids minted live sit at or above `LIVE_ID_BASE` and are never
/// counted into it, so a block-less session over the result keeps minting below the live range.
/// Tombstoned shapes are removed with the engine's own delete routine (Ruling D7), as a local
/// `edit.delete` and a remote delete remove them.
#[must_use]
pub fn deck_from_doc(base: &Presentation, doc: &DeckDoc) -> Presentation {
    let mut out = base.clone();
    let r = Rebuild::new(base, doc);
    let order = r.order();
    let all: BTreeSet<SlideId> = order.iter().copied().collect();
    let shapes = r.entries_on(&all);
    let mut slides = Vec::new();
    let mut tombs = Vec::new();
    for id in order {
        let Some((slide, dead)) = r.slide(id, &shapes) else { continue };
        tombs.push(dead);
        slides.push(slide);
    }
    out.slides = slides;
    for (i, dead) in tombs.into_iter().enumerate() {
        if !dead.is_empty() {
            deckcraft_engine::delete_shapes_in(&mut out, i, &dead);
        }
    }
    out
}

/// The pieces of [`deck_from_doc`], so a remote change can rebuild only the slides it touched.
pub(crate) struct Rebuild<'a> {
    b: Base<'a>,
    doc: &'a DeckDoc,
}

impl<'a> Rebuild<'a> {
    pub(crate) fn new(base: &'a Presentation, doc: &'a DeckDoc) -> Rebuild<'a> {
        Rebuild { b: Base::of(base), doc }
    }

    /// The live slides in order, a junk `pos` read as the base's and a junk `del` as false.
    pub(crate) fn order(&self) -> Vec<SlideId> {
        let mut order = Vec::new();
        for (id, e) in self.doc.slide_entries() {
            let junk = self.doc.slide_junk(*id);
            let bad = |k: &str| junk.is_some_and(|j| j.contains(k));
            let del = e.del && !bad(DEL);
            let pos = if bad(POS) { self.b.slides.get(id).map(|(_, p)| p.clone()).unwrap_or_default() } else { e.pos.clone() };
            if !del {
                order.push((*id, pos));
            }
        }
        crate::order::order(order)
    }

    /// The slide a shape entry is on, a junk `slide` read as the base's.
    fn slide_of(&self, id: ShapeId, e: &ShapeEntry) -> SlideId {
        if self.doc.shape_junk(id).is_some_and(|j| j.contains(SLIDE)) { self.b.shapes.get(&id).map_or(SlideId(0), |(_, p)| p.0) } else { e.slide }
    }

    /// The shape entries on `slides`, with junk core keys put back to the base's.
    pub(crate) fn entries_on(&self, slides: &BTreeSet<SlideId>) -> BTreeMap<ShapeId, ShapeEntry> {
        let mut shapes: BTreeMap<ShapeId, ShapeEntry> = BTreeMap::new();
        for (id, e) in self.doc.shape_entries() {
            let slide = self.slide_of(*id, e);
            if !slides.contains(&slide) {
                continue;
            }
            let mut e = e.clone();
            e.slide = slide;
            if let Some(junk) = self.doc.shape_junk(*id) {
                let placed = self.b.shapes.get(id).map(|(_, p)| p);
                if junk.contains(PARENT) {
                    e.parent = placed.and_then(|p| p.1);
                }
                if junk.contains(Z) {
                    e.z = placed.map(|p| p.2.clone()).unwrap_or_default();
                }
                if junk.contains(DEL) {
                    e.del = false;
                }
            }
            shapes.insert(*id, e);
        }
        shapes
    }

    /// Slide `id` as the replica describes it, its tombstoned shapes still in it, and those ids.
    /// An unchanged slide is the base's own `Arc`. None for a slide with no entry.
    pub(crate) fn slide(&self, id: SlideId, shapes: &BTreeMap<ShapeId, ShapeEntry>) -> Option<(Arc<Slide>, Vec<ShapeId>)> {
        let e = self.doc.slide(id)?;
        let b = &self.b;
        let earlier = b.slides.get(&id).map(|(s, _)| s.as_ref());
        let mut slide = earlier.cloned().unwrap_or_else(|| Slide { id, ..Default::default() });
        if !self.doc.slide_junk(id).is_some_and(|j| j.contains(LAYOUT)) {
            slide.layout = e.layout;
        } else if earlier.is_none() {
            slide.layout = LayoutId(0);
        }
        for (k, v) in &e.fields {
            apply_slide_field(&mut slide, k, v);
        }
        if let Some(ps) = self.doc.text(TextTarget::Notes(id)) {
            let kept = keep_base(&slide.notes.paragraphs, ps);
            // Notes typed live need a notes slide to be saved in; an unchanged body keeps the
            // base's flag.
            if kept != slide.notes.paragraphs && kept.iter().any(|p| !p.is_empty()) {
                slide.has_notes_slide = true;
            }
            slide.notes.paragraphs = kept;
        }
        let (built, dead) = build_tree(self.doc, shapes, id, b);
        slide.shapes = built;
        let slide = match b.slides.get(&id) {
            Some((s, _)) if s.as_ref() == &slide => Arc::clone(s),
            _ => Arc::new(slide),
        };
        Some((slide, dead))
    }
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

type Kids = BTreeMap<Option<ShapeId>, Vec<(ShapeId, String)>>;

/// The slide's shape tree, tombstones included, and the tombstoned ids.
fn build_tree(doc: &DeckDoc, shapes: &BTreeMap<ShapeId, ShapeEntry>, slide: SlideId, base: &Base) -> (Vec<Shape>, Vec<ShapeId>) {
    let mut kids: Kids = BTreeMap::new();
    let mut dead = Vec::new();
    for (id, parent) in tree_in(shapes, slide) {
        if let Some(e) = shapes.get(&id) {
            if e.del {
                dead.push(id);
            }
            kids.entry(parent).or_default().push((id, e.z.clone()));
        }
    }
    let built = build_level(doc, shapes, &kids, None, base, 0);
    (built, dead)
}

fn build_level(doc: &DeckDoc, shapes: &BTreeMap<ShapeId, ShapeEntry>, kids: &Kids, at: Option<ShapeId>, base: &Base, depth: usize) -> Vec<Shape> {
    let Some(list) = kids.get(&at) else { return Vec::new() };
    crate::order::order(list.iter().cloned()).into_iter().filter_map(|id| build_shape(doc, shapes, kids, id, base, depth)).collect()
}

fn build_shape(doc: &DeckDoc, shapes: &BTreeMap<ShapeId, ShapeEntry>, kids: &Kids, id: ShapeId, base: &Base, depth: usize) -> Option<Shape> {
    let e = shapes.get(&id)?;
    let earlier = base.shapes.get(&id).map(|(s, _)| *s);
    let mut s = earlier.cloned().unwrap_or_else(|| Shape { id, ..Default::default() });
    // A text body is there when `body` is (D26 as amended); a junk `body` keeps the base's.
    let body_junk = doc.shape_junk(id).is_some_and(|j| j.contains(BODY));
    if e.has_text || body_junk {
        if e.has_text && s.text.is_none() {
            s.text = Some(TextBody::default());
        }
        if let (Some(t), Some(ps)) = (s.text.as_mut(), doc.text(TextTarget::Shape(id))) {
            t.paragraphs = keep_base(&t.paragraphs, ps);
        }
    } else {
        s.text = None;
    }
    // Every value in the index already read as its type; `body` lands only in a text body.
    for (k, v) in &e.fields {
        apply_shape_field(&mut s, k, v);
    }
    if let ShapeKind::Group { children, .. } = &mut s.kind {
        *children = if depth < 64 { build_level(doc, shapes, kids, Some(id), base, depth + 1) } else { Vec::new() };
    }
    Some(s)
}
