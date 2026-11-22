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

use std::collections::HashMap;
use std::sync::Arc;

use deckcraft_model::Presentation;

use crate::{DocState, Session};

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

/// The observers and the state of the scope in progress.
#[derive(Default)]
pub(crate) struct Live {
    observers: HashMap<u64, EditObserver>,
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

    /// Take the current state of document `uid` as seen, so the open command or gesture does not
    /// report what changed in it so far. For a change that came from the live layer itself (a
    /// remote apply, a live undo), which must not be sent back. Outside a command or gesture
    /// nothing is reported anyway, so this does nothing there.
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
}
