//! The edit observer (H1): how the live co-editing layer hears about local changes.
//!
//! A host binds one observer to an open presentation ([`Session::set_edit_observer`]). The
//! observer gets one [`Edited`] for each change the person made: per outer command (undoable or
//! not), per pointer gesture, at the drop of a drag, when an input method's composition ends.
//!
//! What the observer has seen of a presentation is its document, except while a pointer
//! interaction or a composition is open: then it is the document from before that began
//! (`Interaction::doc`, `Composition::before`), and nothing is reported until it ends. So a
//! drag reports once, at the drop, and a composition once, at its commit. The reports chain:
//! each one's `before` is the previous one's `after`.
//!
//! Every call into the session that can change a presentation ([`Session::execute`],
//! [`Session::pointer`]) is a *scope*. The outermost scope notes what each observer has seen when
//! it starts and reports the difference when it ends, so a command run by a command, or by a
//! pointer gesture, is part of the outer report. A pointer interaction that commits, or a
//! composition that ends, inside a scope reports up to that point first (`Session::live_cut`),
//! so those reports match the undo steps the engine keeps. A failed command puts the
//! presentation back and so reports nothing.
//!
//! The other way (H2, H3): the live layer lands other people's changes with
//! [`Session::apply_remote`] or, for a reset, [`Session::replace_presentation_quiet`]. Neither adds
//! an undo step, a journal entry or a report. While a live undo hook is set
//! ([`Session::set_live_undo_hook`]), Undo and Redo of that presentation go to the live layer.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use deckcraft_model::{Presentation, ShapeId, ShapeKind, Slide, SlideId};

use crate::{DocState, EngineError, Result, Session, TextSel};

/// One local change to an observed presentation.
#[derive(Clone, Debug)]
pub struct Edited {
    /// The presentation's [`DocState::uid`].
    pub doc: u64,
    pub before: Arc<Presentation>,
    pub after: Arc<Presentation>,
    /// The command id; `pointer:<label>` for a pointer gesture (the label of its undo step);
    /// `text.compose` for an input method's committed composition.
    pub command: String,
}

/// Receives the [`Edited`] reports of one presentation.
pub type EditObserver = Box<dyn FnMut(Edited) + Send>;

/// Takes Undo (`false`) and Redo (`true`) of one presentation; answers whether a step was taken.
pub type LiveUndoHook = Box<dyn FnMut(bool) -> bool + Send>;

/// The observers and the state of the scope in progress.
#[derive(Default)]
pub(crate) struct Live {
    observers: HashMap<u64, EditObserver>,
    /// Live undo hooks, by document uid (H3).
    undo_hooks: HashMap<u64, LiveUndoHook>,
    /// Nesting of scopes; only the outermost one reports.
    depth: u32,
    /// What each observer has seen, by document uid, while a scope is open.
    seen: Vec<(u64, Arc<Presentation>)>,
    /// Reports made inside the open scope, delivered when it ends.
    pending: Vec<Edited>,
}

/// What the observer of `st` has seen: the document, or the one from before the open interaction
/// or composition.
fn seen_of(st: &DocState) -> Arc<Presentation> {
    if let Some(i) = &st.interaction {
        i.doc.clone()
    } else if let Some(c) = &st.composition {
        c.before.clone()
    } else {
        st.doc.clone()
    }
}

impl Session {
    /// Bound to one document (by uid); dropped when it closes. Replaces any earlier one.
    ///
    /// `None` removes it. An observer for a uid that no open document has is dropped at once.
    pub fn set_edit_observer(&mut self, doc: u64, observer: Option<EditObserver>) {
        match observer {
            Some(o) if self.docs.iter().any(|d| d.uid == doc) => {
                self.live.observers.insert(doc, o);
            }
            _ => {
                self.live.observers.remove(&doc);
            }
        }
    }

    /// Drop the observer of a closed document.
    pub(crate) fn live_closed(&mut self, uid: u64) {
        self.live.observers.remove(&uid);
        self.live.undo_hooks.remove(&uid);
        self.live.seen.retain(|(u, _)| *u != uid);
    }

    /// Run `f` as a scope: when the outermost scope ends, each observed document whose seen
    /// state changed is reported once, named by `command` (given the session after `f`).
    pub(crate) fn live_scope<T>(&mut self, f: impl FnOnce(&mut Session) -> T, command: impl FnOnce(&Session) -> String) -> T {
        if self.live.observers.is_empty() {
            return f(self);
        }
        let outer = self.live.depth == 0;
        if outer {
            let seen: Vec<(u64, Arc<Presentation>)> =
                self.docs.iter().filter(|d| self.live.observers.contains_key(&d.uid)).map(|d| (d.uid, seen_of(d))).collect();
            self.live.seen = seen;
            self.live.pending.clear();
        }
        self.live.depth += 1;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        self.live.depth -= 1;
        let r = match r {
            Ok(r) => r,
            Err(payload) => {
                if outer {
                    self.live.seen.clear();
                    self.live.pending.clear();
                }
                std::panic::resume_unwind(payload)
            }
        };
        if outer {
            let name = command(self);
            let seen = std::mem::take(&mut self.live.seen);
            let mut reports = std::mem::take(&mut self.live.pending);
            for (uid, before) in seen {
                if let Some(st) = self.docs.iter().find(|d| d.uid == uid) {
                    let after = seen_of(st);
                    if !Arc::ptr_eq(&before, &after) {
                        reports.push(Edited { doc: uid, before, after, command: name.clone() });
                    }
                }
            }
            for e in reports {
                self.live_deliver(e);
            }
        }
        r
    }

    /// A step that ends inside a scope (a pointer interaction committed, a composition ended):
    /// report document `uid` from what its observer saw (`from` outside a scope) up to now.
    pub(crate) fn live_cut(&mut self, uid: u64, from: Arc<Presentation>, command: &str) {
        if !self.live.observers.contains_key(&uid) {
            return;
        }
        let Some(after) = self.docs.iter().find(|d| d.uid == uid).map(seen_of) else { return };
        if self.live.depth == 0 {
            if !Arc::ptr_eq(&from, &after) {
                self.live_deliver(Edited { doc: uid, before: from, after, command: command.into() });
            }
            return;
        }
        let Some(slot) = self.live.seen.iter_mut().find(|(u, _)| *u == uid) else { return };
        let before = std::mem::replace(&mut slot.1, after.clone());
        if !Arc::ptr_eq(&before, &after) {
            self.live.pending.push(Edited { doc: uid, before, after, command: command.into() });
        }
    }

    /// Take the current state of document `uid` as seen, so the open command does not report what
    /// changed in it so far. For a change that came from the live layer itself, made inside a
    /// command (a live undo through the hook), which must not be sent back. Outside a command
    /// nothing is reported anyway, so this does nothing there.
    ///
    /// Call it right after that change and before any local change in the same command: what
    /// changed in the document earlier in the command, and was not reported yet, is taken as seen
    /// too and never reported.
    ///
    /// It cannot keep a change out of an open pointer interaction or composition: their report
    /// runs from the document before they began (`Interaction::doc`, `Composition::before`), so
    /// anything that lands in the document meanwhile is in it. That is why
    /// [`Session::apply_remote`] refuses while one is open ([`Session::remote_busy`]).
    pub fn live_absorb(&mut self, uid: u64) {
        if self.live.depth == 0 {
            return;
        }
        let Some(now) = self.docs.iter().find(|d| d.uid == uid).map(seen_of) else { return };
        if let Some(slot) = self.live.seen.iter_mut().find(|(u, _)| *u == uid) {
            slot.1 = now;
        }
    }

    fn live_deliver(&mut self, e: Edited) {
        if let Some(o) = self.live.observers.get_mut(&e.doc) {
            o(e);
        }
    }

    /// Land a change made by someone else in document `doc`: `f` changes a copy of its
    /// presentation through [`RemoteCtx`], and the copy replaces the document if `f` succeeds.
    ///
    /// No undo step, no journal, no observer call. Selected shapes that are gone are dropped; the
    /// current slide is found again by its [`SlideId`] (Ruling D17). When it was removed, the slide
    /// now at its old index (or the last one) becomes current, with nothing selected on it; the
    /// caller reads that from [`Session::doc`] and tells the person. Err leaves the document
    /// exactly as before (the same allocation). Err([`EngineError::RemoteBusy`]) while a pointer
    /// interaction or a composition is open in it ([`Session::remote_busy`]); the live layer holds
    /// the change until then.
    pub fn apply_remote<R>(&mut self, doc: u64, f: impl FnOnce(&mut RemoteCtx) -> Result<R>) -> Result<R> {
        let Some(base) = self.docs.iter().find(|d| d.uid == doc).map(|d| d.doc.clone()) else { return Err(EngineError::NoDocument) };
        if self.remote_busy(doc) {
            return Err(EngineError::RemoteBusy);
        }
        let mut ctx = RemoteCtx { doc: Cow::Borrowed(&base), text: None };
        let r = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut ctx))) {
            Ok(r) => r?,
            Err(payload) => {
                self.panics_caught += 1;
                return Err(EngineError::Internal("apply_remote".into(), crate::guard::panic_message(payload.as_ref())));
            }
        };
        let RemoteCtx { doc: next, text } = ctx;
        let next = match next {
            Cow::Owned(p) => Some(p),
            Cow::Borrowed(_) => None,
        };
        self.land_remote(doc, &base, next, text);
        Ok(r)
    }

    /// Is a pointer interaction or a composition open in document `doc`? Remote changes wait
    /// until it ends: a drag or a composition holds the presentation from before it began.
    pub fn remote_busy(&self, doc: u64) -> bool {
        let Some(st) = self.docs.iter().find(|d| d.uid == doc) else { return false };
        let active = self.active().is_some_and(|a| a.uid == doc);
        st.interaction.is_some() || st.composition.is_some() || (active && self.tool.dragging())
    }

    /// A reset: the presentation replaced with no undo step; selection kept as
    /// [`Session::apply_remote`] keeps it, and [`EngineError::RemoteBusy`] in the same cases.
    pub fn replace_presentation_quiet(&mut self, doc: u64, p: Presentation) -> Result<()> {
        self.apply_remote(doc, move |ctx| {
            ctx.doc = Cow::Owned(Arc::new(p));
            Ok(())
        })
    }

    /// Bound to one document (by uid); dropped when it closes. While it is set, Undo and Redo of
    /// that document go to it (the argument is `redo`) and it answers whether a step was taken.
    /// The snapshot history is not touched while it is set, and neither is reported as a local
    /// edit. `None` removes it; a hook for a uid that no open document has is dropped at once.
    pub fn set_live_undo_hook(&mut self, doc: u64, hook: Option<LiveUndoHook>) {
        match hook {
            Some(h) if self.docs.iter().any(|d| d.uid == doc) => {
                self.live.undo_hooks.insert(doc, h);
            }
            _ => {
                self.live.undo_hooks.remove(&doc);
            }
        }
    }

    /// Does document `uid` have a live undo hook?
    pub(crate) fn has_live_undo(&self, uid: u64) -> bool {
        self.live.undo_hooks.contains_key(&uid)
    }

    /// Undo (or redo) document `uid` through its live undo hook: `None` without one, else
    /// whether a step was taken. What it changes is not reported as a local edit.
    pub(crate) fn live_undo(&mut self, uid: u64, redo: bool) -> Option<bool> {
        let hook = self.live.undo_hooks.get_mut(&uid)?;
        let taken = hook(redo);
        self.live_absorb(uid);
        Some(taken)
    }

    /// Put a remote change in place: the new presentation (if `f` changed it) and the selection
    /// found again by id.
    fn land_remote(&mut self, uid: u64, base: &Presentation, next: Option<Arc<Presentation>>, text: Option<Option<TextSel>>) {
        let Some(st) = self.docs.iter_mut().find(|d| d.uid == uid) else { return };
        let old_slide = st.selection.slide;
        let current: Option<SlideId> = base.slides.get(old_slide).map(|s| s.id);
        if let Some(n) = next {
            st.doc = n;
        }
        let sel = &mut st.selection;
        let removed = match current.map(|id| st.doc.slides.iter().position(|s| s.id == id)) {
            Some(Some(i)) => {
                sel.slide = i;
                false
            }
            // Gone: the slide now at its index (clamped below) with nothing selected on it.
            Some(None) => true,
            None => false,
        };
        if removed {
            sel.shapes.clear();
            sel.text = None;
            sel.cells = None;
        } else if let Some(t) = text {
            sel.text = t;
        } else if let Some(t) = sel.text.as_mut() {
            clamp_caret(st.doc.slides.get(sel.slide).map(|s| s.as_ref()), t);
        }
        if sel.range_anchor.is_some_and(|(a, b)| st.doc.slide(a).is_none() || st.doc.slide(b).is_none()) {
            sel.range_anchor = None;
        }
        if sel.slide != old_slide || removed {
            st.last_duplicate = None;
        }
        crate::clamp_selection(st);
        let cells_gone = st.selection.cells.is_some_and(|(id, _, _)| st.shape(id).is_none());
        if cells_gone {
            st.selection.cells = None;
        }
        // A remote change may have taken rows or columns: keep a cell range inside its table.
        if let Some((id, a, b)) = st.selection.cells {
            let size = st.shape(id).and_then(|s| if let ShapeKind::Table(t) = &s.kind { Some((t.n_rows(), t.n_cols())) } else { None });
            st.selection.cells = match size {
                Some((r, c)) if r > 0 && c > 0 => {
                    let fit = |(x, y): (usize, usize)| (x.min(r - 1), y.min(c - 1));
                    Some((id, fit(a), fit(b)))
                }
                _ => None,
            };
        }
        st.revision += 1;
        self.live_absorb(uid);
    }
}

/// A copy of one presentation that a remote change edits ([`Session::apply_remote`]). Nothing is
/// copied until the first change, and each change copies only what it touches.
pub struct RemoteCtx<'a> {
    doc: Cow<'a, Arc<Presentation>>,
    text: Option<Option<TextSel>>,
}

impl RemoteCtx<'_> {
    pub fn presentation(&self) -> &Presentation {
        &self.doc
    }

    /// The whole slide list in its new order. Slides that are `Arc::ptr_eq` to the current ones
    /// are untouched.
    pub fn set_slides(&mut self, slides: Vec<Arc<Slide>>) {
        Arc::make_mut(self.doc.to_mut()).slides = slides;
    }

    /// `edit.delete`'s routine on one slide (by id), for shapes deleted by someone else. Returns
    /// every id that is gone ([`crate::cmd::edit::delete_shapes_in`]).
    pub fn delete_shapes(&mut self, slide: SlideId, ids: &[ShapeId]) -> Result<Vec<ShapeId>> {
        let Some(i) = self.doc.slides.iter().position(|s| s.id == slide) else {
            return Err(EngineError::Other(format!("no slide with id {}", slide.0)));
        };
        Ok(crate::cmd::edit::delete_shapes_in(Arc::make_mut(self.doc.to_mut()), i, ids))
    }

    /// The local text caret after the change, computed by the live layer (None drops it).
    pub fn set_text(&mut self, text: Option<TextSel>) {
        self.text = Some(text);
    }
}

/// Keep a caret the live layer did not place inside its text: a remote change may have
/// shortened it, or taken the table row or column it was in.
fn clamp_caret(slide: Option<&Slide>, t: &mut TextSel) {
    let shape = slide.and_then(|s| deckcraft_model::find_shape(&s.shapes, t.shape));
    let body = if t.notes {
        slide.map(|s| &s.notes)
    } else if let Some((r, c)) = t.cell {
        // A table cell's caret: the cell kept inside the table (a remote change may have taken
        // rows or columns), then the caret inside the cell's text.
        let Some(ShapeKind::Table(table)) = shape.map(|s| &s.kind) else { return };
        if table.n_rows() == 0 || table.n_cols() == 0 {
            return;
        }
        let cell = (r.min(table.n_rows() - 1), c.min(table.n_cols() - 1));
        t.cell = Some(cell);
        table.cell(cell.0, cell.1).map(|x| &x.text)
    } else {
        shape.and_then(|s| s.text.as_ref())
    };
    let Some(body) = body else { return };
    let fit = |(p, c): (usize, usize)| {
        let p = p.min(body.paragraphs.len().saturating_sub(1));
        (p, c.min(body.paragraphs.get(p).map_or(0, |x| x.char_len())))
    };
    t.anchor = fit(t.anchor);
    t.caret = fit(t.caret);
    if t.line_end.is_some_and(|l| l != t.caret) {
        t.line_end = None;
    }
}
