//! The `deckcraft/1` yrs schema: root and key names, ids in map keys, the relay schema string and
//! the size limits.
//!
//! ```text
//! "meta"    YMap   "base": sha256 hex of the base bytes
//! "slides"  YMap   slide id (base 36) -> YMap:
//!     "pos" order key, "del", "delBy", "layout", "hidden", "name", "transition", "background",
//!     "colorMap", "showMaster", "notes" (YText)
//! "shapes"  YMap   shape id (base 36) -> YMap:
//!     "slide", "parent" (0 at slide level), "z" order key among siblings, "del", "delBy", "kind"
//!     (a group's without its children), "xfrm", "geom", "ph", "fill", "line", "effects", "style",
//!     "name", "hidden", "locked", "textBox", "descr", "title", "decorative", "click", "hover",
//!     "body" (the text body's BodyProps and list style), "text" (YText), "keep"
//! ```
//!
//! JSON values are the model's serde form. Entries are never removed: a deleted slide or shape is
//! a tombstone (`del: true`, `delBy` the deleter's uid). `text` is absent when the shape has no text
//! body (Ruling D26); `keep` (tags, `raw_ext`, SmartArt `diagram`) is written at the seed and for
//! new shapes and never edited live (Ruling D25).

pub const META: &str = "meta";
pub const SLIDES: &str = "slides";
pub const SHAPES: &str = "shapes";

pub const META_BASE: &str = "base";

pub const POS: &str = "pos";
pub const DEL: &str = "del";
pub const DEL_BY: &str = "delBy";
pub const LAYOUT: &str = "layout";
pub const HIDDEN: &str = "hidden";
pub const NAME: &str = "name";
pub const TRANSITION: &str = "transition";
pub const BACKGROUND: &str = "background";
pub const COLOR_MAP: &str = "colorMap";
pub const SHOW_MASTER: &str = "showMaster";
pub const NOTES: &str = "notes";

pub const SLIDE: &str = "slide";
pub const PARENT: &str = "parent";
pub const Z: &str = "z";
pub const KIND: &str = "kind";
pub const XFRM: &str = "xfrm";
pub const GEOM: &str = "geom";
pub const PH: &str = "ph";
pub const FILL: &str = "fill";
pub const LINE: &str = "line";
pub const EFFECTS: &str = "effects";
pub const STYLE: &str = "style";
pub const LOCKED: &str = "locked";
pub const TEXT_BOX: &str = "textBox";
pub const DESCR: &str = "descr";
pub const TITLE: &str = "title";
pub const DECORATIVE: &str = "decorative";
pub const CLICK: &str = "click";
pub const HOVER: &str = "hover";
pub const BODY: &str = "body";
pub const TEXT: &str = "text";
pub const KEEP: &str = "keep";

/// A slide's latest-wins JSON keys (everything but `pos`, `del`, `delBy`, `layout` and `notes`).
pub const SLIDE_FIELDS: [&str; 6] = [BACKGROUND, COLOR_MAP, HIDDEN, NAME, SHOW_MASTER, TRANSITION];
/// A shape's latest-wins JSON keys (everything but `slide`, `parent`, `z`, `del`, `delBy` and
/// `text`).
pub const SHAPE_FIELDS: [&str; 19] =
    [BODY, CLICK, DECORATIVE, DESCR, EFFECTS, FILL, GEOM, HIDDEN, HOVER, KEEP, KIND, LINE, LOCKED, NAME, PH, STYLE, TEXT_BOX, TITLE, XFRM];

/// The largest seed a room takes, in encoded state bytes (Ruling D9).
pub const MAX_SEED_BYTES: usize = 8 * 1024 * 1024;
/// The planned content of one update at most (Ruling D10); the relay frame is 1 MiB.
pub const UPDATE_BUDGET: usize = 640 * 1024;

/// The relay's schema string for this build: `deckcraft/1_<build>` (Ruling D1). `<build>` is the
/// compile-time `DECKCRAFT_LIVE_BUILD` (ASCII letters and digits, at most 40), else the crate
/// version, so a room never mixes builds.
#[must_use]
pub fn schema() -> String {
    let build: String = option_env!("DECKCRAFT_LIVE_BUILD").unwrap_or_default().chars().filter(char::is_ascii_alphanumeric).take(40).collect();
    let build = if build.is_empty() { env!("CARGO_PKG_VERSION").to_owned() } else { build };
    format!("deckcraft/1_{build}")
}

/// An id as a map key: lowercase base 36.
#[must_use]
pub fn id_key(id: u32) -> String {
    if id == 0 {
        return "0".to_owned();
    }
    let mut n = id;
    let mut out = Vec::new();
    while n > 0 {
        let d = n % 36;
        out.push(char::from_digit(d, 36).unwrap_or('0'));
        n /= 36;
    }
    out.iter().rev().collect()
}

/// A map key as an id: lowercase base 36 only, canonical (no leading zero), within `u32`.
#[must_use]
pub fn parse_key(key: &str) -> Option<u32> {
    if key.is_empty()
        || key.len() > 7
        || (key.len() > 1 && key.starts_with('0'))
        || !key.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    {
        return None;
    }
    u32::from_str_radix(key, 36).ok()
}

/// Lowercase hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// 32 bytes from 64 hex digits.
#[must_use]
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}
