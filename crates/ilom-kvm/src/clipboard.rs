//! Local clipboard access for pasting text to the host and copying
//! screenshots.
//!
//! On Wayland, text goes through the window's own Wayland connection
//! (`smithay-clipboard`, as egui does). That works on every compositor,
//! including GNOME, which lacks the data-control protocols a standalone
//! client needs. Images, and everything on X11, macOS and Windows, use
//! `arboard`; under Wayland it reaches the clipboard through XWayland.

use raw_window_handle::RawDisplayHandle;

pub struct Clipboard {
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    wayland: Option<smithay_clipboard::Clipboard>,
    /// Opened on first use and then kept: on Linux the copied data is served
    /// by this process and vanishes when the handle is dropped.
    arboard: Option<arboard::Clipboard>,
}

impl Clipboard {
    /// `display` is the window's display handle, used on Wayland.
    pub fn new(display: Option<RawDisplayHandle>) -> Self {
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        let _ = display;
        Self {
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            ))]
            wayland: match display {
                // SAFETY: the pointer is the live `wl_display` of the app
                // window, which outlives the viewer that owns this clipboard.
                Some(RawDisplayHandle::Wayland(handle)) => {
                    Some(unsafe { smithay_clipboard::Clipboard::new(handle.display.as_ptr()) })
                }
                _ => None,
            },
            arboard: None,
        }
    }

    pub fn text(&mut self) -> Result<String, String> {
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        if let Some(wayland) = &self.wayland {
            return wayland.load().map_err(|error| error.to_string());
        }
        self.arboard()?
            .get_text()
            .map_err(|error| error.to_string())
    }

    pub fn set_image(&mut self, width: usize, height: usize, rgba: &[u8]) -> Result<(), String> {
        let image = arboard::ImageData {
            width,
            height,
            bytes: std::borrow::Cow::Borrowed(rgba),
        };
        self.arboard()?
            .set_image(image)
            .map_err(|error| error.to_string())
    }

    fn arboard(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.arboard.is_none() {
            self.arboard = Some(arboard::Clipboard::new().map_err(|error| error.to_string())?);
        }
        Ok(self.arboard.as_mut().expect("just opened"))
    }
}
