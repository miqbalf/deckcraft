//! Fractional order keys for slides and z-order.
//!
//! Keys are strings over the digits `0-9A-Za-z` (ASCII order). The scheme is the standard
//! fractional-indexing midpoint: find the first position where a digit fits strictly between the
//! neighbours, else copy the lower neighbour's digit and go deeper. A fixed-width suffix of the
//! author's `space` (digits `1-z`, never `0`) is appended, so two peers inserting into the same
//! gap never produce equal keys, and no key ends in `0` (which would leave no room below it).
//! Any string orders; peers' junk keys never panic.

const DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// Width of the space suffix: 61^3 > 32 768 live id spaces.
const SPACE_WIDTH: usize = 3;
/// Sentinel above every code point: the open upper end.
const OPEN: i64 = 0x11_0000;

fn is_digit(c: i64) -> bool {
    u8::try_from(c).is_ok_and(|b| DIGITS.contains(&b))
}

/// `space` as fixed-minimum-width base 61 over `1-z`: distinct spaces give distinct strings.
fn space_suffix(space: u32) -> String {
    let mut n = u64::from(space);
    let mut out = Vec::new();
    while n > 0 || out.len() < SPACE_WIDTH {
        let d = usize::try_from(n % 61).unwrap_or(0);
        out.push(DIGITS.get(d + 1).copied().unwrap_or(b'1'));
        n /= 61;
    }
    out.reverse();
    out.into_iter().map(char::from).collect()
}

/// `n` strictly increasing keys, evenly spread, deterministic, with room between neighbours.
pub fn seed_keys(n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let mut width = 1u32;
    while 62u128.pow(width) < 4 * (n as u128 + 1) {
        width += 1;
    }
    let total = 62u128.pow(width);
    (0..n)
        .map(|i| {
            let mut v = (i as u128 + 1) * total / (n as u128 + 1);
            let mut ds = Vec::new();
            for _ in 0..width {
                ds.push(usize::try_from(v % 62).unwrap_or(0));
                v /= 62;
            }
            ds.reverse();
            // Never end in '0': spacing is at least 4, so the bump keeps the order strict.
            if let Some(last) = ds.last_mut()
                && *last == 0
            {
                *last = 1;
            }
            ds.into_iter().map(|d| char::from(DIGITS.get(d).copied().unwrap_or(b'1'))).collect()
        })
        .collect()
}

/// lo < key < hi (None = open end). Different `space`s never give equal keys for the same gap.
/// lo and hi may be any strings from a peer (junk included); lo >= hi is treated as hi = None.
pub fn between(lo: Option<&str>, hi: Option<&str>, space: u32) -> String {
    let lo: Vec<char> = lo.unwrap_or("").chars().collect();
    let hi: Option<Vec<char>> = hi.map(|h| h.chars().collect());
    let hi = hi.filter(|h| lo.as_slice() < h.as_slice());
    let mut key = String::new();
    let mut lo_tight = true;
    let mut hi_tight = hi.is_some();
    let mut i = 0usize;
    loop {
        let l: i64 = if lo_tight { lo.get(i).map_or(-1, |c| i64::from(u32::from(*c))) } else { -1 };
        let h: i64 = match (&hi, hi_tight) {
            (Some(hv), true) => match hv.get(i) {
                Some(c) => i64::from(u32::from(*c)),
                None => break, // cannot happen for lo < hi; fall back below
            },
            _ => OPEN,
        };
        let cands: Vec<u8> = DIGITS.iter().copied().filter(|b| l < i64::from(*b) && i64::from(*b) < h).collect();
        if let Some(d) = cands.get(cands.len() / 2) {
            key.push(char::from(*d));
            key.push_str(&space_suffix(space));
            return key;
        }
        if l >= 0 {
            if let Some(c) = u32::try_from(l).ok().and_then(char::from_u32) {
                key.push(c);
            }
            if l < h {
                hi_tight = false;
            }
        } else if let Some(c) = u32::try_from(h).ok().and_then(char::from_u32).filter(|_| h < OPEN) {
            key.push(c);
            lo_tight = false;
        } else {
            break;
        }
        if !is_digit(l) && l < 0 && h >= OPEN {
            break;
        }
        i += 1;
    }
    // No answer exists (or junk edge): still return something that orders after `lo`.
    key.push_str(&space_suffix(space));
    key
}

/// Visible order: by (key, id).
pub fn order<I: Ord + Copy>(items: impl IntoIterator<Item = (I, String)>) -> Vec<I> {
    let mut v: Vec<(String, I)> = items.into_iter().map(|(i, k)| (k, i)).collect();
    v.sort();
    v.into_iter().map(|(_, i)| i).collect()
}
