//! Text bodies as one `YText` each, and minimal edits between two bodies.
//!
//! # Layout
//!
//! * The paragraphs, one after another, each ending with a paragraph mark `'\n'` (the last one
//!   too, so a body with one empty paragraph is `"\n"`, Ruling D26). A line break is `'\u{b}'`. A
//!   field or an equation is one embed, a map `{"field": …, "text": …}` or `{"omml": …, "text": …}`.
//! * A paragraph's own fields are formatting attributes on its mark: [`P_LEVEL`], [`P_PROPS`],
//!   [`P_DEF`] and [`P_END`], each left out when it is the default.
//! * A run's [`RunProps`] are formatting attributes on its characters and embeds, one per
//!   top-level field: `"r."` and the field's serde name (`"r.bold"`, `"r.fontEa"`). A field that
//!   is not set is no attribute.
//!
//! [`BodyProps`](deckcraft_model::text::BodyProps) and the list style are not in the `YText`:
//! they sit in the shape's `"body"` key.
//!
//! # Positions
//!
//! A [`TextOp`] counts positions in chars of the flat form: one per character, per paragraph
//! mark and per embed. yrs counts UTF-8 bytes or UTF-16 units (craft-collab builds its documents
//! with bytes) with an embed as one; [`apply_ops`] converts with the document's own offset kind,
//! the same conversion craft-collab's `ydoc.rs` does for WordCraft.
//!
//! # Determinism
//!
//! [`write_body`] is a pure function of the body: it inserts the plain content first and then
//! sets one attribute key per format call, in key order. yrs keeps several attributes of one call
//! in a hash map and writes them in hash order, so a multi-key call would give two peers seeding
//! the same file different item ids.

use std::collections::BTreeMap;
use std::sync::Arc;

use craft_collab::replica::{to_any, to_json};
use craft_collab::yrs::types::Attrs;
use craft_collab::yrs::types::text::YChange;
use craft_collab::yrs::{Any, OffsetKind, Out, ReadTxn, Text, TextRef, TransactionMut};
use deckcraft_model::text::{ParaProps, Paragraph, Run, RunKind, RunProps, TextBody};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The paragraph mark.
pub const MARK: char = '\n';
/// A line break inside a paragraph.
pub const BREAK: char = '\u{b}';
/// Mark attribute: the outline level, when it is not 0.
pub const P_LEVEL: &str = "p.level";
/// Mark attribute: the paragraph's [`ParaProps`], when not default.
pub const P_PROPS: &str = "p.props";
/// Mark attribute: the paragraph's run defaults (`def_props`), when not default.
pub const P_DEF: &str = "p.def";
/// Mark attribute: the paragraph end mark's run properties (`end_props`), when not default.
pub const P_END: &str = "p.end";
/// Prefix of run attributes: `"r."` and a [`RunProps`] field's serde name.
pub const RUN: &str = "r.";
/// Embed key naming a field.
pub const FIELD: &str = "field";
/// Embed key holding an equation's OMML.
pub const OMML: &str = "omml";
/// Embed key holding the shown text of a field or an equation.
pub const EMBED_TEXT: &str = "text";

/// One edit to a body's `YText`. Positions and lengths count chars of the flat form.
#[derive(Clone, Debug, PartialEq)]
pub enum TextOp {
    Insert {
        at: u32,
        chunks: Vec<Chunk>,
    },
    Delete {
        at: u32,
        len: u32,
    },
    /// Set (`Some`) or clear (`None`) attributes over a range; others are left alone.
    Format {
        at: u32,
        len: u32,
        attrs: Vec<(String, Option<serde_json::Value>)>,
    },
}

/// Inserted content carrying exactly `attrs`.
#[derive(Clone, Debug, PartialEq)]
pub enum Chunk {
    Text { text: String, attrs: Vec<(String, serde_json::Value)> },
    Embed { value: serde_json::Value, attrs: Vec<(String, serde_json::Value)> },
}

type Attributes = BTreeMap<String, Value>;

/// One char of the flat form.
#[derive(Clone, Debug, PartialEq)]
enum Unit {
    Char(char),
    Embed(Value),
    Mark,
}

#[derive(Clone, Debug, PartialEq)]
struct Item {
    unit: Unit,
    attrs: Attributes,
}

/// `v`'s serde JSON object, nulls left out.
fn object<T: Serialize>(v: &T) -> Attributes {
    match serde_json::to_value(v) {
        Ok(Value::Object(m)) => m.into_iter().filter(|(_, v)| !v.is_null()).collect(),
        Ok(_) => Attributes::new(),
        Err(e) => {
            log::warn!("live: text attributes did not serialise: {e}");
            Attributes::new()
        }
    }
}

fn parse<T: DeserializeOwned>(v: &Value, what: &str) -> Option<T> {
    match serde_json::from_value(v.clone()) {
        Ok(t) => Some(t),
        Err(e) => {
            log::warn!("live: shared text attribute {what} did not read back: {e}");
            None
        }
    }
}

fn run_attrs(props: &RunProps) -> Attributes {
    object(props).into_iter().map(|(k, v)| (format!("{RUN}{k}"), v)).collect()
}

fn mark_attrs(p: &Paragraph) -> Attributes {
    let mut out = Attributes::new();
    if p.level != 0 {
        out.insert(P_LEVEL.to_owned(), Value::from(p.level));
    }
    if p.props != ParaProps::default() {
        out.insert(P_PROPS.to_owned(), Value::Object(object(&p.props).into_iter().collect()));
    }
    if p.def_props != RunProps::default() {
        out.insert(P_DEF.to_owned(), Value::Object(object(&p.def_props).into_iter().collect()));
    }
    if p.end_props != RunProps::default() {
        out.insert(P_END.to_owned(), Value::Object(object(&p.end_props).into_iter().collect()));
    }
    out
}

fn embed(kind: &str, value: &str, text: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert(kind.to_owned(), Value::String(value.to_owned()));
    m.insert(EMBED_TEXT.to_owned(), Value::String(text.to_owned()));
    Value::Object(m)
}

/// The paragraphs as flat units.
fn flatten(paragraphs: &[Paragraph]) -> Vec<Item> {
    let mut out = Vec::new();
    for p in paragraphs {
        for r in &p.runs {
            let attrs = run_attrs(&r.props);
            match &r.kind {
                RunKind::Text => out.extend(r.text.chars().map(|c| Item { unit: Unit::Char(c), attrs: attrs.clone() })),
                RunKind::Break => out.push(Item { unit: Unit::Char(BREAK), attrs }),
                RunKind::Field { field } => out.push(Item { unit: Unit::Embed(embed(FIELD, field, &r.text)), attrs }),
                RunKind::Math { omml } => out.push(Item { unit: Unit::Embed(embed(OMML, omml, &r.text)), attrs }),
            }
        }
        out.push(Item { unit: Unit::Mark, attrs: mark_attrs(p) });
    }
    out
}

fn yattrs<'a>(pairs: impl IntoIterator<Item = (&'a String, &'a Value)>) -> Attrs {
    pairs.into_iter().map(|(k, v)| (Arc::from(k.as_str()), to_any(v))).collect()
}

/// Write `body`'s paragraphs at the end of `text` (meant to be empty). Deterministic: the same
/// body gives the same items on every replica with the same client id.
pub fn write_body(txn: &mut TransactionMut, text: &TextRef, body: &TextBody) {
    let items = flatten(&body.paragraphs);
    let kind = txn.doc().offset_kind();
    let base = text.len(txn);
    // Plain content first: nothing in it is formatted yet, so no attribute leaks into it.
    let mut at = base;
    let mut pending = String::new();
    for item in &items {
        match &item.unit {
            Unit::Char(c) => pending.push(*c),
            Unit::Mark => pending.push(MARK),
            Unit::Embed(v) => {
                at = flush(txn, text, at, &mut pending, kind);
                text.insert_embed(txn, at, to_any(v));
                at += 1;
            }
        }
    }
    flush(txn, text, at, &mut pending, kind);
    // Then one key at a time, in key order, over each stretch holding the same value.
    let units: Vec<u32> = items.iter().map(|i| unit_len(&i.unit, kind)).collect();
    let keys: std::collections::BTreeSet<&String> = items.iter().flat_map(|i| i.attrs.keys()).collect();
    for key in keys {
        let mut i = 0usize;
        let mut offset = base;
        while let Some(item) = items.get(i) {
            let Some(v) = item.attrs.get(key) else {
                offset += units.get(i).copied().unwrap_or(0);
                i += 1;
                continue;
            };
            let start = offset;
            while items.get(i).is_some_and(|it| it.attrs.get(key) == Some(v)) {
                offset += units.get(i).copied().unwrap_or(0);
                i += 1;
            }
            text.format(txn, start, offset - start, yattrs([(key, v)]));
        }
    }
}

fn flush(txn: &mut TransactionMut, text: &TextRef, at: u32, pending: &mut String, kind: OffsetKind) -> u32 {
    if pending.is_empty() {
        return at;
    }
    text.insert(txn, at, pending);
    let len: u32 = pending.chars().map(|c| char_len(c, kind)).sum();
    pending.clear();
    at + len
}

fn char_len(c: char, kind: OffsetKind) -> u32 {
    let n = match kind {
        OffsetKind::Bytes => c.len_utf8(),
        OffsetKind::Utf16 => c.len_utf16(),
    };
    n as u32
}

fn unit_len(u: &Unit, kind: OffsetKind) -> u32 {
    match u {
        Unit::Char(c) => char_len(*c, kind),
        Unit::Mark => char_len(MARK, kind),
        Unit::Embed(_) => 1,
    }
}

fn chunk_attrs(a: Option<Box<Attrs>>) -> Attributes {
    a.map(|a| a.iter().filter(|(_, v)| !matches!(v, Any::Null)).map(|(k, v)| (k.to_string(), to_json(v))).collect()).unwrap_or_default()
}

/// Run properties from run attributes, one field at a time: a value that does not read is left
/// out (logged), the others still read.
fn read_run_props(attrs: &Attributes) -> RunProps {
    let mut fields = serde_json::Map::new();
    for (key, value) in attrs {
        let Some(field) = key.strip_prefix(RUN) else { continue };
        let mut one = serde_json::Map::new();
        one.insert(field.to_owned(), value.clone());
        if parse::<RunProps>(&Value::Object(one), key).is_some() {
            fields.insert(field.to_owned(), value.clone());
        }
    }
    parse(&Value::Object(fields), "run").unwrap_or_default()
}

fn read_mark(attrs: &Attributes, p: &mut Paragraph) {
    p.level = attrs.get(P_LEVEL).and_then(|v| parse::<u8>(v, P_LEVEL)).unwrap_or(0);
    p.props = attrs.get(P_PROPS).and_then(|v| parse(v, P_PROPS)).unwrap_or_default();
    p.def_props = attrs.get(P_DEF).and_then(|v| parse(v, P_DEF)).unwrap_or_default();
    p.end_props = attrs.get(P_END).and_then(|v| parse(v, P_END)).unwrap_or_default();
}

fn push_text(p: &mut Paragraph, c: char, props: &RunProps) {
    if let Some(last) = p.runs.last_mut()
        && last.kind == RunKind::Text
        && last.props == *props
    {
        last.text.push(c);
        return;
    }
    p.runs.push(Run::with(c.to_string(), props.clone()));
}

fn read_embed(m: &std::collections::HashMap<String, Any>, props: RunProps) -> Option<Run> {
    let text = match m.get(EMBED_TEXT) {
        Some(Any::String(s)) => s.to_string(),
        _ => String::new(),
    };
    let kind = match (m.get(FIELD), m.get(OMML)) {
        (Some(Any::String(f)), _) => RunKind::Field { field: f.to_string() },
        (_, Some(Any::String(o))) => RunKind::Math { omml: o.to_string() },
        _ => {
            log::warn!("live: an embed in a text body is neither a field nor an equation; left out");
            return None;
        }
    };
    Some(Run { text, props, kind })
}

/// The paragraphs and runs `text` holds. Content after the last paragraph mark (only a peer
/// that wrote past it could put any there) reads as one more paragraph with default properties.
pub fn read_body(txn: &impl ReadTxn, text: &TextRef) -> Vec<Paragraph> {
    let mut out = Vec::new();
    let mut cur = Paragraph::default();
    let mut open = false;
    for d in text.diff(txn, YChange::identity) {
        let attrs = chunk_attrs(d.attributes);
        match d.insert {
            Out::Any(Any::String(s)) => {
                let mut props: Option<RunProps> = None;
                for c in s.chars() {
                    if c == MARK {
                        read_mark(&attrs, &mut cur);
                        out.push(std::mem::take(&mut cur));
                        open = false;
                        continue;
                    }
                    let props = props.get_or_insert_with(|| read_run_props(&attrs));
                    open = true;
                    if c == BREAK {
                        cur.runs.push(Run { text: String::new(), props: props.clone(), kind: RunKind::Break });
                    } else {
                        push_text(&mut cur, c, props);
                    }
                }
            }
            Out::Any(Any::Map(m)) => {
                open = true;
                if let Some(run) = read_embed(&m, read_run_props(&attrs)) {
                    cur.runs.push(run);
                }
            }
            _ => {
                open = true;
                log::warn!("live: a text body holds an embed that is not a map; left out");
            }
        }
    }
    if open {
        out.push(cur);
    }
    out
}

/// The changes from `have` to `want`: `want`, plus a clear for each key only `have` sets.
fn patch(have: &Attributes, want: &Attributes) -> Vec<(String, Option<Value>)> {
    let mut out: Vec<(String, Option<Value>)> =
        want.iter().filter(|(k, v)| have.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
    out.extend(have.keys().filter(|k| !want.contains_key(*k)).map(|k| (k.clone(), None)));
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn chunks_of(items: &[Item]) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    for item in items {
        let attrs: Vec<(String, Value)> = item.attrs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let c = match &item.unit {
            Unit::Char(c) => *c,
            Unit::Mark => MARK,
            Unit::Embed(v) => {
                out.push(Chunk::Embed { value: v.clone(), attrs });
                continue;
            }
        };
        if let Some(Chunk::Text { text, attrs: last }) = out.last_mut()
            && *last == attrs
        {
            text.push(c);
            continue;
        }
        out.push(Chunk::Text { text: c.to_string(), attrs });
    }
    out
}

fn pos(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A minimal edit from `before` to `after`: the content outside their common prefix and suffix
/// is deleted and inserted, then every unit kept whose attributes differ is formatted. The final
/// paragraph mark is always kept when both bodies have one, so the edit never asks to delete it.
pub fn edit_ops(before: &[Paragraph], after: &[Paragraph]) -> Vec<TextOp> {
    let a = flatten(before);
    let b = flatten(after);
    // Both end with a mark when both have paragraphs: keep that mark out of the diff.
    let tail = usize::from(!a.is_empty() && !b.is_empty());
    let (ai, bi) = (a.len() - tail, b.len() - tail);
    let (ac, bc) = (a.get(..ai).unwrap_or_default(), b.get(..bi).unwrap_or_default());
    let prefix = ac.iter().zip(bc).take_while(|(x, y)| x.unit == y.unit).count();
    let room = ai.min(bi) - prefix;
    let suffix = ac.iter().rev().zip(bc.iter().rev()).take(room).take_while(|(x, y)| x.unit == y.unit).count();
    let mut ops = Vec::new();
    let del = ai - prefix - suffix;
    if del > 0 {
        ops.push(TextOp::Delete { at: pos(prefix), len: pos(del) });
    }
    let ins = bc.get(prefix..bi - suffix).unwrap_or_default();
    if !ins.is_empty() {
        ops.push(TextOp::Insert { at: pos(prefix), chunks: chunks_of(ins) });
    }
    // Kept units, in `after` positions: the prefix, the suffix and the final mark.
    let kept = (0..prefix).map(|i| (i, i)).chain((0..suffix + tail).map(|k| (ai - suffix + k, bi - suffix + k)));
    let mut formats: Vec<(usize, usize, Vec<(String, Option<Value>)>)> = Vec::new();
    for (i, j) in kept {
        let (Some(x), Some(y)) = (a.get(i), b.get(j)) else { continue };
        if x.attrs == y.attrs {
            continue;
        }
        let p = patch(&x.attrs, &y.attrs);
        match formats.last_mut() {
            Some((at, len, q)) if *at + *len == j && *q == p => *len += 1,
            _ => formats.push((j, 1, p)),
        }
    }
    ops.extend(formats.into_iter().map(|(at, len, attrs)| TextOp::Format { at: pos(at), len: pos(len), attrs }));
    ops
}

/// Where each flat unit of `text` is, in the document's offsets.
struct Layout {
    /// `starts[i]` is the offset of unit `i`; one more entry holds the end.
    starts: Vec<u32>,
    /// The flat index of the last paragraph mark.
    last_mark: Option<usize>,
}

impl Layout {
    fn of(txn: &TransactionMut, text: &TextRef) -> Layout {
        let kind = txn.doc().offset_kind();
        let mut starts = vec![0u32];
        let mut last_mark = None;
        let mut at = 0u32;
        for d in text.diff(txn, YChange::identity) {
            if let Out::Any(Any::String(s)) = &d.insert {
                for c in s.chars() {
                    if c == MARK {
                        last_mark = Some(starts.len() - 1);
                    }
                    at += char_len(c, kind);
                    starts.push(at);
                }
            } else {
                at += 1;
                starts.push(at);
            }
        }
        Layout { starts, last_mark }
    }

    fn len(&self) -> usize {
        self.starts.len() - 1
    }

    fn offset(&self, i: usize) -> u32 {
        self.starts.get(i.min(self.len())).copied().unwrap_or(0)
    }

    /// The document range of units `[from, to)`, as (offset, length).
    fn span(&self, from: usize, to: usize) -> (u32, u32) {
        let (s, e) = (self.offset(from), self.offset(to));
        (s, e.saturating_sub(s))
    }
}

/// Apply `ops` in order. Positions past the end are clipped to it. A delete never removes the
/// last paragraph mark: an op that would is clipped around it.
pub fn apply_ops(txn: &mut TransactionMut, text: &TextRef, ops: &[TextOp]) {
    for op in ops {
        let lay = Layout::of(txn, text);
        let n = lay.len();
        match op {
            TextOp::Insert { at, chunks } => {
                let mut at = lay.offset(*at as usize);
                let kind = txn.doc().offset_kind();
                for chunk in chunks {
                    match chunk {
                        Chunk::Text { text: s, attrs } => {
                            if s.is_empty() {
                                continue;
                            }
                            text.insert_with_attributes(txn, at, s, yattrs(attrs.iter().map(|(k, v)| (k, v))));
                            at += s.chars().map(|c| char_len(c, kind)).sum::<u32>();
                        }
                        Chunk::Embed { value, attrs } => {
                            text.insert_embed_with_attributes(txn, at, to_any(value), yattrs(attrs.iter().map(|(k, v)| (k, v))));
                            at += 1;
                        }
                    }
                }
            }
            TextOp::Delete { at, len } => {
                let from = (*at as usize).min(n);
                let to = from.saturating_add(*len as usize).min(n);
                let mut ranges = vec![(from, to)];
                if let Some(m) = lay.last_mark
                    && from <= m
                    && m < to
                {
                    // Later range first, so the earlier one's offsets still hold.
                    ranges = vec![(m + 1, to), (from, m)];
                }
                for (f, t) in ranges {
                    let (o, l) = lay.span(f, t);
                    if l > 0 {
                        text.remove_range(txn, o, l);
                    }
                }
            }
            TextOp::Format { at, len, attrs } => {
                let from = (*at as usize).min(n);
                let to = from.saturating_add(*len as usize).min(n);
                let (o, l) = lay.span(from, to);
                let attrs: Attrs = attrs.iter().map(|(k, v)| (Arc::from(k.as_str()), v.as_ref().map_or(Any::Null, to_any))).collect();
                if l > 0 && !attrs.is_empty() {
                    text.format(txn, o, l, attrs);
                }
            }
        }
    }
}
