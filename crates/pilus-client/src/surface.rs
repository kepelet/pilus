//! The live surface: the cell grid plus the out-of-band server requests a
//! window reacts to (notifications, sounds, clipboard, link opens).

use pilus_protocol::{
    FrameData, ServerMessage, ShellDockRect, ShellSidebars, ShellWorkspace, SoundSignal,
};

/// What changed after applying one server message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceChange {
    /// Nothing a window needs to repaint.
    None,
    /// The grid changed and should be repainted.
    Repaint,
    /// The grid changed size, so layout must be recomputed.
    Resized,
}

/// The client-side projection of the server's rendered surface.
#[derive(Default)]
pub struct Surface {
    frame: Option<FrameData>,
    needs_full_frame: bool,
    notify: Option<String>,
    sounds: Vec<SoundSignal>,
    clipboard: Option<String>,
    open_url: Option<String>,
    workspaces: Vec<ShellWorkspace>,
    shell_dock: Option<ShellDockRect>,
    shell_sidebars: Option<ShellSidebars>,
}

impl Surface {
    /// An empty surface with no frame yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current frame, if one has arrived.
    pub fn frame(&self) -> Option<&FrameData> {
        self.frame.as_ref()
    }

    /// Whether a diff was dropped and a full frame must be requested.
    pub fn needs_full_frame(&self) -> bool {
        self.needs_full_frame
    }

    /// Clear the resync flag after a full frame has been requested.
    pub fn clear_resync(&mut self) {
        self.needs_full_frame = false;
    }

    /// Take the most recent notification text.
    pub fn take_notify(&mut self) -> Option<String> {
        self.notify.take()
    }

    /// Drain queued sound cues.
    pub fn take_sounds(&mut self) -> Vec<SoundSignal> {
        std::mem::take(&mut self.sounds)
    }

    /// Take a pending clipboard assignment.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    /// Take a pending URL open request.
    pub fn take_open_url(&mut self) -> Option<String> {
        self.open_url.take()
    }

    /// The workspace catalog the daemon projected, when this client owns the
    /// workspaces dock.
    pub fn workspaces(&self) -> &[ShellWorkspace] {
        &self.workspaces
    }

    /// The exact rectangle the daemon blanked for the client-owned workspaces
    /// dock, in cell coordinates.
    pub fn shell_dock(&self) -> Option<ShellDockRect> {
        self.shell_dock
    }

    /// The daemon's current sidebar layout, which a client may supersede to
    /// own the whole sidebar rather than only the workspaces dock.
    pub fn shell_sidebars(&self) -> Option<&ShellSidebars> {
        self.shell_sidebars.as_ref()
    }

    /// Fold one server message into the surface.
    pub fn apply(&mut self, message: &ServerMessage) -> SurfaceChange {
        match message {
            ServerMessage::Frame(frame) => {
                self.needs_full_frame = false;
                self.frame = Some(frame.clone());
                SurfaceChange::Resized
            }
            ServerMessage::PreparedFrame { frame, .. } => {
                self.needs_full_frame = false;
                self.frame = Some(frame.clone());
                SurfaceChange::Resized
            }
            ServerMessage::FrameDiff(diff) => {
                let applied = match self.frame.as_mut() {
                    Some(frame) => frame.apply_diff(diff).is_ok(),
                    None => false,
                };
                if applied {
                    SurfaceChange::Repaint
                } else {
                    self.needs_full_frame = true;
                    SurfaceChange::None
                }
            }
            ServerMessage::Notify(text) => {
                self.notify = Some(text.clone());
                SurfaceChange::None
            }
            ServerMessage::Sound(signal) => {
                self.sounds.push(*signal);
                SurfaceChange::None
            }
            ServerMessage::Clipboard(text) => {
                self.clipboard = Some(text.clone());
                SurfaceChange::None
            }
            ServerMessage::OpenUrl(url) => {
                self.open_url = Some(url.clone());
                SurfaceChange::None
            }
            ServerMessage::ShellWorkspaces(workspaces) => {
                self.workspaces.clone_from(workspaces);
                SurfaceChange::Repaint
            }
            ServerMessage::ShellDock(rect) => {
                self.shell_dock = *rect;
                SurfaceChange::Repaint
            }
            ServerMessage::ShellSidebars(sidebars) => {
                self.shell_sidebars = Some(sidebars.clone());
                SurfaceChange::Repaint
            }
            _ => SurfaceChange::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilus_protocol::{CellData, DiffRun, FrameDiff};

    fn full(width: u16, height: u16) -> FrameData {
        FrameData {
            width,
            height,
            cells: vec![CellData::blank(0x00ff_ffff, 0); usize::from(width) * usize::from(height)],
            cursor: None,
            cursor_visible: false,
        }
    }

    #[test]
    fn frame_then_diff_repaints() {
        let mut surface = Surface::new();
        assert_eq!(
            surface.apply(&ServerMessage::Frame(full(2, 1))),
            SurfaceChange::Resized
        );
        let diff = ServerMessage::FrameDiff(FrameDiff {
            width: 2,
            height: 1,
            runs: vec![DiffRun {
                start: 0,
                fg: 0x00ff_0000,
                bg: 0,
                mods: 0,
                symbols: vec!["h".into(), "i".into()],
            }],
            cursor: Some((1, 0)),
            cursor_visible: true,
        });
        assert_eq!(surface.apply(&diff), SurfaceChange::Repaint);
        assert_eq!(surface.frame().expect("frame").cells[0].symbol, "h");
    }

    #[test]
    fn clipboard_is_queued_for_the_window_to_install() {
        // The daemon copies through the client when it cannot reach the host
        // clipboard, so the text must survive one take for the window to write
        // it and then be cleared.
        let mut surface = Surface::new();
        assert_eq!(
            surface.apply(&ServerMessage::Clipboard("hi".into())),
            SurfaceChange::None
        );
        assert_eq!(surface.take_clipboard().as_deref(), Some("hi"));
        assert!(surface.take_clipboard().is_none());
    }

    #[test]
    fn diff_without_a_frame_requests_resync() {
        let mut surface = Surface::new();
        let diff = ServerMessage::FrameDiff(FrameDiff {
            width: 2,
            height: 1,
            runs: Vec::new(),
            cursor: None,
            cursor_visible: false,
        });
        assert_eq!(surface.apply(&diff), SurfaceChange::None);
        assert!(surface.needs_full_frame());
    }
}
