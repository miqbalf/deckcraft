//! Save As = Export (`app::save_as`): one list of every format DeckCraft writes, one function every
//! Save As and Export entry point ends in, a download on the web (also inside a page that keeps the
//! files, which is never posted to) and the system Save dialog on the desktop. The presentation
//! keeps its file, name and unsaved changes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

use craft_share::export::{Delivery, Runtime};
use egui::accesskit::Role;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use crate::app::save_as::engine_format;
use crate::{SAVE_AS, Services, Session, SlideApp, backstage, control, menus, qat};

/// What the web build downloaded: (name, MIME type, bytes).
type Downloads = Rc<RefCell<Vec<(String, String, Vec<u8>)>>>;

/// The plain web build: copies download.
fn web(downloads: &Downloads) -> Services {
    let log = downloads.clone();
    Services {
        download: Some(Box::new(move |name: &str, mime: &str, b: &[u8]| {
            log.borrow_mut().push((name.into(), mime.into(), b.to_vec()));
            Ok(())
        })),
        ..Default::default()
    }
}

/// The web build framed by a page that keeps the files (`?host=parent`, `saves: true`): `posted`
/// counts everything handed to the page.
fn hosted(downloads: &Downloads, posted: &Rc<Cell<usize>>) -> Services {
    let mut s = web(downloads);
    let p = posted.clone();
    s.host_save = Some(Box::new(move |_: &str, _: &[u8], _: bool, _: Option<&str>| {
        p.set(p.get() + 1);
        Ok(())
    }));
    s.host_saves.set_confirms();
    s
}

/// What the desktop Save dialog was asked: (suggested name, filter name, extension).
type Asked = Rc<RefCell<Vec<(String, String, String)>>>;

/// The desktop: the Save dialog answers `dir`/`answer` (or the suggested name).
fn desktop(dir: &std::path::Path, answer: Option<&str>, asked: &Asked) -> Services {
    let (dir, answer, log) = (dir.to_path_buf(), answer.map(str::to_string), asked.clone());
    Services {
        pick_save_copy: Some(Box::new(move |name: &str, filter: &str, ext: &str| {
            log.borrow_mut().push((name.into(), filter.into(), ext.into()));
            Some(dir.join(answer.clone().unwrap_or_else(|| name.to_string())).to_string_lossy().to_string())
        })),
        ..Default::default()
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("deckcraft-saveas-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A presentation with two slides and unsaved changes, opened from `path` when given.
fn deck(services: Services, path: Option<&str>) -> SlideApp {
    let mut app = SlideApp::new(Session::with_new(), services);
    if let Some(p) = path {
        app.session.active_mut().unwrap().path = Some(p.into());
    }
    app.run("slide.new", json!({})).unwrap();
    assert!(app.session.active().unwrap().is_dirty());
    app
}

/// (path, title, dirty, revision): what Save As must leave as it was.
fn doc_state(app: &SlideApp) -> (Option<String>, String, bool, u64) {
    let d = app.session.active().unwrap();
    (d.path.clone(), d.title(), d.is_dirty(), d.revision)
}

fn ids() -> Vec<&'static str> {
    SAVE_AS.formats().iter().map(|f| f.id).collect()
}

/// The list follows craft-share's rules and offers every format `file.export` writes, no more.
#[test]
fn the_list_passes_check_and_covers_every_writer() {
    assert_eq!(SAVE_AS.check(), Ok(()));
    let listed: BTreeSet<&str> = ids().iter().map(|id| engine_format(id).unwrap_or_else(|| panic!("{id} has no writer")).0).collect();
    let writers: BTreeSet<&str> = deckcraft_engine::cmd::file::EXPORT_FORMATS.into_iter().collect();
    assert_eq!(listed, writers, "the Save As list and the writers differ");
    // And each writer really writes: `file.export` accepts it.
    let dir = temp_dir("writers");
    let mut s = Session::with_new();
    for w in deckcraft_engine::cmd::file::EXPORT_FORMATS {
        let path = dir.join(format!("w.{w}")).to_string_lossy().to_string();
        s.execute("file.export", &json!({"path": path, "format": w})).unwrap_or_else(|e| panic!("{w}: {e}"));
    }
    assert!(s.execute("file.export", &json!({"path": dir.join("w.x").to_string_lossy(), "format": "docx"})).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The web, plain or framed by a page that keeps the files: the plan is a download.
#[test]
fn the_plan_is_a_download_on_the_web_and_a_dialog_on_the_desktop() {
    let downloads = Downloads::default();
    let posted = Rc::new(Cell::new(0));
    assert_eq!(deck(web(&downloads), None).save_as_runtime(), Runtime::Web);
    assert_eq!(deck(hosted(&downloads, &posted), None).save_as_runtime(), Runtime::WebHosted);
    let dir = temp_dir("runtime");
    assert_eq!(deck(desktop(&dir, None, &Asked::default()), None).save_as_runtime(), Runtime::Desktop);
    for (runtime, delivery) in
        [(Runtime::Web, Delivery::Download), (Runtime::WebHosted, Delivery::Download), (Runtime::Desktop, Delivery::SaveDialog)]
    {
        let plan = SAVE_AS.plan(runtime, "Lecture 3.pptx", "Presentation", "pdf").unwrap();
        assert_eq!((plan.file_name.as_str(), plan.format.mime, plan.delivery), ("Lecture 3.pdf", "application/pdf", delivery));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Web: every format downloads under the plan's name and MIME type; the all-slides pictures one
/// file a slide. The presentation keeps its file and its unsaved changes.
#[test]
fn web_save_as_downloads_every_format_with_the_plans_name_and_mime() {
    let downloads = Downloads::default();
    let mut app = deck(web(&downloads), Some("talks/Lecture 3.pptx"));
    let before = doc_state(&app);
    for f in SAVE_AS.formats() {
        downloads.borrow_mut().clear();
        assert!(app.save_as_copy(f.id, json!({})), "{}", f.id);
        let got: Vec<(String, String)> = downloads.borrow().iter().map(|(n, m, _)| (n.clone(), m.clone())).collect();
        let name = SAVE_AS.file_name("Lecture 3.pptx", "Presentation", f.id).unwrap();
        let want: Vec<(String, String)> = if f.many {
            (1..=2).map(|k| (format!("Lecture 3-{k}.{}", f.ext), f.mime.to_string())).collect()
        } else {
            vec![(name, f.mime.to_string())]
        };
        assert_eq!(got, want, "{}", f.id);
        assert!(downloads.borrow().iter().all(|(_, _, b)| !b.is_empty()));
        assert_eq!(doc_state(&app), before, "{}: the presentation is untouched", f.id);
    }
}

/// Inside Nextcloud (a page that keeps the files and answers saves): Save As of every format, the
/// presentation's own ones too, downloads a copy. Nothing is posted to the page, the presentation
/// keeps the page's file and its unsaved changes, and no refusal is shown.
#[test]
fn hosted_save_as_downloads_and_posts_nothing_to_the_page() {
    let downloads = Downloads::default();
    let posted = Rc::new(Cell::new(0));
    let mut app = SlideApp::new(Session::new(), hosted(&downloads, &posted));
    app.start_for_host().unwrap();
    let bytes = deckcraft_engine::cmd::file::save_bytes(&Session::with_new().doc().unwrap().doc, "pptx").unwrap();
    app.open_from_host(Some("Lecture 3.pptx"), &bytes);
    app.run("slide.new", json!({})).unwrap();
    let before = doc_state(&app);
    for id in ids() {
        assert!(app.save_as_copy(id, json!({})), "{id}");
        assert_eq!(posted.get(), 0, "{id}: nothing went to the page");
        assert_eq!(doc_state(&app), before, "{id}: still the page's file, still unsaved");
        let status = app.status.as_ref().map(|(m, _)| m.to_lowercase()).unwrap_or_default();
        assert!(!status.contains("automatically") && !status.contains("for everyone") && !status.contains("can't"), "{id}: {status}");
    }
    assert!(downloads.borrow().iter().any(|(n, _, _)| n == "Lecture 3.pptx"));
    assert!(downloads.borrow().iter().any(|(n, _, _)| n == "Lecture 3.deckcraft"));
    // Plain Save still goes to the page, under the page's name.
    app.save();
    assert_eq!(posted.get(), 1);
}

/// Desktop: the Save dialog suggests the plan's name with the format's one file type, the copy
/// is written there, and the window stays on its own file with its unsaved changes, also for
/// .pptx and .deckcraft.
#[test]
fn desktop_save_as_writes_a_copy_and_keeps_the_presentation() {
    let dir = temp_dir("desktop");
    let own = dir.join("Deck.pptx").to_string_lossy().to_string();
    let asked = Asked::default();
    let mut app = deck(desktop(&dir, None, &asked), Some(&own));
    let before = doc_state(&app);
    for f in SAVE_AS.formats() {
        asked.borrow_mut().clear();
        assert!(app.save_as_copy(f.id, json!({})), "{}", f.id);
        let name = SAVE_AS.file_name("Deck.pptx", "Presentation", f.id).unwrap();
        assert_eq!(*asked.borrow(), [(name.clone(), f.label.to_string(), f.ext.to_string())], "{}", f.id);
        if f.many {
            for k in 1..=2 {
                assert!(dir.join(format!("Deck-{k}.{}", f.ext)).is_file(), "{} slide {k}", f.id);
            }
        } else if f.id != "pptx" {
            assert!(dir.join(&name).is_file(), "{}", f.id);
        }
        assert_eq!(doc_state(&app), before, "{}: same file, name, dirty flag", f.id);
    }
    assert!(std::fs::metadata(&own).is_ok_and(|m| m.len() > 0), "Save As .pptx over its own name wrote a copy there");
    assert!(app.ui.recent.is_empty(), "a copy is not a recent presentation");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Desktop: the dialog's overwrite question was about the name typed. When the extension rule
/// changes it to a file that exists, Save As refuses instead of overwriting.
#[test]
fn desktop_save_as_refuses_to_overwrite_a_renamed_file() {
    let dir = temp_dir("renamed");
    std::fs::write(dir.join("out.pdf"), b"keep").unwrap();
    let mut app = deck(desktop(&dir, Some("out"), &Asked::default()), None);
    assert!(!app.save_as_copy("pdf", json!({})));
    assert_eq!(std::fs::read(dir.join("out.pdf")).unwrap(), b"keep");
    assert!(app.status.as_ref().is_some_and(|(m, _)| m.contains("already exists; choose another name")), "{:?}", app.status);
    // A name that the dialog itself confirmed is written.
    let mut app = deck(desktop(&dir, Some("out.pdf"), &Asked::default()), None);
    assert!(app.save_as_copy("pdf", json!({})));
    assert!(std::fs::read(dir.join("out.pdf")).unwrap().starts_with(b"%PDF"));
    let _ = std::fs::remove_dir_all(&dir);
}

fn page(app: &SlideApp) -> Option<backstage::Page> {
    app.backstage.as_ref().map(|b| b.page)
}

fn dialog(app: &SlideApp) -> Option<&str> {
    app.dialog.as_ref().map(|d| d.id.as_str())
}

/// Every way to Save As opens the one list (File › Save As) and every way to Export opens the
/// Export… dialog, which lists the same formats; with a format, automation makes the copy at once.
#[test]
fn every_entry_point_reaches_the_one_save_as() {
    let downloads = Downloads::default();
    let fresh = || deck(web(&downloads), Some("Deck.pptx"));

    let file_menu = menus::menu_tree().into_iter().find(|(m, _)| *m == "File").unwrap().1;
    for (label, want_page, want_dialog) in [("Save As…", Some(backstage::Page::SaveAs), None), ("Export…", None, Some("export"))] {
        let (_, id) = file_menu.iter().find(|(l, _)| *l == label).unwrap();
        let mut app = fresh();
        app.run(id, json!({})).unwrap();
        assert_eq!((page(&app), dialog(&app)), (want_page, want_dialog), "File › {label}");
    }

    let mut app = fresh();
    let found = menus::palette_items(&app.session, "save as");
    let (id, _, _) = found.iter().find(|(_, label, _)| label == "Save As…").expect("command search finds Save As…");
    app.run(id, json!({})).unwrap();
    assert_eq!(page(&app), Some(backstage::Page::SaveAs), "command search");

    let mut app = fresh();
    let at = app.ui.qat.iter().position(|q| q.id == "app.exportDialog").expect("Export is on the Quick Access Toolbar");
    assert!(qat::run_index(&mut app, at));
    assert_eq!(dialog(&app), Some("export"), "Quick Access Toolbar");

    // `file.saveAs` without a path (engine registry, automation) is Save As too, not a Save.
    let mut app = fresh();
    app.run("file.saveAs", json!({})).unwrap();
    assert_eq!(page(&app), Some(backstage::Page::SaveAs), "file.saveAs without a path");
    assert_eq!(app.session.active().unwrap().path.as_deref(), Some("Deck.pptx"));

    // The control channel: `app.export` without a path, `app.saveAsDialog` with a format.
    for (method, params, name) in [
        ("app.export", json!({"format": "pdf"}), "Deck.pdf"),
        ("app.export", json!({"format": "png", "all": true}), "Deck-1.png"),
        ("command", json!({"command": "app.saveAsDialog", "params": {"format": "rtf"}}), "Deck.rtf"),
    ] {
        downloads.borrow_mut().clear();
        let mut app = fresh();
        let (req, _rx) = control::ControlRequest::new(method, params.clone());
        let control::Outcome::Done(answer) = control::handle(&mut app, &egui::Context::default(), &req) else { panic!("{method}") };
        assert!(answer.get("error").is_none_or(serde_json::Value::is_null), "{method} {params}: {answer}");
        assert_eq!(downloads.borrow().first().map(|d| d.0.as_str()), Some(name), "{method} {params}");
    }
}

fn harness(app: SlideApp) -> Harness<'static, SlideApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    h
}

/// Clicks the button `label` on the File tab's page (not the side menu's entry of that name).
fn click_on_page(h: &Harness<'_, SlideApp>, label: &str) {
    let node = h
        .query_all_by_role_and_label(Role::Button, label)
        .find(|n| n.rect().left() > backstage::NAV_W + 20.0)
        .or_else(|| h.query_all_by_label(label).find(|n| n.rect().left() > backstage::NAV_W + 20.0));
    node.unwrap_or_else(|| panic!("no “{label}” on the page")).click();
}

/// Through real input: Ctrl+Shift+S opens File › Save As; its Download button, Export › Create
/// PDF and Export › Change File Type, and the Export… dialog each download a copy.
#[test]
fn the_pages_and_the_dialog_download_through_the_one_save_as() {
    let downloads = Downloads::default();
    let mut h = harness(deck(web(&downloads), Some("Deck.pptx")));
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::S);
    h.run_steps(3);
    assert_eq!(page(h.state()), Some(backstage::Page::SaveAs), "Ctrl+Shift+S");
    assert!(h.query_by_label_contains("for everyone").is_none());
    click_on_page(&h, "Download");
    h.run_steps(3);
    assert_eq!(downloads.borrow().last().map(|d| d.0.clone()).as_deref(), Some("Deck.pptx"));
    assert!(h.state().backstage.is_none(), "the copy is made: back to the slide");

    h.state_mut().run("app.backstage", json!({"page": "export"})).unwrap();
    h.run_steps(3);
    click_on_page(&h, "Create PDF");
    h.run_steps(3);
    assert_eq!(downloads.borrow().last().map(|d| (d.0.clone(), d.1.clone())), Some(("Deck.pdf".into(), "application/pdf".into())));
    click_on_page(&h, "Change File Type");
    h.run_steps(3);
    click_on_page(&h, "Animated GIF (*.gif)");
    h.run_steps(2);
    click_on_page(&h, "Save As");
    h.run_steps(3);
    assert_eq!(downloads.borrow().last().map(|d| d.0.clone()).as_deref(), Some("Deck.gif"));
    h.key_press(Key::Escape);
    h.run_steps(3);

    h.state_mut().run("app.exportDialog", json!({})).unwrap();
    h.run_steps(2);
    h.get_by_label("Outline/RTF (.rtf)").click_accesskit();
    h.run_steps(2);
    h.key_press(Key::Enter);
    h.run_steps(3);
    assert_eq!(downloads.borrow().last().map(|d| d.0.clone()).as_deref(), Some("Deck.rtf"));
    assert_eq!(doc_state(h.state()).0.as_deref(), Some("Deck.pptx"));
    assert!(doc_state(h.state()).2, "still unsaved");
}

/// Plain Save is unchanged: on the desktop the File tab's Save on a presentation that was never
/// saved goes to File › Save As (as before), and Ctrl+S asks where and the file picked becomes its
/// file. Inside a page that keeps the files, the File tab's Save goes to the page.
#[test]
fn plain_save_of_an_untitled_presentation_still_takes_the_file() {
    let dir = temp_dir("untitled");
    let picked = dir.join("New.deckcraft").to_string_lossy().to_string();
    let p = picked.clone();
    let services = Services { pick_save: Some(Box::new(move |_: &str| Some(p.clone()))), ..Default::default() };
    let mut app = deck(services, None);
    app.open_backstage(backstage::Page::Info);
    let next = backstage::act(&mut app, backstage::Nav::Save, backstage::Page::Info);
    assert_eq!(next, Some(backstage::Page::SaveAs), "the File tab's Save goes to Save As, as before");
    app.save();
    assert_eq!(app.session.active().unwrap().path.as_deref(), Some(picked.as_str()));
    assert!(!app.session.active().unwrap().is_dirty());

    let downloads = Downloads::default();
    let posted = Rc::new(Cell::new(0));
    let mut app = SlideApp::new(Session::new(), hosted(&downloads, &posted));
    app.start_for_host().unwrap();
    app.run("slide.new", json!({})).unwrap();
    app.open_backstage(backstage::Page::Info);
    let next = backstage::act(&mut app, backstage::Nav::Save, backstage::Page::Info);
    assert_ne!(next, Some(backstage::Page::SaveAs), "inside the page Save is not a download");
    assert_eq!(posted.get(), 1, "the untitled presentation went to the page");
    assert!(downloads.borrow().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Desktop many-file export: the dialog confirms only the base name, so when numbered files
/// already exist the user is asked once, with how many; No writes nothing, Yes replaces them, and
/// without a way to ask nothing is replaced.
#[test]
fn many_file_desktop_export_asks_before_replacing_numbered_files() {
    let dir = temp_dir("many-exist");
    std::fs::write(dir.join("Deck-2.png"), b"old").unwrap();
    let asked: Rc<RefCell<Vec<String>>> = Rc::default();
    for answer in [None, Some(false), Some(true)] {
        let mut services = desktop(&dir, Some("Deck.png"), &Asked::default());
        if let Some(yes) = answer {
            let log = asked.clone();
            services.confirm = Some(Box::new(move |q: &str| {
                log.borrow_mut().push(q.to_string());
                yes
            }));
        }
        let mut app = deck(services, Some("Deck.pptx"));
        let wrote = app.save_as_copy("pngAll", json!({}));
        assert_eq!(wrote, answer == Some(true), "{answer:?}");
        let kept = std::fs::read(dir.join("Deck-2.png")).unwrap() == b"old";
        assert_eq!(kept, answer != Some(true), "{answer:?}: Deck-2.png replaced only on Yes");
        assert_eq!(dir.join("Deck-1.png").exists(), answer == Some(true), "{answer:?}: nothing written unless Yes");
    }
    assert_eq!(asked.borrow().len(), 2, "asked once per export");
    assert!(asked.borrow().iter().all(|q| q.contains("1 of the 2 files")), "{:?}", asked.borrow());

    // No numbered file there: nothing to ask.
    let fresh = temp_dir("many-fresh");
    let mut services = desktop(&fresh, Some("Deck.jpg"), &Asked::default());
    services.confirm = Some(Box::new(|_: &str| panic!("nothing to replace")));
    assert!(deck(services, Some("Deck.pptx")).save_as_copy("jpegAll", json!({})));
    assert!(fresh.join("Deck-1.jpg").is_file() && fresh.join("Deck-2.jpg").is_file());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fresh);
}

/// The web shell's page wiring (`connect_host`, what `deckcraft-web`'s bridge calls): after it,
/// Save As, every export and Print still download and nothing is posted to the page; only plain
/// Save and Share reach it.
#[test]
fn the_page_wiring_posts_only_save_and_share() {
    let downloads = Downloads::default();
    let (posted, shared) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let mut services = web(&downloads);
    let (p, sh) = (posted.clone(), shared.clone());
    let saves = crate::HostSaves::default();
    saves.set_confirms();
    let shares = crate::HostShares::default();
    shares.set();
    crate::connect_host(
        &mut services,
        crate::HostHooks {
            post_save: Box::new(move |_: &str, _: &[u8], _: bool, _: Option<&str>| {
                p.set(p.get() + 1);
                Ok(())
            }),
            post_share: Box::new(move || {
                sh.set(sh.get() + 1);
                Ok(())
            }),
            saves,
            shares,
        },
    );
    let mut app = SlideApp::new(Session::new(), services);
    app.start_for_host().unwrap();
    let bytes = deckcraft_engine::cmd::file::save_bytes(&Session::with_new().doc().unwrap().doc, "pptx").unwrap();
    app.open_from_host(Some("Talk.pptx"), &bytes);
    for id in ids() {
        assert!(app.save_as_copy(id, json!({})), "{id}");
    }
    assert!(app.print_pdf(json!({})));
    app.run("app.saveAsDialog", json!({"format": "pptx"})).unwrap();
    assert_eq!(posted.get(), 0, "Save As, exports and Print post nothing");
    assert!(downloads.borrow().len() > ids().len(), "they download");
    app.save();
    assert_eq!(posted.get(), 1, "plain Save goes to the page");
    app.run("file.share", json!({})).unwrap();
    assert_eq!(shared.get(), 1, "Share asks the page");
}

/// A download the browser refuses is reported, not shown as done.
#[test]
fn a_failed_download_says_so() {
    let services = Services { download: Some(Box::new(|_: &str, _: &str, _: &[u8]| Err("blocked".into()))), ..Default::default() };
    let mut app = deck(services, Some("Deck.pptx"));
    assert!(!app.save_as_copy("pdf", json!({})));
    assert!(app.status.as_ref().is_some_and(|(m, _)| m.contains("Couldn't download Deck.pdf") && m.contains("blocked")), "{:?}", app.status);
}

/// Web downloads get the list's MIME type, the native format's too, and only the file's name.
#[test]
fn download_names_and_types() {
    assert_eq!(crate::download_mime("Talk.deckcraft"), SAVE_AS.get("deckcraft").unwrap().mime);
    assert_eq!(crate::download_mime("Talk.PPTX"), SAVE_AS.get("pptx").unwrap().mime);
    assert_eq!(crate::download_mime("notes.md"), craft_share::export::mime_for_ext("md"));
    assert_eq!(crate::download_name("docs/a\\Talk.pptx"), "Talk.pptx");
    assert!(!crate::download_name("docs/").is_empty());
}
