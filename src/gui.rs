use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use eframe::egui::{
    self, Color32, ColorImage, Event, Key, PointerButton, Pos2, Rect, Sense, TextureHandle,
    TextureOptions, Vec2,
};

use crate::{
    hid,
    keymap::{self, Layout},
    scsi::MediaKind,
    viewer::{
        ConnectionState, DecodedFrame, MediaStatus, Source, ViewerCommand, ViewerHandle,
        spawn_viewer,
    },
};

const USB_LEFT_CTRL: u8 = 0x01;
const USB_LEFT_ALT: u8 = 0x04;
const USAGE_DELETE: u8 = 0x4c;

pub struct IlomApp {
    viewer: Option<ViewerApp>,
    host: String,
    username: String,
    password: String,
    capture_dir: PathBuf,
    form_error: Option<String>,
}

impl IlomApp {
    pub fn new(
        host: Option<String>,
        username: String,
        password: Option<String>,
        capture_dir: PathBuf,
        initial: Option<Source>,
        context: &egui::Context,
    ) -> Self {
        let viewer = initial.map(|source| ViewerApp::new(context, source, capture_dir.clone()));
        Self {
            viewer,
            host: host.unwrap_or_default(),
            username,
            password: password.unwrap_or_default(),
            capture_dir,
            form_error: None,
        }
    }

    fn show_login(&mut self, ui: &mut egui::Ui) -> Option<Source> {
        let mut connect = false;
        let mut open_jnlp = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::from_rgb(18, 18, 20)))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.14).max(24.0));
                    ui.heading("ILOM Remote Console");
                    ui.label("Java-free client for the Oracle ILOM Remote System Console");
                    ui.add_space(18.0);

                    egui::Frame::group(ui.style())
                        .fill(Color32::from_rgb(28, 28, 32))
                        .inner_margin(egui::Margin::same(20))
                        .show(ui, |ui| {
                            ui.set_width(420.0);
                            ui.label("ILOM address");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.host)
                                    .hint_text("192.0.2.10")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.add_space(10.0);
                            ui.label("Username");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.username)
                                    .desired_width(f32::INFINITY),
                            );
                            ui.add_space(10.0);
                            ui.label("Password");
                            let password_response = ui.add(
                                egui::TextEdit::singleline(&mut self.password)
                                    .password(true)
                                    .desired_width(f32::INFINITY),
                            );
                            ui.add_space(14.0);
                            connect = ui
                                .add_sized(
                                    [ui.available_width(), 34.0],
                                    egui::Button::new("Connect"),
                                )
                                .clicked()
                                || (password_response.lost_focus()
                                    && ui.input(|input| input.key_pressed(Key::Enter)));
                            ui.add_space(6.0);
                            open_jnlp = ui
                                .add_sized(
                                    [ui.available_width(), 28.0],
                                    egui::Button::new("Open downloaded JNLP…"),
                                )
                                .clicked();
                            if let Some(error) = &self.form_error {
                                ui.add_space(8.0);
                                ui.colored_label(Color32::from_rgb(235, 90, 90), error);
                            }
                        });
                    ui.add_space(12.0);
                    ui.small("Credentials are kept in memory only and are not saved by the app.");
                });
            });

        if open_jnlp {
            return rfd::FileDialog::new()
                .set_title("Open ILOM console JNLP")
                .pick_file()
                .map(Source::Jnlp);
        }
        if !connect {
            return None;
        }
        match self.web_source() {
            Ok(source) => {
                self.form_error = None;
                Some(source)
            }
            Err(error) => {
                self.form_error = Some(error);
                None
            }
        }
    }

    fn web_source(&self) -> Result<Source, String> {
        let host = self
            .host
            .trim()
            .trim_start_matches("https://")
            .trim_end_matches('/');
        if host.is_empty() {
            return Err("Enter the ILOM address".into());
        }
        if self.username.trim().is_empty() {
            return Err("Enter a username".into());
        }
        if self.password.is_empty() {
            return Err("Enter a password".into());
        }
        Ok(Source::Web {
            host: host.to_owned(),
            username: self.username.trim().to_owned(),
            password: self.password.clone(),
        })
    }
}

impl eframe::App for IlomApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if let Some(viewer) = self.viewer.as_mut() {
            eframe::App::ui(viewer, ui, frame);
            if viewer.disconnect_requested {
                self.viewer = None;
            }
            return;
        }
        if let Some(source) = self.show_login(ui) {
            self.viewer = Some(ViewerApp::new(ui.ctx(), source, self.capture_dir.clone()));
        }
    }
}

pub struct ViewerApp {
    handle: ViewerHandle,
    layout: Layout,
    texture: Option<TextureHandle>,
    displayed_frame: Option<Arc<DecodedFrame>>,
    pressed_usages: BTreeSet<u8>,
    modifiers: u8,
    mouse_buttons: u8,
    fullscreen: bool,
    notice: Option<String>,
    capture_dir: PathBuf,
    disconnect_requested: bool,
    /// Let the host write to the next floppy image mounted.
    floppy_writable: bool,
}

impl ViewerApp {
    pub fn new(context: &egui::Context, source: Source, capture_dir: PathBuf) -> Self {
        let context = context.clone();
        let handle = spawn_viewer(source, move || context.request_repaint());
        Self {
            handle,
            layout: Layout::from_locale(),
            texture: None,
            displayed_frame: None,
            pressed_usages: BTreeSet::new(),
            modifiers: 0,
            mouse_buttons: 0,
            fullscreen: false,
            notice: None,
            capture_dir,
            disconnect_requested: false,
            floppy_writable: false,
        }
    }

    fn refresh_texture(&mut self, ctx: &egui::Context) {
        let latest = self
            .handle
            .shared
            .latest_frame
            .lock()
            .ok()
            .and_then(|frame| frame.clone());
        let Some(frame) = latest else { return };
        if self
            .displayed_frame
            .as_ref()
            .is_some_and(|displayed| Arc::ptr_eq(displayed, &frame))
        {
            return;
        }
        let image = ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match self.texture.as_mut() {
            Some(texture) => texture.set(image, TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture("ILOM framebuffer", image, TextureOptions::LINEAR));
            }
        }
        self.displayed_frame = Some(frame);
    }

    fn send(&self, command: ViewerCommand) {
        let _ = self.handle.commands.send(command);
    }

    fn send_keyboard(&self) {
        self.send(ViewerCommand::Keyboard {
            modifiers: self.modifiers,
            usages: self.pressed_usages.iter().copied().take(6).collect(),
        });
    }

    fn release_input(&mut self) {
        if !self.pressed_usages.is_empty() || self.modifiers != 0 {
            self.pressed_usages.clear();
            self.modifiers = 0;
            self.send_keyboard();
        }
        self.mouse_buttons = 0;
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context, focused: bool) {
        let events = ctx.input(|input| input.events.clone());
        if !focused
            || events
                .iter()
                .any(|event| matches!(event, Event::WindowFocused(false)))
        {
            self.release_input();
            return;
        }

        // Modifiers come from physical key events so that left and right
        // keys stay distinct: AltGr must reach the host as Right Alt, which
        // egui's merged `Modifiers` (and winit on Linux) cannot express.
        let mut changed = false;
        for event in events {
            if let Event::Key {
                key,
                physical_key,
                pressed,
                repeat,
                ..
            } = event
            {
                // USB usages name physical positions; the host applies its
                // own layout, so prefer the physical key when available.
                let key = physical_key.unwrap_or(key);
                if let Some(bit) = modifier_bit(key) {
                    let modifiers = if pressed {
                        self.modifiers | bit
                    } else {
                        self.modifiers & !bit
                    };
                    changed |= modifiers != self.modifiers;
                    self.modifiers = modifiers;
                    continue;
                }
                let Some(usage) = key_to_usage(key) else {
                    continue;
                };
                if pressed {
                    if !repeat {
                        changed |= self.pressed_usages.insert(usage);
                    }
                } else {
                    changed |= self.pressed_usages.remove(&usage);
                }
            }
        }
        if changed {
            self.send_keyboard();
        }
    }

    fn handle_mouse(&mut self, ctx: &egui::Context, image_rect: Rect, focused: bool) {
        let Some(frame) = self.displayed_frame.clone() else {
            return;
        };
        let events = ctx.input(|input| input.events.clone());
        for event in events {
            match event {
                Event::PointerMoved(pos)
                    if focused && (image_rect.contains(pos) || self.mouse_buttons != 0) =>
                {
                    self.send_mouse(pos, image_rect, &frame);
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } if image_rect.contains(pos) || self.mouse_buttons != 0 => {
                    let bit = match button {
                        PointerButton::Primary => hid::BUTTON_LEFT,
                        PointerButton::Secondary => hid::BUTTON_RIGHT,
                        PointerButton::Middle => hid::BUTTON_MIDDLE,
                        _ => 0,
                    };
                    if pressed {
                        self.mouse_buttons |= bit;
                    } else {
                        self.mouse_buttons &= !bit;
                    }
                    self.send_mouse(pos, image_rect, &frame);
                }
                _ => {}
            }
        }
    }

    fn send_mouse(&self, pos: Pos2, rect: Rect, frame: &DecodedFrame) {
        let normalized_x = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let normalized_y = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        let x = (normalized_x * frame.width.saturating_sub(1) as f32).round() as u32;
        let y = (normalized_y * frame.height.saturating_sub(1) as f32).round() as u32;
        self.send(ViewerCommand::MouseAbsolute {
            buttons: self.mouse_buttons,
            x,
            y,
            width: frame.width.saturating_sub(1),
            height: frame.height.saturating_sub(1),
        });
    }

    /// Types the clipboard text into the host using the selected layout.
    fn paste_clipboard(&mut self) {
        let text = match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) => text,
            Err(error) => {
                self.notice = Some(format!("Clipboard unavailable: {error}"));
                return;
            }
        };
        const MAX_PASTE: usize = 10_000;
        if text.chars().count() > MAX_PASTE {
            self.notice = Some(format!(
                "Clipboard text is longer than {MAX_PASTE} characters"
            ));
            return;
        }
        let (strokes, skipped) = keymap::text_to_strokes(self.layout, &text);
        let mut notice = format!(
            "Typing {} characters ({})",
            strokes.len(),
            self.layout.label()
        );
        if !skipped.is_empty() {
            let sample: String = skipped.iter().take(10).collect();
            notice.push_str(&format!(
                "; skipped {} unsupported: {sample:?}",
                skipped.len()
            ));
        }
        self.notice = Some(notice);
        self.send(ViewerCommand::TypeStrokes(strokes));
    }

    fn media_menu(&mut self, ui: &mut egui::Ui, kind: MediaKind, media: &MediaStatus) {
        let title = match kind {
            MediaKind::Cdrom => "CD-ROM",
            MediaKind::Floppy => "Floppy/USB",
        };
        let label = match (&media.image, media.active) {
            (Some(name), true) => format!("💿 {title}: {name}"),
            (Some(name), false) => format!("⏳ {title}: {name}"),
            (None, _) => format!("{title}…"),
        };
        let mut text = egui::RichText::new(label);
        if media.error.is_some() {
            text = text.color(Color32::from_rgb(235, 90, 90));
        } else if media.active {
            text = text.color(Color32::from_rgb(80, 200, 120));
        }
        ui.menu_button(text, |ui| {
            if kind == MediaKind::Floppy {
                ui.checkbox(&mut self.floppy_writable, "Allow the host to write");
            }
            let pick = if media.image.is_some() {
                "Change image…"
            } else {
                "Mount image…"
            };
            if ui.button(pick).clicked() {
                ui.close();
                let dialog = rfd::FileDialog::new().set_title(format!("Mount {title} image"));
                let dialog = match kind {
                    MediaKind::Cdrom => dialog.add_filter("ISO image", &["iso"]),
                    MediaKind::Floppy => dialog.add_filter("Disk image", &["img", "ima", "bin"]),
                };
                if let Some(path) = dialog.add_filter("All files", &["*"]).pick_file() {
                    self.send(ViewerCommand::Mount {
                        kind,
                        path,
                        writable: self.floppy_writable,
                    });
                }
            }
            if media.image.is_some() && ui.button("Unmount").clicked() {
                ui.close();
                self.send(ViewerCommand::Unmount(kind));
            }
            if media.image.is_some() {
                ui.separator();
                ui.label(if media.active {
                    "Redirected to the host"
                } else {
                    "Connecting…"
                });
                if media.writable {
                    ui.label("Host writes allowed");
                }
                ui.label(format!(
                    "{} commands, {} read, {} written",
                    media.stats.commands,
                    human_bytes(media.stats.bytes_read),
                    human_bytes(media.stats.bytes_written)
                ));
            }
            if let Some(error) = &media.error {
                ui.colored_label(Color32::from_rgb(235, 90, 90), error);
            }
        })
        .response
        .on_hover_text(format!(
            "Redirect a local image to the host's virtual {title} drive"
        ));
    }

    fn save_screenshot(&mut self) {
        let Some(frame) = self.displayed_frame.as_ref() else {
            self.notice = Some("No frame is available yet".into());
            return;
        };
        let result = (|| -> anyhow::Result<PathBuf> {
            std::fs::create_dir_all(&self.capture_dir)?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default();
            let path = self.capture_dir.join(format!("ilom-{stamp}.png"));
            image::save_buffer(
                &path,
                &frame.rgba,
                frame.width,
                frame.height,
                image::ColorType::Rgba8,
            )?;
            Ok(path)
        })();
        self.notice = Some(match result {
            Ok(path) => format!("Screenshot saved: {}", path.display()),
            Err(error) => format!("Screenshot failed: {error:#}"),
        });
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.refresh_texture(&ctx);
        let status = self
            .handle
            .shared
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default();

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let color = match status.state {
                    ConnectionState::Connected => Color32::from_rgb(80, 200, 120),
                    ConnectionState::Error => Color32::from_rgb(235, 90, 90),
                    _ => Color32::from_rgb(235, 185, 70),
                };
                ui.colored_label(color, format!("● {}", status.message));
                if let Some(frame) = &self.displayed_frame {
                    ui.separator();
                    ui.label(format!("{}×{}", frame.width, frame.height));
                }
                ui.separator();
                ui.label(if status.keyboard {
                    "Keyboard/mouse on"
                } else {
                    "Keyboard/mouse off"
                });
                ui.separator();
                if ui
                    .add_enabled(status.keyboard, egui::Button::new("Ctrl+Alt+Del"))
                    .clicked()
                {
                    self.send(ViewerCommand::Keystroke {
                        modifiers: USB_LEFT_CTRL | USB_LEFT_ALT,
                        usage: USAGE_DELETE,
                    });
                }
                if let Some(leds) = status.leds {
                    for (label, bit, usage) in [
                        ("NUM", hid::LED_NUM_LOCK, hid::USAGE_NUM_LOCK),
                        ("CAPS", hid::LED_CAPS_LOCK, hid::USAGE_CAPS_LOCK),
                        ("SCROLL", hid::LED_SCROLL_LOCK, hid::USAGE_SCROLL_LOCK),
                    ] {
                        let on = leds & bit != 0;
                        let text = egui::RichText::new(label).monospace().color(if on {
                            Color32::from_rgb(80, 200, 120)
                        } else {
                            Color32::from_gray(110)
                        });
                        let clicked = ui
                            .add(egui::Button::new(text).selected(on))
                            .on_hover_text(format!(
                                "Host {label} lock is {}; click to toggle",
                                if on { "on" } else { "off" }
                            ))
                            .clicked();
                        if clicked {
                            self.send(ViewerCommand::Keystroke {
                                modifiers: 0,
                                usage,
                            });
                            self.send(ViewerCommand::Keyboard {
                                modifiers: 0,
                                usages: Vec::new(),
                            });
                        }
                    }
                    ui.separator();
                }
                if ui
                    .small_button("Stop typing")
                    .on_hover_text("Abort a paste that is still being typed")
                    .clicked()
                {
                    self.handle
                        .shared
                        .cancel_typing
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                if ui
                    .add_enabled(status.keyboard, egui::Button::new("Paste text"))
                    .on_hover_text("Type the clipboard text on the host")
                    .clicked()
                {
                    self.paste_clipboard();
                }
                egui::ComboBox::from_id_salt("host-layout")
                    .selected_text(self.layout.label())
                    .show_ui(ui, |ui| {
                        for layout in Layout::ALL {
                            ui.selectable_value(&mut self.layout, layout, layout.label());
                        }
                    })
                    .response
                    .on_hover_text("Keyboard layout configured on the host (used for pasting)");
                ui.separator();
                for kind in [MediaKind::Cdrom, MediaKind::Floppy] {
                    self.media_menu(ui, kind, status.media(kind));
                }
                ui.separator();
                if ui.button("Screenshot").clicked() {
                    self.save_screenshot();
                }
                if ui
                    .button(if self.fullscreen {
                        "Exit fullscreen"
                    } else {
                        "Fullscreen"
                    })
                    .clicked()
                {
                    self.fullscreen = !self.fullscreen;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                }
                if ui.button("Disconnect").clicked() {
                    self.disconnect_requested = true;
                }
            });
            if let Some(notice) = &self.notice {
                ui.small(notice);
            }
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::from_rgb(18, 18, 20)))
            .show(ui, |ui| {
                let available = ui.available_rect_before_wrap();
                let (Some(texture), Some(frame)) = (&self.texture, &self.displayed_frame) else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                        ui.label("Waiting for the first frame…");
                    });
                    return;
                };
                let scale = (available.width() / frame.width as f32)
                    .min(available.height() / frame.height as f32)
                    .max(0.01);
                let size = Vec2::new(frame.width as f32 * scale, frame.height as f32 * scale);
                let rect = Rect::from_center_size(available.center(), size);
                let response = ui.put(
                    rect,
                    egui::Image::new((texture.id(), size)).sense(Sense::click_and_drag()),
                );
                if response.clicked() || response.drag_started() {
                    response.request_focus();
                }
                let focused = response.has_focus();
                if focused {
                    // Keep Tab and arrows for the host instead of egui focus moves.
                    ui.memory_mut(|memory| {
                        memory.set_focus_lock_filter(
                            response.id,
                            egui::EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                escape: true,
                            },
                        )
                    });
                    ui.painter().rect_stroke(
                        rect,
                        0,
                        egui::Stroke::new(1.0, Color32::from_rgb(90, 160, 255)),
                        egui::StrokeKind::Inside,
                    );
                }
                self.handle_keyboard(&ctx, focused);
                self.handle_mouse(&ctx, rect, focused);
            });
    }
}

impl Drop for ViewerApp {
    fn drop(&mut self) {
        self.release_input();
        self.handle.stop_and_wait();
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// USB HID modifier byte bit for a physical modifier key.
fn modifier_bit(key: Key) -> Option<u8> {
    Some(match key {
        Key::ControlLeft => 0x01,
        Key::ShiftLeft => 0x02,
        Key::AltLeft => 0x04,
        Key::SuperLeft => 0x08,
        Key::ControlRight => 0x10,
        Key::ShiftRight => 0x20,
        Key::AltRight => 0x40,
        Key::SuperRight => 0x80,
        _ => return None,
    })
}

fn key_to_usage(key: Key) -> Option<u8> {
    Some(match key {
        Key::A => 0x04,
        Key::B => 0x05,
        Key::C => 0x06,
        Key::D => 0x07,
        Key::E => 0x08,
        Key::F => 0x09,
        Key::G => 0x0a,
        Key::H => 0x0b,
        Key::I => 0x0c,
        Key::J => 0x0d,
        Key::K => 0x0e,
        Key::L => 0x0f,
        Key::M => 0x10,
        Key::N => 0x11,
        Key::O => 0x12,
        Key::P => 0x13,
        Key::Q => 0x14,
        Key::R => 0x15,
        Key::S => 0x16,
        Key::T => 0x17,
        Key::U => 0x18,
        Key::V => 0x19,
        Key::W => 0x1a,
        Key::X => 0x1b,
        Key::Y => 0x1c,
        Key::Z => 0x1d,
        Key::Num1 => 0x1e,
        Key::Num2 => 0x1f,
        Key::Num3 => 0x20,
        Key::Num4 => 0x21,
        Key::Num5 => 0x22,
        Key::Num6 => 0x23,
        Key::Num7 => 0x24,
        Key::Num8 => 0x25,
        Key::Num9 => 0x26,
        Key::Num0 => 0x27,
        Key::Enter => 0x28,
        Key::Escape => 0x29,
        Key::Backspace => 0x2a,
        Key::Tab => 0x2b,
        Key::Space => 0x2c,
        Key::Minus => 0x2d,
        Key::Equals | Key::Plus => 0x2e,
        Key::OpenBracket => 0x2f,
        Key::CloseBracket => 0x30,
        Key::Backslash | Key::Pipe => 0x31,
        Key::Semicolon | Key::Colon => 0x33,
        Key::Quote => 0x34,
        Key::Backtick => 0x35,
        Key::Comma => 0x36,
        Key::Period => 0x37,
        Key::Slash | Key::Questionmark => 0x38,
        Key::F1 => 0x3a,
        Key::F2 => 0x3b,
        Key::F3 => 0x3c,
        Key::F4 => 0x3d,
        Key::F5 => 0x3e,
        Key::F6 => 0x3f,
        Key::F7 => 0x40,
        Key::F8 => 0x41,
        Key::F9 => 0x42,
        Key::F10 => 0x43,
        Key::F11 => 0x44,
        Key::F12 => 0x45,
        Key::Insert => 0x49,
        Key::Home => 0x4a,
        Key::PageUp => 0x4b,
        Key::Delete => 0x4c,
        Key::End => 0x4d,
        Key::PageDown => 0x4e,
        Key::ArrowRight => 0x4f,
        Key::ArrowLeft => 0x50,
        Key::ArrowDown => 0x51,
        Key::ArrowUp => 0x52,
        // ISO key left of Z (`<>` on AZERTY/QWERTZ).
        Key::IntlBackslash => 0x64,
        Key::F13 => 0x68,
        // Keypad and lock keys, delivered on F14–F35 by the patched
        // egui-winit in third_party/egui-winit.
        Key::F14 => 0x65, // Menu
        Key::F15 => 0x48, // Pause
        Key::F16 => 0x46, // PrintScreen
        Key::F17 => 0x47, // ScrollLock
        Key::F18 => 0x39, // CapsLock
        Key::F19 => 0x53, // NumLock
        Key::F20 => 0x58, // Keypad Enter
        Key::F21 => 0x54, // Keypad /
        Key::F22 => 0x55, // Keypad *
        Key::F23 => 0x56, // Keypad -
        Key::F24 => 0x57, // Keypad +
        Key::F25 => 0x62, // Keypad 0
        Key::F26 => 0x59, // Keypad 1
        Key::F27 => 0x5a,
        Key::F28 => 0x5b,
        Key::F29 => 0x5c,
        Key::F30 => 0x5d,
        Key::F31 => 0x5e,
        Key::F32 => 0x5f,
        Key::F33 => 0x60,
        Key::F34 => 0x61, // Keypad 9
        Key::F35 => 0x63, // Keypad .
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_key_mappings_match_usb_hid() {
        assert_eq!(key_to_usage(Key::A), Some(0x04));
        assert_eq!(key_to_usage(Key::Enter), Some(0x28));
        assert_eq!(key_to_usage(Key::F12), Some(0x45));
        assert_eq!(key_to_usage(Key::ArrowUp), Some(0x52));
    }

    #[test]
    fn modifier_keys_map_to_usb_bits() {
        assert_eq!(modifier_bit(Key::ControlLeft), Some(USB_LEFT_CTRL));
        assert_eq!(modifier_bit(Key::AltLeft), Some(USB_LEFT_ALT));
        assert_eq!(modifier_bit(Key::AltRight), Some(0x40));
        assert_eq!(modifier_bit(Key::A), None);
        assert_eq!(key_to_usage(Key::IntlBackslash), Some(0x64));
        assert_eq!(key_to_usage(Key::F26), Some(0x59));
        assert_eq!(key_to_usage(Key::F25), Some(0x62));
    }
}
