pub type ReadFn = Box<dyn Fn(&str) -> Result<Vec<u8>, String>>;

pub type WriteFn = Box<dyn FnMut(&str, &[u8]) -> Result<(), String>>;

pub type PickFn = Box<dyn FnMut(&str) -> Option<String>>;

/// `(purpose, request id)`. The host shows a picker and later pushes an [`InboxEvent`]
/// carrying that same id. Completing a cancelled or unknown id does nothing: the file is
/// not applied to whichever document happens to be active.
pub type OpenAsyncFn = Box<dyn FnMut(&str, u64)>;

/// [`Services::download`]: `(file name, MIME type, bytes)`.
pub type DownloadFn = Box<dyn FnMut(&str, &str, &[u8])>;

/// [`Services::pick_save_copy`]: `(suggested file name, filter name, extension)` → the path
/// picked in the system Save dialog, or `None` when it was cancelled.
pub type PickSaveCopyFn = Box<dyn FnMut(&str, &str, &str) -> Option<String>>;

/// [`Services::host_save`]: `(file name, bytes, save_as, id)`. An error means the page didn't get
/// the file, and the save failed. `id` is set when the page stores saves and answers each one
/// ([`HostSaves::confirms`]): it names the save its `saved` or `save-failed` answers.
pub type HostSaveFn = Box<dyn FnMut(&str, &[u8], bool, Option<&str>) -> Result<(), String>>;

/// [`Services::share_route`]: where Share goes for a presentation file on this computer
/// (`craft_share::desktop_route` on the desktop: a file in a folder the Nextcloud desktop client
/// syncs, else [`craft_share::ShareRoute::None`]).
pub type ShareRouteFn = Box<dyn Fn(&std::path::Path) -> craft_share::ShareRoute>;

/// [`Services::host_share`]: ask the page framing the app to open its sharing for the file it
/// opened (`deckcraft:share`). An error means the page didn't get the request.
pub type HostShareFn = Box<dyn FnMut() -> Result<(), String>>;

/// Whether the page framing the app shares files itself (it answered `share: true` in its `host`
/// message). Shared between the web shell's listener and the app (a cheap clone).
#[derive(Clone, Default)]
pub struct HostShares(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl HostShares {
    /// The page said it opens Nextcloud's sharing for the file it opened.
    pub fn set(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn get(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

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
    /// Desktop Save As / Export: the system Save dialog with one file type
    /// ([`PickSaveCopyFn`]). Without it a copy uses [`Self::pick_save`] (no file type filter).
    pub pick_save_copy: Option<PickSaveCopyFn>,
    /// Hand bytes to the user as a named file (browser download, `craft_web_host::web::download`).
    /// Every Save As and Export copy on the web goes here, also inside a page that keeps the files.
    pub download: Option<DownloadFn>,
    /// Web, framed by a page that keeps the files (`?host=parent`): Save and Save As hand the
    /// presentation to that page instead of downloading it, with whether it is a new file
    /// ([`HostSaveFn`]). Save writes a presentation the page sent back under its own name and
    /// format, as the desktop app writes back to the file it opened; a presentation the page
    /// never sent (an untitled one, a repaired copy) goes as a new `.deckcraft` file, as Save
    /// suggests on the desktop. Either way the
    /// presentation is then that file, and the next Save goes to it. Save As, the exports and
    /// Print never come here: they are copies and download ([`Self::download`]).
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
    /// Desktop: where Share goes for the active presentation's file ([`ShareRouteFn`]). Without
    /// it (the web build, tests) a file on this computer is not connected to Nextcloud.
    pub share_route: Option<ShareRouteFn>,
    /// Web, framed by a page (`?host=parent`): post the page's share request ([`HostShareFn`]).
    /// Used only once the page said it shares ([`Self::host_shares`]) and only for the file it
    /// opened in the frame.
    pub host_share: Option<HostShareFn>,
    /// Whether the framing page shares files itself ([`HostShares`]).
    pub host_shares: HostShares,
}
