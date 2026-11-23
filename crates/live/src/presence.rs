//! Presence (L7): where each editor is, in stable ids.
//!
//! The payload a window sends is
//! `{"s": <slide id>, "sh": [<shape id>, …], "t": {"k": "s"|"n", "id": <shape or slide id>, "a": "<hex>", "h": "<hex>"}}`:
//! ids in lowercase base 36, at most [`SHAPES_MAX`] shapes (the first ones of the selection), and
//! the text caret's anchor and head as yrs sticky indexes (`StickyIndex::encode_v1`, hex; Ruling
//! D24), so a caret stays on the same characters while others type. `t` is null without a text
//! caret (and in a table cell, which has no text of its own in the document).
//!
//! A peer's caret ([`DeckCaret`]) keeps the ids and sticky bytes and is resolved against this
//! window's deck at each pump: an id this window does not know yet is not drawn until it arrives;
//! one that is deleted (or under a deleted group or slide) is dropped from the caret for good.
//!
//! This window's own text caret is kept the same way across remote text edits: its sticky indexes
//! are taken while the engine and the replica agree, and after a remote change they are resolved
//! and handed to `RemoteCtx::set_text` (spec H2).

use std::sync::{Arc, Mutex, PoisonError};

use deckcraft_engine::{DocState, TextSel};
use deckcraft_model::text::Paragraph;
use deckcraft_model::{Presentation, ShapeId, Slide, SlideId};
use serde_json::{Value, json};

use crate::doc::{DeckDoc, DocView, Known, TextTarget, Tomb};
use crate::kind::{CaretPos, DeckCaret, DeckPos, from_flat, to_flat};
use crate::schema::{hex, id_key, parse_key};
use crate::text::edit_ops;

/// The most shape ids a payload carries (the first ones of the selection).
pub const SHAPES_MAX: usize = 64;

/// The longest sticky index read from a payload, in bytes (an encoded one is a few dozen).
const STICKY_MAX: usize = 64;

/// A text caret as yrs sticky indexes into one text of the document.
#[derive(Clone, Debug, PartialEq)]
pub struct StickyCaret {
    pub target: TextTarget,
    pub anchor: Vec<u8>,
    pub head: Vec<u8>,
}

/// A selection in stable ids, as a payload carries it.
#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    pub slide: SlideId,
    pub shapes: Vec<ShapeId>,
    pub text: Option<StickyCaret>,
}

/// The text body `target` names on `slide`, in the engine's deck.
fn body<'a>(slide: &'a Slide, target: &TextTarget) -> Option<&'a [Paragraph]> {
    match target {
        TextTarget::Notes(id) if *id == slide.id => Some(&slide.notes.paragraphs),
        TextTarget::Notes(_) => None,
        TextTarget::Shape(id) => slide.shape(*id)?.text.as_ref().map(|t| t.paragraphs.as_slice()),
    }
}

/// The document text a non-table text caret is in.
fn target_of(slide: &Slide, t: &TextSel) -> Option<TextTarget> {
    if t.cell.is_some() {
        return None;
    }
    Some(if t.notes { TextTarget::Notes(slide.id) } else { TextTarget::Shape(t.shape) })
}

/// The payload for this window's selection; None without a slide. The text caret is sent when
/// `replica` holds its text.
#[must_use]
pub fn payload(st: &DocState, replica: Option<&DeckDoc>) -> Option<Value> {
    let slide = st.doc.slides.get(st.selection.slide)?;
    let shapes: Vec<String> = st.selection.shapes.iter().take(SHAPES_MAX).map(|id| id_key(id.0)).collect();
    let text = st.selection.text.as_ref().and_then(|t| {
        let target = target_of(slide, t)?;
        let b = body(slide, &target)?;
        let replica = replica?;
        let anchor = replica.sticky(target.clone(), to_flat(b, t.anchor))?;
        let head = replica.sticky(target.clone(), to_flat(b, t.caret))?;
        let (k, id) = match target {
            TextTarget::Shape(id) => ("s", id.0),
            TextTarget::Notes(id) => ("n", id.0),
        };
        Some(json!({"k": k, "id": id_key(id), "a": hex(&anchor), "h": hex(&head)}))
    });
    Some(json!({"s": id_key(slide.id.0), "sh": shapes, "t": text}))
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || s.len() > 2 * STICKY_MAX {
        return None;
    }
    let digits: Option<Vec<u8>> = s.chars().map(|c| c.to_digit(16).and_then(|d| u8::try_from(d).ok())).collect();
    Some(digits?.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

/// A payload read back; None when it does not name a slide. More than [`SHAPES_MAX`] shapes are
/// cut; a text caret that does not read is left out.
#[must_use]
pub fn read(payload: &Value) -> Option<Selection> {
    let slide = SlideId(parse_key(payload.get("s")?.as_str()?)?);
    let shapes = payload
        .get("sh")
        .and_then(Value::as_array)
        .map(|a| a.iter().take(SHAPES_MAX).filter_map(|v| v.as_str().and_then(parse_key)).map(ShapeId).collect())
        .unwrap_or_default();
    let text = payload.get("t").and_then(|t| {
        let id = parse_key(t.get("id")?.as_str()?)?;
        let target = match t.get("k")?.as_str()? {
            "s" => TextTarget::Shape(ShapeId(id)),
            "n" => TextTarget::Notes(SlideId(id)),
            _ => return None,
        };
        Some(StickyCaret { target, anchor: unhex(t.get("a")?.as_str()?)?, head: unhex(t.get("h")?.as_str()?)? })
    });
    Some(Selection { slide, shapes, text })
}

/// What the carets are resolved against: this window's deck and a read handle on the replica.
/// Shared by the mapping, which keeps it current, and every caret, which reads it at each pump.
#[derive(Clone, Default)]
pub(crate) struct Scene(Arc<Mutex<SceneData>>);

#[derive(Default)]
struct SceneData {
    deck: Option<Arc<Presentation>>,
    view: Option<DocView>,
}

impl std::fmt::Debug for Scene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Scene")
    }
}

impl Scene {
    fn data(&self) -> std::sync::MutexGuard<'_, SceneData> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set_deck(&self, deck: Option<Arc<Presentation>>) {
        self.data().deck = deck;
    }

    pub(crate) fn set_view(&self, view: Option<DocView>) {
        self.data().view = view;
    }

    fn known(&self, t: Tomb) -> Known {
        self.data().view.as_ref().map_or(Known::Unknown, |v| v.known(t))
    }

    /// A caret read from `payload` against this scene; None when its slide is deleted.
    pub(crate) fn caret(&self, payload: &Value) -> Option<DeckCaret> {
        let s = read(payload)?;
        let mut c = DeckCaret { slide: s.slide, shapes: s.shapes, text: s.text, scene: self.clone() };
        self.follow(&mut c).then_some(c)
    }

    /// Drops what is deleted from `c`: its shapes and its text caret one by one, the whole caret
    /// with its slide. False when the whole caret goes.
    pub(crate) fn follow(&self, c: &mut DeckCaret) -> bool {
        if self.known(Tomb::Slide(c.slide)) == Known::Dead {
            return false;
        }
        c.shapes.retain(|id| self.known(Tomb::Shape(*id)) != Known::Dead);
        if let Some(t) = &c.text {
            let gone = match t.target {
                TextTarget::Shape(id) => self.known(Tomb::Shape(id)),
                TextTarget::Notes(id) => self.known(Tomb::Slide(id)),
            };
            if gone == Known::Dead {
                c.text = None;
            }
        }
        true
    }

    /// Where `c` is in this window's deck now. A slide, shape or text this window does not have
    /// (yet) is not drawn.
    pub(crate) fn pos(&self, c: &DeckCaret) -> DeckPos {
        let d = self.data();
        let none = DeckPos { slide: None, shapes: Vec::new(), caret: None };
        let Some(deck) = d.deck.as_deref() else { return none };
        let Some((i, slide)) = deck.slides.iter().enumerate().find(|(_, s)| s.id == c.slide) else { return none };
        let shapes = c.shapes.iter().copied().filter(|id| slide.shape(*id).is_some()).collect();
        let caret = c.text.as_ref().and_then(|t| {
            let b = body(slide, &t.target)?;
            let view = d.view.as_ref()?;
            let anchor = from_flat(b, view.resolve(&t.target, &t.anchor)?);
            let head = from_flat(b, view.resolve(&t.target, &t.head)?);
            Some(CaretPos { target: t.target.clone(), anchor, head })
        });
        DeckPos { slide: Some(i), shapes, caret }
    }
}

/// This window's own text caret as sticky indexes, taken while the engine's text and the
/// replica's agree.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Mine {
    /// The engine's caret they were taken for, and its document's revision then.
    pub sel: TextSel,
    pub revision: u64,
    pub slide: SlideId,
    target: TextTarget,
    anchor: Vec<u8>,
    head: Vec<u8>,
}

/// The same caret, whatever its line-end hint.
pub(crate) fn same_caret(a: &TextSel, b: &TextSel) -> bool {
    (a.shape, a.anchor, a.caret, a.cell, a.notes) == (b.shape, b.anchor, b.caret, b.cell, b.notes)
}

/// The sticky indexes of `st`'s text caret in `replica`; None without a text caret, or while the
/// replica's text differs from the engine's (a held remote change).
pub(crate) fn mine(st: &DocState, replica: &DeckDoc) -> Option<Mine> {
    let t = st.selection.text.as_ref()?;
    let slide = st.doc.slides.get(st.selection.slide)?;
    let target = target_of(slide, t)?;
    let b = body(slide, &target)?;
    let theirs = replica.text(target.clone())?;
    if !edit_ops(b, &theirs).is_empty() {
        return None;
    }
    let anchor = replica.sticky(target.clone(), to_flat(b, t.anchor))?;
    let head = replica.sticky(target.clone(), to_flat(b, t.caret))?;
    Some(Mine { sel: t.clone(), revision: st.revision, slide: slide.id, target, anchor, head })
}

/// Where `m` is now in `new`, the engine's rebuilt text of its target; None when the sticky
/// indexes do not resolve.
pub(crate) fn place(m: &Mine, replica: &DeckDoc, new: &[Paragraph]) -> Option<TextSel> {
    let anchor = from_flat(new, replica.resolve(m.target.clone(), &m.anchor)?);
    let caret = from_flat(new, replica.resolve(m.target.clone(), &m.head)?);
    Some(TextSel { anchor, caret, line_end: None, ..m.sel.clone() })
}
