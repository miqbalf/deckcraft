pub type ReadFn = Box<dyn Fn(&str) -> Result<Vec<u8>, String>>;

pub type WriteFn = Box<dyn FnMut(&str, &[u8]) -> Result<(), String>>;

pub type PickFn = Box<dyn FnMut(&str) -> Option<String>>;

/// `(purpose, request id)`. The host shows a picker and later pushes an [`InboxEvent`]
/// carrying that same id. Completing a cancelled or unknown id does nothing: the file is
/// not applied to whichever document happens to be active.
pub type OpenAsyncFn = Box<dyn FnMut(&str, u64)>;

pub type DownloadFn = Box<dyn FnMut(&str, &[u8])>;

/// [`Services::host_save`]: `(file name, bytes, save_as, id)`. An error means the page didn't get
/// the file, and the save failed. `id` is set when the page stores saves and answers each one
/// ([`HostSaves::confirms`]): it names the save its `saved` or `save-failed` answers.
pub type HostSaveFn = Box<dyn FnMut(&str, &[u8], bool, Option<&str>) -> Result<(), String>>;

/// What a page that answers saves said about one that carried an id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostAnswer {
    /// Stored; `name` is set when the page kept it under another name (a conflict copy, Save As).
    Saved { id: String, name: Option<String> },
    /// Not stored.
    Failed { id: String, error: String },
}

#[derive(Default)]
struct HostSavesState {
    confirms: bool,
    next: u64,
    /// Ids of copies (exports, Print) handed over: their failure is shown, their success needs nothing.
    copies: Vec<String>,
    answers: Vec<HostAnswer>,
}

/// What the page framing the app tells it about saves, shared between the web shell's listener and
/// the app (a cheap clone): whether it stores saves (the page's `saves`), and its answers, which
/// the app reads each frame.
#[derive(Clone, Default)]
pub struct HostSaves(std::sync::Arc<std::sync::Mutex<HostSavesState>>);

impl HostSaves {
    fn with<R>(&self, f: impl FnOnce(&mut HostSavesState) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// The page said it stores saves and answers each.
    pub fn set_confirms(&self) {
        self.with(|s| s.confirms = true);
    }

    pub fn confirms(&self) -> bool {
        self.with(|s| s.confirms)
    }

    /// A new id for a save about to be handed to the page.
    pub fn next_id(&self) -> String {
        self.with(|s| {
            s.next += 1;
            format!("save-{}", s.next)
        })
    }

    /// An id for a copy (export, Print) about to be handed over, when the page answers saves.
    pub fn copy_id(&self) -> Option<String> {
        self.confirms().then(|| {
            let id = self.next_id();
            self.with(|s| s.copies.push(id.clone()));
            id
        })
    }

    pub fn answer(&self, answer: HostAnswer) {
        self.with(|s| s.answers.push(answer));
    }

    pub(crate) fn take_answers(&self) -> Vec<HostAnswer> {
        self.with(|s| std::mem::take(&mut s.answers))
    }

    /// Whether `id` named a copy still waiting for its answer (it is not waiting after this).
    pub(crate) fn finish_copy(&self, id: &str) -> bool {
        self.with(|s| match s.copies.iter().position(|c| c == id) {
            Some(at) => {
                s.copies.remove(at);
                true
            }
            None => false,
        })
    }
}

/// A file the host finished reading for the request `id` from [`SlideApp::begin_external_file`].
#[derive(Clone, Debug)]
pub struct InboxFile {
    /// Cancellation identity. A file whose id was cancelled, or never started, is ignored.
    pub id: u64,
    pub name: String,
    pub bytes: Vec<u8>,
}

/// What the host delivers through [`Inbox`] after an asynchronous read.
#[derive(Clone, Debug)]
pub enum InboxEvent {
    File(InboxFile),
    /// The picker was dismissed or the file could not be read. `id` will not be applied.
    Cancelled {
        id: u64,
    },
}

/// Files delivered asynchronously by the host (web file picker, dropped files).
pub type Inbox = std::sync::Arc<std::sync::Mutex<Vec<InboxEvent>>>;

/// Platform services injected by the host (desktop or web).
#[derive(Default)]
pub struct Services {
    /// Browser-owned presenter popup; absent on native hosts.
    pub presenter: Option<Box<dyn crate::presenter::Host>>,
    /// Open-file dialog for a purpose (`open`, `picture`, `audio`, `video`) → path.
    pub pick_open: Option<PickFn>,
    /// Save dialog with a suggested name → path.
    pub pick_save: Option<PickFn>,
    pub read: Option<ReadFn>,
    pub write: Option<WriteFn>,
    /// Asynchronous open dialog (web). The callback receives the purpose and the request id
    /// from [`SlideApp::begin_external_file`]; the chosen file arrives through `inbox`.
    pub open_async: Option<OpenAsyncFn>,
    /// Hand bytes to the user as a named file (browser download).
    pub download: Option<DownloadFn>,
    /// Web, framed by a page that keeps the files (`?host=parent`): Save and Save As hand the
    /// presentation to that page instead of downloading it, with whether it is a new file
    /// ([`HostSaveFn`]). Save writes a presentation the page sent back under its own name and
    /// format, as the desktop app writes back to the file it opened; a presentation the page
    /// never sent (an untitled one, a repaired copy) goes as a new `.deckcraft` file, as Save
    /// suggests on the desktop. Save As (.pptx or .deckcraft) is always a new file. Either way the
    /// presentation is then that file, and the next Save goes to it. Exports and Print still go
    /// through `download`, which the host also points at the page: they are copies.
    pub host_save: Option<HostSaveFn>,
    /// Whether the page stores saves and answers each, and its answers ([`HostSaves`]). The
    /// presentation then counts as saved only when the page's `saved` for that save arrives.
    pub host_saves: HostSaves,
    pub inbox: Option<Inbox>,
    /// Image on the system clipboard (PNG bytes), when the host can read one.
    pub clipboard_image: Option<Box<dyn FnMut() -> Option<Vec<u8>>>>,
    /// HTML on the system clipboard (Pages, Keynote, and other apps that write `public.html`).
    pub clipboard_html: Option<Box<dyn FnMut() -> Option<String>>>,
    /// Put HTML and its plain-text alternative on the system clipboard. `true` when it landed.
    pub clipboard_set_html: Option<Box<dyn FnMut(&str, &str) -> bool>>,
    /// Audio output for media playback (cpal on desktop); without one media plays silently.
    pub audio_out: Option<Box<dyn deckcraft_media::AudioOut>>,
    /// How many displays are connected, when the host can tell: Presenter View needs a second.
    pub displays: Option<Box<dyn FnMut() -> Option<usize>>>,
    /// A presentation the host opened live ([`deckcraft_live::DeckLive::start`] on this window's
    /// session). The window takes it at its next frame, so a host may also set it later.
    pub live: Option<crate::live::LiveDeck>,
    /// Web, framed by a page that keeps the files: hands a copy of edits the room could not merge
    /// to the page as a new file (`saveAs: true`, Ruling D18), while the presentation's own saves
    /// are off. Tried before `download` and the Save dialog; an error keeps the copy waiting.
    pub save_copy: Option<WriteFn>,
}
