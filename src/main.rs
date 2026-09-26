use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ilom_kvm::{
    codec::AspeedCodec,
    hid::HidSession,
    jnlp::{self, ConsoleArgs},
    video::{VideoEvent, VideoSession},
    tls::CertPolicy,
    tokend::Tokend,
    web::WebSession,
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
struct Target {
    /// Path to a `jnlpgenerator-*` file downloaded from the ILOM web UI.
    #[arg(long, conflicts_with = "host")]
    jnlp: Option<PathBuf>,
    /// ILOM address; logs into the web UI to fetch a fresh JNLP.
    #[arg(long, env = "ILOM_HOST")]
    host: Option<String>,
    #[arg(long, env = "ILOM_USER", default_value = "root")]
    user: String,
}

impl Target {
    /// Returns launch arguments. A web session is only needed to mint the
    /// JNLP, so it is closed again straight away (ILOM has few web slots).
    fn resolve(&self) -> Result<ConsoleArgs> {
        if let Some(path) = &self.jnlp {
            return load_jnlp(path);
        }
        let host = self
            .host
            .as_deref()
            .context("pass --jnlp or --host (or set ILOM_HOST)")?;
        let password = std::env::var("ILOM_PASSWORD")
            .context("set ILOM_PASSWORD in the environment or .env")?;
        let web = WebSession::login(host, CertPolicy::Insecure, &self.user, &password)?;
        let args = web.console_args();
        if let Err(error) = web.logout() {
            warn!(%error, "ILOM web logout failed");
        }
        args
    }
}

#[derive(clap::Args)]
struct ProbeArgs {
    #[command(flatten)]
    target: Target,
    /// Number of decoded frames to save as PNG files.
    #[arg(long, default_value_t = 1)]
    frames: u32,
    #[arg(long, default_value = "captures")]
    output_dir: PathBuf,
    /// Also open the keyboard/mouse channel and send a harmless report
    /// (Shift press and release, pointer to the screen centre).
    #[arg(long)]
    hid: bool,
    /// With --hid: after the first frame type this USB usage, then erase it
    /// with Backspace before exiting (e.g. 0x04 for "a").
    #[arg(long, value_parser = parse_usage, requires = "hid")]
    type_usage: Option<u8>,
}

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
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

fn parse_usage(value: &str) -> Result<u8, String> {
    let digits = value.trim_start_matches("0x");
    u8::from_str_radix(digits, 16).map_err(|error| format!("invalid USB usage {value:?}: {error}"))
}

fn probe(args: ProbeArgs) -> Result<()> {
    let console = args.target.resolve()?;
    info!(host = %console.host, user = %console.username, depth = console.color_depth, "loaded JNLP");
    let policy = cert_policy(&console);
    let mut tokend = Tokend::connect(
        &console.host,
        policy,
        &console.username,
        &console.secret,
    )?;
    let mut video = VideoSession::connect(&console.host, policy, &console.username, &mut tokend)?;
    let mut hid = if args.hid {
        let mut hid = HidSession::connect(&console.host, &console.username, &mut tokend)?;
        let mut reader = hid.try_clone_stream()?;
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buffer = [0_u8; 256];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                tracing::debug!(bytes = %hex::encode(&buffer[..n]), "HID socket data");
            }
        });
        hid.send_keyboard(0x02, &[])?;
        hid.send_keyboard(0, &[])?;
        hid.send_absolute_mouse(0, 512, 384, 1024, 768)?;
        info!("sent HID test reports");
        Some(hid)
    } else {
        None
    };
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
                    if saved == 1
                        && let (Some(hid), Some(usage)) = (hid.as_mut(), args.type_usage)
                    {
                        // The host may still be enumerating the virtual USB
                        // keyboard right after the HID channel starts.
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        hid.send_keystroke(0, usage)?;
                        info!(usage, "typed test key");
                    }
                }
                event => info!(?event, "video event"),
            }
        }
        Ok(())
    })();
    if let Some(mut hid) = hid {
        if args.type_usage.is_some() {
            const BACKSPACE: u8 = 0x2a;
            hid.send_keystroke(0, BACKSPACE)?;
        }
        hid.close();
    }
    video.stop();
    tokend.close();
    result
}
