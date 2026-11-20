use std::{
    collections::BTreeMap,
    net::{TcpListener, ToSocketAddrs},
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use ilom_kvm_core::{
    hosts::{DEFAULT_NAME, Host, HostsFile, env_suffix, parse_layout},
    keymap::Layout,
};
use ilom_vnc::{
    client::{self, Options},
    hub::Hub,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Local VNC server for the Oracle ILOM Remote System Console.
///
/// Any VNC client connected to it shows the host console and drives its
/// keyboard and mouse. Several ILOMs can be named in a hosts file
/// (`hosts.toml` in the config directory, or --hosts), each with its own
/// `listen` address; without one, --host or --jnlp defines a single host.
/// Passwords come from ILOM_PASSWORD / ILOM_PASSWORD_<NAME> and the optional
/// VNC password from ILOM_VNC_PASSWORD / ILOM_VNC_PASSWORD_<NAME> (all may be
/// set in `.env`).
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Hosts file [default: hosts.toml in the config directory]. Ignored when
    /// --host or --jnlp is given.
    #[arg(long, env = "ILOM_HOSTS")]
    hosts: Option<PathBuf>,
    /// ILOM address; logs into the web UI for every console session.
    #[arg(long, env = "ILOM_HOST")]
    host: Option<String>,
    #[arg(long, env = "ILOM_USER", default_value = "root")]
    user: String,
    /// Downloaded `jnlpgenerator-*` file instead of a web login; wins over
    /// `--host` and ILOM_HOST. Its secret works once, so the session cannot
    /// be restarted.
    #[arg(long)]
    jnlp: Option<PathBuf>,
    /// Address to listen on for the single --host/--jnlp host. Other
    /// addresses than loopback need ILOM_VNC_PASSWORD. Hosts from a file
    /// use their own `listen`.
    #[arg(long, default_value = "127.0.0.1:5900")]
    listen: String,
    /// Keyboard layout configured on the host, used for clients that send
    /// characters instead of key positions [default: from the locale]. Hosts
    /// from a file may set their own.
    #[arg(long, env = "ILOM_LAYOUT", value_parser = parse_layout)]
    layout: Option<Layout>,
    /// Seconds to keep the ILOM session after the last client left.
    #[arg(long, default_value_t = 30)]
    idle_timeout: u64,
}

/// A host with the address it is served on.
struct Entry {
    name: String,
    host: Host,
    listen: String,
}

/// The hosts to serve: the single one from `--host`/`--jnlp`, else the
/// hosts file. Every file host needs its own `listen`.
fn entries(cli: &Cli, file: impl FnOnce() -> Result<(PathBuf, HostsFile)>) -> Result<Vec<Entry>> {
    if cli.host.is_some() || cli.jnlp.is_some() {
        let host = Host {
            host: cli.host.clone().filter(|_| cli.jnlp.is_none()),
            jnlp: cli.jnlp.clone(),
            user: Some(cli.user.clone()),
            ..Host::default()
        };
        return Ok(vec![Entry {
            name: DEFAULT_NAME.into(),
            host,
            listen: cli.listen.clone(),
        }]);
    }
    let (path, file) = file()?;
    if file.hosts.is_empty() {
        bail!(
            "no ILOM host: pass --host (or set ILOM_HOST) or --jnlp, or describe hosts in {}",
            path.display()
        );
    }
    from_file(file)
}

fn from_file(file: HostsFile) -> Result<Vec<Entry>> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut entries = Vec::new();
    for (name, host) in file.hosts {
        let listen = host
            .listen
            .clone()
            .with_context(|| format!("host {name:?} needs a listen address for ilom-vnc"))?;
        if let Some(other) = seen.insert(listen.clone(), name.clone()) {
            bail!("hosts {other:?} and {name:?} both listen on {listen}");
        }
        entries.push(Entry { name, host, listen });
    }
    Ok(entries)
}

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .without_time()
        .init();
    let cli = Cli::parse();

    let entries = entries(&cli, || {
        let path = match &cli.hosts {
            Some(path) => path.clone(),
            None => HostsFile::default_path()?,
        };
        let file = HostsFile::load(&path)?;
        Ok((path, file))
    })?;

    let env = |key: &str| std::env::var(key).ok();
    let mut servers = Vec::new();
    for entry in entries {
        let Entry { name, host, listen } = entry;
        let source = host.source(&name, &env)?;
        let password = host.vnc_password(&name, &env);
        if password.as_ref().is_some_and(|password| password.len() > 8) {
            warn!(host = %name, "VNC Authentication only uses the first 8 characters of the VNC password");
        }
        let addresses: Vec<_> = listen
            .to_socket_addrs()
            .with_context(|| format!("resolve {listen} for host {name:?}"))?
            .collect();
        if password.is_none() && addresses.iter().any(|address| !address.ip().is_loopback()) {
            bail!(
                "{listen} (host {name:?}) is reachable from other machines: set \
                 ILOM_VNC_PASSWORD (or ILOM_VNC_PASSWORD_{}), or listen on 127.0.0.1",
                env_suffix(&name)
            );
        }
        let listener = TcpListener::bind(addresses.as_slice())
            .with_context(|| format!("listen on {listen} for host {name:?}"))?;
        info!(
            host = %name,
            address = %listener.local_addr()?,
            authentication = password.is_some(),
            "VNC server ready"
        );
        let hub = Hub::new(source, Duration::from_secs(cli.idle_timeout));
        let options = Arc::new(Options {
            password: password.map(String::into_bytes),
            layout: host
                .layout()
                .or(cli.layout)
                .unwrap_or_else(Layout::from_locale),
            name: host.title(&name),
        });
        servers.push((name, listener, hub, options));
    }

    let mut acceptors = Vec::new();
    for (name, listener, hub, options) in servers {
        let spawned = thread::Builder::new()
            .name(format!("ilom-vnc-{name}"))
            .spawn(move || serve_host(&name, &listener, &hub, &options));
        match spawned {
            Ok(handle) => acceptors.push(handle),
            Err(error) => bail!("could not start a listener thread: {error}"),
        }
    }
    for acceptor in acceptors {
        let _ = acceptor.join();
    }
    Ok(())
}

/// Accepts clients of one host until the listener fails for good.
fn serve_host(name: &str, listener: &TcpListener, hub: &Arc<Hub>, options: &Arc<Options>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                warn!(host = %name, %error, "accept failed");
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map_or_else(|_| "?".into(), |peer| peer.to_string());
        info!(host = %name, %peer, "VNC client connected");
        let hub = hub.clone();
        let options = options.clone();
        let host = name.to_string();
        let spawned = thread::Builder::new()
            .name("ilom-vnc-client".into())
            .spawn(move || match client::serve(stream, &hub, &options) {
                Ok(()) => info!(%host, %peer, "VNC client disconnected"),
                Err(error) => {
                    warn!(%host, %peer, error = %format!("{error:#}"), "VNC client dropped");
                }
            });
        if let Err(error) = spawned {
            warn!(host = %name, %error, "could not start a client thread");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_need_distinct_listen_addresses() {
        let ok = HostsFile::parse(
            "[hosts.a]\nhost = \"h\"\nlisten = \"127.0.0.1:5901\"\n\
             [hosts.b]\nhost = \"i\"\nlisten = \"127.0.0.1:5902\"\n",
        )
        .unwrap();
        assert_eq!(from_file(ok).unwrap().len(), 2);

        let missing = HostsFile::parse("[hosts.a]\nhost = \"h\"\n").unwrap();
        let error = from_file(missing).err().unwrap().to_string();
        assert!(error.contains("listen"), "{error}");

        let duplicate = HostsFile::parse(
            "[hosts.a]\nhost = \"h\"\nlisten = \"127.0.0.1:5901\"\n\
             [hosts.b]\nhost = \"i\"\nlisten = \"127.0.0.1:5901\"\n",
        )
        .unwrap();
        let error = from_file(duplicate).err().unwrap().to_string();
        assert!(error.contains("both listen"), "{error}");
    }
}
