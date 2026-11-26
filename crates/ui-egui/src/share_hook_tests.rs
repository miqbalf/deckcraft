//! SH2 regression guard: on the live release line, DeckCraft has a Share button and `file.share`,
//! which is Nextcloud sharing (the host-share hook), never an export. Uses only the app's public
//! command and UI surface, so it also runs against a build that lost the hook (where it fails).

use deckcraft_engine::Session;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use serde_json::json;

use crate::{Services, SlideApp};

const NO_NEXTCLOUD: &str = "No Nextcloud connected";

fn toast(app: &SlideApp) -> Option<&str> {
    app.toast.as_ref().map(|t| t.text.as_str())
}

#[test]
fn file_share_is_a_command_that_never_exports() {
    let mut app = SlideApp::new(Session::with_new(), Services::default());
    let r = app.run("file.share", json!({}));
    assert!(r.is_ok(), "file.share is a command: {r:?}");
    assert_eq!(toast(&app), Some(NO_NEXTCLOUD), "no host and no desktop client: Share says so");
    assert!(app.dialog.is_none(), "Share opens no dialog");
    assert!(app.backstage.is_none(), "Share opens no Export page");
}

#[test]
fn the_title_bar_has_a_share_button_that_runs_file_share() {
    let app = SlideApp::new(Session::with_new(), Services::default());
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).with_step_dt(1.0 / 60.0).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    h.get_by_role_and_label(egui::accesskit::Role::Button, "Share").click();
    h.run_steps(3);
    assert_eq!(toast(h.state()), Some(NO_NEXTCLOUD), "the title bar's Share runs file.share");
    assert!(h.state().backstage.is_none(), "the title bar's Share is not an export");
}
