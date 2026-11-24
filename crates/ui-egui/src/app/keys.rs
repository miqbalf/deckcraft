use super::clipboard::OBJECTS_MARKER;
use super::{Mods, SlideApp, Value, backstage, json, menus};

impl SlideApp {
    /// The app shortcuts that still make sense on the empty File/start screen: create, open,
    /// the command palette, preferences and the File tab itself. Document commands (save, find,
    /// hyperlink, show, zoom) stay out; none of them can act on a presentation that is not open.
    fn start_screen_shortcut(key: egui::Key, m: Mods) -> bool {
        use egui::Key::*;
        matches!(
            (key, m.cmd, m.shift, m.alt),
            (O, true, false, false) | (P, true, true, false) | (Comma, true, false, false) | (F, false, false, true)
        )
    }

    /// File-tab navigation and document-free shortcuts, also used on the start screen.
    fn route_backstage_keys(&mut self, events: Vec<egui::Event>) {
        if self.backstage.is_none() {
            return;
        }
        // The File tab: its own keys (Esc goes back, arrows walk the side menu) and app
        // shortcuts (New, Open, Command Palette…); typing, pasting or deleting would edit the
        // hidden presentation.
        let start_screen = self.session.active().is_none();
        for e in events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = e {
                let mods = Mods { shift: modifiers.shift, alt: modifiers.alt, cmd: modifiers.command || modifiers.ctrl };
                let handled = backstage::key(self, key, mods)
                    // With no presentation the File home only owns the shortcuts that create or
                    // open one. Borrowing the whole document shortcut table here would also arm
                    // Save/Save As, hyperlink, find/replace, show and zoom against the empty
                    // start screen, which is a scope expansion no editing route needs.
                    || (mods.cmd || mods.alt)
                        && (!start_screen || Self::start_screen_shortcut(key, mods))
                        && menus::ui_shortcut(self, key, mods);
                if handled {
                    continue;
                }
                // Engine shortcuts that don't touch the hidden slide (Ctrl+N).
                if mods.cmd
                    && matches!(key, egui::Key::N)
                    && let Err(e) = self.session.key(key_name(key), mods)
                {
                    self.set_status(e.to_string());
                }
            }
        }
    }

    /// Keyboard and clipboard for the slide (when no text field has focus).
    pub(super) fn keyboard(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        // Ctrl+V with only an image on the clipboard: egui-winit sends no Paste event and swallows the
        // key press, so the release of a V press never seen is the paste. Tracked even while a text
        // field has focus, so that a V typed there is not taken for one.
        let mut image_paste = false;
        let mut pasted_image_event = false;
        for e in &events {
            match e {
                egui::Event::PasteImage(_) => {
                    pasted_image_event = true;
                    self.v_seen = true;
                }
                egui::Event::Paste(_) | egui::Event::Key { key: egui::Key::V, pressed: true, .. } => self.v_seen = true,
                egui::Event::Key { key: egui::Key::V, pressed: false, modifiers, .. } => {
                    // Only a release with Command/Ctrl held is a paste: a bare V release is not.
                    let seen = std::mem::take(&mut self.v_seen);
                    image_paste = !seen && (modifiers.command || modifiers.ctrl);
                }
                _ => {}
            }
        }
        let viewport = self.ime_viewport_or(ctx);
        if self.session.active().is_none() {
            // No document: keep slide-editing input suspended, while the start screen's
            // File-tab navigation and document-free shortcuts remain available.
            self.suspend_ime_reset(viewport);
            self.route_ime_controls_only(ctx, &events);
            if self.palette.is_none() && self.dialog.as_ref().is_none_or(|d| !d.modal()) && ctx.memory(|m| m.focused().is_none()) {
                self.route_backstage_keys(events);
            }
            self.drain_requests();
            return;
        }
        let modal = self.dialog.as_ref().is_some_and(|d| d.modal()) || self.palette.is_some();
        // While slide text is being edited, the text owns the keyboard (#93). Tab and the arrow
        // keys move egui's keyboard focus onto ordinary widgets (the window's drag handle, ribbon
        // buttons), and that stray focus would swallow every later key and end text entry. Let it
        // go; a real text field or an open menu keeps its keys.
        if !modal
            && self.session.active().is_some_and(|d| d.selection.text.is_some())
            && !ctx.text_edit_focused()
            && !ctx.any_popup_open()
            && let Some(id) = ctx.memory(|m| m.focused())
        {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
        if ctx.memory(|m| m.focused().is_some()) || modal {
            // A stock field/modal/palette owns input now. Retire the custom fence, then visit
            // controls in raw order; stock text/key events are left for the stock widgets.
            self.suspend_ime_reset(viewport);
            self.route_ime_controls_only(ctx, &events);
            return;
        }
        if self.backstage.is_some() {
            self.suspend_ime_reset(viewport);
            self.route_ime_controls_only(ctx, &events);
            self.route_backstage_keys(events);
            self.drain_requests();
            return;
        }
        let refused = self.live_typing_refused();
        if image_paste {
            self.paste_clipboard_image(refused);
        }
        for e in events {
            // Read-only or paused live presentation: typing, pasting and the editing keys are not
            // taken, and the person is told why (anything else that edits is put back by the
            // live session's own check).
            if let Some(why) = refused
                && matches!(
                    e,
                    egui::Event::Text(_)
                        | egui::Event::Paste(_)
                        | egui::Event::PasteImage(_)
                        | egui::Event::Cut
                        | egui::Event::Ime(egui::ImeEvent::Commit(_))
                        | egui::Event::Key { key: egui::Key::Backspace | egui::Key::Delete | egui::Key::Enter, pressed: true, .. }
                )
            {
                self.set_status(crate::live::tr(why));
                continue;
            }
            // An earlier command in this raw batch may have settled the composition. Request the
            // replacement at this ordered boundary, before the current event is considered.
            self.note_settled_composition(ctx);
            match e {
                egui::Event::ImeResetCompleted { request_id } => self.ime_reset_completed(request_id, ctx),
                egui::Event::PasteImage(image) => match color_image_png(&image) {
                    Ok(png) => {
                        let params = json!({"name": "Pasted image.png", "data": deckcraft_engine::cmd::base64_encode(&png)});
                        if let Err(e) = self.run_engine("insert.picture", &params) {
                            self.set_status(e);
                        }
                    }
                    Err(e) => self.set_status(e),
                },
                egui::Event::Text(t) => {
                    let revision = self.session.active().map(|d| d.revision);
                    if self.session.active().is_some_and(|d| d.selection.text.is_some() || d.selection.shapes.len() == 1)
                        && let Err(e) = self.session.type_text(&t)
                    {
                        self.set_status(e.to_string());
                    }
                    if self.session.active().map(|d| d.revision) != revision {
                        self.note_application_edit(ctx);
                    }
                }
                egui::Event::Ime(ime) => self.ime_event(ctx, &ime),
                egui::Event::Copy => self.publish_clipboard(ctx, false),
                egui::Event::Cut => self.publish_clipboard(ctx, true),
                egui::Event::Paste(text) => {
                    // Our own marker for objects without text: paste what the session copied.
                    let text = if text == OBJECTS_MARKER { String::new() } else { text };
                    // A picture (TIFF from Keynote, a screenshot) lands when the paste has no text.
                    // Text keeps its HTML (bold, italic, underline) when the host can read it.
                    let html = self.services.clipboard_html.as_mut().and_then(|f| f()).filter(|h| !h.is_empty());
                    // egui 0.37 sends `PasteImage` directly; the services fallback stays for
                    // integrations that still send an empty `Paste`, and for a browser's Copy
                    // Image, whose text is only the picture's link. Never insert twice.
                    let wants_picture = text.is_empty() || super::clipboard::names_a_picture(&text, html.as_deref());
                    let img = if wants_picture && !pasted_image_event { self.services.clipboard_image.as_mut().and_then(|f| f()) } else { None };
                    let revision = self.session.active().map(|d| d.revision);
                    let r = match img {
                        Some(png) => self
                            .run_engine("insert.picture", &json!({"name": "Pasted image.png", "data": deckcraft_engine::cmd::base64_encode(&png)})),
                        None => {
                            let mut params = json!({"text": text});
                            if let Some(h) = html {
                                params["html"] = Value::String(h);
                            }
                            self.run_engine("edit.paste", &params)
                        }
                    };
                    if let Err(e) = r {
                        self.set_status(e);
                    }
                    if self.session.active().map(|d| d.revision) != revision {
                        self.note_application_edit(ctx);
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if key == egui::Key::Tab {
                        // The slide has the keyboard, so Tab is ours (indent, tab character, next
                        // shape). Left to egui it would also focus the first ribbon widget, and
                        // every key after it would go there instead of the slide (#78).
                        ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
                    }
                    if key == egui::Key::Escape && self.anim_preview.is_some() && !modifiers.command && !modifiers.ctrl && !modifiers.alt {
                        self.anim_preview = None;
                        continue;
                    }
                    let mods = Mods { shift: modifiers.shift, alt: modifiers.alt, cmd: modifiers.command || modifiers.ctrl };
                    if !self.session.tool.drawing_freeform() && menus::ui_shortcut(self, key, mods) {
                        continue;
                    }
                    let name = key_name(key);
                    // Read-only or paused: a key that would edit is not taken (the gate's reason).
                    if self.refuse_key(name, mods, refused) {
                        continue;
                    }
                    let revision = self.session.active().map(|d| d.revision);
                    match self.session.key(name, mods) {
                        Ok(_) => {}
                        Err(e) => self.set_status(e.to_string()),
                    }
                    if self.session.active().map(|d| d.revision) != revision {
                        self.note_application_edit(ctx);
                        // Shared now, so each key's edit is its own group in the room.
                        self.flush_live();
                    }
                }
                _ => {}
            }
        }
        self.drain_requests();
    }
}

impl SlideApp {
    /// Pastes the host clipboard's picture, unless the live presentation takes no edits now.
    fn paste_clipboard_image(&mut self, refused: Option<&'static str>) {
        let Some(png) = self.services.clipboard_image.as_mut().and_then(|f| f()) else { return };
        if let Some(why) = refused {
            self.set_status(crate::live::tr(why));
            return;
        }
        let data = deckcraft_engine::cmd::base64_encode(&png);
        if let Err(e) = self.run_engine("insert.picture", &json!({"name": "Pasted image.png", "data": data})) {
            self.set_status(e);
        }
    }

    /// True (and says why) when key `name` would edit while the live presentation takes none.
    fn refuse_key(&mut self, name: &str, mods: Mods, refused: Option<&'static str>) -> bool {
        let why = match refused {
            Some(why) => self.key_refusal(name, mods, why),
            // Editing: a shortcut whose command the gate refuses now (the Delete key on slides in
            // a deck with sections, U1 review M7) is refused up front, with the reason.
            // A context key first: Enter on the slide with nothing selected adds a slide, though
            // the registry names Enter as Edit Text's shortcut (Review M2).
            None => match self.context_key_command(name, mods) {
                Some(id) => self.live_refusal(id),
                None => Self::shortcut_spec(name, mods).and_then(|s| self.live_refusal(s.id)),
            },
        };
        let Some(why) = why else { return false };
        self.set_status(crate::live::tr(why));
        true
    }

    /// The command the engine runs itself for key `name` in this context, whatever the registry
    /// names as the key's shortcut, when it edits the slide list: Enter with nothing selected adds
    /// a slide (`Session::key`).
    fn context_key_command(&self, name: &str, mods: Mods) -> Option<&'static str> {
        let st = self.session.active()?;
        // A freeform being drawn takes Enter first: the engine finishes it (U2 fix-1 review N2).
        let idle = st.selection.text.is_none() && st.selection.shapes.is_empty() && !mods.cmd && !self.session.tool.drawing_freeform();
        (idle && name == "Enter").then_some("slide.new")
    }

    /// The registered command whose shortcut key `name` with `mods` is.
    fn shortcut_spec(name: &str, mods: Mods) -> Option<&'static deckcraft_engine::CommandSpec> {
        deckcraft_engine::command_specs().iter().find(|s| s.shortcut.is_some_and(|k| deckcraft_engine::keys::shortcut_matches(k, name, mods)))
    }

    /// Why key `name` is not taken while the live presentation takes no edits (`why`), when it
    /// would edit: a shortcut for a command the gate refuses, or any other modifier shortcut, an
    /// arrow that nudges the selected shapes, or Tab inside text.
    fn key_refusal(&self, name: &str, mods: Mods, why: &'static str) -> Option<&'static str> {
        let st = self.session.active()?;
        let editing = st.selection.text.is_some();
        let nudges = !editing && !st.selection.shapes.is_empty();
        let arrow = matches!(name, "Left" | "Right" | "Up" | "Down");
        // Every registered shortcut, with or without Cmd (Shift+F3, Alt+Shift+V), goes by its
        // command's gate.
        let spec = Self::shortcut_spec(name, mods);
        match spec.map(|s| self.live_refusal(s.id)) {
            Some(Some(refused)) => return Some(refused),
            Some(None) if mods.cmd => return None,
            // Moving the caret or the view by word or page edits nothing.
            None if mods.cmd => return (!arrow && !matches!(name, "Home" | "End" | "PageUp" | "PageDown")).then_some(why),
            // A key the engine also uses itself (an arrow nudges shapes): the rules below.
            _ => {}
        }
        ((arrow && nudges) || (editing && name == "Tab")).then_some(why)
    }
}

/// Engine key name for an egui key.
pub fn key_name(key: egui::Key) -> &'static str {
    use egui::Key::*;
    match key {
        Enter => "Enter",
        Escape => "Escape",
        Backspace => "Backspace",
        Delete => "Delete",
        Tab => "Tab",
        ArrowLeft => "Left",
        ArrowRight => "Right",
        ArrowUp => "Up",
        ArrowDown => "Down",
        Home => "Home",
        End => "End",
        PageUp => "PageUp",
        PageDown => "PageDown",
        Space => "Space",
        F2 => "F2",
        F5 => "F5",
        F7 => "F7",
        other => other.name(),
    }
}

/// Largest clipboard image accepted into the document: 16 megapixels, the same bound as
/// background removal. The image was already allocated by the integration; this keeps the
/// RGBA copy, PNG and base64 from multiplying an arbitrarily large paste before `insert.picture`.
const MAX_PASTE_IMAGE_PIXELS: usize = 16_000_000;

/// Encode an egui color image as the PNG bytes `insert.picture` stores.
fn color_image_png(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let [w, h] = image.size;
    let pixels = w.checked_mul(h).ok_or_else(|| "the pasted image is too large".to_string())?;
    if pixels == 0 {
        return Err("the pasted image is empty".into());
    }
    if pixels > MAX_PASTE_IMAGE_PIXELS {
        return Err(format!("the pasted image is {w}×{h} pixels; the limit is 16 megapixels"));
    }
    let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    Ok(deckcraft_render::Image { width: w as u32, height: h as u32, pixels: rgba }.to_png())
}
