//! A room of 2–8 peers on `craft_live::testing::MemRelay`, each with its own engine session,
//! pumped in a seeded random order on an injected clock. Test decks are built in code through
//! registered commands and saved as `.pptx` (rule 24).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use craft_live::Access;
use craft_live::testing::{MemHub, MemRelay, ROOM};
use deckcraft_engine::{Mods, PointerEvent, PointerKind, Session};
use deckcraft_live::{BaseFile, DeckLive, Notice, deck_from_doc};
use deckcraft_model::{Presentation, ShapeId, SlideId};
use serde_json::{Value, json};

pub const NAME: &str = "Deck.pptx";

/// A deck built with `build` on a new presentation, as `.pptx` bytes.
pub fn deck_bytes(build: impl FnOnce(&mut Session)) -> Vec<u8> {
    let mut s = Session::with_new();
    build(&mut s);
    deckcraft_engine::cmd::file::save_bytes(&s.active().expect("doc").doc, "pptx").expect("pptx")
}

/// A shape with `text` and the `rect` [x, y, w, h].
pub fn add_box(s: &mut Session, text: &str, rect: [f64; 4]) {
    s.execute("shape.insert", &json!({"preset": "rect", "rect": rect, "text": text})).expect("insert");
}

/// Three slides; the first holds the boxes "alpha", "beta" and "gamma".
pub fn basic() -> Vec<u8> {
    deck_bytes(|s| {
        add_box(s, "alpha", [60.0, 60.0, 200.0, 80.0]);
        add_box(s, "beta", [320.0, 60.0, 200.0, 80.0]);
        add_box(s, "gamma", [60.0, 260.0, 200.0, 80.0]);
        s.execute("slide.new", &json!({"layout": "titleAndContent", "title": "Two", "body": "second"})).expect("slide");
        s.execute("slide.new", &json!({"layout": "titleOnly", "title": "Three"})).expect("slide");
        s.execute("slide.go", &json!({"index": 0})).expect("go");
    })
}

/// The shape whose text is `text`, anywhere in `p`.
pub fn find(p: &Presentation, text: &str) -> ShapeId {
    let mut found = None;
    for s in &p.slides {
        deckcraft_model::walk(&s.shapes, &mut |sh, _| {
            if found.is_none() && sh.text.as_ref().is_some_and(|t| t.text() == text) {
                found = Some(sh.id);
            }
        });
    }
    found.unwrap_or_else(|| panic!("no shape with text {text:?}"))
}

/// The slide and shape value of `id`.
pub fn shape_of(p: &Presentation, id: ShapeId) -> Option<deckcraft_model::Shape> {
    p.slides.iter().find_map(|s| s.shape(id).cloned())
}

pub fn ev(kind: PointerKind, x: f64, y: f64) -> PointerEvent {
    PointerEvent { kind, x, y, mods: Mods::default(), tol: 4.0, pressure: None }
}

pub struct P {
    pub live: DeckLive<MemHub>,
    pub session: Session,
    pub notices: Vec<Notice>,
}

impl P {
    pub fn doc(&self) -> Arc<Presentation> {
        let uid = self.live.doc();
        self.session.documents().iter().find(|d| d.uid == uid).expect("live doc").doc.clone()
    }

    pub fn run(&mut self, id: &str, params: Value) -> Result<Value, String> {
        let r = self.live.run(&mut self.session, id, &params);
        self.notices.extend(self.live.take_notices());
        r
    }

    /// What the replica describes, the oracle.
    pub fn rebuilt(&self) -> Presentation {
        deck_from_doc(self.live.base(), self.live.replica().expect("replica"))
    }

    /// The id space this peer mints from.
    pub fn space(&self) -> u32 {
        self.doc().id_block.as_ref().map_or(0, deckcraft_model::IdBlock::space)
    }

    pub fn slide_ids(&self) -> Vec<SlideId> {
        self.doc().slides.iter().map(|s| s.id).collect()
    }
}

pub struct Room {
    pub relay: MemRelay,
    pub peers: Vec<P>,
    rng: u64,
    pub clock: Arc<AtomicU64>,
}

impl Room {
    /// `n` editors on `bytes`, delivery order seeded by `seed`, all live.
    pub fn new(bytes: Vec<u8>, n: usize, seed: u64) -> Room {
        Room::with_access(bytes, &vec![Access::Edit; n], seed)
    }

    pub fn with_access(bytes: Vec<u8>, access: &[Access], seed: u64) -> Room {
        let relay = MemRelay::new();
        let clock = Arc::new(AtomicU64::new(0));
        let mut peers = Vec::new();
        for (i, a) in access.iter().enumerate() {
            let uid = format!("user{i}");
            let hub = relay.connect(&uid, &format!("User {i}"), *a == Access::Edit);
            let mut session = Session::new();
            let base = BaseFile { bytes: &bytes, name: NAME, expected_sha256: None };
            let mut live = DeckLive::start(hub, ROOM, &base, *a, &uid, &mut session).expect("start");
            let c = clock.clone();
            live.set_clock(Box::new(move || c.load(Ordering::Relaxed) as f64));
            peers.push(P { live, session, notices: Vec::new() });
        }
        let mut room = Room { relay, peers, rng: seed | 1, clock };
        room.settle();
        if access.iter().all(|a| *a == Access::Edit) {
            assert!(room.peers.iter().all(|p| p.live.editable()), "every editor is live");
        }
        room
    }

    /// One more editor joins the room on `bytes`; its index.
    pub fn join(&mut self, bytes: &[u8]) -> usize {
        let i = self.peers.len();
        let uid = format!("user{i}");
        let hub = self.relay.connect(&uid, &format!("User {i}"), true);
        let mut session = Session::new();
        let base = BaseFile { bytes, name: NAME, expected_sha256: None };
        let mut live = DeckLive::start(hub, ROOM, &base, Access::Edit, &uid, &mut session).expect("start");
        let c = self.clock.clone();
        live.set_clock(Box::new(move || c.load(Ordering::Relaxed) as f64));
        self.peers.push(P { live, session, notices: Vec::new() });
        self.settle();
        i
    }

    /// Peer `i`'s link drops, as a network outage.
    pub fn drop_link(&self, i: usize) {
        self.relay.drop_link(self.peers[i].live.core().hub());
    }

    /// Peer `i`'s link comes back and rejoins.
    pub fn restore_link(&self, i: usize) {
        self.relay.restore_link(self.peers[i].live.core().hub());
    }

    fn next(&mut self) -> u64 {
        // xorshift64
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    pub fn tick(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::Relaxed);
    }

    /// One pump of every peer, in a seeded order.
    pub fn pump_all(&mut self) {
        let n = self.peers.len();
        let mut order: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        self.tick(16);
        for i in order {
            self.pump(i);
        }
    }

    pub fn pump(&mut self, i: usize) {
        let p = &mut self.peers[i];
        p.live.pump(&mut p.session);
        let n = p.live.take_notices();
        p.notices.extend(n);
    }

    /// Pump until nothing is in flight, checking after every round that each peer that holds
    /// nothing shows what its replica describes.
    pub fn settle(&mut self) {
        for round in 0..64 {
            self.pump_all();
            self.check_rebuild();
            if round >= 3 && self.relay.quiet() && self.peers.iter().all(|p| !p.live.holding()) {
                break;
            }
        }
        self.pump_all();
        self.check_rebuild();
    }

    pub fn check_rebuild(&self) {
        for (i, p) in self.peers.iter().enumerate() {
            // A peer in the middle of a drag shows its own preview, which is shared only at the
            // drop (Ruling D12); a trace that settles the room then (the checkpoint joiner, seed
            // 7) would compare that preview with the replica.
            if p.live.holding() || p.live.replica().is_none() || p.session.remote_busy(p.live.doc()) {
                continue;
            }
            let shown = p.doc();
            let rebuilt = p.rebuilt();
            assert!(
                *shown == rebuilt,
                "peer {i} shows what its replica describes; first difference, shown against rebuilt: {}",
                crate::trace::first_difference(
                    &serde_json::to_value(&*shown).unwrap_or_default(),
                    &serde_json::to_value(&rebuilt).unwrap_or_default(),
                    ""
                )
            );
        }
    }

    /// Every peer's deck equal, and equal to the rebuild of each replica.
    pub fn assert_converged(&self) {
        let first = self.peers[0].doc();
        for (i, p) in self.peers.iter().enumerate() {
            assert!(*p.doc() == *first, "peer {i} equals peer 0");
            assert!(p.rebuilt() == *first, "peer {i}'s replica rebuilds to the shared deck");
        }
    }
}

/// The id space an id was minted from (0: not minted live).
pub fn space_of(id: u32) -> u32 {
    if deckcraft_model::is_live_minted(id) { (id & !deckcraft_model::LIVE_ID_BASE) >> 16 } else { 0 }
}

/// SplitMix64, the house generator for seeded random tests: tiny and deterministic. Each test
/// prints its seed on failure.
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

/// The seeds of a random test: `DECKCRAFT_LIVE_SEED` runs that one, else
/// `DECKCRAFT_LIVE_SEEDS` of them (1, 2, …), else `default` (a short list, so the suite stays
/// quick; the long run sets `DECKCRAFT_LIVE_SEEDS`).
pub fn seeds_or(default: &[u64]) -> Vec<u64> {
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok());
    match (var("DECKCRAFT_LIVE_SEED"), var("DECKCRAFT_LIVE_SEEDS")) {
        (Some(one), _) => vec![one],
        (None, Some(n)) => (1..=n).collect(),
        (None, None) => default.to_vec(),
    }
}
