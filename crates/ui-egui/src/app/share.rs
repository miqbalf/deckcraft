//! Share: Nextcloud's own sharing for the open presentation, and nothing else. Every entry point
//! (the title bar's Share button, File › Share, the File tab, the Quick Access Toolbar, command
//! search and the control channel) runs `file.share`, which is [`SlideApp::share`].
//!
//! Where it goes is `craft-share`'s [`ShareRoute`]:
//! - [`ShareRoute::NextcloudWebHost`]: the web build framed by a Nextcloud page that said it shares
//!   (`share: true`), showing the file that page opened. The page is asked (`deckcraft:share`).
//! - [`ShareRoute::NextcloudDesktop`]: the desktop app, the presentation's file in a folder the
//!   Nextcloud desktop client syncs. Nextcloud's Files app opens at it in the default browser.
//! - [`ShareRoute::None`]: anything else (the plain web build, an unsaved presentation, a file
//!   outside a synced folder, another presentation than the one the page opened). The app says
//!   [`craft_share::NO_NEXTCLOUD`] and does nothing else: Share never exports, mails or opens a
//!   dialog.

use std::path::Path;

use craft_share::{ShareAction, ShareRoute};
use serde_json::{Value, json};

use super::SlideApp;

impl SlideApp {
    /// Where Share goes for the active presentation now. Runs only when Share is used, never per
    /// frame. The desktop lookup (the Nextcloud client's config, on macOS a File Provider
    /// attribute) runs once per presentation and file: its answer is kept until the presentation's
    /// file changes (Save As, a host's new name) and is dropped when the presentation closes.
    pub fn share_route(&mut self) -> ShareRoute {
        let Some((uid, path)) = self.session.active().map(|d| (d.uid, d.path.clone())) else { return ShareRoute::None };
        if self.services.host_share.is_some() && self.services.host_shares.get() {
            // The page shares only the file it opened in the frame, under the name it had then.
            let same = self.host_file.as_ref().is_some_and(|(u, p)| *u == uid && *p == path);
            return if same { ShareRoute::NextcloudWebHost } else { ShareRoute::None };
        }
        let (Some(route), Some(path)) = (&self.services.share_route, path) else { return ShareRoute::None };
        let open: Vec<u64> = self.session.documents().iter().map(|d| d.uid).collect();
        self.share_routes.retain(|u, _| open.contains(u));
        if let Some((known, answer)) = self.share_routes.get(&uid)
            && *known == path
        {
            return answer.clone();
        }
        let answer = match route(Path::new(&path)) {
            desktop @ ShareRoute::NextcloudDesktop { .. } => desktop,
            // Only a framing page can take a web share request.
            _ => ShareRoute::None,
        };
        self.share_routes.insert(uid, (path, answer.clone()));
        answer
    }

    /// `file.share`: open Nextcloud's sharing for the active presentation, or say that no
    /// Nextcloud is connected.
    pub fn share(&mut self) -> Result<Value, String> {
        match self.share_route().action() {
            ShareAction::PostToHost => {
                let post = self.services.host_share.as_mut().ok_or_else(|| craft_share::NO_NEXTCLOUD.to_string())?;
                post()?;
                Ok(json!({"route": "nextcloudWeb"}))
            }
            ShareAction::OpenUrl(url) => {
                self.open_url(&url);
                Ok(json!({"route": "nextcloudDesktop", "url": url}))
            }
            ShareAction::Notice(text) => {
                self.notify(text);
                Ok(json!({"route": "none", "message": text}))
            }
        }
    }

    /// The browser tabs Share (and links) asked to open this frame, not yet handed to egui.
    #[cfg(test)]
    pub(crate) fn pending_urls(&self) -> &[String] {
        &self.pending_urls
    }
}
