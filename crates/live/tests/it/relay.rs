//! Native clients converging through a real craft-collab relay (L8). Ignored by default: they
//! need a relay and its secret, `CRAFT_TEST_RELAY` (for example `http://localhost:9444`, or the
//! dev stack's `http://<lan address>:8035/craft` through Apache) and `CRAFT_TEST_SECRET`, and run
//! with `--features native-hub -- --ignored relay`.
//!
//! - `relay_restart_resumes` also needs `CRAFT_TEST_RESTART_CMD`, a shell command that restarts
//!   that relay and keeps its Valkey.
//! - `a_lift_into_lost_history_keeps_every_edit` needs `CRAFT_TEST_LIFT_CMD`, a shell command
//!   that stops the relay, restarts its Valkey (keeping its data, so a new Valkey run) and starts
//!   the relay again: the room is rebuilt under a Valkey run it does not know, so the relay raises
//!   the room's sequence past every number it handed out (the lift) and the members come back
//!   through the lost history.
//! - `load_five_clients` takes `CRAFT_TEST_LOAD_SECS` (default 60).
//!
//! Neither the secret nor a ticket is ever printed: failures name clients and statuses only.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_collab::native::NativeHub;
use craft_collab::{Hub, Replica};
use craft_live::{Access, LiveStatus, SERVICE_UID, SaveEvent};
use deckcraft_engine::{PointerKind, Session};
use deckcraft_live::{BaseFile, DeckLive, deck_from_state, schema};
use deckcraft_model::{Presentation, ShapeId};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::support::{NAME, Rng, add_box, deck_bytes, ev, find, shape_of};
use crate::trace::{Drag, act, deck_of};

/// Text boxes in the base deck, one per client of the load test.
const BOXES: usize = 5;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A join ticket as the relay's signer makes it: `base64url(claims).base64url(HMAC-SHA256)`.
fn ticket(secret: &str, room: &str, uid: &str, pid: &str) -> String {
    let claims = json!({"rm": room, "uid": uid, "dsp": uid, "perm": 3, "exp": now() + 900, "pid": pid});
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload.as_bytes());
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

/// A room no earlier run has used.
fn fresh_room() -> String {
    format!("it-deck-{}", SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0))
}

/// The base deck: `BOXES` text boxes `box0`… on the first slide, and two more slides.
fn base_bytes() -> Vec<u8> {
    deck_bytes(|s| {
        for k in 0..BOXES {
            add_box(s, &format!("box{k}"), [40.0 + 130.0 * k as f64, 60.0, 120.0, 60.0]);
        }
        s.execute("slide.new", &json!({"layout": "titleAndContent", "title": "Two", "body": "second"})).expect("slide");
        s.execute("slide.new", &json!({"layout": "titleOnly", "title": "Three"})).expect("slide");
        s.execute("slide.go", &json!({"index": 0})).expect("go");
    })
}

struct Client {
    name: String,
    s: Session,
    live: DeckLive<NativeHub>,
    drag: Option<Drag>,
    /// The relay told this client the room's history was lost (a lift, or a restart).
    history_lost: bool,
    /// The unmerged copy handed over, if any.
    copy: Option<Vec<u8>>,
}

impl Client {
    async fn join(relay: &str, secret: &str, room: &str, uid: &str, file: &[u8]) -> Client {
        let token = ticket(secret, room, uid, &format!("{uid}-{}", now()));
        let schema = schema();
        let hub = NativeHub::connect_as(relay, &token, None, Some(&schema)).await.unwrap_or_else(|_| panic!("{uid}: the relay connection failed"));
        let mut s = Session::new();
        let base = BaseFile { bytes: file, name: NAME, expected_sha256: None };
        let live =
            DeckLive::start(hub, &format!("doc:{room}"), &base, Access::Edit, uid, &mut s).unwrap_or_else(|e| panic!("{uid}: start failed: {e:?}"));
        Client { name: uid.to_owned(), s, live, drag: None, history_lost: false, copy: None }
    }

    fn pump(&mut self) {
        self.live.pump(&mut self.s);
        // Notices are for a user; the tests read the status instead.
        let _ = self.live.take_notices();
        if self.live.core_mut().take_history_lost() {
            self.history_lost = true;
        }
        if let Some((_, bytes)) = self.live.take_unmerged() {
            self.copy = Some(bytes);
        }
    }

    fn is_live(&self) -> bool {
        matches!(self.live.status(), LiveStatus::Live { .. }) && self.live.editable()
    }

    fn run(&mut self, id: &str, params: Value) -> bool {
        self.live.run(&mut self.s, id, &params).is_ok()
    }

    fn deck(&self) -> Arc<Presentation> {
        deck_of(&self.live, &self.s)
    }

    fn text(&self, id: ShapeId) -> String {
        shape_of(&self.deck(), id).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default()
    }

    /// Types `text` at the end of `id`.
    fn type_at_end(&mut self, id: ShapeId, text: &str) -> bool {
        self.run("text.edit", json!({"id": id.0, "end": true})) && self.run("text.insert", json!({"text": text}))
    }

    fn state_len(&self) -> usize {
        self.live.replica().map_or(0, |r| r.state().len())
    }

    /// Drops an open drag where it is.
    fn drop_drag(&mut self) {
        if let Some(d) = self.drag.take() {
            let _ = self.s.pointer(ev(PointerKind::Up, d.x, d.y));
        }
    }

    /// The saver's checkpoint, ordered by the room (one at a time, as craft-live asks).
    async fn checkpoint(cs: &mut [Client], saver: usize) {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if cs[saver].live.core_mut().checkpoint().is_ok() {
                break;
            }
            assert!(Instant::now() < end, "the saver never could checkpoint:\n{}", state(cs));
            pump_all(cs);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        loop {
            pump_all(cs);
            let events = cs[saver].live.core_mut().take_save_events();
            if events.iter().any(|e| matches!(e, SaveEvent::Checkpointed { .. })) {
                return;
            }
            assert!(events.is_empty(), "the checkpoint did not land: {events:?}");
            assert!(Instant::now() < end, "the checkpoint never was ordered:\n{}", state(cs));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn pump_all(cs: &mut [Client]) {
    for c in cs.iter_mut() {
        c.pump();
    }
}

fn state(cs: &[Client]) -> String {
    cs.iter()
        .map(|c| {
            format!(
                "{}: {:?}, hub connected {}, editable {}, {} slides, history lost {}, copy {}",
                c.name,
                c.live.status(),
                c.live.core().hub().connected(),
                c.live.editable(),
                c.deck().slides.len(),
                c.history_lost,
                c.copy.is_some()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pump every client until `done` holds, or fail after `secs` with where each one stands.
async fn until(cs: &mut [Client], secs: u64, what: &str, done: impl Fn(&[Client]) -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        pump_all(cs);
        if done(cs) {
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}:\n{}", state(cs));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn all_live(cs: &[Client]) -> bool {
    cs.iter().all(Client::is_live)
}

fn equal(cs: &[Client]) -> bool {
    let first = cs[0].deck();
    cs.iter().skip(1).all(|c| *c.deck() == *first)
}

/// Everyone is Live and equal, and stays so for a while: nothing is still on its way. Then each
/// one also equals the base plus its own replica's state.
async fn converge(cs: &mut [Client], what: &str) {
    let quiet = Duration::from_millis(800);
    let mut since: Option<Instant> = None;
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        pump_all(cs);
        if all_live(cs) && equal(cs) && cs.iter().all(|c| !c.live.holding()) {
            let t = *since.get_or_insert_with(Instant::now);
            if t.elapsed() >= quiet {
                break;
            }
        } else {
            since = None;
        }
        assert!(Instant::now() < end, "{what}: no convergence:\n{}", state(cs));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for c in cs.iter() {
        let st = c.live.replica().unwrap().state();
        let rebuilt = deck_from_state(c.live.base(), &st).unwrap();
        assert!(*c.deck() == rebuilt, "{what}: {} differs from its replica's state", c.name);
    }
}

fn setup() -> (String, String, String, Vec<u8>) {
    (env("CRAFT_TEST_RELAY"), env("CRAFT_TEST_SECRET"), fresh_room(), base_bytes())
}

async fn three(relay: &str, secret: &str, room: &str, file: &[u8]) -> Vec<Client> {
    vec![
        Client::join(relay, secret, room, "alice", file).await,
        Client::join(relay, secret, room, "bob", file).await,
        Client::join(relay, secret, room, "carol", file).await,
    ]
}

/// The trace's step mix through the relay, then a late joiner on the relay's replay, then a
/// saver's checkpoint and a joiner that starts from it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn three_native_clients_converge_through_relay() {
    let (relay, secret, room, file) = setup();
    let mut cs = three(&relay, &secret, &room, &file).await;
    until(&mut cs, 20, "every client Live", all_live).await;
    let base = cs[0].deck();

    let mut rng = Rng(7);
    let (mut ran, mut refused) = (0usize, 0usize);
    for step in 0..300 {
        let k = rng.below(cs.len());
        let c = &mut cs[k];
        let (_, ok) = act(&mut c.live, &mut c.s, &mut rng, &mut c.drag);
        if ok {
            ran += 1
        } else {
            refused += 1
        }
        if step % 3 == 0 {
            pump_all(&mut cs);
        }
        if step % 10 == 0 {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
    for c in &mut cs {
        c.drop_drag();
    }
    eprintln!("three clients: {ran} steps ran, {refused} refused");
    converge(&mut cs, "after the random steps").await;
    assert!(*cs[0].deck() != *base, "the steps landed");

    // A late joiner reaches the same deck from the relay's catch-up.
    cs.push(Client::join(&relay, &secret, &room, "dave", &file).await);
    until(&mut cs, 20, "the late joiner Live", all_live).await;
    converge(&mut cs, "with the late joiner").await;

    // The saver checkpoints; a joiner after it starts from the checkpoint.
    cs.push(Client::join(&relay, &secret, &room, SERVICE_UID, &file).await);
    until(&mut cs, 20, "the saver Live", all_live).await;
    let saver = cs.len() - 1;
    Client::checkpoint(&mut cs, saver).await;
    for _ in 0..20 {
        let k = rng.below(3);
        let c = &mut cs[k];
        let _ = act(&mut c.live, &mut c.s, &mut rng, &mut c.drag);
        c.drop_drag();
    }
    cs.push(Client::join(&relay, &secret, &room, "erin", &file).await);
    until(&mut cs, 20, "the checkpoint joiner Live", all_live).await;
    converge(&mut cs, "with the checkpoint joiner").await;
}

/// The boxes `box0`…, by index. Keystrokes are typed as `q`, which no label holds.
fn boxes(c: &Client) -> Vec<ShapeId> {
    (0..BOXES).map(|k| find(&c.deck(), &format!("box{k}"))).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn relay_restart_resumes() {
    let (relay, secret, room, file) = setup();
    let restart = env("CRAFT_TEST_RESTART_CMD");
    let mut cs = three(&relay, &secret, &room, &file).await;
    until(&mut cs, 20, "every client Live", all_live).await;
    let ids = boxes(&cs[0]);
    for (k, c) in cs.iter_mut().enumerate() {
        assert!(c.type_at_end(ids[k], &format!("<pre{k}>")), "client {k} types before the restart");
        assert!(c.run("shape.move", json!({"ids": [ids[k].0], "dy": 20.0})), "client {k} moves before the restart");
    }
    converge(&mut cs, "before the restart").await;

    // Restart the relay while the clients keep typing into their own boxes.
    let child = std::thread::spawn(move || std::process::Command::new("sh").arg("-c").arg(restart).output());
    let mut typed = [0usize; 3];
    while !child.is_finished() {
        for (k, c) in cs.iter_mut().enumerate() {
            if c.type_at_end(ids[k], "q") {
                typed[k] += 1;
            }
        }
        pump_all(&mut cs);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let out = child.join().unwrap().unwrap();
    assert!(out.status.success(), "the restart command failed");
    eprintln!("during the restart: typed {typed:?}\nafter the restart:\n{}", state(&cs));
    for c in &cs {
        assert!(!matches!(c.live.status(), LiveStatus::Broken(_)), "{} is Broken after the restart: {:?}", c.name, c.live.status());
    }
    until(&mut cs, 60, "every client Live again", all_live).await;
    converge(&mut cs, "after the restart").await;
    for k in 0..3 {
        let t = cs[0].text(ids[k]);
        assert!(t.contains(&format!("<pre{k}>")), "client {k}'s mark survived the restart: {t:?}");
        assert_eq!(t.matches('q').count(), typed[k], "client {k}'s keystrokes during the restart all arrived once: {t:?}");
    }
    for (k, c) in cs.iter_mut().enumerate() {
        assert!(c.type_at_end(ids[k], &format!("[post{k}]")), "client {k} can edit again");
    }
    converge(&mut cs, "after editing again").await;
}

/// The relay's counter cannot be trusted any more (its Valkey restarted while the relay was
/// down): the relay lifts the room's sequence past every number it may have handed out, and
/// everybody comes back through the lost history. No edit is lost:
/// each keystroke is in the room or in its typist's unmerged copy. The saver checkpoints the
/// room again, and a joiner after the lift starts from that checkpoint.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay and CRAFT_TEST_LIFT_CMD"]
async fn a_lift_into_lost_history_keeps_every_edit() {
    let (relay, secret, room, file) = setup();
    let lift = env("CRAFT_TEST_LIFT_CMD");
    let mut cs = three(&relay, &secret, &room, &file).await;
    cs.push(Client::join(&relay, &secret, &room, SERVICE_UID, &file).await);
    let saver = 3;
    until(&mut cs, 20, "every client Live", all_live).await;
    let ids = boxes(&cs[0]);
    for (k, c) in cs.iter_mut().take(3).enumerate() {
        assert!(c.type_at_end(ids[k], &format!("<pre{k}>")), "client {k} types before the lift");
    }
    converge(&mut cs, "before the lift").await;

    let child = std::thread::spawn(move || std::process::Command::new("sh").arg("-c").arg(lift).output());
    let mut typed = [0usize; 3];
    while !child.is_finished() {
        for (k, c) in cs.iter_mut().take(3).enumerate() {
            if c.type_at_end(ids[k], "q") {
                typed[k] += 1;
            }
        }
        pump_all(&mut cs);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let out = child.join().unwrap().unwrap();
    assert!(out.status.success(), "the lift command failed");
    // Keep typing a little after the relay is back, so edits cross the rejoin itself.
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        for (k, c) in cs.iter_mut().take(3).enumerate() {
            if c.type_at_end(ids[k], "q") {
                typed[k] += 1;
            }
        }
        pump_all(&mut cs);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Settle: every client Live again, or given up (Broken) with its copy.
    let settled = |cs: &[Client]| cs.iter().all(|c| c.is_live() || (matches!(c.live.status(), LiveStatus::Broken(_)) && c.copy.is_some()));
    until(&mut cs, 60, "every client Live again or given up with its copy", settled).await;
    eprintln!("after the lift: typed {typed:?}\n{}", state(&cs));
    assert!(cs.iter().any(|c| c.history_lost), "the members came back through the lost history");
    assert!(cs[saver].is_live(), "the saver carries on");
    // The saver checkpoints the room that lost its history, as a document service does.
    let mut live: Vec<Client> = Vec::new();
    let mut gone: Vec<Client> = Vec::new();
    for c in cs {
        if c.is_live() { live.push(c) } else { gone.push(c) }
    }
    converge(&mut live, "the members still live after the lift").await;
    let saver = live.iter().position(|c| c.name == SERVICE_UID).unwrap();
    for (k, typist) in ["alice", "bob", "carol"].iter().enumerate() {
        let room_text = live[0].text(ids[k]);
        let room_x = room_text.matches('q').count();
        assert!(room_text.contains(&format!("<pre{k}>")), "{typist}'s mark from before the lift survived: {room_text:?}");
        if room_x == typed[k] {
            continue;
        }
        let copy = gone.iter().find(|c| c.name == *typist).and_then(|c| c.copy.clone());
        let copy = copy.unwrap_or_else(|| panic!("{typist}: {room_x} of {} keystrokes reached the room and no copy holds the rest", typed[k]));
        let deck = deckcraft_engine::cmd::file::open_presentation(NAME, &copy).unwrap();
        let kept = shape_of(&deck, ids[k]).and_then(|s| s.text).map(|t| t.text()).unwrap_or_default();
        assert_eq!(kept.matches('q').count(), typed[k], "{typist}'s copy holds every keystroke");
    }
    Client::checkpoint(&mut live, saver).await;
    live.push(Client::join(&relay, &secret, &room, "dave", &file).await);
    until(&mut live, 30, "the joiner after the lift Live", all_live).await;
    converge(&mut live, "with the joiner after the lift").await;
    eprintln!("lift: {} members live, {} given up with a copy", live.len(), gone.len());
}

/// The load check: 5 clients, each typing 8 characters a second into its own text box, for 60 s
/// (`CRAFT_TEST_LOAD_SECS`). Each client times every other client's keystrokes, from just before
/// the typist's `run` until the keystroke is in its own deck (remote apply). The target is a 95th
/// percentile under 200 ms; Ruling D22: it is recorded, not asserted. All five clients pump on
/// one thread, so a sample also holds up to one lap of that loop (printed): the figure is an
/// upper bound. Also printed: the replica's state size at the end.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn load_five_clients() {
    const N: usize = BOXES;
    const PERIOD: Duration = Duration::from_millis(125);
    let secs: u64 = std::env::var("CRAFT_TEST_LOAD_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let (relay, secret, room, file) = setup();
    let mut cs = Vec::new();
    for k in 0..N {
        cs.push(Client::join(&relay, &secret, &room, &format!("typist{k}"), &file).await);
    }
    until(&mut cs, 30, "every client Live", all_live).await;
    let ids = boxes(&cs[0]);
    let start_len: Vec<usize> = ids.iter().map(|id| cs[0].text(*id).chars().count()).collect();

    let keystrokes = usize::try_from(secs * 8).unwrap_or(usize::MAX);
    // sent[k][i]: when typist k started its keystroke i.
    let mut sent: Vec<Vec<Instant>> = (0..N).map(|_| Vec::with_capacity(keystrokes)).collect();
    // seen[j][k]: how many of typist k's keystrokes client j has been timed on.
    let mut seen = [[0usize; N]; N];
    let mut ms = Vec::with_capacity(N * (N - 1) * keystrokes);
    let mut refused = 0usize;
    let mut slowest_lap = Duration::ZERO;
    let start = Instant::now() + Duration::from_millis(200);
    let give_up = start + Duration::from_secs(secs + 60);
    loop {
        let lap = Instant::now();
        for k in 0..N {
            // Typists spread over the period, so not all five type in the same instant.
            let due = start + PERIOD * u32::try_from(sent[k].len()).unwrap_or(u32::MAX) + Duration::from_millis(25 * k as u64);
            if sent[k].len() < keystrokes && Instant::now() >= due {
                let t = Instant::now();
                if cs[k].type_at_end(ids[k], "q") {
                    sent[k].push(t);
                } else {
                    refused += 1;
                }
            }
        }
        pump_all(&mut cs);
        let now = Instant::now();
        for j in 0..N {
            for k in (0..N).filter(|&k| k != j) {
                let have = cs[j].text(ids[k]).chars().count().saturating_sub(start_len[k]).min(sent[k].len());
                while seen[j][k] < have {
                    ms.push((now - sent[k][seen[j][k]]).as_secs_f64() * 1000.0);
                    seen[j][k] += 1;
                }
            }
        }
        slowest_lap = slowest_lap.max(lap.elapsed());
        let typed = sent.iter().all(|s| s.len() == keystrokes);
        if typed && (0..N).all(|j| (0..N).all(|k| j == k || seen[j][k] == keystrokes)) {
            break;
        }
        assert!(Instant::now() < give_up, "not every keystroke arrived:\n{}", state(&cs));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    converge(&mut cs, "after the load").await;
    for (k, id) in ids.iter().enumerate() {
        assert_eq!(cs[0].text(*id).matches('q').count(), keystrokes, "typist {k}'s keystrokes each landed once");
    }
    ms.sort_by(f64::total_cmp);
    assert!(!ms.is_empty(), "remote applies were timed");
    let at = |p: f64| ms[((ms.len() as f64 * p).ceil() as usize).saturating_sub(1).min(ms.len() - 1)];
    let sizes: Vec<usize> = cs.iter().map(Client::state_len).collect();
    eprintln!(
        "load: {N} clients x {keystrokes} keystrokes over {secs} s; {} remote applies: min {:.1} ms, p50 {:.1} ms, p95 {:.1} ms (target 200 ms, recorded not asserted), p99 {:.1} ms, max {:.1} ms; refused {refused}; slowest pump lap {:.1} ms",
        ms.len(),
        ms[0],
        at(0.5),
        at(0.95),
        at(0.99),
        ms[ms.len() - 1],
        slowest_lap.as_secs_f64() * 1000.0
    );
    eprintln!("load: replica state size {} bytes (per client {sizes:?}); base file {} bytes", sizes.iter().max().copied().unwrap_or(0), file.len());
}
