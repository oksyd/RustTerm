//! Standard RemoteApp (MS-RDPERP): RAIL control and windowing orders.
//!
//! Graphics use the ordinary desktop bitmap/surface stream. Coordinates below
//! are remote desktop coordinates, not local window-manager coordinates.

mod orders;
pub(crate) use orders::Windows;
#[cfg(test)]
pub(crate) use orders::tests::created as test_window_order;

use crate::{Error, Result};

/// Application published by the server, for example `||notepad`.
/// These strings are sent to the authenticated server, never executed locally.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteApp {
    /// Published alias (including the || prefix) or server-side executable path.
    pub program: String,
    /// Optional server-side working directory, up to 260 UTF-16 units.
    pub working_directory: String,
    /// Literal command-line arguments, up to 8000 UTF-16 units.
    pub arguments: String,
    /// Enable only when the caller handles server local move/size notifications.
    pub local_move_size: bool,
}

impl RemoteApp {
    /// Check protocol string lengths and reject control characters.
    pub fn validate(&self) -> Result<()> {
        if self.program.trim().is_empty() {
            return Err(Error::Invalid("RemoteApp program"));
        }
        for (text, limit) in [
            (&self.program, 260),
            (&self.working_directory, 260),
            (&self.arguments, 8000),
        ] {
            if text.chars().any(char::is_control) || text.encode_utf16().count() > limit {
                return Err(Error::Invalid("RemoteApp launch parameters"));
            }
        }
        Ok(())
    }
}

/// Rectangle with exclusive right/bottom edges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Left edge in pixels.
    pub left: i32,
    /// Top edge in pixels.
    pub top: i32,
    /// Exclusive right edge in pixels.
    pub right: i32,
    /// Exclusive bottom edge in pixels.
    pub bottom: i32,
}

/// Complete window state, with absent fields retained from previous orders.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteWindow {
    /// Server-assigned window identifier.
    pub id: u32,
    /// Owning window identifier, or zero.
    pub owner: u32,
    /// Server-provided window caption.
    pub title: String,
    /// Windows WS_* style flags.
    pub style: u32,
    /// Windows WS_EX_* extended style flags.
    pub extended_style: u32,
    /// Windows show state (0 hidden, 2 minimized, 3 maximized, 5 shown).
    pub show_state: u8,
    /// Window origin relative to the remote desktop.
    pub offset: (i32, i32),
    /// Window width and height including decorations.
    pub size: (u32, u32),
    /// Client-area origin in remote desktop coordinates.
    pub client_offset: (i32, i32),
    /// Client-area width and height.
    pub client_size: (u32, u32),
    /// Difference between window and client coordinates.
    pub client_delta: (i32, i32),
    /// Origin of visibility rectangles in the remote desktop.
    pub visible_offset: (i32, i32),
    /// Transparent hit-test margins, ordered left, top, right, bottom.
    pub resize_margins: [u32; 4],
    /// Window shape rectangles relative to its origin.
    pub window_rects: Vec<Rect>,
    /// None until the first visibility update; Some(empty) means fully obscured.
    pub visibility: Option<Vec<Rect>>,
}

impl RemoteWindow {
    /// Whether the server requests a non-minimized visible window.
    pub fn is_visible(&self) -> bool {
        self.show_state != 0 && self.show_state != 2 && self.size.0 > 0 && self.size.1 > 0
    }

    /// Window rectangle in remote desktop coordinates.
    pub fn bounds(&self) -> Rect {
        Rect {
            left: self.offset.0,
            top: self.offset.1,
            right: self.offset.0.saturating_add(self.size.0 as i32),
            bottom: self.offset.1.saturating_add(self.size.1 as i32),
        }
    }
}

/// Window-manager actions defined by MS-RDPERP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum SystemCommand {
    /// Minimize the application.
    Minimize = 0xf020,
    /// Maximize within the remote workspace.
    Maximize = 0xf030,
    /// Request application closure; the application may ask to save.
    Close = 0xf060,
    /// Restore the previous size.
    Restore = 0xf120,
}

/// Local window-manager operations sent over RAIL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteAppCommand {
    /// Notify the server of keyboard focus.
    Activate {
        /// Target window identifier.
        window_id: u32,
        /// True to activate, false to deactivate.
        enabled: bool,
    },
    /// Request a standard window action.
    System {
        /// Target window identifier.
        window_id: u32,
        /// Requested action.
        command: SystemCommand,
    },
    /// Change the remote window's position or size.
    Move {
        /// Target window identifier.
        window_id: u32,
        /// New outer bounds; all edges must fit signed 16-bit coordinates.
        bounds: Rect,
    },
}

/// Remote application lifecycle and window-manager notifications.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteAppEvent {
    /// Zero result means launch succeeded; nonzero is TS_RAIL_EXEC_RESULT.
    ExecuteResult {
        /// Protocol result code.
        result: u16,
        /// Additional Windows error status.
        status: u32,
    },
    /// Complete merged state of a created or changed window.
    Window(RemoteWindow),
    /// A window was destroyed.
    Deleted(u32),
    /// The active window or server stacking order changed.
    Desktop {
        /// New active window, if present in this update.
        active_window: Option<u32>,
        /// Top-to-bottom stacking order, if present in this update.
        z_order: Option<Vec<u32>>,
    },
    /// The server switched to a desktop that cannot be remoted as applications.
    Suspended,
    /// Start/end of a server-requested native move or resize.
    LocalMoveSize {
        /// Target window identifier.
        window_id: u32,
        /// Whether the operation starts (true) or ends (false).
        start: bool,
        /// RAIL_WMSZ type: edges 1..8, mouse move 9, keyboard move/size 10..11.
        kind: u16,
        /// Server-reported position, in desktop pixels.
        position: (i16, i16),
    },
    /// Server limits for interactive resizing.
    MinMax {
        /// Target window identifier.
        window_id: u32,
        /// Minimum outer window dimensions.
        min: (u16, u16),
        /// Maximum outer window dimensions.
        max: (u16, u16),
    },
}

pub(crate) fn unicode(reader: &mut crate::wire::Reader<'_>) -> Result<String> {
    let length = usize::from(reader.u16_le()?);
    if length % 2 != 0 || length > 16384 {
        return Err(Error::Invalid("RemoteApp Unicode string length"));
    }
    let units: Vec<_> = reader
        .take(length)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    String::from_utf16(&units).map_err(|_| Error::Invalid("RemoteApp Unicode string"))
}
