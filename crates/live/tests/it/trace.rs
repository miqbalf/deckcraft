//! Long seeded traces (L8): two to eight peers typing, pressing Enter and Backspace, formatting,
//! moving, restyling, inserting, duplicating, deleting, reordering, changing the z-order and the
//! notes, undoing and redoing, dragging while remote changes are held, with link drops and
//! rejoins (some between two pumps) and a joiner that starts from a checkpoint, all in a seeded
//! delivery order. At the end every peer holds the same deck, and that deck is the base plus each
//! replica's state (`deck_from_state`).
//!
//! The steps ([`act`]) are generic over the hub, so the relay tests drive the same mix.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use craft_collab::{DOC_SCOPE, Hub, Replica, ServerMsg, update_to_wire};
use craft_live::testing::ROOM;
use craft_live::{Access, LiveStatus};
use deckcraft_engine::{Edited, PointerKind, Session};
use deckcraft_live::{DeckEdit, DeckLive, deck_from_state};
use deckcraft_model::{Presentation, ShapeId, Xfrm};
use serde_json::{Value, json};

use crate::support::{NAME, P, Rng, Room, add_box, basic, deck_bytes, ev, seeds_or, space_of};

/// Steps each peer takes.
const STEPS: usize = 300;
/// The peer counts, by seed: seeds 1 to 7 cover 2 to 8 peers.
const PEERS: [usize; 7] = [2, 8, 3, 6, 4, 7, 5];
/// The default seeds: 2, 8 and 3 peers, about a minute on udinpc (review minors M2, F3). The
/// long run, every count from 2 to 8, is `DECKCRAFT_LIVE_SEEDS=7` or more.
const DEFAULT_SEEDS: [u64; 3] = [1, 2, 3];

/// What one step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Type,
    Enter,
    Backspace,
    Format,
    Move,
    Resize,
    Restyle,
    Insert,
    Duplicate,
    Delete,
    Reorder,
    ZOrder,
    Notes,
    Undo,
    Redo,
    DragStart,
    DragEnd,
}

/// Every kind, so a trace can show it ran each one.
pub const KINDS: [Kind; 17] = [
    Kind::Type,
    Kind::Enter,
    Kind::Backspace,
    Kind::Format,
    Kind::Move,
    Kind::Resize,
    Kind::Restyle,
    Kind::Insert,
    Kind::Duplicate,
    Kind::Delete,
    Kind::Reorder,
    Kind::ZOrder,
    Kind::Notes,
    Kind::Undo,
    Kind::Redo,
    Kind::DragStart,
    Kind::DragEnd,
];

/// The deck the session shows for `live`.
pub fn deck_of<H: Hub>(live: &DeckLive<H>, s: &Session) -> Arc<Presentation> {
    let uid = live.doc();
    s.documents().iter().find(|d| d.uid == uid).map(|d| d.doc.clone()).unwrap_or_default()
}

const WORDS: [&str; 6] = ["a", "b ", "cd", "é", "xyz ", "Q"];
const COLOURS: [&str; 6] = ["#C00000", "#00A000", "#0000C0", "#808000", "accent1", "accent4"];

fn colour(rng: &mut Rng) -> &'static str {
    COLOURS[rng.below(COLOURS.len())]
}

/// An open drag: where the pointer is.
#[derive(Clone, Copy, Debug)]
pub struct Drag {
    pub x: f64,
    pub y: f64,
}

/// One random step by one peer. A peer with an open drag only moves it on or drops it. The
/// result says what was tried and whether the command ran (a refusal is a valid outcome: the
/// gate, a deleted target, nothing to undo).
/// (The relay tests use it; they build with `native-hub` only.)
#[cfg_attr(not(feature = "native-hub"), allow(dead_code))]
pub fn act<H: Hub>(live: &mut DeckLive<H>, s: &mut Session, rng: &mut Rng, drag: &mut Option<Drag>) -> (Kind, bool) {
    let (kind, _, ok) = act_named(live, s, rng, drag);
    (kind, ok)
}

/// Commands that only move the caret, the selection or the view.
const NAVIGATION: [&str; 5] = ["text.exit", "slide.go", "text.edit", "edit.select", "edit.deselect"];

/// Every command a trace step can change the deck with (Undo and Redo apart), for the
/// per-command tally (review P1). Drags (`pointer`) are exempt, see `Intent::Gesture`.
const COMMANDS: [&str; 21] = [
    "text.insert",
    "text.delete",
    "format.bold",
    "format.italic",
    "format.size",
    "format.color",
    "shape.move",
    "shape.resize",
    "shape.fill",
    "shape.line",
    "shape.insert",
    "slide.new",
    "edit.duplicate",
    "slide.duplicate",
    "edit.delete",
    "slide.delete",
    "slide.move",
    "arrange.bringToFront",
    "arrange.bringForward",
    "arrange.sendBackward",
    "arrange.sendToBack",
];

/// [`act`], also naming the last command that is not navigation (`pointer` for a drag, empty
/// when the step ran only navigation).
#[allow(clippy::cognitive_complexity)] // One match over the step kinds, as in `act` before it named the command.
pub fn act_named<H: Hub>(live: &mut DeckLive<H>, s: &mut Session, rng: &mut Rng, drag: &mut Option<Drag>) -> (Kind, &'static str, bool) {
    if let Some(d) = drag.as_mut() {
        let (dx, dy) = (rng.below(30) as f64 - 10.0, rng.below(30) as f64 - 10.0);
        if rng.chance(40) {
            d.x += dx;
            d.y += dy;
            return (Kind::DragStart, "pointer", s.pointer(ev(PointerKind::Drag, d.x, d.y)).is_ok());
        }
        let ok = s.pointer(ev(PointerKind::Up, d.x + dx, d.y + dy)).is_ok();
        *drag = None;
        return (Kind::DragEnd, "pointer", ok);
    }
    let deck = deck_of(live, s);
    if deck.slides.is_empty() {
        return (Kind::Insert, "slide.new", live.run(s, "slide.new", &json!({"layout": "titleOnly", "title": "Again"})).is_ok());
    }
    let k = rng.below(deck.slides.len());
    let slide = &deck.slides[k];
    let tops: Vec<(ShapeId, Option<Xfrm>)> = slide.shapes.iter().map(|sh| (sh.id, sh.xfrm)).collect();
    // Text bodies: the shape and each paragraph's length in characters.
    let mut texts: Vec<(ShapeId, Vec<usize>)> = Vec::new();
    deckcraft_model::walk(&slide.shapes, &mut |sh, _| {
        if let Some(t) = &sh.text {
            texts.push((sh.id, t.paragraphs.iter().map(deckcraft_model::text::Paragraph::char_len).collect()));
        }
    });
    let top = (!tops.is_empty()).then(|| tops[rng.below(tops.len())]);
    let text = (!texts.is_empty()).then(|| texts[rng.below(texts.len())].clone());
    let mut cmd: &'static str = "";
    macro_rules! run {
        ($id:expr, $p:expr) => {{
            let id: &'static str = $id;
            if !NAVIGATION.contains(&id) {
                cmd = id;
            }
            live.run(s, id, &$p).is_ok()
        }};
    }
    // Steps start from the slide they touch and from nothing selected.
    let _ = run!("text.exit", json!({}));
    let _ = run!("slide.go", json!({"index": k}));
    let caret = |rng: &mut Rng| -> Option<Value> {
        let (id, paras) = text.as_ref()?;
        let p = rng.below(paras.len().max(1));
        let at = rng.below(paras.get(p).copied().unwrap_or(0) + 1);
        Some(json!({"id": id.0, "at": [p, at]}))
    };
    let roll = rng.below(100);
    let kind = match roll {
        0..=19 => Kind::Type,
        20..=24 => Kind::Enter,
        25..=29 => Kind::Backspace,
        30..=34 => Kind::Format,
        35..=40 => Kind::Move,
        41..=43 => Kind::Resize,
        44..=48 => Kind::Restyle,
        49..=53 => Kind::Insert,
        54..=56 => Kind::Duplicate,
        57..=60 => Kind::Delete,
        61..=64 => Kind::Reorder,
        65..=68 => Kind::ZOrder,
        69..=72 => Kind::Notes,
        73..=82 => Kind::Undo,
        83..=89 => Kind::Redo,
        _ => Kind::DragStart,
    };
    let ok = match kind {
        Kind::Type => caret(rng).is_some_and(|c| run!("text.edit", c)) && run!("text.insert", json!({"text": WORDS[rng.below(WORDS.len())]})),
        Kind::Enter => caret(rng).is_some_and(|c| run!("text.edit", c)) && run!("text.insert", json!({"text": "\n"})),
        Kind::Backspace => {
            caret(rng).is_some_and(|c| run!("text.edit", c))
                && run!("text.delete", json!({"dir": if rng.chance(70) { "backward" } else { "forward" }}))
        }
        Kind::Format => match top {
            Some((id, _)) => {
                run!("edit.select", json!({"ids": [id.0]}))
                    && match rng.below(4) {
                        0 => run!("format.bold", json!({"on": rng.chance(50)})),
                        1 => run!("format.italic", json!({"on": rng.chance(50)})),
                        2 => run!("format.size", json!({"size": 10 + rng.below(30)})),
                        _ => run!("format.color", json!({"color": colour(rng)})),
                    }
            }
            None => false,
        },
        Kind::Move => top
            .is_some_and(|(id, _)| run!("shape.move", json!({"ids": [id.0], "dx": rng.below(80) as f64 - 40.0, "dy": rng.below(60) as f64 - 30.0}))),
        Kind::Resize => top.is_some_and(|(id, _)| run!("shape.resize", json!({"ids": [id.0], "w": 40 + rng.below(200), "h": 20 + rng.below(120)}))),
        Kind::Restyle => top.is_some_and(|(id, _)| {
            if rng.chance(60) {
                run!("shape.fill", json!({"ids": [id.0], "color": colour(rng)}))
            } else {
                run!("shape.line", json!({"ids": [id.0], "color": colour(rng), "width": 1 + rng.below(6)}))
            }
        }),
        Kind::Insert => {
            if rng.chance(80) {
                let rect = [rng.below(500) as f64, rng.below(300) as f64, 60.0 + rng.below(200) as f64, 30.0 + rng.below(100) as f64];
                run!(
                    "shape.insert",
                    json!({"preset": if rng.chance(50) { "rect" } else { "ellipse" }, "rect": rect, "text": WORDS[rng.below(WORDS.len())]})
                )
            } else {
                run!(
                    "slide.new",
                    json!({"layout": if rng.chance(50) { "titleOnly" } else { "titleAndContent" }, "title": "New", "at": rng.below(deck.slides.len() + 1)})
                )
            }
        }
        Kind::Duplicate => {
            if rng.chance(70) {
                top.is_some_and(|(id, _)| run!("edit.duplicate", json!({"ids": [id.0]})))
            } else {
                run!("slide.duplicate", json!({"index": k}))
            }
        }
        Kind::Delete => {
            if rng.chance(85) || deck.slides.len() < 2 {
                top.is_some_and(|(id, _)| run!("edit.delete", json!({"ids": [id.0]})))
            } else {
                run!("slide.delete", json!({"index": k}))
            }
        }
        Kind::Reorder => run!("slide.move", json!({"from": k, "to": rng.below(deck.slides.len())})),
        Kind::ZOrder => top.is_some_and(|(id, _)| {
            let cmd = ["arrange.bringToFront", "arrange.bringForward", "arrange.sendBackward", "arrange.sendToBack"][rng.below(4)];
            run!(cmd, json!({"ids": [id.0]}))
        }),
        Kind::Notes => run!("text.edit", json!({"notes": true, "end": true})) && run!("text.insert", json!({"text": WORDS[rng.below(WORDS.len())]})),
        Kind::Undo => run!("edit.undo", json!({})),
        Kind::Redo => run!("edit.redo", json!({})),
        Kind::DragStart | Kind::DragEnd => match top {
            Some((_, Some(x))) if x.rot == 0.0 => {
                let (cx, cy) = (x.x + x.w / 2.0, x.y + x.h / 2.0);
                let _ = run!("edit.deselect", json!({}));
                let down = s.pointer(ev(PointerKind::Down, cx, cy)).is_ok();
                let moved = down && s.pointer(ev(PointerKind::Drag, cx + 12.0, cy + 8.0)).is_ok();
                if down {
                    cmd = "pointer";
                    *drag = Some(Drag { x: cx + 12.0, y: cy + 8.0 });
                }
                moved
            }
            _ => false,
        },
    };
    (kind, cmd, ok)
}

/// What a trace did: steps run and refused per kind, and the link events.
#[derive(Default)]
struct Tally {
    ran: BTreeMap<Kind, usize>,
    refused: BTreeMap<Kind, usize>,
    drops: usize,
    rejoins: usize,
    blips: usize,
    held: usize,
    joined: bool,
    /// Steps whose intent was checked against the author's replica, per kind.
    checked: BTreeMap<Kind, usize>,
    /// The first step of each kind whose intent was checked.
    first_checked: BTreeMap<Kind, usize>,
    /// Steps whose intent was checked, per command (review P1).
    checked_cmd: BTreeMap<&'static str, usize>,
    /// Command steps exempt as a gesture only because the peer held remote changes (review F1).
    held_exempt: BTreeMap<Kind, usize>,
    /// Command steps with no report whose author's deck was checked against its replica (review F2).
    unreported_same: usize,
    /// Rebuild checks skipped because a peer was mid-drag (`Room::check_rebuild`).
    rebuild_skipped_drag: usize,
}

/// Every peer: Live, equal to peer 0, and equal to the base plus its own replica's state.
pub fn assert_equal(room: &Room, what: &str) {
    for (i, p) in room.peers.iter().enumerate() {
        assert!(matches!(p.live.status(), LiveStatus::Live { .. }), "{what}: peer {i} is {:?}; notices {:?}", p.live.status(), p.notices);
        assert!(!p.live.holding(), "{what}: peer {i} still holds remote changes");
    }
    room.assert_converged();
    let first = room.peers[0].doc();
    for (i, p) in room.peers.iter().enumerate() {
        let state = p.live.replica().map(|r| r.state()).unwrap_or_else(|| panic!("{what}: peer {i} has a replica"));
        let rebuilt = deck_from_state(p.live.base(), &state).unwrap_or_else(|e| panic!("{what}: peer {i}'s state rebuilds: {e}"));
        assert!(rebuilt == *first, "{what}: peer {i}'s deck_from_state equals the shared deck");
    }
}

/// The joiner's start: the room's state as a saver would have checkpointed it (a connected
/// peer's replica after the room went quiet), taken as the join's catch-up. The relay's ordered
/// replay is cut, so whatever the joiner holds afterwards came from the checkpoint.
fn join_from_checkpoint(room: &mut Room, down: &[bool], drags: &[Option<Drag>]) -> Option<usize> {
    room.settle();
    let from = (0..room.peers.len()).find(|&i| {
        !down[i] && drags[i].is_none() && !room.peers[i].live.holding() && matches!(room.peers[i].live.status(), LiveStatus::Live { .. })
    })?;
    let state = room.peers[from].live.replica()?.state();
    let bytes = basic();
    let i = room.peers.len();
    let uid = format!("user{i}");
    let hub = room.relay.connect(&uid, &format!("User {i}"), true);
    let mut session = Session::new();
    let base = deckcraft_live::BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
    let mut live = DeckLive::start(hub, ROOM, &base, Access::Edit, &uid, &mut session).expect("start");
    let c = room.clock.clone();
    live.set_clock(Box::new(move || c.load(std::sync::atomic::Ordering::Relaxed) as f64));
    let hub = live.core().hub();
    room.relay.cut_replay(hub);
    let seq = room.relay.seq();
    room.relay.inject(
        hub,
        ServerMsg::Joined {
            doc: ROOM.into(),
            peer: room.relay.peer_id(hub),
            seq,
            space: room.relay.granted_space(hub).unwrap_or(0),
            roster: Vec::new(),
            locks: Vec::new(),
            replay_until: Some(seq),
            fallback: true,
            text_cap: None,
            heartbeat: false,
            history_lost: false,
        },
    );
    room.relay.inject(
        hub,
        ServerMsg::TextOps {
            doc: ROOM.into(),
            scope: DOC_SCOPE.into(),
            ops: json!([update_to_wire(&state)]),
            complete: true,
            snap: true,
            more: false,
        },
    );
    room.peers.push(P { live, session, notices: Vec::new() });
    for _ in 0..4 {
        room.pump(i);
    }
    assert!(matches!(room.peers[i].live.status(), LiveStatus::Live { .. }), "the checkpoint joiner is live: {:?}", room.peers[i].live.status());
    assert!(*room.peers[i].doc() == *room.peers[from].doc(), "the checkpoint joiner shows the room's deck, from the checkpoint alone");
    Some(i)
}

/// What the engine reported for one peer's last step: each command's `after` deck.
type Tap = Arc<Mutex<Vec<Arc<Presentation>>>>;

/// Puts a tap on peer `p`'s edit observer: the same observer `DeckLive` installs (each report
/// goes to the session's edit queue), which also keeps the deck each command produced. That deck
/// is the author's intent, before the mapping writes it and lands the replica's view back.
fn tap(p: &mut P) -> Tap {
    let tap: Tap = Arc::default();
    let (q, keep, doc) = (p.live.core().queue(), tap.clone(), p.live.doc());
    p.session.set_edit_observer(
        doc,
        Some(Box::new(move |e: Edited| {
            keep.lock().unwrap_or_else(PoisonError::into_inner).push(e.after.clone());
            q.push(DeckEdit::Command(Box::new(e)));
        })),
    );
    tap
}

/// `deck` with each slide that has notes text marked as having a notes slide. The rebuild marks
/// a slide whose notes were typed live that way (`build.rs`: notes typed live need a notes slide
/// to be saved in), while the engine leaves the flag to the writer, so the two differ in this
/// derived flag only, by design.
fn notes_flag(deck: &Presentation) -> Presentation {
    let mut d = deck.clone();
    for s in &mut d.slides {
        if !s.has_notes_slide && s.notes.paragraphs.iter().any(|p| !p.is_empty()) {
            Arc::make_mut(s).has_notes_slide = true;
        }
    }
    d
}

/// The first place two JSON values differ, as a path and the two values.
pub fn first_difference(a: &Value, b: &Value, at: &str) -> String {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                let (u, v) = (x.get(k).unwrap_or(&Value::Null), y.get(k).unwrap_or(&Value::Null));
                if u != v {
                    return first_difference(u, v, &format!("{at}.{k}"));
                }
            }
            String::new()
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find(|(_, (u, v))| u != v)
            .map_or_else(String::new, |(i, (u, v))| first_difference(u, v, &format!("{at}[{i}]"))),
        _ => format!("{at}: {a} against {b}"),
    }
}

/// Whether the step's intent is checked, or why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Intent {
    Checked,
    /// No command reported a change (navigation only, Undo and Redo, which reach the replica
    /// through the undo hook and have their own tests in `undo.rs`).
    Unreported,
    /// Exempt: refused on purpose (the gate or the safety net, rule 15). The engine's report is
    /// taken back and the deck put back, so the replica rightly lacks it.
    Refused,
    /// Exempt: a gesture in progress or just dropped. An open drag is shared at the drop (Ruling
    /// D12); a drop reaches the replica at the next pump, which also lands the remote changes
    /// held during the drag (Ruling D6), so the replica is not the drop's alone.
    Gesture,
}

/// The author's step is in its own replica (review I1): the deck the engine produced for the
/// step equals the deck rebuilt from the author's replica (as a remote peer would build it),
/// right after the step and before any pump. A change the mapping never wrote would otherwise
/// converge silently: the author's view is landed back from the replica too, so every peer
/// agrees on the deck without it.
fn assert_intent(room: &Room, who: usize, kind: Kind, ok: bool, reports: &[Arc<Presentation>], what: &str) -> Intent {
    let p = &room.peers[who];
    if matches!(kind, Kind::DragStart | Kind::DragEnd) || p.live.holding() {
        return Intent::Gesture;
    }
    let Some(after) = reports.last() else { return Intent::Unreported };
    if !ok {
        return Intent::Refused;
    }
    let (after, rebuilt) = (notes_flag(after), notes_flag(&p.rebuilt()));
    if after != rebuilt {
        panic!(
            "{what}: the step's change is not in the author's replica (it was not written); first difference, engine against replica: {}",
            first_difference(&serde_json::to_value(&after).unwrap_or_default(), &serde_json::to_value(&rebuilt).unwrap_or_default(), "")
        );
    }
    Intent::Checked
}

fn trace(seed: u64) -> Tally {
    let peers = PEERS[usize::try_from((seed - 1) % 7).unwrap_or(0)];
    let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x6465_636b);
    let mut room = Room::new(basic(), peers, seed);
    let mut taps: Vec<Tap> = room.peers.iter_mut().map(tap).collect();
    // Each peer's steps, shuffled together; the joiner takes the slot `peers` once it is in.
    let mut schedule: Vec<usize> = (0..=peers).flat_map(|p| std::iter::repeat_n(p, STEPS)).collect();
    for i in (1..schedule.len()).rev() {
        schedule.swap(i, rng.below(i + 1));
    }
    let join_at = schedule.len() / 4 + rng.below(schedule.len() / 4);
    let mut down = vec![false; peers];
    let mut drags: Vec<Option<Drag>> = vec![None; peers];
    let mut t = Tally::default();
    for (step, slot) in schedule.into_iter().enumerate() {
        if step == join_at
            && let Some(j) = join_from_checkpoint(&mut room, &down, &drags)
        {
            down.push(false);
            drags.push(None);
            taps.push(tap(&mut room.peers[j]));
            t.joined = true;
        }
        // Before the joiner is in, its slots go to a drawn peer.
        let who = if slot < room.peers.len() { slot } else { rng.below(peers) };
        let up = down.iter().filter(|d| !**d).count();
        if down[who] {
            if rng.chance(25) {
                room.restore_link(who);
                down[who] = false;
                t.rejoins += 1;
            }
        } else if rng.chance(2) && up > 1 {
            room.drop_link(who);
            down[who] = true;
            t.drops += 1;
        } else if rng.chance(1) {
            // Down and back between two pumps: the status never left Live (L5 note).
            room.drop_link(who);
            room.restore_link(who);
            t.blips += 1;
        }
        taps[who].lock().unwrap_or_else(PoisonError::into_inner).clear();
        let p = &mut room.peers[who];
        let (kind, cmd, ok) = act_named(&mut p.live, &mut p.session, &mut rng, &mut drags[who]);
        p.notices.extend(p.live.take_notices());
        *if ok { t.ran.entry(kind) } else { t.refused.entry(kind) }.or_default() += 1;
        let reports = std::mem::take(&mut *taps[who].lock().unwrap_or_else(PoisonError::into_inner));
        let intent = assert_intent(&room, who, kind, ok, &reports, &format!("seed {seed}, step {step}, peer {who}, {kind:?}"));
        let gesture = matches!(kind, Kind::DragStart | Kind::DragEnd);
        let undo = matches!(kind, Kind::Undo | Kind::Redo);
        match intent {
            Intent::Checked => {
                *t.checked.entry(kind).or_default() += 1;
                *t.checked_cmd.entry(cmd).or_default() += 1;
                t.first_checked.entry(kind).or_insert(step);
            }
            Intent::Gesture if !gesture => *t.held_exempt.entry(kind).or_default() += 1,
            // A command that changed the deck must report it, or the change is never written and
            // the next landing from the replica erases it (review F2). The author's deck after
            // such a step equals the deck rebuilt from its replica. (Not "equals the deck before
            // the step": the step's navigation can land remote changes the view held while a
            // text box was open, which is a change the author did not make.)
            Intent::Unreported if !undo => {
                let p = &room.peers[who];
                let (now, rebuilt) = (notes_flag(&deck_of(&p.live, &p.session)), notes_flag(&p.rebuilt()));
                if now != rebuilt {
                    panic!(
                        "seed {seed}, step {step}, peer {who}, {kind:?} ({cmd}, ok {ok}): the deck changed but no command reported it; first difference, author against replica: {}",
                        first_difference(&serde_json::to_value(&now).unwrap_or_default(), &serde_json::to_value(&rebuilt).unwrap_or_default(), "")
                    );
                }
                t.unreported_same += 1;
            }
            _ => {}
        }
        // Some peers pump, in a drawn order: delivery is seeded.
        let mut order: Vec<usize> = (0..room.peers.len()).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, rng.below(i + 1));
        }
        for i in order.into_iter().filter(|_| rng.chance(40)) {
            room.pump(i);
            if drags[i].is_some() && room.peers[i].live.holding() {
                t.held += 1;
            }
        }
    }
    for i in 0..room.peers.len() {
        if let Some(d) = drags[i].take() {
            room.peers[i].session.pointer(ev(PointerKind::Up, d.x, d.y)).expect("the last drop");
        }
        if down[i] {
            room.restore_link(i);
        }
    }
    room.settle();
    assert_equal(&room, &format!("seed {seed} ({peers} peers)"));
    t.rebuild_skipped_drag = room.rebuild_skipped_drag.get();
    t
}

#[test]
fn random_traces_converge_2_to_8_peers() {
    for seed in seeds_or(&DEFAULT_SEEDS) {
        let started = std::time::Instant::now();
        let t = trace(seed);
        let peers = PEERS[usize::try_from((seed - 1) % 7).unwrap_or(0)];
        eprintln!(
            "seed {seed}: {peers} peers; ran {:?}; refused {:?}; drops {}, rejoins {}, drop-and-rejoin between pumps {}, pumps holding for a drag {}, checkpoint joiner {}; intent checked {:?}, first at step {:?}; per command {:?}; command steps exempt while holding {:?}; unreported and equal to the replica {}; rebuild checks skipped mid-drag {}; {:.1} s",
            t.ran,
            t.refused,
            t.drops,
            t.rejoins,
            t.blips,
            t.held,
            t.joined,
            t.checked,
            t.first_checked,
            t.checked_cmd,
            t.held_exempt,
            t.unreported_same,
            t.rebuild_skipped_drag,
            started.elapsed().as_secs_f64()
        );
        for k in KINDS {
            assert!(t.ran.get(&k).copied().unwrap_or(0) > 0, "seed {seed}: the trace ran a {k:?} step");
        }
        // Every command kind had its intent checked; Undo and Redo go through the undo hook and
        // drags are exempt (see `Intent`).
        for k in KINDS.into_iter().filter(|k| !matches!(k, Kind::Undo | Kind::Redo | Kind::DragStart | Kind::DragEnd)) {
            assert!(t.checked.get(&k).copied().unwrap_or(0) > 0, "seed {seed}: a {k:?} step had its intent checked");
        }
        // Every command, not only every kind, had its intent checked (review P1); drags are exempt.
        for c in COMMANDS {
            assert!(t.checked_cmd.get(c).copied().unwrap_or(0) > 0, "seed {seed}: a {c} step had its intent checked");
        }
        assert!(t.joined, "seed {seed}: the checkpoint joiner joined");
    }
}

/// Two peers each insert 1,000 shapes at once: every id is new, minted from its author's
/// space, and both decks hold all 2,000 (spec: concurrent minting never collides). Both work on
/// the same slide at a time, 50 inserts each, then move on to the next of 20 slides together:
/// a slide of 2,000 shapes would make each insert's planning slow without testing more.
#[test]
fn ids_minted_concurrently_never_collide() {
    let bytes = deck_bytes(|s| {
        add_box(s, "anchor", [10.0, 10.0, 50.0, 20.0]);
        for _ in 1..20 {
            s.execute("slide.new", &json!({"layout": "blank"})).expect("slide");
        }
    });
    let mut r = Room::new(bytes, 2, 41);
    let ids_of = |p: &Presentation| -> Vec<u32> {
        let mut out = Vec::new();
        for s in &p.slides {
            deckcraft_model::walk(&s.shapes, &mut |sh, _| out.push(sh.id.0));
        }
        out
    };
    let before: BTreeSet<u32> = ids_of(&r.peers[0].doc()).into_iter().collect();
    let spaces = [r.peers[0].space(), r.peers[1].space()];
    assert!(spaces[0] != spaces[1] && spaces.iter().all(|s| *s != 0), "two spaces: {spaces:?}");
    for n in 0..1000 {
        if n % 50 == 0 {
            for p in &mut r.peers {
                p.run("slide.go", json!({"index": n / 50})).expect("go");
            }
        }
        for p in &mut r.peers {
            let rect = [(n % 40) as f64 * 15.0, (n / 40) as f64 * 15.0, 12.0, 12.0];
            p.run("shape.insert", json!({"preset": "rect", "rect": rect})).expect("insert");
        }
        // Both peers pump now and then, so the inserts cross on the wire.
        if n % 50 == 49 {
            r.pump(0);
            r.pump(1);
        }
    }
    r.settle();
    r.assert_converged();
    let after = ids_of(&r.peers[0].doc());
    let set: BTreeSet<u32> = after.iter().copied().collect();
    assert_eq!(set.len(), after.len(), "no id twice");
    let new: Vec<u32> = after.iter().copied().filter(|id| !before.contains(id)).collect();
    assert_eq!(new.len(), 2000, "every insert landed once");
    for (i, space) in spaces.iter().enumerate() {
        assert_eq!(new.iter().filter(|id| space_of(**id) == *space).count(), 1000, "peer {i}'s thousand come from its space");
    }
}
