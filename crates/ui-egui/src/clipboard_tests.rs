//! Copy, cut and paste by keyboard in a browser (DP1). eframe's web runner hands the app both the
//! key press (Ctrl+C, Ctrl+X, Ctrl+V) and the DOM clipboard event (`Copy`, `Cut`, `Paste`) in one
//! batch; egui-winit on the desktop swallows the key and sends only the clipboard event. Either
//! way each shortcut must act once, the same as the menu command.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

type H = egui_kittest::Harness<'static, SlideApp>;

fn app() -> H {
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        SlideApp::new(Session::with_new(), Services::default()),
    );
    h.run_steps(2);
    h
}

/// The batch a browser gives for Ctrl+`key`: the key press, the DOM clipboard event `ev`, and the
/// release. Returns the text the frame put on the system clipboard, if it put any.
pub(crate) fn web_shortcut(h: &mut H, key: egui::Key, ev: Option<egui::Event>) -> Option<String> {
    let cmd = egui::Modifiers { command: true, ctrl: !cfg!(target_os = "macos"), ..Default::default() };
    let k = |pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: cmd };
    h.input_mut().events.push(k(true));
    h.input_mut().events.extend(ev);
    h.input_mut().events.push(k(false));
    h.run_steps(1);
    let copied = h.output().platform_output.commands.iter().rev().find_map(|c| match c {
        egui::output::OutputCommand::CopyText(t) => Some(t.clone()),
        _ => None,
    });
    h.run_steps(1);
    copied
}

fn shapes(h: &H) -> usize {
    h.state().session.active().unwrap().current_slide().unwrap().shapes.len()
}

fn slides(h: &H) -> usize {
    h.state().session.active().unwrap().doc.slides.len()
}

fn rect(h: &mut H) -> u64 {
    let v = h.state_mut().run("shape.insert", json!({"preset": "rect", "rect": [80.0, 80.0, 120.0, 80.0]})).unwrap();
    h.state_mut().run("text.exit", json!({})).ok();
    let id = v["id"].as_u64().unwrap();
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    h.run_steps(1);
    id
}

#[test]
fn web_ctrl_c_ctrl_v_pastes_a_shape_once() {
    let mut h = app();
    rect(&mut h);
    let before = shapes(&h);
    let clip = web_shortcut(&mut h, egui::Key::C, Some(egui::Event::Copy)).expect("the copy reached the system clipboard");
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(shapes(&h), before + 1, "one copy pasted");
    h.state_mut().run("edit.undo", json!({})).unwrap();
    assert_eq!(shapes(&h), before, "and one Undo takes it back");
}

#[test]
fn web_ctrl_x_cuts_the_shape_not_the_slide() {
    let mut h = app();
    h.state_mut().run("slide.new", json!({})).unwrap();
    let id = rect(&mut h);
    let (n_slides, before) = (slides(&h), shapes(&h));
    let clip = web_shortcut(&mut h, egui::Key::X, Some(egui::Event::Cut)).expect("the cut reached the system clipboard");
    assert_eq!(slides(&h), n_slides, "no slide cut");
    assert_eq!(shapes(&h), before - 1, "the shape is cut");
    let st = h.state().session.active().unwrap();
    assert!(st.current_slide().unwrap().shape(deckcraft_model::ShapeId(id as u32)).is_none());
    assert_eq!(clip, OBJECTS_MARKER, "a textless shape leaves the objects marker");
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(shapes(&h), before, "the shape is pasted back once");
    assert_eq!(slides(&h), n_slides);
}

#[test]
fn menu_copy_then_web_ctrl_v_pastes_once() {
    let mut h = app();
    rect(&mut h);
    let before = shapes(&h);
    h.state_mut().run("edit.copy", json!({})).unwrap();
    h.run_steps(2);
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(OBJECTS_MARKER.into())));
    assert_eq!(shapes(&h), before + 1, "one copy pasted");
}

/// A press in the thumbnail pane selects the slide; Ctrl+C, Ctrl+V and Ctrl+X then act on it.
#[test]
fn web_shortcuts_copy_cut_and_paste_a_selected_slide() {
    let mut h = app();
    for _ in 0..2 {
        h.state_mut().run("slide.new", json!({})).unwrap();
    }
    h.state_mut().run("slide.go", json!({"index": 0})).unwrap();
    h.run_steps(3);
    let w = h.state().ui.thumbs_width;
    let pos = egui::pos2(w / 2.0, 300.0);
    h.event(egui::Event::PointerMoved(pos));
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE });
    }
    h.run_steps(2);
    assert!(h.state().slides_focused, "the thumbnail pane has the keyboard");
    assert!(!h.state().session.active().unwrap().selection.slides.is_empty(), "a slide is selected");
    let n = slides(&h);
    let clip = web_shortcut(&mut h, egui::Key::C, Some(egui::Event::Copy)).expect("the copy reached the system clipboard");
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(slides(&h), n + 1, "one slide pasted");
    let clip = web_shortcut(&mut h, egui::Key::X, Some(egui::Event::Cut)).expect("the cut reached the system clipboard");
    assert_eq!(slides(&h), n, "the selected slide is cut");
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(slides(&h), n + 1, "and pasted back once");
}

/// Text selected inside a shape: Ctrl+C then Ctrl+V at the end pastes the words once; Ctrl+X
/// takes them out.
#[test]
fn web_shortcuts_copy_cut_and_paste_text_in_a_shape_once() {
    let mut h = app();
    let v = h.state_mut().run("shape.insert", json!({"preset": "rect", "rect": [80.0, 80.0, 200.0, 80.0], "text": "Ship"})).unwrap();
    let id = v["id"].as_u64().unwrap();
    let text = |h: &H| h.state().session.active().unwrap().shape(deckcraft_model::ShapeId(id as u32)).unwrap().text.as_ref().unwrap().text();
    h.state_mut().run("text.edit", json!({"id": id})).unwrap();
    h.state_mut().run("text.selectAll", json!({})).unwrap();
    h.run_steps(1);
    let clip = web_shortcut(&mut h, egui::Key::C, Some(egui::Event::Copy)).expect("the copy reached the system clipboard");
    assert_eq!(clip, "Ship");
    h.state_mut().run("text.move", json!({"to": "end"})).unwrap();
    web_shortcut(&mut h, egui::Key::V, Some(egui::Event::Paste(clip)));
    assert_eq!(text(&h), "ShipShip", "pasted once");
    h.state_mut().run("text.move", json!({"to": "start", "extend": true})).unwrap();
    web_shortcut(&mut h, egui::Key::X, Some(egui::Event::Cut));
    assert_eq!(text(&h), "", "cut once");
}

/// A platform that sends only the key (no clipboard event) still copies and pastes.
#[test]
fn a_bare_shortcut_key_still_copies_and_pastes() {
    let mut h = app();
    rect(&mut h);
    let before = shapes(&h);
    web_shortcut(&mut h, egui::Key::C, None);
    web_shortcut(&mut h, egui::Key::V, None);
    assert_eq!(shapes(&h), before + 1, "one copy pasted");
}
