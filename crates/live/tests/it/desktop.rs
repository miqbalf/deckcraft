//! The desktop app's join (`desktop`): a session file as the launcher writes it, the native hub
//! that connects in the background, and the launcher's ticket refresh, against a real relay
//! beside a client that joins as the web app does (its own socket, the base checked against the
//! room's checksum, the renewing hub). The relay tests are ignored by default, as in `relay`:
//! they need `CRAFT_TEST_RELAY` and `CRAFT_TEST_SECRET`. No ticket or secret is printed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_collab::native::NativeHub;
use craft_collab::{ClientMsg, Handover, Hub, ServerMsg, Wake};
use craft_live::{Access, LiveStatus};
use deckcraft_engine::Session;
use deckcraft_live::desktop::{self, Change, LazyHub, Watch};
use deckcraft_live::{BaseFile, DeckLive, LiveFail, schema};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

use crate::support::{NAME, add_box, deck_bytes, find, shape_of};
use crate::trace::deck_of;
use deckcraft_model::ShapeId;

/// A boxed hub, so the web-like and the desktop client have one type.
struct Dyn(Box<dyn Hub>);

impl Hub for Dyn {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.0.send(msg)
    }
    fn poll(&mut self) -> Vec<ServerMsg> {
        self.0.poll()
    }
    fn connected(&self) -> bool {
        self.0.connected()
    }
    fn set_token(&mut self, token: String) {
        self.0.set_token(token);
    }
    fn set_space(&mut self, space: u32) {
        self.0.set_space(space);
    }
    fn close(&mut self) {
        self.0.close();
    }
    fn set_wake(&mut self, wake: &Wake) {
        self.0.set_wake(wake);
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A join ticket as the Nextcloud app signs it, valid for `secs`.
fn ticket(secret: &str, room: &str, uid: &str, secs: u64) -> String {
    let pid = format!("{uid}~{}", SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0));
    let claims = json!({"rm": room, "uid": uid, "dsp": uid, "perm": 3, "exp": now() + secs, "pid": pid});
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload.as_bytes());
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn base_bytes() -> Vec<u8> {
    deck_bytes(|s| {
        add_box(s, "web", [40.0, 60.0, 160.0, 60.0]);
        add_box(s, "desk", [240.0, 60.0, 160.0, 60.0]);
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A folder of its own for the session file and the working copy.
fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deck-desktop-{}", SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes the session file as the launcher does: a temporary file, then a rename over it.
fn write_session(path: &Path, server: &str, relay: &str, room: &str, ticket: &str, sha: &str) {
    let body = json!({
        "v": 1, "server": server, "fileId": 7, "path": "/Talks/Deck.pptx", "ticket": ticket,
        "relayUrl": relay, "room": room, "expiresAt": "2026-11-27T12:00:00Z", "readOnly": false,
        "displayName": "Desk", "base": {"url": "/apps/craft_office/live/base/x", "sha256": sha},
        "later": "a field this build does not know"
    });
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&body).unwrap()).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// The desktop app's start, as `apps/deckcraft` does it: the session file, the relay on the
/// server, the background hub inside a renewing one, and the base checked against the session's
/// checksum.
fn desktop_join(session: &Path, file: &Path, s: &mut Session) -> Result<DeckLive<Dyn>, LiveFail> {
    let sf = desktop::read(session).unwrap();
    let relay = sf.relay_endpoint().unwrap();
    let rt = tokio::runtime::Handle::current();
    let hub = LazyHub::connect(&rt, &relay, &sf.ticket, None, &schema());
    let hub = Handover::new(hub, move |r: &craft_collab::Reopen<'_>| Ok(LazyHub::connect(&rt, &relay, r.ticket, r.applied, &schema())));
    let bytes = std::fs::read(file).unwrap();
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: sf.base_sha256 };
    let uid = sf.uid().unwrap_or_else(|| "desk".to_owned());
    DeckLive::start(Dyn(Box::new(hub)), &sf.doc(), &base, sf.access(), &uid, s)
}

struct Peer {
    s: Session,
    live: DeckLive<Dyn>,
}

impl Peer {
    fn live(&self) -> bool {
        matches!(self.live.status(), LiveStatus::Live { .. }) && self.live.editable()
    }
    fn text(&self, id: ShapeId) -> String {
        shape_of(&deck_of(&self.live, &self.s), id).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default()
    }
    fn type_into(&mut self, id: ShapeId, text: &str) {
        assert!(self.live.run(&mut self.s, "text.edit", &json!({"id": id.0, "end": true})).is_ok());
        assert!(self.live.run(&mut self.s, "text.insert", &json!({"text": text})).is_ok());
    }
}

fn pump(ps: &mut [&mut Peer]) {
    for p in ps.iter_mut() {
        p.live.pump(&mut p.s);
        let _ = p.live.take_notices();
    }
}

/// Pumps both until `done`, failing after `secs`; `steady` is told each pump whether the desktop
/// peer is live.
async fn until(web: &mut Peer, desk: &mut Peer, secs: u64, what: &str, mut steady: impl FnMut(bool), done: impl Fn(&Peer, &Peer) -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        pump(&mut [web, desk]);
        steady(desk.live());
        if done(web, desk) {
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}: web {:?}, desktop {:?}", web.live.status(), desk.live.status());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn same(web: &Peer, desk: &Peer) -> bool {
    *deck_of(&web.live, &web.s) == *deck_of(&desk.live, &desk.s)
}

/// A desktop window joined from a launcher's session file converges with a web client; the
/// launcher's rewritten ticket reaches the hub through the watch, and the link carries on past
/// the first ticket's expiry without dropping (the renewed ticket opened a second link).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn a_desktop_window_from_a_session_file_coedits_with_a_web_client() {
    let (relay, secret) = (env("CRAFT_TEST_RELAY"), env("CRAFT_TEST_SECRET"));
    let rm = format!("it-desk-{}", SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let bytes = base_bytes();
    let sha = deckcraft_live::seed::base_sha256(&bytes);

    // The web client: its own socket, the base checked against the room's checksum.
    let web_hub = NativeHub::connect_as(&relay, &ticket(&secret, &rm, "web", 900), None, Some(&schema())).await.expect("the web client connects");
    let web_hub = Handover::new(web_hub, |_: &craft_collab::Reopen<'_>| Err(craft_collab::Error::Hub("no renewal in this test".into())));
    let mut web_s = Session::new();
    let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: Some(sha) };
    let web_live =
        DeckLive::start(Dyn(Box::new(web_hub)), &format!("doc:{rm}"), &base, Access::Edit, "web", &mut web_s).expect("the web client starts");
    let mut web = Peer { s: web_s, live: web_live };

    // The desktop window, from the launcher's hand-off. Its first ticket expires soon.
    let dir = scratch();
    let (session, file) = (dir.join("live.json"), dir.join(NAME));
    std::fs::write(&file, &bytes).unwrap();
    let first_expiry = Instant::now() + Duration::from_secs(20);
    write_session(&session, &relay, &relay, &rm, &ticket(&secret, &rm, "desk", 20), &hex(&sha));
    let mut desk_s = Session::new();
    let desk_live = desktop_join(&session, &file, &mut desk_s).expect("the desktop window starts");
    let mut desk = Peer { s: desk_s, live: desk_live };
    let mut watch = Watch::new(&session, &desktop::read(&session).unwrap());

    until(&mut web, &mut desk, 30, "both live", |_| {}, |w, d| w.live() && d.live()).await;
    let start = deck_of(&web.live, &web.s);
    let (web_box, desk_box) = (find(&start, "web"), find(&start, "desk"));
    web.type_into(web_box, " from the browser");
    desk.type_into(desk_box, " from the desktop");
    until(
        &mut web,
        &mut desk,
        20,
        "the first edits on both",
        |_| {},
        |w, d| same(w, d) && w.text(desk_box).contains("from the desktop") && d.text(web_box).contains("from the browser"),
    )
    .await;

    // The launcher renews the ticket: the watch sees the rewrite, and the hub takes it.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    write_session(&session, &relay, &relay, &rm, &ticket(&secret, &rm, "desk", 900), &hex(&sha));
    let change = watch.check().expect("the rewrite is seen");
    let Change::Ticket(fresh) = change else { panic!("the rewrite moved the room") };
    desk.live.core_mut().hub_mut().set_token(fresh);
    assert!(watch.check().is_none(), "one rewrite is told once");

    // Past the first ticket's expiry, the desktop link never left Live.
    let mut dropped = false;
    let wait = first_expiry.saturating_duration_since(Instant::now()).as_secs() + 8;
    until(
        &mut web,
        &mut desk,
        wait + 5,
        "the first ticket's expiry",
        |live| dropped |= !live,
        |_, _| Instant::now() > first_expiry + Duration::from_secs(8),
    )
    .await;
    assert!(!dropped, "the desktop link dropped at the first ticket's expiry: {:?}", desk.live.status());

    desk.type_into(desk_box, " after renewal");
    web.type_into(web_box, " again");
    until(
        &mut web,
        &mut desk,
        20,
        "the edits after renewal on both",
        |_| {},
        |w, d| same(w, d) && w.text(desk_box).contains("after renewal") && d.text(web_box).contains("again"),
    )
    .await;

    // The launcher moved the session to another room: the watch says the window's link ends.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    write_session(&session, &relay, &relay, "another-room", &ticket(&secret, "another-room", "desk", 900), &hex(&sha));
    assert_eq!(watch.check(), Some(Change::Restarted));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A working copy that is not the room's base (the file changed on the server since the room
/// opened) is refused before anything is shared, and the hub connects no further than it got.
#[tokio::test(flavor = "multi_thread")]
async fn a_working_copy_that_is_not_the_rooms_base_does_not_join() {
    let dir = scratch();
    let (session, file) = (dir.join("live.json"), dir.join(NAME));
    let bytes = base_bytes();
    std::fs::write(&file, &bytes).unwrap();
    let other = hex(&deckcraft_live::seed::base_sha256(b"another file"));
    // Nothing listens on port 9 of this machine: the hub keeps trying until it is dropped.
    write_session(&session, "http://127.0.0.1:9", "/craft", "inst:7", "e30.sig", &other);
    let mut s = Session::new();
    assert!(matches!(desktop_join(&session, &file, &mut s), Err(LiveFail::BaseMismatch)));
    assert!(s.documents().is_empty(), "nothing opened for a base that is not the room's");
    let _ = std::fs::remove_dir_all(&dir);
}
