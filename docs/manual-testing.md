# Manual test checklist

Run this before tagging a release. Unit tests cover the protocol and the pure
helpers, but not how the GUI behaves against a real SP.

Tick each box, and note the platform (Linux X11, Linux Wayland, Windows,
macOS) next to anything that fails. Items marked **(Mac)** or **(Windows)**
only apply there.

## Safety first

The test host may have a root shell open on its console. Keys you send are
typed into it.

- **Never press Ctrl+Alt+Del** (menu entry or host key + Del) on a host you
  do not want to reboot. Test it only on a host at a BIOS screen, installer or
  login prompt that can take a reboot.
- **Magic SysRq:** only use **H (Help)**. B, O, E, I, U and S act at once.
- **Paste:** only paste a single line with no line break, then erase it
  (Ctrl+U in a shell). A pasted line break runs the command.
- Prefer a host sitting at a login prompt, a BIOS screen or an installer.

## Build and start

- [ ] `cargo build --release` succeeds.
- [ ] `ilom-kvm` with no arguments opens the login form.
- [ ] The window and taskbar show the app icon (blue tile, monitor). On
      Wayland and in the macOS Dock the generic icon is expected.

## Login form

- [ ] A saved host and username are filled in on start (after one successful
      login).
- [ ] Enter in the address, username or password field connects.
- [ ] Empty address, username or password shows the matching error.
- [ ] **Open downloaded JNLP…** connects with a fresh `jnlpgenerator-*` file.
- [ ] The window title becomes `<host> — ILOM Remote Console` while connected
      and goes back to `ILOM Remote Console` after Disconnect.

Wrong password (only with care: repeated failures may lock the account):

- [ ] One wrong password returns to the form with "Login rejected" and an
      empty password field. It does **not** retry by itself.
- [ ] Note the `msg=` code shown; it tells a bad password apart from other
      refusals.

## Video

- [ ] The host screen appears and updates.
- [ ] Resizing the window keeps the screen fitted and centred.
- [ ] **100%** shows host pixels 1:1. With a window smaller than the host
      screen, the wheel and scroll bars scroll; dragging on the screen does
      not scroll.
- [ ] Text is sharp at 100% and at other whole-number scales.
- [ ] The **100%** choice is remembered after a restart.
- [ ] Blank the host screen (for example turn it off in the OS or reboot into
      a mode change): the status shows "Host video is blank", then goes back
      to "Connected to …" when video returns.

## Keyboard capture and host key

- [ ] Before clicking, the toolbar says "Click the screen or press Right Ctrl
      to capture the keyboard" (Right Cmd on macOS).
- [ ] Clicking the screen captures: blue border, "⌨ Keyboard captured".
- [ ] Typing reaches the host, including Tab, arrows, Escape, F1–F12 and
      AltGr characters on AZERTY.
- [ ] **Release** stops sending keys; typing no longer reaches the host.
- [ ] Tapping the host key alone toggles capture on and off.
- [ ] Host key + F toggles fullscreen; the F does not reach the host.
- [ ] Host key + V pastes (see the paste safety note).
- [ ] The host key itself never reaches the host.
- [ ] Holding a letter, then pressing the host key, then releasing both: the
      letter does not stay stuck on the host.
- [ ] Keyboard menu → Host key: pick another key; it works at once and is
      remembered after a restart.
- [ ] `--host-key none`: tapping Right Ctrl reaches the host; only a click
      captures.
- [ ] Switching to another window releases all keys on the host.

## Send keys menu

- [ ] Ctrl+Alt+F2 then Ctrl+Alt+F1 (or F7) switches Linux virtual terminals.
- [ ] Alt+Tab, Super and Print Screen reach the host.
- [ ] Magic SysRq → H prints the SysRq help on a Linux console.
- [ ] Ctrl+Alt+Del: only on a host that may reboot (see safety).

## Keyboard menu

- [ ] Paste text types a single line with the chosen host layout (see the
      safety note). Characters the layout cannot type are listed as skipped.
- [ ] A long paste shows "Typing n/m" with a progress bar and a **Stop**
      button; Stop aborts it.
- [ ] Host layout choice is remembered after a restart.
- [ ] NUM/CAPS/SCROLL labels follow the host LEDs; the menu entries toggle
      them.

## Mouse

- [ ] The host pointer follows the local pointer and clicks land where
      expected, at fitted and at 100% scale.
- [ ] Right and middle buttons work.
- [ ] Dragging (for example selecting text) works, also when leaving the
      screen area while the button is held.
- [ ] With a graphical host session: the host cursor is drawn over the
      screen, and while captured the local pointer is hidden over it. Its
      shape changes (arrow, text beam, resize) follow the host.
- [ ] With a text console (no host cursor): the local pointer is a crosshair
      while captured.
- [ ] Relative mouse mode, if an SP ever selects it: capture locks and hides
      the pointer, motion moves the host pointer, and the host key releases
      it.

## Fullscreen and toolbar

- [ ] A narrow window wraps the toolbar onto extra rows instead of cutting
      widgets.
- [ ] Fullscreen hides the toolbar; moving the pointer to the top edge shows
      it over the video without resizing the screen; it stays while a menu
      is open.
- [ ] Clicks on the fullscreen toolbar do not reach the host.
- [ ] Leaving fullscreen with the window manager (F11, Super+Up) updates the
      button label.
- [ ] Toolbar notices disappear after a few seconds.

## Virtual media

- [ ] CD-ROM… → Mount image… with an ISO: the host sees the drive and can
      read it; the menu shows commands and bytes read.
- [ ] Floppy/USB… with an `.img`: read-only by default; with "Allow the host
      to write" ticked before mounting, the host can write.
- [ ] Unmount removes the drive from the host.
- [ ] Drag an `.iso` onto the window: the overlay explains the drop; the ISO
      mounts as CD-ROM.
- [ ] Drag an `.img`: mounts as floppy/USB.
- [ ] Drag a file with another extension: a notice lists the accepted types.
- [ ] Drag an image while that drive already has one: nothing is replaced and
      a notice says so.
- [ ] **(Windows)** Drag and drop works, and the Mount image… file dialog
      still opens afterwards.

## Screenshot

- [ ] Save as PNG writes a file to the Pictures folder (`~/Images/ilom-kvm`
      on a French Linux desktop); the notice shows the path.
- [ ] Two quick saves give two files.
- [ ] Copy to clipboard: pasting into an image editor or chat gives the host
      screen.
- [ ] Open folder opens the screenshot folder in the file manager.
- [ ] **(Mac)** started from the Finder, Save as PNG still works.

## Connection loss and reconnect

- [ ] Unplug the ILOM network (or block it) for a minute: within about 30 s
      (65 s on Windows) the screen dims and shows "reconnecting in Ns".
- [ ] **Reconnect now** skips the countdown.
- [ ] After the network returns, the session comes back without a new login,
      and mounted images come back.
- [ ] With a JNLP session, a lost connection says to open a fresh JNLP and
      does not retry.

## Leaving

- [ ] Disconnect with no media mounted goes straight back to the form.
- [ ] Disconnect with media mounted asks first and lists the images; Cancel
      keeps the session.
- [ ] While that dialog is open, typing does not reach the host.
- [ ] Closing the window with media mounted asks first.
- [ ] **(Mac)** Cmd+Q with media mounted: note whether it asks first or quits
      straight away.

## Settings

- [ ] The settings file exists after a login: `~/.config/ilom-kvm/settings`
      (Linux), `~/Library/Application Support/ilom-kvm/settings` or
      `~/.config/ilom-kvm/settings` if that folder already existed (macOS),
      `%APPDATA%\ilom-kvm\settings` (Windows).
- [ ] It never contains the password.
- [ ] `--host`, `--user` and `--host-key` override the saved values for that
      run.
- [ ] **(Windows)** Web login works without `HOME` set (the certificate store
      is found under `%APPDATA%`).

## VNC bridge

- [ ] `ilom-vnc` with `.env` credentials: `remote-viewer vnc://127.0.0.1:5900`
      shows the host screen after about 10 s.
- [ ] RustConn: a VNC connection to `127.0.0.1:5900` shows the screen, and
      typing and clicking reach the host.
- [ ] With a French host (`--layout fr`) and a client that sends keysyms
      (RustConn), `a`, `1`, `@` and `ê` come out right on the host.
- [ ] A second client at the same time sees the same screen; the log shows
      one ILOM session only.
- [ ] After the last client leaves, the log shows the ILOM session closing
      after `--idle-timeout` seconds; a new client starts a new one.
- [ ] A host resolution change (e.g. the BIOS to the OS) resizes the client
      window.
- [ ] `--listen 0.0.0.0:5900` without `ILOM_VNC_PASSWORD` refuses to start;
      with it, the client asks for the password and rejects a wrong one.
- [ ] A wrong `ILOM_PASSWORD`: the client shows the login error, and later
      clients get the same error without a new web login.
