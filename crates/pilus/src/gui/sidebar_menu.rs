//! The native right-click context menus for the sidebar's WORKSPACES and
//! AGENTS rows, for the terminal grid, and for the Files tab's tree.
//!
//! Luvus opens a context menu on a WORKSPACES row (close, rename, path
//! visibility, copy path/branch, new worktree, and the git/task/mission
//! boards) and on an AGENTS row (rename, close, path visibility, and the
//! dock's workspace scope). `pilus` draws those menus natively; the items a
//! client can reach ride the control API (`workspace.*`, `agent.name`,
//! `pane.close`, `worktree.create`), and the purely presentational ones (path
//! toggle, scope) are client state, exactly as they are in the daemon's
//! own TUI. The terminal's menu replaces the one the daemon draws into the grid
//! on a right click: it offers the clipboard and the pane/tab actions a client
//! can reach, so the click never reaches the daemon. The Files tab's menu
//! creates, renames, and deletes the entries under the daemon's tree, which
//! the client does over disk or SSH since the daemon never writes. This module owns the
//! menu's state machine and item order; the window renders it and performs
//! the actions, so nothing here knows GPUI.

/// The sidebar row an open menu is anchored to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuTarget {
    /// A WORKSPACES row. `index` is the daemon's workspace index, which
    /// `worktree.create` reads to resolve the right-clicked node's repo.
    Workspace {
        /// Endpoint key (`local` or a machine id).
        endpoint: String,
        /// Stable workspace id accepted by `workspace.*`.
        workspace_id: String,
        /// Storage index accepted by `worktree.create`'s `workspace` param.
        index: u16,
        /// Display label, for status feedback.
        name: String,
        /// Shortened path, for Copy Path.
        cwd: String,
        /// Current branch, for Copy Branch.
        branch: Option<String>,
    },
    /// A live AGENTS row, addressed by its pane id.
    Agent {
        /// Endpoint key (`local` or a machine id).
        endpoint: String,
        /// Live pane id accepted by `agent.name` and `pane.close`.
        pane: String,
        /// The label the row is currently showing, pre-filling a rename.
        name: String,
    },
    /// The terminal grid. `selection` is whether text is highlighted, which
    /// is what Copy needs, so the row only appears when it can work.
    Terminal { selection: bool },
    /// A row of the Files tab's tree, or its blank area (`root`), which acts
    /// on the tree's own folder.
    File {
        /// Endpoint key (`local` or a machine id).
        endpoint: String,
        /// Absolute path on that endpoint's machine.
        path: String,
        /// The entry is a folder: new entries go inside it, not beside it.
        dir: bool,
        /// The tree's root, which can be added to but not renamed or deleted.
        root: bool,
    },
}

/// One row of the open context menu, in render order.
///
/// `Divider` is a non-interactive separator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// Close the workspace, or close the agent's pane.
    Close,
    /// Rename the workspace's label, or give the agent's pane a live alias.
    Rename,
    /// Toggle the second (path) line of every row in the owning section.
    TogglePaths,
    /// The parent row of the copy submenu.
    QuickActions,
    CopyPath,
    CopyBranch,
    /// Create a linked worktree of the row's repo.
    NewWorktree,
    /// Delete a linked worktree and its files. Destructive; only offered on
    /// rows the daemon's `worktree.list` confirms as linked checkouts.
    DeleteWorktree,
    /// Open the daemon's Git tab for this workspace.
    OpenGit,
    /// Open the daemon's task board for this workspace.
    OpenBoard,
    /// Open Mission Control for this workspace.
    OpenMission,
    /// Show only the active workspace's agents in the AGENTS dock.
    ToggleScope,
    /// Copy the terminal's highlighted text.
    CopySelection,
    /// Create an empty file in the folder the menu is on.
    NewFile,
    /// Create a folder in the folder the menu is on.
    NewFolder,
    /// Delete a file, or a folder and its contents. Destructive.
    Delete,
    /// Send the clipboard to the focused pane.
    Paste,
    /// Highlight the whole visible grid.
    SelectAll,
    /// Split the focused pane side by side.
    SplitRight,
    /// Split the focused pane stacked.
    SplitDown,
    NewTab,
    /// Close the active tab. Destructive: it ends every pane in it.
    CloseTab,
    Divider,
}

impl Item {
    /// Whether an item draws as a selection row rather than a label hairline.
    pub fn selectable(self) -> bool {
        self != Self::Divider
    }

    /// Whether the row is destructive (Close, Delete Worktree, Close Tab), painted in
    /// the alert color.
    pub fn destructive(self) -> bool {
        matches!(
            self,
            Self::Close | Self::DeleteWorktree | Self::CloseTab | Self::Delete
        )
    }

    /// The label, resolved against the state the row toggles: `paths_visible`
    /// flips Show Path/Hide Path, and `scoped` flips the AGENTS scope row.
    pub fn label(self, paths_visible: bool, scoped: bool) -> &'static str {
        match self {
            Self::Close => "Close",
            Self::Rename => "Rename",
            Self::TogglePaths => {
                if paths_visible {
                    "Hide Path"
                } else {
                    "Show Path"
                }
            }
            Self::QuickActions => "Quick Actions",
            Self::CopyPath => "Copy Path",
            Self::CopyBranch => "Copy Branch",
            Self::NewWorktree => "New Git Worktree",
            Self::DeleteWorktree => "Delete Worktree",
            Self::OpenGit => "Open Git Tab",
            Self::OpenBoard => "Open Task Board",
            Self::OpenMission => "Open Mission Control",
            Self::ToggleScope => {
                if scoped {
                    "Show All Workspaces"
                } else {
                    "Show Workspace Only"
                }
            }
            Self::NewFile => "New File",
            Self::NewFolder => "New Folder",
            Self::Delete => "Delete",
            Self::CopySelection => "Copy",
            Self::Paste => "Paste",
            Self::SelectAll => "Select All",
            Self::SplitRight => "Split Right",
            Self::SplitDown => "Split Down",
            Self::NewTab => "New Tab",
            Self::CloseTab => "Close Tab",
            Self::Divider => "",
        }
    }
}

/// The open context menu: target, anchor point in window pixels, and the
/// keyboard cursor. Mouse users get no persistent selection, like every
/// other native list here; `selected` is only painted when the keyboard is
/// driving. `quick_open` swaps the rows for the Quick Actions submenu.
pub struct SidebarMenu {
    pub target: MenuTarget,
    /// Anchor column in window pixels; the renderer clamps the panel to fit.
    pub column: f32,
    /// Anchor row in window pixels.
    pub row: f32,
    pub selected: Option<usize>,
    pub quick_open: bool,
}

impl SidebarMenu {
    pub fn new(target: MenuTarget, column: f32, row: f32) -> Self {
        Self {
            target,
            column,
            row,
            selected: None,
            quick_open: false,
        }
    }

    /// The submenu the open menu shows: the Quick Actions copy rows when that
    /// submenu is up, otherwise the top-level rows.
    ///
    /// A WORKSPACES target's git rows appear when its branch says it is a repo, and the deleted-worktree row
    /// only when `linked` says the daemon's `worktree.list` found this row's
    /// checkout as a linked worktree.
    pub fn rows(&self, linked: bool) -> Vec<Item> {
        if self.quick_open {
            return self.quick_rows();
        }
        match &self.target {
            MenuTarget::Workspace { branch, .. } => {
                workspace_rows(branch.as_deref().is_some(), linked)
            }
            MenuTarget::Agent { .. } => agent_rows(),
            MenuTarget::Terminal { selection } => terminal_rows(*selection),
            MenuTarget::File { root, .. } => file_rows(*root),
        }
    }

    /// The rows of the Quick Actions submenu, below the top level.
    fn quick_rows(&self) -> Vec<Item> {
        let mut items = vec![Item::CopyPath];
        if let MenuTarget::Workspace { branch, .. } = &self.target
            && branch.as_deref().is_some_and(|branch| !branch.is_empty())
        {
            items.push(Item::CopyBranch);
        }
        items
    }

    /// The first selectable row index at or after `from`, wrapping, or `None`
    /// when nothing is selectable. Divider rows are skipped.
    pub fn next_selectable(&self, rows: &[Item], from: usize, dir: isize) -> Option<usize> {
        if rows.iter().all(|item| !item.selectable()) {
            return None;
        }
        let len = rows.len();
        let mut cursor = from;
        for _ in 0..len {
            cursor = (cursor as isize + dir).rem_euclid(len as isize) as usize;
            if rows[cursor].selectable() {
                return Some(cursor);
            }
        }
        None
    }
}

/// WORKSPACES top-level rows, in the daemon's render order: node actions,
/// the copy submenu, worktrees when the row is a git repo, then the board
/// actions below a divider. The path toggle's label is applied at render.
fn workspace_rows(is_repo: bool, linked: bool) -> Vec<Item> {
    let mut items = vec![
        Item::Close,
        Item::Rename,
        Item::TogglePaths,
        Item::QuickActions,
    ];
    // A linked checkout can be deleted; a main checkout or plain folder
    // cannot, so the row only appears when `worktree.list` confirmed it.
    if linked {
        items.push(Item::DeleteWorktree);
    }
    if is_repo {
        items.push(Item::NewWorktree);
    }
    items.push(Item::Divider);
    if is_repo {
        items.push(Item::OpenGit);
    }
    items.push(Item::OpenBoard);
    items.push(Item::OpenMission);
    items
}

/// A live agent's rows: pane actions, path toggle, then the dock's
/// workspace scope below a divider.
fn agent_rows() -> Vec<Item> {
    vec![
        Item::Rename,
        Item::Close,
        Item::TogglePaths,
        Item::Divider,
        Item::ToggleScope,
    ]
}

/// The terminal's rows: the clipboard first, then the pane and tab actions
/// below a divider. Copy needs a highlight, so it is left out without one
/// rather than shown dead.
fn terminal_rows(selection: bool) -> Vec<Item> {
    let mut items = Vec::new();
    if selection {
        items.push(Item::CopySelection);
    }
    items.extend([
        Item::Paste,
        Item::SelectAll,
        Item::Divider,
        Item::SplitRight,
        Item::SplitDown,
        Item::NewTab,
        Item::Divider,
        Item::CloseTab,
    ]);
    items
}

/// A tree entry's rows: where to add, what to do to the entry, then the one
/// destructive row alone at the end. The root keeps only the rows that make
/// sense for the folder the tree is showing.
fn file_rows(root: bool) -> Vec<Item> {
    let mut items = vec![Item::NewFile, Item::NewFolder, Item::Divider];
    if !root {
        items.push(Item::Rename);
    }
    items.push(Item::CopyPath);
    if !root {
        items.extend([Item::Divider, Item::Delete]);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_rows_follow_the_daemons_order() {
        let rows = workspace_rows(true, true);
        assert_eq!(
            rows,
            vec![
                Item::Close,
                Item::Rename,
                Item::TogglePaths,
                Item::QuickActions,
                Item::DeleteWorktree,
                Item::NewWorktree,
                Item::Divider,
                Item::OpenGit,
                Item::OpenBoard,
                Item::OpenMission,
            ]
        );
        // A non-repo row drops the git rows; the boards stay. The delete row
        // needs a confirmed linked checkout, not just any repo row.
        let rows = workspace_rows(false, false);
        assert!(!rows.contains(&Item::DeleteWorktree));
        assert!(!rows.contains(&Item::NewWorktree));
        assert!(!rows.contains(&Item::OpenGit));
        let main = workspace_rows(true, false);
        assert!(!main.contains(&Item::DeleteWorktree));
        let linked = workspace_rows(true, true);
        assert!(linked.contains(&Item::DeleteWorktree));
        assert_eq!(rows[rows.len() - 2], Item::OpenBoard);
        assert_eq!(rows[rows.len() - 1], Item::OpenMission);
    }

    #[test]
    fn path_label_names_the_action() {
        assert_eq!(Item::TogglePaths.label(true, false), "Hide Path");
        assert_eq!(Item::TogglePaths.label(false, false), "Show Path");
    }

    #[test]
    fn quick_actions_offer_copy_branch_only_when_there_is_one() {
        let target = MenuTarget::Workspace {
            endpoint: "local".into(),
            workspace_id: "w1".into(),
            index: 0,
            name: "api".into(),
            cwd: "/repo/api".into(),
            branch: Some("main".into()),
        };
        let menu = SidebarMenu::new(target, 0.0, 0.0);
        assert_eq!(menu.quick_rows(), vec![Item::CopyPath, Item::CopyBranch]);
        assert_eq!(menu_without_branch().quick_rows(), vec![Item::CopyPath]);
    }

    #[test]
    fn agent_rows_carry_scope_below_the_divider() {
        assert_eq!(
            agent_rows(),
            vec![
                Item::Rename,
                Item::Close,
                Item::TogglePaths,
                Item::Divider,
                Item::ToggleScope,
            ]
        );
        assert_eq!(Item::ToggleScope.label(true, true), "Show All Workspaces");
        assert_eq!(Item::ToggleScope.label(false, false), "Show Workspace Only");
    }

    #[test]
    fn terminal_rows_offer_copy_only_over_a_selection() {
        let with = terminal_rows(true);
        assert_eq!(with[0], Item::CopySelection);
        let without = terminal_rows(false);
        assert!(!without.contains(&Item::CopySelection));
        assert_eq!(without[0], Item::Paste);
        // Closing the tab sits alone at the end, away from the safe rows.
        assert_eq!(without.last(), Some(&Item::CloseTab));
        assert_eq!(without[without.len() - 2], Item::Divider);
        assert!(Item::CloseTab.destructive());
    }

    #[test]
    fn the_terminal_target_builds_its_own_rows() {
        let menu = SidebarMenu::new(MenuTarget::Terminal { selection: false }, 0.0, 0.0);
        assert_eq!(menu.rows(true), terminal_rows(false));
    }

    #[test]
    fn the_trees_root_cannot_be_renamed_or_deleted() {
        let root = file_rows(true);
        assert_eq!(
            root,
            vec![
                Item::NewFile,
                Item::NewFolder,
                Item::Divider,
                Item::CopyPath
            ]
        );
        let entry = file_rows(false);
        assert!(entry.contains(&Item::Rename));
        assert_eq!(entry.last(), Some(&Item::Delete));
        assert_eq!(entry[entry.len() - 2], Item::Divider);
        assert!(Item::Delete.destructive());
    }

    #[test]
    fn dividers_are_not_selectable() {
        assert!(!Item::Divider.selectable());
        assert!(Item::Close.selectable());
    }

    #[test]
    fn keyboard_cursor_skips_dividers_and_wraps() {
        let rows = vec![Item::Rename, Item::Close, Item::Divider, Item::ToggleScope];
        let target = MenuTarget::Agent {
            endpoint: "local".into(),
            pane: "2".into(),
            name: "auth-fix".into(),
        };
        let menu = SidebarMenu::new(target, 0.0, 0.0);
        // The divider sits between rows 1 and 3; moving forward from 1 lands
        // on the scope row, never the divider.
        assert_eq!(menu.next_selectable(&rows, 1, 1), Some(3));
        // Moving backward from 0 wraps to the end of the list.
        assert_eq!(menu.next_selectable(&rows, 0, -1), Some(3));
    }

    fn menu_without_branch() -> SidebarMenu {
        SidebarMenu::new(
            MenuTarget::Workspace {
                endpoint: "local".into(),
                workspace_id: "w2".into(),
                index: 1,
                name: "web".into(),
                cwd: "/repo/web".into(),
                branch: None,
            },
            0.0,
            0.0,
        )
    }
}
