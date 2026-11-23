//! Share (`file.share`, `app::share`): Nextcloud's sharing for the open presentation, or the
//! "No Nextcloud connected" message, from every entry point; never an export.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;
use std::rc::Rc;

use craft_share::{NO_NEXTCLOUD, ShareRoute};
use egui_kittest::kittest::Queryable;
use serde_json::json;

use crate::{Services, Session, SlideApp, backstage, control, menus, qat};

/// Fake values only: a Nextcloud server and a synced folder that do not exist.
const SERVER: &str = "https://cloud.example.test";
const SYNCED: &str = "/home/someone/Nextcloud/";

/// Everything Share must never do: export, save, download, pick a file or write one.
#[derive(Clone, Default)]
struct Spies {
    exported: Rc<Cell<usize>>,
    host_shares: Rc<Cell<usize>>,
}

/// Services whose file hooks all count as an export; `desktop` adds the synced-folder lookup,
/// `host` a framing page that shares (`share: true` when `page_shares`).
fn services(spies: &Spies, desktop: bool, host: bool, page_shares: bool) -> Services {
    let mut s = Services::default();
    let e = spies.exported.clone();
    s.download = Some(Box::new(move |_: &str, _: &str, _: &[u8]| {
        e.set(e.get() + 1);
        Ok(())
    }));
    let e = spies.exported.clone();
    s.pick_save = Some(Box::new(move |_: &str| {
        e.set(e.get() + 1);
        None
    }));
    let e = spies.exported.clone();
    s.write = Some(Box::new(move |_: &str, _: &[u8]| {
        e.set(e.get() + 1);
        Ok(())
    }));
    if desktop {
        s.share_route = Some(Box::new(|path: &std::path::Path| {
            let path = path.to_string_lossy();
            match path.strip_prefix(SYNCED) {
                Some(rest) => ShareRoute::NextcloudDesktop { server: SERVER.into(), user: "someone".into(), remote_path: format!("/{rest}") },
                None => ShareRoute::None,
            }
        }));
    }
    if host {
        let e = spies.exported.clone();
        s.host_save = Some(Box::new(move |_: &str, _: &[u8], _: bool, _: Option<&str>| {
            e.set(e.get() + 1);
            Ok(())
        }));
        let asked = spies.host_shares.clone();
        s.host_share = Some(Box::new(move || {
            asked.set(asked.get() + 1);
            Ok(())
        }));
        if page_shares {
            s.host_shares.set();
        }
    }
    s
}

fn with_path(app: &mut SlideApp, path: &str) {
    app.session.active_mut().unwrap().path = Some(path.into());
}

fn said_no_nextcloud(app: &SlideApp) -> bool {
    app.toast.as_ref().is_some_and(|t| t.text == NO_NEXTCLOUD)
}

/// Nothing but the message: no export, no dialog, no File tab, no browser tab, no page request.
fn assert_only_the_message(app: &SlideApp, spies: &Spies, case: &str) {
    assert!(said_no_nextcloud(app), "{case}: says {NO_NEXTCLOUD:?}");
    assert_eq!(spies.exported.get(), 0, "{case}: nothing exported or saved");
    assert_eq!(spies.host_shares.get(), 0, "{case}: the page is not asked");
    assert!(app.dialog.is_none(), "{case}: no dialog ({:?})", app.dialog.as_ref().map(|d| d.id.clone()));
    assert!(app.backstage.is_none(), "{case}: no Save As or Export page");
    assert!(app.pending_urls().is_empty(), "{case}: no browser tab");
}

/// Desktop, the file in a folder the Nextcloud client syncs: Nextcloud opens in the browser at
/// that file's sharing (the opener is the app's own URL queue, read before egui gets it).
#[test]
fn desktop_share_of_a_synced_file_opens_nextcloud_sharing_in_the_browser() {
    let spies = Spies::default();
    let mut app = SlideApp::new(Session::with_new(), services(&spies, true, false, false));
    with_path(&mut app, &format!("{SYNCED}Talks/Q3 review.pptx"));
    let r = app.run("file.share", json!({})).unwrap();
    let url = craft_share::sharing_url(SERVER, "/Talks/Q3 review.pptx");
    assert_eq!(app.pending_urls(), std::slice::from_ref(&url), "{r}");
    assert_eq!(r["url"], url);
    assert!(app.toast.is_none(), "no message when Nextcloud is connected");
    assert_eq!(spies.exported.get(), 0, "Share is not an export");
}

/// The desktop lookup reads the Nextcloud client's config (and on macOS runs `xattr`): it runs
/// once per presentation and file, not on every press, and again only when the file changes.
#[test]
fn the_desktop_lookup_runs_once_per_file() {
    let looked = Rc::new(Cell::new(0));
    let l = looked.clone();
    let services = Services {
        share_route: Some(Box::new(move |_: &std::path::Path| {
            l.set(l.get() + 1);
            ShareRoute::NextcloudDesktop { server: SERVER.into(), user: "someone".into(), remote_path: "/a.pptx".into() }
        })),
        ..Default::default()
    };
    let mut app = SlideApp::new(Session::with_new(), services);
    with_path(&mut app, &format!("{SYNCED}a.pptx"));
    for _ in 0..3 {
        app.run("file.share", json!({})).unwrap();
    }
    assert_eq!(looked.get(), 1, "pressing Share again reuses the answer");
    with_path(&mut app, &format!("{SYNCED}b.pptx"));
    app.run("file.share", json!({})).unwrap();
    assert_eq!(looked.get(), 2, "a new file (Save As) is looked up again");
}

/// Desktop without Nextcloud (a file outside every synced folder, an unsaved presentation, no
/// Nextcloud client at all) and the plain web build: only the message.
#[test]
fn without_nextcloud_share_only_says_so() {
    let cases: [(&str, bool, Option<&str>); 4] = [
        ("desktop, file outside the synced folders", true, Some("/home/someone/Documents/local.pptx")),
        ("desktop, never saved", true, None),
        ("desktop without the Nextcloud client", false, Some("/home/someone/Nextcloud/Talks/a.pptx")),
        ("plain web build", false, None),
    ];
    for (case, desktop, path) in cases {
        let spies = Spies::default();
        let mut app = SlideApp::new(Session::with_new(), services(&spies, desktop, false, false));
        if let Some(p) = path {
            with_path(&mut app, p);
        }
        let r = app.run("file.share", json!({})).unwrap();
        assert_eq!(r["message"], NO_NEXTCLOUD, "{case}");
        assert_only_the_message(&app, &spies, case);
    }
}

/// Web inside Nextcloud: the page that said `share: true` is asked to share the file it opened,
/// and only that file; a page that did not say so gets nothing and the message shows.
#[test]
fn web_share_asks_the_page_only_for_the_file_it_opened() {
    let pptx = {
        let s = Session::with_new();
        deckcraft_engine::cmd::file::save_bytes(&s.doc().unwrap().doc, "pptx").unwrap()
    };
    let spies = Spies::default();
    let mut app = SlideApp::new(Session::new(), services(&spies, false, true, true));
    app.start_for_host().unwrap();
    assert!(app.run("file.share", json!({})).is_ok());
    assert_only_the_message(&app, &spies, "the blank start presentation is not the page's file");
    app.toast = None;

    app.open_from_host(Some("Lecture.pptx"), &pptx);
    let r = app.run("file.share", json!({})).unwrap();
    assert_eq!(spies.host_shares.get(), 1, "{r}: the page is asked once");
    assert!(app.toast.is_none() && app.pending_urls().is_empty());
    assert_eq!(spies.exported.get(), 0, "Share is not a save");

    app.run("file.new", json!({})).unwrap();
    app.run("file.share", json!({})).unwrap();
    assert_eq!(spies.host_shares.get(), 1, "another presentation is not the page's file");
    assert!(said_no_nextcloud(&app));

    // A page that never said it shares (an older page) is not asked.
    let spies = Spies::default();
    let mut app = SlideApp::new(Session::new(), services(&spies, false, true, false));
    app.start_for_host().unwrap();
    app.open_from_host(Some("Lecture.pptx"), &pptx);
    app.run("file.share", json!({})).unwrap();
    assert_only_the_message(&app, &spies, "a page without share: true");
}

/// Every way to reach Share runs `file.share`: the File menu, command search, the Quick Access
/// Toolbar, the File tab's side menu and the control channel each open the synced file's sharing.
#[test]
fn every_entry_point_runs_share() {
    let url = craft_share::sharing_url(SERVER, "/a.pptx");
    let fresh = || {
        let mut app = SlideApp::new(Session::with_new(), services(&Spies::default(), true, false, false));
        with_path(&mut app, &format!("{SYNCED}a.pptx"));
        app
    };

    let menu = menus::menu_tree().into_iter().find(|(m, _)| *m == "File").unwrap().1;
    let (_, id) = menu.into_iter().find(|(label, _)| *label == "Share").expect("File › Share");
    let mut app = fresh();
    app.run(id, json!({})).unwrap();
    assert_eq!(app.pending_urls(), std::slice::from_ref(&url), "File menu");

    let mut app = fresh();
    let found = menus::palette_items(&app.session, "share");
    let (id, _, _) = found.iter().find(|(_, label, _)| label == "Share").expect("command search finds Share");
    app.run(id, json!({})).unwrap();
    assert_eq!(app.pending_urls(), std::slice::from_ref(&url), "command search");

    let mut app = fresh();
    qat::command(&mut app, &json!({"add": "file.share"})).unwrap();
    let at = app.ui.qat.iter().position(|q| q.id == "file.share").unwrap();
    assert!(qat::run_index(&mut app, at));
    assert_eq!(app.pending_urls(), std::slice::from_ref(&url), "Quick Access Toolbar");

    let mut app = fresh();
    assert!(backstage::entries(true).0.contains(&backstage::Nav::Share));
    app.open_backstage(backstage::Page::Info);
    let next = backstage::act(&mut app, backstage::Nav::Share, backstage::Page::Info);
    assert!(next.is_none(), "the File tab closes for the presentation");
    assert_eq!(app.pending_urls(), std::slice::from_ref(&url), "File tab");

    let mut app = fresh();
    let (req, _rx) = control::ControlRequest::new("command", json!({"command": "file.share"}));
    let control::Outcome::Done(answer) = control::handle(&mut app, &egui::Context::default(), &req) else { panic!() };
    assert_eq!(answer["result"]["url"], url, "{answer}");
    assert_eq!(app.pending_urls(), std::slice::from_ref(&url), "control channel");
    assert!(control::all_commands(&app).as_array().unwrap().iter().any(|c| c["id"] == "file.share"), "listed for automation");
}

/// The title bar's Share button runs Share: here, with no Nextcloud, it shows the message.
#[test]
fn the_title_bar_share_button_runs_share() {
    let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(1400.0, 800.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        SlideApp::new(Session::with_new(), Services::default()),
    );
    harness.run_steps(2);
    harness.get_by_label("Share").click();
    harness.run_steps(2);
    assert!(said_no_nextcloud(harness.state()), "the button ran Share");
    assert!(harness.state().dialog.is_none() && harness.state().backstage.is_none(), "and opened nothing");
    // The message is drawn where the user is looking.
    assert!(harness.query_by_label_contains(NO_NEXTCLOUD).is_some(), "the toast shows the message");
}
