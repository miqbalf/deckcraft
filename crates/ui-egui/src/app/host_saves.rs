//! Saves to a page that stores them and answers each (`?host=parent`, the page's `saves`).
//!
//! The bytes go out with an id; the presentation counts as saved only when the page's `saved` for
//! that id arrives, and a `save-failed` leaves it unsaved with the page's reason. The page may
//! store the file under another name (a conflict copy, Save As): that is its file from then on.
//! Copies (exports, Print) carry ids too, so a failure to store one is shown.

use std::sync::Arc;

use deckcraft_model::Presentation;

use super::SlideApp;
use crate::HostAnswer;

/// A save the page has not answered yet.
pub(crate) struct PendingHostSave {
    pub(crate) id: String,
    /// The presentation it came from.
    pub(crate) uid: u64,
    /// What the presentation held when the bytes were taken: edits since are not in the stored file.
    pub(crate) doc: Arc<Presentation>,
    /// The name it went out under.
    pub(crate) name: String,
}

impl SlideApp {
    /// Read what the page answered since the last frame. An answer for a save that is not waiting
    /// (unknown, repeated, its presentation closed) changes nothing.
    pub(crate) fn poll_host_saves(&mut self) {
        for answer in self.services.host_saves.take_answers() {
            let (HostAnswer::Saved { id, .. } | HostAnswer::Failed { id, .. }) = &answer;
            if let Some(at) = self.copies_sent.iter().position(|(sent, _)| sent == id) {
                self.services.host_saves.finish_copy(id);
                let (_, copy) = self.copies_sent.remove(at);
                self.copy_answered(copy, answer);
                continue;
            }
            if self.services.host_saves.finish_copy(id) {
                if let HostAnswer::Failed { error, .. } = answer {
                    self.notify(format!("Couldn't save to Nextcloud: {error}"));
                }
                continue;
            }
            let Some(at) = self.pending_host_saves.iter().position(|p| p.id == *id) else { continue };
            let pending = self.pending_host_saves.remove(at);
            match answer {
                HostAnswer::Failed { error, .. } => {
                    let text = format!("Couldn't save to Nextcloud: {error}");
                    self.set_status(text.clone());
                    self.notify(text);
                }
                HostAnswer::Saved { name, .. } => self.host_stored(pending, name),
            }
        }
    }

    fn host_stored(&mut self, pending: PendingHostSave, name: Option<String>) {
        let name = name.unwrap_or(pending.name);
        let Some(doc) = self.session.document_mut(pending.uid) else { return };
        // What the page stored is the saved state, even when more was done since: that stays unsaved.
        let unchanged = Arc::ptr_eq(&doc.doc, &pending.doc);
        doc.saved_doc = pending.doc;
        doc.path = Some(name.clone());
        doc.revision += 1;
        if unchanged {
            self.set_status(format!("Saved {name}"));
        } else {
            self.set_status(format!("Saved {name}, but changes made since are not saved yet"));
        }
    }
}

impl SlideApp {
    /// The page is sending fonts (`true`), or all of them have arrived or failed (`false`). While
    /// they come the missing-font notice waits, and when they are in, the open presentations
    /// are checked again and redrawn with them.
    pub fn host_fonts_coming(&mut self, coming: bool) {
        self.host_fonts_coming = coming;
        if coming {
            return;
        }
        let found: Vec<(u64, Vec<String>)> =
            self.session.documents().iter().map(|d| (d.uid, deckcraft_engine::fonts::missing_fonts(&d.doc))).collect();
        for (uid, missing) in found {
            if let Some(doc) = self.session.document_mut(uid) {
                doc.missing_fonts = missing;
            }
        }
        // Slides drawn before the fonts came are drawn again with them.
        self.textures.invalidate();
        self.fonts_noted.clear();
    }

    /// Install a font file the page sent, like a font the user added. Returns the faces added
    /// (0 when it is not a font or is already there).
    pub fn install_host_font(&mut self, bytes: Vec<u8>) -> usize {
        deckcraft_fonts::FontDb::global().add_font(bytes)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use deckcraft_engine::Session;
    use serde_json::json;

    use super::*;
    use crate::Services;

    /// What the pretend page was handed: (name, save as, id).
    type Got = Rc<RefCell<Vec<(String, bool, Option<String>)>>>;

    /// The web app inside a page that stores saves and answers each, with a .pptx open.
    fn app() -> (SlideApp, Got) {
        let got: Got = Rc::default();
        let saves = got.clone();
        let services = Services {
            host_save: Some(Box::new(move |name: &str, _: &[u8], save_as: bool, id: Option<&str>| {
                saves.borrow_mut().push((name.to_string(), save_as, id.map(str::to_string)));
                Ok(())
            })),
            ..Default::default()
        };
        let mut app = SlideApp::new(Session::new(), services);
        app.start_for_host().unwrap();
        app.services.host_saves.set_confirms();
        let mut s = Session::with_new();
        s.execute("slide.new", &json!({})).unwrap();
        let bytes = deckcraft_engine::cmd::file::save_bytes(&s.doc().unwrap().doc, "pptx").unwrap();
        app.open_from_host(Some("deck.pptx"), &bytes);
        (app, got)
    }

    fn edit(app: &mut SlideApp) {
        app.session.execute("slide.new", &json!({})).unwrap();
    }

    fn dirty(app: &SlideApp) -> bool {
        app.session.active().unwrap().is_dirty()
    }

    fn path(app: &SlideApp) -> Option<String> {
        app.session.active().and_then(|d| d.path.clone())
    }

    fn sent(app: &mut SlideApp, got: &Got) -> String {
        edit(app);
        app.save();
        let id = got.borrow().last().and_then(|g| g.2.clone());
        id.expect("the save carried an id")
    }

    fn saved(id: &str, name: Option<&str>) -> HostAnswer {
        HostAnswer::Saved { id: id.into(), name: name.map(Into::into) }
    }

    #[test]
    fn the_presentation_is_saved_only_when_the_page_says_so_and_takes_its_name() {
        let (mut app, got) = app();
        let id = sent(&mut app, &got);
        app.poll_host_saves();
        assert!(dirty(&app), "handing the bytes over is not saving them");
        app.services.host_saves.answer(saved(&id, Some("deck (conflict).pptx")));
        app.poll_host_saves();
        assert!(!dirty(&app));
        assert_eq!(path(&app).as_deref(), Some("deck (conflict).pptx"));
    }

    #[test]
    fn a_refused_save_stays_unsaved_and_says_why() {
        let (mut app, got) = app();
        let id = sent(&mut app, &got);
        app.services.host_saves.answer(HostAnswer::Failed { id, error: "the file is locked".into() });
        app.poll_host_saves();
        assert!(dirty(&app));
        assert_eq!(app.toast.as_ref().map(|t| t.text.as_str()), Some("Couldn't save to Nextcloud: the file is locked"));
        assert_eq!(path(&app).as_deref(), Some("deck.pptx"), "the file keeps its name");
    }

    #[test]
    fn an_answer_counts_once_and_only_for_its_own_save() {
        let (mut app, got) = app();
        let id = sent(&mut app, &got);
        app.services.host_saves.answer(saved("save-99", None));
        app.poll_host_saves();
        assert!(dirty(&app), "an unknown id changes nothing");
        app.services.host_saves.answer(saved(&id, None));
        app.poll_host_saves();
        assert!(!dirty(&app));
        edit(&mut app);
        app.services.host_saves.answer(saved(&id, None));
        app.poll_host_saves();
        assert!(dirty(&app), "a repeated answer does not save a later edit");
        app.services.host_saves.answer(HostAnswer::Failed { id, error: "late".into() });
        app.poll_host_saves();
        assert!(app.toast.is_none(), "nor does a late failure of a save already answered");
    }

    #[test]
    fn work_done_while_the_page_stores_the_save_stays_unsaved() {
        let (mut app, got) = app();
        let id = sent(&mut app, &got);
        edit(&mut app);
        app.services.host_saves.answer(saved(&id, None));
        app.poll_host_saves();
        assert!(dirty(&app), "the stored file lacks the later edit");
    }

    #[test]
    fn an_export_is_a_copy_the_page_answers_but_the_presentation_stays_as_it_was() {
        let (mut app, got) = app();
        edit(&mut app);
        let id = app.services.host_saves.copy_id().expect("the page answers saves");
        app.services.host_saves.answer(HostAnswer::Failed { id, error: "no space".into() });
        app.poll_host_saves();
        assert_eq!(app.toast.as_ref().map(|t| t.text.as_str()), Some("Couldn't save to Nextcloud: no space"));
        assert!(dirty(&app));
        assert!(got.borrow().is_empty());
    }

    /// A .pptx whose title is set in `font`.
    fn deck_in_font(font: &str) -> Vec<u8> {
        let mut p = deckcraft_model::Presentation::default();
        let slide = Arc::make_mut(&mut p.slides[0]);
        let mut body = deckcraft_model::text::TextBody::from_text("Hello");
        body.paragraphs[0].runs[0].props.font = Some(font.into());
        slide.shapes[0].text = Some(body);
        deckcraft_engine::cmd::file::save_bytes(&p, "pptx").unwrap()
    }

    const FONT: &[u8] = include_bytes!("../../../render/tests/fixtures/colr-squares.ttf");

    #[test]
    fn fonts_from_the_page_clear_the_missing_font_notice_and_wait_for_it() {
        let (mut app, _) = app();
        app.open_from_host(Some("fonts.pptx"), &deck_in_font("DeckCraft Colr Squares"));
        assert_eq!(app.session.active().unwrap().missing_fonts, ["DeckCraft Colr Squares"]);
        app.host_fonts_coming(true);
        crate::toast::note_missing_fonts(&mut app);
        assert!(app.toast.is_none(), "the fonts may be the missing ones: no notice while they come");
        assert_eq!(app.install_host_font(FONT.to_vec()), 1);
        app.host_fonts_coming(false);
        assert!(app.session.active().unwrap().missing_fonts.is_empty());
        crate::toast::note_missing_fonts(&mut app);
        assert!(app.toast.is_none(), "nothing is missing any more");
    }

    #[test]
    fn a_font_the_page_sent_that_stays_missing_is_still_reported() {
        let (mut app, _) = app();
        app.open_from_host(Some("fonts.pptx"), &deck_in_font("Qqq Nonexistent Sans"));
        app.host_fonts_coming(true);
        app.host_fonts_coming(false);
        crate::toast::note_missing_fonts(&mut app);
        assert!(app.toast.as_ref().is_some_and(|t| t.text.contains("Qqq Nonexistent Sans")));
    }
}
