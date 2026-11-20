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

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::{config, keymap::Layout, session::Source};

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
        Ok(file)
    }
}

impl Host {
    fn validate(&self, name: &str) -> Result<()> {
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

    /// Where the console session gets its launch parameters. The password
    /// comes from `password_env`, else `ILOM_PASSWORD_<NAME>`, else
    /// `ILOM_PASSWORD`. A JNLP file needs none.
    pub fn source(&self, name: &str, env: Env) -> Result<Source> {
        if let Some(path) = &self.jnlp {
            return Ok(Source::Jnlp(path.clone()));
        }
        let host = self
            .host
            .clone()
            .ok_or_else(|| anyhow!("host {name:?} has no host or jnlp"))?;
        let per_host = format!("ILOM_PASSWORD_{}", env_suffix(name));
        let mut candidates = Vec::new();
        if let Some(variable) = &self.password_env {
            candidates.push(variable.clone());
        }
        candidates.push(per_host.clone());
        candidates.push("ILOM_PASSWORD".to_string());
        let password = candidates
            .iter()
            .find_map(|variable| env(variable).filter(|value| !value.is_empty()))
            .ok_or_else(|| {
                anyhow!(
                    "no ILOM password for host {name:?}: set {} (or ILOM_PASSWORD) in the \
                     environment or .env",
                    self.password_env.as_deref().unwrap_or(&per_host)
                )
            })?;
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
            rack1.source("rack1", &env).unwrap(),
            Source::Web { ref username, .. } if username == "root"
        ));
        assert!(matches!(
            file.hosts["rack2"].source("rack2", &env).unwrap(),
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
        let source = |pairs: &[(&str, &str)]| host.source("rack-1", &env_of(pairs));
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
        let error = host.source("rack-1", &env_of(&[])).unwrap_err().to_string();
        assert!(error.contains("ILOM_PASSWORD_RACK_1"), "{error}");
    }

    #[test]
    fn vnc_password_and_name_sanitising() {
        assert_eq!(env_suffix("rack-1.b"), "RACK_1_B");
        let host = Host {
            host: Some("h".into()),
            ..Host::default()
        };
        let both = [("ILOM_VNC_PASSWORD_A", "x"), ("ILOM_VNC_PASSWORD", "y")];
        assert_eq!(host.vnc_password("a", &env_of(&both)).as_deref(), Some("x"));
        assert_eq!(host.vnc_password("b", &env_of(&both)).as_deref(), Some("y"));
        assert_eq!(host.vnc_password("b", &env_of(&[])), None);
    }
}
