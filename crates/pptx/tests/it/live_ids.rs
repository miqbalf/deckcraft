//! The same `.pptx` bytes import to the same ids, in one process and across threads, whatever the
//! hasher state. The live seed (L3) depends on this: two peers importing the base bytes must agree
//! on every slide, shape, layout, master and media id, and on `next_id`.
use std::sync::Arc;

use deckcraft_engine::Session;
use deckcraft_geom::Xfrm;
use deckcraft_model::style::PictureFill;
use deckcraft_model::table::Table;
use deckcraft_model::text::TextBody;
use deckcraft_model::{Presentation, Shape, ShapeId, ShapeKind, defaults};

fn shape(p: &mut Presentation, name: &str, x: f64, y: f64, w: f64, h: f64) -> Shape {
    Shape { id: ShapeId(p.alloc_id().expect("id")), name: name.into(), xfrm: Some(Xfrm::new(x, y, w, h)), ..Default::default() }
}

fn sample_bytes() -> Vec<u8> {
    let mut s = Session::new();
    deckcraft_engine::sample::open_sample(&mut s).expect("sample");
    let p = s.doc().expect("doc").doc.as_ref().clone();
    deckcraft_pptx::export(&p).expect("export")
}

/// Groups, a connector, a picture, a table and notes, built in code.
fn mixed_bytes() -> Vec<u8> {
    let mut p = defaults::new_presentation(None);
    let image = image::RgbaImage::from_fn(8, 4, |x, y| image::Rgba([(x * 30) as u8, (y * 60) as u8, 90, 255]));
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image).write_to(&mut png, image::ImageFormat::Png).expect("PNG");
    let media = p.add_media("pic.png", "image/png", png.into_inner()).expect("media");

    let layout = defaults::layout_of_kind(&p, deckcraft_model::LayoutType::Blank).unwrap_or_default();
    let mut s = defaults::new_slide(&mut p, layout).expect("slide");

    let mut pic = shape(&mut p, "Picture", 20.0, 10.0, 80.0, 40.0);
    pic.kind = ShapeKind::Picture { fill: PictureFill { media, ..Default::default() } };
    let inner = shape(&mut p, "Inner", 10.0, 60.0, 60.0, 30.0);
    let mut nested = shape(&mut p, "Nested", 10.0, 10.0, 200.0, 100.0);
    nested.kind = ShapeKind::Group { children: vec![inner], child: Xfrm::new(0.0, 0.0, 200.0, 100.0) };
    let mut group = shape(&mut p, "Group", 100.0, 150.0, 400.0, 180.0);
    group.kind = ShapeKind::Group { children: vec![pic, nested], child: Xfrm::new(0.0, 0.0, 400.0, 180.0) };

    let left = shape(&mut p, "Left", 40.0, 400.0, 80.0, 40.0);
    let right = shape(&mut p, "Right", 240.0, 400.0, 80.0, 40.0);
    let (l, r) = (left.id, right.id);
    let mut cx = shape(&mut p, "Connector", 120.0, 420.0, 120.0, 0.0);
    cx.kind = ShapeKind::Connector { start: Some((l, 3)), end: Some((r, 1)) };
    cx.geom = deckcraft_model::Geom::preset("straightConnector1");

    let mut table = Table::new(3, 3, 360.0, 28.0);
    if let Some(cell) = table.cell_mut(0, 0) {
        cell.text = TextBody::from_text("Cell");
    }
    let mut ts = shape(&mut p, "Table", 40.0, 320.0, 360.0, 84.0);
    ts.kind = ShapeKind::Table(table);

    s.shapes.extend([group, left, right, cx, ts]);
    s.notes = TextBody::from_text("Speaker notes");
    s.has_notes_slide = true;
    p.slides.push(Arc::new(s));
    deckcraft_pptx::export(&p).expect("export")
}

/// The first import of the mixed deck must still hold every feature it was built to exercise.
fn assert_mixed_features(p: &Presentation) {
    let all: Vec<&Shape> = p.slides.iter().flat_map(|s| s.shapes.iter()).collect();
    assert!(all.iter().any(|s| matches!(s.kind, ShapeKind::Group { .. }) && depth(s) >= 3), "nested group (depth 3 with its leaf) dropped");
    let glued = all.iter().any(|s| matches!(s.kind, ShapeKind::Connector { start: Some(_), end: Some(_), .. }));
    assert!(glued, "glued connector dropped");
    assert!(!p.media.is_empty(), "media dropped");
    fn find_pic<'a>(shapes: &'a [Shape], out: &mut Vec<&'a Shape>) {
        for s in shapes {
            if matches!(s.kind, ShapeKind::Picture { .. }) {
                out.push(s);
            }
            find_pic(s.children(), out);
        }
    }
    let mut pics = Vec::new();
    for s in &p.slides {
        find_pic(&s.shapes, &mut pics);
    }
    assert!(!pics.is_empty(), "picture dropped");
    for pic in pics {
        let ShapeKind::Picture { fill } = &pic.kind else { continue };
        assert!(p.media.iter().any(|m| m.id == fill.media), "picture media id not in media list");
    }
    assert!(all.iter().any(|s| matches!(s.kind, ShapeKind::Table(_))), "table dropped");
    assert!(p.slides.iter().any(|s| !s.notes_text().is_empty()), "notes dropped");
}

fn decks() -> Vec<(&'static str, Vec<u8>)> {
    vec![("sample", sample_bytes()), ("mixed", mixed_bytes())]
}

fn import(bytes: &[u8]) -> Presentation {
    deckcraft_pptx::import(bytes).expect("import")
}

/// Every id the engine can address, in document order, plus `next_id`.
fn ids(p: &Presentation) -> Vec<u32> {
    fn walk(shapes: &[Shape], out: &mut Vec<u32>) {
        for s in shapes {
            out.push(s.id.0);
            walk(s.children(), out);
        }
    }
    let mut out = vec![p.next_id];
    for s in &p.slides {
        out.push(s.id.0);
        walk(&s.shapes, &mut out);
    }
    for m in &p.media {
        out.push(m.id.0);
    }
    out.extend(master_layout_ids(p));
    out
}

/// Master ids and, after each, the ids of its layouts.
fn master_layout_ids(p: &Presentation) -> Vec<u32> {
    let mut out = Vec::new();
    for m in &p.masters {
        out.push(m.id.0);
        out.extend(m.layouts.iter().map(|l| l.id.0));
    }
    out
}

/// Slide ids, each followed by the ids of its shapes (depth first).
fn slide_shape_ids(p: &Presentation) -> Vec<Vec<u32>> {
    fn walk(shapes: &[Shape], out: &mut Vec<u32>) {
        for s in shapes {
            out.push(s.id.0);
            walk(s.children(), out);
        }
    }
    p.slides
        .iter()
        .map(|s| {
            let mut v = vec![s.id.0];
            walk(&s.shapes, &mut v);
            v
        })
        .collect()
}

fn digest(p: &Presentation) -> String {
    ids(p).iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

fn depth(shape: &Shape) -> usize {
    1 + shape.children().iter().map(depth).max().unwrap_or(0)
}

fn check_all_equal(name: &str, bytes: &[u8]) {
    let first = import(bytes);
    assert!(!first.slides.is_empty(), "{name}: no slides");
    if name == "mixed" {
        assert_mixed_features(&first);
    }
    let first_ids = ids(&first);
    for i in 0..20 {
        let again = import(bytes);
        assert_eq!(ids(&again), first_ids, "{name}: import {i} ids");
        assert!(again == first, "{name}: import {i} differs from the first");
    }
    let owned = bytes.to_vec();
    let from_thread = std::thread::spawn(move || import(&owned)).join().expect("thread");
    assert_eq!(ids(&from_thread), first_ids, "{name}: thread ids");
    assert!(from_thread == first, "{name}: thread import differs from the first");
}

#[test]
fn same_bytes_import_to_the_same_ids() {
    for (name, bytes) in decks() {
        check_all_equal(name, &bytes);
    }
}

const CHILD_ENV: &str = "DECKCRAFT_LIVE_IDS_CHILD";

/// Run by the parent test below in a separate process (own random hasher seed); a no-op otherwise.
#[test]
fn child_prints_digests() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    for (name, bytes) in decks() {
        println!("\nDIGEST {name} {}", digest(&import(&bytes)));
    }
}

#[test]
fn import_order_does_not_follow_hash_order() {
    let here: Vec<String> = decks().iter().map(|(n, b)| format!("DIGEST {n} {}", digest(&import(b)))).collect();
    let exe = std::env::current_exe().expect("exe");
    for run in 0..4 {
        let out = std::process::Command::new(&exe)
            .args(["live_ids::child_prints_digests", "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .output()
            .expect("spawn child");
        assert!(out.status.success(), "child {run} failed");
        let text = String::from_utf8_lossy(&out.stdout);
        let got: Vec<String> = text.lines().filter(|l| l.starts_with("DIGEST ")).map(str::to_owned).collect();
        assert_eq!(got, here, "child process {run} imported different ids");
    }
}

#[test]
fn sample_deck_ids_are_the_recorded_ones() {
    let p = import(&sample_bytes());
    assert_eq!(slide_shape_ids(&p), GOLDEN_SAMPLE.iter().map(|s| s.to_vec()).collect::<Vec<_>>(), "slide and shape ids of the sample deck");
    assert!(!master_layout_ids(&p).is_empty(), "no masters");
}

const GOLDEN_SAMPLE: &[&[u32]] = &[
    &[291, 258, 259, 260],
    &[292, 262, 263],
    &[293, 265, 266],
    &[294, 268, 269, 270, 271, 272, 273, 274, 275, 276],
    &[295, 278, 279],
    &[296, 281, 282],
    &[297, 284, 285, 286],
    &[298, 288, 289, 290],
];
