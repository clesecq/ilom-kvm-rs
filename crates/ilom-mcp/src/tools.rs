//! The MCP tools.

use std::{path::PathBuf, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD};
use ilom_kvm_core::{hid, scsi::MediaKind};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    console::{Button, Console, Pointer, state_label},
    keys,
    ocr::{Ocr, format_lines},
    registry::Registry,
};

const INSTRUCTIONS: &str = "Drives the host console of an Oracle ILOM server: \
screenshots, screen text (read_screen), keyboard, mouse and virtual media. The \
first call opens the console session and takes about 10 seconds. Mouse \
coordinates are pixels of the latest screenshot or read_screen result. \
Several ILOM hosts may be configured: pass `host` (see list_hosts) \
to pick one; it can be omitted only when there is a single host.";

const BLANK_SCREEN: &str = "The screen is blank: the host is powered off or sends no video \
signal (no picture was received).";

const STALE_WARNING: &str = "WARNING: the console is not connected, this picture may be old; \
call status or retry shortly";

#[derive(Clone)]
pub struct IlomServer {
    registry: Arc<Registry>,
    ocr: Arc<Ocr>,
    tool_router: ToolRouter<Self>,
}

/// Parameters of the tools that only pick a host.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct HostParams {
    /// Name of the ILOM host (see list_hosts). Optional when only one host
    /// is configured.
    #[serde(default)]
    pub host: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeTextParams {
    /// Name of the ILOM host (see list_hosts). Optional when only one host
    /// is configured.
    #[serde(default)]
    pub host: Option<String>,
    /// Text to type on the host keyboard. Newlines press Enter. Characters
    /// the host layout cannot produce are skipped and reported.
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendKeysParams {
    /// Name of the ILOM host (see list_hosts). Optional when only one host
    /// is configured.
    #[serde(default)]
    pub host: Option<String>,
    /// Modifiers held while each key is pressed: ctrl, shift, alt, altgr or
    /// super (also rctrl, rshift, rgui). Empty for none.
    #[serde(default)]
    pub modifiers: Vec<String>,
    /// Keys pressed one after the other: a letter or digit, enter, esc,
    /// tab, space, backspace, delete, insert, home, end, pageup, pagedown,
    /// up, down, left, right, f1 to f24, and so on. For example
    /// modifiers [ctrl, alt] and keys [delete] sends Ctrl+Alt+Del.
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MouseAction {
    /// Move the pointer.
    Move,
    /// Move, then click once.
    Click,
    /// Move, then double-click.
    Double,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MouseParams {
    /// Name of the ILOM host (see list_hosts). Optional when only one host
    /// is configured.
    #[serde(default)]
    pub host: Option<String>,
    pub action: MouseAction,
    /// Column in pixels of the latest screenshot, 0 at the left.
    pub x: u32,
    /// Row in pixels of the latest screenshot, 0 at the top.
    pub y: u32,
    /// Button for click and double; left by default.
    #[serde(default)]
    pub button: MouseButton,
    /// Width in pixels of the screenshot the coordinates come from. The call
    /// fails if the screen size has changed since.
    #[serde(default)]
    pub expected_width: Option<u32>,
    /// Height in pixels of the screenshot the coordinates come from.
    #[serde(default)]
    pub expected_height: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Drive {
    Cdrom,
    Floppy,
}

impl From<Drive> for MediaKind {
    fn from(drive: Drive) -> Self {
        match drive {
            Drive::Cdrom => Self::Cdrom,
            Drive::Floppy => Self::Floppy,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MountParams {
    /// Name of the ILOM host (see list_hosts). Optional when only one host
    /// is configured.
    #[serde(default)]
    pub host: Option<String>,
    /// Virtual drive to attach the image to.
    pub kind: Drive,
    /// Image file below the media directory the server was started with
    /// (absolute, or relative to it): an ISO 9660 image for cdrom, a raw
    /// sector dump for floppy.
    pub path: PathBuf,
    /// Let the host write to the image. Floppy only, and refused unless the
    /// server was started with --allow-writable-media.
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnmountParams {
    /// Name of the ILOM host (see list_hosts). Optional when only one host
    /// is configured.
    #[serde(default)]
    pub host: Option<String>,
    /// Virtual drive to release.
    pub kind: Drive,
}

impl IlomServer {
    pub fn new(registry: Registry, ocr: Ocr) -> Self {
        Self {
            registry: Arc::new(registry),
            ocr: Arc::new(ocr),
            tool_router: Self::tool_router(),
        }
    }

    /// Runs blocking console work off the async runtime. An `Err` becomes a
    /// tool error the agent can read.
    async fn run<F>(&self, host: Option<String>, work: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(&Console) -> Result<Vec<ContentBlock>, String> + Send + 'static,
    {
        let console = match self.registry.resolve(host.as_deref()) {
            Ok(console) => console.clone(),
            Err(message) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(message)]));
            }
        };
        let outcome = tokio::task::spawn_blocking(move || work(&console))
            .await
            .map_err(|error| {
                ErrorData::internal_error(format!("tool task failed: {error}"), None)
            })?;
        Ok(match outcome {
            Ok(content) => CallToolResult::success(content),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        })
    }
}

#[tool_router]
impl IlomServer {
    #[tool(
        description = "List the configured ILOM hosts with their console connection state. Does not open any session."
    )]
    async fn list_hosts(&self) -> Result<CallToolResult, ErrorData> {
        let registry = self.registry.clone();
        let lines = tokio::task::spawn_blocking(move || registry.describe())
            .await
            .map_err(|error| {
                ErrorData::internal_error(format!("tool task failed: {error}"), None)
            })?;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            lines.join("\n"),
        )]))
    }

    #[tool(
        description = "Take a screenshot of the host console as a PNG image. Mouse coordinates are pixels of this image; the text says whether the console is connected, because a reconnecting console shows its last picture."
    )]
    async fn screenshot(
        &self,
        Parameters(params): Parameters<HostParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(params.host, |console| {
            Ok(match console.screenshot()? {
                Some(shot) => {
                    let mut note = format!(
                        "{}x{} pixels; connection {} ({})",
                        shot.width,
                        shot.height,
                        state_label(shot.state),
                        shot.message
                    );
                    if shot.stale() {
                        note.push_str(&format!(". {STALE_WARNING}"));
                    }
                    vec![
                        ContentBlock::image(STANDARD.encode(&shot.png), "image/png"),
                        ContentBlock::text(note),
                    ]
                }
                None => vec![ContentBlock::text(BLANK_SCREEN)],
            })
        })
        .await
    }

    #[tool(
        description = "Read the text on the host console with OCR. Returns each text line with its pixel position (same coordinate space as screenshot and mouse). Prefer screenshot for graphics; OCR can misread small or low-contrast text."
    )]
    async fn read_screen(
        &self,
        Parameters(params): Parameters<HostParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let ocr = self.ocr.clone();
        self.run(params.host, move |console| {
            let Some(capture) = console.capture()? else {
                return Ok(vec![ContentBlock::text(BLANK_SCREEN)]);
            };
            let lines = ocr.read(&capture.frame)?;
            let mut text = if lines.is_empty() {
                format!(
                    "{}x{} pixels; no text found. Try screenshot to look at the picture.",
                    capture.frame.width, capture.frame.height
                )
            } else {
                format_lines(&lines, capture.frame.width, capture.frame.height)
            };
            text.push_str(&format!(
                "\nconnection {} ({})",
                state_label(capture.state),
                capture.message
            ));
            if capture.stale() {
                text.push_str(&format!(". {STALE_WARNING}"));
            }
            Ok(vec![ContentBlock::text(text)])
        })
        .await
    }

    #[tool(description = "Type text on the host keyboard, through the host keyboard layout.")]
    async fn type_text(
        &self,
        Parameters(params): Parameters<TypeTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let host = params.host.clone();
        self.run(host, move |console| {
            let (count, skipped) = console.type_text(&params.text)?;
            let mut message = format!("typed {count} keystrokes");
            if !skipped.is_empty() {
                let skipped: String = skipped.into_iter().collect();
                message.push_str(&format!(
                    "; skipped characters the layout cannot type: {skipped}"
                ));
            }
            Ok(vec![ContentBlock::text(message)])
        })
        .await
    }

    #[tool(
        description = "Press keys on the host keyboard with modifiers held, for example Ctrl+Alt+Delete, Enter, Escape, F2 or Tab."
    )]
    async fn send_keys(
        &self,
        Parameters(params): Parameters<SendKeysParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let host = params.host.clone();
        self.run(host, move |console| {
            let (modifiers, usages) = parse_keys(&params)?;
            console.send_keys(modifiers, &usages)?;
            Ok(vec![ContentBlock::text(format!(
                "pressed {} key(s)",
                usages.len()
            ))])
        })
        .await
    }

    #[tool(
        description = "Move the host mouse pointer, click or double-click at pixel coordinates of the latest screenshot. The mouse wheel is not supported."
    )]
    async fn mouse(
        &self,
        Parameters(params): Parameters<MouseParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let host = params.host.clone();
        self.run(host, move |console| {
            let action = match params.action {
                MouseAction::Move => Pointer::Move,
                MouseAction::Click => Pointer::Click,
                MouseAction::Double => Pointer::Double,
            };
            let button = match params.button {
                MouseButton::Left => Button::Left,
                MouseButton::Right => Button::Right,
                MouseButton::Middle => Button::Middle,
            };
            let (width, height) = console.mouse(
                action,
                params.x,
                params.y,
                button,
                (params.expected_width, params.expected_height),
            )?;
            Ok(vec![ContentBlock::text(format!(
                "pointer at ({}, {}) on a {width}x{height} screen",
                params.x, params.y
            ))])
        })
        .await
    }

    #[tool(
        description = "Report the console connection state, host keyboard LEDs, input availability and the virtual media drives."
    )]
    async fn status(
        &self,
        Parameters(params): Parameters<HostParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(params.host, |console| {
            let status = console.status()?;
            let mut lines = vec![
                format!(
                    "connection: {} ({})",
                    state_label(status.state),
                    status.message
                ),
                format!(
                    "keyboard: {}, mouse: {}",
                    if status.keyboard {
                        "ready"
                    } else {
                        "not ready"
                    },
                    if status.absolute_mouse {
                        "absolute"
                    } else {
                        "relative (unsupported)"
                    }
                ),
            ];
            if let Some(leds) = status.leds {
                lines.push(format!(
                    "leds: num {}, caps {}, scroll {}",
                    on(leds & hid::LED_NUM_LOCK),
                    on(leds & hid::LED_CAPS_LOCK),
                    on(leds & hid::LED_SCROLL_LOCK),
                ));
            }
            for (name, media) in [("cdrom", &status.cdrom), ("floppy", &status.floppy)] {
                lines.push(match (&media.image, &media.error) {
                    (_, Some(error)) => format!("{name}: error: {error}"),
                    (Some(image), None) => format!(
                        "{name}: {image} ({}{})",
                        if media.active { "active" } else { "connecting" },
                        if media.writable { ", writable" } else { "" }
                    ),
                    (None, None) => format!("{name}: empty"),
                });
            }
            Ok(vec![ContentBlock::text(lines.join("\n"))])
        })
        .await
    }

    #[tool(
        description = "Attach an image file from this machine to the host's virtual CD-ROM or floppy drive. The image stays attached until unmount_media. This exposes the file's contents to the host."
    )]
    async fn mount_media(
        &self,
        Parameters(params): Parameters<MountParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let host = params.host.clone();
        self.run(host, move |console| {
            let message = console.mount(params.kind.into(), &params.path, params.writable)?;
            Ok(vec![ContentBlock::text(message)])
        })
        .await
    }

    #[tool(description = "Release the host's virtual CD-ROM or floppy drive.")]
    async fn unmount_media(
        &self,
        Parameters(params): Parameters<UnmountParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let host = params.host.clone();
        self.run(host, move |console| {
            Ok(vec![ContentBlock::text(
                console.unmount(params.kind.into())?,
            )])
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for IlomServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ilom-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

fn on(bit: u8) -> &'static str {
    if bit != 0 { "on" } else { "off" }
}

/// Modifier bitmap and key usages of a `send_keys` call.
pub fn parse_keys(params: &SendKeysParams) -> Result<(u8, Vec<u8>), String> {
    if params.keys.is_empty() {
        return Err("keys must not be empty".into());
    }
    let mut modifiers = 0;
    for name in &params.modifiers {
        modifiers |= keys::modifier(name).ok_or_else(|| format!("unknown modifier {name:?}"))?;
    }
    let usages = params
        .keys
        .iter()
        .map(|name| keys::usage(name).ok_or_else(|| format!("unknown key {name:?}")))
        .collect::<Result<_, _>>()?;
    Ok((modifiers, usages))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_the_nine_tools() {
        let mut names: Vec<_> = IlomServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "list_hosts",
                "mount_media",
                "mouse",
                "read_screen",
                "screenshot",
                "send_keys",
                "status",
                "type_text",
                "unmount_media"
            ]
        );
    }

    #[test]
    fn params_deserialize() {
        let params: MouseParams =
            serde_json::from_str(r#"{"action":"click","x":3,"y":4}"#).unwrap();
        assert!(matches!(params.button, MouseButton::Left));
        let params: MouseParams = serde_json::from_str(
            r#"{"action":"move","x":1,"y":2,"expected_width":640,"expected_height":480}"#,
        )
        .unwrap();
        assert_eq!(params.expected_width, Some(640));
        let params: TypeTextParams =
            serde_json::from_str(r#"{"text":"a","host":"rack1"}"#).unwrap();
        assert_eq!(params.host.as_deref(), Some("rack1"));
        let params: HostParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.host, None);
        let params: MountParams =
            serde_json::from_str(r#"{"kind":"cdrom","path":"/tmp/a.iso"}"#).unwrap();
        assert!(!params.writable);
        assert!(serde_json::from_str::<MouseParams>(r#"{"action":"scroll","x":0,"y":0}"#).is_err());
    }

    #[test]
    fn ctrl_alt_del() {
        let params = SendKeysParams {
            host: None,
            modifiers: vec!["ctrl".into(), "alt".into()],
            keys: vec!["delete".into()],
        };
        assert_eq!(parse_keys(&params), Ok((0x05, vec![0x4c])));
        let bad = SendKeysParams {
            host: None,
            modifiers: vec![],
            keys: vec!["bogus".into()],
        };
        assert!(parse_keys(&bad).is_err());
    }
}
