//! The safety net: a local edit that changed something the room does not share is refused, put
//! back, and nothing is sent. The gate ([`crate::gate`]) refuses such commands up front; the net
//! catches the ones that slip past it (a command run without the gate, a shared command that
//! touches an unshared part on some decks).
//!
//! Everything outside slides' and shapes' shared keys and their text is compared: the
//! presentation's own parts, and per slide its animations, comments, tags, kept XML and the notes'
//! body properties; per shape its `keep` part, its parent, a `kind` change of a table, chart,
//! media, picture, ink, 3-D model or opaque shape, and a new shape of those kinds. The animation
//! changes the delete routine makes for the shapes an edit deleted are allowed (Ruling D7): they
//! are compared against the routine's real output. A copied slide or shape that carries
//! animations or comments is refused with [`COPY_UNSHARED_TEXT`] (Ruling D15).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use deckcraft_model::{MediaItem, Presentation, Shape, ShapeId, ShapeKind, Slide, SlideId};

use crate::gate::{MEDIA_TEXT, NOT_YET_TEXT};

/// Why a copy of slides or shapes with animations or comments is refused (Ruling D15).
pub const COPY_UNSHARED_TEXT: &str = "Slides or shapes with animations or comments can't be copied while editing live yet";

/// Why `after` cannot be shared as an edit of `before`, or None when it can.
#[must_use]
pub fn unshared(before: &Presentation, after: &Presentation) -> Option<&'static str> {
    if !same_media(&before.media, &after.media) {
        return Some(MEDIA_TEXT);
    }
    let same_list = |a: &[Arc<deckcraft_model::Master>], b: &[Arc<deckcraft_model::Master>]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| Arc::ptr_eq(x, y) || x == y)
    };
    let same_opt = |a: &Option<Arc<deckcraft_model::Master>>, b: &Option<Arc<deckcraft_model::Master>>| match (a, b) {
        (Some(x), Some(y)) => Arc::ptr_eq(x, y) || x == y,
        (None, None) => true,
        _ => false,
    };
    let outside_slides = before.slide_size == after.slide_size
        && before.notes_size == after.notes_size
        && same_list(&before.masters, &after.masters)
        && same_opt(&before.notes_master, &after.notes_master)
        && same_opt(&before.handout_master, &after.handout_master)
        && before.sections == after.sections
        && before.custom_shows == after.custom_shows
        && before.show == after.show
        && before.header_footer == after.header_footer
        && before.props == after.props
        && before.default_text_style == after.default_text_style
        && before.first_slide_number == after.first_slide_number
        && before.next_id == after.next_id
        && before.embedded_fonts == after.embedded_fonts
        && before.table_styles == after.table_styles
        && before.tags == after.tags
        && before.kept_parts == after.kept_parts
        && before.guides == after.guides
        && before.raw_ext == after.raw_ext;
    if !outside_slides {
        return Some(NOT_YET_TEXT);
    }
    let earlier: HashMap<SlideId, &Arc<Slide>> = before.slides.iter().map(|s| (s.id, s)).collect();
    let mut everywhere: Option<HashSet<ShapeId>> = None;
    for s in &after.slides {
        let why = match earlier.get(&s.id) {
            Some(b) if Arc::ptr_eq(b, s) => None,
            Some(b) => changed_slide(before, b, s, &mut everywhere),
            None => new_slide(before, s, &mut everywhere),
        };
        if why.is_some() {
            return why;
        }
    }
    None
}

/// Media lists equal: the same items, the bytes compared by pointer first.
fn same_media(a: &[MediaItem], b: &[MediaItem]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.id == y.id
                && x.name == y.name
                && x.content_type == y.content_type
                && x.link == y.link
                && (Arc::ptr_eq(&x.data, &y.data) || x.data == y.data)
        })
}

/// A kind only shared as it was: its whole value is one opaque key that M1 never edits.
fn fixed_kind(k: &ShapeKind) -> bool {
    !matches!(k, ShapeKind::Shape | ShapeKind::Group { .. } | ShapeKind::Connector { .. })
}

fn media_kind(k: &ShapeKind) -> bool {
    matches!(k, ShapeKind::Picture { .. } | ShapeKind::Media(_) | ShapeKind::Model3d { .. })
}

/// Every shape of a slide's tree with its parent.
fn tree(s: &Slide) -> HashMap<ShapeId, (&Shape, Option<ShapeId>)> {
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
    rec(&mut out, &s.shapes, None, 0);
    out
}

/// Every shape id of `p`, made on first use.
fn all_ids<'a>(p: &Presentation, cache: &'a mut Option<HashSet<ShapeId>>) -> &'a HashSet<ShapeId> {
    cache.get_or_insert_with(|| {
        let mut ids = HashSet::new();
        for s in &p.slides {
            deckcraft_model::walk(&s.shapes, &mut |sh, _| {
                ids.insert(sh.id);
            });
        }
        ids
    })
}

/// A shape new to its slide: of a kind M1 does not share, or come from another slide.
fn new_shape(before: &Presentation, s: &Shape, everywhere: &mut Option<HashSet<ShapeId>>) -> Option<&'static str> {
    if fixed_kind(&s.kind) {
        return Some(if media_kind(&s.kind) { MEDIA_TEXT } else { NOT_YET_TEXT });
    }
    // Moved from another slide under the same id: a re-parent across slides.
    if all_ids(before, everywhere).contains(&s.id) {
        return Some(NOT_YET_TEXT);
    }
    None
}

fn new_slide(before: &Presentation, s: &Slide, everywhere: &mut Option<HashSet<ShapeId>>) -> Option<&'static str> {
    if !s.animations.is_empty() || !s.comments.is_empty() {
        return Some(COPY_UNSHARED_TEXT);
    }
    if !s.tags.is_empty() || s.raw_ext.is_some() {
        return Some(NOT_YET_TEXT);
    }
    let mut why = None;
    deckcraft_model::walk(&s.shapes, &mut |sh, _| {
        if why.is_none() {
            why = new_shape(before, sh, everywhere);
        }
    });
    why
}

fn changed_slide(before: &Presentation, b: &Arc<Slide>, a: &Slide, everywhere: &mut Option<HashSet<ShapeId>>) -> Option<&'static str> {
    if b.comments != a.comments || b.tags != a.tags || b.raw_ext != a.raw_ext {
        return Some(NOT_YET_TEXT);
    }
    if b.notes.body != a.notes.body || b.notes.list_style != a.notes.list_style {
        return Some(NOT_YET_TEXT);
    }
    let (tb, ta) = (tree(b), tree(a));
    for (id, (sa, pa)) in &ta {
        match tb.get(id) {
            Some((sb, pb)) => {
                if pb != pa || sb.tags != sa.tags || sb.raw_ext != sa.raw_ext || sb.diagram != sa.diagram {
                    return Some(NOT_YET_TEXT);
                }
                if (fixed_kind(&sb.kind) || fixed_kind(&sa.kind)) && sb.kind != sa.kind {
                    return Some(NOT_YET_TEXT);
                }
                // A text body is never removed live (there is no write for it).
                if sb.text.is_some() && sa.text.is_none() {
                    return Some(NOT_YET_TEXT);
                }
            }
            None => {
                if let Some(why) = new_shape(before, sa, everywhere) {
                    return Some(why);
                }
            }
        }
    }
    if b.animations != a.animations {
        // A copied shape that brought its animations along (Ruling D15).
        if a.animations.iter().any(|x| !tb.contains_key(&x.shape)) {
            return Some(COPY_UNSHARED_TEXT);
        }
        // Allowed only as the delete routine leaves them for the shapes that went (D7).
        let gone: Vec<ShapeId> = tb.keys().filter(|id| !ta.contains_key(id)).copied().collect();
        if gone.is_empty() {
            return Some(NOT_YET_TEXT);
        }
        let mut p = Presentation { slides: vec![Arc::clone(b)], ..Presentation::default() };
        p.slide_size = before.slide_size;
        deckcraft_engine::delete_shapes_in(&mut p, 0, &gone);
        if p.slides.first().is_none_or(|s| s.animations != a.animations) {
            return Some(NOT_YET_TEXT);
        }
    }
    None
}
