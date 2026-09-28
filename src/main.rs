use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ilom_kvm::{
    codec::AspeedCodec,
    config,
    gui::{HostKey, IlomApp, Startup},
    hid::HidSession,
    jnlp::{self, ConsoleArgs},
    keymap::Layout,
    known_certs::KnownCerts,
    scsi::{MediaImage, MediaKind},
    settings::Settings,
    tls::CertPolicy,
    tokend::Tokend,
    video::{VideoEvent, VideoSession},
    viewer::Source,
    vmedia::MediaChannel,
    web,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    version,
    about = "Java-free client for the Oracle ILOM Remote System Console"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the interactive console window (default).
    Viewer(ViewerArgs),
    /// Connect, capture frames and run protocol diagnostics.
    Probe(ProbeArgs),
    /// Redirect disk images to the host's virtual CD-ROM and floppy/USB
    /// drives until the SP closes the channels (Ctrl-C to stop).
    Media(MediaArgs),
    /// Forget the pinned certificate of an ILOM (after a legitimate change).
    ForgetCert {
        /// ILOM address as used for login.
        host: String,
    },
}

#[derive(clap::Args, Default)]
struct ViewerArgs {
    /// Connect straight away with a downloaded JNLP file.
    #[arg(long)]
    jnlp: Option<PathBuf>,
    /// ILOM address prefilled in the login form [default: last one used].
    #[arg(long, env = "ILOM_HOST")]
    host: Option<String>,
    /// [default: last one used, else root]
    #[arg(long, env = "ILOM_USER")]
    user: Option<String>,
    /// Connect straight away using ILOM_HOST/ILOM_USER/ILOM_PASSWORD.
    #[arg(long)]
    auto: bool,
    /// Screenshot folder [default: ilom-kvm in the Pictures folder].
    #[arg(long)]
    capture_dir: Option<PathBuf>,
    /// Client key never sent to the host: tap to capture or release the
    /// keyboard, hold for shortcuts (F fullscreen, V paste, Del Ctrl+Alt+Del)
    /// [default: last one chosen, else right-ctrl (right-super on macOS)].
    #[arg(long, env = "ILOM_HOST_KEY", value_enum)]
    host_key: Option<HostKey>,
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
        web::fetch_console_args(host, &self.user, &password)
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

#[derive(clap::Args)]
struct MediaArgs {
    #[command(flatten)]
    target: Target,
    /// ISO 9660 image for the virtual CD-ROM.
    #[arg(long, required_unless_present = "floppy")]
    cdrom: Option<PathBuf>,
    /// Raw disk image (floppy or USB stick) for the virtual floppy drive.
    #[arg(long)]
    floppy: Option<PathBuf>,
    /// Let the host write to the floppy image.
    #[arg(long, requires = "floppy")]
    writable: bool,
    /// Skip the video session the vendor client always runs alongside.
    #[arg(long)]
    no_video: bool,
}

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .without_time()
        .init();

    match Cli::parse().command {
        Some(Command::Probe(args)) => probe(args),
        Some(Command::Viewer(args)) => viewer(args),
        Some(Command::Media(args)) => media(args),
        Some(Command::ForgetCert { host }) => forget_cert(&host),
        None => viewer(ViewerArgs {
            user: std::env::var("ILOM_USER").ok(),
            host: std::env::var("ILOM_HOST").ok(),
            host_key: std::env::var("ILOM_HOST_KEY")
                .ok()
                .and_then(|value| clap::ValueEnum::from_str(&value, true).ok()),
            ..Default::default()
        }),
    }
}

fn viewer(args: ViewerArgs) -> Result<()> {
    let password = std::env::var("ILOM_PASSWORD").ok();
    // Options and environment win over saved settings, which win over defaults.
    let settings_path = Settings::default_path()
        .inspect_err(|error| warn!(error = %format!("{error:#}"), "settings disabled"))
        .ok();
    let settings = settings_path
        .as_ref()
        .map(|path| {
            Settings::load(path).unwrap_or_else(|error| {
                warn!(error = %format!("{error:#}"), "ignoring unreadable settings");
                Settings::default()
            })
        })
        .unwrap_or_default();
    let host = args.host.or_else(|| settings.host.clone());
    let username = args
        .user
        .or_else(|| settings.username.clone())
        .unwrap_or_else(|| "root".into());
    let host_key = args
        .host_key
        .or_else(|| {
            let saved = settings.host_key.as_deref()?;
            clap::ValueEnum::from_str(saved, true).ok()
        })
        .unwrap_or_default();
    let layout = settings
        .layout
        .as_deref()
        .and_then(Layout::from_id)
        .unwrap_or_else(Layout::from_locale);
    let initial = match (&args.jnlp, args.auto) {
        (Some(path), _) => Some(Source::Jnlp(path.clone())),
        (None, true) => Some(Source::Web {
            host: host.clone().context("--auto needs ILOM_HOST")?,
            username: username.clone(),
            password: password.clone().context("--auto needs ILOM_PASSWORD")?,
        }),
        (None, false) => None,
    };
    let startup = Startup {
        host,
        username,
        password,
        capture_dir: args.capture_dir.unwrap_or_else(config::screenshot_dir),
        host_key,
        layout,
        settings,
        settings_path,
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("ILOM Remote Console")
            .with_inner_size([1100.0, 850.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ILOM Remote Console",
        options,
        Box::new(move |cc| Ok(Box::new(IlomApp::new(startup, initial, &cc.egui_ctx)))),
    )
    .map_err(|error| anyhow::anyhow!("GUI failed: {error}"))
}

fn forget_cert(host: &str) -> Result<()> {
    let mut known = KnownCerts::load_default()?;
    if known.remove(host)? {
        println!(
            "forgot the certificate of {host} ({})",
            known.path().display()
        );
    } else {
        println!(
            "no pinned certificate for {host} in {}",
            known.path().display()
        );
    }
    Ok(())
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

fn is_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
    })
}

fn parse_usage(value: &str) -> Result<u8, String> {
    let digits = value.trim_start_matches("0x");
    u8::from_str_radix(digits, 16).map_err(|error| format!("invalid USB usage {value:?}: {error}"))
}

fn probe(args: ProbeArgs) -> Result<()> {
    let console = args.target.resolve()?;
    info!(host = %console.host, user = %console.username, depth = console.color_depth, "loaded JNLP");
    let policy = cert_policy(&console);
    let mut tokend = Tokend::connect(&console.host, policy, &console.username, &console.secret)?;
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
        hid.nudge_leds()?;
        hid.send_absolute_mouse(0, 512, 384, 1024, 768)?;
        info!("sent HID test reports");
        Some(hid)
    } else {
        None
    };
    std::fs::create_dir_all(&args.output_dir)?;
    // Frames only arrive when the host screen changes; do not wait forever.
    video.set_read_timeout(Some(std::time::Duration::from_secs(8)))?;
    let mut codec = AspeedCodec::new()?;
    let mut saved = 0;
    let result = (|| -> Result<()> {
        while saved < args.frames {
            let event = match video.next_event() {
                Ok(event) => event,
                Err(error) if is_timeout(&error) => {
                    info!("no screen change for 8 s, stopping");
                    break;
                }
                Err(error) => return Err(error),
            };
            match event {
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

fn media(args: MediaArgs) -> Result<()> {
    let mut images = Vec::new();
    for (kind, path) in [
        (MediaKind::Cdrom, &args.cdrom),
        (MediaKind::Floppy, &args.floppy),
    ] {
        if let Some(path) = path {
            let image = MediaImage::open(path, kind, args.writable)?;
            info!(
                kind = kind.label(),
                path = %path.display(),
                blocks = image.blocks(),
                writable = image.writable(),
                "opened image"
            );
            images.push(image);
        }
    }

    let console = args.target.resolve()?;
    info!(host = %console.host, user = %console.username, "loaded JNLP");
    let policy = cert_policy(&console);
    let mut tokend = Tokend::connect(&console.host, policy, &console.username, &console.secret)?;
    let video = if args.no_video {
        None
    } else {
        let mut video =
            VideoSession::connect(&console.host, policy, &console.username, &mut tokend)?;
        video.set_read_timeout(None)?;
        let stream = video.try_clone_stream()?;
        // Frames are not needed; keep reading so the SP does not stall.
        std::thread::spawn(move || {
            while let Ok(event) = video.next_event() {
                tracing::trace!(?event, "video event ignored");
            }
        });
        Some(stream)
    };

    let mut workers = Vec::new();
    for mut image in images {
        let kind = image.kind();
        let token = tokend.redirection_token()?;
        let mut channel = MediaChannel::connect(&console.host, kind, &token)?;
        workers.push(std::thread::spawn(move || -> Result<()> {
            let mut last_report = std::time::Instant::now();
            channel.serve(&mut image, |stats| {
                if last_report.elapsed() >= std::time::Duration::from_secs(10) {
                    last_report = std::time::Instant::now();
                    info!(
                        kind = kind.label(),
                        commands = stats.commands,
                        read_mib = stats.bytes_read / (1 << 20),
                        written_kib = stats.bytes_written / 1024,
                        "media activity"
                    );
                }
            })
        }));
    }
    info!("media redirected; press Ctrl-C to stop");

    let mut result = Ok(());
    for worker in workers {
        match worker.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => result = Err(error),
            Err(_) => result = Err(anyhow::anyhow!("media thread panicked")),
        }
    }
    if let Some(stream) = video {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    tokend.close();
    result
}
