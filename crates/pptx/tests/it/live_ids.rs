//! The same `.pptx` bytes import to the same ids, in one process and across threads, whatever the
//! hasher state. The live seed (L3) depends on this: two peers importing the base bytes must agree
//! on every slide, shape, layout, master and media id, and on `next_id`.
use std::collections::HashMap;
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
    out
}

fn check_all_equal(name: &str, bytes: &[u8]) {
    let first = import(bytes);
    assert!(!first.slides.is_empty(), "{name}: no slides");
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

#[test]
fn import_order_does_not_follow_hash_order() {
    // Touch the hasher state first: a per-process random hasher would show in the result.
    let mut noise: HashMap<String, u32> = HashMap::new();
    for i in 0..500u32 {
        noise.insert(format!("k{i}-{}", i.wrapping_mul(2_654_435_761)), i);
    }
    assert!(!noise.is_empty());
    for (name, bytes) in decks() {
        let before = import(&bytes);
        for i in 0..100u32 {
            noise.insert(format!("more{i}"), i);
        }
        let after = import(&bytes);
        assert_eq!(ids(&after), ids(&before), "{name}: ids");
        assert!(after == before, "{name}: differs after hasher noise");
        check_all_equal(name, &bytes);
    }
}
