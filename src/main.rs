use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ilom_kvm::{
    codec::AspeedCodec,
    jnlp::{self, ConsoleArgs},
    video::{VideoEvent, VideoSession},
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
    /// Number of decoded frames to save as PNG files.
    #[arg(long, default_value_t = 1)]
    frames: u32,
    #[arg(long, default_value = "captures")]
    output_dir: PathBuf,
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
    let mut video = VideoSession::connect(&console.host, &console.username, &mut tokend)?;
    std::fs::create_dir_all(&args.output_dir)?;
    let mut codec = AspeedCodec::new()?;
    let mut saved = 0;
    let result = (|| -> Result<()> {
        while saved < args.frames {
            match video.next_event()? {
                VideoEvent::Frame(frame) => {
                    let header = &frame.header;
                    info!(
                        number = header.frame_number,
                        width = header.source_width,
                        height = header.source_height,
                        bytes = frame.data.len(),
                        mode = header.compression_mode,
                        rc4 = header.rc4_enabled,
                        "frame"
                    );
                    let rgba = codec.decode(&frame)?;
                    let path = args.output_dir.join(format!("frame-{saved:03}.png"));
                    image::save_buffer(
                        &path,
                        &rgba,
                        header.source_width.into(),
                        header.source_height.into(),
                        image::ColorType::Rgba8,
                    )?;
                    info!(path = %path.display(), "saved");
                    saved += 1;
                }
                event => info!(?event, "video event"),
            }
        }
        Ok(())
    })();
    video.stop();
    tokend.close();
    result
}
