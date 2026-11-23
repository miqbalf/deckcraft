//! The web shell's decisions about a framing page (`?host=parent`) and about downloads, kept here
//! so they compile and are tested natively; `deckcraft-web` only supplies the transport.
//!
//! What reaches the page: plain Save ([`Services::host_save`], `deckcraft:save`) and Share
//! ([`Services::host_share`], `deckcraft:share`). Nothing else. Save As, every export and Print
//! are copies: they stay on [`Services::download`], the browser download the web shell set up,
//! and are never posted as `deckcraft:save`.

use crate::{HostSaveFn, HostSaves, HostShareFn, HostShares, SAVE_AS, Services};

/// The page's transport and state, as the web shell's bridge provides them.
pub struct HostHooks {
    /// Post `deckcraft:save {name, saveAs, bytes, id?}` to the page.
    pub post_save: HostSaveFn,
    /// Post `deckcraft:share {}` to the page.
    pub post_share: HostShareFn,
    /// Whether the page answers saves, and its answers (shared with the bridge's listener).
    pub saves: HostSaves,
    /// Whether the page shares files itself (shared with the bridge's listener).
    pub shares: HostShares,
}

/// Points plain Save and Share at the page. [`Services::download`] is left as it is: Save As,
/// exports and Print keep downloading to the user's computer.
pub fn connect_host(services: &mut Services, hooks: HostHooks) {
    services.host_saves = hooks.saves;
    services.host_save = Some(hooks.post_save);
    services.host_shares = hooks.shares;
    services.host_share = Some(hooks.post_share);
}

/// The MIME type of a web download named `name`: the Save As list's type for its extension
/// (`application/zip` for .deckcraft), else craft-share's table.
pub fn download_mime(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    SAVE_AS.for_ext(ext).map_or_else(|| craft_share::export::mime_for_ext(ext), |f| f.mime)
}

/// The name a download of `path` gets: its last component (`deckcraft` when it has none).
pub fn download_name(path: &str) -> String {
    let last = path.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    if last.is_empty() || last == "." || last == ".." { "deckcraft".to_string() } else { last.to_string() }
}
