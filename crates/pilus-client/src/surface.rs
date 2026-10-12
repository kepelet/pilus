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
    /// A recovery request is already out for the current outage, so a burst
    /// of rejected diffs asks the daemon once rather than once per diff.
    resync_requested: bool,
    notify: Option<String>,
    sounds: Vec<SoundSignal>,
    clipboard: Option<String>,
    /// Receipt to acknowledge once the pending clipboard text is installed.
    clipboard_receipt: Option<u64>,
    open_url: Option<String>,
    workspaces: Vec<ShellWorkspace>,
    /// Whether the daemon has reported a catalog yet, so an empty list can
    /// be told apart from one still in flight.
    workspaces_loaded: bool,
    shell_dock: Option<ShellDockRect>,
    /// The dock in effect when the current frame was produced. The server
    /// sends `ShellDock` before the matching `Frame`, so the live rect can
    /// update while the client still holds the previous frame; the skip that
    /// lines the grid up with the dock must follow the frame, not the control
    /// message, or a collapse paints the old dock's blank cells.
    frame_dock: Option<ShellDockRect>,
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

    /// Whether the caller should ask the daemon for a full frame now.
    ///
    /// True once per outage: after a rejected diff, until a full frame lands.
    /// Later rejected diffs in the same outage stay quiet because a request is
    /// already in flight.
    pub fn take_resync(&mut self) -> bool {
        if self.needs_full_frame && !self.resync_requested {
            self.resync_requested = true;
            return true;
        }
        false
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

    /// Take the receipt that must be acknowledged after the pending clipboard
    /// text is installed. `None` for an untracked `Clipboard` assignment.
    pub fn take_clipboard_receipt(&mut self) -> Option<u64> {
        self.clipboard_receipt.take()
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

    /// Whether a workspace catalog has arrived, even an empty one.
    pub fn workspaces_loaded(&self) -> bool {
        self.workspaces_loaded
    }

    /// Update a workspace's projected label in place.
    ///
    /// A rename reads immediately instead of waiting for the daemon's next
    /// catalog projection; the authoritative refresh will confirm it.
    pub fn rename_workspace(&mut self, workspace_id: &str, name: &str) {
        if let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        {
            workspace.name = name.to_string();
        }
    }

    /// The exact rectangle the daemon blanked for the client-owned workspaces
    /// dock, in cell coordinates.
    pub fn shell_dock(&self) -> Option<ShellDockRect> {
        self.shell_dock
    }

    /// The dock that matches the current frame, for deriving the columns a
    /// client-owned sidebar covers. Updates only when a frame or diff lands.
    pub fn frame_dock(&self) -> Option<ShellDockRect> {
        self.frame_dock
    }

    /// The daemon's current sidebar layout, which a client may supersede to
    /// own the whole sidebar rather than only the workspaces dock.
    pub fn shell_sidebars(&self) -> Option<&ShellSidebars> {
        self.shell_sidebars.as_ref()
    }

    /// Replace the grid with a full frame the daemon sent.
    fn install_frame(&mut self, frame: FrameData) -> SurfaceChange {
        self.needs_full_frame = false;
        self.resync_requested = false;
        self.frame = Some(frame);
        self.frame_dock = self.shell_dock;
        SurfaceChange::Resized
    }

    /// [`Surface::apply`] for a message the caller is done with: a full frame
    /// moves into the surface instead of copying every cell's string.
    pub fn apply_owned(&mut self, message: ServerMessage) -> SurfaceChange {
        match message {
            ServerMessage::Frame(frame) | ServerMessage::PreparedFrame { frame, .. } => {
                self.install_frame(frame)
            }
            other => self.apply(&other),
        }
    }

    /// Fold one server message into the surface.
    pub fn apply(&mut self, message: &ServerMessage) -> SurfaceChange {
        match message {
            ServerMessage::Frame(frame) | ServerMessage::PreparedFrame { frame, .. } => {
                self.install_frame(frame.clone())
            }
            ServerMessage::FrameDiff(diff) => {
                let applied = match self.frame.as_mut() {
                    Some(frame) => frame.apply_diff(diff).is_ok(),
                    None => false,
                };
                if applied {
                    // A dock toggle keeps the grid's size, so the daemon
                    // usually redraws it as a diff, not a full frame. The
                    // diff follows its `ShellDock` like a frame would; a
                    // skip left on the old dock would show the daemon's
                    // own dock columns beside the native sidebar.
                    self.frame_dock = self.shell_dock;
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
            ServerMessage::ClipboardTracked { text, receipt } => {
                self.clipboard = Some(text.clone());
                self.clipboard_receipt = Some(*receipt);
                SurfaceChange::None
            }
            ServerMessage::OpenUrl(url) => {
                self.open_url = Some(url.clone());
                SurfaceChange::None
            }
            ServerMessage::ShellWorkspaces(workspaces) => {
                self.workspaces.clone_from(workspaces);
                self.workspaces_loaded = true;
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
            hyperlinks: Vec::new(),
            cursor: None,
            cursor_visible: false,
        }
    }

    #[test]
    fn an_owned_frame_installs_like_a_borrowed_one() {
        let mut borrowed = Surface::new();
        let mut owned = Surface::new();
        let message = ServerMessage::Frame(full(2, 1));
        assert_eq!(borrowed.apply(&message), SurfaceChange::Resized);
        assert_eq!(owned.apply_owned(message), SurfaceChange::Resized);
        assert_eq!(owned.frame(), borrowed.frame());
        // Everything that is not a frame still takes the borrowed path.
        assert_eq!(
            owned.apply_owned(ServerMessage::Clipboard("hi".into())),
            SurfaceChange::None
        );
        assert_eq!(owned.take_clipboard().as_deref(), Some("hi"));
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
    fn frame_dock_lags_the_control_message_to_match_the_frame() {
        let mut surface = Surface::new();
        let dock = ShellDockRect {
            width: 36,
            ..ShellDockRect::default()
        };
        surface.apply(&ServerMessage::ShellDock(Some(dock)));
        surface.apply(&ServerMessage::Frame(full(2, 1)));
        assert_eq!(surface.frame_dock(), Some(dock));

        // The server drops the dock before the matching frame arrives; the
        // frame on screen still carries the dock, so its skip must not jump.
        surface.apply(&ServerMessage::ShellDock(None));
        assert_eq!(surface.frame_dock(), Some(dock));

        surface.apply(&ServerMessage::Frame(full(2, 1)));
        assert!(surface.frame_dock().is_none());
    }

    #[test]
    fn a_diff_after_the_dock_message_adopts_the_new_dock() {
        let mut surface = Surface::new();
        surface.apply(&ServerMessage::Frame(full(2, 1)));
        assert!(surface.frame_dock().is_none());
        // Showing the sidebar keeps the grid size, so the redraw is a diff.
        let dock = ShellDockRect {
            width: 36,
            ..ShellDockRect::default()
        };
        surface.apply(&ServerMessage::ShellDock(Some(dock)));
        assert!(surface.frame_dock().is_none());
        let diff = ServerMessage::FrameDiff(FrameDiff {
            width: 2,
            height: 1,
            runs: Vec::new(),
            cursor: None,
            cursor_visible: false,
        });
        surface.apply(&diff);
        assert_eq!(surface.frame_dock(), Some(dock));
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
    fn tracked_clipboard_keeps_its_receipt() {
        // A protocol-20 foreground copy must be acknowledged after the window
        // installs it, so the receipt rides beside the text and is taken once.
        let mut surface = Surface::new();
        surface.apply(&ServerMessage::ClipboardTracked {
            text: "hi".into(),
            receipt: 7,
        });
        assert_eq!(surface.take_clipboard().as_deref(), Some("hi"));
        assert_eq!(surface.take_clipboard_receipt(), Some(7));
        assert!(surface.take_clipboard_receipt().is_none());
    }

    #[test]
    fn a_local_rename_updates_the_projected_label() {
        let mut surface = Surface::new();
        let workspace = ShellWorkspace {
            dot: "●".into(),
            dot_color: 0,
            id: "ws1".into(),
            index: 0,
            name: "api".into(),
            cwd: "/repo/api".into(),
            branch: Some("main".into()),
            active: true,
            selected: false,
            nested: false,
        };
        surface.apply(&ServerMessage::ShellWorkspaces(vec![workspace]));
        surface.rename_workspace("ws1", "assets");
        assert_eq!(surface.workspaces()[0].name, "assets");
        // A rename for an id the catalog does not know is a no-op.
        surface.rename_workspace("missing", "nope");
        assert_eq!(surface.workspaces().len(), 1);
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

    #[test]
    fn a_rejected_diff_asks_for_one_full_frame_until_it_lands() {
        let mut surface = Surface::new();
        surface.apply(&ServerMessage::Frame(full(2, 1)));
        assert!(!surface.take_resync());

        // A diff sized for a different grid is rejected and leaves the frame.
        let stale = ServerMessage::FrameDiff(FrameDiff {
            width: 4,
            height: 1,
            runs: Vec::new(),
            cursor: None,
            cursor_visible: false,
        });
        assert_eq!(surface.apply(&stale), SurfaceChange::None);
        assert!(surface.take_resync(), "first rejection asks for recovery");

        // More diffs arrive before the daemon answers: no second request.
        surface.apply(&stale);
        assert!(!surface.take_resync());

        // The full frame ends the outage, and a later rejection asks again.
        surface.apply(&ServerMessage::Frame(full(2, 1)));
        assert!(!surface.needs_full_frame());
        surface.apply(&stale);
        assert!(surface.take_resync());
    }
}
