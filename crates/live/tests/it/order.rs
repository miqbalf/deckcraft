use deckcraft_live::order::{between, order, seed_keys};
use proptest::prelude::*;
use std::collections::BTreeMap;

fn in_gap(k: &str, lo: Option<&str>, hi: Option<&str>) -> bool {
    lo.is_none_or(|l| l < k) && hi.is_none_or(|h| k < h)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn between_is_strictly_between(picks in proptest::collection::vec((any::<u16>(), 1u32..9), 1..80)) {
        let mut keys: Vec<String> = Vec::new();
        for (p, space) in picks {
            let at = usize::from(p) % (keys.len() + 1);
            let lo = if at == 0 { None } else { keys.get(at - 1).map(String::as_str) };
            let hi = keys.get(at).map(String::as_str);
            let k = between(lo, hi, space);
            prop_assert!(in_gap(&k, lo, hi), "{lo:?} < {k:?} < {hi:?}");
            keys.insert(at, k);
        }
        prop_assert!(keys.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn concurrent_inserts_at_one_gap_never_tie(n in 0usize..6, a in any::<u16>()) {
        let seeds = seed_keys(n.max(1) + 1);
        let at = usize::from(a) % (seeds.len() + 1);
        let lo = if at == 0 { None } else { seeds.get(at - 1).map(String::as_str) };
        let hi = seeds.get(at).map(String::as_str);
        let ks: Vec<String> = (1..=8).map(|s| between(lo, hi, s)).collect();
        let mut sorted = ks.clone();
        sorted.sort();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), ks.len());
    }

    #[test]
    fn between_accepts_alphabet_strings_strictly(lo in "[0-9A-Za-z]{0,30}", hi in "[0-9A-Za-z]{1,30}", space in any::<u32>()) {
        if lo < hi && !hi.chars().all(|c| c == '0') && hi != format!("{lo}0") {
            let k = between(Some(&lo), Some(&hi), space);
            prop_assert!(lo < k && k < hi, "{lo:?} < {k:?} < {hi:?}");
        }
    }

    #[test]
    fn between_accepts_any_remote_strings(lo in ".{0,40}", hi in ".{0,40}", long in 0usize..400, space in any::<u32>()) {
        let long_key = "z".repeat(long);
        for (l, h) in [(Some(lo.as_str()), Some(hi.as_str())), (None, Some(hi.as_str())), (Some(lo.as_str()), None),
                       (Some(long_key.as_str()), Some(hi.as_str())), (Some(lo.as_str()), Some(long_key.as_str()))] {
            let k = between(l, h, space);
            // lo >= hi means hi is open; with an open end an answer always exists.
            let h_eff = h.filter(|h| l.unwrap_or("") < *h);
            if h_eff.is_none() {
                prop_assert!(l.is_none_or(|l| l < k.as_str()), "{l:?} < {k:?}");
            }
        }
    }
}

#[test]
fn between_handles_junk_without_panic() {
    for s in ["", "!", "~", "\u{0}", "0", "00", "zzzz", "é", "A0"] {
        for t in ["", "!", "~", "0", "z", "A0", "zzzz"] {
            let _ = between(Some(s), Some(t), 1);
            let _ = between(Some(s), None, 1);
            let _ = between(None, Some(t), u32::MAX);
        }
    }
    assert!(between(Some("A"), Some("A0x"), 1).as_str() > "A");
}

#[test]
fn between_alphabet_gaps_are_strict() {
    let seeds = seed_keys(5);
    for w in seeds.windows(2) {
        let k = between(Some(&w[0]), Some(&w[1]), 3);
        assert!(w[0] < k && k < w[1]);
        assert!(!k.ends_with('0'));
    }
    let k = between(None, Some("0x"), 2);
    assert!(k.as_str() < "0x", "{k}");
}

proptest! {
    #[test]
    fn seed_keys_have_room_between_neighbours(n in 2usize..40, picks in proptest::collection::vec(any::<u16>(), 1000)) {
        let seeds = seed_keys(n);
        prop_assert!(seeds.windows(2).all(|w| w[0] < w[1]));
        let mut keys = vec![seeds[0].clone(), seeds[1].clone()];
        for (i, p) in picks.iter().enumerate() {
            let at = 1 + usize::from(*p) % (keys.len() - 1);
            let k = between(Some(&keys[at - 1]), Some(&keys[at]), 1 + (i as u32 % 8));
            prop_assert!(keys[at - 1] < k && k < keys[at]);
            keys.insert(at, k);
        }
        prop_assert!(keys.iter().all(|k| k.len() < 64), "longest {}", keys.iter().map(String::len).max().unwrap_or(0));
    }
}

#[test]
fn seed_keys_are_deterministic_and_empty_for_zero() {
    assert_eq!(seed_keys(7), seed_keys(7));
    assert!(seed_keys(0).is_empty());
}

// ---- replicas -------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Update {
    id: u32,
    key: String,
    dead: bool,
    stamp: (u64, usize),
}

#[derive(Clone, Default)]
struct Replica {
    map: BTreeMap<u32, Update>,
    clock: u64,
}

impl Replica {
    fn apply(&mut self, u: &Update) {
        self.clock = self.clock.max(u.stamp.0);
        let newer = self.map.get(&u.id).is_none_or(|c| c.stamp < u.stamp);
        if newer {
            self.map.insert(u.id, u.clone());
        }
    }
    fn live(&self) -> Vec<(u32, String)> {
        self.map.values().filter(|u| !u.dead).map(|u| (u.id, u.key.clone())).collect()
    }
    fn visible(&self) -> Vec<u32> {
        order(self.live())
    }
    fn gap(&self, at: usize, skip: Option<u32>) -> (Option<String>, Option<String>) {
        let mut keys: Vec<(String, u32)> = self.live().into_iter().map(|(i, k)| (k, i)).collect();
        keys.sort();
        let keys: Vec<String> = keys.into_iter().filter(|(_, i)| Some(*i) != skip).map(|(k, _)| k).collect::<Vec<_>>();
        let at = at % (keys.len() + 1);
        (if at == 0 { None } else { keys.get(at - 1).cloned() }, keys.get(at).cloned())
    }
}

#[derive(Clone, Debug)]
enum Op {
    Insert(usize),
    Move(usize, usize),
    Kill(usize),
    Sync(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u8>().prop_map(|a| Op::Insert(usize::from(a))),
        (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::Move(usize::from(a), usize::from(b))),
        any::<u8>().prop_map(|a| Op::Kill(usize::from(a))),
        any::<u8>().prop_map(|a| Op::Sync(usize::from(a))),
    ]
}

proptest! {
    #[test]
    fn replicas_agree_on_order(n in 2usize..9, ops in proptest::collection::vec((any::<u8>(), op()), 1..120), shuffle in any::<u64>()) {
        let mut reps = vec![Replica::default(); n];
        let mut log: Vec<Update> = Vec::new();
        let mut next_id = 1u32;
        let mut seen: Vec<usize> = vec![0; n];
        for (who, o) in ops {
            let r = usize::from(who) % n;
            let mut mint = |reps: &mut Vec<Replica>, u: Update| {
                reps[r].apply(&u);
                log.push(u);
            };
            match o {
                Op::Insert(at) => {
                    let (lo, hi) = reps[r].gap(at, None);
                    let key = between(lo.as_deref(), hi.as_deref(), r as u32 + 1);
                    reps[r].clock += 1;
                    let id = next_id * 16 + r as u32; next_id += 1;
                    let u = Update { id, key, dead: false, stamp: (reps[r].clock, r) };
                    mint(&mut reps, u);
                }
                Op::Move(..) | Op::Kill(_) if reps[r].live().is_empty() => {}
                Op::Move(a, at) => {
                    let ids = reps[r].visible();
                    let id = ids[a % ids.len()];
                    let (lo, hi) = reps[r].gap(at, Some(id));
                    let key = between(lo.as_deref(), hi.as_deref(), r as u32 + 1);
                    reps[r].clock += 1;
                    let u = Update { id, key, dead: false, stamp: (reps[r].clock, r) };
                    mint(&mut reps, u);
                }
                Op::Kill(a) => {
                    let ids = reps[r].visible();
                    let id = ids[a % ids.len()];
                    let key = reps[r].map[&id].key.clone();
                    reps[r].clock += 1;
                    let u = Update { id, key, dead: true, stamp: (reps[r].clock, r) };
                    mint(&mut reps, u);
                }
                Op::Sync(a) => {
                    // Deliver some of the log in a shuffled order, with duplicates.
                    let from = seen[r];
                    let mut batch: Vec<Update> = log[from..].to_vec();
                    batch.extend(log.iter().step_by(1 + a % 3).cloned());
                    let mut s = shuffle ^ (a as u64);
                    for i in (1..batch.len()).rev() {
                        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        batch.swap(i, (s >> 33) as usize % (i + 1));
                    }
                    for u in &batch { reps[r].apply(u); }
                    seen[r] = log.len();
                }
            }
        }
        // Final delivery of everything to everyone, each in its own shuffled order.
        for (r, rep) in reps.iter_mut().enumerate() {
            let mut batch = log.clone();
            batch.extend(log.iter().cloned());
            let mut s = shuffle.wrapping_add(r as u64);
            for i in (1..batch.len()).rev() {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                batch.swap(i, (s >> 33) as usize % (i + 1));
            }
            for u in &batch { rep.apply(u); }
        }
        let first = reps[0].visible();
        for rep in &reps { prop_assert_eq!(&rep.visible(), &first); }
        let mut uniq = first.clone(); uniq.sort(); uniq.dedup();
        prop_assert_eq!(uniq.len(), first.len());
        let alive: Vec<u32> = reps[0].map.values().filter(|u| !u.dead).map(|u| u.id).collect();
        let mut a = alive; a.sort();
        let mut f = first; f.sort();
        prop_assert_eq!(a, f);
        let minted: std::collections::BTreeSet<u32> = log.iter().map(|u| u.id).collect();
        prop_assert_eq!(minted.len(), reps[0].map.len());
    }
}
