use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use ilom_kvm_core::{keymap::Layout, session::Source};
use ilom_mcp::{
    console::{Console, MediaPolicy},
    tools::IlomServer,
};
use ilom_vnc::hub::Hub;
use rmcp::{ServiceExt, transport::stdio};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// MCP server (stdio) for the Oracle ILOM Remote System Console.
///
/// Lets an MCP client such as Claude Code see the host screen and drive its
/// keyboard, mouse and virtual media. The ILOM password comes from
/// ILOM_PASSWORD (may be set in `.env`). Logs go to stderr: stdout carries
/// the protocol.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// ILOM address; logs into the web UI for every console session.
    #[arg(long, env = "ILOM_HOST", required_unless_present = "jnlp")]
    host: Option<String>,
    #[arg(long, env = "ILOM_USER", default_value = "root")]
    user: String,
    /// Downloaded `jnlpgenerator-*` file instead of a web login; wins over
    /// `--host` and ILOM_HOST. Its secret works once, so the session cannot
    /// be restarted.
    #[arg(long)]
    jnlp: Option<PathBuf>,
    /// Directory the `mount_media` tool may read images from. Without it,
    /// mounting is refused.
    #[arg(long, env = "ILOM_MCP_MEDIA_DIR")]
    media_dir: Option<PathBuf>,
    /// Let `mount_media` give the host write access to floppy images.
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

fn parse_layout(value: &str) -> Result<Layout, String> {
    Layout::from_id(value).ok_or_else(|| {
        let ids: Vec<_> = Layout::ALL.iter().map(|layout| layout.id()).collect();
        format!(
            "unknown layout {value:?}, expected one of {}",
            ids.join(", ")
        )
    })
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

    let source = match (&cli.jnlp, &cli.host) {
        (Some(path), _) => Source::Jnlp(path.clone()),
        (None, Some(host)) => {
            let password = std::env::var("ILOM_PASSWORD")
                .context("set ILOM_PASSWORD in the environment or .env")?;
            Source::Web {
                host: host.clone(),
                username: cli.user.clone(),
                password,
            }
        }
        (None, None) => bail!("pass --host (or set ILOM_HOST) or --jnlp"),
    };

    let hub = Hub::new(source, Duration::from_secs(cli.idle_timeout));
    let policy = MediaPolicy {
        dir: cli.media_dir,
        allow_writable: cli.allow_writable_media,
    };
    let console = Console::new(hub, cli.layout.unwrap_or_else(Layout::from_locale), policy);
    info!("MCP server ready on stdio");
    let service = IlomServer::new(console)
        .serve(stdio())
        .await
        .context("start the MCP server")?;
    service.waiting().await?;
    Ok(())
}
