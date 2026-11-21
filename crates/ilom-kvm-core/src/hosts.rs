//! Named ILOM hosts, read from a TOML file shared by the headless front
//! ends (`ilom-mcp`, `ilom-vnc`).
//!
//! ```toml
//! [hosts.rack1]
//! host = "10.0.0.5"         # or jnlp = "/path/file.jnlp"
//! user = "root"             # default root
//! layout = "fr"             # optional
//! listen = "127.0.0.1:5901" # ilom-vnc only, required there
//! media_dir = "/srv/iso"    # ilom-mcp only
//! allow_writable_media = false
//! password_env = "ILOM_PASSWORD_RACK1"
//! ```
//!
//! Passwords are not in the file: they come from the environment, else from
//! the OS keyring entry named after the host (see [`crate::keyring`]).

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Table, value};
use tracing::warn;

use crate::{config, keymap::Layout, keyring::SecretStore, session::Source};

/// Name of the host built from `--host`/`--jnlp` when there is no file.
pub const DEFAULT_NAME: &str = "default";

/// Looks up an environment variable; injected so tests do not touch the
/// process environment.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub host: Option<String>,
    pub jnlp: Option<PathBuf>,
    pub user: Option<String>,
    pub layout: Option<String>,
    pub listen: Option<String>,
    pub media_dir: Option<PathBuf>,
    pub allow_writable_media: Option<bool>,
    pub password_env: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostsFile {
    #[serde(default)]
    pub hosts: BTreeMap<String, Host>,
}

/// No password for a web host: none of the variables is set. Front ends
/// with a keyboard can prompt instead of failing (downcast the error from
/// [`Host::source`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "no ILOM password for host {name:?}: set {variable} (or ILOM_PASSWORD) in the environment \
     or .env, or save one in the keyring from the viewer"
)]
pub struct MissingPassword {
    pub name: String,
    /// The per-host variable to set.
    pub variable: String,
}

/// Where a host's password was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordOrigin {
    Env,
    Keyring,
}

/// Parses a layout id such as `us` or `fr`, listing the valid ones on error.
pub fn parse_layout(value: &str) -> Result<Layout, String> {
    Layout::from_id(value).ok_or_else(|| {
        let ids: Vec<_> = Layout::ALL.iter().map(|layout| layout.id()).collect();
        format!(
            "unknown layout {value:?}, expected one of {}",
            ids.join(", ")
        )
    })
}

/// `rack-1` becomes `RACK_1`: the suffix of the per-host variables.
pub fn env_suffix(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

impl HostsFile {
    /// `$ILOM_HOSTS`, else `hosts.toml` in the config directory.
    pub fn default_path() -> Result<PathBuf> {
        match std::env::var_os("ILOM_HOSTS").filter(|value| !value.is_empty()) {
            Some(path) => Ok(PathBuf::from(path)),
            None => Ok(config::dir()?.join("hosts.toml")),
        }
    }

    /// Reads and validates `path`. A missing file gives no hosts.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("read {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: Self = toml::from_str(text).context("invalid hosts file")?;
        for (name, host) in &file.hosts {
            host.validate(name)
                .with_context(|| format!("host {name:?}"))?;
        }
        for name in file.hosts.keys() {
            check_suffix_clash(file.hosts.keys().map(String::as_str), name)?;
        }
        Ok(file)
    }
}

/// Fails when two distinct host names share an `env_suffix`, since they would
/// read each other's `ILOM_PASSWORD_<NAME>` variable.
fn check_suffix_clash<'a>(names: impl Iterator<Item = &'a str>, name: &str) -> Result<()> {
    let suffix = env_suffix(name);
    if let Some(other) = names
        .into_iter()
        .find(|other| *other != name && env_suffix(other) == suffix)
    {
        bail!(
            "hosts {name:?} and {other:?} would share the password variable ILOM_PASSWORD_{suffix}; rename one"
        );
    }
    Ok(())
}

impl HostsFile {
    /// Creates or updates `[hosts.<name>]`, setting only the keys the GUI
    /// owns (`host`/`jnlp`, `user`, `layout`, `password_env`). Other keys,
    /// other hosts and comments stay as they are.
    pub fn save_host(path: &Path, name: &str, host: &Host) -> Result<()> {
        host.validate(name)?;
        if let Ok(existing) = Self::load(path) {
            check_suffix_clash(existing.hosts.keys().map(String::as_str), name)?;
        }
        let mut document = read_document(path)?;
        let table = host_table(&mut document, name)?;
        set_or_remove(table, "host", host.host.as_deref());
        set_or_remove(
            table,
            "jnlp",
            host.jnlp
                .as_deref()
                .map(|path| path.to_string_lossy())
                .as_deref(),
        );
        set_or_remove(table, "user", host.user.as_deref());
        set_or_remove(table, "layout", host.layout.as_deref());
        set_or_remove(table, "password_env", host.password_env.as_deref());
        write_document(path, &document)
    }

    /// Removes `[hosts.<name>]`; false when there was none.
    pub fn remove_host(path: &Path, name: &str) -> Result<bool> {
        let mut document = read_document(path)?;
        let removed = document
            .get_mut("hosts")
            .and_then(Item::as_table_like_mut)
            .is_some_and(|hosts| hosts.remove(name).is_some());
        if removed {
            write_document(path, &document)?;
        }
        Ok(removed)
    }

    /// Renames `[hosts.<old>]`, keeping its keys and comments. Fails when
    /// `new` exists or `old` does not.
    pub fn rename_host(path: &Path, old: &str, new: &str) -> Result<()> {
        if new.trim().is_empty() {
            bail!("the name must not be empty");
        }
        let mut document = read_document(path)?;
        let hosts = document
            .get_mut("hosts")
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| anyhow!("no host called {old:?}"))?;
        if hosts.contains_key(new) {
            bail!("a host called {new:?} already exists");
        }
        let others = hosts
            .iter()
            .map(|(key, _)| key.to_string())
            .filter(|key| key != old);
        check_suffix_clash(others.collect::<Vec<_>>().iter().map(String::as_str), new)?;
        let item = hosts
            .remove(old)
            .ok_or_else(|| anyhow!("no host called {old:?}"))?;
        hosts.insert(new, item);
        write_document(path, &document)
    }
}

fn read_document(path: &Path) -> Result<DocumentMut> {
    match fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .with_context(|| format!("parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// `[hosts.<name>]`, created (with an implicit `[hosts]`) when missing.
fn host_table<'a>(document: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table> {
    if !document.contains_key("hosts") {
        let mut hosts = Table::new();
        hosts.set_implicit(true);
        document.insert("hosts", Item::Table(hosts));
    }
    let hosts = document["hosts"]
        .as_table_mut()
        .ok_or_else(|| anyhow!("`hosts` is not a table"))?;
    if !hosts.contains_key(name) {
        hosts.insert(name, Item::Table(Table::new()));
    }
    hosts[name]
        .as_table_mut()
        .ok_or_else(|| anyhow!("host {name:?} is not a table"))
}

fn set_or_remove(table: &mut Table, key: &str, new: Option<&str>) {
    match new {
        Some(new) => {
            // Keep the existing key's comments when only the value changes.
            if let Some(Item::Value(old)) = table.get_mut(key) {
                let decor = old.decor().clone();
                let mut replacement = value(new);
                if let Some(replacement) = replacement.as_value_mut() {
                    *replacement.decor_mut() = decor;
                }
                *old = replacement.into_value().expect("a value");
            } else {
                table.insert(key, value(new));
            }
        }
        None => {
            table.remove(key);
        }
    }
}

/// Writes through a temporary file so a crash never leaves half a file.
fn write_document(path: &Path, document: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    // Write through a symlink (dotfile managers) instead of replacing it.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = target.as_path();
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, document.to_string())
        .with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}

impl Host {
    /// Checks the name and the fields the GUI edits.
    pub fn validate(&self, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            bail!("the name must not be empty");
        }
        match (&self.host, &self.jnlp) {
            (Some(_), None) | (None, Some(_)) => {}
            (Some(_), Some(_)) => bail!("set either host or jnlp, not both"),
            (None, None) => bail!("set host or jnlp"),
        }
        if let Some(layout) = &self.layout {
            parse_layout(layout).map_err(|message| anyhow!(message))?;
        }
        Ok(())
    }

    pub fn layout(&self) -> Option<Layout> {
        self.layout.as_deref().and_then(Layout::from_id)
    }

    /// Finds the password: `password_env`, else `ILOM_PASSWORD_<NAME>`, else
    /// `ILOM_PASSWORD`, else the keyring entry named `name`. A keyring that
    /// fails or does not answer counts as having no entry.
    fn find_password(
        &self,
        name: &str,
        env: Env,
        store: &dyn SecretStore,
    ) -> Result<(String, PasswordOrigin), MissingPassword> {
        let per_host = format!("ILOM_PASSWORD_{}", env_suffix(name));
        let mut candidates = Vec::new();
        if let Some(variable) = &self.password_env {
            candidates.push(variable.clone());
        }
        candidates.push(per_host.clone());
        candidates.push("ILOM_PASSWORD".to_string());
        if let Some(password) = candidates
            .iter()
            .find_map(|variable| env(variable).filter(|value| !value.is_empty()))
        {
            return Ok((password, PasswordOrigin::Env));
        }
        match store.get(name) {
            Ok(Some(password)) if !password.is_empty() => {
                return Ok((password, PasswordOrigin::Keyring));
            }
            Ok(_) => {}
            Err(error) => warn!(host = name, error = %format!("{error:#}"), "keyring not used"),
        }
        Err(MissingPassword {
            name: name.to_string(),
            variable: self.password_env.clone().unwrap_or(per_host),
        })
    }

    /// Where the password comes from; `None` when there is none (or the host
    /// is a JNLP file, which needs none).
    pub fn password_origin(
        &self,
        name: &str,
        env: Env,
        store: &dyn SecretStore,
    ) -> Option<PasswordOrigin> {
        if self.jnlp.is_some() {
            return None;
        }
        self.find_password(name, env, store)
            .ok()
            .map(|(_, origin)| origin)
    }

    /// Where the console session gets its launch parameters. The password
    /// comes from the environment, else from the keyring (see
    /// [`Host::find_password`]). A JNLP file needs none.
    pub fn source(&self, name: &str, env: Env, store: &dyn SecretStore) -> Result<Source> {
        if let Some(path) = &self.jnlp {
            return Ok(Source::Jnlp(path.clone()));
        }
        let host = self
            .host
            .clone()
            .ok_or_else(|| anyhow!("host {name:?} has no host or jnlp"))?;
        let (password, _) = self.find_password(name, env, store)?;
        Ok(Source::Web {
            host,
            username: self.user.clone().unwrap_or_else(|| "root".into()),
            password,
        })
    }

    /// VNC password: `ILOM_VNC_PASSWORD_<NAME>`, else `ILOM_VNC_PASSWORD`.
    pub fn vnc_password(&self, name: &str, env: Env) -> Option<String> {
        let per_host = format!("ILOM_VNC_PASSWORD_{}", env_suffix(name));
        [per_host.as_str(), "ILOM_VNC_PASSWORD"]
            .iter()
            .find_map(|variable| env(variable).filter(|value| !value.is_empty()))
    }

    /// Display name for VNC clients.
    pub fn title(&self, name: &str) -> String {
        match &self.host {
            Some(host) => format!("ILOM {host}"),
            None => format!("ILOM {name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyring::MemoryStore;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn parses_hosts_and_defaults() {
        let file = HostsFile::parse(
            r#"
            [hosts.rack1]
            host = "10.0.0.5"
            layout = "fr"
            listen = "127.0.0.1:5901"
            media_dir = "/srv/iso"
            allow_writable_media = true

            [hosts.rack2]
            jnlp = "/tmp/a.jnlp"
            "#,
        )
        .unwrap();
        assert_eq!(file.hosts.len(), 2);
        let rack1 = &file.hosts["rack1"];
        assert_eq!(rack1.layout(), Some(Layout::French));
        assert_eq!(rack1.allow_writable_media, Some(true));
        assert_eq!(rack1.user, None);
        let env = env_of(&[("ILOM_PASSWORD", "pw")]);
        assert!(matches!(
            rack1.source("rack1", &env, &MemoryStore::new()).unwrap(),
            Source::Web { ref username, .. } if username == "root"
        ));
        assert!(matches!(
            file.hosts["rack2"]
                .source("rack2", &env, &MemoryStore::new())
                .unwrap(),
            Source::Jnlp(_)
        ));
    }

    #[test]
    fn rejects_invalid_entries() {
        for text in [
            "[hosts.a]\n",
            "[hosts.a]\nhost = \"h\"\njnlp = \"f\"\n",
            "[hosts.a]\nhost = \"h\"\nlayout = \"xx\"\n",
            "[hosts.a]\nhost = \"h\"\nbogus = 1\n",
            "[hosts.\"\"]\nhost = \"h\"\n",
            "[hosts.a]\nhost = \"h\"\n[hosts.a]\nhost = \"i\"\n",
        ] {
            assert!(HostsFile::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let path = std::env::temp_dir().join("ilom-hosts-does-not-exist.toml");
        assert!(HostsFile::load(&path).unwrap().hosts.is_empty());
    }

    #[test]
    fn password_resolution_order() {
        let host = Host {
            host: Some("h".into()),
            password_env: Some("MY_PW".into()),
            ..Host::default()
        };
        let source =
            |pairs: &[(&str, &str)]| host.source("rack-1", &env_of(pairs), &MemoryStore::new());
        let password = |pairs: &[(&str, &str)]| match source(pairs).unwrap() {
            Source::Web { password, .. } => password,
            Source::Jnlp(_) => unreachable!(),
        };
        let all = [
            ("MY_PW", "custom"),
            ("ILOM_PASSWORD_RACK_1", "named"),
            ("ILOM_PASSWORD", "global"),
        ];
        assert_eq!(password(&all), "custom");
        assert_eq!(password(&all[1..]), "named");
        assert_eq!(password(&all[2..]), "global");
        let error = source(&[("ILOM_PASSWORD", "")]).unwrap_err().to_string();
        assert!(error.contains("MY_PW"), "{error}");
    }

    #[test]
    fn missing_password_names_the_variable() {
        let host = Host {
            host: Some("h".into()),
            ..Host::default()
        };
        let error = host
            .source("rack-1", &env_of(&[]), &MemoryStore::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("ILOM_PASSWORD_RACK_1"), "{error}");
    }

    #[test]
    fn vnc_password_and_name_sanitising() {
        assert_eq!(env_suffix("rack-1.b"), "RACK_1_B");
    }

    #[test]
    fn names_sharing_a_password_variable_are_rejected() {
        let text = "[hosts.rack-1]\nhost = \"a\"\n[hosts.rack_1]\nhost = \"b\"\n";
        let error = HostsFile::parse(text).unwrap_err().to_string();
        assert!(error.contains("ILOM_PASSWORD_RACK_1"), "{error}");
        let host = Host {
            host: Some("h".into()),
            ..Host::default()
        };
        let both = [("ILOM_VNC_PASSWORD_A", "x"), ("ILOM_VNC_PASSWORD", "y")];
        assert_eq!(host.vnc_password("a", &env_of(&both)).as_deref(), Some("x"));
        assert_eq!(host.vnc_password("b", &env_of(&both)).as_deref(), Some("y"));
        assert_eq!(host.vnc_password("b", &env_of(&[])), None);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ilom-hosts-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("hosts.toml")
    }

    fn web(host: &str) -> Host {
        Host {
            host: Some(host.into()),
            ..Host::default()
        }
    }

    #[test]
    fn save_creates_the_file_and_directory() {
        let path = scratch("create")
            .parent()
            .unwrap()
            .join("nested")
            .join("hosts.toml");
        let host = Host {
            user: Some("admin".into()),
            layout: Some("fr".into()),
            ..web("10.0.0.5")
        };
        HostsFile::save_host(&path, "rack1", &host).unwrap();
        let file = HostsFile::load(&path).unwrap();
        assert_eq!(file.hosts["rack1"], host);
    }

    #[test]
    fn edit_keeps_comments_and_foreign_keys() {
        let path = scratch("edit");
        fs::write(
            &path,
            "# my racks\n[hosts.rack1]\nhost = \"10.0.0.5\" # main\nlisten = \"127.0.0.1:5901\"\n\
             media_dir = \"/srv/iso\"\nallow_writable_media = true\nuser = \"old\"\n\n\
             [hosts.rack2]\nhost = \"10.0.0.6\"\nlisten = \"127.0.0.1:5902\"\n",
        )
        .unwrap();
        let edited = Host {
            user: None,
            layout: Some("us".into()),
            ..web("10.0.0.9")
        };
        HostsFile::save_host(&path, "rack1", &edited).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my racks"), "{text}");
        assert!(text.contains("# main"), "{text}");
        assert!(!text.contains("user"), "{text}");
        let file = HostsFile::load(&path).unwrap();
        let rack1 = &file.hosts["rack1"];
        assert_eq!(rack1.host.as_deref(), Some("10.0.0.9"));
        assert_eq!(rack1.layout.as_deref(), Some("us"));
        assert_eq!(rack1.listen.as_deref(), Some("127.0.0.1:5901"));
        assert_eq!(rack1.media_dir, Some(PathBuf::from("/srv/iso")));
        assert_eq!(rack1.allow_writable_media, Some(true));
        assert_eq!(file.hosts["rack2"].host.as_deref(), Some("10.0.0.6"));
    }

    #[test]
    fn switching_between_host_and_jnlp() {
        let path = scratch("jnlp");
        HostsFile::save_host(&path, "a", &web("h")).unwrap();
        let jnlp = Host {
            jnlp: Some("/tmp/a.jnlp".into()),
            ..Host::default()
        };
        HostsFile::save_host(&path, "a", &jnlp).unwrap();
        let file = HostsFile::load(&path).unwrap();
        assert_eq!(file.hosts["a"].host, None);
        assert_eq!(file.hosts["a"].jnlp, Some(PathBuf::from("/tmp/a.jnlp")));
    }

    #[test]
    fn remove_host_keeps_the_others() {
        let path = scratch("remove");
        HostsFile::save_host(&path, "a", &web("h")).unwrap();
        HostsFile::save_host(&path, "b", &web("i")).unwrap();
        assert!(HostsFile::remove_host(&path, "a").unwrap());
        assert!(!HostsFile::remove_host(&path, "a").unwrap());
        let file = HostsFile::load(&path).unwrap();
        assert_eq!(file.hosts.keys().collect::<Vec<_>>(), ["b"]);
        assert!(!HostsFile::remove_host(&scratch("missing"), "a").unwrap());
    }

    #[test]
    fn rename_refuses_an_existing_target() {
        let path = scratch("rename");
        fs::write(&path, "[hosts.a]\n# about a\nhost = \"h\"\n").unwrap();
        HostsFile::save_host(&path, "b", &web("i")).unwrap();
        assert!(HostsFile::rename_host(&path, "a", "b").is_err());
        assert!(HostsFile::rename_host(&path, "zz", "c").is_err());
        assert!(HostsFile::rename_host(&path, "a", " ").is_err());
        HostsFile::rename_host(&path, "a", "c").unwrap();
        let file = HostsFile::load(&path).unwrap();
        assert_eq!(file.hosts.keys().collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(file.hosts["c"].host.as_deref(), Some("h"));
        assert!(fs::read_to_string(&path).unwrap().contains("# about a"));
    }

    #[test]
    fn save_validates() {
        let path = scratch("validate");
        assert!(HostsFile::save_host(&path, "", &web("h")).is_err());
        assert!(HostsFile::save_host(&path, "a", &Host::default()).is_err());
        let both = Host {
            jnlp: Some("f".into()),
            ..web("h")
        };
        assert!(HostsFile::save_host(&path, "a", &both).is_err());
        let bad_layout = Host {
            layout: Some("xx".into()),
            ..web("h")
        };
        assert!(HostsFile::save_host(&path, "a", &bad_layout).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn missing_password_can_be_downcast() {
        let error = web("h")
            .source("rack-1", &env_of(&[]), &MemoryStore::new())
            .unwrap_err();
        let missing = error.downcast_ref::<MissingPassword>().unwrap();
        assert_eq!(missing.variable, "ILOM_PASSWORD_RACK_1");
        assert_eq!(missing.name, "rack-1");
    }

    fn password(source: Source) -> String {
        match source {
            Source::Web { password, .. } => password,
            Source::Jnlp(_) => unreachable!(),
        }
    }

    #[test]
    fn keyring_comes_after_the_environment() {
        let host = web("h");
        let store = MemoryStore::with(&[("rack1", "saved")]);
        let with_env = env_of(&[("ILOM_PASSWORD", "global")]);
        let source = host.source("rack1", &with_env, &store).unwrap();
        assert_eq!(password(source), "global");
        assert_eq!(
            host.password_origin("rack1", &with_env, &store),
            Some(PasswordOrigin::Env)
        );
        let source = host.source("rack1", &env_of(&[]), &store).unwrap();
        assert_eq!(password(source), "saved");
        assert_eq!(
            host.password_origin("rack1", &env_of(&[]), &store),
            Some(PasswordOrigin::Keyring)
        );
        // The entry is per host name.
        assert_eq!(host.password_origin("rack2", &env_of(&[]), &store), None);
    }

    #[test]
    fn keyring_failure_falls_through() {
        let host = web("h");
        let store = MemoryStore::failing();
        let error = host.source("rack1", &env_of(&[]), &store).unwrap_err();
        assert!(error.downcast_ref::<MissingPassword>().is_some());
        // Env still works with a broken keyring.
        let with_env = env_of(&[("ILOM_PASSWORD", "global")]);
        assert!(host.source("rack1", &with_env, &store).is_ok());
        // An empty saved password is no password.
        let empty = MemoryStore::with(&[("rack1", "")]);
        assert!(host.source("rack1", &env_of(&[]), &empty).is_err());
    }

    #[test]
    fn jnlp_has_no_password_origin() {
        let jnlp = Host {
            jnlp: Some("f".into()),
            ..Host::default()
        };
        let store = MemoryStore::with(&[("a", "pw")]);
        assert_eq!(jnlp.password_origin("a", &env_of(&[]), &store), None);
    }
}
