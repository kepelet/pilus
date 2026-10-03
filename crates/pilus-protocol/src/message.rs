//! The two top-level message enums.
//!
//! Variant order is wire-significant: bincode encodes an enum as its variant
//! index followed by the payload. Append only.

use crossterm::event::{KeyEvent, MouseEvent};
use serde::{Deserialize, Serialize};

use crate::frame::{FrameData, FrameDiff};
use crate::theme::{
    MachineFormTheme, ShellDockBlock, ShellDockLayout, ShellDockRect, ShellSidebars,
    ShellWorkspace, SoundSignal, TerminalColors,
};

/// Messages a display client sends to the server.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ClientMessage {
    /// First frame on every connection. Keep the shape frozen: the server
    /// decodes it before any version agreement exists.
    Hello {
        /// Client protocol revision.
        version: u32,
        /// Initial viewport width in cells.
        cols: u16,
        /// Initial viewport height in cells.
        rows: u16,
    },
    /// A key press, release, or repeat.
    Key(KeyEvent),
    /// A mouse press, release, drag, move, or scroll.
    Mouse(MouseEvent),
    /// Bracketed paste text.
    Paste(String),
    /// PNG bytes from the client's local clipboard.
    ClipboardImage(Vec<u8>),
    /// Viewport and cell-size change.
    Resize {
        /// New width in cells.
        cols: u16,
        /// New height in cells.
        rows: u16,
        /// Width of one cell in pixels.
        cell_width_px: u16,
        /// Height of one cell in pixels.
        cell_height_px: u16,
    },
    /// Negotiate whether this display owns a visible surface.
    SurfaceInterest(SurfaceInterest),
    /// Advertise client shell-dock capabilities.
    ShellDockLayout(ShellDockLayout),
    /// Client-owned sidebar arrangement.
    ShellSidebars(ShellSidebars),
    /// Focus a workspace before the client switches its visible surface.
    ShellWorkspaceFocus {
        /// Workspace to focus.
        workspace_id: String,
    },
    /// Ask the selected server to reopen its workspace picker.
    OpenWorkspacePicker,
    /// Detach without stopping the server.
    Detach,
    /// Answer a server terminal-color probe; `None` means the probe found no
    /// usable colors.
    TerminalColors(Option<TerminalColors>),
    /// Cell geometry reported after the handshake.
    CellPixels {
        /// Width of one cell in pixels.
        cell_width_px: u16,
        /// Height of one cell in pixels.
        cell_height_px: u16,
    },
    /// Request a fresh candidate frame, tagged so a stale one cannot commit.
    PrepareSurface {
        /// Opaque ticket echoed by the server.
        ticket: u64,
        /// Requested width in cells.
        cols: u16,
        /// Requested height in cells.
        rows: u16,
    },
    /// Open the native workspace menu at a point.
    ShellWorkspaceMenu {
        /// Workspace to anchor the menu to.
        workspace_id: String,
        /// Anchor column.
        col: u16,
        /// Anchor row.
        row: u16,
    },
    /// Lightweight liveness check.
    HealthCheck {
        /// Nonce echoed in the reply.
        nonce: u64,
    },
    /// The client installed the clipboard text a `ClipboardTracked` carried.
    /// The server uses it to confirm the foreground copy with a success toast.
    ClipboardSucceeded {
        /// Receipt echoed from the `ClipboardTracked` message.
        receipt: u64,
    },
}

/// How much of a visible surface a display client wants.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceInterest {
    /// Connected but receiving no frames.
    Suspended,
    /// Receiving candidate frames but not owning input.
    Prepared,
    /// Owning frames, cursor, and input.
    Active,
}

/// Messages the server sends to a display client.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ServerMessage {
    /// First frame on every connection, decoded before version agreement.
    Welcome {
        /// Server protocol revision.
        version: u32,
        /// Set when the handshake is rejected.
        error: Option<String>,
    },
    /// Server-owned sidebar arrangement superseding the client's.
    ShellSidebars(ShellSidebars),
    /// A full frame, sent first, on resize, and to a freshly-attached client.
    Frame(FrameData),
    /// Only the cells that changed since the previous frame.
    FrameDiff(FrameDiff),
    /// Ring the bell and raise a desktop notification.
    Notify(String),
    /// Play a notification cue.
    Sound(SoundSignal),
    /// Set the client's system clipboard.
    Clipboard(String),
    /// Open a URL in the client's browser.
    OpenUrl(String),
    /// Reconnect to another validated named session.
    SwitchSession {
        /// Target session name.
        name: String,
    },
    /// Detach (the server keeps running).
    Detach,
    /// The server is shutting down.
    ServerShutdown {
        /// Human-readable reason.
        reason: String,
    },
    /// Completes negotiation after `Welcome`.
    Ready {
        /// Whether the client should probe and report terminal colors.
        probe_terminal: bool,
    },
    /// Exact client-owned workspaces dock for the current viewport.
    ShellDock(Option<ShellDockRect>),
    /// Bounded owner-local workspace metadata.
    ShellWorkspaces(Vec<ShellWorkspace>),
    /// Ask a thin client to open its owner-local machine selector.
    OpenMachineSelector,
    /// Ask a thin client to open its owner-local machine form.
    OpenMachineCreate {
        /// Theme for the created modal.
        theme: MachineFormTheme,
        /// Modal rectangle.
        modal: ShellDockBlock,
    },
    /// A candidate frame tied to a `PrepareSurface` ticket.
    PreparedFrame {
        /// The requesting ticket.
        ticket: u64,
        /// The candidate frame.
        frame: FrameData,
    },
    /// Reply to a `HealthCheck`.
    HealthCheck {
        /// Echoed nonce.
        nonce: u64,
    },
    /// Opaque identity for the running server process.
    EndpointIdentity {
        /// Server boot identifier.
        boot_id: u64,
        /// Selected session name.
        session: String,
    },
    /// The owner-local machine catalog advanced to a new revision.
    MachineCatalogChanged {
        /// New revision.
        revision: u64,
    },
    /// A foreground clipboard copy that may earn a success toast. The client
    /// installs `text` and acknowledges with `ClipboardSucceeded { receipt }`.
    ClipboardTracked {
        /// Text to install on the client's clipboard.
        text: String,
        /// Opaque token echoed in the acknowledgement.
        receipt: u64,
    },
}
