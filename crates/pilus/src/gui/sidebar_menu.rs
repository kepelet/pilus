//! The right-click context menus for the sidebar's WORKSPACES and AGENTS
//! rows, for the terminal grid, and for the Files tab's tree.
//!
//! Luvus opens a context menu on a WORKSPACES row (close, rename, path
//! visibility, copy path/branch, new worktree, and the git/task/mission
//! boards) and on an AGENTS row (rename, close, path visibility, and the
//! dock's workspace scope). `pilus` shows those menus as macOS context menus;
//! the items a client can reach ride the control API (`workspace.*`,
//! `agent.name`, `pane.close`, `worktree.create`), and the purely
//! presentational ones (path toggle, scope) are client state, exactly as they
//! are in the daemon's own TUI. The terminal's menu replaces the one the
//! daemon draws into the grid on a right click: it offers the clipboard and
//! the pane/tab actions a client can reach (split, fork, rename, move to a
//! tab), so the click never reaches the daemon. A tab in the strip offers
//! renaming and closing that tab. The Files tab's menu creates,
//! renames, and deletes the entries under the daemon's tree, which the client
//! does over disk or SSH since the daemon never writes.
//!
//! This module owns each menu's items and their order, and folds them into
//! [`Entry`] trees. [`super::native_menu`] turns a tree into an `NSMenu` and
//! the window runs the picked item, so nothing here knows GPUI or AppKit.

use super::browser_viewport::Viewport;

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
        /// Whether the folder is already pinned, which turns Pin into Unpin.
        pinned: bool,
    },
    /// A saved folder in the sidebar head's pinned menu or on Home's Pinned and
    /// Recent blocks, which is not open as a workspace (yet).
    Saved {
        /// Endpoint key (`local` or a machine id).
        endpoint: String,
        /// The folder on that endpoint's machine.
        path: String,
        /// What the folder is called, kept if it is pinned from here.
        name: String,
        /// Whether it is pinned now, which turns Pin into Unpin.
        pinned: bool,
    },
    /// The AGENTS header's filter button: which statuses the list shows.
    AgentFilter,
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
    /// is what Copy needs, so the row only appears when it can work. `agent`
    /// is whether the focused pane runs an agent, which Fork needs.
    Terminal { selection: bool, agent: bool },
    /// A tab in the strip, addressed by the daemon's stable tab id rather
    /// than its position, so a reorder while the menu is up cannot retarget
    /// the pick.
    Tab { tab_id: String },
    /// The GitHub section's header in the sidebar.
    Github,
    /// The Browser's three-dot button. `url` is the page the tab shows, empty
    /// for a tab with no page, which has nothing to open or copy.
    Browser {
        url: String,
        /// The default browser's name, for the Open row; `None` when the
        /// system does not say.
        browser: Option<String>,
    },
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

/// One row of a context menu, in display order.
///
/// `Divider` is a separator line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// Close the workspace, or close the agent's pane.
    Close,
    /// Rename the workspace's label, or give the agent's pane a live alias.
    Rename,
    /// Keep the workspace's folder in the sidebar head's pinned menu and on Home.
    PinWorkspace,
    /// Take the folder out of the pinned menu again.
    UnpinWorkspace,
    /// Toggle the second (path) line of every row in the owning section.
    TogglePaths,
    /// The parent row of the copy submenu, which is never picked itself.
    QuickActions,
    CopyPath,
    CopyBranch,
    /// Create a linked worktree of the row's repo.
    NewWorktree,
    /// Delete a linked worktree and its files. Destructive; only offered on
    /// rows the daemon's `worktree.list` confirms as linked checkouts.
    DeleteWorktree,
    /// Show only the active workspace's agents in the AGENTS dock.
    ToggleScope,
    /// List agents of every status.
    FilterAll,
    /// List only agents of one status (see [`Item::status`]).
    FilterBlocked,
    FilterDone,
    FilterWorking,
    FilterIdle,
    /// Copy the terminal's highlighted text.
    CopySelection,
    /// Create an empty file in the folder the menu is on.
    NewFile,
    /// Create a folder in the folder the menu is on.
    NewFolder,
    /// Pick local files to upload into the folder the menu is on.
    Upload,
    /// Save the file the menu is on to this machine.
    Download,
    /// Save the folder the menu is on to this machine as a `.tar.gz`.
    DownloadArchive,
    /// Delete a file, or a folder and its contents. Destructive.
    Delete,
    /// Send the clipboard to the focused pane.
    Paste,
    /// Split the focused pane side by side.
    SplitRight,
    /// Split the focused pane stacked.
    SplitDown,
    /// Fork the focused agent's session into a sibling pane.
    ForkPane,
    /// Move the focused pane to another tab, or to a tab of its own.
    MoveToTab,
    /// Open the page listing every repository.
    GithubAll,
    /// Ask GitHub again, past the cache's lifetime.
    GithubRefresh,
    /// Stop drawing one GitHub section; the Settings page brings it back.
    GithubHideSection,
    /// Turn the GitHub integration off, which also stops its requests.
    GithubDisable,
    NewTab,
    /// Show or hide the Browser's tray of notes.
    BrowserNotes,
    /// The parent row of the viewport submenu, which is never picked itself.
    ViewportMenu,
    /// Hold the Browser's page to this viewport.
    Viewport(Viewport),
    /// Hand the Browser's page to the system's default browser.
    OpenInBrowser,
    /// Put the Browser's address on the clipboard.
    CopyUrl,
    /// Rename the right-clicked tab in the strip.
    RenameTab,
    /// Close the active tab, or the right-clicked one in the strip.
    /// Destructive: it ends every pane in it.
    CloseTab,
    Divider,
}

impl Item {
    /// The label, resolved against the state the row toggles: `paths_visible`
    /// flips Show Path/Hide Path, and `scoped` flips the AGENTS scope row.
    pub fn label(self, paths_visible: bool, scoped: bool) -> &'static str {
        match self {
            Self::Close => "Close",
            Self::Rename => "Rename",
            Self::PinWorkspace => "Pin this Workspace",
            Self::UnpinWorkspace => "Unpin this Workspace",
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
            Self::ToggleScope => {
                if scoped {
                    "Show All Workspaces"
                } else {
                    "Show Workspace Only"
                }
            }
            Self::FilterAll => "All Statuses",
            Self::FilterBlocked => "Blocked",
            Self::FilterDone => "Done",
            Self::FilterWorking => "Working",
            Self::FilterIdle => "Idle",
            Self::NewFile => "New File",
            Self::NewFolder => "New Folder",
            Self::Upload => "Upload Files…",
            Self::Download => "Download…",
            Self::DownloadArchive => "Download as Archive…",
            Self::Delete => "Delete",
            Self::CopySelection => "Copy",
            Self::Paste => "Paste",
            Self::SplitRight => "Split Right",
            Self::SplitDown => "Split Down",
            Self::ForkPane => "Fork to New Pane",
            Self::MoveToTab => "Move to Tab",
            Self::GithubAll => "All Repositories",
            Self::GithubRefresh => "Refresh",
            Self::GithubHideSection => "Hide Section",
            Self::GithubDisable => "Turn Off GitHub",
            Self::NewTab => "New Tab",
            Self::BrowserNotes => "Notes",
            Self::ViewportMenu => "Viewport",
            Self::Viewport(viewport) => viewport.label(),
            Self::OpenInBrowser => "Open in Default Browser",
            Self::CopyUrl => "Copy URL",
            Self::RenameTab => "Rename Tab",
            Self::CloseTab => "Close Tab",
            Self::Divider => "",
        }
    }

    /// The SF Symbol drawn beside the row, the way macOS menus mark their
    /// items. A name the system lacks just leaves the row without one.
    pub fn symbol(self, paths_visible: bool) -> &'static str {
        match self {
            Self::Close | Self::CloseTab => "xmark",
            Self::Rename | Self::RenameTab => "pencil",
            Self::PinWorkspace => "pin",
            Self::UnpinWorkspace => "pin.slash",
            Self::TogglePaths => {
                if paths_visible {
                    "eye.slash"
                } else {
                    "eye"
                }
            }
            Self::QuickActions => "bolt",
            Self::CopyPath | Self::CopyBranch | Self::CopySelection => "doc.on.doc",
            Self::NewWorktree => "arrow.triangle.branch",
            Self::DeleteWorktree | Self::Delete => "trash",
            Self::ToggleScope => "macwindow",
            Self::FilterAll
            | Self::FilterBlocked
            | Self::FilterDone
            | Self::FilterWorking
            | Self::FilterIdle => "",
            Self::NewFile => "doc.badge.plus",
            Self::NewFolder => "folder.badge.plus",
            Self::Upload => "square.and.arrow.up",
            Self::Download => "square.and.arrow.down",
            Self::DownloadArchive => "archivebox",
            Self::Paste => "doc.on.clipboard",
            Self::SplitRight => "rectangle.split.2x1",
            Self::SplitDown => "rectangle.split.1x2",
            Self::ForkPane => "arrow.branch",
            Self::MoveToTab => "arrow.turn.down.right",
            Self::NewTab => "plus",
            Self::BrowserNotes => "bubble.left",
            Self::ViewportMenu => "rectangle.dashed",
            Self::Viewport(_) => "",
            Self::OpenInBrowser => "safari",
            Self::CopyUrl => "link",
            Self::GithubAll => "list.bullet",
            Self::GithubRefresh => "arrow.clockwise",
            Self::GithubHideSection => "eye.slash",
            Self::GithubDisable => "power",
            Self::Divider => "",
        }
    }
}

impl Item {
    /// The daemon's status a filter row stands for; `None` for every other
    /// row, and for [`Item::FilterAll`], which stands for no status at all.
    pub fn status(self) -> Option<&'static str> {
        match self {
            Self::FilterBlocked => Some("blocked"),
            Self::FilterDone => Some("done"),
            Self::FilterWorking => Some("working"),
            Self::FilterIdle => Some("idle"),
            _ => None,
        }
    }
}

/// One entry of a built menu, in display order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A row that runs `item` when picked.
    Action {
        item: Item,
        title: String,
        symbol: &'static str,
        /// Drawn with the menu's own check mark, not an icon, so the tick
        /// sits where macOS puts it on every other menu.
        checked: bool,
    },
    /// A row that opens `entries` beside it.
    Submenu {
        title: String,
        symbol: &'static str,
        entries: Vec<Entry>,
    },
    Separator,
}

/// The window state some rows read their wording from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Labels {
    /// The owning section shows its path lines, so the toggle hides them.
    pub paths_visible: bool,
    /// The AGENTS dock shows only the active workspace's agents.
    pub scoped: bool,
    /// How many repositories the GitHub section knows, shown on its first row.
    pub repos: usize,
    /// The one status the AGENTS list is narrowed to, which its filter menu
    /// ticks; `None` when it lists them all.
    pub status: Option<&'static str>,
    /// The Browser's tray of notes is open, which its Notes row ticks.
    pub notes_open: bool,
    /// How many notes the Browser's tab holds, shown on its Notes row.
    pub notes: usize,
    /// The viewport the Browser's tab is held to, which its submenu ticks.
    pub viewport: Viewport,
    /// The room the Browser's column leaves its page, in pixels, so each
    /// viewport row can say what size the page would get. Zero before the
    /// Browser has been drawn, when the rows show no size.
    pub room: (u32, u32),
}

impl MenuTarget {
    /// The top-level rows. Quick Actions stands for its submenu, whose rows
    /// come from [`Self::quick_rows`].
    ///
    /// A WORKSPACES target's git rows appear when its branch says it is a repo, and the deleted-worktree row
    /// only when `linked` says the daemon's `worktree.list` found this row's
    /// checkout as a linked worktree.
    pub fn rows(&self, linked: bool) -> Vec<Item> {
        match self {
            Self::Workspace { branch, pinned, .. } => {
                workspace_rows(branch.as_deref().is_some(), linked, *pinned)
            }
            Self::Saved { pinned, .. } => saved_rows(*pinned),
            Self::Agent { .. } => agent_rows(),
            Self::AgentFilter => agent_filter_rows(),
            Self::Terminal { selection, agent } => terminal_rows(*selection, *agent),
            Self::Tab { .. } => tab_rows(),
            Self::Github => github_rows(),
            Self::Browser { url, .. } => browser_rows(!url.is_empty()),
            Self::File { root, dir, .. } => file_rows(*root, *dir),
        }
    }

    /// The rows of the Quick Actions submenu.
    pub fn quick_rows(&self) -> Vec<Item> {
        let mut items = vec![Item::CopyPath];
        if let Self::Workspace { branch, .. } = self
            && branch.as_deref().is_some_and(|branch| !branch.is_empty())
        {
            items.push(Item::CopyBranch);
        }
        items
    }

    /// The menu as it is shown: [`Self::rows`] with their wording resolved
    /// against `labels`, dividers as separators, and Quick Actions holding
    /// its rows as a submenu.
    pub fn entries(&self, linked: bool, labels: Labels) -> Vec<Entry> {
        self.rows(linked)
            .into_iter()
            .map(|item| match item {
                Item::Divider => Entry::Separator,
                Item::QuickActions => Entry::Submenu {
                    title: item.label(labels.paths_visible, labels.scoped).to_string(),
                    symbol: item.symbol(labels.paths_visible),
                    entries: self
                        .quick_rows()
                        .into_iter()
                        .map(|item| action(item, labels))
                        .collect(),
                },
                Item::ViewportMenu => Entry::Submenu {
                    title: item.label(labels.paths_visible, labels.scoped).to_string(),
                    symbol: item.symbol(labels.paths_visible),
                    entries: Viewport::ALL
                        .into_iter()
                        .map(|viewport| action(Item::Viewport(viewport), labels))
                        .collect(),
                },
                Item::OpenInBrowser => {
                    let mut row = action(item, labels);
                    if let (
                        Entry::Action { title, .. },
                        Self::Browser {
                            browser: Some(name),
                            ..
                        },
                    ) = (&mut row, self)
                    {
                        *title = format!("Open in {name}");
                    }
                    row
                }
                item => action(item, labels),
            })
            .collect()
    }
}

/// A row that runs `item`, worded for `labels`.
fn action(item: Item, labels: Labels) -> Entry {
    let mut title = item.label(labels.paths_visible, labels.scoped).to_string();
    if item == Item::GithubAll {
        title.push_str(&format!(" · {}", labels.repos));
    }
    if item == Item::BrowserNotes && labels.notes > 0 {
        title.push_str(&format!(" · {}", labels.notes));
    }
    if let Item::Viewport(viewport) = item {
        title = viewport.describe(labels.room);
    }
    // A filter row is ticked when it is the one in force, as is the
    // viewport the tab is held to and the tray while it is open.
    let ticked = match item {
        Item::FilterAll
        | Item::FilterBlocked
        | Item::FilterDone
        | Item::FilterWorking
        | Item::FilterIdle => item.status() == labels.status,
        Item::Viewport(viewport) => viewport == labels.viewport,
        Item::BrowserNotes => labels.notes_open,
        _ => false,
    };
    Entry::Action {
        item,
        title,
        symbol: item.symbol(labels.paths_visible),
        checked: ticked,
    }
}

/// WORKSPACES top-level rows, in the daemon's render order: node actions,
/// the copy submenu, then worktrees when the row is a git repo. Pinning is
/// pilus's own, so it sits with the row's own actions rather than among the
/// daemon's. The path toggle's label is applied at render.
fn workspace_rows(is_repo: bool, linked: bool, pinned: bool) -> Vec<Item> {
    let mut items = vec![
        Item::Close,
        Item::Rename,
        if pinned {
            Item::UnpinWorkspace
        } else {
            Item::PinWorkspace
        },
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
    items
}

/// A saved folder's rows: a click already opens it, so the menu only pins or
/// unpins it and hands over its path.
fn saved_rows(pinned: bool) -> Vec<Item> {
    vec![
        if pinned {
            Item::UnpinWorkspace
        } else {
            Item::PinWorkspace
        },
        Item::CopyPath,
    ]
}

/// The GitHub section header's rows: the full list, a refresh, then the two
/// ways to put the section away, the narrower one first.
fn github_rows() -> Vec<Item> {
    vec![
        Item::GithubAll,
        Item::GithubRefresh,
        Item::Divider,
        Item::GithubHideSection,
        Item::GithubDisable,
    ]
}

/// The Browser's three-dot menu: the page's own settings, then what to do
/// with its address. A tab with no page has no address, so those rows are
/// left out rather than shown dead.
fn browser_rows(has_url: bool) -> Vec<Item> {
    let mut items = vec![Item::BrowserNotes, Item::ViewportMenu];
    if has_url {
        items.extend([Item::Divider, Item::OpenInBrowser, Item::CopyUrl]);
    }
    items
}

/// A strip tab's rows: the rename, then the destructive close alone below a
/// divider.
fn tab_rows() -> Vec<Item> {
    vec![Item::RenameTab, Item::Divider, Item::CloseTab]
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

/// The AGENTS header's filter button: every status, then one row per status in the
/// order the list sorts them.
fn agent_filter_rows() -> Vec<Item> {
    vec![
        Item::FilterAll,
        Item::Divider,
        Item::FilterBlocked,
        Item::FilterDone,
        Item::FilterWorking,
        Item::FilterIdle,
    ]
}

/// The terminal's rows: the clipboard first, then the pane actions, the
/// pane's name and tab, and the tab actions, each group below a divider. Copy
/// needs a highlight and Fork needs an agent, so each is left out without one
/// rather than shown dead.
fn terminal_rows(selection: bool, agent: bool) -> Vec<Item> {
    let mut items = Vec::new();
    if selection {
        items.push(Item::CopySelection);
    }
    items.extend([
        Item::Paste,
        Item::Divider,
        Item::SplitRight,
        Item::SplitDown,
    ]);
    if agent {
        items.push(Item::ForkPane);
    }
    items.extend([
        Item::Divider,
        Item::Rename,
        Item::MoveToTab,
        Item::NewTab,
        Item::Divider,
        Item::CloseTab,
    ]);
    items
}

/// A tree entry's rows: where to add, what to do to the entry, then the one
/// destructive row alone at the end. The root keeps only the rows that make
/// sense for the folder the tree is showing.
fn file_rows(root: bool, dir: bool) -> Vec<Item> {
    let mut items = vec![Item::NewFile, Item::NewFolder, Item::Upload, Item::Divider];
    if !root {
        items.push(Item::Rename);
    }
    items.push(if dir {
        Item::DownloadArchive
    } else {
        Item::Download
    });
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
        let rows = workspace_rows(true, true, false);
        assert_eq!(
            rows,
            vec![
                Item::Close,
                Item::Rename,
                Item::PinWorkspace,
                Item::TogglePaths,
                Item::QuickActions,
                Item::DeleteWorktree,
                Item::NewWorktree,
            ]
        );
        // A non-repo row drops the git rows. The delete row needs a
        // confirmed linked checkout, not just any repo row.
        let rows = workspace_rows(false, false, false);
        assert!(!rows.contains(&Item::DeleteWorktree));
        assert!(!rows.contains(&Item::NewWorktree));
        let main = workspace_rows(true, false, false);
        assert!(!main.contains(&Item::DeleteWorktree));
        let linked = workspace_rows(true, true, false);
        assert!(linked.contains(&Item::DeleteWorktree));
    }

    #[test]
    fn a_pinned_folder_offers_unpin_in_place_of_pin() {
        let pin = workspace_rows(false, false, false);
        assert!(pin.contains(&Item::PinWorkspace) && !pin.contains(&Item::UnpinWorkspace));
        let unpin = workspace_rows(false, false, true);
        assert!(unpin.contains(&Item::UnpinWorkspace) && !unpin.contains(&Item::PinWorkspace));
        assert_eq!(Item::PinWorkspace.label(false, false), "Pin this Workspace");
        assert_eq!(saved_rows(true), vec![Item::UnpinWorkspace, Item::CopyPath]);
        assert_eq!(saved_rows(false), vec![Item::PinWorkspace, Item::CopyPath]);
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
            pinned: false,
        };
        assert_eq!(target.quick_rows(), vec![Item::CopyPath, Item::CopyBranch]);
        assert_eq!(target_without_branch().quick_rows(), vec![Item::CopyPath]);
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
        let with = terminal_rows(true, false);
        assert_eq!(with[0], Item::CopySelection);
        let without = terminal_rows(false, false);
        assert!(!without.contains(&Item::CopySelection));
        assert_eq!(without[0], Item::Paste);
        // Closing the tab sits alone at the end, away from the safe rows.
        assert_eq!(without.last(), Some(&Item::CloseTab));
        assert_eq!(without[without.len() - 2], Item::Divider);
        for item in [Item::Rename, Item::MoveToTab] {
            assert!(without.contains(&item));
        }
    }

    #[test]
    fn fork_is_offered_only_for_an_agent() {
        assert!(!terminal_rows(false, false).contains(&Item::ForkPane));
        assert!(terminal_rows(false, true).contains(&Item::ForkPane));
    }

    #[test]
    fn the_terminal_target_builds_its_own_rows() {
        let target = MenuTarget::Terminal {
            selection: false,
            agent: true,
        };
        assert_eq!(target.rows(true), terminal_rows(false, true));
    }

    #[test]
    fn a_tab_offers_rename_then_close() {
        let target = MenuTarget::Tab {
            tab_id: "t1".into(),
        };
        assert_eq!(
            target.rows(false),
            vec![Item::RenameTab, Item::Divider, Item::CloseTab]
        );
        let titles: Vec<String> = target
            .entries(false, Labels::default())
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Action { title, .. } => Some(title),
                _ => None,
            })
            .collect();
        assert_eq!(titles, vec!["Rename Tab", "Close Tab"]);
    }

    #[test]
    fn the_trees_root_cannot_be_renamed_or_deleted() {
        let root = file_rows(true, true);
        assert_eq!(
            root,
            vec![
                Item::NewFile,
                Item::NewFolder,
                Item::Upload,
                Item::Divider,
                Item::DownloadArchive,
                Item::CopyPath
            ]
        );
        let entry = file_rows(false, false);
        assert!(entry.contains(&Item::Rename));
        assert_eq!(entry.last(), Some(&Item::Delete));
        assert_eq!(entry[entry.len() - 2], Item::Divider);
    }

    #[test]
    fn a_folder_downloads_as_an_archive_and_a_file_as_itself() {
        assert!(file_rows(false, true).contains(&Item::DownloadArchive));
        assert!(!file_rows(false, true).contains(&Item::Download));
        assert!(file_rows(false, false).contains(&Item::Download));
        assert!(!file_rows(false, false).contains(&Item::DownloadArchive));
    }

    #[test]
    fn quick_actions_become_a_submenu_of_its_copy_rows() {
        let target = MenuTarget::Workspace {
            endpoint: "local".into(),
            workspace_id: "w1".into(),
            index: 0,
            name: "api".into(),
            cwd: "/repo/api".into(),
            branch: Some("main".into()),
            pinned: false,
        };
        let entries = target.entries(false, Labels::default());
        let quick = entries
            .iter()
            .find_map(|entry| match entry {
                Entry::Submenu { title, entries, .. } => Some((title.as_str(), entries)),
                _ => None,
            })
            .expect("a Quick Actions submenu");
        assert_eq!(quick.0, "Quick Actions");
        let picks: Vec<Item> = quick
            .1
            .iter()
            .filter_map(|entry| match entry {
                Entry::Action { item, .. } => Some(*item),
                _ => None,
            })
            .collect();
        assert_eq!(picks, vec![Item::CopyPath, Item::CopyBranch]);
        // The parent stands for its submenu, so it is never a pick of its own.
        assert!(!entries.iter().any(|entry| matches!(
            entry,
            Entry::Action {
                item: Item::QuickActions,
                ..
            }
        )));
    }

    #[test]
    fn dividers_become_separators_in_place() {
        let target = MenuTarget::Agent {
            endpoint: "local".into(),
            pane: "2".into(),
            name: "auth-fix".into(),
        };
        let entries = target.entries(false, Labels::default());
        assert_eq!(entries.len(), agent_rows().len());
        assert_eq!(entries[3], Entry::Separator);
    }

    #[test]
    fn the_filter_menu_ticks_the_status_in_force() {
        let ticked = |status| -> Vec<Item> {
            MenuTarget::AgentFilter
                .entries(
                    false,
                    Labels {
                        status,
                        ..Labels::default()
                    },
                )
                .into_iter()
                .filter_map(|entry| match entry {
                    Entry::Action {
                        item,
                        checked: true,
                        ..
                    } => Some(item),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(ticked(None), [Item::FilterAll]);
        assert_eq!(ticked(Some("done")), [Item::FilterDone]);
    }

    fn browser(url: &str) -> MenuTarget {
        MenuTarget::Browser {
            url: url.into(),
            browser: Some("Safari".into()),
        }
    }

    #[test]
    fn the_browser_menu_leaves_out_the_address_rows_without_a_page() {
        assert_eq!(
            browser_rows(true),
            vec![
                Item::BrowserNotes,
                Item::ViewportMenu,
                Item::Divider,
                Item::OpenInBrowser,
                Item::CopyUrl,
            ]
        );
        assert_eq!(
            browser_rows(false),
            vec![Item::BrowserNotes, Item::ViewportMenu]
        );
        assert_eq!(browser("").rows(false), browser_rows(false));
        assert_eq!(browser("https://a.test").rows(false), browser_rows(true));
    }

    #[test]
    fn the_open_row_names_the_default_browser_when_known() {
        let title = |browser| {
            let target = MenuTarget::Browser {
                url: "https://a.test".into(),
                browser,
            };
            target
                .entries(false, Labels::default())
                .into_iter()
                .find_map(|entry| match entry {
                    Entry::Action {
                        item: Item::OpenInBrowser,
                        title,
                        ..
                    } => Some(title),
                    _ => None,
                })
        };
        assert_eq!(title(Some("Arc".into())).as_deref(), Some("Open in Arc"));
        assert_eq!(title(None).as_deref(), Some("Open in Default Browser"));
    }

    #[test]
    fn the_viewport_rows_say_what_size_the_page_gets() {
        // The room is short, so the pages are drawn smaller, but they lay out
        // at their own sizes.
        let labels = Labels {
            room: (1000, 700),
            ..Labels::default()
        };
        let entries = browser("https://a.test").entries(false, labels);
        let Some(Entry::Submenu { entries: sub, .. }) = entries
            .iter()
            .find(|entry| matches!(entry, Entry::Submenu { .. }))
        else {
            panic!("the Viewport row opens a submenu");
        };
        let titles: Vec<&str> = sub
            .iter()
            .filter_map(|entry| match entry {
                Entry::Action { title, .. } => Some(title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            titles,
            [
                "Responsive (1000\u{d7}700)",
                "16:10 (1280\u{d7}800)",
                "4:3 (1024\u{d7}768)",
                "Mobile (390\u{d7}844)",
                "Tablet (768\u{d7}1024)",
            ]
        );
    }

    #[test]
    fn the_viewport_submenu_ticks_the_one_in_force() {
        let labels = Labels {
            viewport: Viewport::Mobile,
            notes_open: true,
            notes: 3,
            ..Labels::default()
        };
        let entries = browser("https://a.test").entries(false, labels);
        let Some(Entry::Submenu { entries: sub, .. }) = entries
            .iter()
            .find(|entry| matches!(entry, Entry::Submenu { .. }))
        else {
            panic!("the Viewport row opens a submenu");
        };
        let rows: Vec<(Item, bool)> = sub
            .iter()
            .filter_map(|entry| match entry {
                Entry::Action { item, checked, .. } => Some((*item, *checked)),
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), Viewport::ALL.len());
        assert_eq!(
            rows.iter()
                .filter(|(_, checked)| *checked)
                .collect::<Vec<_>>(),
            [&(Item::Viewport(Viewport::Mobile), true)]
        );
        // The Notes row shows its count and is ticked while the tray is open.
        assert!(matches!(
            &entries[0],
            Entry::Action { item: Item::BrowserNotes, title, checked: true, .. } if title == "Notes · 3"
        ));
    }

    #[test]
    fn entries_take_their_wording_from_the_window() {
        let agent = MenuTarget::Agent {
            endpoint: "local".into(),
            pane: "2".into(),
            name: "auth-fix".into(),
        };
        let labels = Labels {
            paths_visible: true,
            scoped: true,
            repos: 0,
            status: None,
            ..Labels::default()
        };
        let titles: Vec<String> = agent
            .entries(false, labels)
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Action { title, .. } => Some(title),
                _ => None,
            })
            .collect();
        assert!(titles.contains(&"Hide Path".to_string()));
        assert!(titles.contains(&"Show All Workspaces".to_string()));
        let github = MenuTarget::Github.entries(
            false,
            Labels {
                repos: 12,
                ..Labels::default()
            },
        );
        assert!(matches!(
            &github[0],
            Entry::Action { title, .. } if title == "All Repositories · 12"
        ));
    }

    #[test]
    fn every_row_but_the_divider_has_a_symbol() {
        let target = MenuTarget::Terminal {
            selection: true,
            agent: true,
        };
        for item in target.rows(false) {
            assert_eq!(item.symbol(false).is_empty(), item == Item::Divider);
        }
        assert_ne!(
            Item::TogglePaths.symbol(true),
            Item::TogglePaths.symbol(false)
        );
    }

    fn target_without_branch() -> MenuTarget {
        MenuTarget::Workspace {
            endpoint: "local".into(),
            workspace_id: "w2".into(),
            index: 1,
            name: "web".into(),
            cwd: "/repo/web".into(),
            branch: None,
            pinned: false,
        }
    }
}
