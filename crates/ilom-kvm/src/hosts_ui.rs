//! Named hosts in the login screen: a picker and a dialog to add, edit and
//! remove entries of `hosts.toml`. The state and the decisions are plain
//! functions (tested below); only `HostsUi::show_*` draws.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use eframe::egui::{self, Color32};
use ilom_kvm_core::{
    hosts::{Env, Host, HostsFile, MissingPassword, PasswordOrigin},
    keymap::Layout,
    keyring::{SecretStore, move_entry},
    session::Source,
};

use crate::host_cache::HostCache;

/// What a host entry connects to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TargetKind {
    #[default]
    Address,
    Jnlp,
}

/// The edit dialog's buffer: text as typed, checked on save.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostForm {
    /// Name the entry had when the dialog opened; `None` for a new one.
    pub original: Option<String>,
    pub name: String,
    pub kind: TargetKind,
    pub host: String,
    pub jnlp: String,
    pub user: String,
    pub layout: Option<Layout>,
    pub password_env: String,
    /// Typed password. Never written to the hosts file: it goes to the OS
    /// keyring on save, or nowhere.
    pub password: String,
    /// "Save to the OS keyring" unticked (the box is on by default).
    pub skip_keyring: bool,
}

impl HostForm {
    pub fn from_host(name: &str, host: &Host) -> Self {
        Self {
            original: Some(name.to_owned()),
            name: name.to_owned(),
            kind: if host.jnlp.is_some() {
                TargetKind::Jnlp
            } else {
                TargetKind::Address
            },
            host: host.host.clone().unwrap_or_default(),
            jnlp: host
                .jnlp
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            user: host.user.clone().unwrap_or_default(),
            layout: host.layout(),
            password_env: host.password_env.clone().unwrap_or_default(),
            ..Self::default()
        }
    }

    /// The entry the buffer describes, or what to fix. Only the keys the
    /// dialog owns are filled; the file keeps the others.
    pub fn to_host(&self) -> Result<(String, Host), String> {
        let name = self.name.trim().to_owned();
        let text = |value: &str| Some(value.trim().to_owned()).filter(|value| !value.is_empty());
        let host = Host {
            host: match self.kind {
                TargetKind::Address => text(&self.host).map(|host| {
                    host.trim_start_matches("https://")
                        .trim_end_matches('/')
                        .to_owned()
                }),
                TargetKind::Jnlp => None,
            },
            jnlp: match self.kind {
                TargetKind::Jnlp => text(&self.jnlp).map(PathBuf::from),
                TargetKind::Address => None,
            },
            user: text(&self.user),
            layout: self.layout.map(|layout| layout.id().to_owned()),
            password_env: text(&self.password_env),
            ..Host::default()
        };
        if name.is_empty() {
            return Err("Enter a name".into());
        }
        if host.host.is_none() && host.jnlp.is_none() {
            return Err(match self.kind {
                TargetKind::Address => "Enter the ILOM address".into(),
                TargetKind::Jnlp => "Choose a JNLP file".into(),
            });
        }
        host.validate(&name).map_err(|error| format!("{error:#}"))?;
        Ok((name, host))
    }
}

/// A name must be new, unless it is the entry's own.
pub fn check_name(file: &HostsFile, original: Option<&str>, name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("Enter a name".into());
    }
    if original != Some(name) && file.hosts.contains_key(name) {
        return Err(format!("A host called {name:?} already exists"));
    }
    Ok(())
}

/// A saved entry: its name, and a problem with the keyring that did not
/// stop the save.
#[derive(Debug, PartialEq, Eq)]
pub struct Saved {
    pub name: String,
    pub warning: Option<String>,
}

/// Saves the dialog's entry to `path` (renaming first when the name
/// changed, which also moves the keyring password) and returns its name.
pub fn apply_save(
    path: &std::path::Path,
    file: &HostsFile,
    form: &HostForm,
    store: &dyn SecretStore,
) -> Result<Saved, String> {
    let (name, host) = form.to_host()?;
    check_name(file, form.original.as_deref(), &name)?;
    let render = |error: anyhow::Error| format!("{error:#}");
    let mut warning = None;
    if let Some(original) = form
        .original
        .as_deref()
        .filter(|original| *original != name)
    {
        HostsFile::rename_host(path, original, &name).map_err(render)?;
        if let Err(error) = move_entry(store, original, &name) {
            warning = Some(format!(
                "Renamed, but the saved password could not be moved: {error:#}"
            ));
        }
    }
    HostsFile::save_host(path, &name, &host).map_err(render)?;
    // The host is saved whatever happens to the password. An empty field
    // leaves any existing entry alone; a JNLP host has no password.
    if !form.password.is_empty()
        && !form.skip_keyring
        && host.jnlp.is_none()
        && let Err(error) = store.set(&name, &form.password)
    {
        let message = format!("Host saved, but the password could not be saved: {error:#}");
        warning = Some(match warning {
            Some(earlier) => format!("{earlier}; {message}"),
            None => message,
        });
    }
    Ok(Saved { name, warning })
}

/// What picking a host puts in the login form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    pub address: String,
    pub username: String,
    pub layout: Option<Layout>,
}

pub fn fill(host: &Host) -> Fill {
    Fill {
        address: host.host.clone().unwrap_or_default(),
        username: host.user.clone().unwrap_or_else(|| "root".into()),
        layout: host.layout(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    Ready(Source),
    /// No password in the environment: ask for one. Names the variable.
    NeedPassword(String),
    Invalid(String),
}

/// Turns a picked host into a session source. The environment wins, then
/// the keyring; a typed password is only used when both have nothing.
pub fn resolve(
    name: &str,
    host: &Host,
    typed_password: &str,
    env: Env,
    store: &dyn SecretStore,
) -> Resolved {
    match host.source(name, env, store) {
        Ok(source) => Resolved::Ready(source),
        Err(error) => match error.downcast_ref::<MissingPassword>() {
            Some(missing) if typed_password.is_empty() => {
                Resolved::NeedPassword(missing.variable.clone())
            }
            Some(_) => Resolved::Ready(Source::Web {
                host: host.host.clone().unwrap_or_default(),
                username: host.user.clone().unwrap_or_else(|| "root".into()),
                password: typed_password.to_owned(),
            }),
            None => Resolved::Invalid(format!("{error:#}")),
        },
    }
}

/// What the login form must do about the password of a picked host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordLookup {
    /// A JNLP file: no password.
    NotNeeded,
    Found(PasswordOrigin),
    /// Nowhere to be found: ask for it. Names the variable.
    Prompt(String),
}

pub fn lookup(name: &str, host: &Host, env: Env, store: &dyn SecretStore) -> PasswordLookup {
    if host.jnlp.is_some() {
        return PasswordLookup::NotNeeded;
    }
    match host.source(name, env, store) {
        Ok(_) => host
            .password_origin(name, env, store)
            .map_or(PasswordLookup::NotNeeded, PasswordLookup::Found),
        Err(error) => match error.downcast_ref::<MissingPassword>() {
            Some(missing) => PasswordLookup::Prompt(missing.variable.clone()),
            None => PasswordLookup::NotNeeded,
        },
    }
}

/// What a switch request does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchPlan {
    /// Already on that host.
    Noop,
    Go,
    /// Mounted images would be lost: ask first.
    Confirm,
}

pub fn plan_switch(current: Option<&str>, target: &str, mounted: bool) -> SwitchPlan {
    if current == Some(target) {
        SwitchPlan::Noop
    } else if mounted {
        SwitchPlan::Confirm
    } else {
        SwitchPlan::Go
    }
}

/// Resolves a switch to `name` without a typed password. Unknown and
/// JNLP-file hosts cannot be switched to (the file's secret works once).
pub fn switch_resolution(
    file: &HostsFile,
    name: &str,
    env: Env,
    store: &dyn SecretStore,
) -> Result<(Host, Resolved), String> {
    let host = file
        .hosts
        .get(name)
        .ok_or_else(|| format!("No host called {name:?}"))?;
    if host.jnlp.is_some() {
        return Err(format!(
            "{name} uses a JNLP file, which works once: connect from the login form"
        ));
    }
    Ok((host.clone(), resolve(name, host, "", env, store)))
}

/// One host in the toolbar switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitcherItem {
    pub name: String,
    /// False for JNLP-file hosts.
    pub switchable: bool,
    pub tooltip: String,
}

/// The toolbar's host combo; returns the host picked, if it is another one.
pub fn show_switcher(
    ui: &mut egui::Ui,
    items: &[SwitcherItem],
    current: Option<&str>,
) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut picked = None;
    egui::ComboBox::from_id_salt("toolbar-host-switcher")
        .selected_text(current.unwrap_or("Switch host"))
        .width(120.0)
        .show_ui(ui, |ui| {
            for item in items {
                let response = ui.add_enabled(
                    item.switchable,
                    egui::Button::selectable(current == Some(item.name.as_str()), &item.name),
                );
                let response = if item.tooltip.is_empty() {
                    response
                } else {
                    response
                        .on_disabled_hover_text(&item.tooltip)
                        .on_hover_text(&item.tooltip)
                };
                if response.clicked() {
                    picked = Some(item.name.clone());
                }
            }
        })
        .response
        .on_hover_text("Switch to another host");
    picked.filter(|name| current != Some(name.as_str()))
}

#[derive(Default)]
struct Manage {
    editing: Option<HostForm>,
    confirm_delete: Option<String>,
    error: Option<String>,
    /// Something to tell, not a failure of the action.
    notice: Option<String>,
    /// Whether the edited host has a keyring entry, asked once when the
    /// form opens (never per frame).
    saved_password: Option<bool>,
}

/// Hosts file, picker selection and dialog state of the login screen.
pub struct HostsUi {
    path: Option<PathBuf>,
    file: HostsFile,
    pub load_error: Option<String>,
    /// Name picked in the combo; `None` is manual entry.
    pub selected: Option<String>,
    manage: Option<Manage>,
    store: Arc<dyn SecretStore>,
    /// The keyring is slow to ask, so each host's answer is kept until
    /// something may have changed it.
    lookups: HashMap<String, PasswordLookup>,
    /// Hosts edited, renamed or removed in the dialog since the last
    /// [`HostsUi::take_changed`], whose cached frames are stale.
    changed: Vec<String>,
}

const ERROR_COLOR: Color32 = Color32::from_rgb(235, 90, 90);
const NOTICE_COLOR: Color32 = Color32::from_rgb(225, 185, 80);

impl HostsUi {
    pub fn load(
        path: Option<PathBuf>,
        selected: Option<String>,
        store: Arc<dyn SecretStore>,
    ) -> Self {
        let mut ui = Self {
            path,
            file: HostsFile::default(),
            load_error: None,
            selected,
            manage: None,
            store,
            lookups: HashMap::new(),
            changed: Vec::new(),
        };
        ui.reload();
        ui
    }

    /// Rereads the file; a broken file shows its error and no hosts.
    pub fn reload(&mut self) {
        self.lookups.clear();
        self.load_error = None;
        self.file = match &self.path {
            Some(path) => HostsFile::load(path).unwrap_or_else(|error| {
                self.load_error = Some(format!("{error:#}"));
                HostsFile::default()
            }),
            None => HostsFile::default(),
        };
        if self
            .selected
            .as_ref()
            .is_some_and(|name| !self.file.hosts.contains_key(name))
        {
            self.selected = None;
        }
    }

    pub fn file(&self) -> &HostsFile {
        &self.file
    }

    pub fn selected_host(&self) -> Option<(&str, &Host)> {
        let name = self.selected.as_ref()?;
        self.file
            .hosts
            .get_key_value(name)
            .map(|(name, host)| (name.as_str(), host))
    }

    pub fn store(&self) -> &Arc<dyn SecretStore> {
        &self.store
    }

    /// Forgets the cached password lookup: the keyring may have changed.
    pub fn invalidate(&mut self) {
        self.lookups.clear();
    }

    /// Names changed in the dialog since the last call.
    pub fn take_changed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.changed)
    }

    /// Where the picked host's password comes from, asking the keyring once
    /// per host.
    pub fn password_lookup(&mut self, env: Env) -> Option<PasswordLookup> {
        let name = self.selected.clone()?;
        self.password_lookup_for(&name, env)
    }

    /// Like [`HostsUi::password_lookup`] for any host of the file.
    pub fn password_lookup_for(&mut self, name: &str, env: Env) -> Option<PasswordLookup> {
        if let Some(found) = self.lookups.get(name) {
            return Some(found.clone());
        }
        let host = self.file.hosts.get(name)?;
        let found = lookup(name, host, env, self.store.as_ref());
        self.lookups.insert(name.to_owned(), found.clone());
        Some(found)
    }

    /// The hosts the toolbar switcher offers. Uses only what is already
    /// cached, so it is safe to call every frame.
    pub fn switcher_items(&self, cache: &HostCache) -> Vec<SwitcherItem> {
        self.file
            .hosts
            .iter()
            .map(|(name, host)| {
                let switchable = host.jnlp.is_none();
                let mut tooltip = if switchable {
                    String::new()
                } else {
                    "A JNLP file works once: connect from the login form".to_owned()
                };
                if switchable {
                    if let Some(((width, height), age)) = cache.describe(name) {
                        tooltip = format!("Last seen {width}×{height}, {} s ago", age.as_secs());
                    }
                    let password = match self.lookups.get(name) {
                        Some(PasswordLookup::Found(PasswordOrigin::Env)) => {
                            Some("password from the environment")
                        }
                        Some(PasswordLookup::Found(PasswordOrigin::Keyring)) => {
                            Some("password from the keyring")
                        }
                        Some(PasswordLookup::Prompt(_)) => Some("asks for a password"),
                        _ => None,
                    };
                    if let Some(password) = password {
                        if !tooltip.is_empty() {
                            tooltip.push_str("; ");
                        }
                        tooltip.push_str(password);
                    }
                }
                SwitcherItem {
                    name: name.clone(),
                    switchable,
                    tooltip,
                }
            })
            .collect()
    }

    pub fn open_manage(&mut self) {
        self.manage = Some(Manage::default());
    }

    /// The host combo. True when the selection changed.
    pub fn show_picker(&mut self, ui: &mut egui::Ui) -> bool {
        if self.file.hosts.is_empty() {
            return false;
        }
        ui.label("Host");
        let before = self.selected.clone();
        egui::ComboBox::from_id_salt("host-picker")
            .width(ui.available_width())
            .selected_text(self.selected.as_deref().unwrap_or("Custom"))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.selected, None, "Custom");
                for name in self.file.hosts.keys() {
                    ui.selectable_value(&mut self.selected, Some(name.clone()), name);
                }
            });
        self.selected != before
    }

    /// The add/edit/remove dialog, while open.
    pub fn show_manage(&mut self, ctx: &egui::Context) {
        let Some(path) = self.path.clone() else {
            self.manage = None;
            return;
        };
        let Some(mut manage) = self.manage.take() else {
            return;
        };
        let mut action = None;
        let modal = egui::Modal::new(egui::Id::new("manage-hosts"))
            .backdrop_color(Color32::from_black_alpha(200))
            .show(ctx, |ui| {
                ui.set_width(440.0);
                if manage.editing.is_some() {
                    action = Self::show_form(ui, &mut manage);
                } else {
                    action = self.show_list(ui, &mut manage);
                }
                if let Some(notice) = &manage.notice {
                    ui.add_space(6.0);
                    ui.colored_label(NOTICE_COLOR, notice);
                }
                if let Some(error) = &manage.error {
                    ui.add_space(6.0);
                    ui.colored_label(ERROR_COLOR, error);
                }
            });
        let mut keep = !modal.should_close();
        match action {
            Some(Action::Close) => keep = false,
            Some(Action::Add) => {
                manage.error = None;
                manage.notice = None;
                manage.saved_password = None;
                manage.editing = Some(HostForm::default());
            }
            Some(Action::Edit(name)) => {
                manage.error = None;
                manage.notice = None;
                manage.saved_password = self.store.get(&name).ok().map(|saved| saved.is_some());
                manage.editing = self
                    .file
                    .hosts
                    .get(&name)
                    .map(|host| HostForm::from_host(&name, host));
            }
            Some(Action::Delete(name)) => manage.confirm_delete = Some(name),
            Some(Action::Forget(name)) => {
                manage.error = None;
                manage.notice = None;
                match self.store.delete(&name) {
                    Ok(()) => {
                        self.lookups.remove(&name);
                        manage.notice = Some(format!("Saved password of {name} forgotten"));
                        if manage
                            .editing
                            .as_ref()
                            .and_then(|form| form.original.as_deref())
                            == Some(name.as_str())
                        {
                            manage.saved_password = Some(false);
                        }
                    }
                    Err(error) => {
                        manage.error = Some(format!("Could not forget the password: {error:#}"));
                    }
                }
            }
            Some(Action::CancelDelete) => manage.confirm_delete = None,
            Some(Action::ConfirmDelete(name)) => {
                manage.confirm_delete = None;
                manage.notice = None;
                self.changed.push(name.clone());
                manage.error = HostsFile::remove_host(&path, &name)
                    .err()
                    .map(|error| format!("{error:#}"));
                if manage.error.is_none()
                    && let Err(error) = self.store.delete(&name)
                {
                    manage.error = Some(format!(
                        "Removed, but the saved password could not be deleted: {error:#}"
                    ));
                }
                self.reload();
            }
            Some(Action::CancelEdit) => {
                manage.editing = None;
                manage.error = None;
                manage.notice = None;
            }
            Some(Action::Save) => {
                if let Some(form) = manage.editing.clone() {
                    match apply_save(&path, &self.file, &form, self.store.as_ref()) {
                        Ok(saved) => {
                            self.changed.extend(form.original.clone());
                            self.changed.push(saved.name.clone());
                            manage.editing = None;
                            manage.error = None;
                            manage.notice = saved.warning;
                            self.selected = Some(saved.name);
                            self.reload();
                        }
                        Err(message) => manage.error = Some(message),
                    }
                }
            }
            None => {}
        }
        if keep {
            self.manage = Some(manage);
        }
    }

    fn show_list(&self, ui: &mut egui::Ui, manage: &mut Manage) -> Option<Action> {
        let mut action = None;
        ui.heading("Hosts");
        ui.small(format!(
            "Saved in {}. Passwords are not in this file: they come from the environment or the OS keyring.",
            self.path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        ));
        ui.add_space(8.0);
        if self.file.hosts.is_empty() {
            ui.label("No hosts yet.");
        }
        for (name, host) in &self.file.hosts {
            ui.horizontal(|ui| {
                let target = host
                    .host
                    .clone()
                    .or_else(|| host.jnlp.as_ref().map(|path| path.display().to_string()))
                    .unwrap_or_default();
                ui.label(format!("{name}  ({target})"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if manage.confirm_delete.as_deref() == Some(name.as_str()) {
                        if ui.button("Cancel").clicked() {
                            action = Some(Action::CancelDelete);
                        }
                        if ui.button(format!("Delete {name}")).clicked() {
                            action = Some(Action::ConfirmDelete(name.clone()));
                        }
                    } else {
                        if ui.button("Delete").clicked() {
                            action = Some(Action::Delete(name.clone()));
                        }
                        if host.jnlp.is_none() && ui.button("Forget password").clicked() {
                            action = Some(Action::Forget(name.clone()));
                        }
                        if ui.button("Edit").clicked() {
                            action = Some(Action::Edit(name.clone()));
                        }
                    }
                });
            });
        }
        if let Some(error) = &self.load_error {
            ui.colored_label(ERROR_COLOR, error);
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Add host").clicked() {
                action = Some(Action::Add);
            }
            if ui.button("Close").clicked() {
                action = Some(Action::Close);
            }
        });
        action
    }

    fn show_form(ui: &mut egui::Ui, manage: &mut Manage) -> Option<Action> {
        let form = manage.editing.as_mut()?;
        let mut action = None;
        ui.heading(if form.original.is_some() {
            "Edit host"
        } else {
            "Add host"
        });
        ui.add_space(8.0);
        ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut form.kind, TargetKind::Address, "ILOM address");
            ui.selectable_value(&mut form.kind, TargetKind::Jnlp, "JNLP file");
        });
        match form.kind {
            TargetKind::Address => {
                ui.add(
                    egui::TextEdit::singleline(&mut form.host)
                        .hint_text("192.0.2.10")
                        .desired_width(f32::INFINITY),
                );
            }
            TargetKind::Jnlp => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut form.jnlp).desired_width(300.0));
                    if ui.button("Browse…").clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .set_title("Open ILOM console JNLP")
                            .pick_file()
                    {
                        form.jnlp = path.to_string_lossy().into_owned();
                    }
                });
            }
        }
        ui.label("Username (default root)");
        ui.add(egui::TextEdit::singleline(&mut form.user).desired_width(f32::INFINITY));
        ui.label("Host keyboard layout");
        egui::ComboBox::from_id_salt("host-form-layout")
            .selected_text(form.layout.map_or("Viewer setting", Layout::label))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut form.layout, None, "Viewer setting");
                for layout in Layout::ALL {
                    ui.selectable_value(&mut form.layout, Some(layout), layout.label());
                }
            });
        if form.kind == TargetKind::Address {
            ui.label("Password (optional)");
            ui.add(
                egui::TextEdit::singleline(&mut form.password)
                    .password(true)
                    .hint_text(match manage.saved_password {
                        Some(true) => "A password is saved: leave empty to keep it",
                        _ => "Not written to hosts.toml",
                    })
                    .desired_width(f32::INFINITY),
            );
            if !form.password.is_empty() {
                let mut save = !form.skip_keyring;
                ui.checkbox(&mut save, "Save to the OS keyring");
                form.skip_keyring = !save;
            }
            if form.original.is_some() {
                ui.horizontal(|ui| match manage.saved_password {
                    Some(true) => {
                        ui.small("A password is saved in the OS keyring.");
                        if ui.small_button("Forget saved password").clicked() {
                            action = form.original.clone().map(Action::Forget);
                        }
                    }
                    Some(false) => {
                        ui.small("No password saved in the OS keyring.");
                    }
                    None => {
                        ui.small("The OS keyring could not be read.");
                    }
                });
            }
        }
        egui::CollapsingHeader::new("Advanced")
            .id_salt(("host-advanced", form.original.clone()))
            .default_open(!form.password_env.is_empty())
            .show(ui, |ui| {
                ui.label("Password variable (optional)");
                ui.add(
                    egui::TextEdit::singleline(&mut form.password_env)
                        .hint_text("ILOM_PASSWORD_<NAME> is used by default")
                        .desired_width(f32::INFINITY),
                );
            });
        ui.add_space(4.0);
        ui.small(
            "Settings for ilom-vnc and ilom-mcp (listen, media_dir, \
             allow_writable_media) are edited in the file and kept on save.",
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                action = Some(Action::Save);
            }
            if ui.button("Cancel").clicked() {
                action = Some(Action::CancelEdit);
            }
        });
        action
    }
}

enum Action {
    Close,
    Add,
    Edit(String),
    Delete(String),
    Forget(String),
    CancelDelete,
    ConfirmDelete(String),
    CancelEdit,
    Save,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ilom_kvm_core::keyring::MemoryStore;

    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn web(host: &str) -> Host {
        Host {
            host: Some(host.into()),
            ..Host::default()
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ilom-hosts-ui-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("hosts.toml")
    }

    #[test]
    fn buffer_round_trips_through_host() {
        let host = Host {
            user: Some("admin".into()),
            layout: Some("fr".into()),
            password_env: Some("PW".into()),
            ..web("10.0.0.5")
        };
        let form = HostForm::from_host("rack1", &host);
        assert_eq!(form.original.as_deref(), Some("rack1"));
        assert_eq!(form.to_host().unwrap(), ("rack1".to_owned(), host));
    }

    #[test]
    fn buffer_is_trimmed_and_checked() {
        let form = HostForm {
            name: " rack1 ".into(),
            host: "https://10.0.0.5/ ".into(),
            user: "  ".into(),
            ..HostForm::default()
        };
        let (name, host) = form.to_host().unwrap();
        assert_eq!(name, "rack1");
        assert_eq!(host.host.as_deref(), Some("10.0.0.5"));
        assert_eq!(host.user, None);

        let empty_name = HostForm {
            host: "h".into(),
            ..HostForm::default()
        };
        assert!(empty_name.to_host().is_err());
        let no_address = HostForm {
            name: "a".into(),
            ..HostForm::default()
        };
        assert_eq!(no_address.to_host().unwrap_err(), "Enter the ILOM address");
        let no_jnlp = HostForm {
            name: "a".into(),
            kind: TargetKind::Jnlp,
            host: "ignored".into(),
            ..HostForm::default()
        };
        assert_eq!(no_jnlp.to_host().unwrap_err(), "Choose a JNLP file");
    }

    #[test]
    fn name_checks() {
        let mut file = HostsFile::default();
        file.hosts.insert("a".into(), web("h"));
        assert!(check_name(&file, None, "").is_err());
        assert!(check_name(&file, None, "a").is_err());
        assert!(check_name(&file, Some("a"), "a").is_ok());
        assert!(check_name(&file, Some("b"), "a").is_err());
        assert!(check_name(&file, Some("a"), "c").is_ok());
    }

    #[test]
    fn fill_uses_defaults() {
        let host = Host {
            layout: Some("fr".into()),
            ..web("h")
        };
        assert_eq!(
            fill(&host),
            Fill {
                address: "h".into(),
                username: "root".into(),
                layout: Some(Layout::French),
            }
        );
        assert_eq!(
            fill(&Host {
                user: Some("u".into()),
                ..web("h")
            })
            .username,
            "u"
        );
    }

    #[test]
    fn password_from_env_then_keyring_then_prompt() {
        let host = web("10.0.0.5");
        let with_env = env_of(&[("ILOM_PASSWORD_RACK1", "pw")]);
        let saved = MemoryStore::with(&[("rack1", "saved")]);
        let empty = MemoryStore::new();
        assert!(matches!(
            resolve("rack1", &host, "", &with_env, &saved),
            Resolved::Ready(Source::Web { ref password, .. }) if password == "pw"
        ));
        assert_eq!(
            lookup("rack1", &host, &with_env, &saved),
            PasswordLookup::Found(PasswordOrigin::Env)
        );

        let none = env_of(&[]);
        assert!(matches!(
            resolve("rack1", &host, "", &none, &saved),
            Resolved::Ready(Source::Web { ref password, .. }) if password == "saved"
        ));
        assert_eq!(
            lookup("rack1", &host, &none, &saved),
            PasswordLookup::Found(PasswordOrigin::Keyring)
        );
        // A typed password does not override the keyring entry.
        assert!(matches!(
            resolve("rack1", &host, "typed", &none, &saved),
            Resolved::Ready(Source::Web { ref password, .. }) if password == "saved"
        ));

        assert_eq!(
            resolve("rack1", &host, "", &none, &empty),
            Resolved::NeedPassword("ILOM_PASSWORD_RACK1".into())
        );
        assert_eq!(
            lookup("rack1", &host, &none, &empty),
            PasswordLookup::Prompt("ILOM_PASSWORD_RACK1".into())
        );
        assert_eq!(
            resolve("rack1", &host, "typed", &none, &empty),
            Resolved::Ready(Source::Web {
                host: "10.0.0.5".into(),
                username: "root".into(),
                password: "typed".into(),
            })
        );
        // A broken keyring behaves like an empty one.
        assert_eq!(
            resolve("rack1", &host, "", &none, &MemoryStore::failing()),
            Resolved::NeedPassword("ILOM_PASSWORD_RACK1".into())
        );
        let jnlp = Host {
            jnlp: Some("/tmp/a.jnlp".into()),
            ..Host::default()
        };
        assert!(matches!(
            resolve("j", &jnlp, "", &none, &empty),
            Resolved::Ready(Source::Jnlp(_))
        ));
        assert_eq!(lookup("j", &jnlp, &none, &empty), PasswordLookup::NotNeeded);
    }

    #[test]
    fn save_renames_and_preserves_foreign_keys() {
        let path = scratch("save");
        fs::write(
            &path,
            "[hosts.a]\nhost = \"h\"\nlisten = \"127.0.0.1:5901\"\n",
        )
        .unwrap();
        let file = HostsFile::load(&path).unwrap();
        let mut form = HostForm::from_host("a", &file.hosts["a"]);
        form.name = "b".into();
        form.host = "i".into();
        let store = MemoryStore::with(&[("a", "pw")]);
        let saved = apply_save(&path, &file, &form, &store).unwrap();
        assert_eq!((saved.name.as_str(), saved.warning), ("b", None));
        // The saved password follows the rename.
        assert_eq!(store.get("a").unwrap(), None);
        assert_eq!(store.get("b").unwrap().as_deref(), Some("pw"));
        let saved = HostsFile::load(&path).unwrap();
        assert_eq!(saved.hosts.keys().collect::<Vec<_>>(), ["b"]);
        assert_eq!(saved.hosts["b"].host.as_deref(), Some("i"));
        assert_eq!(saved.hosts["b"].listen.as_deref(), Some("127.0.0.1:5901"));

        let mut clash = HostForm {
            name: "b".into(),
            host: "x".into(),
            ..HostForm::default()
        };
        assert!(apply_save(&path, &saved, &clash, &store).is_err());
        clash.name = "c".into();
        assert_eq!(apply_save(&path, &saved, &clash, &store).unwrap().name, "c");
    }

    #[test]
    fn ui_state_follows_the_file() {
        let path = scratch("state");
        let mut ui = HostsUi::load(
            Some(path.clone()),
            Some("gone".into()),
            Arc::new(MemoryStore::new()),
        );
        assert_eq!(ui.selected, None);
        HostsFile::save_host(&path, "a", &web("h")).unwrap();
        ui.selected = Some("a".into());
        ui.reload();
        assert_eq!(ui.selected_host().map(|(name, _)| name), Some("a"));
        fs::write(&path, "[hosts.a]\n").unwrap();
        ui.reload();
        assert!(ui.load_error.is_some());
        assert!(ui.file().hosts.is_empty());
    }

    #[test]
    fn rename_with_a_broken_keyring_warns_but_saves() {
        let path = scratch("warn");
        fs::write(&path, "[hosts.a]\nhost = \"h\"\n").unwrap();
        let file = HostsFile::load(&path).unwrap();
        let mut form = HostForm::from_host("a", &file.hosts["a"]);
        form.name = "b".into();
        let saved = apply_save(&path, &file, &form, &MemoryStore::failing()).unwrap();
        assert_eq!(saved.name, "b");
        assert!(saved.warning.is_some());
        assert!(HostsFile::load(&path).unwrap().hosts.contains_key("b"));
    }

    #[test]
    fn password_lookup_is_cached_until_invalidated() {
        let path = scratch("cache");
        HostsFile::save_host(&path, "a", &web("h")).unwrap();
        let store = Arc::new(MemoryStore::new());
        let mut ui = HostsUi::load(Some(path), Some("a".into()), store.clone());
        let none = env_of(&[]);
        assert_eq!(
            ui.password_lookup(&none),
            Some(PasswordLookup::Prompt("ILOM_PASSWORD_A".into()))
        );
        store.set("a", "pw").unwrap();
        // Still the old answer: the keyring is not asked again.
        assert!(matches!(
            ui.password_lookup(&none),
            Some(PasswordLookup::Prompt(_))
        ));
        ui.invalidate();
        assert_eq!(
            ui.password_lookup(&none),
            Some(PasswordLookup::Found(PasswordOrigin::Keyring))
        );
    }

    fn file_with(name: &str, text: &str) -> (PathBuf, HostsFile) {
        let path = scratch(name);
        fs::write(&path, text).unwrap();
        let file = HostsFile::load(&path).unwrap();
        (path, file)
    }

    #[test]
    fn saving_with_a_password_stores_it_in_the_keyring_only() {
        let path = scratch("pw-new");
        let store = MemoryStore::new();
        let form = HostForm {
            name: "rack1".into(),
            host: "h".into(),
            password: "secret".into(),
            ..HostForm::default()
        };
        let saved = apply_save(&path, &HostsFile::default(), &form, &store).unwrap();
        assert_eq!(saved.warning, None);
        assert_eq!(store.get("rack1").unwrap().as_deref(), Some("secret"));
        assert!(!fs::read_to_string(&path).unwrap().contains("secret"));
    }

    #[test]
    fn empty_password_leaves_the_entry_alone() {
        let (path, file) = file_with("pw-empty", "[hosts.a]\nhost = \"h\"\n");
        let store = MemoryStore::with(&[("a", "old")]);
        let form = HostForm::from_host("a", &file.hosts["a"]);
        apply_save(&path, &file, &form, &store).unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("old"));
    }

    #[test]
    fn unticked_box_stores_nothing() {
        let path = scratch("pw-skip");
        let store = MemoryStore::new();
        let form = HostForm {
            name: "a".into(),
            host: "h".into(),
            password: "secret".into(),
            skip_keyring: true,
            ..HostForm::default()
        };
        apply_save(&path, &HostsFile::default(), &form, &store).unwrap();
        assert_eq!(store.get("a").unwrap(), None);
    }

    #[test]
    fn rename_moves_then_a_new_password_replaces() {
        let (path, file) = file_with("pw-rename", "[hosts.a]\nhost = \"h\"\n");
        let store = MemoryStore::with(&[("a", "old")]);
        let mut form = HostForm::from_host("a", &file.hosts["a"]);
        form.name = "b".into();
        apply_save(&path, &file, &form, &store).unwrap();
        assert_eq!(store.get("a").unwrap(), None);
        assert_eq!(store.get("b").unwrap().as_deref(), Some("old"));

        let file = HostsFile::load(&path).unwrap();
        let mut form = HostForm::from_host("b", &file.hosts["b"]);
        form.password = "new".into();
        apply_save(&path, &file, &form, &store).unwrap();
        assert_eq!(store.get("b").unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn failing_keyring_still_saves_the_host() {
        let path = scratch("pw-fail");
        let form = HostForm {
            name: "a".into(),
            host: "h".into(),
            password: "secret".into(),
            ..HostForm::default()
        };
        let saved =
            apply_save(&path, &HostsFile::default(), &form, &MemoryStore::failing()).unwrap();
        assert!(
            saved
                .warning
                .unwrap()
                .contains("password could not be saved")
        );
        assert_eq!(
            HostsFile::load(&path).unwrap().hosts["a"].host.as_deref(),
            Some("h")
        );
    }

    #[test]
    fn jnlp_hosts_take_no_password() {
        let path = scratch("pw-jnlp");
        let store = MemoryStore::new();
        let form = HostForm {
            name: "j".into(),
            kind: TargetKind::Jnlp,
            jnlp: "/tmp/a.jnlp".into(),
            password: "secret".into(),
            ..HostForm::default()
        };
        apply_save(&path, &HostsFile::default(), &form, &store).unwrap();
        assert_eq!(store.get("j").unwrap(), None);
    }

    #[test]
    fn password_env_survives_a_save() {
        let (path, file) = file_with(
            "pw-env",
            "[hosts.a]\nhost = \"h\"\npassword_env = \"MY_PW\"\n",
        );
        let form = HostForm::from_host("a", &file.hosts["a"]);
        assert_eq!(form.password_env, "MY_PW");
        apply_save(&path, &file, &form, &MemoryStore::new()).unwrap();
        let saved = HostsFile::load(&path).unwrap();
        assert_eq!(saved.hosts["a"].password_env.as_deref(), Some("MY_PW"));
    }

    #[test]
    fn plan_switch_cases() {
        assert_eq!(plan_switch(Some("a"), "a", false), SwitchPlan::Noop);
        assert_eq!(plan_switch(Some("a"), "a", true), SwitchPlan::Noop);
        assert_eq!(plan_switch(Some("a"), "b", false), SwitchPlan::Go);
        assert_eq!(plan_switch(Some("a"), "b", true), SwitchPlan::Confirm);
        // A custom (unnamed) session can switch to any host.
        assert_eq!(plan_switch(None, "b", false), SwitchPlan::Go);
        assert_eq!(plan_switch(None, "b", true), SwitchPlan::Confirm);
    }

    fn two_hosts_and_a_jnlp(name: &str) -> (PathBuf, Arc<MemoryStore>, HostsUi) {
        let path = scratch(name);
        fs::write(
            &path,
            "[hosts.a]\nhost = \"10.0.0.1\"\n[hosts.b]\nhost = \"10.0.0.2\"\n\
             [hosts.j]\njnlp = \"/tmp/j.jnlp\"\n",
        )
        .unwrap();
        let store = Arc::new(MemoryStore::new());
        let ui = HostsUi::load(Some(path.clone()), None, store.clone());
        (path, store, ui)
    }

    #[test]
    fn lookups_are_cached_per_host() {
        let (_, store, mut ui) = two_hosts_and_a_jnlp("multi");
        let none = env_of(&[]);
        assert_eq!(
            ui.password_lookup_for("a", &none),
            Some(PasswordLookup::Prompt("ILOM_PASSWORD_A".into()))
        );
        assert_eq!(
            ui.password_lookup_for("b", &none),
            Some(PasswordLookup::Prompt("ILOM_PASSWORD_B".into()))
        );
        assert_eq!(ui.password_lookup_for("zz", &none), None);
        store.set("a", "pw").unwrap();
        store.set("b", "pw").unwrap();
        // Both still cached: the keyring is not asked again.
        assert!(matches!(
            ui.password_lookup_for("a", &none),
            Some(PasswordLookup::Prompt(_))
        ));
        assert!(matches!(
            ui.password_lookup_for("b", &none),
            Some(PasswordLookup::Prompt(_))
        ));
        ui.invalidate();
        assert_eq!(
            ui.password_lookup_for("a", &none),
            Some(PasswordLookup::Found(PasswordOrigin::Keyring))
        );
        // Selecting a host reuses the per-host entry, cached once asked.
        ui.selected = Some("b".into());
        let found = Some(PasswordLookup::Found(PasswordOrigin::Keyring));
        assert_eq!(ui.password_lookup(&none), found);
        store.delete("b").unwrap();
        assert_eq!(ui.password_lookup(&none), found);
    }

    #[test]
    fn switch_resolution_env_keyring_none() {
        let (_, _, ui) = two_hosts_and_a_jnlp("switch");
        let file = ui.file();
        let none = env_of(&[]);
        let with_env = env_of(&[("ILOM_PASSWORD_A", "envpw")]);
        let store = MemoryStore::with(&[("b", "kpw")]);

        let (_, resolved) = switch_resolution(file, "a", &with_env, &store).unwrap();
        assert!(matches!(
            resolved,
            Resolved::Ready(Source::Web { ref password, .. }) if password == "envpw"
        ));
        let (_, resolved) = switch_resolution(file, "b", &none, &store).unwrap();
        assert!(matches!(
            resolved,
            Resolved::Ready(Source::Web { ref password, .. }) if password == "kpw"
        ));
        let (_, resolved) = switch_resolution(file, "a", &none, &store).unwrap();
        assert_eq!(resolved, Resolved::NeedPassword("ILOM_PASSWORD_A".into()));
        // A broken keyring means a prompt, not a failure.
        let (_, resolved) = switch_resolution(file, "b", &none, &MemoryStore::failing()).unwrap();
        assert_eq!(resolved, Resolved::NeedPassword("ILOM_PASSWORD_B".into()));
    }

    #[test]
    fn jnlp_and_unknown_hosts_cannot_be_switched_to() {
        let (_, _, ui) = two_hosts_and_a_jnlp("jnlp-switch");
        let store = MemoryStore::new();
        let none = env_of(&[]);
        let error = switch_resolution(ui.file(), "j", &none, &store).unwrap_err();
        assert!(error.contains("JNLP"), "{error}");
        assert!(switch_resolution(ui.file(), "nope", &none, &store).is_err());
    }

    #[test]
    fn switcher_lists_every_host_and_disables_jnlp() {
        let (_, _, ui) = two_hosts_and_a_jnlp("items");
        let mut cache = HostCache::default();
        cache.insert(
            "a",
            Arc::new(ilom_kvm_core::session::DecodedFrame {
                width: 4,
                height: 3,
                sequence: 1,
                rgba: vec![0; 48],
            }),
        );
        let items = ui.switcher_items(&cache);
        let names: Vec<_> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "j"]);
        assert!(items[0].switchable && items[1].switchable && !items[2].switchable);
        assert!(items[0].tooltip.contains("4×3"), "{}", items[0].tooltip);
        assert_eq!(items[1].tooltip, "");
        assert!(items[2].tooltip.contains("JNLP"));
    }

    #[test]
    fn dialog_changes_are_reported_once() {
        let (path, _, mut ui) = two_hosts_and_a_jnlp("changed");
        assert!(ui.take_changed().is_empty());
        // Simulate what the dialog records on save and delete.
        ui.changed.push("a".into());
        assert_eq!(ui.take_changed(), ["a"]);
        assert!(ui.take_changed().is_empty());
        let _ = path;
    }
}
