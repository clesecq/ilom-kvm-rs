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
cargo build --release   # needs openssl-devel (pkg-config: openssl)
cargo test
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
**Screenshot** saves PNG files to an `ilom-kvm` folder in the Pictures
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
come back after a reconnect.

Headless media redirection (keeps a video session open like the vendor
client; Ctrl-C to stop):

```sh
cargo run --release -- media --cdrom install.iso
cargo run --release -- media --floppy stick.img --writable
```

Diagnostics:

```sh
RUST_LOG=ilom_kvm=debug cargo run -- probe --frames 3          # saves PNGs to captures/
RUST_LOG=ilom_kvm=debug cargo run -- probe --jnlp file.jnlp
```

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

## Clean-room note and licences

The protocol was reimplemented from observation and from reading the vendor
client. No vendor code is included. Decompiled references stay local in the
git-ignored `reference/` directory.

`third_party/aspeed_codec/decoder_wasm.wasm` is an unmodified build from
[AspeedTech-BMC/aspeed_codec](https://github.com/AspeedTech-BMC/aspeed_codec)
under MPL-2.0. The Rust code is MIT licensed.

Keep service processors on a management network. Never expose them to the
Internet.
