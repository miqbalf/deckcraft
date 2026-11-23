//! A live peer's id block (H4): once it is used up, commands that need new ids refuse and leave
//! the deck as it was (#407).

use std::sync::Arc;

use deckcraft_engine::Session;
use deckcraft_model::{IdBlock, Presentation};
use serde_json::json;

/// Give the active deck a block and use every id in it.
fn exhaust(s: &mut Session) {
    let block = IdBlock::new(5).expect("block");
    let st = s.doc_mut().expect("doc");
    Arc::make_mut(&mut st.doc).id_block = Some(block.clone());
    let mut scratch = Presentation { id_block: Some(block), ..Default::default() };
    while scratch.alloc_id().is_some() {}
}

#[test]
fn exhausted_block_refuses_new_shapes_and_keeps_the_deck() {
    let mut s = Session::with_new();
    let id = s.execute("shape.insert", &json!({"preset": "rect", "rect": [10.0, 10.0, 100.0, 100.0]})).expect("insert")["id"].as_u64().expect("id");
    s.execute("edit.copy", &json!({})).expect("copy");
    exhaust(&mut s);
    for (cmd, params) in [
        ("shape.insert", json!({"preset": "ellipse", "rect": [200.0, 10.0, 50.0, 50.0]})),
        ("slide.new", json!({})),
        ("edit.duplicate", json!({"ids": [id]})),
        ("edit.paste", json!({})),
    ] {
        let before = s.doc().expect("doc").doc.clone();
        assert!(s.execute(cmd, &params).is_err(), "{cmd} refuses with a used-up block");
        assert!(Arc::ptr_eq(&before, &s.doc().expect("doc").doc), "{cmd} keeps the deck");
    }
}
