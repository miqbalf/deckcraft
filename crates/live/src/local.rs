//! Local edits to writes: one engine report ([`Edited`]) becomes the writes that bring the
//! replica from `before` to `after`.
//!
//! Slides are compared one by one (`Arc::ptr_eq` first), and the shapes of a changed slide by
//! id. A changed latest-wins key is a key write; a changed text body is the minimal text edit
//! ([`edit_ops`]); a new slide or shape is a new entry whose order key sits between its
//! neighbours' (minted in this peer's space, so concurrent inserts never tie); a removed one is a
//! tombstone. A reorder writes only the moved items' keys: the items whose current keys already
//! run in the new order keep them.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use deckcraft_engine::Edited;
use deckcraft_model::text::Paragraph;
use deckcraft_model::{Shape, ShapeId, Slide, SlideId};

use crate::doc::{DeckDoc, TextTarget, Tomb, shape_values, slide_values};
use crate::order::between;
use crate::schema::LAYOUT;
use crate::split::PlannedWrite;
use crate::text::{Chunk, TextOp, edit_ops};

/// What one report plans.
#[derive(Default)]
pub(crate) struct Planned {
    pub writes: Vec<PlannedWrite>,
    /// The slides it touched (their entries or their shapes), for the check after it landed.
    pub slides: BTreeSet<SlideId>,
}

/// The writes for one report. `space` mints order keys; `uid` signs tombstones.
pub(crate) fn plan(doc: &DeckDoc, e: &Edited, space: u32, uid: &str) -> Planned {
    let mut out = Planned::default();
    let (before, after) = (&e.before, &e.after);
    let earlier: HashMap<SlideId, &Arc<Slide>> = before.slides.iter().map(|s| (s.id, s)).collect();
    let now: BTreeSet<SlideId> = after.slides.iter().map(|s| s.id).collect();
    // Order: new and moved slides get keys.
    let ids: Vec<SlideId> = after.slides.iter().map(|s| s.id).collect();
    let keys = rekey(&ids, |id| doc.slide(id).filter(|_| earlier.contains_key(&id)).map(|s| s.pos), space);
    for s in &after.slides {
        match earlier.get(&s.id) {
            None => {
                let pos = keys.get(&s.id).cloned().unwrap_or_else(|| between(None, None, space));
                out.writes.push(PlannedWrite::PutSlide { slide: Arc::clone(s), pos });
                put_tree(&mut out.writes, s.id, None, &s.shapes, space, 0);
                out.slides.insert(s.id);
            }
            Some(b) => {
                if let Some(pos) = keys.get(&s.id) {
                    out.writes.push(PlannedWrite::SetSlide { id: s.id, key: crate::schema::POS.to_owned(), value: pos.clone().into() });
                    out.slides.insert(s.id);
                }
                if !Arc::ptr_eq(b, s) {
                    changed_slide(&mut out, doc, b, s, space, uid);
                }
            }
        }
    }
    for s in &before.slides {
        if !now.contains(&s.id) {
            out.writes.push(PlannedWrite::Del { target: Tomb::Slide(s.id), del: true, by: uid.to_owned() });
        }
    }
    out
}

fn changed_slide(out: &mut Planned, doc: &DeckDoc, b: &Slide, a: &Slide, space: u32, uid: &str) {
    let id = a.id;
    let mut touched = false;
    let (vb, va) = (slide_values(b), slide_values(a));
    for (k, v) in &va {
        if vb.iter().find(|(kb, _)| kb == k).map(|(_, x)| x) != Some(v) {
            out.writes.push(PlannedWrite::SetSlide { id, key: (*k).to_owned(), value: v.clone() });
            touched = true;
        }
    }
    if b.layout != a.layout {
        out.writes.push(PlannedWrite::SetSlide { id, key: LAYOUT.to_owned(), value: a.layout.0.into() });
        touched = true;
    }
    if b.notes.paragraphs != a.notes.paragraphs {
        let target = TextTarget::Notes(id);
        let ops = text_ops(doc, &target, &b.notes.paragraphs, &a.notes.paragraphs);
        if !ops.is_empty() {
            out.writes.push(PlannedWrite::Text { target, ops });
        }
        touched = true;
    }
    if b.shapes != a.shapes {
        shapes(out, doc, id, b, a, space, uid);
        touched = true;
    }
    if touched || b.has_notes_slide != a.has_notes_slide {
        out.slides.insert(id);
    }
}

/// Every shape of a tree with its parent and its sibling list.
fn flat(shapes: &[Shape]) -> HashMap<ShapeId, (&Shape, Option<ShapeId>)> {
    fn rec<'a>(out: &mut HashMap<ShapeId, (&'a Shape, Option<ShapeId>)>, shapes: &'a [Shape], parent: Option<ShapeId>, depth: usize) {
        if depth > 64 {
            return;
        }
        for s in shapes {
            out.entry(s.id).or_insert((s, parent));
            rec(out, s.children(), Some(s.id), depth + 1);
        }
    }
    let mut out = HashMap::new();
    rec(&mut out, shapes, None, 0);
    out
}

fn shapes(out: &mut Planned, doc: &DeckDoc, slide: SlideId, b: &Slide, a: &Slide, space: u32, uid: &str) {
    let (fb, fa) = (flat(&b.shapes), flat(&a.shapes));
    // Each sibling list in `after`, in order: new and moved shapes get `z` keys.
    let mut lists: Vec<(Option<ShapeId>, &[Shape])> = vec![(None, &a.shapes)];
    let mut i = 0;
    while let Some((_, list)) = lists.get(i).copied() {
        for s in list {
            if !s.children().is_empty() {
                lists.push((Some(s.id), s.children()));
            }
        }
        i += 1;
        if i > 100_000 {
            break;
        }
    }
    for (parent, list) in lists {
        let ids: Vec<ShapeId> = list.iter().map(|s| s.id).collect();
        let keys = rekey(&ids, |id| doc.shape(id).filter(|_| fb.get(&id).is_some_and(|(_, p)| *p == parent)).map(|e| e.z), space);
        for s in list {
            match fb.get(&s.id) {
                None => {
                    // A new shape; a new group's children follow in their own list.
                    let z = keys.get(&s.id).cloned().unwrap_or_else(|| between(None, None, space));
                    out.writes.push(PlannedWrite::PutShape { slide, parent, z, shape: Box::new(s.clone()) });
                }
                Some((sb, _)) => {
                    if let Some(z) = keys.get(&s.id) {
                        out.writes.push(PlannedWrite::SetShape { id: s.id, key: crate::schema::Z.to_owned(), value: z.clone().into() });
                    }
                    if !std::ptr::eq(*sb, s) {
                        changed_shape(out, doc, sb, s);
                    }
                }
            }
        }
    }
    // Removed shapes: a tombstone for each whose parent did not go too.
    for (id, (_, parent)) in &fb {
        if !fa.contains_key(id) && !parent.is_some_and(|p| !fa.contains_key(&p)) {
            out.writes.push(PlannedWrite::Del { target: Tomb::Shape(*id), del: true, by: uid.to_owned() });
        }
    }
}

fn changed_shape(out: &mut Planned, doc: &DeckDoc, b: &Shape, a: &Shape) {
    if b == a {
        return;
    }
    let (vb, va) = (shape_values(b), shape_values(a));
    for (k, v) in &va {
        if vb.iter().find(|(kb, _)| kb == k).map(|(_, x)| x) != Some(v) {
            out.writes.push(PlannedWrite::SetShape { id: a.id, key: (*k).to_owned(), value: v.clone() });
        }
    }
    let empty = Vec::new();
    let pb = b.text.as_ref().map_or(&empty, |t| &t.paragraphs);
    let pa = a.text.as_ref().map_or(&empty, |t| &t.paragraphs);
    if pb != pa {
        let target = TextTarget::Shape(a.id);
        let ops = text_ops(doc, &target, pb, pa);
        if !ops.is_empty() {
            out.writes.push(PlannedWrite::Text { target, ops });
        }
    }
}

/// `shapes` (all new) as entries under `parent`, depth first, each sibling list keyed in order.
fn put_tree(writes: &mut Vec<PlannedWrite>, slide: SlideId, parent: Option<ShapeId>, shapes: &[Shape], space: u32, depth: usize) {
    if depth > 64 {
        return;
    }
    let mut lo: Option<String> = None;
    for s in shapes {
        let z = between(lo.as_deref(), None, space);
        writes.push(PlannedWrite::PutShape { slide, parent, z: z.clone(), shape: Box::new(s.clone()) });
        put_tree(writes, slide, Some(s.id), s.children(), space, depth + 1);
        lo = Some(z);
    }
}

/// New order keys for the items of `ids` (in their new order) that need one: new items (no
/// current key) and moved ones. The items whose current keys already run in order, the longest
/// such run, keep theirs; each other item gets a key between its neighbours.
fn rekey<I: Copy + Ord + std::hash::Hash>(ids: &[I], key: impl Fn(I) -> Option<String>, space: u32) -> HashMap<I, String> {
    let cur: Vec<Option<(String, I)>> = ids.iter().map(|id| key(*id).map(|k| (k, *id))).collect();
    let keep = longest_run(&cur);
    let mut out = HashMap::new();
    let mut lo: Option<String> = None;
    for (i, id) in ids.iter().enumerate() {
        if keep.contains(&i) {
            lo = cur.get(i).and_then(|c| c.as_ref()).map(|(k, _)| k.clone());
            continue;
        }
        let hi = (i + 1..ids.len()).find(|j| keep.contains(j)).and_then(|j| cur.get(j).and_then(|c| c.as_ref()).map(|(k, _)| k.clone()));
        let k = between(lo.as_deref(), hi.as_deref(), space);
        lo = Some(k.clone());
        out.insert(*id, k);
    }
    out
}

/// The indices of a longest strictly increasing run (not contiguous) of the present values.
fn longest_run<T: Ord>(v: &[Option<T>]) -> BTreeSet<usize> {
    // Patience sorting: tails[k] is the index ending the best run of length k + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; v.len()];
    for (i, x) in v.iter().enumerate() {
        let Some(x) = x else { continue };
        let at = tails.partition_point(|&t| v.get(t).and_then(Option::as_ref).is_some_and(|y| y < x));
        prev[i] = at.checked_sub(1).and_then(|k| tails.get(k).copied());
        if at == tails.len() {
            tails.push(i);
        } else if let Some(t) = tails.get_mut(at) {
            *t = i;
        }
    }
    let mut out = BTreeSet::new();
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        out.insert(i);
        cur = prev.get(i).copied().flatten();
    }
    out
}

/// The text edit from `before` to `after`, made against what the replica holds now. When the
/// replica holds remote text the engine has not seen yet (held during a drag or a composition,
/// Ruling D6), the local edit is moved past it.
fn text_ops(doc: &DeckDoc, target: &TextTarget, before: &[Paragraph], after: &[Paragraph]) -> Vec<TextOp> {
    let local = edit_ops(before, after);
    let Some(now) = doc.text(target.clone()) else { return local };
    let normal: Vec<Paragraph> = before
        .iter()
        .map(|p| {
            let mut p = p.clone();
            p.normalize();
            p
        })
        .collect();
    if now == normal || (before.is_empty() && now.is_empty()) {
        return local;
    }
    rebase(&local, &edit_ops(before, &now))
}

/// The one changed region of a minimal edit: (start, deleted units, inserted units).
pub(crate) fn region(ops: &[TextOp]) -> Option<(u32, u32, u32)> {
    let mut r: Option<(u32, u32, u32)> = None;
    for op in ops {
        match op {
            TextOp::Delete { at, len } => r = Some((*at, *len, r.map_or(0, |x| x.2))),
            TextOp::Insert { at, chunks } => {
                let n: usize = chunks
                    .iter()
                    .map(|c| match c {
                        Chunk::Text { text, .. } => text.chars().count(),
                        Chunk::Embed { .. } => 1,
                    })
                    .sum();
                let n = u32::try_from(n).unwrap_or(u32::MAX);
                r = Some((r.map_or(*at, |x| x.0), r.map_or(0, |x| x.1), n));
            }
            TextOp::Format { .. } => {}
        }
    }
    r
}

/// `local` (made against `base`) moved onto `base` + `remote`: positions before the remote
/// region stay, those after it shift by its change in length, those inside it go to its end.
fn rebase(local: &[TextOp], remote: &[TextOp]) -> Vec<TextOp> {
    let Some((ra, rd, ri)) = region(remote) else { return local.to_vec() };
    let m = |p: u32| -> u32 {
        if p <= ra {
            p
        } else if p >= ra + rd {
            p - rd + ri
        } else {
            ra + ri
        }
    };
    let Some((la, ld, li)) = region(local) else {
        // Formats only: their positions are in `base` coordinates.
        return local.iter().map(|op| shift_format(op, &|p| m(p))).collect();
    };
    let (ms, me) = (m(la), m(la + ld).max(m(la)));
    // A position of the local result (after its own region) in the final text.
    let fin = |f: u32| -> u32 {
        if f >= la && f < la + li {
            return ms + (f - la);
        }
        let g = if f < la { f } else { f - li + ld };
        let g = m(g);
        if g >= me { g - (me - ms) + li } else { g }
    };
    let mut out = Vec::new();
    for op in local {
        match op {
            TextOp::Delete { .. } => {
                if me > ms {
                    out.push(TextOp::Delete { at: ms, len: me - ms });
                }
            }
            TextOp::Insert { chunks, .. } => out.push(TextOp::Insert { at: ms, chunks: chunks.clone() }),
            TextOp::Format { .. } => out.push(shift_format(op, &fin)),
        }
    }
    out
}

fn shift_format(op: &TextOp, f: &dyn Fn(u32) -> u32) -> TextOp {
    match op {
        TextOp::Format { at, len, attrs } => {
            let (s, e) = (f(*at), f(at + len));
            TextOp::Format { at: s, len: e.saturating_sub(s).max(u32::from(*len > 0)), attrs: attrs.clone() }
        }
        other => other.clone(),
    }
}
