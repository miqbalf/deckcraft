//! The host bridge, for pages that embed DeckCraft and keep its files (a Nextcloud app, a personal
//! file server). The messages and the two transports are `craft-web-host`'s
//! (storytold/craft-libs), the same contract as the other Craft apps' bridges, with the
//! `deckcraft:` prefix. This module is DeckCraft's part of it.
//!
//! - `?host=parent` (an `<iframe>`) talks to the same-origin parent over `postMessage`;
//!   `data-host="page"` on the canvas (the page's own document, no iframe) uses DOM
//!   `CustomEvent`s on the canvas instead. `?host=parent` wins when both are present. With no
//!   parent, or a parent on another origin, there is no bridge: the normal app with downloads.
//! - `deckcraft:ready` (to the host): sent once the app is running and listening; send a file now. DeckCraft
//!   reads bytes, so it says `file: false` and a host sends `bytes`.
//! - `deckcraft:open` (from the host): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`, name a bare file name. It opens like a file picked with File › Open on the
//!   desktop: a presentation sent with its name (.pptx, .potx, .ppsx, .deckcraft) has that name
//!   as its file, so Save goes back to it; an outline (.txt, .md, .rtf), a file without a name and
//!   a damaged file open untitled.
//! - `deckcraft:save` (to the host): `{ name, saveAs, bytes }` (with the bytes as a transferred `ArrayBuffer` over `postMessage`) for
//!   every file DeckCraft would otherwise download. The host stores it and reports failures
//!   itself. `saveAs` says whether the file becomes the presentation's file:
//!   - Save: `false`. It writes the opened file under its own name and format (a .pptx stays a
//!     .pptx), as the desktop app does. A name the parent never sent (an untitled presentation, an
//!     outline, a file sent without a name, a repaired copy of a damaged file, a file the user
//!     picked or dropped) is a new file: `true`, as for Save As, named `<title>.deckcraft` as
//!     Save suggests on the desktop.
//!   - Save As (File › Save As as .pptx or .deckcraft, Ctrl+Shift+S): `true`. The presentation is
//!     now that file; the next Save sends its name with `false`.
//!   - Export (PDF, pictures, outline, GIF, handouts, a Save As type that isn't a presentation)
//!     and Print: `false`. They are copies; the presentation's file stays the same.
//! - `deckcraft:host` with `saves: true` (from the host): the host stores saves and answers each.
//!   `deckcraft:save` then carries an `id` (for exports and Print too), the presentation counts as
//!   saved only on `deckcraft:saved { id, name? }` (a `name` is its file from then on: a conflict
//!   copy, Save As), and on `deckcraft:save-failed { id, error }` it stays unsaved with "Couldn't
//!   save to Nextcloud: {error}". Without `saves` nothing changes: a file handed over counts as
//!   saved at once.
//! - `deckcraft:host` with `share: true` (from the host): the host opens Nextcloud's own sharing
//!   for the file it opened. Share (`file.share`: the title bar's Share button, File › Share,
//!   command search, automation) then posts `deckcraft:share {}` while that file is the one shown;
//!   for any other presentation, or without `share`, the app says "No Nextcloud connected" and
//!   does nothing else. `deckcraft:ready` says `shareButton: true`: the app has its own Share
//!   button, so the host shows none.
//! - `deckcraft:fonts { fonts: [{name, url}] }` (from the host): each same-origin font file is
//!   fetched and installed like a font the user added; the missing-font notice waits for them.
//! - `deckcraft:dirty` whenever unsaved changes appear or go away, `deckcraft:failed` when the
//!   app couldn't start or refused a file of 2 GiB or larger.
//!
//! In host mode the app starts with an empty presentation instead of the sample, which gives way to
//! the first file the host sends; the host guards leaving the page, and `?author=` names the user
//! for comments.

use std::cell::RefCell;
use std::rc::Rc;

use craft_web_host::web::{FileData, Host, Message, fetch_font};
use deckcraft_ui_egui::{HostAnswer, HostSaves, HostShares, Services, SlideApp};

/// The app id in the host messages (`deckcraft:ready`).
const APP: &str = "deckcraft";

/// The name `craft-web-host` gives an open request that carries none (it cleans a host's name and
/// falls back to this): no name is no file, so the presentation opens untitled.
const NAMELESS: &str = "document";

/// The largest file a host may send. File › Open on the desktop refuses anything over 2 GiB.
/// On wasm32 a `Vec` cannot hold 2 GiB (`isize::MAX` is one byte less), so a file of exactly
/// 2 GiB is refused too, before its bytes are copied.
const MAX_OPEN_BYTES: f64 = 2_147_483_648.0;

/// The longest user name taken from `?author=` (the preferences' own limit).
const MAX_AUTHOR_CHARS: usize = 256;

/// What the host's `fonts` message leaves for the next frame.
enum FontEvent {
    /// Fonts are being fetched (true), or all arrived or failed (false).
    Coming(bool),
    /// A fetched font file.
    File(Vec<u8>),
}

#[derive(Clone)]
pub struct Bridge {
    host: Rc<Host>,
    /// Whether the host stores saves and answers each, and its answers (shared with the app).
    saves: HostSaves,
    /// Whether the host opens Nextcloud's sharing itself (its `host` message said `share: true`).
    shares: HostShares,
    fonts: Rc<RefCell<Vec<FontEvent>>>,
    /// Files the host sent (the name, if it sent one), waiting for the next frame.
    opened: Rc<RefCell<Vec<(Option<String>, Vec<u8>)>>>,
}

impl Bridge {
    /// The bridge for this page, or `None` when DeckCraft isn't hosted: no `?host=parent` and no
    /// `data-host="page"` on the canvas, an unframed `?host=parent` page (it stays the normal app
    /// with downloads), or a parent on another origin.
    pub fn detect(canvas: &web_sys::HtmlCanvasElement) -> Option<Self> {
        let host = Host::connect(APP, canvas)?;
        // An opaque origin (a file:// page in some browsers) can't be a postMessage target.
        if host.mode() == craft_web_host::Mode::Parent && web_sys::window()?.location().origin().ok()? == "null" {
            return None;
        }
        Some(Self { host: Rc::new(host), saves: HostSaves::default(), shares: HostShares::default(), fonts: Rc::default(), opened: Rc::default() })
    }

    /// Hand saved and exported files to the host instead of downloading them, and hear the files
    /// it sends.
    pub fn connect(&self, services: &mut Services, ctx: egui::Context) {
        self.listen(ctx);
        // The app and the listener share one answer queue.
        services.host_saves = self.saves.clone();
        let bridge = self.clone();
        services.host_save =
            Some(Box::new(move |name: &str, bytes: &[u8], save_as: bool, id: Option<&str>| bridge.post_save(name, bytes, save_as, id)));
        // Share (`file.share`): a host that said `share: true` opens Nextcloud's sharing for the
        // file it opened (`deckcraft:share`); otherwise the app says no Nextcloud is connected.
        services.host_shares = self.shares.clone();
        let bridge = self.clone();
        services.host_share = Some(Box::new(move || bridge.host.post_share()));
        // Exports and Print are copies: the host answers them too, so a failure is shown.
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            let id = bridge.saves.copy_id();
            if let Err(e) = bridge.post_save(name, bytes, false, id.as_deref()) {
                log::error!("couldn't hand {name} to the page: {e}");
                if let Some(id) = id {
                    // No answer will come: say so now.
                    bridge.saves.answer(HostAnswer::Failed { id, error: e });
                }
            }
        }));
    }

    /// `?author=`: the host's name for the user signs new comments.
    pub fn set_author(app: &mut SlideApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.prefs.author = author;
        }
    }

    pub fn post_ready(&self) {
        // `shareButton: true`: DeckCraft has its own Share button, so the host shows none.
        let _ = self.host.post_ready_with(env!("CARGO_PKG_VERSION"), false, true);
    }

    /// The app couldn't start (no WebGPU or WebGL2, for example): let the host say so.
    pub fn post_failed(&self, error: &str) {
        let _ = self.host.post_failed(error, None);
    }

    /// Before each frame: fonts the host's files may need go in first.
    pub fn install_fonts(&self, app: &mut SlideApp) {
        let events = std::mem::take(&mut *self.fonts.borrow_mut());
        for event in events {
            match event {
                FontEvent::Coming(coming) => app.host_fonts_coming(coming),
                FontEvent::File(bytes) => {
                    if app.install_host_font(bytes) == 0 {
                        log::warn!("a host font was not a font DeckCraft can read, or was already there");
                    }
                }
            }
        }
    }

    /// Before each frame: open the files the host sent.
    pub fn open_files(&self, app: &mut SlideApp) {
        let opened = std::mem::take(&mut *self.opened.borrow_mut());
        for (name, bytes) in opened {
            app.open_from_host(name.as_deref(), &bytes);
        }
    }

    /// After each frame: tell the host when unsaved changes appear or go away (the crate does not
    /// repeat a state it already reported).
    pub fn report_dirty(&self, app: &SlideApp) {
        self.host.post_dirty(app.session.documents().iter().any(|d| d.is_dirty()));
    }

    fn post_save(&self, path: &str, bytes: &[u8], save_as: bool, id: Option<&str>) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        self.host.post_save(&name, bytes, id, save_as)
    }

    /// Hear `deckcraft:open` and queue the file for the next frame.
    fn listen(&self, ctx: egui::Context) {
        let opened = self.opened.clone();
        let host = self.host.clone();
        let saves = self.saves.clone();
        let shares = self.shares.clone();
        let fonts = self.fonts.clone();
        self.host.listen(move |message| {
            let (name, data) = match message {
                Message::Open { name, data } => (name, data),
                Message::Host { saves: confirms, share, .. } => {
                    if confirms {
                        saves.set_confirms();
                    }
                    if share {
                        shares.set();
                    }
                    return;
                }
                Message::Saved { id, name } => {
                    saves.answer(HostAnswer::Saved { id, name });
                    return ctx.request_repaint();
                }
                Message::SaveFailed { id, error } => {
                    saves.answer(HostAnswer::Failed { id, error });
                    return ctx.request_repaint();
                }
                Message::Fonts { fonts: list } => return fetch_fonts(list, fonts.clone(), ctx.clone()),
                _ => return,
            };
            if data.size() >= MAX_OPEN_BYTES {
                log::warn!("host file {name} is 2 GiB or larger; not opened");
                let _ = host.post_failed(&format!("{name} is 2 GiB or larger, which DeckCraft's web build cannot open"), None);
                return;
            }
            let FileData::Bytes(array) = data else {
                log::warn!("host file {name} came as a File; DeckCraft reads bytes only; not opened");
                return;
            };
            // No name is no file: the presentation opens untitled, never under an invented name.
            let name = (name != NAMELESS).then_some(name);
            opened.borrow_mut().push((name, array.to_vec()));
            ctx.request_repaint();
        });
    }
}

/// Fetch each font a host offered (one at a time, same-origin only, size-capped by the bridge) and
/// leave its bytes for the next frame; a failure is logged and skipped.
fn fetch_fonts(list: Vec<craft_web_host::FontRef>, events: Rc<RefCell<Vec<FontEvent>>>, ctx: egui::Context) {
    if list.is_empty() {
        return;
    }
    events.borrow_mut().push(FontEvent::Coming(true));
    ctx.request_repaint();
    wasm_bindgen_futures::spawn_local(async move {
        for font in list {
            match fetch_font(&font).await {
                Ok(bytes) => events.borrow_mut().push(FontEvent::File(bytes)),
                Err(e) => log::warn!("host font {} not fetched: {e}", font.name),
            }
            ctx.request_repaint();
        }
        events.borrow_mut().push(FontEvent::Coming(false));
        ctx.request_repaint();
    });
}
