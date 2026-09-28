use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Color32, ColorImage, Event, Key, PointerButton, Pos2, Rect, Sense, TextureHandle,
    TextureOptions, Vec2,
};

use ilom_kvm_core::{
    hid,
    keymap::{self, Layout},
    scsi::MediaKind,
    session::{
        ConnectionState, DecodedFrame, HostCursor, MediaStatus, Source, ViewerCommand,
        ViewerHandle, ViewerStatus, spawn_viewer,
    },
    video,
};

use crate::{clipboard::Clipboard, settings::Settings};

const USB_LEFT_CTRL: u8 = 0x01;
const USB_LEFT_ALT: u8 = 0x04;
const USB_LEFT_GUI: u8 = 0x08;
const USAGE_A: u8 = 0x04;
const USAGE_BACKSPACE: u8 = 0x2a;
const USAGE_TAB: u8 = 0x2b;
const USAGE_F1: u8 = 0x3a;
const USAGE_F4: u8 = 0x3d;
const USAGE_PRINT_SCREEN: u8 = 0x46;
const USAGE_DELETE: u8 = 0x4c;
/// Image file extensions offered for each virtual drive, also used to pick
/// the drive for a dropped file.
const CDROM_EXTENSIONS: &[&str] = &["iso"];
const FLOPPY_EXTENSIONS: &[&str] = &["img", "ima", "bin"];

/// Host lock keys: toolbar label, LED bit and USB usage.
const LOCK_KEYS: [(&str, u8, u8); 3] = [
    ("NUM", hid::LED_NUM_LOCK, hid::USAGE_NUM_LOCK),
    ("CAPS", hid::LED_CAPS_LOCK, hid::USAGE_CAPS_LOCK),
    ("SCROLL", hid::LED_SCROLL_LOCK, hid::USAGE_SCROLL_LOCK),
];
/// Magic SysRq commands offered in the "Send keys" menu.
const SYSRQ_KEYS: [(char, &str); 8] = [
    ('h', "Help"),
    ('s', "Sync disks"),
    ('u', "Remount read-only"),
    ('e', "Terminate all tasks"),
    ('i', "Kill all tasks"),
    ('r', "Keyboard raw mode off"),
    ('b', "Reboot now"),
    ('o', "Power off now"),
];
const APP_TITLE: &str = "ILOM Remote Console";
/// How long a toolbar notice stays visible.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(6);

pub struct IlomApp {
    viewer: Option<ViewerApp>,
    host: String,
    username: String,
    password: String,
    capture_dir: PathBuf,
    prefs: ViewerPrefs,
    /// Remembered choices, rewritten when the user changes one in the GUI.
    settings: Settings,
    settings_path: Option<PathBuf>,
    form_error: Option<String>,
}

/// Viewer choices the user can change in the menus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewerPrefs {
    pub host_key: HostKey,
    /// Keyboard layout configured on the host, used for pasting.
    pub layout: Layout,
    /// Show host pixels 1:1 instead of fitting the window.
    pub actual_size: bool,
}

/// Values the GUI starts with, already merged from options and settings.
pub struct Startup {
    pub host: Option<String>,
    pub username: String,
    pub password: Option<String>,
    pub capture_dir: PathBuf,
    pub prefs: ViewerPrefs,
    pub settings: Settings,
    pub settings_path: Option<PathBuf>,
}

impl IlomApp {
    pub fn new(startup: Startup, initial: Option<Source>, context: &egui::Context) -> Self {
        let viewer = initial.map(|source| {
            ViewerApp::new(context, source, startup.capture_dir.clone(), startup.prefs)
        });
        Self {
            viewer,
            host: startup.host.unwrap_or_default(),
            username: startup.username,
            password: startup.password.unwrap_or_default(),
            capture_dir: startup.capture_dir,
            prefs: startup.prefs,
            settings: startup.settings,
            settings_path: startup.settings_path,
            form_error: None,
        }
    }

    fn save_settings(&self) {
        let Some(path) = &self.settings_path else {
            return;
        };
        if let Err(error) = self.settings.save(path) {
            tracing::warn!(error = %format!("{error:#}"), "could not save settings");
        }
    }

    /// Remembers choices made in the viewer menus.
    fn remember_viewer_choices(&mut self, prefs: ViewerPrefs) {
        if prefs == self.prefs {
            return;
        }
        self.prefs = prefs;
        self.settings.host_key = clap::ValueEnum::to_possible_value(&prefs.host_key)
            .map(|value| value.get_name().to_owned());
        self.settings.layout = Some(prefs.layout.id().to_owned());
        self.settings.actual_size = Some(prefs.actual_size);
        self.save_settings();
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
                            let host_response = ui.add(
                                egui::TextEdit::singleline(&mut self.host)
                                    .hint_text("192.0.2.10")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.add_space(10.0);
                            ui.label("Username");
                            let username_response = ui.add(
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
                                // Enter in any field submits, like a web form.
                                || ([host_response, username_response, password_response]
                                    .iter()
                                    .any(egui::Response::lost_focus)
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
                if let Source::Web { host, username, .. } = &source {
                    self.settings.host = Some(host.clone());
                    self.settings.username = Some(username.clone());
                    self.save_settings();
                }
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
            let prefs = viewer.prefs;
            let rejected = viewer.rejection();
            let login = viewer.login.clone();
            let leaving = viewer.disconnect_requested || rejected.is_some();
            self.remember_viewer_choices(prefs);
            if leaving {
                // A rejected session goes back to the form instead of retrying.
                if let Some((credentials, message)) = rejected {
                    if let Some((host, username)) = login {
                        self.host = host;
                        self.username = username;
                    }
                    self.form_error = Some(if credentials {
                        self.password.clear();
                        format!("Login rejected; check the username and password ({message})")
                    } else {
                        message
                    });
                }
                self.viewer = None;
                let ctx = ui.ctx();
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(APP_TITLE.into()));
                ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(
                    egui::viewport::CursorGrab::None,
                ));
                ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
            }
            return;
        }
        if let Some(source) = self.show_login(ui) {
            self.viewer = Some(ViewerApp::new(
                ui.ctx(),
                source,
                self.capture_dir.clone(),
                self.prefs,
            ));
        }
    }
}

/// Client key never sent to the host; see [`ViewerApp::handle_keyboard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HostKey {
    RightCtrl,
    RightSuper,
    Menu,
    ScrollLock,
    /// No host key: every key goes to the host.
    None,
}

impl Default for HostKey {
    /// Mac laptop keyboards have no Right Ctrl; Right Cmd is their spare key.
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Self::RightSuper
        } else {
            Self::RightCtrl
        }
    }
}

impl HostKey {
    const ALL: [Self; 5] = [
        Self::RightCtrl,
        Self::RightSuper,
        Self::Menu,
        Self::ScrollLock,
        Self::None,
    ];

    fn key(self) -> Option<Key> {
        Some(match self {
            Self::RightCtrl => Key::ControlRight,
            Self::RightSuper => Key::SuperRight,
            // Menu and Scroll Lock arrive as F14/F17 from the patched egui-winit.
            Self::Menu => Key::F14,
            Self::ScrollLock => Key::F17,
            Self::None => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::RightCtrl => "Right Ctrl",
            Self::RightSuper if cfg!(target_os = "macos") => "Right Cmd",
            Self::RightSuper => "Right Super",
            Self::Menu => "Menu",
            Self::ScrollLock => "Scroll Lock",
            Self::None => "None",
        }
    }

    fn help(self) -> String {
        match self {
            Self::None => "No host key: click the screen to capture the keyboard".into(),
            _ => {
                let key = self.label();
                format!(
                    "{key}: capture or release the keyboard\n\
                     {key}+F: fullscreen\n\
                     {key}+V: paste text\n\
                     {key}+Del: Ctrl+Alt+Del"
                )
            }
        }
    }
}

/// Client shortcut run with the host key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostAction {
    ToggleCapture,
    Fullscreen,
    Paste,
    CtrlAltDel,
}

impl HostAction {
    fn for_key(key: Key) -> Option<Self> {
        Some(match key {
            Key::F => Self::Fullscreen,
            Key::V => Self::Paste,
            Key::Delete => Self::CtrlAltDel,
            _ => return None,
        })
    }
}

/// How the user asked to leave the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Disconnect,
    Quit,
}

pub struct ViewerApp {
    handle: ViewerHandle,
    /// Host and username of a web login, to refill the form after a rejection.
    login: Option<(String, String)>,
    /// Choices made in the menus, remembered between runs.
    pub prefs: ViewerPrefs,
    texture: Option<TextureHandle>,
    texture_options: TextureOptions,
    /// Host cursor image, keyed by cursor serial, frame and filter, with its
    /// place in host pixels.
    cursor_texture: Option<((u64, u64, TextureOptions), TextureHandle, Rect)>,
    displayed_frame: Option<Arc<DecodedFrame>>,
    pressed_usages: BTreeSet<u8>,
    modifiers: u8,
    mouse_buttons: u8,
    /// Unsent relative-mode motion, in host pixels.
    relative_motion: Vec2,
    cursor_grabbed: bool,
    /// Fullscreen toolbar floating over the video, when shown.
    toolbar_overlay: Option<Rect>,
    notice: Option<(String, Instant)>,
    capture_dir: PathBuf,
    /// Opened on the first frame, which gives the window's display handle.
    clipboard: Option<Clipboard>,
    disconnect_requested: bool,
    /// Exit waiting for confirmation because images are still mounted.
    pending_exit: Option<Exit>,
    quit_confirmed: bool,
    /// Framebuffer widget holding keyboard focus in the last frame, if any.
    captured_by: Option<egui::Id>,
    /// Host key held down; `true` once it was used in a shortcut.
    host_key: Option<bool>,
    /// Let the host write to the next floppy image mounted.
    floppy_writable: bool,
}

impl ViewerApp {
    pub fn new(
        context: &egui::Context,
        source: Source,
        capture_dir: PathBuf,
        prefs: ViewerPrefs,
    ) -> Self {
        context.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "{} — {APP_TITLE}",
            source_host(&source)
        )));
        let login = match &source {
            Source::Web { host, username, .. } => Some((host.clone(), username.clone())),
            Source::Jnlp(_) => None,
        };
        let context = context.clone();
        let handle = spawn_viewer(source, move || context.request_repaint());
        Self {
            handle,
            login,
            prefs,
            texture: None,
            texture_options: TextureOptions::LINEAR,
            cursor_texture: None,
            displayed_frame: None,
            pressed_usages: BTreeSet::new(),
            modifiers: 0,
            mouse_buttons: 0,
            relative_motion: Vec2::ZERO,
            cursor_grabbed: false,
            toolbar_overlay: None,
            notice: None,
            capture_dir,
            clipboard: None,
            disconnect_requested: false,
            pending_exit: None,
            quit_confirmed: false,
            captured_by: None,
            host_key: None,
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
        self.upload_texture(ctx, &frame);
        self.displayed_frame = Some(frame);
    }

    fn upload_texture(&mut self, ctx: &egui::Context, frame: &DecodedFrame) {
        let image = ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match self.texture.as_mut() {
            Some(texture) => texture.set(image, self.texture_options),
            None => {
                self.texture =
                    Some(ctx.load_texture("ILOM framebuffer", image, self.texture_options));
            }
        }
    }

    /// Mounts image files dropped on the window, choosing the drive from the
    /// extension. A drive that already holds an image is left alone, so a
    /// stray drop cannot pull media from a running installation.
    fn handle_dropped_files(&mut self, ctx: &egui::Context, status: &ViewerStatus) {
        let (hovering, dropped) = ctx.input(|input| {
            (
                !input.raw.hovered_files.is_empty(),
                input.raw.dropped_files.clone(),
            )
        });
        if hovering {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop-hint"),
            ));
            painter.rect_filled(screen, 0, Color32::from_black_alpha(180));
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "Drop an .iso to mount it as CD-ROM, or an .img as floppy/USB",
                egui::FontId::proportional(20.0),
                Color32::WHITE,
            );
        }
        for file in dropped {
            let path = file.path().to_path_buf();
            if path.as_os_str().is_empty() {
                continue;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Some(kind) = drive_for(&path) else {
                self.notify(format!(
                    "Cannot mount {name}: use an .iso (CD-ROM) or .img/.ima/.bin (floppy/USB) file"
                ));
                continue;
            };
            if let Some(current) = &status.media(kind).image {
                self.notify(format!(
                    "{current} is already mounted; unmount it before dropping {name}"
                ));
                continue;
            }
            self.notify(format!("Mounting {name}"));
            self.send(ViewerCommand::Mount {
                kind,
                path,
                writable: kind == MediaKind::Floppy && self.floppy_writable,
            });
        }
    }

    /// Leaves at once, or asks first when the host would lose mounted images.
    fn request_exit(&mut self, ctx: &egui::Context, status: &ViewerStatus, exit: Exit) {
        if mounted_images(status).is_empty() {
            self.exit(ctx, exit);
        } else {
            self.pending_exit = Some(exit);
            // Keys typed in the dialog must not reach the host.
            if let Some(id) = self.captured_by {
                ctx.memory_mut(|memory| memory.surrender_focus(id));
            }
        }
    }

    fn exit(&mut self, ctx: &egui::Context, exit: Exit) {
        match exit {
            Exit::Disconnect => self.disconnect_requested = true,
            Exit::Quit => {
                self.quit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn confirm_exit_dialog(&mut self, ctx: &egui::Context, status: &ViewerStatus) {
        let Some(exit) = self.pending_exit else {
            return;
        };
        let verb = match exit {
            Exit::Disconnect => "Disconnect",
            Exit::Quit => "Quit",
        };
        let mut confirmed = false;
        let mut cancelled = false;
        let modal = egui::Modal::new(egui::Id::new("confirm-exit")).show(ctx, |ui| {
            ui.set_max_width(360.0);
            ui.heading(format!("{verb} while media is mounted?"));
            ui.add_space(6.0);
            for image in mounted_images(status) {
                ui.label(format!("• {image}"));
            }
            ui.label("The host loses these drives, which can break an installation in progress.");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                confirmed = ui.button(verb).clicked();
                cancelled = ui.button("Cancel").clicked();
            });
        });
        if confirmed {
            self.pending_exit = None;
            self.exit(ctx, exit);
        } else if cancelled || modal.should_close() {
            self.pending_exit = None;
        }
    }

    /// `(credentials, message)` once the session failed for good.
    fn rejection(&self) -> Option<(bool, String)> {
        let status = self.handle.shared.status.lock().ok()?;
        match status.state {
            ConnectionState::Rejected { credentials } => {
                Some((credentials, status.message.clone()))
            }
            _ => None,
        }
    }

    /// Reads the real window state, which the window manager can change too.
    fn toggle_fullscreen(&self, ctx: &egui::Context) {
        let fullscreen = ctx.input(|input| input.viewport().fullscreen.unwrap_or(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
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

    /// Forwards key events to the host while `focused`. The host key
    /// (Right Ctrl by default) is never forwarded: tapped alone it toggles capture, and
    /// held with another key it runs a client shortcut.
    fn handle_keyboard(&mut self, ctx: &egui::Context, focused: bool) -> Option<HostAction> {
        let events = ctx.input(|input| input.events.clone());
        let window_blurred = events
            .iter()
            .any(|event| matches!(event, Event::WindowFocused(false)));
        if window_blurred {
            self.host_key = None;
        }

        // Modifiers come from physical key events so that left and right
        // keys stay distinct: AltGr must reach the host as Right Alt, which
        // egui's merged `Modifiers` (and winit on Linux) cannot express.
        let mut changed = false;
        let mut action = None;
        for event in events {
            let Event::Key {
                key,
                physical_key,
                pressed,
                repeat,
                ..
            } = event
            else {
                continue;
            };
            // USB usages name physical positions; the host applies its
            // own layout, so prefer the physical key when available.
            let key = physical_key.unwrap_or(key);
            if Some(key) == self.prefs.host_key.key() {
                if pressed {
                    if !repeat {
                        self.host_key = Some(false);
                    }
                } else if self.host_key.take() == Some(false) {
                    action = Some(HostAction::ToggleCapture);
                }
                continue;
            }
            // Releases still go through, so keys held before the host key
            // do not stick on the host.
            if pressed && let Some(used) = self.host_key.as_mut() {
                if !repeat {
                    *used = true;
                    action = HostAction::for_key(key).or(action);
                }
                continue;
            }
            if !focused {
                continue;
            }
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
        if !focused || window_blurred {
            self.release_input();
        } else if changed {
            self.send_keyboard();
        }
        action
    }

    /// `image_rect` maps positions to host pixels; only `visible` (the part
    /// inside the viewport) takes input.
    fn handle_mouse(
        &mut self,
        ctx: &egui::Context,
        image_rect: Rect,
        visible: Rect,
        focused: bool,
        absolute: bool,
    ) {
        if !absolute {
            self.handle_relative_mouse(ctx, focused);
            return;
        }
        let Some(frame) = self.displayed_frame.clone() else {
            return;
        };
        let events = ctx.input(|input| input.events.clone());
        // Clicks on the fullscreen toolbar are not for the host.
        let over_toolbar = |pos: Pos2| self.toolbar_overlay.is_some_and(|rect| rect.contains(pos));
        for event in events {
            match event {
                Event::PointerMoved(pos) if over_toolbar(pos) => {}
                Event::PointerButton { pos, .. }
                    if over_toolbar(pos) && self.mouse_buttons == 0 => {}
                Event::PointerMoved(pos)
                    if focused && (visible.contains(pos) || self.mouse_buttons != 0) =>
                {
                    self.send_mouse(pos, image_rect, &frame);
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } if visible.contains(pos) || self.mouse_buttons != 0 => {
                    let bit = button_bit(button);
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

    /// Relative mode: the local cursor is locked while captured, and raw
    /// motion deltas go to the host.
    fn handle_relative_mouse(&mut self, ctx: &egui::Context, focused: bool) {
        if !focused {
            self.relative_motion = Vec2::ZERO;
            return;
        }
        let events = ctx.input(|input| input.events.clone());
        for event in events {
            match event {
                Event::MouseMoved(delta) => self.relative_motion += delta,
                Event::PointerButton {
                    button, pressed, ..
                } => {
                    let bit = button_bit(button);
                    if pressed {
                        self.mouse_buttons |= bit;
                    } else {
                        self.mouse_buttons &= !bit;
                    }
                    self.send(ViewerCommand::MouseRelative {
                        buttons: self.mouse_buttons,
                        dx: 0,
                        dy: 0,
                    });
                }
                _ => {}
            }
        }
        // Reports carry i8 deltas: split big moves, keep the fraction.
        let step = |value: f32| value.round().clamp(-127.0, 127.0);
        loop {
            let (dx, dy) = (step(self.relative_motion.x), step(self.relative_motion.y));
            if dx == 0.0 && dy == 0.0 {
                break;
            }
            self.relative_motion -= Vec2::new(dx, dy);
            self.send(ViewerCommand::MouseRelative {
                buttons: self.mouse_buttons,
                dx: dx as i8,
                dy: dy as i8,
            });
        }
    }

    /// Locks and hides the local cursor while a relative-mode host is captured.
    fn set_cursor_grab(&mut self, ctx: &egui::Context, grab: bool) {
        if self.cursor_grabbed == grab {
            return;
        }
        self.cursor_grabbed = grab;
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(if grab {
            egui::viewport::CursorGrab::Locked
        } else {
            egui::viewport::CursorGrab::None
        }));
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(!grab));
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
        let Some(clipboard) = self.clipboard.as_mut() else {
            return;
        };
        let text = match clipboard.text() {
            Ok(text) => text,
            Err(error) => {
                self.notify(format!("Clipboard unavailable: {error}"));
                return;
            }
        };
        const MAX_PASTE: usize = 10_000;
        if text.chars().count() > MAX_PASTE {
            self.notify(format!(
                "Clipboard text is longer than {MAX_PASTE} characters"
            ));
            return;
        }
        let (strokes, skipped) = keymap::text_to_strokes(self.prefs.layout, &text);
        let mut notice = format!(
            "Typing {} characters ({})",
            strokes.len(),
            self.prefs.layout.label()
        );
        if !skipped.is_empty() {
            let sample: String = skipped.iter().take(10).collect();
            notice.push_str(&format!(
                "; skipped {} unsupported: {sample:?}",
                skipped.len()
            ));
        }
        self.notify(notice);
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
                    MediaKind::Cdrom => dialog.add_filter("ISO image", CDROM_EXTENSIONS),
                    MediaKind::Floppy => dialog.add_filter("Disk image", FLOPPY_EXTENSIONS),
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

    /// Progress of a paste being typed, with a button to abort it.
    fn typing_progress(&self, ui: &mut egui::Ui) {
        use std::sync::atomic::Ordering;
        let shared = &self.handle.shared;
        let total = shared.typing_total.load(Ordering::SeqCst);
        // Single strokes come from the Send keys menu; no progress needed.
        if total <= 1 {
            return;
        }
        let done = shared.typing_done.load(Ordering::SeqCst);
        ui.add(
            egui::ProgressBar::new(done as f32 / total as f32)
                .desired_width(120.0)
                .text(format!("Typing {done}/{total}")),
        );
        if ui
            .small_button("Stop")
            .on_hover_text("Abort the paste")
            .clicked()
        {
            shared.cancel_typing.store(true, Ordering::SeqCst);
        }
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    /// Paste, host layout and lock toggles, kept out of the toolbar row.
    fn keyboard_menu(&mut self, ui: &mut egui::Ui, leds: Option<u8>) {
        ui.menu_button("Keyboard", |ui| {
            if ui
                .button("Paste text")
                .on_hover_text("Type the clipboard text on the host")
                .clicked()
            {
                self.paste_clipboard();
            }
            ui.menu_button(format!("Host layout: {}", self.prefs.layout.label()), |ui| {
                for layout in Layout::ALL {
                    ui.selectable_value(&mut self.prefs.layout, layout, layout.label());
                }
            })
            .response
            .on_hover_text("Keyboard layout configured on the host (used for pasting)");
            ui.menu_button(format!("Host key: {}", self.prefs.host_key.label()), |ui| {
                for choice in HostKey::ALL {
                    ui.selectable_value(&mut self.prefs.host_key, choice, choice.label());
                }
            })
            .response
            .on_hover_text(
                "Client key that is never sent to the host (default with --host-key or ILOM_HOST_KEY)",
            );
            if let Some(leds) = leds {
                ui.separator();
                for (label, bit, usage) in LOCK_KEYS {
                    let on = leds & bit != 0;
                    let text = format!("{label} lock ({})", if on { "on" } else { "off" });
                    if ui.button(text).on_hover_text("Click to toggle").clicked() {
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
            }
        });
    }

    /// Key combinations the local system would otherwise capture.
    fn send_keys_menu(&mut self, ui: &mut egui::Ui) {
        let combo = |modifiers, usage| ViewerCommand::TypeStrokes(vec![(modifiers, usage)]);
        let ctrl_alt = USB_LEFT_CTRL | USB_LEFT_ALT;
        let mut command = None;
        ui.menu_button("Send keys", |ui| {
            if ui.button("Ctrl+Alt+Del").clicked() {
                command = Some(ctrl_alt_del());
            }
            if ui.button("Ctrl+Alt+Backspace").clicked() {
                command = Some(combo(ctrl_alt, USAGE_BACKSPACE));
            }
            ui.menu_button("Ctrl+Alt+F1…F12", |ui| {
                for n in 0..12 {
                    if ui.button(format!("Ctrl+Alt+F{}", n + 1)).clicked() {
                        command = Some(combo(ctrl_alt, USAGE_F1 + n));
                    }
                }
            })
            .response
            .on_hover_text("Switch the host's virtual terminal");
            ui.separator();
            if ui.button("Alt+Tab").clicked() {
                command = Some(combo(USB_LEFT_ALT, USAGE_TAB));
            }
            if ui.button("Alt+F4").clicked() {
                command = Some(combo(USB_LEFT_ALT, USAGE_F4));
            }
            if ui.button("Super (Windows key)").clicked() {
                command = Some(combo(USB_LEFT_GUI, 0));
            }
            if ui.button("Print Screen").clicked() {
                command = Some(combo(0, USAGE_PRINT_SCREEN));
            }
            ui.separator();
            ui.menu_button("Magic SysRq", |ui| {
                for (key, label) in SYSRQ_KEYS {
                    if ui
                        .button(format!("Alt+SysRq+{} — {label}", key.to_ascii_uppercase()))
                        .clicked()
                    {
                        self.send_sysrq(key);
                    }
                }
            })
            .response
            .on_hover_text("Linux kernel emergency keys");
        })
        .response
        .on_hover_text("Send key combinations that the local system would capture");
        if let Some(command) = command {
            self.send(command);
        }
    }

    /// Holds Alt+SysRq (Print Screen) while tapping `key`.
    fn send_sysrq(&self, key: char) {
        let usage = USAGE_A + (key as u8 - b'a');
        for usages in [
            vec![USAGE_PRINT_SCREEN],
            vec![USAGE_PRINT_SCREEN, usage],
            vec![USAGE_PRINT_SCREEN],
        ] {
            self.send(ViewerCommand::Keyboard {
                modifiers: USB_LEFT_ALT,
                usages,
            });
        }
        self.send(ViewerCommand::Keyboard {
            modifiers: 0,
            usages: Vec::new(),
        });
    }

    fn screenshot_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Screenshot", |ui| {
            if ui.button("Save as PNG").clicked() {
                self.save_screenshot();
            }
            if ui.button("Copy to clipboard").clicked() {
                self.copy_screenshot();
            }
            if ui
                .button("Open folder")
                .on_hover_text(self.capture_dir.display().to_string())
                .clicked()
            {
                self.open_capture_dir();
            }
        });
    }

    fn copy_screenshot(&mut self) {
        let Some(frame) = self.displayed_frame.clone() else {
            self.notify("No frame is available yet");
            return;
        };
        let Some(clipboard) = self.clipboard.as_mut() else {
            return;
        };
        let result = clipboard.set_image(frame.width as usize, frame.height as usize, &frame.rgba);
        self.notify(match result {
            Ok(()) => format!("Screenshot copied ({}×{})", frame.width, frame.height),
            Err(error) => format!("Could not copy the screenshot: {error}"),
        });
    }

    fn open_capture_dir(&mut self) {
        let dir = self.capture_dir.clone();
        let result = std::fs::create_dir_all(&dir).and_then(|()| {
            let opener = if cfg!(target_os = "windows") {
                "explorer"
            } else if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            std::process::Command::new(opener)
                .arg(&dir)
                .spawn()
                .map(drop)
        });
        if let Err(error) = result {
            self.notify(format!("Could not open {}: {error}", dir.display()));
        }
    }

    fn save_screenshot(&mut self) {
        let Some(frame) = self.displayed_frame.as_ref() else {
            self.notify("No frame is available yet");
            return;
        };
        let result = (|| -> anyhow::Result<PathBuf> {
            std::fs::create_dir_all(&self.capture_dir)?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            // Milliseconds keep two shots in the same second apart.
            let path = self.capture_dir.join(format!(
                "ilom-{}-{:03}.png",
                stamp.as_secs(),
                stamp.subsec_millis()
            ));
            image::save_buffer(
                &path,
                &frame.rgba,
                frame.width,
                frame.height,
                image::ColorType::Rgba8,
            )?;
            Ok(path)
        })();
        self.notify(match result {
            Ok(path) => format!("Screenshot saved: {}", path.display()),
            Err(error) => format!("Screenshot failed: {error:#}"),
        });
    }

    fn toolbar(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        status: &ViewerStatus,
        fullscreen: bool,
    ) {
        // Wrap whole widgets onto extra rows instead of truncating them
        // when the window is too narrow.
        ui.horizontal_wrapped(|ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            let color = match status.state {
                ConnectionState::Connected => Color32::from_rgb(80, 200, 120),
                ConnectionState::Error | ConnectionState::Rejected { .. } => {
                    Color32::from_rgb(235, 90, 90)
                }
                _ => Color32::from_rgb(235, 185, 70),
            };
            ui.colored_label(color, format!("● {}", status.message));
            if status.state == ConnectionState::Reconnecting
                && ui
                    .small_button("Reconnect now")
                    .on_hover_text("Skip the wait before the next attempt")
                    .clicked()
            {
                self.handle
                    .shared
                    .reconnect_now
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if let Some(frame) = &self.displayed_frame {
                ui.separator();
                ui.label(format!("{}×{}", frame.width, frame.height));
            }
            ui.separator();
            if !status.keyboard {
                ui.weak("Keyboard/mouse off");
            } else if let Some(id) = self.captured_by {
                let host_key = self.prefs.host_key;
                let text = if self.cursor_grabbed {
                    // The locked cursor cannot reach the Release button.
                    format!(
                        "⌨ Keyboard and mouse captured ({} releases)",
                        host_key.label()
                    )
                } else {
                    "⌨ Keyboard captured".into()
                };
                ui.colored_label(Color32::from_rgb(90, 160, 255), text)
                    .on_hover_text(host_key.help());
                if ui
                    .small_button("Release")
                    .on_hover_text("Stop sending keys to the host")
                    .clicked()
                {
                    ui.memory_mut(|memory| memory.surrender_focus(id));
                }
            } else {
                let host_key = self.prefs.host_key;
                let text = match host_key {
                    HostKey::None => "Click the screen to capture the keyboard".into(),
                    _ => format!(
                        "Click the screen or press {} to capture the keyboard",
                        host_key.label()
                    ),
                };
                ui.colored_label(Color32::from_rgb(235, 185, 70), text)
                    .on_hover_text(host_key.help());
            }
            ui.separator();
            ui.add_enabled_ui(status.keyboard, |ui| self.send_keys_menu(ui));
            ui.add_enabled_ui(status.keyboard, |ui| self.keyboard_menu(ui, status.leds));
            self.typing_progress(ui);
            if let Some(leds) = status.leds {
                for (label, bit) in LOCK_KEYS.map(|(label, bit, _)| (label, bit)) {
                    let on = leds & bit != 0;
                    ui.label(egui::RichText::new(label).small().monospace().color(if on {
                        Color32::from_rgb(80, 200, 120)
                    } else {
                        Color32::from_gray(90)
                    }))
                    .on_hover_text(format!(
                        "Host {label} lock is {}",
                        if on { "on" } else { "off" }
                    ));
                }
            }
            ui.separator();
            for kind in [MediaKind::Cdrom, MediaKind::Floppy] {
                self.media_menu(ui, kind, status.media(kind));
            }
            ui.separator();
            ui.toggle_value(&mut self.prefs.actual_size, "100%")
                .on_hover_text(
                    "Show host pixels 1:1 and scroll when larger than the window; \
                 off fits the screen to the window",
                );
            self.screenshot_menu(ui);
            if ui
                .button(if fullscreen {
                    "Exit fullscreen"
                } else {
                    "Fullscreen"
                })
                .clicked()
            {
                self.toggle_fullscreen(ctx);
            }
            if ui.button("Disconnect").clicked() {
                self.request_exit(ctx, status, Exit::Disconnect);
            }
        });
        if let Some((notice, shown)) = &self.notice {
            match NOTICE_TIMEOUT.checked_sub(shown.elapsed()) {
                Some(remaining) => {
                    ui.small(notice);
                    ctx.request_repaint_after(remaining);
                }
                None => self.notice = None,
            }
        }
    }

    /// Draws the framebuffer in `rect` and routes keyboard and mouse input.
    fn show_framebuffer(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        status: &ViewerStatus,
        texture_id: egui::TextureId,
        rect: Rect,
    ) {
        self.update_texture_filter(ctx, rect.width());
        // Only the part inside the viewport takes pointer input.
        let visible = rect.intersect(ui.clip_rect());
        let response = ui.put(
            rect,
            egui::Image::new((texture_id, rect.size())).sense(Sense::click_and_drag()),
        );
        let host_cursor = self.draw_host_cursor(ui, ctx, rect, visible);
        if status.state != ConnectionState::Connected {
            // Dim the last frame so a stale screen is not mistaken for a live one.
            ui.painter()
                .rect_filled(rect, 0, Color32::from_black_alpha(170));
            let galley = ui.painter().layout(
                status.message.clone(),
                egui::FontId::proportional(18.0),
                Color32::WHITE,
                (rect.width() - 32.0).max(80.0),
            );
            let origin = rect.center() - galley.size() / 2.0;
            ui.painter().galley(origin, galley, Color32::WHITE);
        }
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = response.has_focus();
        self.captured_by = focused.then_some(response.id);
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
        match self.handle_keyboard(ctx, focused) {
            Some(HostAction::ToggleCapture) if focused => {
                ui.memory_mut(|memory| memory.surrender_focus(response.id));
            }
            Some(HostAction::ToggleCapture) if self.pending_exit.is_none() => {
                response.request_focus();
            }
            Some(HostAction::Fullscreen) => self.toggle_fullscreen(ctx),
            Some(HostAction::Paste) => self.paste_clipboard(),
            Some(HostAction::CtrlAltDel) => self.send(ctrl_alt_del()),
            Some(HostAction::ToggleCapture) | None => {}
        }
        let absolute = status.absolute_mouse;
        // Without a host key a locked cursor could never be released.
        let grab = focused && status.keyboard && !absolute && self.prefs.host_key.key().is_some();
        self.set_cursor_grab(ctx, grab);
        if focused
            && absolute
            && ctx
                .pointer_hover_pos()
                .is_some_and(|pos| visible.contains(pos))
        {
            // The host cursor is in the picture; a second local arrow lagging
            // next to it only confuses.
            ctx.set_cursor_icon(if host_cursor {
                egui::CursorIcon::None
            } else {
                egui::CursorIcon::Crosshair
            });
        }
        self.handle_mouse(ctx, rect, visible, focused, absolute);
    }

    /// Paints the host hardware cursor over the framebuffer drawn in `rect`.
    /// Returns whether one is shown.
    fn draw_host_cursor(
        &mut self,
        ui: &egui::Ui,
        ctx: &egui::Context,
        rect: Rect,
        visible: Rect,
    ) -> bool {
        let cursor = self
            .handle
            .shared
            .cursor
            .lock()
            .ok()
            .and_then(|cursor| cursor.clone());
        let (Some(cursor), Some(frame)) = (cursor, self.displayed_frame.clone()) else {
            self.cursor_texture = None;
            return false;
        };
        let key = (cursor.serial, frame.sequence, self.texture_options);
        if self
            .cursor_texture
            .as_ref()
            .is_none_or(|(cached, ..)| *cached != key)
        {
            self.cursor_texture = render_cursor(&cursor, &frame).map(|(image, host_rect)| {
                let texture = ctx.load_texture("host cursor", image, self.texture_options);
                (key, texture, host_rect)
            });
        }
        let Some((_, texture, host_rect)) = &self.cursor_texture else {
            return false;
        };
        let scale = rect.width() / frame.width as f32;
        let screen = Rect::from_min_size(
            rect.min + host_rect.min.to_vec2() * scale,
            host_rect.size() * scale,
        );
        ui.painter().with_clip_rect(visible).image(
            texture.id(),
            screen,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        true
    }

    /// Sharp pixels at whole-number scales, smooth filtering otherwise.
    fn update_texture_filter(&mut self, ctx: &egui::Context, width_points: f32) {
        let Some(frame) = self.displayed_frame.clone() else {
            return;
        };
        let scale = width_points * ctx.pixels_per_point() / frame.width as f32;
        let options = if scale >= 0.99 && (scale - scale.round()).abs() < 0.01 {
            TextureOptions::NEAREST
        } else {
            TextureOptions::LINEAR
        };
        if options != self.texture_options {
            self.texture_options = options;
            self.upload_texture(ctx, &frame);
        }
    }

    /// In fullscreen the toolbar hides until the pointer touches the top edge,
    /// and stays while the pointer or one of its menus is on it.
    fn reveal_toolbar(&self, ctx: &egui::Context) -> bool {
        let pointer = ctx.input(|input| input.pointer.hover_pos());
        let near_top = pointer.is_some_and(|pos| pos.y <= ctx.content_rect().top() + 2.0);
        let on_toolbar = pointer
            .zip(self.toolbar_overlay)
            .is_some_and(|(pos, rect)| rect.expand(24.0).contains(pos));
        near_top || on_toolbar || egui::Popup::is_any_open(ctx)
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.clipboard.is_none() {
            use raw_window_handle::HasDisplayHandle;
            let display = frame.display_handle().ok().map(|handle| handle.as_raw());
            self.clipboard = Some(Clipboard::new(display));
        }
        self.refresh_texture(&ctx);
        let status = self
            .handle
            .shared
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default();
        if ctx.input(|input| input.viewport().close_requested()) && !self.quit_confirmed {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.request_exit(&ctx, &status, Exit::Quit);
        }
        self.confirm_exit_dialog(&ctx, &status);
        self.handle_dropped_files(&ctx, &status);

        let fullscreen = ctx.input(|input| input.viewport().fullscreen.unwrap_or(false));
        if !fullscreen {
            self.toolbar_overlay = None;
            egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui, &ctx, &status, fullscreen));
        } else if self.reveal_toolbar(&ctx) {
            // Float over the video so it does not rescale when revealed.
            let area = egui::Area::new(egui::Id::new("fullscreen-toolbar"))
                .fixed_pos(ctx.content_rect().left_top())
                .show(&ctx, |ui| {
                    egui::Frame::side_top_panel(ui.style()).show(ui, |ui| {
                        ui.set_width(ctx.content_rect().width());
                        self.toolbar(ui, &ctx, &status, fullscreen);
                    });
                });
            self.toolbar_overlay = Some(area.response.rect);
        } else {
            self.toolbar_overlay = None;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::from_rgb(18, 18, 20)))
            .show(ui, |ui| {
                let available = ui.available_rect_before_wrap();
                let (Some(texture), Some(frame)) = (&self.texture, &self.displayed_frame) else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                        ui.label(if status.state == ConnectionState::Connected {
                            "Waiting for the first frame…"
                        } else {
                            status.message.as_str()
                        });
                    });
                    return;
                };
                let texture_id = texture.id();
                let frame_size = Vec2::new(frame.width as f32, frame.height as f32);
                if self.prefs.actual_size {
                    // One host pixel per screen pixel; scroll when it does not fit.
                    let size = frame_size / ctx.pixels_per_point();
                    egui::ScrollArea::both()
                        // Dragging belongs to the host.
                        .scroll_source(egui::scroll_area::ScrollSource {
                            mouse_wheel: true,
                            ..egui::scroll_area::ScrollSource::SCROLL_BAR
                        })
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            let available = ui.available_rect_before_wrap();
                            let min =
                                available.min + ((available.size() - size) / 2.0).max(Vec2::ZERO);
                            let rect = Rect::from_min_size(min, size);
                            self.show_framebuffer(ui, &ctx, &status, texture_id, rect);
                        });
                } else {
                    let scale = (available.size() / frame_size).min_elem().max(0.01);
                    let rect = Rect::from_center_size(available.center(), frame_size * scale);
                    self.show_framebuffer(ui, &ctx, &status, texture_id, rect);
                }
            });
    }
}

impl Drop for ViewerApp {
    fn drop(&mut self) {
        self.release_input();
        self.handle.stop_and_wait();
    }
}

fn button_bit(button: PointerButton) -> u8 {
    match button {
        PointerButton::Primary => hid::BUTTON_LEFT,
        PointerButton::Secondary => hid::BUTTON_RIGHT,
        PointerButton::Middle => hid::BUTTON_MIDDLE,
        _ => 0,
    }
}

fn ctrl_alt_del() -> ViewerCommand {
    ViewerCommand::Keystroke {
        modifiers: USB_LEFT_CTRL | USB_LEFT_ALT,
        usage: USAGE_DELETE,
    }
}

/// Clips the 64×64 hardware cursor pattern to the framebuffer and turns it
/// into RGBA. XOR pixels invert the framebuffer below, as on the host.
fn render_cursor(cursor: &HostCursor, frame: &DecodedFrame) -> Option<(ColorImage, Rect)> {
    const SIZE: usize = video::CURSOR_SIZE;
    let x_offset = cursor.x_offset.clamp(0, SIZE as i16 - 1) as usize;
    let y_offset = cursor.y_offset.clamp(0, SIZE as i16 - 1) as usize;
    let x = cursor.x.max(0) as usize;
    // The vendor client treats rows past 1200 as a wrapped negative value.
    let y = if cursor.y > 1200 {
        0
    } else {
        cursor.y.max(0) as usize
    };
    let (frame_width, frame_height) = (frame.width as usize, frame.height as usize);
    let width = (SIZE - x_offset).min(frame_width.saturating_sub(x));
    let height = (SIZE - y_offset).min(frame_height.saturating_sub(y));
    if width == 0 || height == 0 || cursor.pattern.len() < SIZE * SIZE {
        return None;
    }
    let nibble = |value: u16, shift: u32| ((value >> shift) & 0xf) as u8 * 17;
    let mut rgba = vec![0_u8; width * height * 4];
    for row in 0..height {
        for column in 0..width {
            let pixel = cursor.pattern[(row + y_offset) * SIZE + column + x_offset];
            let color = [nibble(pixel, 8), nibble(pixel, 4), nibble(pixel, 0)];
            let out = &mut rgba[(row * width + column) * 4..][..4];
            if cursor.alpha {
                out[..3].copy_from_slice(&color);
                out[3] = nibble(pixel, 12);
            } else if pixel & 0x8000 == 0 {
                out[..3].copy_from_slice(&color);
                out[3] = 255;
            } else if pixel & 0x4000 != 0 {
                let below = &frame.rgba[((y + row) * frame_width + x + column) * 4..][..3];
                for (channel, value) in out.iter_mut().zip(below) {
                    *channel = 255 - value;
                }
                out[3] = 255;
            }
        }
    }
    let image = ColorImage::from_rgba_unmultiplied([width, height], &rgba);
    let host_rect = Rect::from_min_size(
        Pos2::new(x as f32, y as f32),
        Vec2::new(width as f32, height as f32),
    );
    Some((image, host_rect))
}

/// Virtual drive for an image file, from its extension.
fn drive_for(path: &std::path::Path) -> Option<MediaKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if CDROM_EXTENSIONS.contains(&extension.as_str()) {
        Some(MediaKind::Cdrom)
    } else if FLOPPY_EXTENSIONS.contains(&extension.as_str()) {
        Some(MediaKind::Floppy)
    } else {
        None
    }
}

/// "CD-ROM: name.iso" for each drive with an image.
fn mounted_images(status: &ViewerStatus) -> Vec<String> {
    [
        (MediaKind::Cdrom, "CD-ROM"),
        (MediaKind::Floppy, "Floppy/USB"),
    ]
    .into_iter()
    .filter_map(|(kind, title)| {
        let image = status.media(kind).image.as_ref()?;
        Some(format!("{title}: {image}"))
    })
    .collect()
}

/// Host shown in the window title, so several open consoles stay apart.
fn source_host(source: &Source) -> String {
    match source {
        Source::Web { host, .. } => host.clone(),
        Source::Jnlp(path) => source
            .console_args()
            .map(|args| args.host)
            .unwrap_or_else(|_| path.display().to_string()),
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
    fn cursor_pixels_follow_and_xor_rules() {
        let mut pattern = vec![0x8000_u16; 64 * 64]; // AND 1, XOR 0: transparent
        pattern[0] = 0x0f00; // AND 0: opaque red
        pattern[1] = 0xc000; // AND 1, XOR 1: invert
        let cursor = HostCursor {
            serial: 1,
            alpha: false,
            x: 98,
            y: 0,
            x_offset: 0,
            y_offset: 0,
            pattern: Arc::new(pattern),
        };
        let frame = DecodedFrame {
            width: 100,
            height: 80,
            sequence: 1,
            rgba: vec![10; 100 * 80 * 4],
        };
        let (image, host_rect) = render_cursor(&cursor, &frame).unwrap();
        // Clipped at the right edge: only two columns fit.
        assert_eq!(image.size, [2, 64]);
        assert_eq!(host_rect.min, Pos2::new(98.0, 0.0));
        assert_eq!(image.pixels[0], Color32::from_rgb(255, 0, 0));
        assert_eq!(image.pixels[1], Color32::from_rgb(245, 245, 245));
        assert_eq!(image.pixels[2], Color32::TRANSPARENT);
    }

    #[test]
    fn dropped_files_pick_a_drive() {
        use std::path::Path;
        assert_eq!(
            drive_for(Path::new("/tmp/debian.ISO")),
            Some(MediaKind::Cdrom)
        );
        assert_eq!(drive_for(Path::new("stick.img")), Some(MediaKind::Floppy));
        assert_eq!(drive_for(Path::new("notes.txt")), None);
        assert_eq!(drive_for(Path::new("README")), None);
    }

    #[test]
    fn host_key_shortcuts() {
        assert_eq!(HostAction::for_key(Key::F), Some(HostAction::Fullscreen));
        assert_eq!(
            HostAction::for_key(Key::Delete),
            Some(HostAction::CtrlAltDel)
        );
        assert_eq!(HostAction::for_key(Key::A), None);
        let default = if cfg!(target_os = "macos") {
            Key::SuperRight
        } else {
            Key::ControlRight
        };
        assert_eq!(HostKey::default().key(), Some(default));
        assert_eq!(HostKey::None.key(), None);
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
