# ilom-kvm-rs

Java-free native client for the **Oracle ILOM Remote System Console**, the
AMI MegaRAC-based redirection service found on ILOM service processors with
an ASPEED AST2100-class video engine.

The vendor console is a Java Web Start application (`jnlpgenerator-16`). This
client speaks the same protocols directly:

| Channel | Port | Transport | Status |
|---|---|---|---|
| Web login + JNLP (`/cgi-bin/jnlpgenerator-16`) | 443 | HTTPS | working |
| Token daemon (one-time JNLP secret to redirection tokens) | 5556 | TLS, certificate pinned | working |
| Video (AST2100, RC4-protected) | 7578 | TCP | working |
| RC4 video key | 5555 | TLS, certificate pinned | working |
| Keyboard / mouse (IUSB, AES-128-CBC) | 5121 | TCP | handshake working, input delivery unverified |
| Virtual CD-ROM / floppy (IUSB SCSI) | 5120 / 5123 | TCP | working (image files) |

## Build

The ILOM only offers TLS 1.2 with RSA key exchange, so the client uses the
system OpenSSL through `native-tls` (rustls cannot talk to it).

```sh
cargo build --workspace --release   # needs openssl-devel (pkg-config: openssl)
cargo test --workspace
```

## Use

Credentials can come from the environment or a git-ignored `.env` file:

```sh
cp .env.example .env    # set ILOM_HOST, ILOM_USER, ILOM_PASSWORD
```

Interactive viewer (the default command):

```sh
cargo run --release                 # login form
cargo run --release -- viewer --auto          # connect with .env credentials
cargo run --release -- viewer --jnlp jnlpgenerator-16   # downloaded launch file
```

Click the framebuffer to capture the keyboard; the toolbar shows the capture
state and a **Release** button. Keys are sent by physical position, so the
host keyboard layout applies.

**Right Ctrl** (**Right Cmd** on macOS, whose laptops lack Right Ctrl) is the
client's host key by default and never reaches the host. Pick another one
(`right-ctrl`, `right-super`, `menu`, `scroll-lock` or `none`) with
`--host-key` or `ILOM_HOST_KEY`, or in the Keyboard menu (remembered). Tap it to capture or release the keyboard; hold it and press
**F** for fullscreen, **V** to paste text or **Del** for Ctrl+Alt+Del. In
fullscreen the toolbar hides; move the pointer to the top edge to show it.
The **Screenshot** menu copies the screen to the clipboard, opens the
screenshot folder, or saves a PNG file to an `ilom-kvm` folder in the Pictures
folder: the XDG pictures directory on Linux (`XDG_PICTURES_DIR` or
`user-dirs.dirs`, e.g. `~/Images` on a French desktop), `~/Pictures` on macOS
and `%USERPROFILE%\Pictures` on Windows. `--capture-dir` changes it.

The **100%** toggle shows host pixels 1:1 (scroll with the wheel or the
scroll bars when the screen is larger than the window); otherwise the screen
fits the window. Whole-number scales stay sharp.

The **Send keys** menu sends combinations the local system would capture:
Ctrl+Alt+Del, Ctrl+Alt+F1…F12, Alt+Tab, Super, Print Screen and Magic SysRq.
The **Keyboard** menu pastes clipboard text (typed with the chosen host
layout) and toggles the host lock keys.

When the SP or network drops, the viewer dims the last frame and reconnects
by itself (keep-alive notices a dead link in about 30 s). Attempts back off
from 5 s to 30 s; **Reconnect now** skips the wait. Only logins from the form
or `--auto` can reconnect: a downloaded JNLP works for one connection.

The **CD-ROM…** and **Floppy/USB…** toolbar menus redirect an ISO image or a
raw disk image to the host's virtual drives. Floppy images are read-only
unless "Allow the host to write" is ticked before mounting. Mounted images
come back after a reconnect. You can also drop a file on the window: `.iso`
goes to the CD-ROM, `.img`/`.ima`/`.bin` to the floppy/USB drive. A drop never
replaces an image that is already mounted.

Headless media redirection (keeps a video session open like the vendor
client; Ctrl-C to stop):

```sh
cargo run --release -- media --cdrom install.iso
cargo run --release -- media --floppy stick.img --writable
```

Diagnostics:

```sh
RUST_LOG=ilom_kvm_core=debug cargo run -- probe --frames 3     # saves PNGs to captures/
RUST_LOG=ilom_kvm_core=debug cargo run -- probe --jnlp file.jnlp
```

## VNC bridge

`ilom-vnc` serves the console as a local VNC (RFB 3.3–3.8) server, so any VNC
client can show it: RustConn, Remmina, TigerVNC or `remote-viewer`.

```sh
cargo run --release -p ilom-vnc              # ILOM_* from .env, listens on 127.0.0.1:5900
cargo run --release -p ilom-vnc -- --listen 127.0.0.1:5901 --layout fr
remote-viewer vnc://127.0.0.1:5900
```

In RustConn, add a **VNC** connection to `127.0.0.1`, port `5900`.

- One ILOM session is shared by all clients. It starts with the first client
  (the first picture takes about 10 s) and closes when no client has been
  connected for `--idle-timeout` seconds (default 30), which frees the ILOM's
  few session slots. Rejected credentials or a changed certificate stop the
  bridge from logging in again until it restarts.
- Keys: clients that send QEMU extended key events (gtk-vnc, TigerVNC) send
  physical key positions, as the egui viewer does. Other clients send
  characters, which are typed through the host layout (`--layout us|fr` or
  `ILOM_LAYOUT`, default from the locale). Characters then come out right
  whatever the client's own layout is.
- The host cursor is drawn into the picture. Clients that draw a local cursor
  show a small dot at the pointer.
- Encodings: ZRLE and Raw, with only changed tiles sent. DesktopSize follows
  host resolution changes.
- Not supported: clipboard (ignored), mouse wheel (the SP's reports have no
  wheel), virtual media (use the viewer or `ilom-kvm media`).

Security: the server listens on loopback only unless `ILOM_VNC_PASSWORD` is
set. With a password it offers VNC Authentication, which is weak: DES, and
only 8 characters count. For remote access, tunnel through SSH
(`ssh -L 5900:127.0.0.1:5900 host`) rather than listening on the network.

Files live in the config directory: `~/.config/ilom-kvm` on Linux,
`~/Library/Application Support/ilom-kvm` on macOS (an existing
`~/.config/ilom-kvm` is kept) and `%APPDATA%\ilom-kvm` on Windows.
`XDG_CONFIG_HOME` overrides it everywhere.

The GUI remembers the last host, username, host key, host layout and view
(fit or 100%) in `settings` (override with `ILOM_SETTINGS`); the password is
never stored.
Command-line options and environment variables win over saved values.

The first web login to an ILOM stores its certificate fingerprint in
`known_certs` (override with `ILOM_KNOWN_CERTS`). Later
logins refuse a different certificate before sending the password. After a
legitimate certificate change, run `ilom-kvm forget-cert <host>`.

A JNLP secret works **once**. With web credentials, every connection (and
every reconnect) mints a fresh JNLP and then closes the web session straight
away, because the ILOM has only a few web session slots.

## How it works

The repository is a Cargo workspace:

- `crates/ilom-kvm-core`: protocol and session library, with no GUI code.
- `crates/ilom-kvm`: the egui viewer and the `probe`, `media` and
  `forget-cert` commands.
- `crates/ilom-vnc`: the VNC bridge (`rfb.rs` wire format and encodings,
  `keys.rs` keysym and scancode mapping, `hub.rs` shared session, `client.rs`
  per-client connection).

Main modules of `ilom-kvm-core`:

- `web.rs`: minimal HTTP/1.0 client. The ILOM web server repeats status lines
  and mixes line endings, which strict HTTP stacks reject.
- `tokend.rs`: exchanges the JNLP user and secret for 20-byte redirection
  tokens and per-channel challenge data.
- `video.rs`: `REDIRECT` device-capabilities exchange, 7-byte IVTP framing,
  token authentication, challenge login, fragment reassembly and RC4.
- `codec.rs`: runs AspeedTech's MPL-2.0 decoder (WebAssembly, via `wasmi`).
- `hid.rs`: `HIDCMD` handshake, AES key derivation, IUSB input reports.
- `vmedia.rs`, `scsi.rs`: virtual media channels and the SCSI command set of
  the vendor image readers (CD-ROM and floppy/USB).
- `session.rs`: background session for front ends: decoded frames, host
  cursor, keyboard/mouse commands, media mounts and reconnects.

## Clean-room note and licences

The protocol was reimplemented from observation and from reading the vendor
client. No vendor code is included. Decompiled references stay local in the
git-ignored `reference/` directory.

`crates/ilom-kvm-core/third_party/aspeed_codec/decoder_wasm.wasm` is an
unmodified build from [AspeedTech-BMC/aspeed_codec](https://github.com/AspeedTech-BMC/aspeed_codec)
under MPL-2.0. The Rust code is MIT licensed.

Keep service processors on a management network. Never expose them to the
Internet.
