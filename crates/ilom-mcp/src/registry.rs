//! The configured ILOM hosts, one console each.

use std::{collections::BTreeMap, sync::Arc};

use crate::console::{Console, state_label};

pub struct Registry {
    consoles: BTreeMap<String, Arc<Console>>,
}

impl Registry {
    pub fn new(consoles: BTreeMap<String, Arc<Console>>) -> Self {
        Self { consoles }
    }

    pub fn names(&self) -> Vec<&str> {
        self.consoles.keys().map(String::as_str).collect()
    }

    /// The console called `host`; `None` picks the only one.
    pub fn resolve(&self, host: Option<&str>) -> Result<&Arc<Console>, String> {
        match host {
            Some(name) => self.consoles.get(name).ok_or_else(|| {
                format!(
                    "unknown host {name:?}; configured hosts: {}",
                    self.names().join(", ")
                )
            }),
            None => {
                let mut consoles = self.consoles.values();
                match (consoles.next(), consoles.next()) {
                    (Some(console), None) => Ok(console),
                    (None, _) => Err("no ILOM host is configured".into()),
                    _ => Err(format!(
                        "several hosts are configured, pass `host`: {}",
                        self.names().join(", ")
                    )),
                }
            }
        }
    }

    /// One line per host: its name and console state, without connecting.
    pub fn describe(&self) -> Vec<String> {
        self.consoles
            .iter()
            .map(|(name, console)| match console.peek() {
                Some((state, message)) => format!("{name}: {} ({message})", state_label(state)),
                None => format!("{name}: not connected (no session open)"),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ilom_kvm_core::{keymap::Layout, session::Source};
    use ilom_vnc::hub::Hub;

    use super::*;
    use crate::console::MediaPolicy;

    fn registry(names: &[&str]) -> Registry {
        let consoles = names
            .iter()
            .map(|name| {
                let hub = Hub::new(Source::Jnlp("/nonexistent".into()), Duration::ZERO);
                let console = Console::new(hub, Layout::Us, MediaPolicy::default());
                (name.to_string(), Arc::new(console))
            })
            .collect();
        Registry::new(consoles)
    }

    #[test]
    fn single_host_is_the_default() {
        let registry = registry(&["a"]);
        assert!(registry.resolve(None).is_ok());
        assert!(registry.resolve(Some("a")).is_ok());
    }

    #[test]
    fn several_hosts_need_a_name() {
        let registry = registry(&["a", "b"]);
        let error = registry.resolve(None).err().unwrap();
        assert!(error.contains("a, b"), "{error}");
        assert!(registry.resolve(Some("b")).is_ok());
    }

    #[test]
    fn unknown_host_lists_names() {
        let error = registry(&["a", "b"]).resolve(Some("c")).err().unwrap();
        assert!(error.contains("\"c\"") && error.contains("a, b"), "{error}");
        assert!(registry(&[]).resolve(None).is_err());
    }

    #[test]
    fn describe_does_not_connect() {
        let lines = registry(&["a", "b"]).describe();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("a: not connected"));
    }
}
