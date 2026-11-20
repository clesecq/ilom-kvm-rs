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
cargo run --release -- viewer --name rack1 --auto       # host from hosts.toml
```

The login form has a host picker when `hosts.toml` (see
[Several ILOMs](#several-iloms)) lists hosts: picking one fills in its address,
user and keyboard layout (for that session only), and the last pick is
remembered. The password comes from the environment (`password_env`,
`ILOM_PASSWORD_<NAME>` or `ILOM_PASSWORD`), else from the OS keyring (see
[Several ILOMs](#several-iloms)); the form asks for it only when neither has
one. Tick **Save password to the OS keyring** and it is stored once the login
succeeds. A saved password the ILOM rejects is deleted so you can type a new
one. **Manage hosts…** (the Add and Edit dialog has a masked Password field with
**Save to the OS keyring**, a **Forget saved password** button, and an
**Advanced** section for the password variable; the password never goes into
`hosts.toml`, an empty field keeps the saved one, and a keyring failure only
warns after the host is saved; the list also has **Forget password**) adds, edits and removes
entries. Saving keeps comments and the `listen`, `media_dir` and
`allow_writable_media` keys that the viewer does not edit. **Custom** keeps the
manual address entry. `--name` preselects a host (and `--auto` connects to it);
`--jnlp` and `--host` win over it. Switch hosts with Disconnect, or in one step with the host combo in the
toolbar (shown when `hosts.toml` has hosts): the old session is closed first
(the ILOM has few session slots), then the chosen host connects with its
environment or keyring password; with none, the login form opens on that host
and asks for it. Hosts that use a JNLP file are listed but disabled, because
their launch file works once. With virtual media mounted, switching asks first.

The viewer remembers the last picture of the hosts it used recently (at most 8
hosts and 32 MB), only in memory and never on disk, and shows it dimmed while
a new session to that host starts, until the first live frame replaces it. A
rejected login or an edit of the host in Manage hosts… forgets its picture.

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

- With a hosts file, each host has its own `listen` address (required, and
  distinct), its own session and its own password; the loopback-or-password
  rule applies per listener.
- One ILOM session is shared by all clients of a host. It starts with the first client
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
never stored there (the optional keyring entry is separate).
Command-line options and environment variables win over saved values.

The first web login to an ILOM stores its certificate fingerprint in
`known_certs` (override with `ILOM_KNOWN_CERTS`). Later
logins refuse a different certificate before sending the password. After a
legitimate certificate change, run `ilom-kvm forget-cert <host>`.

A JNLP secret works **once**. With web credentials, every connection (and
every reconnect) mints a fresh JNLP and then closes the web session straight
away, because the ILOM has only a few web session slots.

## Several ILOMs

`ilom-mcp` and `ilom-vnc` can drive several ILOMs from one process, and the
viewer picks and edits the same entries. Name them
in `hosts.toml` in the config directory (`ILOM_HOSTS` or `--hosts <file>`
select another file):

```toml
[hosts.rack1]
host = "10.0.0.5"          # or jnlp = "/path/jnlpgenerator-file"
user = "root"              # default root
layout = "fr"              # optional host keyboard layout
listen = "127.0.0.1:5901"  # ilom-vnc only, required there, one per host
media_dir = "/srv/iso"     # ilom-mcp only, optional (see below)
allow_writable_media = false
password_env = "ILOM_PASSWORD_RACK1"   # optional

[hosts.rack2]
host = "10.0.0.6"
listen = "127.0.0.1:5902"
```

The ILOM password is read from `password_env` if set, else
`ILOM_PASSWORD_<NAME>` (the name in capitals, other characters turned into
`_`: `rack-1` gives `ILOM_PASSWORD_RACK_1`), else `ILOM_PASSWORD`. The VNC
password likewise comes from `ILOM_VNC_PASSWORD_<NAME>`, else
`ILOM_VNC_PASSWORD`. Passwords never go in the file; they can live in `.env`.

When no environment variable has the ILOM password, the OS keyring is tried:
an entry with service `ilom-kvm` and the host's name as user (Keychain on
macOS, Credential Manager on Windows, Secret Service on Linux, talking D-Bus
in pure Rust, so no system library is needed). The viewer saves it (see
[Use](#use)); `ilom-mcp`, `ilom-vnc` and `ilom-kvm --name` read it. A locked
or missing keyring is skipped after at most 3 s, with a warning, and the usual
"no password" error follows. Renaming a host in the viewer moves its entry;
deleting the host deletes it. `ILOM_VNC_PASSWORD` is not kept in the keyring.
The `probe` and `media` commands accept `--name` too.

Without a hosts file, `--host`/`ILOM_HOST` or `--jnlp` defines one host named
`default`, as before. When `--host` or `--jnlp` is given, the file is ignored.

## MCP server

`ilom-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
server on stdio, so an LLM agent can see the host console, type, click and
mount virtual media.

```sh
cargo build --release -p ilom-mcp
claude mcp add ilom --env ILOM_HOST=10.0.0.5 --env ILOM_PASSWORD=... -- \
  /path/to/target/release/ilom-mcp --layout fr --media-dir ~/isos
```

Other clients take the same command in their JSON configuration:

```json
{
  "mcpServers": {
    "ilom": {
      "command": "/path/to/ilom-mcp",
      "args": ["--layout", "fr", "--media-dir", "/home/me/isos"],
      "env": { "ILOM_HOST": "10.0.0.5", "ILOM_PASSWORD": "..." }
    }
  }
}
```

Options are those of `ilom-vnc` without `--listen` (and `--hosts` for several
ILOMs, see above): `--host`, `--user`,
`--jnlp` (wins over `--host` and `ILOM_HOST`), `--layout` and `--idle-timeout`
(default 60 s), with `ILOM_*` read from the environment or `.env`. Two more
control `mount_media`: `--media-dir` (`ILOM_MCP_MEDIA_DIR`) and
`--allow-writable-media` (`ILOM_MCP_ALLOW_WRITABLE_MEDIA`). Logs go to stderr,
because stdout carries the protocol.

Tools:

- `list_hosts`: the configured hosts and their console state, without opening
  a session. With several hosts, every other tool takes a `host` argument; it
  can be left out only when there is one host (the error lists the names).
- `screenshot`: the host screen as a PNG, with the connection state in the
  text. A reconnecting console shows its last picture, and the text then says
  it may be old. A host that sends no video gives a text answer instead.
- `type_text`: types text through the host layout.
- `send_keys`: key presses with modifiers, for example `ctrl`+`alt`+`delete`.
- `mouse`: `move`, `click` or `double` at pixel coordinates of the latest
  screenshot, with the left, right or middle button. Pass `expected_width` and
  `expected_height` (the screenshot size) to fail instead of clicking the wrong
  place after a resolution change. The result states the screen size.
- `status`: connection state, keyboard LEDs and the virtual media drives.
- `mount_media` and `unmount_media`: attach an ISO (`cdrom`) or a raw image
  (`floppy`) from the host's `media_dir`, else `--media-dir`. Mounting is refused without that option,
  for paths outside it (symlinks are resolved first), and for `writable`
  unless the host's `allow_writable_media` (else `--allow-writable-media`)
  is true.

Limits: the first call takes about 10 s (it opens the console session). The
session closes after `--idle-timeout` seconds without calls, except while
media is mounted. Input tools run one at a time. A `type_text` that times out
is cancelled, but keys already queued cannot be recalled. The mouse wheel is not supported (the SP's reports have
none), nor is relative mouse mode. There is no power control and no text
recognition: the agent reads the screenshots itself.

Security: the server runs with your ILOM credentials and gives the agent full
control of the host console, including login prompts. `mount_media` lets the
agent expose any file below `--media-dir` to the host, so keep that directory
to images you are happy to share, and enable writable media only if needed. Register it only in clients and projects you
trust, and keep the ILOM credentials out of shared configuration files.

## How it works

The repository is a Cargo workspace:

- `crates/ilom-kvm-core`: protocol and session library, with no GUI code
  (`hosts.rs` reads the named hosts file for the headless front ends).
- `crates/ilom-kvm`: the egui viewer and the `probe`, `media` and
  `forget-cert` commands.
- `crates/ilom-vnc`: the VNC bridge (`rfb.rs` wire format and encodings,
  `keys.rs` keysym and scancode mapping, `hub.rs` shared session, `client.rs`
  per-client connection).
- `crates/ilom-mcp`: the MCP server (`console.rs` blocking session operations
  on the VNC bridge's `Hub`, `tools.rs` the tools, `keys.rs` key names to USB
  usages, `registry.rs` the named hosts). It is the only crate that uses
  `tokio`.

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
