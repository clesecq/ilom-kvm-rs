use std::{
    net::{TcpListener, ToSocketAddrs},
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use ilom_kvm_core::{keymap::Layout, session::Source};
use ilom_vnc::{
    client::{self, Options},
    hub::Hub,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Local VNC server for the Oracle ILOM Remote System Console.
///
/// Any VNC client connected to it shows the host console and drives its
/// keyboard and mouse. The ILOM password comes from ILOM_PASSWORD; the
/// optional VNC password from ILOM_VNC_PASSWORD (both may be set in `.env`).
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// ILOM address; logs into the web UI for every console session.
    #[arg(long, env = "ILOM_HOST", required_unless_present = "jnlp")]
    host: Option<String>,
    #[arg(long, env = "ILOM_USER", default_value = "root")]
    user: String,
    /// Downloaded `jnlpgenerator-*` file instead of a web login. Its secret
    /// works once, so the session cannot be restarted.
    #[arg(long, conflicts_with = "host")]
    jnlp: Option<PathBuf>,
    /// Address to listen on. Other addresses than loopback need
    /// ILOM_VNC_PASSWORD.
    #[arg(long, default_value = "127.0.0.1:5900")]
    listen: String,
    /// Keyboard layout configured on the host, used for clients that send
    /// characters instead of key positions [default: from the locale].
    #[arg(long, env = "ILOM_LAYOUT", value_parser = parse_layout)]
    layout: Option<Layout>,
    /// Seconds to keep the ILOM session after the last client left.
    #[arg(long, default_value_t = 30)]
    idle_timeout: u64,
}

fn parse_layout(value: &str) -> Result<Layout, String> {
    Layout::from_id(value).ok_or_else(|| {
        let ids: Vec<_> = Layout::ALL.iter().map(|layout| layout.id()).collect();
        format!(
            "unknown layout {value:?}, expected one of {}",
            ids.join(", ")
        )
    })
}

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .without_time()
        .init();
    let cli = Cli::parse();

    let (source, name) = match (&cli.jnlp, &cli.host) {
        (Some(path), _) => (Source::Jnlp(path.clone()), "ILOM console".to_string()),
        (None, Some(host)) => {
            let password = std::env::var("ILOM_PASSWORD")
                .context("set ILOM_PASSWORD in the environment or .env")?;
            let source = Source::Web {
                host: host.clone(),
                username: cli.user.clone(),
                password,
            };
            (source, format!("ILOM {host}"))
        }
        (None, None) => bail!("pass --host (or set ILOM_HOST) or --jnlp"),
    };
    let password = std::env::var("ILOM_VNC_PASSWORD")
        .ok()
        .filter(|password| !password.is_empty());
    if password.as_ref().is_some_and(|password| password.len() > 8) {
        warn!("VNC Authentication only uses the first 8 characters of ILOM_VNC_PASSWORD");
    }

    let addresses: Vec<_> = cli
        .listen
        .to_socket_addrs()
        .with_context(|| format!("resolve {}", cli.listen))?
        .collect();
    if password.is_none() && addresses.iter().any(|address| !address.ip().is_loopback()) {
        bail!(
            "{} is reachable from other machines: set ILOM_VNC_PASSWORD, or listen on 127.0.0.1",
            cli.listen
        );
    }
    let listener = TcpListener::bind(addresses.as_slice())
        .with_context(|| format!("listen on {}", cli.listen))?;
    info!(
        address = %listener.local_addr()?,
        authentication = password.is_some(),
        "VNC server ready"
    );

    let hub = Hub::new(source, Duration::from_secs(cli.idle_timeout));
    let options = Arc::new(Options {
        password: password.map(String::into_bytes),
        layout: cli.layout.unwrap_or_else(Layout::from_locale),
        name,
    });
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                warn!(%error, "accept failed");
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map_or_else(|_| "?".into(), |peer| peer.to_string());
        info!(%peer, "VNC client connected");
        let hub = hub.clone();
        let options = options.clone();
        let spawned = thread::Builder::new()
            .name("ilom-vnc-client".into())
            .spawn(move || match client::serve(stream, &hub, &options) {
                Ok(()) => info!(%peer, "VNC client disconnected"),
                Err(error) => warn!(%peer, error = %format!("{error:#}"), "VNC client dropped"),
            });
        if let Err(error) = spawned {
            warn!(%error, "could not start a client thread");
        }
    }
    Ok(())
}
