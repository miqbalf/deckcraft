//! Fractional order keys for slides and z-order.
//!
//! This is a port of the public-domain fractional-indexing algorithm (David Greenspan's
//! description, as published by rocicorp): a key is a variable-length integer part (its first
//! letter says how long it is, `a`..`z` and `A`..`Z`) followed by a base-62 fraction over the
//! digits `0-9A-Za-z` that never ends in `0`. Appending or prepending steps the integer part, so
//! keys grow by O(log n), not by a character per operation; inserting between two neighbours
//! takes the midpoint of the fractions.
//!
//! Additions on top of the published algorithm:
//! - a fixed-width suffix made from the author's `space` (digits `1-z`) is appended when it still
//!   fits strictly inside the gap, so two peers inserting into the same gap never tie;
//! - peers' strings are untrusted: an invalid neighbour is ignored (a bad upper neighbour is an
//!   open end) and nothing here panics.
//!
//! Any string orders (`order` sorts by `(key, id)`); only `between` makes keys.

/// Digits in ASCII order.
const DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: usize = 62;
/// Width of the varying part of the space suffix: 61^3 > 32 768 live id spaces.
const SPACE_WIDTH: usize = 3;
/// First digit of every suffix: mid-range, so the suffix never blocks a later insert below it.
const SUFFIX_LEAD: u8 = b'V';
/// The smallest integer part; it has no predecessor, so it is not a valid key by itself.
const SMALLEST_INTEGER: &[u8] = b"A00000000000000000000000000";

fn digit_index(b: u8) -> Option<usize> {
    DIGITS.iter().position(|d| *d == b)
}

fn digit_at(i: usize) -> u8 {
    DIGITS.get(i).copied().unwrap_or(b'0')
}

/// Length of the integer part announced by its first letter.
fn integer_len(head: u8) -> Option<usize> {
    match head {
        b'a'..=b'z' => Some(usize::from(head - b'a') + 2),
        b'A'..=b'Z' => Some(usize::from(b'Z' - head) + 2),
        _ => None,
    }
}

fn integer_part(key: &[u8]) -> &[u8] {
    let n = key.first().and_then(|h| integer_len(*h)).unwrap_or(0);
    key.get(..n).unwrap_or(key)
}

fn valid(key: &[u8]) -> bool {
    let Some(n) = key.first().and_then(|h| integer_len(*h)) else {
        return false;
    };
    if key.len() < n || key == SMALLEST_INTEGER || !key.iter().all(|b| digit_index(*b).is_some()) {
        return false;
    }
    key.get(n..).is_some_and(|f| f.last() != Some(&b'0'))
}

/// True when `key` is a key this scheme could have made (peers' other strings still order).
pub fn is_key(key: &str) -> bool {
    valid(key.as_bytes())
}

/// Midpoint of two fractions (`a` < `b`, neither ending in `0`; `b` None is the open end).
fn midpoint(mut a: &[u8], mut b: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        if let Some(bb) = b {
            let mut n = 0;
            while a.get(n).copied().unwrap_or(b'0') == bb.get(n).copied().unwrap_or(0) {
                n += 1;
            }
            if n > 0 {
                out.extend_from_slice(bb.get(..n).unwrap_or(&[]));
                a = a.get(n..).unwrap_or(&[]);
                b = Some(bb.get(n..).unwrap_or(&[]));
                continue;
            }
        }
        let digit_a = a.first().and_then(|d| digit_index(*d)).unwrap_or(0);
        let digit_b = match b.and_then(|bb| bb.first()) {
            Some(d) => digit_index(*d).unwrap_or(BASE),
            None => BASE,
        };
        if digit_b > digit_a + 1 {
            out.push(digit_at((digit_a + digit_b).div_ceil(2)));
            return out;
        }
        if let Some(bb) = b.filter(|bb| bb.len() > 1) {
            out.push(bb.first().copied().unwrap_or(b'0'));
            return out;
        }
        out.push(digit_at(digit_a));
        a = a.get(1..).unwrap_or(&[]);
        b = None;
    }
}

fn increment_integer(x: &[u8]) -> Option<Vec<u8>> {
    let (head, rest) = x.split_first()?;
    let mut digs = rest.to_vec();
    let mut carry = true;
    for d in digs.iter_mut().rev() {
        let next = digit_index(*d)? + 1;
        if next == BASE {
            *d = b'0';
        } else {
            *d = digit_at(next);
            carry = false;
            break;
        }
    }
    let mut head = *head;
    if carry {
        if head == b'Z' {
            return Some(b"a0".to_vec());
        }
        if head == b'z' {
            return None;
        }
        head += 1;
        if head > b'a' {
            digs.push(b'0');
        } else {
            digs.pop();
        }
    }
    let mut out = vec![head];
    out.extend(digs);
    Some(out)
}

fn decrement_integer(x: &[u8]) -> Option<Vec<u8>> {
    let (head, rest) = x.split_first()?;
    let mut digs = rest.to_vec();
    let mut borrow = true;
    for d in digs.iter_mut().rev() {
        let i = digit_index(*d)?;
        if i == 0 {
            *d = digit_at(BASE - 1);
        } else {
            *d = digit_at(i - 1);
            borrow = false;
            break;
        }
    }
    let mut head = *head;
    if borrow {
        if head == b'a' {
            let mut out = vec![b'Z'];
            out.push(digit_at(BASE - 1));
            return Some(out);
        }
        if head == b'A' {
            return None;
        }
        head -= 1;
        if head < b'Z' {
            digs.push(digit_at(BASE - 1));
        } else {
            digs.pop();
        }
    }
    let mut out = vec![head];
    out.extend(digs);
    Some(out)
}

/// The published `generateKeyBetween`, on valid keys with `a` < `b`. None only when the key
/// space is exhausted (`a` is the largest or `b` the smallest possible key).
fn generate(a: Option<&[u8]>, b: Option<&[u8]>) -> Option<Vec<u8>> {
    match (a, b) {
        (None, None) => Some(b"a0".to_vec()),
        (None, Some(b)) => {
            let ib = integer_part(b);
            let fb = b.get(ib.len()..).unwrap_or(&[]);
            if ib == SMALLEST_INTEGER {
                let mut out = ib.to_vec();
                out.extend(midpoint(&[], Some(fb)));
                return Some(out);
            }
            if ib < b {
                return Some(ib.to_vec());
            }
            decrement_integer(ib)
        }
        (Some(a), None) => {
            let ia = integer_part(a);
            let fa = a.get(ia.len()..).unwrap_or(&[]);
            match increment_integer(ia) {
                Some(i) => Some(i),
                None => {
                    let mut out = ia.to_vec();
                    out.extend(midpoint(fa, None));
                    Some(out)
                }
            }
        }
        (Some(a), Some(b)) => {
            let ia = integer_part(a);
            let fa = a.get(ia.len()..).unwrap_or(&[]);
            let ib = integer_part(b);
            let fb = b.get(ib.len()..).unwrap_or(&[]);
            if ia == ib {
                let mut out = ia.to_vec();
                out.extend(midpoint(fa, Some(fb)));
                return Some(out);
            }
            let i = increment_integer(ia)?;
            if i.as_slice() < b {
                return Some(i);
            }
            let mut out = ia.to_vec();
            out.extend(midpoint(fa, None));
            Some(out)
        }
    }
}

/// `V` then `space` as fixed-minimum-width base 61 over `1-z`: distinct spaces give distinct
/// strings, and the last digit is never `0`.
fn space_suffix(space: u32) -> Vec<u8> {
    let mut n = u64::from(space);
    let mut out = Vec::new();
    while n > 0 || out.len() < SPACE_WIDTH {
        let d = usize::try_from(n % 61).unwrap_or(0);
        out.push(digit_at(d + 1));
        n /= 61;
    }
    out.push(SUFFIX_LEAD);
    out.reverse();
    out
}

/// `n` strictly increasing keys, deterministic, with room between neighbours.
pub fn seed_keys(n: usize) -> Vec<String> {
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(n);
    for _ in 0..n {
        let next = match out.last() {
            None => generate(None, None),
            Some(last) => generate(Some(last), None),
        };
        match next {
            Some(k) => out.push(k),
            None => break,
        }
    }
    out.into_iter().map(|k| String::from_utf8_lossy(&k).into_owned()).collect()
}

/// lo < key < hi (None = open end). Different `space`s never give equal keys for the same gap.
/// lo and hi may be any strings from a peer (junk included); lo >= hi is treated as hi = None.
/// An invalid `hi` is an open end; an invalid `lo` is ignored. Neither panics; the key's content
/// is never logged.
pub fn between(lo: Option<&str>, hi: Option<&str>, space: u32) -> String {
    let lo = lo.map(str::as_bytes).filter(|k| {
        let ok = valid(k);
        if !ok {
            log::debug!("order: ignoring a lower neighbour that is not an order key");
        }
        ok
    });
    let hi = hi.map(str::as_bytes).filter(|k| {
        let ok = valid(k);
        if !ok {
            log::debug!("order: treating an upper neighbour that is not an order key as open");
        }
        ok
    });
    let hi = hi.filter(|h| lo.is_none_or(|l| l < *h));
    let key = make(lo, hi, &space_suffix(space));
    String::from_utf8_lossy(&key).into_owned()
}

fn make(lo: Option<&[u8]>, hi: Option<&[u8]>, suffix: &[u8]) -> Vec<u8> {
    let fits = |k: &[u8]| -> Option<Vec<u8>> {
        let mut cand = k.to_vec();
        cand.extend_from_slice(suffix);
        hi.is_none_or(|h| cand.as_slice() < h).then_some(cand)
    };
    let mut plain: Option<Vec<u8>> = None;
    // Prefer the integer part of `hi` as the bound: it leaves the whole fraction range free.
    let mut bound = hi;
    if let Some(h) = hi {
        let ib = integer_part(h);
        if ib < h && lo.is_none_or(|l| l < ib) {
            bound = Some(ib);
        }
    }
    for b in [bound, hi] {
        let mut k = generate(lo, b);
        for _ in 0..=hi.map_or(0, <[u8]>::len) {
            let Some(cur) = k else { break };
            if plain.is_none() {
                plain = Some(cur.clone());
            }
            if let Some(c) = fits(&cur) {
                return c;
            }
            k = generate(Some(&cur), hi);
        }
    }
    // Nothing with a suffix fits: the plain key is still strictly inside the gap.
    plain.or_else(|| generate(lo, None)).unwrap_or_else(|| b"a0".to_vec())
}

/// Visible order: by (key, id).
pub fn order<I: Ord + Copy>(items: impl IntoIterator<Item = (I, String)>) -> Vec<I> {
    let mut v: Vec<(String, I)> = items.into_iter().map(|(i, k)| (k, i)).collect();
    v.sort();
    v.into_iter().map(|(_, i)| i).collect()
}
