# Oracle ILOM Remote System Console protocol

This document describes the wire protocols used by the Oracle ILOM "Remote
System Console" (the Java Web Start client `JavaRConsole.jar` +
`RedirLib.jar`, an AMI MegaRAC G3/AST2100 design). It was written while
building `ilom-kvm-rs`, from observing a live service processor and from
reading the vendor client. No vendor code is reproduced here.

Verified against: ILOM web server `Oracle-ILOM-Web-Server/1.0`, video server
ID 4 (AST2100), server version 4.

All multi-byte integers are **little-endian** unless stated otherwise.

## Contents

1. [Overview](#1-overview)
2. [Web login and JNLP](#2-web-login-and-jnlp)
3. [TLS and certificate pinning](#3-tls-and-certificate-pinning)
4. [Token daemon (tokend)](#4-token-daemon-tokend)
5. [Video channel](#5-video-channel)
6. [RC4 video key service](#6-rc4-video-key-service)
7. [Video frame format](#7-video-frame-format)
8. [Keyboard and mouse channel](#8-keyboard-and-mouse-channel)
9. [Password hashing primitives](#9-password-hashing-primitives)
10. [Virtual media channels](#10-virtual-media-channels)
11. [Quirks and pitfalls](#11-quirks-and-pitfalls)

## 1. Overview

| Service | Port | Transport | Purpose |
|---|---|---|---|
| Web UI | 443 | HTTPS | Login, JNLP generation |
| tokend | 5556 | TLS | Trades the JNLP secret for redirection tokens |
| RC4 key service | 5555 | TLS | Hands out the dynamic video RC4 key |
| Video | 7578 | TCP (plain) | Screen redirection (7579 = serial console, colour depth 0) |
| Keyboard/mouse | 5121 | TCP (plain) | USB HID redirection |
| Virtual CD-ROM | 5120 | TCP (plain) | Image redirection |
| Virtual floppy | 5123 | TCP (plain) | Image redirection |

The redirection channels themselves are not encrypted with TLS. They are
authenticated with short-lived tokens from tokend. Keyboard/mouse reports
are encrypted (AES) and video frames are RC4-encrypted.

Connection order used by the vendor client:

1. Log into the web UI and download the JNLP.
2. Open tokend and authenticate with the JNLP user and secret. Keep this
   connection open for the whole session: every channel asks it for a fresh
   token.
3. Start video (handshake, token, login, RC4 key).
4. Start keyboard/mouse (needs video to be active first).
5. Start virtual media on demand.

## 2. Web login and JNLP

The web server emits non-standard HTTP (see [§11](#11-quirks-and-pitfalls));
a tolerant HTTP/1.0 client is required.

### 2.1 Login

1. `GET /iPages/i_login.asp`
   - Response sets cookie `ORA_ILOM_LOGIN=<value>`.
   - The page contains `setElementValue("loginToken", "<token>")`; extract
     `<token>` (30 characters observed).
2. `POST /iPages/loginProcessor.asp`
   - Cookies: `ORA_ILOM_LOGIN=<value>; ilom=1`
   - Body (`application/x-www-form-urlencoded`):
     `sclink=&loginToken=<token>&username=<user>&password=<password>&button=Log+In`
   - Success: sets `ORA_ILOM_SESSION_SP=<session>` and the body redirects to
     `/iPages/suntab.asp`. `ORA_ILOM_LOGIN` is expired.
   - Failure: body contains `location.href='/iPages/i_login.asp?msg=<n>'`.
     `msg=5` was observed when too many web sessions were open.
3. Logout: `GET /logout.asp` with the session cookie (returns 200).

ILOM has few web session slots. Always log out, ideally right after the
JNLP has been downloaded; the console session does not depend on the web
session.

### 2.2 JNLP

`GET /cgi-bin/jnlpgenerator-16` (colour console) or
`/cgi-bin/jnlpgenerator-serial` (serial console), with the session cookie.
The browser UI reaches it through `/iPages/i_redopt.asp` →
`/iPages/i_redlaunch.asp`, but the direct request works.

Positional `<argument>` values in `<application-desc>`:

| # | Example | Meaning |
|---|---|---|
| 0 | `16` | Colour depth (`0` = serial console on port 7579) |
| 1 | `192.0.2.1` | SP address |
| 2 | `root-sp-5` | Per-launch session user |
| 3 | `iVXDdkDMTlbZVZf` | One-time secret for tokend |
| 4 | `40:c9:…:ff:b0` | SHA-256 fingerprint of the SP certificate (optional) |
| 5 | `-----BEGIN CERTIFICATE-----…` | SP certificate in PEM (optional) |

A trailing `CMM` argument marks chassis (blade) mode. Multi-blade JNLPs
repeat the per-session group.

**The secret is single-use.** Each launch needs a newly generated JNLP.

If arguments 4 and 5 are missing, nothing is left to pin the TLS channels
against. A client can refuse to connect or connect without verification.
`ilom-kvm-rs` connects without verification and logs a warning.

## 3. TLS and certificate pinning

- The SP offers TLS 1.2 with `AES128-GCM-SHA256` (RSA key exchange). TLS
  stacks that dropped RSA key exchange (for example rustls) cannot connect;
  OpenSSL works.
- The certificate is self-signed and typically expired, so normal
  validation fails. The vendor client trusts it by comparing its SHA-256
  fingerprint (over the DER encoding) with JNLP argument 4. The web server
  on port 443 presents the same certificate.
- The web login happens before any JNLP exists, so on the first visit the
  port 443 certificate cannot be pinned. `ilom-kvm-rs` trusts it on first use
  and stores its fingerprint (`~/.config/ilom-kvm/known_certs`). Later logins
  pin to the stored fingerprint and refuse a different certificate before the
  password is sent. It also compares the web fingerprint with the JNLP
  fingerprint; a mismatch only logs a warning, and the console channels are
  still pinned to the JNLP fingerprint.

## 4. Token daemon (tokend)

TLS on port 5556 (same certificate and pinning as
[§3](#3-tls-and-certificate-pinning)). Raw byte protocol: no framing, no
length prefixes, no command IDs in replies. Each request has a reply of a
fixed size that the client must know.

### 4.1 Authenticate

Sent once, right after the TLS handshake.

```
client → username[16]   JNLP argument 2, zero padded / truncated
client → password[32]   JNLP argument 3 (one-time secret), zero padded / truncated
server → result[1]
```

| `result` | Meaning |
|---|---|
| `0x01` | Authenticated. The connection now accepts token requests. |
| `0x02` | Authentication failed. The server is expected to close the connection. |

- Fields are raw bytes (ASCII in practice) with no terminator. A value of
  exactly 16 or 32 bytes fills its field completely.
- The vendor client treats **any value other than `0x02`** as success.
  `ilom-kvm-rs` accepts only `0x01`. No other value has been seen.
- A secret that was already used gets `0x02` (observed). Each launch needs a
  new JNLP ([§2.2](#22-jnlp)).
- The vendor client opens one tokend TLS connection, then opens a *second*
  one immediately before sending the username and authenticates on the
  second. The first is never used or closed. One connection is enough.

### 4.2 Get redirection token

```
client → 0x01
server → token[20]
```

- The token is 20 opaque bytes. There is no status byte and no error reply:
  a failure shows up only as a closed or stalled connection.
- **Each channel gets its own token.** The vendor client requests a new one
  for video token authentication ([§5.4](#54-handshake)), the RC4 key
  service ([§6](#6-rc4-video-key-service)), keyboard/mouse, virtual CD-ROM
  and virtual floppy. Token reuse and the lifetime of an unused token have
  not been tested *(uncertain)*.
- Without an authenticated tokend session, the vendor token call returns a
  single zero byte instead of 20 bytes (fallback for older servers). A new
  client can ignore this.

### 4.3 Get challenge data

Used to derive keys for encrypted channels: keyboard/mouse
([§8](#8-keyboard-and-mouse-channel)) and the encrypted serial console.

```
client → 0x02
client → session_handle[4]
server → challenge_data[32]
```

| Field | Size | Notes |
|---|---|---|
| command | 1 | `0x02` |
| session_handle | 4 | The **first 4 bytes** of the 32-byte challenge from the channel's own "encryption challenge" reply, sent as-is (no byte-order conversion). |
| challenge_data | 32 | Opaque. Becomes the IV/key material for the channel cipher. |

No status byte here either.

### 4.4 Lifetime and keep-alive

- No keep-alive or ping command. Keep the connection open for the whole
  console session and send requests on demand.
- The vendor client closes tokend when video redirection stops.
  `ilom-kvm-rs` closes it (TLS shutdown) when the session ends.
- The server idle timeout is unknown *(uncertain)*. In live tests the
  connection stayed usable for the whole session; keyboard/mouse, started
  after the video handshake, still gets its token and challenge data.

### 4.5 Constants

| Name | Value |
|---|---|
| Port | 5556 |
| Username field | 16 bytes |
| Password field | 32 bytes |
| `GET_TOKEN` | `0x01` |
| `GET_CHALLENGE` | `0x02` |
| `AUTHENTICATED` / `AUTH_FAILED` | `0x01` / `0x02` |
| Token size | 20 |
| Session handle size | 4 |
| Challenge data size | 32 |

## 5. Video channel

Plain TCP on port 7578. Port 7579 is the serial console (colour depth 0); it
uses the legacy framing only and is not covered here. The first exchange uses
the legacy 24-byte `REDIRECT` header. After it, the AST2000/AST2100 video
server ("adviserd") switches to the 7-byte IVTP header for the rest of the
connection.

### 5.1 Legacy `REDIRECT` header (24 bytes)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 8 | signature | ASCII `REDIRECT` |
| 8 | 2 | header_len | `24` |
| 10 | 4 | data_len | Payload bytes after the header |
| 14 | 2 | command | See below |
| 16 | 2 | status | 0 = success |
| 18 | 1 | originator | 1 = client, 0 = server |
| 19 | 1 | video_server_id | Filled by the server |
| 20 | 1 | video_server_ver | Filled by the server |
| 21 | 3 | reserved | 0 |

If `header_len` is larger than 24, skip the extra bytes (`ilom-kvm-rs` does;
the vendor client assumes 24).

On an AST2100 ILOM only command `6` (device capabilities) uses this header.
The other legacy commands belong to the older G3 video daemon and the serial
console:

| Cmd | Name | Cmd | Name |
|---|---|---|---|
| 1 | challenge | 10 | palette info |
| 2 | login | 17 | pause redirection |
| 3 | start redirection | 18 | resume redirection |
| 4 | take screen | 19 | serial console output |
| 5 | stop redirection | 20 | serial keypress |
| 6 | get device capabilities | 21 / 22 | serial take write lock / write lock status |
| 7 | get port capabilities | 23 | token auth (legacy) |
| 8 | active clients | 24 | serial encryption challenge |

Legacy error statuses: `2058` authentication failed, `2059` invalid
redirection mode, `2060` connection limit reached, `2061` invalid
configuration.

### 5.2 IVTP header (7 bytes)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | type | Packet type ([§5.3](#53-packet-types)) |
| 1 | 4 | pkt_size | Announced payload length, u32 |
| 5 | 2 | status | 0 = success, 1 = host powered off, 2 = failure. Login replies may also carry `2058`. |

The payload follows directly.

**Do not trust `pkt_size` on server replies.** The vendor client uses it to
split the stream only for video fragments (type 5) and cursor packets
(type 48). For every other type it parses a fixed layout. Some SP replies
announce a length that includes the 7-byte header: the 20-byte token reply is
announced as `27` (observed). `ilom-kvm-rs` reads replies with these real
payload sizes:

| Type | Payload bytes actually sent |
|---|---|
| 1 challenge | 16 + salt_len + 32 (60 for server version ≠ 1) |
| 2 login | 0 |
| 15 blank screen | 0 |
| 33 mouse mode | 1 |
| 41 video engine configs | 8 |
| 51 active clients | 1 |
| 52 server info | 2 |
| 58 token auth | 20 |
| 5, 48, others | `pkt_size` |

Client requests carry the correct payload length, with one vendor exception
(type 40, see [§5.8](#58-video-engine-configuration)).

### 5.3 Packet types

C→S = client to server, S→C = server to client.

| Type | Name | Dir | Payload | Use |
|---|---|---|---|---|
| 1 | GET_CHALLENGE | both | [§5.4](#54-handshake) | Login challenge |
| 2 | LOGIN | both | C→S 32 bytes, S→C 0 | Challenge response; the reply status carries the result |
| 3 | INVALIDCMD | — | — | Defined, never handled |
| 4 | DUMMY_LOGIN | — | — | Defined, unused |
| 5 | VIDEO_FRAGMENT | S→C | u16 + data | Screen data ([§5.6](#56-video-fragments)) |
| 7–11 | SET_BANDWIDTH, SET_QUALITY_LEVEL, SET_CUSTOM_QUALITY, SET_FPS, ADJUST_COLOR_GAIN | — | — | Defined, unused |
| 12 | REFRESH_VIDEO_SCREEN | — | — | Defined, unused (behaviour untested) |
| 13 | PAUSE_REDIRECTION | C→S | empty | Pause frame updates |
| 14 | RESUME_REDIRECTION | C→S | empty | Resume frame updates |
| 15 | BLANK_SCREEN | S→C | empty | Host has no video signal; clear the display |
| 16, 17 | GET / DEFAULT_COLOR_GAIN | — | — | Defined, unused |
| 18–21 | SHIFT_IMAGE_LEFT / RIGHT / UP / DOWN | — | — | Defined, unused |
| 22 | AUTO_CALIBRATE | — | — | Defined, unused |
| 23 | GET_COLOR_GAIN_RESPONSE | — | — | Defined, unused |
| 24 | SET_COMPRESSION_TYPE | — | — | Defined, unused |
| 25 | STOP_SESSION_IMMEDIATE | C→S | empty | Never sent by the vendor client; `ilom-kvm-rs` sends it before closing |
| 26–30 | ENABLE / DISABLE_ENCRYPTION, ENCRYPTION_KEY, ENCRYPTION_STATUS, INITIAL_ENCRYPTION_STATUS | — | — | Defined, unused |
| 31, 32 | BW_DETECT_REQ / RES | — | — | Defined; the vendor parser does not decode type 32 |
| 33 | GET_USB_MOUSE_MODE | both | 1 byte | C→S mode `0` = request; S→C = current mode |
| 34–39 | VALIDATE_VIDEO / CDROM / FLOPPY_SESSION (+ response) | — | — | Defined, unused |
| 40 | SET_VIDEO_ENGINE_CONFIGS | C→S | 8 or 10 bytes | [§5.8](#58-video-engine-configuration) |
| 41 | GET_VIDEO_ENGINE_CONFIGS | both | C→S empty, S→C 8 bytes | [§5.8](#58-video-engine-configuration) |
| 48 | ADD_NEW_MOUSE_CURSOR | S→C | 57 (+ 8192) | Hardware cursor ([§5.7](#57-hardware-cursor)) |
| 49 | MAX_NUM_SESSION | — | — | Defined, never received; max sessions is signalled in device caps |
| 50 | SET_SCALAR_PARAM | C→S | 22 bytes | Server-side scaling ([§5.8](#58-video-engine-configuration)) |
| 51 | NUM_ACTIVE_CLIENTS | S→C | 1 byte | Number of viewers. Can arrive at any time, including as the login reply |
| 52 | SERVER_INFO | S→C | 2 bytes | `server_id u8, server_version u8`; unsolicited |
| 57 | HID_PKT | — | — | Defined, unused on this port |
| 58 | SR_TOKEN | both | 20 bytes | Token authentication |

The vendor client rejects any other type as a protocol error. `ilom-kvm-rs`
passes unknown types to the caller.

Server IDs (from server info, or the `REDIRECT` header on legacy servers):
`1` AMI G3, `2` Sun SP BMC, `3` AST2000, `4` AST2100. Only 4 is implemented
in `ilom-kvm-rs`.

### 5.4 Handshake

```
C→S  REDIRECT cmd 6 (device capabilities), data_len 4:
       ports u16 = 0, reserved[2] = {0x00, 0x02}
S→C  REDIRECT cmd 6, data_len 4: ports u16, reserved[2]
                         --- switch to IVTP framing ---
S→C  IVTP 52 SERVER_INFO   {server_id, server_version}   (unsolicited)
C→S  IVTP 58 SR_TOKEN      token[20]   (fresh token from tokend)
S→C  IVTP 58               status 0 = accepted (20-byte payload, unused)
C→S  IVTP 1  GET_CHALLENGE user[16], zero salt[salt_len], zero challenge[32]
S→C  IVTP 1                user[16], salt[salt_len], challenge[32]
C→S  IVTP 2  LOGIN         user[16], digest[16]
S→C  IVTP 2 (or 51)        status 0 = logged in
     (RC4 key fetched on port 5555 with another fresh token, §6)
C→S  IVTP 33 GET_USB_MOUSE_MODE  {0x00}
S→C  ... frames, mouse mode, active clients, cursor, blank screen ...
```

**Device capabilities.** The request's `reserved[1] = 2` is fixed by the
vendor client; it probably advertises the client's highest encryption level
*(uncertain)*. Reply:

| `reserved[0]` | Meaning | `reserved[1]` |
|---|---|---|
| `0` | Legacy G3 video daemon (next step would be port caps, cmd 7) | Serial encryption level |
| `1` | AST2000/AST2100 adviserd: switch to IVTP | High nibble = encryption level (unused for video). Low nibble `1` = **maximum number of sessions reached**: abort. |

A non-zero reply `status` is a hard failure. `ilom-kvm-rs` requires
`reserved[0] == 1`.

**Token authentication.** A non-zero status is a failure. After the token is
accepted, the vendor client replaces the login password with the literal
string `token`; the challenge login below is still required.

**Challenge.** `salt_len` is 8 when `server_version == 1`, otherwise 12. The
request payload is the username followed by zeros. Reply:

| Offset | Size | Field |
|---|---|---|
| 0 | 16 | username echo (NUL padded) |
| 16 | salt_len | salt: `$1$xxxxxxxx$` selects MD5-crypt; otherwise only the first 2 bytes are used as a DES salt. NUL salt bytes have been observed. |
| 16 + salt_len | 32 | challenge |

Reply status `2058` or `2` means login denied (for example a bad user). Any
other non-zero status is a protocol failure.

**Login.** `digest = MD5(crypt("token", salt) zero padded to 34 bytes ‖
challenge[32])`; see [§9](#9-password-hashing-primitives). Payload: username
(zero padded to 16 bytes) then the 16-byte digest, 32 bytes in total. The
vendor client accepts either a type-2 reply or a type-51 active-clients
packet as the answer. A non-zero status (`2058` = bad credentials) is a login
failure.

**Mouse mode.** Send type 33 with `pkt_size 1` and mode byte `0`. The server
replies (and may later send unsolicited updates) with a 1-byte mode. The
value mapping is shared with the HID channel
([§8](#8-keyboard-and-mouse-channel)) *(not verified on the video port)*.

The session is now active. Frames arrive only when the host screen changes,
so long silent periods are normal.

### 5.5 Session control

| Action | Packet |
|---|---|
| Pause | Type 13, `pkt_size 0` |
| Resume | Type 14, `pkt_size 0` |
| Stop | The vendor client sends **nothing** on AST2x00: it clears its framebuffer and RC4 state, then closes the socket (and tokend). `ilom-kvm-rs` sends type 25 with an empty payload, then shuts the socket down. The server accepts both. |
| Request engine config | Type 41, `pkt_size 0` |

Refusals: max sessions is `reserved[1] & 0x0f == 1` in device caps. A token
or login refusal is a non-zero IVTP status. The legacy `2060` is used only by
G3 servers.

Liveness: the vendor client enables TCP keep-alive and waits up to 120 s per
read. On timeout it probes the SP by opening (and immediately closing) a new
TCP connection to the same port. `ilom-kvm-rs` uses 30 s I/O timeouts during
the handshake and no read timeout afterwards. It enables TCP keep-alive
(15 s idle, 5 s interval, 3 probes), so a dead SP or network ends the read
within about 30 s and the viewer reconnects.

### 5.6 Video fragments

Type 5 payload:

| Offset | Size | Field |
|---|---|---|
| 0 | 2 | frag_num, u16. Bit 15 (`0x8000`) = last fragment. Bits 0–14 = index, restarting at 0 for each frame. |
| 2 | `pkt_size − 2` | fragment data |

`pkt_size` is reliable for this type. Reassembly: when
`frag_num & 0x7fff == 0`, start a new buffer; append each fragment's data;
when bit 15 is set the buffer holds one complete frame:

```
+0    39 bytes  video header ("ASP-2000" signature at +19)
+39   86 bytes  AST image header (§7)
+125  ...       compressed stream, RC4-encrypted when the header says so (§6.3)
```

39-byte video header:

| Offset | Size | Field |
|---|---|---|
| 0 | 19 | reserved |
| 19 | 8 | signature `ASP-2000` |
| 27 | 2 | header_len (39) |
| 29 | 4 | data_len |
| 33 | 2 | command |
| 35 | 2 | status |
| 37 | 2 | reserved |

Both clients check only the signature. The vendor client shows a dialog and
drops the frame if it is wrong.

The vendor reassembly code appears to handle correctly only frames sent as a
single fragment (`frag_num = 0x8000`) *(reading of decompiled code,
uncertain)*. `ilom-kvm-rs` concatenates in general and has unit tests for
multi-fragment frames. The fragment patterns the SP really sends have not
been logged *(uncertain)*.

Resolution changes are detected by comparing each frame's source and
destination sizes with the previous frame. No separate packet announces them.

### 5.7 Hardware cursor

Type 48 payload (`pkt_size` = 57, or 57 + 8192 when a new shape is
included):

| Offset | Size | Field |
|---|---|---|
| 0 | 41 | reserved |
| 41 | 4 | cursor_type: 1 = alpha-blended, otherwise AND/XOR |
| 45 | 4 | checksum: 0 = position-only update, keep the previous shape |
| 49 | 2 | x |
| 51 | 2 | y |
| 53 | 2 | x hotspot offset |
| 55 | 2 | y hotspot offset |
| 57 | 8192 | 64×64 pattern, u16 per pixel (present when `pkt_size > 57`) |

Pixel format: bits 8–11 R, 4–7 G, 0–3 B (4 bits each). If
`cursor_type == 1`, bits 12–15 are alpha (0–15). Otherwise bit 15 = AND and
bit 14 = XOR: AND 0 paints the colour; AND 1 with XOR 1 inverts the screen
pixel. The vendor client draws the cursor into the framebuffer.
`ilom-kvm-rs` does not parse type 48 yet (passed through as an "other"
event).

### 5.8 Video engine configuration

AST2000 engine configs (type 41 reply and type 40 request, 8 bytes):

| Offset | Field |
|---|---|
| 0 | differential_setting |
| 1 | dct_quant_quality |
| 2 | dct_quant_tbl_select |
| 3 | sharp_mode_selection |
| 4 | sharp_quant_quality |
| 5 | sharp_quant_tbl_select |
| 6 | compression_mode (a received 1 is read as 2 by the vendor client) |
| 7 | disable_hw_cursor |

The AST2100 variant appends `rc4_enable` (offset 8) and `rc4_reset`
(offset 9), 10 bytes in total.

**Compression-mode echo (vendor only).** On every received packet the vendor
client compares the compression mode of the last decoded frame (image header
offset 42) with the last mode it saw. On a change it sends type 40 with the
AST2100 layout: `differential 1, dct_quality 16, dct_tbl 7, sharp_mode 1,
sharp_quality 16, sharp_tbl 7, compression_mode = new mode,
disable_hw_cursor 1, rc4_enable / rc4_reset = the frame's flags`. It does the
same when the user picks a mode from the menu (0 = YUV420, 1 = YUV444,
2 = VQ 2-colour, 3 = VQ 4-colour). **The header announces `pkt_size 8` but 10
bytes are sent.** `ilom-kvm-rs` never sends type 40; frames decode without it
(observed).

Scaling (type 50, `pkt_size 22`): `filt1..filt4 u32, scalar_method u16 (0),
dest_x u16, dest_y u16`. The vendor client computes the filter words from the
source/destination width ratio. Optional; `ilom-kvm-rs` does not send it.

### 5.9 Differences between `ilom-kvm-rs` and the vendor client

- **Order.** The vendor client opens the video TCP connection first, then
  tokend. `ilom-kvm-rs` authenticates to tokend first. Both work.
- **Framing.** `ilom-kvm-rs` reads replies by fixed payload size per type
  ([§5.2](#52-ivtp-header-7-bytes)); the vendor client parses fixed layouts
  from a stream buffer.
- **Salt length.** The vendor client builds its challenge request with the
  real server version but parses the reply with a default version of 0, so it
  always assumes 12 salt bytes. `ilom-kvm-rs` uses the real version in both
  directions. No difference on the tested server (version 4).
- **RC4 key failure.** `ilom-kvm-rs` logs a warning and continues if fetching
  the RC4 key fails, and fails only when an encrypted frame arrives. The
  vendor client aborts the session.
- **Stop.** `ilom-kvm-rs` sends type 25; the vendor client only closes the
  socket.

## 6. RC4 video key service

TLS on port 5555, same certificate and pinning as tokend. Provides the
per-session key used to decrypt AST2100 video frames. The vendor client
always fetches it for server ID 4, after the video login and before the
mouse-mode request.

### 6.1 Exchange

Use a new TLS connection for each key:

```
client → token[20]      fresh token from tokend (not the one used for video auth)
server → status[1]      0x00 = OK; anything else = refused
client → 0x01           ack
server → key[16]
client → 0x01           ack
(client closes)
```

On a non-zero status the vendor client reports "server is not ready" and
aborts the video session.

### 6.2 Key handling

- The key is 16 bytes. The vendor client converts it to a Java `String` and
  back, which is a UTF-8 round trip: an invalid UTF-8 sequence would become
  `EF BF BD` and change the key's length and content. `ilom-kvm-rs`
  reproduces this with a lossy UTF-8 conversion and logs a warning if the
  length changes. Keys seen so far are plain ASCII.
- The vendor key schedule adds key bytes as *signed* values, so a non-ASCII
  key would likely crash it (negative index): another sign that real keys are
  ASCII *(inference)*.
- The key is repeated cyclically to 256 bytes before the key schedule. This
  equals standard RC4 keyed with the 16 bytes. `ilom-kvm-rs` uses plain RC4,
  checked against the RFC 6229 test vectors.
- The decoder contains a hard-coded default key `fedcba9876543210`, used only
  by a dead code path. Do not use it.

### 6.3 How frames are encrypted

- Flags are in the AST image header ([§7](#7-video-frame-format)):
  `rc4_enable` at image header offset 53 (frame offset 92), `rc4_reset` at
  offset 54 (frame offset 93).
- **Encrypted range:** everything after the 39-byte video header and the
  86-byte image header (frame offset 125) up to the end of the reassembled
  frame, including any bytes past `compressed_size`. The headers are clear.
- **Keystream continuity:** one RC4 state for the whole session. The
  keystream is **not** restarted per frame; it advances over every encrypted
  byte received.
- **Reset:** when a frame has `rc4_enable = 1` and `rc4_reset = 1`,
  re-initialise RC4 from the key before decrypting that frame.
- **Plain frames:** frames with `rc4_enable = 0` are not decrypted and do not
  advance the keystream.
- **Truncation:** decrypt the whole remainder first, then cut it to
  `compressed_size` for the decoder. Cutting first desynchronises the
  keystream for later frames (found by observation).
- `ilom-kvm-rs` also initialises the cipher lazily if the first encrypted
  frame arrives without the reset flag; the vendor would decrypt from an unkeyed
  all-zero state ([§7.4](#74-rc4-encryption)). In
  practice the first encrypted frame carries reset.
- With the type-40 echo the vendor client can ask the server to enable,
  disable or reset RC4. `ilom-kvm-rs` keeps the server default (RC4 on).

## 7. Video frame format

A video frame arrives as one or more type-5 fragments
([§5.6](#56-video-fragments)). After the 2-byte fragment numbers are removed
and the fragments joined, the reassembled frame is:

| Offset | Size | Part |
|---|---|---|
| 0 | 39 | Video header (`ASP-2000` signature) |
| 39 | 86 | AST2100 image header |
| 125 | rest | Compressed bitstream, RC4-encrypted when the image header says so |

Fields are packed with no alignment, so some `u32` values sit at odd
offsets. The encoder is the ASPEED AST2100 video engine. Its output is
JPEG-like but is **not** a JFIF stream: no markers, no byte stuffing, no
restart intervals.

Verified live: 1024×768 frames, RC4 on, decoded with the bundled ASPEED
codec. The rest of this section comes from reading the vendor client unless
marked *observed*.

### 7.1 Video header (39 bytes)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 19 | reserved | Ignored by the vendor client; contents unknown |
| 19 | 8 | signature | ASCII `ASP-2000`. The only field the vendor checks; a mismatch rejects the frame |
| 27 | 2 | header length | Nominally 39; not checked *(value never verified)* |
| 29 | 4 | data length | Not used |
| 33 | 2 | command | Not used |
| 35 | 2 | status | Not used |
| 37 | 2 | reserved | |

### 7.2 AST2100 image header (86 bytes)

"Rel" is the offset inside the image header, "Abs" the offset inside the
reassembled frame (Rel + 39).

| Rel | Abs | Size | Field | Used by vendor decoder | Notes |
|---|---|---|---|---|---|
| 0 | 39 | 2 | engine version | no | |
| 2 | 41 | 2 | header length | no | |
| 4 | 43 | 2 | source width | **yes** | Capture resolution; sets framebuffer size and pixel stride |
| 6 | 45 | 2 | source height | **yes** | |
| 8 | 47 | 2 | source colour depth | no | |
| 10 | 49 | 2 | source refresh rate | no | |
| 12 | 51 | 1 | source mode index | no | |
| 13 | 52 | 2 | destination width | **yes** | Encoded (possibly down-scaled) size; sets the macroblock grid |
| 15 | 54 | 2 | destination height | **yes** | |
| 17 | 56 | 2 | destination colour depth | no | |
| 19 | 58 | 2 | destination refresh rate | no | |
| 21 | 60 | 1 | destination mode index | no | |
| 22 | 61 | 4 | start code | no | Value not observed |
| 26 | 65 | 4 | frame number | no | `ilom-kvm-rs` logs it |
| 30 | 69 | 2 | H size | no | |
| 32 | 71 | 2 | V size | no | |
| 34 | 73 | 8 | reserved | no | |
| 42 | 81 | 1 | compression mode | UI only | [§7.3](#73-compression-mode) |
| 43 | 82 | 1 | JPEG scale factor | **yes** | Scale for the primary quantisation tables |
| 44 | 83 | 1 | JPEG table selector | **yes** | 0–7, [§7.7](#77-jpeg-macroblocks) |
| 45 | 84 | 1 | JPEG YUV table mapping | **yes** | 1 = chroma uses the luma base table |
| 46 | 85 | 1 | sharp mode selection | read, unused | |
| 47 | 86 | 1 | advance table selector | **yes** | 0–7, for "advance" (sharp) JPEG blocks |
| 48 | 87 | 1 | advance scale factor | **yes** | |
| 49 | 88 | 4 | number of macroblocks | **yes** | Upper bound for the decode loop |
| 53 | 92 | 1 | RC4 enable | **yes** | 1 = bitstream encrypted |
| 54 | 93 | 1 | RC4 reset | **yes** | 1 = re-key RC4 before this frame |
| 55 | 94 | 1 | Mode420 | **yes** | 1 = YCbCr 4:2:0 (16×16 MB), 0 = 4:4:4 (8×8 MB) |
| 56 | 95 | 1 | down-scaling method | no | |
| 57 | 96 | 1 | differential setting | no | |
| 58 | 97 | 2 | analog differential threshold | no | |
| 60 | 99 | 2 | digital differential threshold | no | |
| 62 | 101 | 1 | external signal enable | no | |
| 63 | 102 | 1 | auto mode | no | |
| 64 | 103 | 1 | VQ mode | no | |
| 65 | 104 | 4 | source frame size | no | |
| 69 | 108 | 4 | compressed size | **yes** | Length of the meaningful bitstream |
| 73 | 112 | 4 | H debug | no | |
| 77 | 116 | 4 | V debug | no | |
| 81 | 120 | 1 | input signal | no | |
| 82 | 121 | 2 | cursor X | no | |
| 84 | 123 | 2 | cursor Y | no | |

The vendor image-header class declares a size constant of **84** but reads
86 bytes. 84 is the older AST2000 header size
([§7.12](#712-legacy-formats-and-colour-depth)), which lacks the Mode420 and
VQ-mode bytes.

`ilom-kvm-rs` reads only the source and destination sizes, frame number,
compression mode, both table selectors, YUV mapping, the RC4 flags, Mode420
and compressed size. It rejects a width or height of 0 or above 4096. The
vendor declares a maximum resolution of 1500 but never enforces it, and
allocates a 1600×1200 buffer by default.

### 7.3 Compression mode

The compression-mode byte is informational: the decoder takes the chroma
layout from **Mode420**. Values (vendor UI menu entries):

| Value | Mode |
|---|---|
| 0 | YUV 4:2:0 |
| 1 | YUV 4:4:4 |
| 2 | VQ, 2 colours |
| 3 | VQ, 4 colours |

On a change of this value the vendor client sends SET_VIDEO_ENGINE_CONFIGS
([§5.8](#58-video-engine-configuration)). `ilom-kvm-rs` never does.

### 7.4 RC4 encryption

See [§6.3](#63-how-frames-are-encrypted). In short: every byte from frame
offset 125 to the end of the reassembled frame is encrypted; the keystream
runs across frames and restarts only on RC4 reset = 1; decrypt the whole
remainder before truncating to the compressed size.

If the first encrypted frame arrives without the reset flag, the vendor
client runs RC4 from an all-zero, never-keyed state. `ilom-kvm-rs` keys the
cipher on the first encrypted frame instead. *(The SP has been observed to
set reset on the first frame.)*

### 7.5 Bitstream reader

- **Padding:** the compressed bytes are zero padded to a multiple of 4.
- **Word order:** the stream is read as **little-endian 32-bit words**.
- **Bit order:** bits are consumed **MSB-first within each word**, starting
  at bit 31. A plain byte-wise MSB-first reader gets this wrong: byte-swap
  each 4-byte group first.
- **Look-ahead:** the vendor keeps a 64-bit window made of two words and
  refills from word 2 onward. Near the end of the buffer it clamps instead of
  failing.
- **No alignment:** commands, Huffman codes and VQ bitmaps follow each other
  bit to bit.
- `ilom-kvm-rs` hands the codec exactly `compressed size` bytes followed by 8
  zero bytes.

### 7.6 Block commands

Each macroblock (MB) starts with a **4-bit command** from the top of the bit
window. Positioned variants carry an absolute position in the 16 bits after
the command, so a positioned header is **20 bits**:

| Bits | Width | Meaning |
|---|---|---|
| 31–28 | 4 | command |
| 27–20 | 8 | MB column (x) |
| 19–12 | 8 | MB row (y) |

Positions count MBs: 16 px in 4:2:0 mode, 8 px in 4:4:4 mode.

| Code | Name | Header bits | Block body |
|---|---|---|---|
| 0x0 | JPEG | 4 | JPEG MB, primary tables (QT 0/1) |
| 0x4 | JPEG, advance ("low"/sharp) | 4 | JPEG MB, advance tables (QT 2/3) |
| 0x5 | VQ, 1 colour | 4 | 1 VQ entry, no bitmap |
| 0x6 | VQ, 2 colours | 4 | 2 VQ entries + 64×1-bit bitmap |
| 0x7 | VQ, 4 colours | 4 | 4 VQ entries + 64×2-bit bitmap |
| 0x8 | JPEG at position | 20 | as 0x0 |
| 0x9 | **End of frame** | 4 (not consumed) | Decoding stops |
| 0xC | JPEG advance at position | 20 | as 0x4 |
| 0xD | VQ 1 colour at position | 20 | as 0x5 |
| 0xE | VQ 2 colours at position | 20 | as 0x6 |
| 0xF | VQ 4 colours at position | 20 | as 0x7 |
| 0x1–0x3, 0xA, 0xB | undefined | — | No bits consumed; the loop spins until the MB counter runs out, so these effectively end the frame |

Bit 3 of the code means "explicit position". Bit 2 means "VQ or advance".

Stream traversal:

- Decoding starts at MB (0,0).
- After each block the column moves one MB to the right.
- When the column reaches the MB column count, the decoder goes to column 0
  of the next row. The column count comes from the **destination** width,
  padded up to a whole MB.
- Past the last row, the decoder goes back to row 0.
- Positioned commands set the position before the block is decoded.
- Decoding stops at command 0x9, or after (number of macroblocks + 1)
  commands (the vendor loop runs one extra iteration). VQ commands count too.

### 7.7 JPEG macroblocks

| Mode | Blocks in order | Pixels covered |
|---|---|---|
| 4:4:4 (Mode420 = 0) | Y, Cb, Cr | 8×8 |
| 4:2:0 (Mode420 = 1) | Y0 (top-left), Y1 (top-right), Y2 (bottom-left), Y3 (bottom-right), Cb, Cr | 16×16 |

In 4:2:0, each chroma sample covers 2×2 luma pixels.

**Huffman coding:** each 8×8 block is baseline-JPEG Huffman coded.

- **DC:** a size category, then that many extra bits with the usual JPEG sign
  extension. The value is a difference from the previous DC of the same
  component.
- **AC:** run/size symbols; `0x00` = end of block, `0xF0` = run of 16 zeros.
  Standard zig-zag order.
- **DC predictors:** one each for Y, Cb and Cr, reset to 0 **only at the
  start of the frame**. They carry over across positioned jumps and VQ
  blocks.

**Huffman tables:** the four standard tables of ITU-T T.81 Annex K.3
(luminance DC/AC for Y, chrominance DC/AC for Cb/Cr). They are fixed and
never sent in the stream.

**Quantisation:** four tables are rebuilt for every frame.

| QT | Base table | Scale factor | Used by |
|---|---|---|---|
| 0 | luma[JPEG table selector] | JPEG scale factor | Y, commands 0x0/0x8 |
| 1 | chroma[JPEG table selector] (luma if YUV mapping = 1) | JPEG scale factor | Cb/Cr, commands 0x0/0x8 |
| 2 | luma[advance table selector] | advance scale factor | Y, commands 0x4/0xC |
| 3 | chroma[advance table selector] (luma if mapping = 1) | advance scale factor | Cb/Cr, commands 0x4/0xC |

- Each entry is `clamp(base × 16 / scale, 1, 255)`. Scale 16 (the vendor
  default) uses the base table unchanged.
- Base tables: 8 luma/chroma pairs of 64 bytes, in natural row-major order.
  The vendor names them 000, 014, 029, 043, 057, 071, 086 and 100 for
  selectors 0–7. Selector 7 is the finest, selector 0 the coarsest (about the
  Annex K tables × 1.25 for luma and × 1.85 for chroma). The values are data
  and are not repeated here; the MPL-2.0 ASPEED codec source has them.

**Inverse DCT and level shift:**

- Fast integer AAN inverse DCT of the IJG "ifast" kind. Multipliers with 8
  fractional bits: 277, 362, 473, 669 (1.082392200, 1.414213562,
  1.847759065, 2.613125930 × 256).
- The quantisation tables are pre-multiplied by the AAN factors (1.0,
  1.3870399, 1.306563, 1.1758755, 1.0, 0.78569496, 0.5411961, 0.27589938,
  row × column) and by 2¹⁶.
- The result is shifted right by 3, 128 is added, and the value is limited to
  0–255.

### 7.8 VQ macroblocks

VQ blocks use a **4-slot colour cache**. Each slot holds a 24-bit YCbCr
colour `Y<<16 | Cb<<8 | Cr`. The cache is reset at the start of every frame:

| Slot | Value | Colour |
|---|---|---|
| 0 | 0x008080 | black |
| 1 | 0xFF8080 | white |
| 2 | 0x808080 | mid grey |
| 3 | 0xC08080 | light grey |

The block has n entries (n = 1, 2 or 4):

| Bit (window) | Width | Meaning |
|---|---|---|
| 31 | 1 | update flag |
| 30–29 | 2 | cache slot index |
| 28–5 | 24 | new colour for that slot (only when flag = 1) |

An entry is therefore **3 bits** (no update) or **27 bits** (with update).

A bitmap of 64 values in row-major 8×8 order follows:

| Colours | Bits per pixel | Bitmap size |
|---|---|---|
| 1 | 0 | none (solid fill) |
| 2 | 1 | 64 bits |
| 4 | 2 | 128 bits |

Each bitmap value v selects entry v of *this block*; that entry names a
cache slot, whose colour is the pixel colour.

A VQ block is an 8×8 4:4:4 tile. The vendor decoder places it correctly only
when Mode420 = 0 *(VQ modes presumably always come with 4:4:4; not observed
live)*.

### 7.9 Colour conversion

BT.601 limited ("studio") range, 16.16 fixed point with rounding, each result
clamped to 0–255:

```
Y' = 1.164 × (Y − 16)
R  = Y' + 1.597656 × (Cr − 128)
G  = Y' − 0.390625 × (Cb − 128) − 0.8125 × (Cr − 128)
B  = Y' + 2.015625 × (Cb − 128)
```

The vendor also builds full-range JFIF tables but never uses them. The vendor
framebuffer is 24-bit **BGR** (byte 0 = B). The codec used by `ilom-kvm-rs`
outputs **RGBA** with opaque alpha.

### 7.10 Framebuffer and incremental updates

- **Frames are differential.** A frame carries only the MBs that changed.
  Every MB not coded keeps its previous pixels, so keep one framebuffer for
  the life of the stream. There is no "unchanged block" command: positioned
  commands are the only way to skip MBs.
- **Size and stride:** the buffer has the **source** resolution. The vendor
  computes the pixel stride from the source width padded up to a whole MB but
  allocates the image unpadded, so a width that is not a multiple of the MB
  size would shear the image. Common widths (640, 800, 1024, 1280) are
  multiples of 16.
- **600-line 4:2:0 modes:** the height pads to 608. The vendor writes only 8
  rows for MB row 37 to avoid overrunning the buffer.
- **Resolution change:** when any source or destination dimension changes,
  the vendor allocates a new black buffer.
- **Blank screen (type 15):** the vendor fills the buffer with black.
  Stopping redirection also clears it.
- **Hardware cursor (type 48):** the vendor draws the cursor into the same
  framebuffer, saving and restoring the pixels under it
  ([§5.7](#57-hardware-cursor)).

### 7.11 `ilom-kvm-rs` implementation status

- There is no native Rust decoder. `src/codec.rs` runs AspeedTech's MPL-2.0
  decoder (`third_party/aspeed_codec/decoder_wasm.wasm`) in `wasmi`.
- It passes the stream, an RGBA output buffer, the **source** size, Mode420,
  and both table selectors.
- It does not pass the scale factors, the YUV table mapping, the destination
  size or the MB count. The codec therefore assumes fixed values (probably
  scale 16 and mapping 0) *(uncertain: matches the vendor defaults and live
  frames decode, but non-default SP settings are untested)*.
- The output buffer persists across frames, so differential updates work. A
  new or larger buffer starts **opaque white**; a smaller resolution reuses
  the old buffer without clearing it.
- On blank screen the viewer shows a message and keeps the last image.
- `decode()` refuses a frame that still has RC4 enabled; the decryptor clears
  the flag after decrypting.
- Not implemented: cursor overlay, SET_VIDEO_ENGINE_CONFIGS, AST2000 (server
  ID 3 is rejected in the handshake), legacy RLE paths.

### 7.12 Legacy formats and colour depth

**AST2000 (server ID 3), not supported.** Same 39-byte video header, but an
**84-byte** image header: the AST2100 layout without the Mode420 byte
(rel 55) and the VQ-mode byte, so later fields shift down. From the vendor
decoder *(not verified)*:

- Always 4:2:0, 6 blocks per MB.
- **2-bit** commands: 1 = JPEG (QT 0/1), 0 = JPEG (QT 2/3), 3 = positioned
  (QT 0/1), 2 = positioned (QT 2/3).
- Positioned header: 16 bits, 7-bit x at bits 29–23, 7-bit y at bits 22–16.
- No VQ and no end-of-frame code; the loop ends by pixel count.
- RC4 keyed from the fixed string `fedcba9876543210` instead of the key
  service, applied to 32-bit words in a way that looks broken.

**8 vs 16 bpp.** The JNLP colour-depth argument (`16`, `8` or `0`) matters
only for the pre-ASPEED G3 RLE path (TakeScreen with 16- or 8-bpp line
decoders). For AST2000/AST2100 it only selects serial (0) versus video. The
AST2100 path always decodes to 24-bit colour; the colour-depth fields in the
image header are parsed but unused.

## 8. Keyboard and mouse channel

Port 5121, plain TCP (no TLS). The vendor client opens it only after video
redirection is up (see [§1](#1-overview)).

Two kinds of message share the socket. The first 8 bytes tell them apart:

| Signature (8 bytes) | Used for | Direction |
|---|---|---|
| `HIDCMD  ` (`48 49 44 43 4D 44 20 20`) | Control commands and their replies | both |
| `IUSB    ` (`49 55 53 42 20 20 20 20`) | Input reports (client → SP), LED/status reports (SP → client) | both |

These messages are not wrapped in IVTP. The vendor client reads replies
through 1024-byte buffers.

### 8.1 HIDCMD header (16 bytes)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 8 | signature | `"HIDCMD  "` |
| 8 | 2 | command | See [§8.2](#82-command-codes) |
| 10 | 2 | status | 0 = success. Requests send 0. |
| 12 | 4 | data_len | Length of the payload that follows |
| 16 | n | payload | |

The vendor parser **ignores the announced length**. It reads a fixed payload
size for each command, and only when `status == 0`. On an error status
nothing after the header is read. `ilom-kvm-rs` does the same.

### 8.2 Command codes

| Code | Name | Request payload | Reply payload (status 0) |
|---|---|---|---|
| 100 | START_REDIRECTION | 4 | 4 |
| 101 | (unused, rejected by the parser) | | |
| 102 | DEVCAPS | 8 | 8 |
| 103 | PORTCAPS | 8 | 8 |
| 104 | CHALLENGE (legacy login) | 16 + salt + 32 | 16 + salt + 32 |
| 105 | LOGIN (legacy login) | 16 + 16 | 0 |
| 107 | ENCRYPTION_CHALLENGE | 16 + salt + 32 | 16 + salt + 32 |
| 111 | MOUSE_MODE | 1 | 1 (status byte) |
| 112 | RESCMD_MOUSE_MODE | – | 1 (the vendor parser rejects it) |
| 115 | TOKEN | 16 + 20 = 36 | 0 |

`salt` is 8 bytes when the server version is 1, otherwise 12 (AST2100,
version 4: **12**). The username field is 16 bytes for server IDs 2/3/4 and
32 bytes for other IDs.

HID status codes (u16 in the header):

| Code | Hex | Meaning |
|---|---|---|
| 0 | 0x000 | success |
| 1281 | 0x501 | max connection limit reached |
| 1282 | 0x502 | server ports busy |
| 1283 / 1284 | 0x503 / 0x504 | device access allowed / denied |
| 1285 | 0x505 | no device found |
| 1286 | 0x506 | invalid command |
| 1287 / 1288 | 0x507 / 0x508 | KBC output / input buffer full |
| 1289–1291 | 0x509–0x50B | POR init / keyboard no response / keyboard abs |
| 1292–1295 | 0x50C–0x50F | kbd low, mouse low, kbd high, mouse high |
| 1296 | 0x510 | LED changed |
| 1297 | 0x511 | mouse data error (the vendor client toggles absolute/relative mode) |
| 1298 | 0x512 | keyboard data error |
| 1299 | 0x513 | output buffer too small |
| 1300 | 0x514 | UART read |
| 1301 | 0x515 | authentication failure / login timed out |
| 1302 / 1303 | 0x516 / 0x517 | no session / invalid session |
| 1304 | 0x518 | try again |
| 1305 | 0x519 | device protocol |
| 1306 | 0x51A | invalid configuration |

### 8.3 Handshake

Every step is a request followed by a reply. A non-zero status aborts.

1. **TOKEN (115).** Payload `username[16]` (zero padded / truncated) then
   `token[20]`. The token is a fresh one from tokend
   ([§4](#4-token-daemon-tokend)); the username is the JNLP session user. The
   reply has no payload.
2. **DEVCAPS (102).** Request:

   | Offset | Size | Field | Value sent |
   |---|---|---|---|
   | 0 | 1 | HID protocol | 0 |
   | 1 | 4 | ports | 0 |
   | 5 | 3 | reserved | `[2, 0, 0]`; byte 5 = highest encryption level the client supports |

   The reply has the same layout. **Reply byte 5 is the encryption level**
   chosen by the SP: 0 = none, 1 = Blowfish ("pre-v3.2.4"), 2 = AES ("FIPS
   compatible").
3. **PORTCAPS (103).** Request `port u16, access_mode u8, clients u32,
   reserved u8`, all zero (8 bytes). Same reply layout; the client ignores
   it.
4. **CHALLENGE (104) and LOGIN (105), only without a tokend session.** Legacy
   path, never used with ILOM JNLP launches. Request
   `username[16] + zero salt + zero challenge[32]`; reply
   `username[16], salt, challenge[32]`. The client then sends LOGIN:
   `username[16]` then a 16-byte digest =
   `MD5(unix_hash(password, salt) padded to 129 bytes ‖ challenge)`
   ([§9](#9-password-hashing-primitives)). Server IDs other than 2/3/4 use
   the default salt of the negotiated level instead of the SP salt. Status
   1301 means the login timed out. *Not implemented in `ilom-kvm-rs`.*
5. **ENCRYPTION_CHALLENGE (107), only if the level is 1 or 2.** Request
   `username[16] + zeros[12] + zeros[32]` (60 bytes). Reply: `username[16]`
   (echo), `salt[12]` (unused), `challenge[32]`. Send the **first 4 bytes of
   the 32-byte challenge** to tokend as the session handle of its
   GET_CHALLENGE command ([§4.3](#43-get-challenge-data)). The 32 bytes of
   challenge data that tokend returns feed the key derivation
   ([§8.8](#88-encryption)). Without tokend, the challenge field itself is
   used.
6. **START_REDIRECTION (100).** Request `port u16 = 0, mode u8,
   reserved u8 = 0`, with mode 0 = absolute mouse, 1 = relative mouse. Same
   reply layout; **reply byte 2** is the mode the SP selected. The vendor
   client adopts it.

After this, the client may send **MOUSE_MODE (111)** with a 1-byte mode
(0 = absolute, 1 = relative) to switch modes. The vendor client does so when
it receives status 1297. It then starts a reader thread for status packets
([§8.7](#87-messages-from-the-sp)).

Right after the channel is up, the vendor UI sends NumLock press/release
**twice**. The net lock state is unchanged, but the host is prompted to
report its LED state.

`ilom-kvm-rs` implements steps 1–3, 5 and 6 over the tokend path only:

- The salt length is fixed at 12.
- Level 1 (Blowfish) fails the connection with an error.
- MOUSE_MODE is never sent, and status 1297 is not handled.
- Debug environment variables: `ILOM_HID_LEVEL` overrides the level byte
  offered in DEVCAPS. `ILOM_HID_KEY_USER=echo` derives the key from the
  username echoed in the 107 reply instead of the local one (the vendor uses
  the local one, and the default works).

### 8.4 IUSB header (32 bytes)

The same header is used by virtual media ([§10](#10-virtual-media-channels)).

| Offset | Size | Field | Client → SP | SP → client (LED capture) |
|---|---|---|---|---|
| 0 | 8 | signature | `"IUSB    "` | same |
| 8 | 1 | major | 1 | 1 |
| 9 | 1 | minor | 0 | 0 |
| 10 | 1 | header length | 0x20 | 0x20 |
| 11 | 1 | header checksum | see below | |
| 12 | 4 | data length | body bytes after the header | 2 |
| 16 | 1 | server caps | 0 | 0 |
| 17 | 1 | device type | 0x30 keyboard, 0x31 mouse (0x05 CD-ROM) | 0x30 |
| 18 | 1 | protocol | 0x10 keyboard, 0x20 mouse (0x01 CD-ROM) | 0x11 |
| 19 | 1 | direction | 0x80 (to device) | 0x00 |
| 20 | 1 | device number | 2 | 3 |
| 21 | 1 | interface | 0 keyboard, 1 mouse | 0 |
| 22 | 2 | client data | 0 | 0 |
| 24 | 4 | sequence number | **always 0** from the vendor client | 0x12 observed |
| 28 | 4 | reserved | `[0] = 1` if the body is encrypted, else 0 | 0 |
| 32 | n | body | | |

**Checksum:** write byte 11 as 0, sum all header bytes modulo 256, and set
byte 11 to the two's complement of the sum. The 32 header bytes then sum to
0 mod 256.

- The vendor client sums its *whole send buffer*. For encrypted packets that
  buffer is freshly zeroed, so the sum covers only the header. The test
  vector in [§8.9](#89-test-vector) matches this.
- For unencrypted packets the vendor buffer can hold stale bytes from earlier
  packets, so its checksum may be wrong. Whether the SP checks it is unknown.
  `ilom-kvm-rs` always uses the header-only sum.

### 8.5 Keyboard report

Body, 9 bytes in clear (device 0x30, protocol 0x10, interface 0):

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | report length = 8 (bytes after this one) |
| 1 | 1 | modifier bitmap |
| 2 | 1 | **auto-keybreak** flag: 1 = the SP releases the key by itself (the vendor calls it "reserved") |
| 3 | 6 | key usages (USB HID usage page 0x07) |

Modifier bits (standard USB boot keyboard): 0x01 L-Ctrl, 0x02 L-Shift,
0x04 L-Alt, 0x08 L-GUI, 0x10 R-Ctrl, 0x20 R-Shift, 0x40 R-Alt/AltGr,
0x80 R-GUI.

How the vendor client sends keys (auto-keybreak is on by default):

- It fills only **key slot 0**.
- **Modifier key down/up** (Shift, Ctrl, Alt, Windows, AltGraph): one report
  with the updated modifier byte, no key, keybreak 0.
- **Other key down:** one report with the current modifiers,
  `keys[0] = usage` and keybreak 1. Nothing is sent on release.
- **NumLock / CapsLock / ScrollLock release:** an empty report (all keys 0,
  keybreak 0).
- With auto-keybreak off, a release sends an empty report.
- On non-Windows clients, dead keys and some locale "shifted" keys are sent
  on *release*, followed by an extra empty report.
- **Ctrl+Alt+Del:** L-Ctrl down, L-Alt down, Delete down/up, L-Alt up,
  L-Ctrl up.

`ilom-kvm-rs`:

- `send_keystroke` produces the vendor form (one usage, keybreak 1). The LED
  nudge uses it: NumLock usage 0x53 with keybreak, then an empty report, done
  twice.
- The GUI uses `send_keyboard` instead. It sends the **full state** (held
  modifiers plus up to 6 held usages, keybreak 0) and an explicit empty
  report on release. The SP accepts this (observed).
- Paste sends modifiers alone, then modifiers + key, then an empty report,
  with a **12 ms delay** between reports. Reports sent back to back get
  dropped (observed).
- On focus loss the GUI releases every key.

### 8.6 Mouse reports

Device 0x31, protocol 0x20, interface 1. Buttons: bit 0 = left,
bit 1 = right, bit 2 = middle. **There is no wheel field.**

**Absolute** (13 bytes in clear):

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 1 | report length | 12 |
| 1 | 1 | buttons | |
| 2 | 1 | dx (i8) | 0 |
| 3 | 1 | dy (i8) | 0 |
| 4 | 2 | X | 0…32767 |
| 6 | 2 | Y | 0…32767 |
| 8 | 2 | resolution X | 1024 (constant) |
| 10 | 2 | resolution Y | 768 (constant) |
| 12 | 1 | "is valid data" | 0 |

Vendor scaling: `X = floor(x * 32767 / W)`, `Y = floor(y * 32767 / H)`, with
x and y in remote framebuffer pixels.

- On AST2100, W and H are the video engine's *source* mode resolution. G3
  servers use 1024×768.
- The vendor adds 0.5 *after* the integer division, so no rounding happens.
- The resolution fields are always 1024/768, whatever the real mode.

`ilom-kvm-rs` uses the same integer formula, but passes `extent = width − 1`
and clamps the position. The last pixel therefore maps to exactly 32767
(vendor maximum: `(W−1)·32767/W`), a difference of under one pixel.

**Relative** (4 bytes in clear): `[3, buttons, dx i8, dy i8]`.

- The vendor client divides the pointer delta by **1.5** (truncating) and
  tries to split large moves into ±126 steps (buggy, see
  [§11](#11-quirks-and-pitfalls)).
- It keeps the local pointer centred with `java.awt.Robot`.

When the SP selects relative mode, the `ilom-kvm-rs` viewer locks and hides
the local cursor while the console is captured, and sends raw mouse motion
deltas without the vendor's 1.5 divisor. Large moves are split into ±127
steps and the fraction is carried to the next report. *(uncertain: never
seen live; the test SP always selects absolute mode.)*

### 8.7 Messages from the SP

- **IUSB status packet.** Body `[reserved, status]` (2 bytes). Body byte 1
  is the **host keyboard LED bitmap**: 0x01 NumLock, 0x02 CapsLock,
  0x04 ScrollLock. Never encrypted. Captured after a NumLock press:

  ```
  49555342 20202020 01 00 20 d4 02000000 00 30 11 00 03 00 0000 12000000 00000000 | 00 01
  ```

  Device 0x30, protocol 0x11, direction 0, device number 3, sequence 0x12,
  LED = NumLock.
- **HIDCMD with status 1297:** toggles absolute/relative mode (the vendor
  client then sends MOUSE_MODE).
- **HIDCMD mouse-mode reply:** 1-byte status payload.

`ilom-kvm-rs` treats body byte 1 of *any* IUSB packet as LEDs (no device
type filter), reads one payload byte only for HIDCMD 111/112 with status 0,
and refuses IUSB bodies larger than 4096 bytes. The GUI shows
NUM/CAPS/SCROLL indicators; the matching Keyboard menu entries send the lock
usage 0x53 / 0x39 / 0x47.

### 8.8 Encryption

The level is chosen in DEVCAPS ([§8.3](#83-handshake)). Key material comes
from the **session username** (not a password) and the 32-byte challenge
data from tokend.

| | Level 1 ("non-FIPS") | Level 2 ("FIPS compatible") |
|---|---|---|
| Cipher | Blowfish/ECB/PKCS5 | AES-128/CBC/PKCS5 (= PKCS#7) |
| Unix hash | MD5-crypt(username, `$1$A17c6z5w$`), 34 bytes | vendor SHA-512 "crypt"(username, `$6$I27m0w5z15um9P5f`), 64 raw bytes zero padded to **129** |
| Key | `MD5(hash34 ‖ challenge_data)`, 16 bytes | `SHA-512(hash129 ‖ challenge_data)[0..16]` |
| IV | none | `challenge_data[0..16]` |
| `ilom-kvm-rs` | **not implemented** | implemented |

The vendor SHA-512 "crypt" is one `SHA-512(username ‖ "$6$" ‖ salt[3..16])`:
only `I27m0w5z15um9` is used and `P5f` is dropped. It is **not** glibc
SHA-crypt ([§9.3](#93-sha512crypt-vendor)).

The level-2 class first loads a built-in IV `243f6a8885a308d313198a2e03707344`
(digits of π). The challenge data always overwrites it before use.

**What is encrypted:**

- The 32-byte header and body byte 0 (report length) stay in clear.
- All body bytes after byte 0 are encrypted: 8 for the keyboard, 12 for the
  absolute mouse, 3 for the relative mouse.
- Padding is PKCS#7 to 16 bytes, so every encrypted report has 1 + 16 =
  **17** body bytes.
- Set data length = 17 and reserved[0] = 1 in the header, then compute the
  checksum.
- **The cipher restarts for every packet.** CBC starts from the same IV each
  time, so identical reports give identical ciphertexts.
- After an encryption error, the vendor client drops encryption for the rest
  of the session.

### 8.9 Test vector

Generated with the vendor classes; `ilom-kvm-rs` matches it byte for byte
(`hid::tests::encrypted_keystroke_matches_vendor_client`). The key
derivation and the AES decryption of the body were also checked
independently.

| Item | Value |
|---|---|
| username | `root-sp-5` |
| challenge data | `acdedf699213b500038e5a3728851a2aa9655571efa0f7fd4120583f6193820f` |
| AES key | `c9d4b51fed21836f0ce33d2579e953ca` |
| IV | `acdedf699213b500038e5a3728851a2a` |
| report | Enter (0x28), no modifiers, keybreak 1: plaintext `00 01 28 00 00 00 00 00` + `08`×8 padding |
| packet (49 bytes) | `49555342 20202020 01 00 20 58 11000000 00 30 10 80 02 00 0000 00000000 01000000` `08` `07f5d0763b51e0698e214025e2f0400b` |

### 8.10 Keymap

**Vendor.** A Java AWT virtual-key code and its *key location* map to a USB
usage through `.properties` tables:

- Four sub-maps: `standard`, `keypad`, `left`, `right`. The location picks
  the sub-map: 1 = standard, 2 = left, 3 = right, 4 = numpad. Location 0
  (unknown) falls back to standard for some locales, or on Linux/Solaris with
  Java ≥ 1.7.
- `default.properties` gives US positions, for example: letters A–Z → 4…29;
  digits 1–9, 0 → 30…39; Enter 40, Esc 41, Backspace 42, Tab 43, Space 44;
  CapsLock 57, PrintScreen 70, ScrollLock 71, Pause 72, Delete 76, Left 80,
  Up 82; keypad NumLock 83, `/` 84, `*` 85, `-` 86, `+` 87, Enter 88,
  1–9 → 89–97, 0 98, `.` 99, `=` 103; left Ctrl 224, Shift 225, Alt 226,
  Win 227; right Ctrl 228, Shift 229, Alt 230, Win 231.
- A per-locale file (30+ locales such as `fr_FR`, `de_DE`) overrides entries,
  for example French `VK_A → 20`, `VK_AMPERSAND → 30`, `VK_LESS → 100`.
- The locale comes from the *client* JVM (or the `redirlib.keymap`
  property). The host layout must therefore match the client locale.
  Characters from dead keys or AltGr need extra per-locale patch tables.
- For modifier keys the usage lookup only confirms that the key is known; the
  report carries the modifier *bit*.

**`ilom-kvm-rs`.**

- The GUI sends **physical key positions** mapped directly to usages. The
  host applies its own layout, and no client-side locale tables are needed.
- Modifier bits are tracked per physical key, left and right separately.
  AltGr is sent as bit 0x40 (winit on Linux does not report AltGr). The ISO
  `<>` key maps to 0x64.
- A patched egui-winit (`third_party/egui-winit/PATCH.md`) routes keypad,
  NumLock, CapsLock, ScrollLock, PrintScreen, Pause and Menu through
  F14–F35; the GUI maps these back to the proper usages. Upstream egui-winit
  folds keypad digits onto the top row and has no lock keys.
- `src/keymap.rs` (clipboard paste only) converts *text* to
  (modifier, usage) strokes for a chosen **host** layout: US, or French
  AZERTY with the AltGr layer and dead-key accents (`^`/`¨` on usage 0x2F,
  then the base letter).

## 9. Password hashing primitives

The vendor code puts every scheme behind one entry point: Unix-hash a secret
with a salt, then digest the hash together with a challenge. The first three
characters of the salt select the scheme:

| Salt prefix | Scheme | Output of the hash step |
|---|---|---|
| `$1$` | MD5-crypt | 34 ASCII bytes `$1$<8 salt>$<22 chars>` |
| `$6$` | vendor SHA-512 (not SHA-crypt) | 64 **raw binary** bytes |
| anything else | traditional DES crypt | 13 ASCII bytes |

### 9.1 Hash and digest

- **Unix hash to a fixed length:** hash the secret with the salt, then copy
  the result into a zero-filled buffer of the requested length
  (`min(hash, len)` bytes). Lengths used: 34, 64 or 129.
- **Challenge digest:** `Digest(unix_hash_buffer ‖ challenge)`.
  "Backward compatible" = **MD5**, 16 bytes. "FIPS" = **SHA-512**, 64 bytes
  (the HID key takes the first 16).
- Constants: digest length 16, SHA-512 digest length 64, maximum hashed
  password length 128, Unix MD5 hash length 34.

Where each combination is used:

| Use | Secret | Salt | Hash length | Digest | `ilom-kvm-rs` |
|---|---|---|---|---|---|
| Video login ([§5.4](#54-handshake)), after token auth | literal `"token"` | SP challenge (`$1$…` or 2-char DES) | 34 (levels 0/1), 64 (level 2) | MD5 | yes, always 34 |
| Serial console login | password | SP salt | 34/64 by level | MD5, or SHA-512 at level 2 | no |
| HID legacy LOGIN (105) | password | SP salt (server IDs 2/3/4) or level default | **129** | MD5 | no |
| HID key, level 1 | session username | `$1$A17c6z5w$` | 34 | MD5, Blowfish key | no |
| HID key, level 2 | session username | `$6$I27m0w5z15um9P5f` | 129 | SHA-512[0..16], AES key | yes |

**SP challenge salt field** (8 bytes when the server version is 1,
otherwise 12):

- If it starts with `$1$`, it is an MD5-crypt salt. The vendor keeps the
  whole field including NULs; `ilom-kvm-rs` cuts at the first NUL. Both are
  equivalent, since only 8 salt characters are used.
- Otherwise only the **first 2 bytes** are kept, as a DES salt. The SP has
  been observed sending NUL bytes here.

### 9.2 MD5Crypt

Standard FreeBSD/glibc `$1$` MD5-crypt (1000 rounds, crypt base-64 alphabet
`./0-9A-Za-z`). Output is compatible for ASCII secrets.

- The vendor takes salt characters 3..11, so exactly 8 salt characters are
  assumed. A shorter salt raises an error and returns null.
- The secret is encoded in the platform default charset.

Test vector: `openssl passwd -1 -salt saltsalt password` =
`$1$saltsalt$qjXMvbEw8oaL.CzflDtaK/`.

### 9.3 SHA512Crypt (vendor)

- Computes `SHA-512(secret ‖ "$6$" ‖ salt[3 .. min(16, len(salt))])` and
  returns the 64 raw bytes (UTF-8 input).
- No rounds, no base-64 encoding, at most 13 salt characters.
- Used only for the HID level-2 key (and, in theory, serial or video login at
  level 2).
- Verified through the [§8.9](#89-test-vector) vector.

### 9.4 Salt dispatch

The dispatcher is described at the top of this section. The SP salt parser
never keeps a `$6$` salt: it truncates any non-`$1$` salt to 2 characters, so
an SP-supplied SHA-512 salt would silently become DES. *Not seen in
practice.*

### 9.5 DESCrypt

Traditional 13-character Unix `crypt(3)`:

- **Key:** up to 8 secret characters, each shifted left by 1.
- **Salt:** 2 characters; a missing character counts as `.`.
- **Cipher:** 25 DES iterations over a zero block with a salt-perturbed
  E-box.
- **Output:** the 2 salt characters, then 11 base-64 characters.

Two compatibility quirks:

- A salt character outside the crypt alphabet counts as value 0 (like `.`),
  but is **copied unchanged** into the first two output bytes. With the SP's
  NUL salt the output starts `00 00`.
- The key uses Java `char` values, so characters above 127 overflow into the
  next key byte. *Unverified; irrelevant for `"token"`.*

`ilom-kvm-rs` maps non-alphabet salt bytes to `.` for the DES computation,
then puts the raw bytes back in output positions 0–1 (test
`des_salt_with_nul_bytes_behaves_like_dots`). The result is zero padded to
the requested length (34 for video).

## 10. Virtual media channels

Virtual CD-ROM and virtual floppy use two separate plain-TCP channels that
work almost the same way. The SP presents a USB mass-storage device to the
host. Every SCSI command that the host sends to that device is forwarded to
the client, wrapped in an IUSB packet. The client runs the command against a
local image or drive and sends back the result.

| Channel | Port | Device | Block size | Max data per response |
|---|---|---|---|---|
| CD-ROM | 5120 | CD/DVD drive or `.iso` image | 2048 | 64 sectors = 131072 bytes |
| Floppy | 5123 | Floppy/USB drive or `.img` image | 512 | 256 sectors = 131072 bytes |

No TLS, no IVTP framing, and no username/password login: one token
authenticates the channel. IUSB fields are little-endian; CDB bytes keep
normal SCSI big-endian order.

**Status:** implemented in `ilom-kvm-rs` (`src/vmedia.rs`, `src/scsi.rs`) for
image files. The layout and command set below come from the vendor Java code
and from the exported functions of its Linux image readers; the handshake,
the packet layout and the host command traffic were then checked against a
live SP (floppy image read by the host, CD image enumerated and read).
Physical drives and NFS mode are not implemented.

### 10.1 Split between Java and native code

| Layer | Responsibility |
|---|---|
| Java (`CDROMRedir` / `FloppyRedir`, `PacketMaster`, `IUSBHeader`, `IUSBSCSI`) | TCP connect, token, ACK check, IUSB reassembly, request/response loop, stop/teardown, NFS-mode packets |
| Native (`libjavacdromwrapper.so`, `libjavafloppywrapper.so`) | Listing and opening drives/images, **the whole SCSI command interpreter**, writing the **full** response packet (IUSB header included) |

Java passes two direct `ByteBuffer`s to the native "execute" call: one holds
the raw request packet from offset 0; the native code writes the complete
response packet into the other, also from offset 0, and returns its total
length. Java interprets no SCSI.

**Native libraries** in the per-OS jars:

| Jar | Contents |
|---|---|
| `linuxi386.jar` | `libjavacdromwrapper.so`, `libjavacdromwrapper-34.so`, `libjavafloppywrapper.so`, `libjavafloppywrapper-34.so`. The `-34` builds are a gcc 3.4 fallback, loaded only when the normal build fails a test instantiation. |
| `solarissparc.jar`, `solarisx86.jar` | `libjavacdromwrapper.so`, `libjavafloppywrapper.so` |
| `win32.jar` | `javacdromwrapper.dll`, `javafloppywrapper.dll` |

All are 32-bit only. The Java code refuses to start either channel on a
64-bit JRE ("not supported with the 64-bit JRE").

**JNI methods** (same for CD-ROM and floppy; `X` = `CDROM` or `Floppy`):

| Java native method | Purpose |
|---|---|
| `newXReader(boolean physical)` | Create the reader: physical drive or image file |
| `openX(String path)` → `boolean` | Open the device or image |
| `closeX()`, `deleteXReader()` | Teardown |
| `listXDrives()` → `String[]` | Enumerate local drives (physical mode only) |
| `executeXSCSICmd(ByteBuffer req, ByteBuffer resp)` → `int` | Run one request; return the total response length |
| `getVersion()` → `String` | Library version |

**Native internals.** The image readers (`ExecuteSCSICmd`,
`ExecuteFloppyImageSCSICmd`) were examined for their packet offsets, command
dispatch and sense codes; the results are in [§10.4](#104-iusb-scsi-packet)
and [§10.5](#105-scsi-commands). Other points:

- **CD image check:** the reader requires `CD001`, the ISO 9660 volume
  descriptor identifier at byte 32769 (sector 16, offset 1).
- **Physical Linux CD reader:** drives come from `/proc/sys/dev/cdrom/info`
  and open as `/dev/%s` or `/dev/scd%c`.
- **Floppy image reader:** it reopens the image for every command, read-only
  unless a WRITE arrives and writes are allowed.
- **Physical Linux floppy reader:** scans `/dev/fd%d` and USB floppies on
  `/dev/sd%c`.
- **Return value:** the native command function reports the number of data
  bytes; the JNI wrapper adds 61 to get the packet length.

### 10.2 Connection handshake

1. **(Local mode)** Create and open the reader first. If opening fails, the
   channel is never connected.
2. Open TCP to port 5120 or 5123 with `SO_KEEPALIVE`.
3. **Token.** Get a fresh 20-byte token from tokend
   ([§4.2](#42-get-redirection-token)) and send it **raw**: exactly 20 bytes,
   no header, no length. (Without tokend the vendor client gets a 1-byte
   dummy and sends no token at all, a legacy path.)
4. **ACK.** Read one IUSB packet from the SP (framing in
   [§10.3](#103-iusb-header)):

   | Absolute offset | Size | Meaning |
   |---|---|---|
   | 41 | 1 | Opcode; must be `0xF1` (DEVICE_REDIRECTION_ACK). Anything else is a protocol error and the channel closes. |
   | 62 | 1 | Connection status. Read only when the IUSB data length is > 30 (packet ≥ 63 bytes). |

   | Status | Meaning | Vendor message |
   |---|---|---|
   | `1` | Accepted | — |
   | `2` | Denied | "…redirection has been established by another running session." |
   | `3` | Not supported (only the floppy code checks it; the CD code reports "denied") | "Floppy redirection is not supported by this Host" |
   | other / missing | Treated as denied | — |

   Offset 62 is the **second** byte of the data area
   ([§10.4](#104-iusb-scsi-packet)). The ACK observed from the SP is
   87 bytes: data length 55, opcode `0xF1` at 41, status `1` at 62, all other
   bytes zero (device type and protocol are 0 in this packet). There is no
   separate max-sessions reply: one CD session and one floppy session per SP
   seems to be the limit, enforced by status 2.
5. **(NFS mode only)** Send START_REMOTE_IMAGE ([§10.8](#108-nfs-mode)).
6. Enter the request/response loop ([§10.6](#106-requestresponse-loop)).

### 10.3 IUSB header

Same 32-byte header as the HID channel ([§8.4](#84-iusb-header-32-bytes)).
Values the vendor client uses for CD-ROM:

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | signature | `"IUSB    "` |
| 8 | 1 | major | 1 |
| 9 | 1 | minor | 0 |
| 10 | 1 | header length | 32 |
| 11 | 1 | checksum | two's complement ([§8.4](#84-iusb-header-32-bytes)) |
| 12 | 4 | data length | bytes after the 32-byte header |
| 16 | 1 | server caps | 0 |
| 17 | 1 | device type | `0x05` (CD-ROM) |
| 18 | 1 | protocol | `0x01` |
| 19 | 1 | direction | client always sends `0x80` |
| 20 | 1 | device number | 0 |
| 21 | 1 | interface | 0 |
| 22 | 2 | client data | 0 |
| 24 | 4 | sequence number | 0 in client-built packets |
| 28 | 4 | reserved | 0 |

These values come from the vendor factory for NFS packets. For normal
responses the native reader starts from a copy of the request's first 62
bytes, so the header fields (device type, protocol, device/interface number,
sequence) are echoed; Java then sets direction `0x80`, the data length and
the checksum. `ilom-kvm-rs` does the same, with a header-only checksum, and
the SP accepts it.

**Framing.** A packet is 32 + data length bytes. The receiver waits until the
whole packet is buffered and rejects it if the signature does not match.

### 10.4 IUSB SCSI packet

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 32 | IUSB header | [§10.3](#103-iusb-header) |
| 32 | 4 | transfer length (u32) | *Inferred* name; echoed unchanged |
| 36 | 4 | tag (u32) | *Inferred* name; echoed unchanged |
| 40 | 1 | data direction | *Inferred* name; echoed unchanged |
| 41 | 12 | CDB | Standard SCSI CDB (big-endian fields): opcode at 41, LBA at 43, READ(10) length at 48. Also carries vmedia control opcodes `0xF0`–`0xF4`. |
| 53 | 1 | overall status | 0 = good, 1 = check condition |
| 54 | 1 | sense key | |
| 55 | 1 | ASC | |
| 56 | 1 | ASCQ | |
| 57 | 4 | data length (u32) | Payload bytes that follow |
| 61 | n | data | Read payload (response) or write payload (floppy WRITE request), or NFS path |

Offsets 41, 53–61 are the ones the native readers use. Bytes 32–40 are only
copied, so their meaning does not matter to a client.

A response is at most 61 + 131072 = **131133** bytes (the vendor response
buffer size). Request buffers are 1024 bytes for CD-ROM (CD requests never
carry data) and 131133 bytes for floppy (WRITE carries up to 128 KiB).

**Vmedia control opcodes** (byte 41, not SCSI):

| Opcode | Name | Direction | Vendor use |
|---|---|---|---|
| `0xF0` | START_REMOTE_IMAGE_REDIRECTION | client → SP | NFS mode |
| `0xF1` | DEVICE_REDIRECTION_ACK | SP → client | Handshake; also repeated by the SP while tearing a redirection down (observed) |
| `0xF2` | STOP_REMOTE_IMAGE_REDIRECTION | client → SP | Defined, **never sent** |
| `0xF3` | START_LOCAL_IMAGE_REDIRECTION | client → SP | Defined, never sent |
| `0xF4` | CONTINUE_REMOTE_IMAGE_REDIRECTION | client → SP | NFS mode keep-alive/poll |

### 10.5 SCSI commands

The SP's USB gadget answers device-identity commands (INQUIRY, MODE SENSE,
GET CONFIGURATION, ...) itself. Only media access reaches the client. These
are the commands the vendor image readers handle:

**CD-ROM (2048-byte sectors):**

| Opcode | Command | Answer |
|---|---|---|
| `0x00` | TEST UNIT READY | GOOD. Right after the image is opened, UNIT ATTENTION (6/28/00) once. |
| `0x1B` | START STOP UNIT | GOOD |
| `0x25` | READ CAPACITY(10) | 8 bytes big-endian: last LBA (blocks − 1), block length 2048. Also reports the pending UNIT ATTENTION. |
| `0x28` | READ(10) | LBA (CDB bytes 2–5) and 16-bit count (bytes 7–8). No size check in the reader. |
| `0x43` | READ TOC | Always a single-track TOC (see below), cut to the allocation length |
| other | — | CHECK CONDITION 5/20/00 (invalid opcode). This includes PREVENT ALLOW MEDIUM REMOVAL (`0x1E`) and READ(12). |

The vendor TOC is a 4-byte header (length, first track 1, last track 1),
a track 1 descriptor (control `0x14` = data track, address LBA 0 or MSF
00:02:00 when CDB bit 1.1 is set) and a lead-out descriptor (track `0xAA`,
control `0x16`). Two vendor bugs: the lead-out is always in MSF form and is
computed from the last LBA instead of the capacity, and the length field is
taken after truncation. A start track above 1 other than `0xAA` returns no
data. `ilom-kvm-rs` returns correct lead-out addresses, full lengths, the
lead-out alone for start track `0xAA`, session info for format 1, and an
error for other start tracks.

**Floppy / USB image (512-byte sectors):**

| Opcode | Command | Answer |
|---|---|---|
| `0x00` | TEST UNIT READY | As for CD-ROM |
| `0x04` | FORMAT UNIT | GOOD (nothing is done) |
| `0x1B` | START STOP UNIT | GOOD |
| `0x1E` | PREVENT ALLOW MEDIUM REMOVAL | GOOD |
| `0x23` | READ FORMAT CAPACITIES | 12 bytes: list length 8, block count (u32 BE), descriptor type 2 (formatted) with block length 512. Without media: 2880 blocks, type 3. |
| `0x25` | READ CAPACITY(10) | Last LBA, block length 512 |
| `0x28` | READ(10) | At most 256 blocks, else 5/26/00 |
| `0x2A` | WRITE(10) | Data at request offset 61; at most 256 blocks. Read-only image: 7/27/00. |
| other | — | 5/20/00 |

**Sense codes** used by the readers (overall status 1 unless noted):

| Sense | Meaning | Used for |
|---|---|---|
| 0/00/00 (status 0) | good | success |
| 5/20/00 | invalid command operation code | unsupported command |
| 5/21/00 | LBA out of range | read past the end |
| 5/26/00 | invalid field in parameter list | floppy transfer above 256 blocks |
| 5/53/02 | medium removal prevented | (defined) |
| 6/28/00 | not ready to ready change, medium may have changed | first TUR / READ CAPACITY after open |
| 3/11/00 | unrecovered read error | short read |
| 2/3A/00 | medium not present | image not open |
| 3/30/01, 3/30/02 | cannot read medium: unknown / incompatible format | (defined) |
| 7/27/00 | write protected | WRITE to a read-only floppy image |

**Observed host traffic.** A Linux host (Proxmox kernel 7.0) sends, for the
CD: TEST UNIT READY, READ TOC in formats 0 and 1 with 12- and 20-byte
allocations and an MSF lead-out query (start track `0xAA`), READ CAPACITY,
PREVENT ALLOW MEDIUM REMOVAL, then READ(10). For the floppy it sends TEST
UNIT READY, READ CAPACITY, PREVENT ALLOW MEDIUM REMOVAL and READ(10) of
1 or 8 blocks at LBA 0 and at power-of-two offsets (partition and filesystem
probes). READ FORMAT CAPACITIES was not seen.

`ilom-kvm-rs` answers PREVENT ALLOW MEDIUM REMOVAL with GOOD for the CD too,
and also accepts READ(12).

Images: a CD image must pass the ISO 9660 `CD001` check. The file choosers
filter on `.iso` (CD) and `.img` (floppy). Floppy images are raw sector
dumps.

### 10.6 Request/response loop

- **Strict lockstep:** receive one request, run it, send one response. No
  pipelining.
- The vendor client re-parses the first 32 bytes of the native response as
  the IUSB header, sets data length = N − 32 and direction `0x80`,
  recomputes the checksum and sends N bytes (N = native return value).
- **Keep-alive:** none at application level for local images, only TCP
  `SO_KEEPALIVE`. The receive wait times out after 120 s. On timeout the
  vendor client tests liveness by **opening a new TCP connection to the same
  port and closing it at once**. If that connect fails, the channel is
  declared dead ("Connection with SP is down…"). EOF or a read error also
  closes the channel.
- Errors in the loop go to an error callback; the UI then stops the
  redirection and shows the message.
- `ilom-kvm-rs` uses no read timeout: the host may not touch the drive for
  a long time, and stopping shuts the socket down.

### 10.7 Stop, eject and media change

- **Stop:** no stop message (`0xF2` is never sent). The client stops its
  worker thread, shuts the socket down and closes it, then closes and
  deletes the native reader. The SP presumably treats the closed connection
  as media removal *(uncertain)*.
- **SP teardown:** when the video session closed while a CD redirection was
  still open, the SP sent `0xF1` packets on the media channel over and over
  (observed). They are not SCSI commands: do not answer them. `ilom-kvm-rs`
  ignores an `0xF1` with status 1 and ends the redirection on any other
  status.
- **Eject / change:** the Java side does nothing. To change the image, stop,
  then start with the new path. Media-change sense exists only inside the
  native physical-floppy reader. Behaviour on a host eject (START STOP UNIT
  with LoEj) is unknown.
- CD and CD-image redirection are mutually exclusive, and so are floppy and
  floppy-image. Virtual media menus are enabled only while video is active,
  and all virtual media stop when video stops.

### 10.8 NFS mode

The "CD-ROM NFS" and "Floppy NFS" menus ask for `host:/path/image.iso` (or
`.img`). Input starting with `nfs` (for example `nfs://host/path`) loses its
first 6 characters and becomes `host:/path`. No local reader is created: the
SP apparently mounts the export and serves the image itself *(inferred)*.
The client only sends control packets.

After the `0xF1` ACK, the client sends START_REMOTE_IMAGE:

| Offset | Size | Value |
|---|---|---|
| 0 | 32 | IUSB header with CD-ROM defaults, data length = 29 + L (floppy too) |
| 32 | 9 | 0 |
| 41 | 1 | `0xF0` |
| 42 | 19 | 0 |
| 61 | L | path bytes (platform charset), **no NUL terminator** |

It then loops sending CONTINUE: a 61-byte packet, header data length 29,
`0xF4` at offset 41, everything else zero. The client **never reads** from
the socket in this mode.

## 11. Quirks and pitfalls

### 11.1 Web and TLS

- The web server repeats the HTTP status line (`HTTP/1.0 200 OK` twice) and
  sometimes uses bare LF line endings. Strict HTTP stacks reject the
  response; use a tolerant HTTP/1.0 client that reads until EOF.
- The SP often closes the HTTPS connection without a TLS `close_notify`.
  Reading to EOF then ends with an error although the full response was
  received. Treat that error as end of response when data was read.
- The JNLP secret is single-use. Re-authenticating to tokend with it returns
  `0x02`. Every connection and every reconnect needs a new JNLP.
- ILOM has few web session slots (`msg=5` on the login page when full). Log
  out right after downloading the JNLP.
- TLS 1.2 with RSA key exchange only: rustls cannot connect; use OpenSSL.

### 11.2 tokend

- Token and challenge replies have no framing and no status byte. An error
  shows up only as a stall or a closed connection.
- Each channel (video auth, RC4 key, HID, CD-ROM, floppy) needs its own
  token.
- The vendor client accepts any auth result except `0x02`, and opens a second
  tokend connection while leaving the first unused.

### 11.3 Video

- The first exchange uses the 24-byte `REDIRECT` header; the connection then
  switches to the 7-byte IVTP header with no explicit signal.
- The IVTP `pkt_size` on replies can include the 7-byte header (the token
  reply is announced as 27). Use fixed sizes per type, except for types 5
  and 48.
- Max sessions is reported by device caps (`reserved[1] & 0x0f == 1`), not by
  type 49 or status 2060.
- After token authentication, a challenge login with the literal password
  `token` is still required.
- The login reply can be a NUM_ACTIVE_CLIENTS (51) packet instead of LOGIN
  (2).
- The vendor client parses the challenge reply with a 12-byte salt whatever
  the server version.
- The DES salt from the SP may contain NUL bytes. They count as `.` in the
  hash but are copied raw into its first two bytes.
- Frames arrive only when the screen changes. Long silence is normal and does
  not mean a dead link.
- The vendor type-40 packet announces 8 bytes but sends 10. The vendor parser
  rewrites a received compression mode 1 to 2.
- The vendor parser returns "no packet" for type 32 (BW_DETECT_RES), which
  would stall its stream parser if the server ever sent it *(uncertain)*.
- The vendor fragment reassembly appears to handle only single-fragment
  frames *(uncertain)*.

### 11.4 RC4 and frame decoding

- The RC4 keystream runs across frames and over every byte after the
  125 header bytes, including padding past `compressed_size`. Truncating
  before decrypting breaks every later frame. The state resets only on the
  frame's reset flag.
- The vendor client converts the RC4 key through a Java `String` and treats
  key bytes as signed; non-ASCII keys would break it.
- `ASP-2000` sits at offset 19 of the video header, after 19 reserved bytes.
- The vendor image-header size constant says 84, but the AST2100 header is
  86 bytes (84 is the AST2000 size).
- The image header is packed: `u32` fields sit at odd offsets (for example
  `compressed size` at rel 69).
- The bitstream is MSB-first inside little-endian 32-bit words, not a plain
  byte stream.
- DC predictors reset only at frame start, not at positioned jumps.
- Undefined command codes (0x1–0x3, 0xA, 0xB) consume no bits; the vendor
  loop spins until its MB counter (number of MBs + 1, off by one) runs out.
- The MB grid wraps on the **destination** width while pixels are placed with
  the **source** stride. Scaled modes may misplace blocks.
- The vendor quantisation builder treats base-table bytes ≥ 128 as negative,
  which clamps them to 1. This affects selectors 0–2, so the vendor client
  dequantises the coarsest tables differently from the hardware and the
  ASPEED codec.
- The VQ colour cache resets every frame, and VQ blocks decode correctly only
  in 4:4:4 mode.
- The WASM codec used by `ilom-kvm-rs` ignores the scale factors and the YUV
  mapping; it is correct only at the vendor defaults (scale 16).
- `ilom-kvm-rs` starts a new framebuffer white (vendor: black), does not
  clear it when the resolution shrinks, and ignores BLANK_SCREEN and
  hardware-cursor packets.

### 11.5 Keyboard and mouse

- The HID channel is plain TCP. Only the report body after the length byte
  is encrypted; headers and the length byte stay in clear.
- The encryption level is DEVCAPS reply byte 5; the client advertises 2 in
  the same byte.
- The AES key comes from the session *username*, not a password. The vendor
  SHA-512 "crypt" is a single hash with the salt truncated to
  `I27m0w5z15um9`, zero padded to 129 bytes.
- The AES-CBC IV is the first 16 bytes of the tokend challenge data. The
  cipher restarts for every packet, so identical reports give identical
  ciphertexts.
- Every encrypted report has 17 body bytes. Set the header data length and
  `reserved[0] = 1` *before* computing the checksum.
- The vendor IUSB checksum sums the whole send buffer, which can include
  stale bytes on unencrypted packets. Whether the SP validates it is unknown.
- The client IUSB sequence number is always 0.
- HIDCMD reply payload sizes are fixed per command and the announced length
  is ignored. Nothing follows the header when the status is non-zero.
- Auto-keybreak (report byte 2 = 1): the SP releases the key itself. Full
  state reports with explicit release also work.
- The SP drops reports sent back to back. Leave about 12 ms between reports
  when typing text (observed).
- The host reports its LED state only after a key event. Press NumLock twice
  on connect to get it.
- Absolute mouse coordinates use the source mode size, but the report's
  resolution fields are always 1024×768.
- Vendor relative-mouse splitting is buggy (it sends the remainder instead of
  126, and the loop condition uses `&&`), and deltas are divided by 1.5.
- Vendor modifier bugs: left Windows key sends bit 0x10 (R-Ctrl) instead of
  0x08; Shift/Ctrl/Alt with an unknown location send the right-side bit; the
  default keymap has `VK_KP_RIGHT = 95` (KP 7) instead of 94.
- egui/winit merges left and right modifiers, drops AltGr on Linux and folds
  keypad keys onto the top row; `ilom-kvm-rs` works around this with
  per-key tracking and a patched egui-winit.

### 11.6 Virtual media

- The token is 20 raw bytes with no header, sent right after the TCP
  connect.
- The connection-status byte in the ACK is at offset 62, not 61, and is read
  only when the data length is > 30. The vendor CD code reports status 3
  ("not supported") as "busy".
- There is no stop packet: stop is a socket close. `0xF2` and `0xF3` are
  defined but unused.
- The vendor native code reads each request at offset 0 of an already
  compacted buffer. This works only because the SP sends one request at a
  time and waits for the answer.
- When the video session closes first, the SP repeats `0xF1` on the media
  channel. Answering it as a SCSI command keeps the flood going.
- The CD request buffer is 1024 bytes, so CD requests never carry data.
  Floppy WRITE requests carry up to 128 KiB.
- A single READ response is capped at 131072 bytes (64 CD sectors,
  256 floppy sectors). Behaviour with a larger READ is unverified.
- NFS mode floods the SP with `0xF4` packets in a tight loop, never reads,
  and never checks whether `0xF0` succeeded. Floppy NFS packets use the
  CD-ROM device type (5) and protocol (1). The path has no NUL terminator.
- The vendor native libraries are 32-bit only, and the vendor client refuses
  virtual media on a 64-bit JRE.

### 11.7 Liveness probing

The vendor client waits up to 120 s per read on video and virtual media
channels. On timeout it opens a new TCP connection to the same SP port and
closes it at once to check that the SP is alive. That probe may count as a
session attempt on the SP.
