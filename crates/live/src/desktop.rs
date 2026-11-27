//! The desktop app's side of a live session (phase 2, "desktop joins").
//!
//! The launcher asks Nextcloud for a ticket, downloads the room's base file and starts DeckCraft
//! with `--live-session <file>` and that file. The session file ([`SessionFile`]) carries the
//! ticket, the relay, the room and the base file's checksum; the launcher rewrites it atomically
//! with a fresh ticket before the old one expires ([`Watch`]) and deletes it once the app has
//! quit. The app joins over [`LazyHub`], a native socket hub that connects in the background so
//! no frame waits on the relay.
//!
//! The ticket is never logged, and [`SessionFile`]'s `Debug` leaves it out.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_live::Access;
use serde_json::Value;

/// The session file format this build reads.
pub const SESSION_VERSION: u64 = 1;
/// How often the session file is checked for a fresh ticket, in ms.
pub const WATCH_EVERY_MS: u64 = 2000;

/// Said when the session file could not be read.
pub const UNREADABLE_TEXT: &str = "The live session file could not be read.";
/// Said when the session file is of a format this build does not know.
pub const NEWER_SESSION_TEXT: &str = "This live session needs a newer DeckCraft.";
/// Said when the session file lacks something a join needs: `{field}`.
pub const MISSING_TEXT: &str = "The live session file has no {field}.";
/// Said when the relay is not on the server the file came from.
pub const RELAY_ELSEWHERE_TEXT: &str = "The live session's relay is not on the file's server.";
/// Said when the session names no checksum for the room's base file, so it cannot be checked.
pub const NO_BASE_HASH_TEXT: &str = "The live session gives no checksum for the file, so it cannot be checked.";
/// Said, after the reason, when live editing did not start and the file opened for one person.
pub const NOT_SHARED_TEXT: &str =
    "Live editing could not start: {why} Your changes are not shared and stay on this computer; use Save As to keep them.";
/// Said when the launcher's session moved to another room while this window was in one.
pub const RESTARTED_TEXT: &str = "The live session was restarted. Reopen the file.";
/// Said by Save while the live presentation is active, before the room's first save.
pub const SAVES_LIVE_TEXT: &str = "Live: the server saves this presentation as you edit";
/// Said by Save while the live presentation is active, after a save: `{time}`.
pub const SAVED_LIVE_TEXT: &str = "Live · saved {time}";
/// Said by Save while edits are still on their way to the room.
pub const SENDING_TEXT: &str = "Live: your latest changes are still being sent";

/// Said when the room has no document service saving it right now (the ticket answer's
/// `saver` is false).
pub const NOT_SAVING_TEXT: &str = "Changes are not being saved right now";

/// Every English key this module adds, for the translation report.
pub const ENGLISH_KEYS: &[&str] = &[
    UNREADABLE_TEXT,
    NEWER_SESSION_TEXT,
    MISSING_TEXT,
    RELAY_ELSEWHERE_TEXT,
    NO_BASE_HASH_TEXT,
    NOT_SHARED_TEXT,
    RESTARTED_TEXT,
    SAVES_LIVE_TEXT,
    SAVED_LIVE_TEXT,
    NOT_SAVING_TEXT,
    SENDING_TEXT,
];

/// The relay's permission bit for changing the file (OCS numbering: read 1, update 2).
const PERMISSION_UPDATE: u64 = 2;

/// What the launcher handed over (`v` 1). Fields this build does not know are ignored.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionFile {
    /// The Nextcloud server, `https://host[/path]`.
    pub server: String,
    pub file_id: Option<u64>,
    /// The file's path on the server, for the person.
    pub path: Option<String>,
    pub ticket: String,
    /// The relay as the ticket answer gave it: absolute, or a path on the server.
    pub relay_url: String,
    pub room: String,
    pub expires_at: Option<String>,
    pub read_only: bool,
    pub display_name: Option<String>,
    /// The SHA-256 of the room's base file (`base.sha256` of the ticket answer), which the file
    /// argument must be.
    pub base_sha256: Option<[u8; 32]>,
    /// The ticket answer's `saver`: false when no document service saves the room right now.
    /// True when the launcher does not say.
    pub saver: bool,
}

impl std::fmt::Debug for SessionFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionFile")
            .field("server", &self.server)
            .field("file_id", &self.file_id)
            .field("room", &self.room)
            .field("relay_url", &self.relay_url)
            .field("read_only", &self.read_only)
            .field("ticket", &"<hidden>")
            .finish_non_exhaustive()
    }
}

/// Why a session file cannot be used. Its `Display` is the line for the person (an English key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    Unreadable,
    /// A `v` this build does not know (or none).
    Version(Option<u64>),
    Missing(&'static str),
    RelayElsewhere,
    NoBaseHash,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Unreadable => f.write_str(UNREADABLE_TEXT),
            SessionError::Version(_) => f.write_str(NEWER_SESSION_TEXT),
            SessionError::Missing(field) => f.write_str(&MISSING_TEXT.replace("{field}", field)),
            SessionError::RelayElsewhere => f.write_str(RELAY_ELSEWHERE_TEXT),
            SessionError::NoBaseHash => f.write_str(NO_BASE_HASH_TEXT),
        }
    }
}

impl std::error::Error for SessionError {}

/// Reads a session file.
///
/// # Errors
/// [`SessionError`].
pub fn read(path: &Path) -> Result<SessionFile, SessionError> {
    let bytes = std::fs::read(path).map_err(|_| SessionError::Unreadable)?;
    parse(&bytes)
}

/// Parses a session file's bytes.
///
/// # Errors
/// [`SessionError`]: not JSON, another version, or a field a join needs is missing.
pub fn parse(bytes: &[u8]) -> Result<SessionFile, SessionError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| SessionError::Unreadable)?;
    let version = v.get("v").and_then(Value::as_u64);
    if version != Some(SESSION_VERSION) {
        return Err(SessionError::Version(version));
    }
    let text = |key: &str| v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let need = |key: &'static str| text(key).ok_or(SessionError::Missing(key));
    let base_sha256 = v.get("base").and_then(|b| b.get("sha256")).and_then(Value::as_str).and_then(hex32);
    Ok(SessionFile {
        server: need("server")?,
        file_id: v.get("fileId").and_then(Value::as_u64),
        path: text("path"),
        ticket: need("ticket")?,
        relay_url: need("relayUrl")?,
        room: need("room")?,
        expires_at: text("expiresAt"),
        // `permissions` is the ticket answer's own field, taken too in case the launcher copies it.
        read_only: v.get("readOnly").and_then(Value::as_bool).unwrap_or(false) || text("permissions").as_deref() == Some("read"),
        display_name: text("displayName"),
        base_sha256,
        saver: v.get("saver").and_then(Value::as_bool).unwrap_or(true),
    })
}

/// 64 hex digits as 32 bytes.
fn hex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let pair = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// `scheme://authority` of an absolute URL, the scheme lower-cased; `None` for anything else.
fn split_origin(url: &str) -> Option<(String, &str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    (!authority.is_empty() && !authority.contains('@')).then(|| (scheme.to_ascii_lowercase(), authority, path))
}

impl SessionFile {
    /// The relay to connect to, as an absolute `http(s)` URL on the server's own host: a path is
    /// taken on the server, a `ws(s)` URL as its `http(s)` twin. A relay on another host is
    /// refused, so the ticket goes nowhere else (as the web app's same-host rule).
    ///
    /// # Errors
    /// [`SessionError::RelayElsewhere`].
    pub fn relay_endpoint(&self) -> Result<String, SessionError> {
        let (server_scheme, server_host, _) = split_origin(&self.server).ok_or(SessionError::RelayElsewhere)?;
        if !matches!(server_scheme.as_str(), "http" | "https") {
            return Err(SessionError::RelayElsewhere);
        }
        if self.relay_url.starts_with('/') && !self.relay_url.starts_with("//") {
            return Ok(format!("{server_scheme}://{server_host}{}", self.relay_url));
        }
        let (scheme, host, path) = split_origin(&self.relay_url).ok_or(SessionError::RelayElsewhere)?;
        let scheme = match scheme.as_str() {
            "http" | "ws" => "http",
            "https" | "wss" => "https",
            _ => return Err(SessionError::RelayElsewhere),
        };
        if !host.eq_ignore_ascii_case(server_host) {
            return Err(SessionError::RelayElsewhere);
        }
        Ok(format!("{scheme}://{host}{path}"))
    }

    /// The access to join with: read-only when the launcher says so or the ticket lacks the
    /// update bit (the relay enforces it either way).
    #[must_use]
    pub fn access(&self) -> Access {
        if self.read_only || ticket_access(&self.ticket) == Access::Read { Access::Read } else { Access::Edit }
    }

    /// The relay's name for the room: `doc:<rm>` of the ticket, as the relay names it on the
    /// wire; the session's `room` (the ticket answer's) when the ticket can't be read.
    #[must_use]
    pub fn doc(&self) -> String {
        craft_collab::doc_from_token(&self.ticket).unwrap_or_else(|| self.room.clone())
    }

    /// The user id the ticket names.
    #[must_use]
    pub fn uid(&self) -> Option<String> {
        claims(&self.ticket)?.get("uid")?.as_str().filter(|u| !u.is_empty()).map(str::to_owned)
    }
}

/// The claims of a ticket (`payload.signature`, base64url JSON). The relay checks the signature;
/// this only reads what the window needs. (The same reading as the web app's `live_ticket`.)
fn claims(ticket: &str) -> Option<Value> {
    let payload = ticket.split('.').next()?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The access a ticket grants, from its `perm` claim; one that can't be read is left to the relay.
fn ticket_access(ticket: &str) -> Access {
    match claims(ticket).and_then(|c| c.get("perm").and_then(Value::as_u64)) {
        Some(perm) if perm & PERMISSION_UPDATE == 0 => Access::Read,
        _ => Access::Edit,
    }
}

/// What a rewrite of the session file brought.
#[derive(Clone, PartialEq, Eq)]
pub enum Change {
    /// A fresh ticket for the room.
    Ticket(String),
    /// The session moved to another room: this window's link ends ([`RESTARTED_TEXT`]).
    Restarted,
}

impl std::fmt::Debug for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Change::Ticket(_) => f.write_str("Ticket(<hidden>)"),
            Change::Restarted => f.write_str("Restarted"),
        }
    }
}

/// Watches the session file for the launcher's rewrites (a new ticket before the old one
/// expires). [`Watch::check`] looks at the file's modification time and size and reads it only
/// when they changed; a file that is gone, half written or unreadable is looked at again next
/// time.
pub struct Watch {
    path: PathBuf,
    seen: Option<(std::time::SystemTime, u64)>,
    ticket: String,
    room: String,
    restarted: bool,
    saver: bool,
}

impl Watch {
    /// Watches `path`, whose current contents are `now`.
    #[must_use]
    pub fn new(path: &Path, now: &SessionFile) -> Watch {
        Watch { path: path.to_owned(), seen: stamp(path), ticket: now.ticket.clone(), room: now.room.clone(), restarted: false, saver: now.saver }
    }

    /// What changed since the last check, if anything.
    pub fn check(&mut self) -> Option<Change> {
        if self.restarted {
            return None;
        }
        let now = stamp(&self.path)?;
        if self.seen == Some(now) {
            return None;
        }
        let file = match read(&self.path) {
            Ok(f) => f,
            Err(e) => {
                // Not taken as seen: the next check reads it again.
                log::warn!("live session file rewrite not used: {e}");
                return None;
            }
        };
        self.seen = Some(now);
        self.saver = file.saver;
        if file.room != self.room {
            self.restarted = true;
            return Some(Change::Restarted);
        }
        if file.ticket == self.ticket {
            return None;
        }
        self.ticket = file.ticket;
        Some(Change::Ticket(self.ticket.clone()))
    }
}

impl Watch {
    /// Whether the newest session file says a document service saves the room.
    #[must_use]
    pub fn saver(&self) -> bool {
        self.saver
    }
}

fn stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(all(feature = "native-hub", not(target_arch = "wasm32")))]
pub use lazy::LazyHub;

#[cfg(all(feature = "native-hub", not(target_arch = "wasm32")))]
mod lazy {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, mpsc};
    use std::time::Duration;

    use craft_collab::native::NativeHub;
    use craft_collab::{ClientMsg, Hub, ServerMsg, Wake};

    /// The first wait after a connect that failed, and the longest.
    const RETRY_FIRST: Duration = Duration::from_millis(500);
    const RETRY_MAX: Duration = Duration::from_secs(5);

    /// What the hub and its connecting task share: the token the next attempt uses, and whether
    /// the hub was closed (or dropped), which ends the task.
    struct Want {
        token: Mutex<String>,
        closed: AtomicBool,
    }

    /// A native socket hub that connects in the background on `runtime`, so opening one never
    /// blocks a frame (a renewed ticket opens a second link while the window draws, through
    /// [`craft_collab::Handover`]). Until the relay has answered, it is not connected, and what is
    /// sent is dropped as a link that is down drops it (`craft-live` sends its unconfirmed edits
    /// again once joined). A connect that fails is tried again, backing off to five seconds,
    /// with the newest token ([`Hub::set_token`]), until it succeeds or the hub is closed.
    pub struct LazyHub {
        hub: Option<NativeHub>,
        ready: mpsc::Receiver<NativeHub>,
        want: Arc<Want>,
        space: Option<u32>,
        wake: Option<Wake>,
    }

    impl LazyHub {
        /// Starts connecting to `url` with `token`, resuming at `applied`, naming `schema`.
        #[must_use]
        pub fn connect(runtime: &tokio::runtime::Handle, url: &str, token: &str, applied: Option<u64>, schema: &str) -> LazyHub {
            let want = Arc::new(Want { token: Mutex::new(token.to_owned()), closed: AtomicBool::new(false) });
            let (tx, ready) = mpsc::channel();
            let (task_want, url, schema) = (want.clone(), url.to_owned(), schema.to_owned());
            runtime.spawn(async move {
                let mut wait = RETRY_FIRST;
                while !task_want.closed.load(Ordering::SeqCst) {
                    let token = task_want.token.lock().unwrap_or_else(PoisonError::into_inner).clone();
                    match NativeHub::connect_as(&url, &token, applied, Some(&schema)).await {
                        Ok(hub) => {
                            // Dropped at once (closing it) when the hub went meanwhile.
                            let _ = tx.send(hub);
                            return;
                        }
                        Err(e) => {
                            // The error names the relay, never the token (it travels in the auth).
                            log::warn!("deckcraft-live: the relay could not be reached: {e}");
                            tokio::time::sleep(wait).await;
                            wait = (wait * 2).min(RETRY_MAX);
                        }
                    }
                }
            });
            LazyHub { hub: None, ready, want, space: None, wake: None }
        }

        /// Takes the connected hub once the task has it.
        fn arrive(&mut self) {
            if self.hub.is_some() || self.want.closed.load(Ordering::SeqCst) {
                return;
            }
            let Ok(mut hub) = self.ready.try_recv() else { return };
            // A token renewed while connecting: the hub reconnects with it.
            let token = self.want.token.lock().unwrap_or_else(PoisonError::into_inner).clone();
            hub.set_token(token);
            if let Some(space) = self.space {
                hub.set_space(space);
            }
            if let Some(wake) = &self.wake {
                hub.set_wake(wake);
            }
            self.hub = Some(hub);
        }
    }

    impl Hub for LazyHub {
        fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
            self.arrive();
            match self.hub.as_mut() {
                Some(hub) => hub.send(msg),
                None => Ok(()),
            }
        }

        fn poll(&mut self) -> Vec<ServerMsg> {
            self.arrive();
            self.hub.as_mut().map(Hub::poll).unwrap_or_default()
        }

        fn connected(&self) -> bool {
            self.hub.as_ref().is_some_and(Hub::connected)
        }

        fn set_token(&mut self, token: String) {
            *self.want.token.lock().unwrap_or_else(PoisonError::into_inner) = token.clone();
            if let Some(hub) = self.hub.as_mut() {
                hub.set_token(token);
            }
        }

        fn set_space(&mut self, space: u32) {
            self.space = Some(space);
            if let Some(hub) = self.hub.as_mut() {
                hub.set_space(space);
            }
        }

        fn close(&mut self) {
            self.want.closed.store(true, Ordering::SeqCst);
            if let Some(hub) = self.hub.as_mut() {
                hub.close();
            }
            self.hub = None;
        }

        fn set_wake(&mut self, wake: &Wake) {
            self.wake = Some(wake.clone());
            if let Some(hub) = self.hub.as_mut() {
                hub.set_wake(wake);
            }
        }
    }

    impl Drop for LazyHub {
        fn drop(&mut self) {
            self.want.closed.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn file(extra: &str) -> Vec<u8> {
        format!(
            r#"{{"v":1,"server":"https://cloud.example","fileId":7,"path":"/Talks/a.pptx","ticket":"t.s","relayUrl":"/craft","room":"inst:7","expiresAt":"2026-11-27T12:00:00Z","readOnly":false,"displayName":"A"{extra}}}"#
        )
        .into_bytes()
    }

    fn with_relay(relay: &str) -> SessionFile {
        SessionFile { relay_url: relay.to_owned(), ..parse(&file("")).unwrap() }
    }

    #[test]
    fn unknown_fields_are_ignored_and_an_unknown_version_is_refused() {
        assert!(parse(&file(r#","later":{"x":1}"#)).is_ok());
        let newer = String::from_utf8(file("")).unwrap().replace(r#""v":1"#, r#""v":2"#);
        assert_eq!(parse(newer.as_bytes()), Err(SessionError::Version(Some(2))));
    }

    #[test]
    fn the_base_checksum_is_read_from_the_ticket_answers_base() {
        let hex = "ab".repeat(32);
        let s = parse(&file(&format!(r#","base":{{"url":"/x","sha256":"{hex}"}}"#))).unwrap();
        assert_eq!(s.base_sha256, Some([0xab; 32]));
        assert_eq!(parse(&file(r#","base":{"sha256":"abc"}"#)).unwrap().base_sha256, None);
    }

    #[test]
    fn the_relay_must_be_on_the_servers_host() {
        assert_eq!(with_relay("/craft").relay_endpoint().unwrap(), "https://cloud.example/craft");
        assert_eq!(with_relay("wss://CLOUD.example/craft").relay_endpoint().unwrap(), "https://CLOUD.example/craft");
        for elsewhere in ["https://evil.example/craft", "//evil.example/craft", "https://cloud.example@evil.example/", "ftp://cloud.example/"] {
            assert_eq!(with_relay(elsewhere).relay_endpoint(), Err(SessionError::RelayElsewhere), "{elsewhere}");
        }
    }

    #[test]
    fn a_read_only_session_or_ticket_joins_read_only() {
        let ticket = |perm: u64| format!("{}.sig", URL_SAFE_NO_PAD.encode(format!(r#"{{"uid":"ann","perm":{perm}}}"#)));
        let s = SessionFile { ticket: ticket(3), ..parse(&file("")).unwrap() };
        assert_eq!(s.access(), Access::Edit);
        assert_eq!(s.uid().as_deref(), Some("ann"));
        assert_eq!(SessionFile { read_only: true, ..s.clone() }.access(), Access::Read);
        assert_eq!(SessionFile { ticket: ticket(1), ..s }.access(), Access::Read);
        assert_eq!(parse(&file(r#","permissions":"read""#)).unwrap().access(), Access::Read);
    }

    /// The launcher's rewrites, as a temporary file renamed over the session file: a fresh
    /// ticket is told once, a room without a saver is noted, another room ends the session.
    #[test]
    fn the_watch_tells_each_rewrite_once() {
        let dir = std::env::temp_dir().join(format!("deck-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("live.json");
        let write = |extra: &str| {
            let tmp = dir.join("live.tmp");
            std::fs::write(&tmp, file(extra)).unwrap();
            std::fs::rename(&tmp, &path).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1100));
        };
        write("");
        let mut watch = Watch::new(&path, &read(&path).unwrap());
        assert_eq!(watch.check(), None);
        let renewed = String::from_utf8(file(r#","saver":false"#)).unwrap().replace(r#""ticket":"t.s""#, r#""ticket":"t2.s2""#);
        let tmp = dir.join("live.tmp");
        std::fs::write(&tmp, renewed).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert_eq!(watch.check(), Some(Change::Ticket("t2.s2".into())));
        assert!(!watch.saver());
        assert_eq!(watch.check(), None);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let moved = String::from_utf8(file("")).unwrap().replace(r#""room":"inst:7""#, r#""room":"inst:7:new""#);
        std::fs::write(&tmp, moved).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert_eq!(watch.check(), Some(Change::Restarted));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ticket_never_shows_in_debug_output() {
        let s = SessionFile { ticket: "SECRET-TICKET".into(), ..parse(&file("")).unwrap() };
        assert!(!format!("{s:?}").contains("SECRET"));
        assert!(!format!("{:?}", Change::Ticket("SECRET-TICKET".into())).contains("SECRET"));
    }
}
