//! Trust-on-first-use store for SP certificates, like SSH `known_hosts`.
//!
//! The web login runs before any JNLP exists, so there is nothing to pin the
//! HTTPS connection against on the first visit. After a successful login the
//! certificate fingerprint is stored here. Later logins pin to it, so the
//! password is never sent to a server with a different certificate.
//!
//! File format: one `host fingerprint` pair per line, fingerprint as
//! colon-separated hex. Lines starting with `#` are comments.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};

use crate::{jnlp, tls};

/// Overrides the store location (used by tests and portable setups).
pub const PATH_ENV: &str = "ILOM_KNOWN_CERTS";

#[derive(Debug, Default)]
pub struct KnownCerts {
    path: PathBuf,
    entries: Vec<(String, [u8; 32])>,
}

impl KnownCerts {
    /// Default location: `$ILOM_KNOWN_CERTS`, else
    /// `$XDG_CONFIG_HOME/ilom-kvm/known_certs`, else
    /// `~/.config/ilom-kvm/known_certs`.
    pub fn default_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os(PATH_ENV) {
            return Ok(path.into());
        }
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
            .ok_or_else(|| anyhow!("cannot locate the config directory (HOME is not set)"))?;
        Ok(config.join("ilom-kvm").join("known_certs"))
    }

    pub fn load_default() -> Result<Self> {
        Self::load(Self::default_path()?)
    }

    /// Loads the store. A missing file is an empty store.
    pub fn load(path: PathBuf) -> Result<Self> {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let mut entries = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (host, fingerprint) = line.split_once(char::is_whitespace).ok_or_else(|| {
                anyhow!(
                    "{}:{}: expected `host fingerprint`",
                    path.display(),
                    number + 1
                )
            })?;
            let fingerprint = jnlp::parse_fingerprint(fingerprint.trim())
                .with_context(|| format!("{}:{}", path.display(), number + 1))?;
            entries.push((host.to_string(), fingerprint));
        }
        Ok(Self { path, entries })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, host: &str) -> Option<[u8; 32]> {
        self.entries
            .iter()
            .find(|(known, _)| known == host)
            .map(|(_, fingerprint)| *fingerprint)
    }

    /// Records the fingerprint for `host`, replacing any older one, and saves.
    pub fn insert(&mut self, host: &str, fingerprint: [u8; 32]) -> Result<()> {
        self.entries.retain(|(known, _)| known != host);
        self.entries.push((host.to_string(), fingerprint));
        self.save()
    }

    /// Forgets `host`. Returns whether an entry existed.
    pub fn remove(&mut self, host: &str) -> Result<bool> {
        let before = self.entries.len();
        self.entries.retain(|(known, _)| known != host);
        if self.entries.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut text =
            String::from("# ilom-kvm pinned SP certificates: host SHA-256 fingerprint\n");
        for (host, fingerprint) in &self.entries {
            text.push_str(&format!(
                "{host} {}\n",
                tls::format_fingerprint(fingerprint)
            ));
        }
        // Write then rename, so a crash never leaves a truncated store.
        let temporary = self.path.with_extension("tmp");
        fs::write(&temporary, text).with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replace {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ilom-kvm-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("known_certs")
    }

    #[test]
    fn missing_file_is_empty() {
        let store = KnownCerts::load(scratch("missing")).unwrap();
        assert_eq!(store.get("sp"), None);
    }

    #[test]
    fn insert_replace_remove_round_trip() {
        let path = scratch("round-trip");
        let mut store = KnownCerts::load(path.clone()).unwrap();
        store.insert("sp", [1; 32]).unwrap();
        store.insert("other", [2; 32]).unwrap();
        store.insert("sp", [3; 32]).unwrap();

        let mut reloaded = KnownCerts::load(path.clone()).unwrap();
        assert_eq!(reloaded.get("sp"), Some([3; 32]));
        assert_eq!(reloaded.get("other"), Some([2; 32]));
        assert!(reloaded.remove("sp").unwrap());
        assert!(!reloaded.remove("sp").unwrap());
        assert_eq!(KnownCerts::load(path.clone()).unwrap().get("sp"), None);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_malformed_lines() {
        let path = scratch("malformed");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# comment\n\nsp not-hex\n").unwrap();
        assert!(KnownCerts::load(path.clone()).is_err());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
