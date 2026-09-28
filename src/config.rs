//! Per-user configuration directory, shared by the certificate store and the
//! GUI settings.

use std::path::PathBuf;

use anyhow::{Result, anyhow};

/// `$XDG_CONFIG_HOME/ilom-kvm` when set, else the platform config directory:
/// `~/.config/ilom-kvm` on Linux, `~/Library/Application Support/ilom-kvm` on
/// macOS and `%APPDATA%\ilom-kvm` on Windows. An existing `~/.config/ilom-kvm`
/// (the only location used by 0.1.x) is kept on every Unix.
/// Default screenshot folder: `ilom-kvm` in the user's Pictures folder, else
/// in the home folder. Never relative to the working directory, which is `/`
/// for apps started from the macOS Finder.
pub fn screenshot_dir() -> PathBuf {
    dirs::picture_dir()
        .or_else(dirs::home_dir)
        .map(|base| base.join("ilom-kvm"))
        .unwrap_or_else(|| PathBuf::from("captures"))
}

pub fn dir() -> Result<PathBuf> {
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(config).join("ilom-kvm"));
    }
    if cfg!(unix)
        && let Some(legacy) = dirs::home_dir().map(|home| home.join(".config").join("ilom-kvm"))
        && legacy.is_dir()
    {
        return Ok(legacy);
    }
    dirs::config_dir()
        .map(|config| config.join("ilom-kvm"))
        .ok_or_else(|| anyhow!("cannot locate the user config directory"))
}
