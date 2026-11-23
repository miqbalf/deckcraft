//! Save As = Export: one list of every format DeckCraft writes ([`SAVE_AS`]) and one function
//! every Save As and Export entry point ends in ([`SlideApp::save_as_copy`]).
//!
//! The entry points: File › Save As (its type list and Save/Download button), File › Export (Create
//! PDF, Create Handouts, Change File Type), the Export… dialog (the Quick Access Toolbar's Export,
//! command search), Ctrl+Shift+S and command search's Save As… (`app.saveAsDialog`, which opens
//! File › Save As, or saves at once with a `format`), `file.saveAs` without a path, and the control
//! channel's `app.export` without a path.
//!
//! A copy is only delivered: the presentation keeps its file, name, dirty flag and saved state,
//! also for .pptx and .deckcraft. Where it goes is `craft-share`'s [`Delivery`]:
//! - [`Delivery::Download`] (the web, plain or framed by a page that keeps the files): a browser
//!   download through [`Services::download`](crate::Services::download). Nothing is posted to the
//!   page: Save As never writes the page's file.
//! - [`Delivery::SaveDialog`] (the desktop): the system Save dialog with the format's one file type,
//!   then `file.export` to the picked path.
//!
//! Plain Save is not here: it writes the presentation's own file (or the page's), as before.

use craft_share::export::{Delivery, ExportFormat, ExportList, Runtime};
use serde_json::{Value, json};

use super::SlideApp;
use super::files::web_image_files;

/// The PowerPoint MIME type.
const PPTX_MIME: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";

/// Every format DeckCraft writes, in the order File › Save As lists them. The id is what
/// [`SlideApp::save_as_copy`] takes; the label and detail are the existing English strings.
pub const SAVE_AS: ExportList = ExportList::new(&[
    ExportFormat::new("pptx", "PowerPoint Presentation (*.pptx)", "pptx", PPTX_MIME).detail("Opens in PowerPoint and other presentation apps"),
    ExportFormat::new("deckcraft", "DeckCraft Presentation (*.deckcraft)", "deckcraft", "application/zip")
        .detail("DeckCraft's own format: everything is kept"),
    ExportFormat::new("potx", "PowerPoint Template (*.potx)", "potx", "application/vnd.openxmlformats-officedocument.presentationml.template")
        .detail("A template for new presentations"),
    ExportFormat::new("ppsx", "PowerPoint Show (*.ppsx)", "ppsx", "application/vnd.openxmlformats-officedocument.presentationml.slideshow")
        .detail("Opens straight into the slide show"),
    ExportFormat::new("pdf", "PDF (*.pdf)", "pdf", "application/pdf").detail("A fixed copy that keeps its layout on any device"),
    ExportFormat::new("outline", "Outline (*.txt)", "txt", "text/plain").detail("The slide titles and bullet text as plain text"),
    ExportFormat::new("rtf", "Outline/RTF (*.rtf)", "rtf", "application/rtf").detail("The slide titles and bullet text for word processors"),
    ExportFormat::new("png", "PNG Picture (*.png)", "png", "image/png").detail("The current slide as a picture"),
    ExportFormat::new("jpeg", "JPEG Picture (*.jpg)", "jpg", "image/jpeg").detail("The current slide as a compressed picture"),
    ExportFormat::new("pngAll", "PNG Pictures, all slides (*.png)", "png", "image/png").detail("Every slide as its own picture").many(),
    ExportFormat::new("jpegAll", "JPEG Pictures, all slides (*.jpg)", "jpg", "image/jpeg").detail("Every slide as its own compressed picture").many(),
    ExportFormat::new("gif", "Animated GIF (*.gif)", "gif", "image/gif").detail("The slides one after another as an animated picture"),
])
// Presentation files DeckCraft opens but doesn't write: a copy of `talk.ppt` as PDF is `talk.pdf`.
.replacing(&["ppt", "pot", "pps", "odp", "slidecraft"]);

/// The name a copy of an untitled presentation falls back to.
const FALLBACK: &str = "Presentation";

/// The `file.export` format and `all` flag for a [`SAVE_AS`] id.
pub(crate) fn engine_format(id: &str) -> Option<(&'static str, bool)> {
    Some(match id {
        "pptx" => ("pptx", false),
        "deckcraft" => ("deckcraft", false),
        "potx" => ("potx", false),
        "ppsx" => ("ppsx", false),
        "pdf" => ("pdf", false),
        "outline" => ("outline", false),
        "rtf" => ("rtf", false),
        "png" => ("png", false),
        "jpeg" => ("jpeg", false),
        "pngAll" => ("png", true),
        "jpegAll" => ("jpeg", true),
        "gif" => ("gif", false),
        _ => return None,
    })
}

/// The [`SAVE_AS`] id for a `file.export` format (and `all`), as automation names them; the
/// ids themselves are accepted too.
pub(crate) fn save_as_id(format: &str, all: bool) -> Option<&'static str> {
    let id = match (format, all) {
        ("png", true) => "pngAll",
        ("jpeg" | "jpg", true) => "jpegAll",
        ("jpg", false) => "jpeg",
        ("txt", _) => "outline",
        (f, _) => SAVE_AS.get(f)?.id,
    };
    Some(id)
}

impl SlideApp {
    /// Where a Save As copy goes here: the desktop has a Save dialog; the web (framed by a page
    /// that keeps the files or not) downloads.
    pub fn save_as_runtime(&self) -> Runtime {
        if self.services.pick_save.is_some() || self.services.pick_save_copy.is_some() {
            Runtime::Desktop
        } else {
            Runtime::web(self.services.host_save.is_some())
        }
    }

    /// The one Save As = Export: a copy of the active presentation as `id` (a [`SAVE_AS`] id),
    /// with `extra` `file.export` params (the PDF's layout, a PNG's transparent background, the
    /// GIF's settings). The web downloads it; the desktop asks where with the system Save dialog.
    /// The presentation keeps its file, name and unsaved changes. Returns whether a file was
    /// written or handed over (false when cancelled or failed, with the reason in the status bar).
    pub fn save_as_copy(&mut self, id: &str, extra: Value) -> bool {
        let Some((own, title, slide)) = self.session.active().map(|d| {
            let own = d.path.as_deref().and_then(|p| std::path::Path::new(p).file_name()).map(|n| n.to_string_lossy().to_string());
            (own, d.title(), d.selection.slide)
        }) else {
            return false;
        };
        let runtime = self.save_as_runtime();
        // The file's own name when it has one, else the title (a title's `/` is not a folder).
        let planned = SAVE_AS.plan(runtime, own.as_deref().unwrap_or_default(), FALLBACK, id).and_then(|mut plan| {
            if own.is_none() {
                plan.file_name = SAVE_AS.file_name_from_title(&title, FALLBACK, id)?;
            }
            Ok(plan)
        });
        let (plan, (format, all)) = match planned.map(|p| (engine_format(p.format.id), p)) {
            Ok((Some(engine), plan)) => (plan, engine),
            Ok((None, _)) | Err(_) => {
                self.set_status(format!("DeckCraft can't export “{id}”"));
                return false;
            }
        };
        let mut params = json!({"format": format, "all": all});
        if let (Some(o), Some(x)) = (params.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                if k != "path" && k != "format" && k != "all" {
                    o.insert(k.clone(), v.clone());
                }
            }
        }
        match plan.delivery {
            Delivery::SaveDialog => self.save_copy_to_picked(&plan.file_name, plan.format, params),
            Delivery::Download => self.download_copy(&plan.file_name, plan.format, slide, &params),
        }
    }

    /// The names of a many-file copy named `base` (one per slide): `base` itself for a single
    /// file, else craft-share's numbered names (`Deck-1.png`, `Deck-2.png`…).
    fn numbered(base: &str, format: ExportFormat, count: usize) -> Vec<String> {
        if count == 1 {
            return vec![base.to_string()];
        }
        (1..=count).map(|n| SAVE_AS.numbered_file_name(base, FALLBACK, format.id, n).unwrap_or_else(|_| base.to_string())).collect()
    }

    /// Desktop: the system Save dialog for `format`, then `file.export` to the picked path.
    fn save_copy_to_picked(&mut self, suggested: &str, format: ExportFormat, mut params: Value) -> bool {
        let picked = if let Some(pick) = self.services.pick_save_copy.as_mut() {
            pick(suggested, format.label, format.ext)
        } else if let Some(pick) = self.services.pick_save.as_mut() {
            pick(suggested)
        } else {
            None
        };
        let Some(picked) = picked else { return false };
        let path = match SAVE_AS.picked_path(&picked, format.id) {
            Ok(p) if p.renamed && std::path::Path::new(&p.path).exists() => {
                // The dialog asked about overwriting the name typed, not this one.
                self.set_status(format!("{} already exists; choose another name", p.path));
                return false;
            }
            Ok(p) => p.path,
            Err(e) => {
                self.set_status(e.to_string());
                return false;
            }
        };
        if format.many {
            // One file a slide, named as on the web: the folder picked, craft-share's numbers.
            let count = self.session.active().map_or(0, |d| d.doc.slides.len());
            let cut = path.rfind(['/', '\\']).map_or(0, |i| i + 1);
            let (dir, base) = path.split_at(cut);
            let targets: Vec<String> = Self::numbered(base, format, count).into_iter().map(|name| format!("{dir}{name}")).collect();
            // The dialog asked only about the name typed; the numbered files are other names.
            let existing = if count > 1 { targets.iter().filter(|t| std::path::Path::new(t).exists()).count() } else { 0 };
            if existing > 0 {
                let question = if existing == 1 {
                    format!("1 of the {count} files this export writes in {dir} already exists. Replace it?")
                } else {
                    format!("{existing} of the {count} files this export writes in {dir} already exist. Replace them?")
                };
                let replace = self.services.confirm.as_mut().is_some_and(|ask| ask(&question));
                if !replace {
                    self.set_status(format!("Nothing was exported: {existing} of the files already exist in {dir}"));
                    return false;
                }
            }
            params["all"] = json!(false);
            for (slide, target) in targets.into_iter().enumerate() {
                params["slide"] = json!(slide);
                params["path"] = json!(target);
                if self.run("file.export", params.clone()).is_err() {
                    return false;
                }
            }
            self.set_status(format!("Exported {count} slides to {dir}"));
            return true;
        }
        params["path"] = json!(path);
        match self.run("file.export", params) {
            Ok(_) => {
                self.set_status(format!("Exported {path}"));
                true
            }
            Err(_) => false,
        }
    }

    /// Web: the copy as a browser download (one per slide for the all-slides pictures).
    fn download_copy(&mut self, name: &str, format: ExportFormat, slide: usize, params: &Value) -> bool {
        let Some(doc) = self.session.active().map(|d| d.doc.clone()) else { return false };
        let engine = params["format"].as_str().unwrap_or_default();
        let bytes = match engine {
            "png" | "jpeg" => {
                let all = params["all"].as_bool().unwrap_or(false);
                let transparent = params.get("transparentBackground").and_then(Value::as_bool).unwrap_or(false);
                let images = web_image_files(&doc, slide, all, format.ext, transparent);
                let mut any = false;
                for (file, bytes) in Self::numbered(name, format, images.len()).into_iter().zip(images) {
                    any = self.download(&file, format.mime, Ok(bytes)) || any;
                }
                return any;
            }
            "pdf" => deckcraft_engine::cmd::file::pdf_bytes(&doc, params),
            "gif" => deckcraft_engine::cmd::file::gif_bytes(&doc, params),
            other => deckcraft_engine::cmd::file::save_bytes(&doc, other),
        };
        self.download(name, format.mime, bytes)
    }
}
