use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ilom_kvm::{
    jnlp::{self, ConsoleArgs},
    tls::CertPolicy,
    tokend::Tokend,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Java-free client for the Oracle ILOM Remote System Console")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect with a downloaded JNLP file and run protocol diagnostics.
    Probe(ProbeArgs),
}

#[derive(clap::Args)]
struct ProbeArgs {
    /// Path to a `jnlpgenerator-*` file downloaded from the ILOM web UI.
    #[arg(long)]
    jnlp: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .without_time()
        .init();

    match Cli::parse().command {
        Command::Probe(args) => probe(args),
    }
}

fn load_jnlp(path: &PathBuf) -> Result<ConsoleArgs> {
    let xml = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    jnlp::parse(&xml)
}

fn cert_policy(args: &ConsoleArgs) -> CertPolicy {
    match args.fingerprint {
        Some(fingerprint) => CertPolicy::Pinned(fingerprint),
        None => {
            warn!("JNLP has no certificate fingerprint; TLS peer is not verified");
            CertPolicy::Insecure
        }
    }
}

fn probe(args: ProbeArgs) -> Result<()> {
    let console = load_jnlp(&args.jnlp)?;
    info!(host = %console.host, user = %console.username, depth = console.color_depth, "loaded JNLP");
    let mut tokend = Tokend::connect(
        &console.host,
        cert_policy(&console),
        &console.username,
        &console.secret,
    )?;
    let token = tokend.redirection_token()?;
    info!(token_len = token.len(), "tokend issued redirection token");
    tokend.close();
    Ok(())
}
