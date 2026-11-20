//! ILOM passwords in the operating system's keyring: Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux.
//!
//! Entries are filed under the service `ilom-kvm`, with the host's name from
//! `hosts.toml` as the user. Front ends use the [`SecretStore`] trait so
//! tests can inject a [`MemoryStore`] and never touch the real keyring.

use std::{
    collections::HashMap,
    sync::{Mutex, mpsc},
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow};

/// Service name of every entry.
pub const SERVICE: &str = "ilom-kvm";

/// How long a keyring call may take: a locked or missing keyring can wait
/// for a prompt that never comes (headless sessions).
pub const TIMEOUT: Duration = Duration::from_secs(3);

/// Password storage by host name.
pub trait SecretStore: Send + Sync {
    /// The saved password; `None` when there is none.
    fn get(&self, name: &str) -> Result<Option<String>>;
    fn set(&self, name: &str, password: &str) -> Result<()>;
    /// Removes the entry. A missing entry is not an error.
    fn delete(&self, name: &str) -> Result<()>;
}

/// The operating system's keyring.
#[derive(Debug, Clone, Copy)]
pub struct OsKeyring {
    timeout: Duration,
}

impl Default for OsKeyring {
    fn default() -> Self {
        Self { timeout: TIMEOUT }
    }
}

impl OsKeyring {
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

/// Runs `work` on its own thread and gives up after `timeout`. The thread
/// is left to finish by itself.
pub fn run_with_timeout<T: Send + 'static>(
    timeout: Duration,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("ilom-keyring".into())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .map_err(|error| anyhow!("could not start the keyring thread: {error}"))?;
    receiver
        .recv_timeout(timeout)
        .map_err(|_| anyhow!("the keyring did not answer within {} s", timeout.as_secs()))?
}

fn entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).map_err(|error| anyhow!("keyring unavailable: {error}"))
}

impl SecretStore for OsKeyring {
    fn get(&self, name: &str) -> Result<Option<String>> {
        let name = name.to_owned();
        run_with_timeout(self.timeout, move || match entry(&name)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(anyhow!("keyring read failed: {error}")),
        })
    }

    fn set(&self, name: &str, password: &str) -> Result<()> {
        let (name, password) = (name.to_owned(), password.to_owned());
        run_with_timeout(self.timeout, move || {
            entry(&name)?
                .set_password(&password)
                .map_err(|error| anyhow!("keyring write failed: {error}"))
        })
    }

    fn delete(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        run_with_timeout(self.timeout, move || {
            match entry(&name)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(anyhow!("keyring delete failed: {error}")),
            }
        })
    }
}

/// In-memory store for tests. `failing` makes every call fail, like a
/// keyring that is locked or missing.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: Mutex<HashMap<String, String>>,
    failing: bool,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn failing() -> Self {
        Self {
            failing: true,
            ..Self::default()
        }
    }

    pub fn with(entries: &[(&str, &str)]) -> Self {
        let store = Self::default();
        for (name, password) in entries {
            store.entries().insert((*name).into(), (*password).into());
        }
        store
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn check(&self) -> Result<()> {
        if self.failing {
            Err(anyhow!("keyring unavailable"))
        } else {
            Ok(())
        }
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, name: &str) -> Result<Option<String>> {
        self.check()?;
        Ok(self.entries().get(name).cloned())
    }

    fn set(&self, name: &str, password: &str) -> Result<()> {
        self.check()?;
        self.entries().insert(name.into(), password.into());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.check()?;
        self.entries().remove(name);
        Ok(())
    }
}

/// Moves a saved password to a renamed host. Nothing happens when there is
/// none; the old entry is only removed once the new one is written.
pub fn move_entry(store: &dyn SecretStore, old: &str, new: &str) -> Result<()> {
    if old == new {
        return Ok(());
    }
    if let Some(password) = store.get(old)? {
        store.set(new, &password)?;
        store.delete(old)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryStore::new();
        assert_eq!(store.get("a").unwrap(), None);
        store.set("a", "pw").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("pw"));
        store.delete("a").unwrap();
        store.delete("a").unwrap();
        assert_eq!(store.get("a").unwrap(), None);
    }

    #[test]
    fn failing_store_fails() {
        let store = MemoryStore::failing();
        assert!(store.get("a").is_err());
        assert!(store.set("a", "x").is_err());
        assert!(store.delete("a").is_err());
    }

    #[test]
    fn rename_moves_the_entry() {
        let store = MemoryStore::with(&[("a", "pw")]);
        move_entry(&store, "a", "b").unwrap();
        assert_eq!(store.get("a").unwrap(), None);
        assert_eq!(store.get("b").unwrap().as_deref(), Some("pw"));
        // Nothing saved: nothing created.
        move_entry(&store, "x", "y").unwrap();
        assert_eq!(store.get("y").unwrap(), None);
        // Unchanged name: untouched.
        move_entry(&store, "b", "b").unwrap();
        assert_eq!(store.get("b").unwrap().as_deref(), Some("pw"));
    }

    #[test]
    fn slow_calls_time_out() {
        let result: Result<()> = run_with_timeout(Duration::from_millis(50), || {
            thread::sleep(Duration::from_secs(2));
            Ok(())
        });
        let error = result.unwrap_err().to_string();
        assert!(error.contains("did not answer"), "{error}");
        let fast = run_with_timeout(Duration::from_secs(2), || Ok(7));
        assert_eq!(fast.unwrap(), 7);
    }
}
