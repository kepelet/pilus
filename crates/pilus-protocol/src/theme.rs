//! Supporting wire types: colors, sounds, and the optional shell geometry a
//! machine-aware client negotiates with the server.

use serde::{Deserialize, Serialize};

/// The foreground, background, and 16-entry ANSI palette a display client
/// reports back so the server can theme its own rendering to match.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalColors {
    /// Foreground as linear-free sRGB bytes.
    pub fg: [u8; 3],
    /// Background as sRGB bytes.
    pub bg: [u8; 3],
    /// The terminal's ANSI palette, indexed 0-15.
    pub palette: [[u8; 3]; 16],
}

/// Which notification an agent state change should sound.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SoundCue {
    /// A completed turn.
    Done,
    /// An agent waiting on the user.
    Blocked,
}

/// Which synthesized cue family to play.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SoundStyle {
    /// Square-wave chiptune jingle.
    Retro,
    /// Triangle-wave soft chime.
    Soft,
    /// Short pulse blips.
    Pulse,
}

/// A complete notification cue request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SoundSignal {
    /// The event to sound.
    pub cue: SoundCue,
    /// The cue family to play.
    pub style: SoundStyle,
}

/// Which sidebar a dock lives in.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// The left sidebar.
    Left,
    /// The right sidebar.
    Right,
}

/// One sidebar's persisted layout: visibility, width, and ordered dock ids.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SideConfig {
    /// Whether the sidebar is painted.
    pub visible: bool,
    /// The sidebar's width in cells.
    pub width: u16,
    /// Ordered dock identifiers on this side.
    pub docks: Vec<String>,
    /// Relative height share of each dock, parallel to `docks`.
    pub dock_weights: Vec<u16>,
}

/// The left and right sidebar configuration.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SidebarsConfig {
    /// Left sidebar.
    pub left: SideConfig,
    /// Right sidebar.
    pub right: SideConfig,
    /// Last explicit placement of the file dock, retained while hidden.
    pub files_side: Option<Side>,
}

/// One rectangle on the server's grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellDockBlock {
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
}

/// The draggable edge of the workspaces dock.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellResize {
    /// Whether the drag edge is on the left.
    pub left: bool,
    /// Current divider column.
    pub column: u16,
    /// Top row of the drag handle.
    pub top: u16,
    /// Bottom row of the drag handle.
    pub bottom: u16,
    /// Minimum divider column.
    pub origin: u16,
    /// Maximum divider column.
    pub maximum: u16,
}

/// Theme projection the server sends so a thin client can paint the owner-local
/// machine form without receiving any profile data.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MachineFormTheme {
    /// Base surface color.
    pub surface: u32,
    /// Border color.
    pub border: u32,
    /// Primary text color.
    pub text: u32,
    /// Muted text color.
    pub subtext0: u32,
    /// Slightly brighter muted text color.
    pub subtext1: u32,
    /// Accent color.
    pub accent: u32,
    /// Text color drawn on the accent.
    pub accent_text: u32,
    /// Divider color.
    pub divider: u32,
    /// Rule color.
    pub rule: u32,
    /// Error color.
    pub error: u32,
}

/// Exact client-owned workspaces dock geometry for the current viewport.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellDockRect {
    /// Draggable edge, when the dock is resizable.
    pub resize: Option<ShellResize>,
    /// Complete chrome interval reserved for the session control.
    pub session_slot: Option<ShellDockBlock>,
    /// Exact named-session button rectangle.
    pub session_button: Option<ShellDockBlock>,
    /// Server-owned popup rectangle overlapping the client shell.
    pub overlay: Option<ShellDockBlock>,
    /// Whether the server's overlay dims content behind it.
    pub overlay_dims_background: bool,
    /// Whether a workspace currently has keyboard focus.
    pub workspace_focused: bool,
    /// Whether the workspace picker is open as a modal.
    pub workspace_modal: bool,
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
    /// Whether workspace paths are shown.
    pub show_paths: bool,
    /// Normal-row foreground color.
    pub normal_fg: u32,
    /// Secondary text color.
    pub secondary_fg: u32,
    /// Active-row foreground color.
    pub active_fg: u32,
    /// Active-row secondary text color.
    pub active_secondary_fg: u32,
    /// Active-row background color.
    pub active_bg: u32,
    /// Branch-label color.
    pub branch_fg: u32,
    /// Theme for the owner-local machine form.
    pub chrome: MachineFormTheme,
}

/// Bounded workspace metadata used to keep local workspaces reachable while a
/// remote surface is visible.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellWorkspace {
    /// State dot glyph.
    pub dot: String,
    /// State dot color.
    pub dot_color: u32,
    /// Stable workspace identifier.
    pub id: String,
    /// Display index.
    pub index: u16,
    /// Workspace name.
    pub name: String,
    /// Working directory.
    pub cwd: String,
    /// Current git branch, when known.
    pub branch: Option<String>,
    /// Whether this workspace is active in the server.
    pub active: bool,
    /// Whether this workspace is selected in the dock.
    pub selected: bool,
    /// Whether this workspace is nested under another.
    pub nested: bool,
}

/// Client-advertised capability flags for the shell dock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellDockLayout {
    /// Fixed width the client reserves for the workspaces dock, if any.
    pub workspace_width: Option<u16>,
    /// Whether the client owns the workspaces dock.
    pub owns_workspaces: bool,
    /// Whether the client composes the named-session chrome.
    pub owns_session_chrome: bool,
    /// Viewport height in rows.
    pub rows: u16,
    /// Whether the dock is leading.
    pub leading: bool,
    /// Whether workspace rows are indented.
    pub indent_workspaces: bool,
}

/// Client-owned dock arrangement, revisioned so late updates cannot clobber a
/// newer layout.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellSidebars {
    /// Monotonic revision.
    pub revision: u64,
    /// The layout itself.
    pub layout: SidebarsConfig,
    /// Whether workspace paths are shown in the combined workspace tree.
    pub workspace_paths: bool,
}
