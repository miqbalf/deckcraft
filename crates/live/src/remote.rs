//! Remote changes into the engine: the slides and shapes a change named are rebuilt from the
//! replica and landed through [`Session::apply_remote`]. Unchanged slides keep their `Arc`,
//! unchanged shapes their value; shapes newly tombstoned go through the engine's delete routine
//! ([`RemoteCtx::delete_shapes`], Ruling D7). The local caret moves with the text around it.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use deckcraft_engine::{EngineError, RemoteCtx, Session, TextSel};
use deckcraft_model::text::Paragraph;
use deckcraft_model::{IdBlock, Presentation, Shape, ShapeId, Slide, SlideId};

use crate::build::{Rebuild, deck_from_doc};
use crate::doc::{DeckChange, DeckDoc};
use crate::kind::{from_flat, to_flat};
use crate::local::region;
use crate::presence::{Mine, place, same_caret};
use crate::text::edit_ops;

/// What to bring the engine in line with.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Targets {
    /// Slides whose entry changed (order, tombstone, a key, notes).
    pub slides: BTreeSet<SlideId>,
    /// Shapes whose entry or text changed.
    pub shapes: BTreeSet<ShapeId>,
    /// Slides to rebuild whole, every shape from the replica (after a local edit landed).
    pub whole: BTreeSet<SlideId>,
}

impl Targets {
    pub(crate) fn from_changes(changes: &[DeckChange]) -> Targets {
        let mut t = Targets::default();
        for c in changes {
            match c {
                DeckChange::Slides(ids) => t.slides.extend(ids.iter().copied()),
                DeckChange::Shapes(ids) => t.shapes.extend(ids.iter().copied()),
                DeckChange::Text(id) => {
                    t.shapes.insert(*id);
                }
                DeckChange::Notes(id) => {
                    t.slides.insert(*id);
                }
                DeckChange::Meta => {}
            }
        }
        t
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.slides.is_empty() && self.shapes.is_empty() && self.whole.is_empty()
    }

    pub(crate) fn merge(&mut self, o: Targets) {
        self.slides.extend(o.slides);
        self.shapes.extend(o.shapes);
        self.whole.extend(o.whole);
    }
}

/// Where `p` lands after the minimal edit `ops`: before the changed region it stays, after it it
/// shifts by the change in length, inside it it goes to the region's end.
pub(crate) fn map_pos(ops: &[crate::text::TextOp], p: u32) -> u32 {
    let Some((a, d, i)) = region(ops) else { return p };
    if p <= a {
        p
    } else if p >= a + d {
        p - d + i
    } else {
        a + i
    }
}

/// A caret in `old` moved to the same content in `new`.
fn follow((p, c): (usize, usize), old: &[Paragraph], new: &[Paragraph], ops: &[crate::text::TextOp]) -> (usize, usize) {
    from_flat(new, map_pos(ops, to_flat(old, (p, c))))
}

/// The engine's slide holding each of `ids`.
fn slides_holding(p: &Presentation, ids: &BTreeSet<ShapeId>) -> BTreeSet<SlideId> {
    let mut out = BTreeSet::new();
    if ids.is_empty() {
        return out;
    }
    for s in &p.slides {
        deckcraft_model::walk(&s.shapes, &mut |sh, _| {
            if ids.contains(&sh.id) {
                out.insert(s.id);
            }
        });
    }
    out
}

/// Put the engine's value of every shape not in `changed` back into `built`, where `cur` has it.
fn keep_unchanged(built: &mut [Shape], cur: &Slide, changed: &BTreeSet<ShapeId>, depth: usize) {
    if depth > 64 {
        return;
    }
    for s in built.iter_mut() {
        if s.is_group() {
            if let Some(ch) = s.children_mut() {
                keep_unchanged(ch, cur, changed, depth + 1);
            }
            continue;
        }
        if !changed.contains(&s.id)
            && let Some(old) = cur.shape(s.id)
            && !old.is_group()
        {
            s.clone_from(old);
        }
    }
}

/// The text body a caret is in, on slide `slide` of `p`.
fn body_of(p: &Presentation, slide: SlideId, c: &TextSel) -> Option<Vec<Paragraph>> {
    let s = p.slides.iter().find(|s| s.id == slide)?;
    if c.notes { Some(s.notes.paragraphs.clone()) } else { s.shape(c.shape).and_then(|x| x.text.as_ref()).map(|t| t.paragraphs.clone()) }
}

/// Bring document `doc` of `session` in line with `t` in `replica`. Err([`EngineError::RemoteBusy`])
/// while a drag or a composition is open: the caller holds `t`.
///
/// The local text caret stays on the same characters: by its sticky indexes (`mine`, spec H2) when
/// they were taken for the caret the engine has now, otherwise by the minimal edit around it.
/// After a sticky placement, `mine` is kept for the placed caret.
pub(crate) fn land(
    session: &mut Session,
    doc: u64,
    base: &Presentation,
    replica: &DeckDoc,
    t: &Targets,
    mine: &mut Option<Mine>,
) -> Result<(), EngineError> {
    let Some(st) = session.documents().iter().find(|d| d.uid == doc) else { return Err(EngineError::NoDocument) };
    let at = st.doc.slides.get(st.selection.slide).map(|s| s.id);
    // The caret and the body it is in now (table cells are the engine's to clamp).
    let caret = st.selection.text.clone().filter(|t| t.cell.is_none()).and_then(|c| {
        let slide = at?;
        let old = body_of(&st.doc, slide, &c)?;
        Some((c, slide, old))
    });
    let sticky = mine.as_ref().filter(|m| caret.as_ref().is_some_and(|(c, slide, _)| m.slide == *slide && same_caret(c, &m.sel))).cloned();
    let mut placed = None;
    let r = session.apply_remote(doc, |ctx| {
        rebuild_into(ctx, base, replica, t);
        if let Some((c, slide, old)) = caret {
            match body_of(ctx.presentation(), slide, &c) {
                Some(new) if new == old => {}
                // Always placed when its text changed (D4 review M2).
                Some(new) => {
                    let by_sticky = sticky.as_ref().and_then(|m| place(m, replica, &new));
                    placed.clone_from(&by_sticky);
                    ctx.set_text(Some(by_sticky.unwrap_or_else(|| moved_caret(&c, &old, &new))));
                }
                None => ctx.set_text(None),
            }
        }
        Ok(())
    });
    // The sticky indexes still name the caret the engine now shows (as it clamped it).
    let now = session.documents().iter().find(|d| d.uid == doc).and_then(|d| d.selection.text.clone());
    if r.is_ok()
        && placed.is_some()
        && let (Some(sel), Some(m)) = (now, mine.as_mut())
    {
        m.sel = sel;
    }
    r
}

fn rebuild_into(ctx: &mut RemoteCtx, base: &Presentation, replica: &DeckDoc, t: &Targets) {
    let cur = ctx.presentation();
    let r = Rebuild::new(base, replica);
    let order = r.order();
    let mut affected: BTreeSet<SlideId> = t.slides.iter().chain(&t.whole).copied().collect();
    affected.extend(slides_holding(cur, &t.shapes));
    for id in &t.shapes {
        if let Some(e) = replica.shape(*id) {
            affected.insert(e.slide);
        }
    }
    let present: HashMap<SlideId, &Arc<Slide>> = cur.slides.iter().map(|s| (s.id, s)).collect();
    affected.extend(order.iter().filter(|id| !present.contains_key(id)).copied());
    let entries = r.entries_on(&affected);
    let mut slides = Vec::with_capacity(order.len());
    let mut deaths = Vec::new();
    for id in &order {
        let old = present.get(id);
        if !affected.contains(id)
            && let Some(old) = old
        {
            slides.push(Arc::clone(old));
            continue;
        }
        let Some((built, dead)) = r.slide(*id, &entries) else { continue };
        let mut built = built;
        if let Some(old) = old
            && !t.whole.contains(id)
        {
            let mut s = (*built).clone();
            keep_unchanged(&mut s.shapes, old, &t.shapes, 0);
            built = Arc::new(s);
        }
        let built = match old {
            Some(old) if dead.is_empty() && old.as_ref() == built.as_ref() => Arc::clone(old),
            _ => built,
        };
        if !dead.is_empty() {
            deaths.push((*id, dead));
        }
        slides.push(built);
    }
    let same = slides.len() == cur.slides.len() && slides.iter().zip(&cur.slides).all(|(a, b)| Arc::ptr_eq(a, b));
    if !same {
        ctx.set_slides(slides);
    }
    for (id, dead) in deaths {
        if let Err(e) = ctx.delete_shapes(id, &dead) {
            log::warn!("deckcraft-live: a remote delete did not land: {e}");
        }
    }
}

/// The deck the replica describes, with this peer's id block, for a reset or a put back.
pub(crate) fn whole(base: &Presentation, replica: &DeckDoc, block: Option<&IdBlock>) -> Presentation {
    let mut p = deck_from_doc(base, replica);
    p.id_block = block.cloned();
    p
}

/// The caret in `old` (the engine's body before) moved into `new`.
pub(crate) fn moved_caret(c: &TextSel, old: &[Paragraph], new: &[Paragraph]) -> TextSel {
    if old == new {
        return c.clone();
    }
    let ops = edit_ops(old, new);
    TextSel { anchor: follow(c.anchor, old, new, &ops), caret: follow(c.caret, old, new, &ops), line_end: None, ..c.clone() }
}
