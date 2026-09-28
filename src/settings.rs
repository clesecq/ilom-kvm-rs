//! GUI choices remembered between runs. The password is never stored.
//!
//! File format: `key = value` lines; lines starting with `#` are comments and
//! unknown keys are ignored, so older and newer versions can share the file.

use std::{fs, io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result};

use crate::config;

/// Overrides the settings file location (used by tests and portable setups).
pub const PATH_ENV: &str = "ILOM_SETTINGS";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub host: Option<String>,
    pub username: Option<String>,
    /// `--host-key` value name, e.g. `right-ctrl`.
    pub host_key: Option<String>,
    /// Host keyboard layout id, e.g. `fr`.
    pub layout: Option<String>,
    /// `true` for the 1:1 view, `false` to fit the window.
    pub actual_size: Option<bool>,
}

impl Settings {
    /// `$ILOM_SETTINGS`, else `settings` in the config directory.
    pub fn default_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os(PATH_ENV) {
            return Ok(path.into());
        }
        Ok(config::dir()?.join("settings"))
    }

    /// Loads the settings; a missing file gives the defaults.
    pub fn load(path: &PathBuf) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines().map(str::trim) {
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = Some(value.trim().to_owned()).filter(|value| !value.is_empty());
            match key.trim() {
                "host" => settings.host = value,
                "username" => settings.username = value,
                "host_key" => settings.host_key = value,
                "layout" => settings.layout = value,
                "scale" => {
                    settings.actual_size = match value.as_deref() {
                        Some("100%") => Some(true),
                        Some("fit") => Some(false),
                        _ => None,
                    }
                }
                _ => {}
            }
        }
        settings
    }

    pub fn to_text(&self) -> String {
        let mut text = String::from("# ilom-kvm settings (the password is never stored)\n");
        for (key, value) in [
            ("host", &self.host),
            ("username", &self.username),
            ("host_key", &self.host_key),
            ("layout", &self.layout),
        ] {
            // Line breaks would corrupt the file; such values are skipped.
            if let Some(value) = value.as_deref().filter(|value| !value.contains('\n')) {
                text.push_str(&format!("{key} = {value}\n"));
            }
        }
        if let Some(actual_size) = self.actual_size {
            let scale = if actual_size { "100%" } else { "fit" };
            text.push_str(&format!("scale = {scale}\n"));
        }
        text
    }

    /// Writes through a temporary file so a crash never leaves half a file.
    pub fn save(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, self.to_text())
            .with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_ignores_unknown_keys() {
        let settings = Settings {
            host: Some("192.0.2.10".into()),
            username: Some("root".into()),
            host_key: Some("menu".into()),
            layout: None,
            actual_size: Some(true),
        };
        assert_eq!(Settings::parse(&settings.to_text()), settings);
        let parsed = Settings::parse("# comment\nfuture = 1\nlayout = fr\nhost =\n");
        assert_eq!(parsed.layout.as_deref(), Some("fr"));
        assert_eq!(parsed.host, None);
    }

    #[test]
    fn save_creates_the_directory_and_replaces_the_file() {
        let dir = std::env::temp_dir().join(format!("ilom-settings-{}", std::process::id()));
        let path = dir.join("nested").join("settings");
        let mut settings = Settings {
            layout: Some("us".into()),
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        settings.layout = Some("fr".into());
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), settings);
        fs::remove_dir_all(dir).unwrap();
    }
}
