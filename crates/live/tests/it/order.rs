use deckcraft_live::order::{between, is_key, order, seed_keys};
use proptest::prelude::*;
use std::collections::BTreeMap;

fn in_gap(k: &str, lo: Option<&str>, hi: Option<&str>) -> bool {
    lo.is_none_or(|l| l < k) && hi.is_none_or(|h| k < h)
}

/// Any valid key: integer part of the length its head announces, then a fraction not ending in 0.
fn valid_key() -> impl Strategy<Value = String> {
    prop_oneof![
        "a[0-9A-Za-z]([0-9A-Za-z]{0,6}[1-9A-Za-z])?",
        "b[0-9A-Za-z]{2}([0-9A-Za-z]{0,6}[1-9A-Za-z])?",
        "Z[0-9A-Za-z]([0-9A-Za-z]{0,6}[1-9A-Za-z])?",
    ]
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
            prop_assert!(is_key(&k));
            keys.insert(at, k);
        }
        prop_assert!(keys.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn concurrent_inserts_at_one_gap_never_tie(n in 0usize..6, a in any::<u16>(), grow in proptest::collection::vec(any::<u16>(), 0..30)) {
        // A gap made by earlier random inserts (so hi may carry a space suffix), then 8 spaces.
        let mut keys = seed_keys(n + 1);
        for (i, g) in grow.iter().enumerate() {
            let at = usize::from(*g) % (keys.len() + 1);
            let lo = if at == 0 { None } else { keys.get(at - 1).cloned() };
            let k = between(lo.as_deref(), keys.get(at).map(String::as_str), 1 + i as u32 % 8);
            keys.insert(at, k);
        }
        let at = usize::from(a) % (keys.len() + 1);
        let lo = if at == 0 { None } else { keys.get(at - 1).map(String::as_str) };
        let hi = keys.get(at).map(String::as_str);
        let ks: Vec<String> = (1..=8).map(|s| between(lo, hi, s)).collect();
        for k in &ks {
            prop_assert!(in_gap(k, lo, hi), "{lo:?} < {k:?} < {hi:?}");
        }
        let mut sorted = ks.clone();
        sorted.sort();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), ks.len());
    }

    #[test]
    fn valid_keys_give_a_strict_answer(lo in valid_key(), hi in valid_key(), space in any::<u32>()) {
        // Every pair of valid keys lo < hi has an answer; no filter needed.
        prop_assume!(is_key(&lo) && is_key(&hi) && lo < hi);
        let k = between(Some(&lo), Some(&hi), space);
        prop_assert!(lo < k && k < hi, "{lo:?} < {k:?} < {hi:?}");
    }

    #[test]
    fn between_accepts_any_remote_strings(lo in ".{0,40}", hi in ".{0,40}", long in 0usize..400, space in any::<u32>()) {
        let long_key = "z".repeat(long);
        let all = [lo.as_str(), hi.as_str(), long_key.as_str(), "", "A", "a0", "a00", "A00000000000000000000000000"];
        for l in all.iter().map(|x| Some(*x)).chain([None]) {
            for h in all.iter().map(|x| Some(*x)).chain([None]) {
                let k = between(l, h, space);
                // An invalid hi is open, an invalid lo is ignored; valid pairs are strict.
                let l_ok = l.filter(|x| is_key(x));
                let h_ok = h.filter(|x| is_key(x)).filter(|x| l_ok.is_none_or(|l| l < *x));
                prop_assert!(in_gap(&k, l_ok, h_ok), "{l:?} {h:?} -> {k:?}");
            }
        }
    }
}

#[test]
fn zero_ended_hi_is_open_not_a_panic() {
    // `lo + "00"` has no key between; it is not a valid key, so it is an open end.
    for (lo, hi) in [("a1", "a100"), ("a1", "a10"), ("A0", "A00"), ("", "!")] {
        let k = between(Some(lo), Some(hi), 1);
        assert!(k.as_str() > lo || lo.is_empty(), "{lo:?} {hi:?} -> {k:?}");
    }
}

fn longest(keys: &[String]) -> usize {
    keys.iter().map(String::len).max().unwrap_or(0)
}

#[test]
fn ten_thousand_prepends_stay_short() {
    let mut keys = vec![between(None, None, 1)];
    for i in 0..10_000u32 {
        let k = between(None, keys.last().map(String::as_str), 1 + i % 8);
        assert!(keys.last().is_some_and(|f| k < *f));
        keys.push(k);
    }
    eprintln!("prepend x10000: longest {}", longest(&keys));
    assert!(longest(&keys) < 64);
}

#[test]
fn ten_thousand_appends_stay_short() {
    let mut keys = vec![between(None, None, 1)];
    for i in 0..10_000u32 {
        let k = between(keys.last().map(String::as_str), None, 1 + i % 8);
        assert!(keys.last().is_some_and(|f| k > *f));
        keys.push(k);
    }
    eprintln!("append x10000: longest {}", longest(&keys));
    assert!(longest(&keys) < 64);
}

#[test]
fn prepend_and_append_between_seed_keys_stay_short() {
    let seeds = seed_keys(3);
    let (mut front, mut back) = (seeds[0].clone(), seeds[2].clone());
    for i in 0..10_000u32 {
        let k = between(None, Some(&front), 1 + i % 8);
        assert!(k < front);
        front = k;
        let k = between(Some(&back), None, 1 + i % 8);
        assert!(k > back);
        back = k;
    }
    assert!(front.len() < 64 && back.len() < 64, "{} {}", front.len(), back.len());
}

/// 1,000 inserts at one position, each directly after the same lower neighbour (so every new
/// key is nested inside the previous gap). Information-theoretically each insert halves the gap
/// and a base-62 character holds about 6 bits, so 1,000 of them need about 170 characters with
/// ANY scheme that cannot see the future: the brief's 64-character bound cannot hold here. The
/// bound below (measured: 194 after the lower neighbour, 256 before the upper one) is not the
/// brief's.
const SAME_POSITION_BOUND: usize = 300;

#[test]
fn a_thousand_inserts_at_one_position_stay_bounded() {
    let seeds = seed_keys(2);
    // Always right after the lower neighbour.
    let mut hi = seeds[1].clone();
    let mut worst = 0;
    for i in 0..1000u32 {
        let k = between(Some(&seeds[0]), Some(&hi), 1 + i % 8);
        assert!(seeds[0] < k && k < hi);
        worst = worst.max(k.len());
        hi = k;
    }
    // Always right before the upper neighbour.
    let mut lo = seeds[0].clone();
    let mut worst_up = 0;
    for i in 0..1000u32 {
        let k = between(Some(&lo), Some(&seeds[1]), 1 + i % 8);
        assert!(lo < k && k < seeds[1]);
        worst_up = worst_up.max(k.len());
        lo = k;
    }
    eprintln!("same position x1000: after-lo longest {worst}, before-hi longest {worst_up}");
    assert!(worst <= SAME_POSITION_BOUND && worst_up <= SAME_POSITION_BOUND);
}

proptest! {
    #[test]
    fn seed_keys_have_room_between_neighbours(n in 2usize..40, picks in proptest::collection::vec(any::<u16>(), 1000)) {
        let seeds = seed_keys(n);
        prop_assert!(seeds.windows(2).all(|w| w[0] < w[1]));
        prop_assert!(seeds.iter().all(|k| is_key(k)));
        let mut keys = vec![seeds[0].clone(), seeds[1].clone()];
        for (i, p) in picks.iter().enumerate() {
            let at = 1 + usize::from(*p) % (keys.len() - 1);
            let k = between(Some(&keys[at - 1]), Some(&keys[at]), 1 + (i as u32 % 8));
            prop_assert!(keys[at - 1] < k && k < keys[at]);
            keys.insert(at, k);
        }
        prop_assert!(keys.iter().all(|k| k.len() < 64), "longest {}", longest(&keys));
    }
}

#[test]
fn seed_keys_are_deterministic_and_empty_for_zero() {
    assert_eq!(seed_keys(7), seed_keys(7));
    assert!(seed_keys(0).is_empty());
    let big = seed_keys(10_000);
    assert!(big.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn two_replicas_fill_the_same_gap_and_merge_identically() {
    let seeds = seed_keys(2);
    let (lo, hi) = (Some(seeds[0].as_str()), Some(seeds[1].as_str()));
    // Each replica inserts 50 keys into the gap, chained after its own previous key.
    let mut a: Vec<String> = Vec::new();
    let mut b: Vec<String> = Vec::new();
    for _ in 0..50 {
        a.push(between(a.last().map(String::as_str).or(lo), hi, 1));
        b.push(between(b.last().map(String::as_str).or(lo), hi, 2));
    }
    let merge = |x: &[String], y: &[String]| {
        let mut v: Vec<String> = x.iter().chain(y).cloned().collect();
        v.sort();
        v
    };
    let (left, right) = (merge(&a, &b), merge(&b, &a));
    assert_eq!(left, right);
    assert!(left.windows(2).all(|w| w[0] < w[1]), "a tie or misorder after merge");
    assert!(left.iter().all(|k| in_gap(k, lo, hi)));
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
                    prop_assert!(in_gap(&key, lo.as_deref(), hi.as_deref()), "{lo:?} < {key:?} < {hi:?}");
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
                    prop_assert!(in_gap(&key, lo.as_deref(), hi.as_deref()), "{lo:?} < {key:?} < {hi:?}");
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
        // Concurrent inserts from different replicas never share a key.
        let mut live_keys: Vec<String> = reps[0].live().into_iter().map(|(_, k)| k).collect();
        live_keys.sort();
        let total = live_keys.len();
        live_keys.dedup();
        prop_assert_eq!(live_keys.len(), total, "two live items share a key");
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
