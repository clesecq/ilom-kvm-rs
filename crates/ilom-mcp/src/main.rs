use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use clap::Parser;
use ilom_kvm_core::{
    hosts::{DEFAULT_NAME, Host, HostsFile, parse_layout},
    keymap::Layout,
    keyring::OsKeyring,
};
use ilom_mcp::{
    console::{Console, MediaPolicy},
    registry::Registry,
    tools::IlomServer,
};
use ilom_vnc::hub::Hub;
use rmcp::{ServiceExt, transport::stdio};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// MCP server (stdio) for the Oracle ILOM Remote System Console.
///
/// Lets an MCP client such as Claude Code see the host screen and drive its
/// keyboard, mouse and virtual media. Several ILOMs can be named in a hosts
/// file (`hosts.toml` in the config directory, or --hosts); without one,
/// --host or --jnlp defines a single host called `default`. The ILOM
/// password comes from ILOM_PASSWORD or ILOM_PASSWORD_<NAME> (may be set in
/// `.env`). Logs go to stderr: stdout carries the protocol.
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
    /// Directory the `mount_media` tool may read images from, for hosts
    /// without their own `media_dir`. Without one, mounting is refused.
    #[arg(long, env = "ILOM_MCP_MEDIA_DIR")]
    media_dir: Option<PathBuf>,
    /// Let `mount_media` give hosts write access to floppy images, unless
    /// their `allow_writable_media` says otherwise.
    #[arg(long, env = "ILOM_MCP_ALLOW_WRITABLE_MEDIA")]
    allow_writable_media: bool,
    /// Keyboard layout configured on the host, used by `type_text`
    /// [default: from the locale].
    #[arg(long, env = "ILOM_LAYOUT", value_parser = parse_layout)]
    layout: Option<Layout>,
    /// Seconds to keep the ILOM session after the last tool call. Mounted
    /// media keeps it open regardless.
    #[arg(long, default_value_t = 60)]
    idle_timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .init();
    let cli = Cli::parse();

    let hosts = if cli.host.is_some() || cli.jnlp.is_some() {
        let host = Host {
            host: cli.host.clone().filter(|_| cli.jnlp.is_none()),
            jnlp: cli.jnlp.clone(),
            user: Some(cli.user.clone()),
            ..Host::default()
        };
        BTreeMap::from([(DEFAULT_NAME.to_string(), host)])
    } else {
        let path = match &cli.hosts {
            Some(path) => path.clone(),
            None => HostsFile::default_path()?,
        };
        let file = HostsFile::load(&path)?;
        if file.hosts.is_empty() {
            bail!(
                "no ILOM host: pass --host (or set ILOM_HOST) or --jnlp, or describe hosts in {}",
                path.display()
            );
        }
        file.hosts
    };

    let env = |key: &str| std::env::var(key).ok();
    let keyring = OsKeyring::default();
    let mut consoles = BTreeMap::new();
    for (name, host) in &hosts {
        let source = host.source(name, &env, &keyring)?;
        let hub = Hub::new(source, Duration::from_secs(cli.idle_timeout));
        let policy = MediaPolicy {
            dir: host.media_dir.clone().or_else(|| cli.media_dir.clone()),
            allow_writable: host
                .allow_writable_media
                .unwrap_or(cli.allow_writable_media),
        };
        let layout = host
            .layout()
            .or(cli.layout)
            .unwrap_or_else(Layout::from_locale);
        consoles.insert(name.clone(), Arc::new(Console::new(hub, layout, policy)));
    }
    info!(hosts = %hosts.keys().cloned().collect::<Vec<_>>().join(", "), "configured");
    info!("MCP server ready on stdio");
    let service = IlomServer::new(Registry::new(consoles))
        .serve(stdio())
        .await
        .context("start the MCP server")?;
    service.waiting().await?;
    Ok(())
}
