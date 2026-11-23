//! The types of a DeckCraft live session on top of `craft-live`: what a local edit is, and where
//! another editor is.

use craft_live::LiveKind;
use deckcraft_engine::Edited;
use deckcraft_model::text::{Paragraph, RunKind};
use deckcraft_model::{ShapeId, SlideId};

use crate::doc::{DeckDoc, TextTarget};
use crate::presence::{Scene, StickyCaret};

/// DeckCraft's [`LiveKind`].
pub struct Deck;

/// One local edit, queued by the engine's observer and live undo hook and handed back to the
/// mapping at the next flush.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum DeckEdit {
    /// A change the engine reported: one outer command, a pointer gesture at its drop, or an input
    /// method's commit. [`Edited::command`] is a label; the mapping reads only its `pointer:` prefix, to refuse a drop on a deleted shape (L5).
    Command(Box<Edited>),
    /// The person asked for Undo (the engine's live undo hook).
    Undo,
    /// The person asked for Redo.
    Redo,
    /// Bring back a deleted slide or shape (`del = false`), as a group of its own (L5).
    Restore(crate::doc::Tomb),
}

/// Where another editor is, as the host draws it: the slide's index now, the selected shapes and
/// the text caret.
#[derive(Clone, Debug, PartialEq)]
pub struct DeckPos {
    pub slide: Option<usize>,
    pub shapes: Vec<ShapeId>,
    pub caret: Option<CaretPos>,
}

/// A text caret: (paragraph, char), as the engine's `TextSel` counts.
#[derive(Clone, Debug, PartialEq)]
pub struct CaretPos {
    pub target: TextTarget,
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

/// Another editor's selection as the mapping keeps it between edits: by stable id, with its text
/// caret as sticky indexes (L7). It is resolved to a [`DeckPos`] against this window's deck at each
/// pump; what this window does not have is not drawn, and a deleted slide drops the caret.
#[derive(Clone, Debug)]
pub struct DeckCaret {
    pub slide: SlideId,
    /// At most [`crate::presence::SHAPES_MAX`].
    pub shapes: Vec<ShapeId>,
    pub text: Option<StickyCaret>,
    pub(crate) scene: Scene,
}

impl PartialEq for DeckCaret {
    fn eq(&self, o: &Self) -> bool {
        (self.slide, &self.shapes, &self.text) == (o.slide, &o.shapes, &o.text)
    }
}

impl DeckCaret {
    /// The position this caret resolves to now.
    #[must_use]
    pub fn at(&self) -> DeckPos {
        self.scene.pos(self)
    }
}

impl LiveKind for Deck {
    type Replica = DeckDoc;
    type Edit = DeckEdit;
    type Caret = DeckCaret;
    type Pos = DeckPos;

    fn pos(caret: &DeckCaret) -> DeckPos {
        caret.at()
    }
}

/// One run's width in the engine's characters and in units of the flat form ([`crate::text`]):
/// a field or an equation is one unit but its shown text in the engine.
fn run_widths(p: &Paragraph) -> impl Iterator<Item = (usize, usize)> + '_ {
    p.runs.iter().map(|r| {
        let chars = r.char_len();
        match r.kind {
            RunKind::Field { .. } | RunKind::Math { .. } => (chars, 1),
            RunKind::Text | RunKind::Break => (chars, chars),
        }
    })
}

/// The flat-form offset of an engine position (paragraph, char) in `body`; a position inside a
/// field snaps to its end. Past the end, the end.
pub(crate) fn to_flat(body: &[Paragraph], (p, c): (usize, usize)) -> u32 {
    let mut flat = 0usize;
    for (i, para) in body.iter().enumerate() {
        if i == p {
            let mut chars = 0usize;
            for (w, u) in run_widths(para) {
                if c < chars + w {
                    flat += if u == w { c - chars } else { u };
                    return u32::try_from(flat).unwrap_or(u32::MAX);
                }
                chars += w;
                flat += u;
            }
            return u32::try_from(flat).unwrap_or(u32::MAX);
        }
        flat += run_widths(para).map(|(_, u)| u).sum::<usize>() + 1;
    }
    u32::try_from(flat.saturating_sub(1)).unwrap_or(u32::MAX)
}

/// The engine position (paragraph, char) of a flat-form offset in `body`.
pub(crate) fn from_flat(body: &[Paragraph], at: u32) -> (usize, usize) {
    let mut left = usize::try_from(at).unwrap_or(usize::MAX);
    for (i, para) in body.iter().enumerate() {
        let mut chars = 0usize;
        for (w, u) in run_widths(para) {
            if left < u {
                return (i, chars + if u == w { left } else { 0 });
            }
            left -= u;
            chars += w;
        }
        if left == 0 || i + 1 == body.len() {
            return (i, chars);
        }
        left -= 1;
    }
    (0, 0)
}
