//! [`DeckDoc`]: the room's yrs document in the `deckcraft/1` schema ([`crate::schema`]).
//!
//! It holds every slide and shape of the pinned base deck (the seed) and what peers changed since.
//! Reads go through a parsed index of the entries, kept up to date by the change observers, so a
//! value that does not read (a peer's junk) keeps the earlier one: never a panic, never a refused
//! update (GridCraft G8 I2).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};

use craft_collab::replica::{LOCAL, REMOTE, UNTRACKED, integrate, to_any, to_json};
use craft_collab::yrs::types::text::YChange;
use craft_collab::yrs::types::{Event, PathSegment};
use craft_collab::yrs::updates::decoder::Decode;
use craft_collab::yrs::updates::encoder::Encode;
use craft_collab::yrs::{
    Any, Assoc, DeepObservable, Doc, IndexedSequence, Map, MapPrelim, MapRef, OffsetKind, Out, ReadTxn, StickyIndex, Text, TextPrelim, TextRef,
    Transact, TransactionMut,
};
use craft_collab::{LiveError, Replica, YReplica};
use deckcraft_model::text::{BodyProps, ListStyle, Paragraph, TextBody};
use deckcraft_model::{Diagram, LayoutId, Presentation, Shape, ShapeId, ShapeKind, Slide, SlideId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::order;
use crate::schema::{
    BACKGROUND, BODY, CLICK, COLOR_MAP, DECORATIVE, DEL, DEL_BY, DESCR, EFFECTS, FILL, GEOM, HIDDEN, HOVER, KEEP, KIND, LAYOUT, LINE, LOCKED, META,
    META_BASE, NAME, NOTES, PARENT, PH, POS, SHAPE_FIELDS, SHAPES, SHOW_MASTER, SLIDE, SLIDE_FIELDS, SLIDES, STYLE, TEXT, TEXT_BOX, TITLE,
    TRANSITION, XFRM, Z, hex, id_key, parse_key, unhex32,
};
use crate::text::{TextOp, apply_ops, read_body, write_body};

/// The key the change observers are registered under.
const OBSERVER: &str = "deckcraft-live/changes";
/// How deep a parent chain may go before it is read as broken (the model walks 64 levels).
const MAX_DEPTH: usize = 64;

/// What a room is seeded from: the base bytes' hash and the deck they import to. Equal seeds give
/// equal item ids on every replica (the import is deterministic, D1).
pub struct DeckSeed {
    pub base_sha256: [u8; 32],
    pub deck: Arc<Presentation>,
}

/// What one remote update changed.
#[derive(Clone, Debug, PartialEq)]
pub enum DeckChange {
    /// Slides whose entry changed (order, tombstone, layout or a field), or new ones.
    Slides(Vec<SlideId>),
    /// Shapes whose entry changed, or new ones.
    Shapes(Vec<ShapeId>),
    /// A shape's text.
    Text(ShapeId),
    /// A slide's speaker notes.
    Notes(SlideId),
    Meta,
}

/// A text body in the document.
#[derive(Clone, Debug, PartialEq)]
pub enum TextTarget {
    Shape(ShapeId),
    Notes(SlideId),
}

/// A slide's entry as last read well.
#[derive(Clone, Debug, PartialEq)]
pub struct SlideEntry {
    pub pos: String,
    pub del: bool,
    pub del_by: Option<String>,
    pub layout: LayoutId,
    /// The latest-wins keys ([`SLIDE_FIELDS`]) that are set and read as their model type.
    pub fields: BTreeMap<String, Value>,
}

/// A shape's entry as last read well.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeEntry {
    pub slide: SlideId,
    pub parent: Option<ShapeId>,
    pub z: String,
    pub del: bool,
    pub del_by: Option<String>,
    /// The entry holds a `text` YText.
    pub has_text: bool,
    /// The latest-wins keys ([`SHAPE_FIELDS`]) that are set and read as their model type.
    pub fields: BTreeMap<String, Value>,
}

/// A local write that did not complete. `update` holds what `f` had already written before it
/// erred, which this replica keeps: it must be sent. There is deliberately no conversion to
/// [`LiveError`], so `?` cannot drop the update.
#[derive(Debug)]
pub struct WriteError {
    pub error: LiveError,
    pub update: Option<Vec<u8>>,
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for WriteError {}

/// A slide or shape to tombstone or bring back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tomb {
    Slide(SlideId),
    Shape(ShapeId),
}

/// One latest-wins key of an entry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyRef {
    Slide(SlideId, String),
    Shape(ShapeId, String),
}

/// The `body` key: a text body without its paragraphs.
#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BodyKey {
    body: BodyProps,
    #[serde(skip_serializing_if = "ListStyle::is_empty")]
    list_style: ListStyle,
}

/// The `keep` key: carried from the file, never edited live (Ruling D25).
#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Keep {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_ext: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagram: Option<Diagram>,
}

impl Keep {
    fn of(s: &Shape) -> Keep {
        Keep { tags: s.tags.clone(), raw_ext: s.raw_ext.clone(), diagram: s.diagram.clone() }
    }
    fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.raw_ext.is_none() && self.diagram.is_none()
    }
}

fn json<T: Serialize>(v: &T) -> Option<Value> {
    serde_json::to_value(v).map_err(|e| log::warn!("deckcraft-live: a value did not encode ({e})")).ok()
}

fn de<T: serde::de::DeserializeOwned>(v: &Value) -> Option<T> {
    T::deserialize(v).ok()
}

/// A shape's latest-wins keys and their values, `keep` only when it holds something and `body`
/// only with a text body. A group's `kind` is written without its children.
pub(crate) fn shape_values(s: &Shape) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    let mut put = |k: &'static str, v: Option<Value>| {
        if let Some(v) = v {
            out.push((k, v));
        }
    };
    if let Some(t) = &s.text {
        put(BODY, json(&BodyKey { body: t.body.clone(), list_style: t.list_style.clone() }));
    }
    put(CLICK, json(&s.click));
    put(DECORATIVE, json(&s.decorative));
    put(DESCR, json(&s.descr));
    put(EFFECTS, json(&s.effects));
    put(FILL, json(&s.fill));
    put(GEOM, json(&s.geom));
    put(HIDDEN, json(&s.hidden));
    put(HOVER, json(&s.hover));
    let keep = Keep::of(s);
    if !keep.is_empty() {
        put(KEEP, json(&keep));
    }
    let kind = match &s.kind {
        ShapeKind::Group { child, .. } => json(&ShapeKind::Group { children: Vec::new(), child: *child }),
        k => json(k),
    };
    put(KIND, kind);
    put(LINE, json(&s.line));
    put(LOCKED, json(&s.locked));
    put(NAME, json(&s.name));
    put(PH, json(&s.ph));
    put(STYLE, json(&s.style));
    put(TEXT_BOX, json(&s.text_box));
    put(TITLE, json(&s.title));
    put(XFRM, json(&s.xfrm));
    out
}

/// Set one latest-wins key on `s`. False (and `s` unchanged) when the value does not read as the
/// field's type, an `xfrm` is not finite, or the key is not a shape field. A group's children are
/// left for the caller; `body` needs a text body to land in.
pub(crate) fn apply_shape_field(s: &mut Shape, key: &str, v: &Value) -> bool {
    fn set<T: serde::de::DeserializeOwned>(slot: &mut T, v: &Value) -> bool {
        de::<T>(v).map(|x| *slot = x).is_some()
    }
    match key {
        BODY => match de::<BodyKey>(v) {
            Some(b) => {
                if let Some(t) = s.text.as_mut() {
                    t.body = b.body;
                    t.list_style = b.list_style;
                }
                true
            }
            None => false,
        },
        CLICK => set(&mut s.click, v),
        DECORATIVE => set(&mut s.decorative, v),
        DESCR => set(&mut s.descr, v),
        EFFECTS => set(&mut s.effects, v),
        FILL => set(&mut s.fill, v),
        GEOM => set(&mut s.geom, v),
        HIDDEN => set(&mut s.hidden, v),
        HOVER => set(&mut s.hover, v),
        KEEP => match de::<Keep>(v) {
            Some(k) => {
                s.tags = k.tags;
                s.raw_ext = k.raw_ext;
                s.diagram = k.diagram;
                true
            }
            None => false,
        },
        KIND => set(&mut s.kind, v),
        LINE => set(&mut s.line, v),
        LOCKED => set(&mut s.locked, v),
        NAME => set(&mut s.name, v),
        PH => set(&mut s.ph, v),
        STYLE => set(&mut s.style, v),
        TEXT_BOX => set(&mut s.text_box, v),
        TITLE => set(&mut s.title, v),
        XFRM => match de::<Option<deckcraft_model::Xfrm>>(v) {
            Some(x) if x.is_none_or(|x| [x.x, x.y, x.w, x.h, x.rot].iter().all(|f| f.is_finite())) => {
                s.xfrm = x;
                true
            }
            _ => false,
        },
        _ => false,
    }
}

/// A slide's latest-wins keys and their values.
pub(crate) fn slide_values(s: &Slide) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    let mut put = |k: &'static str, v: Option<Value>| {
        if let Some(v) = v {
            out.push((k, v));
        }
    };
    put(BACKGROUND, json(&s.background));
    put(COLOR_MAP, json(&s.color_map));
    put(HIDDEN, json(&s.hidden));
    put(NAME, json(&s.name));
    put(SHOW_MASTER, json(&s.show_master_shapes));
    put(TRANSITION, json(&s.transition));
    out
}

/// Set one latest-wins key on a slide; false when it does not read.
pub(crate) fn apply_slide_field(s: &mut Slide, key: &str, v: &Value) -> bool {
    fn set<T: serde::de::DeserializeOwned>(slot: &mut T, v: &Value) -> bool {
        de::<T>(v).map(|x| *slot = x).is_some()
    }
    match key {
        BACKGROUND => set(&mut s.background, v),
        COLOR_MAP => set(&mut s.color_map, v),
        HIDDEN => set(&mut s.hidden, v),
        NAME => set(&mut s.name, v),
        SHOW_MASTER => set(&mut s.show_master_shapes, v),
        TRANSITION => set(&mut s.transition, v),
        _ => false,
    }
}

/// Whether a shape entry's `kind` names a group.
pub(crate) fn is_group_kind(fields: &BTreeMap<String, Value>) -> bool {
    fields.get(KIND).and_then(|k| k.get("type")).and_then(Value::as_str) == Some("group")
}

/// What the observers saw, before it is grouped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Seen {
    Slide(SlideId),
    Shape(ShapeId),
    Text(ShapeId),
    Notes(SlideId),
    Meta,
}

#[derive(Clone)]
struct Roots {
    meta: MapRef,
    slides: MapRef,
    shapes: MapRef,
}

impl Roots {
    fn of(doc: &Doc) -> Roots {
        Roots { meta: doc.get_or_insert_map(META), slides: doc.get_or_insert_map(SLIDES), shapes: doc.get_or_insert_map(SHAPES) }
    }

    fn entry<T: ReadTxn>(root: &MapRef, txn: &T, id: u32) -> Option<MapRef> {
        match root.get(txn, &id_key(id)) {
            Some(Out::YMap(m)) => Some(m),
            _ => None,
        }
    }

    fn text<T: ReadTxn>(&self, txn: &T, target: &TextTarget) -> Option<TextRef> {
        let (entry, key) = match target {
            TextTarget::Shape(id) => (Self::entry(&self.shapes, txn, id.0)?, TEXT),
            TextTarget::Notes(id) => (Self::entry(&self.slides, txn, id.0)?, NOTES),
        };
        match entry.get(txn, key) {
            Some(Out::YText(t)) => Some(t),
            _ => None,
        }
    }
}

/// One replica of a room's deck document.
pub struct DeckDoc {
    y: YReplica,
    roots: Roots,
    /// What the observers saw, drained after every transaction.
    seen: Arc<Mutex<Vec<Seen>>>,
    slides: BTreeMap<SlideId, SlideEntry>,
    shapes: BTreeMap<ShapeId, ShapeEntry>,
    /// Per entry, the keys that hold junk. The index is a function of the state alone: a
    /// replica built from the state reads exactly as a live one.
    slide_junk: BTreeMap<SlideId, BTreeSet<&'static str>>,
    shape_junk: BTreeMap<ShapeId, BTreeSet<&'static str>>,
}

impl DeckDoc {
    fn assemble(doc: Doc, client: Option<u32>) -> DeckDoc {
        let roots = Roots::of(&doc);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        roots.slides.observe_deep(OBSERVER, move |txn, events| {
            let found: Vec<Seen> = events.iter().flat_map(|e| seen_in(txn, e, true)).collect();
            push_seen(&sink, found);
        });
        let sink = Arc::clone(&seen);
        roots.shapes.observe_deep(OBSERVER, move |txn, events| {
            let found: Vec<Seen> = events.iter().flat_map(|e| seen_in(txn, e, false)).collect();
            push_seen(&sink, found);
        });
        let sink = Arc::clone(&seen);
        roots.meta.observe_deep(OBSERVER, move |_, _| push_seen(&sink, vec![Seen::Meta]));
        let scope = roots.clone();
        let y = YReplica::assemble(doc, client, move |doc, undo| {
            undo.expand_scope(doc, &scope.slides);
            undo.expand_scope(doc, &scope.shapes);
        });
        let mut d =
            DeckDoc { y, roots, seen, slides: BTreeMap::new(), shapes: BTreeMap::new(), slide_junk: BTreeMap::new(), shape_junk: BTreeMap::new() };
        d.index_all();
        d
    }

    /// Parse every entry (a new replica).
    fn index_all(&mut self) {
        let txn = self.y.doc().transact();
        let slides: Vec<u32> = self.roots.slides.keys(&txn).filter_map(parse_key).collect();
        let shapes: Vec<u32> = self.roots.shapes.keys(&txn).filter_map(parse_key).collect();
        drop(txn);
        for id in slides {
            self.reindex_slide(SlideId(id));
        }
        for id in shapes {
            self.reindex_shape(ShapeId(id));
        }
    }

    fn reindex_slide(&mut self, id: SlideId) {
        let txn = self.y.doc().transact();
        let mut bad = BTreeSet::new();
        let e = match Roots::entry(&self.roots.slides, &txn, id.0) {
            Some(m) => {
                let del_by = read_by(&m, &txn).or(DEL_BY, None, None, &mut bad);
                SlideEntry {
                    pos: read_order(&m, &txn, POS).or(POS, String::new(), String::new(), &mut bad),
                    del: read_del(&m, &txn, del_by.is_some(), &mut bad),
                    del_by,
                    layout: LayoutId(read_id(&m, &txn, LAYOUT).or(LAYOUT, 0, 0, &mut bad)),
                    fields: read_fields(&m, &txn, &SLIDE_FIELDS, &mut bad, |k, v| apply_slide_field(&mut Slide::default(), k, v)),
                }
            }
            None if self.roots.slides.get(&txn, &id_key(id.0)).is_some() => {
                log::warn!("deckcraft-live: a slide entry is not a map; the base's slide is used");
                bad.extend([POS, DEL, LAYOUT]);
                SlideEntry { pos: String::new(), del: false, del_by: None, layout: LayoutId(0), fields: BTreeMap::new() }
            }
            None => return,
        };
        drop(txn);
        self.slides.insert(id, e);
        self.slide_junk.insert(id, bad);
    }

    fn reindex_shape(&mut self, id: ShapeId) {
        let txn = self.y.doc().transact();
        let mut bad = BTreeSet::new();
        let e = match Roots::entry(&self.roots.shapes, &txn, id.0) {
            Some(m) => {
                let parent = read_id(&m, &txn, PARENT).or(PARENT, 0, 0, &mut bad);
                let fields = read_fields(&m, &txn, &SHAPE_FIELDS, &mut bad, |k, v| {
                    let mut s = Shape { text: Some(TextBody::default()), ..Default::default() };
                    apply_shape_field(&mut s, k, v)
                });
                let del_by = read_by(&m, &txn).or(DEL_BY, None, None, &mut bad);
                ShapeEntry {
                    slide: SlideId(read_id(&m, &txn, SLIDE).or(SLIDE, 0, 0, &mut bad)),
                    parent: (parent != 0).then_some(ShapeId(parent)),
                    z: read_order(&m, &txn, Z).or(Z, String::new(), String::new(), &mut bad),
                    del: read_del(&m, &txn, del_by.is_some(), &mut bad),
                    del_by,
                    // A text body is there when `body` is (Ruling D26 as amended in L3 fix round 1).
                    has_text: fields.contains_key(BODY),
                    fields,
                }
            }
            None if self.roots.shapes.get(&txn, &id_key(id.0)).is_some() => {
                log::warn!("deckcraft-live: a shape entry is not a map; the base's shape is used");
                bad.extend([SLIDE, PARENT, Z, DEL, BODY]);
                ShapeEntry { slide: SlideId(0), parent: None, z: String::new(), del: false, del_by: None, has_text: false, fields: BTreeMap::new() }
            }
            None => return,
        };
        drop(txn);
        self.shapes.insert(id, e);
        self.shape_junk.insert(id, bad);
    }

    /// The keys of a slide entry that hold junk (the rebuild uses the base's value for them).
    pub(crate) fn slide_junk(&self, id: SlideId) -> Option<&BTreeSet<&'static str>> {
        self.slide_junk.get(&id).filter(|b| !b.is_empty())
    }

    /// The keys of a shape entry that hold junk.
    pub(crate) fn shape_junk(&self, id: ShapeId) -> Option<&BTreeSet<&'static str>> {
        self.shape_junk.get(&id).filter(|b| !b.is_empty())
    }

    /// Every slide entry, as indexed.
    pub(crate) fn slide_entries(&self) -> &BTreeMap<SlideId, SlideEntry> {
        &self.slides
    }

    /// Every shape entry, as indexed.
    pub(crate) fn shape_entries(&self) -> &BTreeMap<ShapeId, ShapeEntry> {
        &self.shapes
    }

    /// Drain what the observers saw and bring the index up to date with it.
    fn settle(&mut self) -> Vec<Seen> {
        let seen: BTreeSet<Seen> = std::mem::take(&mut *self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner)).into_iter().collect();
        for s in &seen {
            match s {
                Seen::Slide(id) => self.reindex_slide(*id),
                Seen::Shape(id) => self.reindex_shape(*id),
                Seen::Text(_) | Seen::Notes(_) | Seen::Meta => {}
            }
        }
        seen.into_iter().collect()
    }

    /// Whether this replica dropped updates it held for dependencies that never came: it is sound
    /// but behind. C_REV's craft-live cannot replace a replica in place, so the link ends (L4).
    #[must_use]
    pub fn needs_resync(&self) -> bool {
        self.y.needs_resync()
    }

    #[must_use]
    pub fn base_sha256(&self) -> Option<[u8; 32]> {
        let txn = self.y.doc().transact();
        match self.roots.meta.get(&txn, META_BASE) {
            Some(Out::Any(Any::String(s))) => unhex32(&s),
            _ => None,
        }
    }

    /// The live slides, by (pos, id).
    #[must_use]
    pub fn slide_order(&self) -> Vec<SlideId> {
        order::order(self.slides.iter().filter(|(_, e)| !e.del).map(|(id, e)| (*id, e.pos.clone())))
    }

    #[must_use]
    pub fn slide(&self, id: SlideId) -> Option<SlideEntry> {
        self.slides.get(&id).cloned()
    }

    #[must_use]
    pub fn shape(&self, id: ShapeId) -> Option<ShapeEntry> {
        self.shapes.get(&id).cloned()
    }

    /// Every shape entry on `slide`, tombstones included, as (id, effective parent).
    pub(crate) fn tree_of(&self, slide: SlideId) -> Vec<(ShapeId, Option<ShapeId>)> {
        tree_in(&self.shapes, slide)
    }

    /// The live shapes of `slide` in tree order: siblings by (z, id), each group followed by its
    /// children. A tombstoned group's children are not live.
    #[must_use]
    pub fn shapes_of(&self, slide: SlideId) -> Vec<ShapeId> {
        let tree = self.tree_of(slide);
        let mut kids: BTreeMap<Option<ShapeId>, Vec<(ShapeId, String)>> = BTreeMap::new();
        for (id, p) in &tree {
            if let Some(e) = self.shapes.get(id) {
                kids.entry(*p).or_default().push((*id, e.z.clone()));
            }
        }
        let mut out = Vec::new();
        fn rec(d: &DeckDoc, kids: &BTreeMap<Option<ShapeId>, Vec<(ShapeId, String)>>, at: Option<ShapeId>, out: &mut Vec<ShapeId>) {
            let Some(list) = kids.get(&at) else { return };
            for id in order::order(list.iter().cloned()) {
                if d.shapes.get(&id).is_some_and(|e| !e.del) {
                    out.push(id);
                    rec(d, kids, Some(id), out);
                }
            }
        }
        rec(self, &kids, None, &mut out);
        out
    }

    /// The paragraphs of a text body; None when there is no such YText.
    #[must_use]
    pub fn text(&self, target: TextTarget) -> Option<Vec<Paragraph>> {
        let txn = self.y.doc().transact();
        let t = self.roots.text(&txn, &target)?;
        Some(read_body(&txn, &t))
    }

    /// A position (chars of the flat form, as [`TextOp`] counts) as a yrs sticky index, `encode_v1`.
    #[must_use]
    pub fn sticky(&self, target: TextTarget, at: u32) -> Option<Vec<u8>> {
        let txn = self.y.doc().transact();
        let t = self.roots.text(&txn, &target)?;
        let kind = self.y.doc().offset_kind();
        let units = units(&txn, &t, kind);
        let at = usize::try_from(at).ok()?.min(units.len());
        let offset = units.iter().take(at).sum::<u32>();
        t.sticky_index(&txn, offset, Assoc::After).map(|s| s.encode_v1())
    }

    /// Where a sticky index from [`sticky`](Self::sticky) is now, in chars of the flat form. None
    /// for bytes that do not decode or name another text.
    #[must_use]
    pub fn resolve(&self, target: TextTarget, sticky: &[u8]) -> Option<u32> {
        let s = StickyIndex::decode_v1(sticky).ok()?;
        let txn = self.y.doc().transact();
        let t = self.roots.text(&txn, &target)?;
        let off = s.get_offset(&txn)?;
        let own: &craft_collab::yrs::branch::Branch = t.as_ref();
        if off.branch != craft_collab::yrs::branch::BranchPtr::from(own) {
            return None;
        }
        let kind = self.y.doc().offset_kind();
        let mut sum = 0u32;
        let mut n = 0u32;
        for u in units(&txn, &t, kind) {
            if sum >= off.index {
                break;
            }
            sum += u;
            n += 1;
        }
        Some(n)
    }

    /// The encoded state's size, to hold against [`MAX_SEED_BYTES`](crate::schema::MAX_SEED_BYTES).
    #[must_use]
    pub fn seed_bytes(&self) -> usize {
        self.y.state().len()
    }

    /// Run `f` as one local transaction and return its update. `tracked`: in the yrs undo history
    /// (text, Ruling D4); otherwise untracked.
    ///
    /// A yrs transaction commits what it holds, so an `Err` from `f` after it wrote still leaves
    /// the write in this replica. The update for it is then handed out in [`WriteError::update`]:
    /// the caller must send it like any other, or every later update from this client would wait
    /// at every peer for the missing clocks (L3 fix round 1). Refused before any write, with no
    /// update: [`LiveError::Damaged`] on a damaged replica, [`LiveError::NoClient`] before a space
    /// was granted.
    pub fn write(&mut self, tracked: bool, f: impl FnOnce(&mut DeckTxn) -> Result<(), LiveError>) -> Result<Vec<u8>, WriteError> {
        if self.y.damaged() {
            return Err(WriteError { error: LiveError::Damaged, update: None });
        }
        if self.y.client().is_none() {
            return Err(WriteError { error: LiveError::NoClient, update: None });
        }
        let (result, update) = {
            let mut t = DeckTxn { txn: self.y.doc().transact_mut_with(if tracked { LOCAL } else { UNTRACKED }), roots: &self.roots, skipped: 0 };
            let r = f(&mut t);
            if t.skipped > 0 {
                log::warn!("deckcraft-live: {} writes named an entry or a text the document does not hold; skipped", t.skipped);
            }
            (r, t.txn.encode_update_v1())
        };
        self.settle();
        match result {
            Ok(()) => Ok(update),
            Err(error) => {
                let wrote = update.as_slice() != craft_collab::yrs::Update::EMPTY_V1;
                Err(WriteError { error, update: wrote.then_some(update) })
            }
        }
    }

    /// Undo this replica's last tracked (text) transaction; the update to send.
    pub fn undo_text(&mut self) -> Option<Vec<u8>> {
        let u = self.y.undo();
        self.settle();
        u
    }

    pub fn redo_text(&mut self) -> Option<Vec<u8>> {
        let u = self.y.redo();
        self.settle();
        u
    }

    fn changes(seen: Vec<Seen>) -> Vec<DeckChange> {
        let mut slides = Vec::new();
        let mut shapes = Vec::new();
        let mut texts = Vec::new();
        let mut notes = Vec::new();
        let mut meta = false;
        for s in seen {
            match s {
                Seen::Slide(id) => slides.push(id),
                Seen::Shape(id) => shapes.push(id),
                Seen::Text(id) => texts.push(DeckChange::Text(id)),
                Seen::Notes(id) => notes.push(DeckChange::Notes(id)),
                Seen::Meta => meta = true,
            }
        }
        let mut out = Vec::new();
        if meta {
            out.push(DeckChange::Meta);
        }
        if !slides.is_empty() {
            out.push(DeckChange::Slides(slides));
        }
        if !shapes.is_empty() {
            out.push(DeckChange::Shapes(shapes));
        }
        out.extend(texts);
        out.extend(notes);
        out
    }
}

impl Replica for DeckDoc {
    type Change = DeckChange;
    type Seed = DeckSeed;

    /// The seed as client 0, in one transaction: `meta`, then each slide of the base in order
    /// followed by its shapes (group children flattened, depth first), every entry's keys sorted
    /// and written one call per key. `pos` and each sibling list's `z` come from `seed_keys`.
    fn seeded(seed: &DeckSeed) -> Result<DeckDoc, LiveError> {
        let doc = YReplica::seed_doc();
        let roots = Roots::of(&doc);
        {
            let mut t = DeckTxn { txn: doc.transact_mut_with(REMOTE), roots: &roots, skipped: 0 };
            t.roots.meta.insert(&mut t.txn, META_BASE, Any::from(hex(&seed.base_sha256)));
            let pos = order::seed_keys(seed.deck.slides.len());
            for (slide, pos) in seed.deck.slides.iter().zip(&pos) {
                t.put_slide(slide, pos);
                t.put_tree(slide.id, None, &slide.shapes, 0);
            }
            if pos.len() < seed.deck.slides.len() || t.skipped > 0 {
                return Err(LiveError::OutOfRange);
            }
        }
        Ok(DeckDoc::assemble(doc, None))
    }

    fn from_state(state: &[u8]) -> Result<DeckDoc, LiveError> {
        let doc = YReplica::seed_doc();
        integrate(&doc, state)?;
        Ok(DeckDoc::assemble(doc, None))
    }

    /// Space 0 and the space already held are no-ops; any other space rebuilds the document
    /// minting under it, the state carried across (the index is a function of it).
    fn set_client(&mut self, space: u32) -> Result<(), LiveError> {
        if space == 0 {
            log::warn!("deckcraft-live: client id 0 belongs to the seed and is never adopted");
            return Ok(());
        }
        if self.y.client() == Some(space) {
            return Ok(());
        }
        let doc = self.y.rebuilt(space)?;
        let old = std::mem::replace(self, DeckDoc::assemble(doc, Some(space)));
        self.y.carry_marks(&old.y);
        Ok(())
    }

    fn apply(&mut self, update: &[u8]) -> Result<Vec<DeckChange>, LiveError> {
        self.settle();
        let r = self.y.apply(update);
        let seen = self.settle();
        r.map(|()| DeckDoc::changes(seen))
    }

    fn state(&self) -> Vec<u8> {
        self.y.state()
    }

    /// Never: undo is DeckCraft's own log (Ruling D4); text goes through [`DeckDoc::undo_text`].
    fn undo(&mut self) -> Option<Vec<u8>> {
        None
    }

    fn redo(&mut self) -> Option<Vec<u8>> {
        None
    }

    fn damaged(&self) -> bool {
        self.y.damaged()
    }

    fn needs_resync(&self) -> bool {
        self.y.needs_resync()
    }
}

/// One local transaction on a [`DeckDoc`]. A write naming an entry the document does not hold
/// is skipped and logged.
pub struct DeckTxn<'a> {
    txn: TransactionMut<'a>,
    roots: &'a Roots,
    skipped: usize,
}

/// A value for one key of a new entry.
enum New<'b> {
    Any(Any),
    Text(&'b TextBody),
}

impl DeckTxn<'_> {
    /// A new entry under `root`, its keys written in key order, one call per key.
    fn put_entry(&mut self, root: &MapRef, id: u32, keys: BTreeMap<&'static str, New>) {
        let entry = root.insert(&mut self.txn, id_key(id), MapPrelim::default());
        for (k, v) in keys {
            match v {
                New::Any(a) => {
                    entry.insert(&mut self.txn, k, a);
                }
                New::Text(body) => {
                    let text = entry.insert(&mut self.txn, k, TextPrelim::new(""));
                    write_body(&mut self.txn, &text, body);
                }
            }
        }
    }

    /// A new slide: every key, its notes as a YText.
    pub fn put_slide(&mut self, slide: &Slide, pos: &str) {
        let mut keys: BTreeMap<&'static str, New> = slide_values(slide).into_iter().map(|(k, v)| (k, New::Any(to_any(&v)))).collect();
        keys.insert(POS, New::Any(Any::from(pos)));
        keys.insert(DEL, New::Any(Any::Bool(false)));
        keys.insert(LAYOUT, New::Any(Any::from(i64::from(slide.layout.0))));
        keys.insert(NOTES, New::Text(&slide.notes));
        let roots = self.roots;
        self.put_entry(&roots.slides, slide.id.0, keys);
    }

    pub fn set_slide(&mut self, id: SlideId, key: &str, v: Value) {
        if let Some(m) = self.entry(Tomb::Slide(id)) {
            m.insert(&mut self.txn, key, to_any(&v));
        }
    }

    /// A new shape: every key, and always a `text` YText, holding the text body when there is one
    /// and empty otherwise; `body` says whether the shape has a text body (Ruling D26 as amended
    /// in L3 fix round 1), so no peer ever creates the YText concurrently with another. A group's
    /// children are not written: each needs its own `put_shape` with `parent`.
    pub fn put_shape(&mut self, slide: SlideId, parent: Option<ShapeId>, z: &str, shape: &Shape) {
        let empty = TextBody::default();
        let mut keys: BTreeMap<&'static str, New> = shape_values(shape).into_iter().map(|(k, v)| (k, New::Any(to_any(&v)))).collect();
        keys.insert(SLIDE, New::Any(Any::from(i64::from(slide.0))));
        keys.insert(PARENT, New::Any(Any::from(i64::from(parent.map_or(0, |p| p.0)))));
        keys.insert(Z, New::Any(Any::from(z)));
        keys.insert(DEL, New::Any(Any::Bool(false)));
        keys.insert(TEXT, New::Text(shape.text.as_ref().unwrap_or(&empty)));
        let roots = self.roots;
        self.put_entry(&roots.shapes, shape.id.0, keys);
    }

    /// `shapes` and their descendants, siblings keyed by `seed_keys`.
    fn put_tree(&mut self, slide: SlideId, parent: Option<ShapeId>, shapes: &[Shape], depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let z = order::seed_keys(shapes.len());
        if z.len() < shapes.len() {
            self.skipped += 1;
            return;
        }
        for (s, z) in shapes.iter().zip(&z) {
            self.put_shape(slide, parent, z, s);
            self.put_tree(slide, Some(s.id), s.children(), depth + 1);
        }
    }

    pub fn set_shape(&mut self, id: ShapeId, key: &str, v: Value) {
        if let Some(m) = self.entry(Tomb::Shape(id)) {
            m.insert(&mut self.txn, key, to_any(&v));
        }
    }

    /// Tombstone (`del` true, `delBy` the deleter) or bring back (`del` false, `delBy` removed).
    pub fn set_del(&mut self, target: Tomb, del: bool, by: &str) {
        if let Some(m) = self.entry(target) {
            m.insert(&mut self.txn, DEL, Any::Bool(del));
            if del {
                m.insert(&mut self.txn, DEL_BY, Any::from(by));
            } else {
                m.remove(&mut self.txn, DEL_BY);
            }
        }
    }

    /// Apply text ops to a body. Every entry has its YText from the start (`put_shape`,
    /// `put_slide`); one that does not (a peer's junk in the key) is skipped, never created here.
    pub fn text_ops(&mut self, target: TextTarget, ops: &[TextOp]) {
        match self.roots.text(&self.txn, &target) {
            Some(text) => apply_ops(&mut self.txn, &text, ops),
            None => self.skipped += 1,
        }
    }

    /// The value at a latest-wins key now, as JSON; None when absent or a nested type.
    #[must_use]
    pub fn read(&self, key: KeyRef) -> Option<Value> {
        let (root, id, k) = match &key {
            KeyRef::Slide(id, k) => (&self.roots.slides, id.0, k),
            KeyRef::Shape(id, k) => (&self.roots.shapes, id.0, k),
        };
        match Roots::entry(root, &self.txn, id)?.get(&self.txn, k)? {
            Out::Any(a) => Some(to_json(&a)),
            _ => None,
        }
    }

    fn entry(&mut self, target: Tomb) -> Option<MapRef> {
        let m = match target {
            Tomb::Slide(id) => Roots::entry(&self.roots.slides, &self.txn, id.0),
            Tomb::Shape(id) => Roots::entry(&self.roots.shapes, &self.txn, id.0),
        };
        if m.is_none() {
            self.skipped += 1;
        }
        m
    }
}

fn push_seen(sink: &Mutex<Vec<Seen>>, found: Vec<Seen>) {
    sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner).extend(found);
}

/// What one event under `"slides"` (`slides` true) or `"shapes"` changed.
fn seen_in(txn: &TransactionMut, e: &Event, slides: bool) -> Vec<Seen> {
    let path = e.path();
    let entry = |id: u32| if slides { Seen::Slide(SlideId(id)) } else { Seen::Shape(ShapeId(id)) };
    match path.front() {
        None => match e {
            Event::Map(m) => m.keys(txn).keys().filter_map(|k| parse_key(k)).map(entry).collect(),
            _ => Vec::new(),
        },
        Some(PathSegment::Key(k)) => {
            let Some(id) = parse_key(k) else { return Vec::new() };
            match path.get(1) {
                None => vec![entry(id)],
                Some(PathSegment::Key(part)) if slides && part.as_ref() == NOTES => vec![Seen::Notes(SlideId(id))],
                Some(PathSegment::Key(part)) if !slides && part.as_ref() == TEXT => vec![Seen::Text(ShapeId(id))],
                // Something nested a peer should not have made: the entry is re-read.
                Some(_) => vec![entry(id)],
            }
        }
        Some(PathSegment::Index(_)) => Vec::new(),
    }
}

/// One key as read: absent, read well, or present but unreadable (a peer's junk, logged).
#[derive(Clone, Debug, PartialEq)]
enum Read<T> {
    Absent,
    Good(T),
    Junk,
}

impl<T> Read<T> {
    /// The value, `absent` when the key is absent and `junk` (noting `key` in `bad`) when unreadable.
    fn or(self, key: &'static str, absent: T, junk: T, bad: &mut BTreeSet<&'static str>) -> T {
        match self {
            Read::Good(v) => v,
            Read::Absent => absent,
            Read::Junk => {
                log::warn!("deckcraft-live: a {key} value does not read; the base's is used");
                bad.insert(key);
                junk
            }
        }
    }
}

fn read_order<T: ReadTxn>(m: &MapRef, txn: &T, key: &str) -> Read<String> {
    match m.get(txn, key) {
        None => Read::Absent,
        Some(Out::Any(Any::String(s))) if order::is_key(&s) => Read::Good(s.to_string()),
        Some(_) => Read::Junk,
    }
}

fn read_bool<T: ReadTxn>(m: &MapRef, txn: &T, key: &str) -> Read<bool> {
    match m.get(txn, key) {
        None => Read::Absent,
        Some(Out::Any(Any::Bool(b))) => Read::Good(b),
        Some(_) => Read::Junk,
    }
}

/// `del`; a junk or removed value reads as deleted when the entry names a deleter (`delBy` is
/// written with every delete and removed by every restore), so a peer's junk or a removed key
/// cannot revive a tombstone (L3 review, L5 review M5), and as live otherwise.
fn read_del<T: ReadTxn>(m: &MapRef, txn: &T, by: bool, bad: &mut BTreeSet<&'static str>) -> bool {
    read_bool(m, txn, DEL).or(DEL, by, by, bad)
}

fn read_by<T: ReadTxn>(m: &MapRef, txn: &T) -> Read<Option<String>> {
    match m.get(txn, DEL_BY) {
        None => Read::Absent,
        Some(Out::Any(Any::String(s))) => Read::Good(Some(s.to_string())),
        Some(_) => Read::Junk,
    }
}

/// An id stored as a number.
fn read_id<T: ReadTxn>(m: &MapRef, txn: &T, key: &str) -> Read<u32> {
    let v = match m.get(txn, key) {
        None => return Read::Absent,
        Some(Out::Any(Any::Number(n))) => {
            n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && f.is_finite() && f.abs() < 9.0e15).map(|f| f as i64))
        }
        Some(_) => None,
    };
    v.and_then(|i| u32::try_from(i).ok()).map_or(Read::Junk, Read::Good)
}

/// The set latest-wins keys that read as their type. A key holding junk is left out and noted in
/// `bad` (logged), so the rebuild uses the base's value.
fn read_fields<T: ReadTxn>(
    m: &MapRef,
    txn: &T,
    keys: &[&'static str],
    bad: &mut BTreeSet<&'static str>,
    valid: impl Fn(&str, &Value) -> bool,
) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for k in keys {
        let v = match m.get(txn, k) {
            None => continue,
            Some(Out::Any(a)) => Some(to_json(&a)).filter(|v| valid(k, v)),
            Some(_) => None,
        };
        match v {
            Some(v) => {
                out.insert((*k).to_owned(), v);
            }
            None => {
                log::warn!("deckcraft-live: a {k} value does not read; the base's is used");
                bad.insert(k);
            }
        }
    }
    out
}

/// Every entry of `shapes` on `slide`, tombstones included, as (id, effective parent): the parent
/// is kept only when it is a group on the same slide and the chain up from it ends at the slide;
/// otherwise the shape reads at slide level.
pub(crate) fn tree_in(shapes: &BTreeMap<ShapeId, ShapeEntry>, slide: SlideId) -> Vec<(ShapeId, Option<ShapeId>)> {
    let on: BTreeMap<ShapeId, &ShapeEntry> = shapes.iter().filter(|(_, e)| e.slide == slide).map(|(id, e)| (*id, e)).collect();
    let parent_of =
        |id: ShapeId| -> Option<ShapeId> { on.get(&id).and_then(|e| e.parent).filter(|p| on.get(p).is_some_and(|pe| is_group_kind(&pe.fields))) };
    let mut out = Vec::new();
    for id in on.keys() {
        let mut p = parent_of(*id);
        let mut seen = HashSet::from([*id]);
        let mut cur = p;
        while let Some(c) = cur {
            if !seen.insert(c) || seen.len() > MAX_DEPTH {
                log::warn!("deckcraft-live: a shape's parent chain loops or is too deep; it reads at slide level");
                p = None;
                break;
            }
            cur = parent_of(c);
        }
        out.push((*id, p));
    }
    out
}

/// The length in document offsets of each unit (char or embed) of a text's flat form.
fn units<T: ReadTxn>(txn: &T, t: &TextRef, kind: OffsetKind) -> Vec<u32> {
    let mut out = Vec::new();
    for d in t.diff(txn, YChange::identity) {
        match d.insert {
            Out::Any(Any::String(s)) => {
                for c in s.chars() {
                    out.push(match kind {
                        OffsetKind::Bytes => u32::try_from(c.len_utf8()).unwrap_or(1),
                        OffsetKind::Utf16 => u32::try_from(c.len_utf16()).unwrap_or(1),
                    });
                }
            }
            _ => out.push(1),
        }
    }
    out
}
