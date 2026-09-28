//! The native Open Workspace picker.
//!
//! A folder browser that lists directories on an endpoint's own filesystem:
//! Local reads it directly, a machine lists it over the same SSH client the
//! render bridge uses. A host dropdown switches which endpoint is browsed, and
//! opening a folder is one `workspace.open` request, so the daemon still owns
//! workspace creation.

use std::sync::mpsc::Sender;

use l3s_client::SshTarget;

use super::text_field::TextField;

/// Where the picker reads directories from.
#[derive(Clone, PartialEq)]
pub enum DirSource {
    /// This machine's filesystem.
    Local,
    /// A remote host, listed through the local OpenSSH client.
    Remote(SshTarget),
}

/// One directory entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// Base name.
    pub name: String,
    /// Absolute path.
    pub path: String,
    /// Whether this is the synthetic `..` parent row.
    pub is_parent: bool,
    /// Whether this is the synthetic Home (`~`) row.
    pub is_home: bool,
}

/// One endpoint choice in the picker's host dropdown.
#[derive(Clone)]
pub struct MenuChoice {
    /// Display label.
    pub label: String,
    /// The filesystem this choice browses.
    pub source: DirSource,
    /// Whether the picker is already browsing this choice.
    pub current: bool,
    /// Reachability from the last probe: `None` while unknown.
    pub online: Option<bool>,
}

/// A finished directory listing.
pub struct Listed {
    /// The directory's resolved absolute path (`~` expanded).
    pub root: String,
    /// Its child directories.
    pub entries: Vec<Entry>,
}

/// What one background picker task reports back.
pub enum PickerEvent {
    /// A directory listing finished.
    Listed {
        /// The directory that was requested.
        path: String,
        /// The resolved listing, or the error that stopped it.
        result: Result<Listed, String>,
    },
    /// A `workspace.open` request finished.
    Opened(Result<(), String>),
    /// A `worktree.create` request finished.
    WorktreeOpened(Result<(), String>),
    /// A project's `workspace.open` finished; `machine` owns the project
    /// (`None` is local), so the surface can hand off to its endpoint.
    ProjectOpened {
        /// The project's machine profile id, or `None` for local.
        machine: Option<String>,
        /// The open outcome.
        result: Result<(), String>,
    },
    /// A project's worktree finished; `machine` owns the project.
    ProjectWorktreeOpened {
        /// The project's machine profile id, or `None` for local.
        machine: Option<String>,
        /// The create outcome.
        result: Result<(), String>,
    },
}

/// The Open Workspace picker's state.
pub struct WorkspacePicker {
    open: bool,
    source: DirSource,
    cwd: String,
    /// Every directory in `cwd`, hidden ones included.
    all: Vec<Entry>,
    /// The rows drawn: `..` (when not at the root) plus visible directories.
    entries: Vec<Entry>,
    selected: usize,
    loading: bool,
    error: Option<String>,
    show_hidden: bool,
    /// Whether the path field owns the keyboard.
    editing: bool,
    /// The editable path shown above the list.
    path: TextField,
    /// Directories visited, for the “back” shortcut.
    history: Vec<String>,
    /// Whether the host dropdown is showing.
    menu_open: bool,
    /// The host dropdown's choices, rebuilt by the view.
    menu: Vec<MenuChoice>,
    /// Highlighted dropdown row.
    menu_selected: usize,
}

impl Default for WorkspacePicker {
    fn default() -> Self {
        Self {
            open: false,
            source: DirSource::Local,
            cwd: String::new(),
            all: Vec::new(),
            entries: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
            show_hidden: false,
            editing: false,
            path: TextField::default(),
            history: Vec::new(),
            menu_open: false,
            menu: Vec::new(),
            menu_selected: 0,
        }
    }
}

impl WorkspacePicker {
    /// Whether the picker is showing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The directory rows currently drawn.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The highlighted row index.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The current directory.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Whether the path field is being edited.
    pub fn editing(&self) -> bool {
        self.editing
    }

    /// The editable path buffer.
    pub fn path(&self) -> &TextField {
        &self.path
    }

    /// The editable path buffer for key handling.
    pub fn path_mut(&mut self) -> &mut TextField {
        &mut self.path
    }

    /// Whether hidden directories are shown.
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// Whether a listing is in flight.
    pub fn loading(&self) -> bool {
        self.loading
    }

    /// The last listing error, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The source the picker is browsing.
    pub fn source(&self) -> &DirSource {
        &self.source
    }

    /// Whether the host dropdown is showing.
    pub fn menu_open(&self) -> bool {
        self.menu_open
    }

    /// The host dropdown's choices.
    pub fn menu_choices(&self) -> &[MenuChoice] {
        &self.menu
    }

    /// The highlighted host row.
    pub fn menu_selected(&self) -> usize {
        self.menu_selected
    }

    /// Begin browsing `cwd` on `source`.
    pub fn open(&mut self, source: DirSource, cwd: String) {
        self.open = true;
        self.source = source;
        self.menu_open = false;
        self.history.clear();
        self.browse(cwd);
    }

    /// Stop showing the picker.
    pub fn close(&mut self) {
        self.open = false;
        self.editing = false;
        self.menu_open = false;
    }

    /// Switch the browsed endpoint without leaving the picker.
    pub fn rebase(&mut self, source: DirSource, cwd: String) {
        if !self.open {
            return;
        }
        self.source = source;
        self.menu_open = false;
        self.history.clear();
        self.browse(cwd);
    }

    /// Apply a finished listing when it matches the current directory.
    pub fn apply_listing(&mut self, path: &str, result: Result<Listed, String>) {
        if !self.open || path != self.cwd {
            return;
        }
        self.loading = false;
        match result {
            Ok(listed) => {
                self.error = None;
                // Adopt the resolved absolute path so `~` becomes a real
                // directory and `..` keeps working.
                if listed.root != self.cwd {
                    self.cwd = listed.root.clone();
                    self.path = TextField::with_value(self.cwd.clone());
                }
                self.all = listed.entries;
            }
            Err(error) => {
                self.all.clear();
                self.error = Some(error);
            }
        }
        self.rebuild();
    }

    /// Move the highlight by `delta` rows.
    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.entries.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    /// Select the row under a click.
    pub fn select(&mut self, index: usize) {
        if index < self.entries.len() {
            self.selected = index;
        }
    }

    /// Enter the highlighted directory, returning the new path to list.
    pub fn enter(&mut self) -> Option<String> {
        let entry = self.entries.get(self.selected)?.clone();
        self.navigate(entry.path);
        Some(self.cwd.clone())
    }

    /// Go to the parent directory, returning the new path to list.
    pub fn go_up(&mut self) -> Option<String> {
        let parent = parent_of(&self.cwd)?;
        self.navigate(parent);
        Some(self.cwd.clone())
    }

    /// Return to the previously visited directory.
    pub fn go_back(&mut self) -> Option<String> {
        let previous = self.history.pop()?;
        self.browse(previous);
        Some(self.cwd.clone())
    }

    /// Browse `path`, remembering the current directory for “back”.
    fn navigate(&mut self, path: String) {
        if path != self.cwd {
            self.history.push(self.cwd.clone());
        }
        self.browse(path);
    }

    /// Start editing the path field.
    pub fn begin_edit(&mut self) {
        self.editing = true;
        self.path = TextField::with_value(self.cwd.clone());
    }

    /// Stop editing without changing the directory.
    pub fn end_edit(&mut self) {
        self.editing = false;
    }

    /// Jump to the edited path, returning it to list.
    pub fn jump(&mut self) -> Option<String> {
        let target = self.path.value.trim().to_string();
        self.editing = false;
        if target.is_empty() || target == self.cwd {
            return None;
        }
        self.navigate(target);
        Some(self.cwd.clone())
    }

    /// Toggle hidden directories.
    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.rebuild();
    }

    /// Open or close the host dropdown.
    pub fn toggle_menu(&mut self, choices: Vec<MenuChoice>) {
        if self.menu_open {
            self.menu_open = false;
            return;
        }
        self.menu_selected = choices
            .iter()
            .position(|choice| choice.current)
            .unwrap_or(0);
        self.menu = choices;
        self.menu_open = true;
    }

    /// Close the host dropdown.
    pub fn close_menu(&mut self) {
        self.menu_open = false;
    }

    /// Move the host highlight by `delta` rows.
    pub fn menu_move(&mut self, delta: isize) {
        if self.menu.is_empty() {
            return;
        }
        let last = self.menu.len() as isize - 1;
        self.menu_selected = (self.menu_selected as isize + delta).clamp(0, last) as usize;
    }

    /// Pick the highlighted host, returning the source to browse.
    pub fn take_menu_source(&mut self) -> Option<DirSource> {
        let source = self.menu.get(self.menu_selected).map(|c| c.source.clone());
        self.menu_open = false;
        source
    }

    /// Pick the host at `index`, returning the source to browse.
    pub fn choose_menu(&mut self, index: usize) -> Option<DirSource> {
        if index < self.menu.len() {
            self.menu_selected = index;
        }
        self.take_menu_source()
    }

    /// Point the picker at `path` and rebuild its rows.
    fn browse(&mut self, path: String) {
        self.cwd = path;
        self.all.clear();
        self.entries.clear();
        self.selected = 0;
        self.loading = true;
        self.error = None;
        self.editing = false;
        self.path = TextField::with_value(self.cwd.clone());
    }

    /// Recompute the drawn rows from the full listing.
    fn rebuild(&mut self) {
        let mut entries = Vec::with_capacity(self.all.len() + 2);
        entries.push(Entry {
            name: "~".to_string(),
            path: "~".to_string(),
            is_parent: false,
            is_home: true,
        });
        if let Some(parent) = parent_of(&self.cwd) {
            entries.push(Entry {
                name: "..".to_string(),
                path: parent,
                is_parent: true,
                is_home: false,
            });
        }
        for entry in &self.all {
            if self.show_hidden || !entry.name.starts_with('.') {
                entries.push(entry.clone());
            }
        }
        self.entries = entries;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }
}

/// The parent of an absolute POSIX path, or `None` at the root.
pub fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(index) => Some(trimmed[..index].to_string()),
        None => None,
    }
}

/// List one directory on a background thread and report the result.
pub fn spawn_list(tx: Sender<PickerEvent>, source: DirSource, path: String) {
    std::thread::spawn(move || {
        let result = list(&source, &path);
        tx.send(PickerEvent::Listed { path, result }).ok();
    });
}

/// List the child directories of `path`, resolving `~` on the target host.
pub fn list(source: &DirSource, path: &str) -> Result<Listed, String> {
    match source {
        DirSource::Local => list_local(path),
        DirSource::Remote(target) => list_remote(target, path),
    }
}

fn list_local(path: &str) -> Result<Listed, String> {
    let root = expand_local(path);
    let read = std::fs::read_dir(&root).map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    for entry in read.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Symlinks to directories are followed so a linked project is usable.
        let is_dir = file_type.is_dir() || (file_type.is_symlink() && entry.path().is_dir());
        if !is_dir {
            continue;
        }
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path().to_string_lossy().into_owned(),
            is_parent: false,
            is_home: false,
        });
    }
    sort_entries(&mut entries);
    Ok(Listed { root, entries })
}

fn list_remote(target: &SshTarget, path: &str) -> Result<Listed, String> {
    // `cd` resolves `~`; `$PWD` reports the absolute directory; `find` prints
    // NUL-separated relative names, so spaces survive and the root is explicit.
    let remote = format!(
        "cd {} && printf '%s\\0' \"$PWD\" && find . -maxdepth 1 -mindepth 1 -type d -print0",
        remote_cd_arg(path)
    );
    let output = target
        .shell(&remote)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "remote directory listing failed".to_string()
        } else {
            stderr
        });
    }
    let mut fields = output.stdout.split(|byte| *byte == 0);
    let root = fields
        .next()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    if root.is_empty() {
        return Err("remote shell did not report a directory".to_string());
    }
    let mut entries = Vec::new();
    for raw in fields {
        if raw.is_empty() {
            continue;
        }
        let relative = String::from_utf8_lossy(raw);
        let name = relative.trim_start_matches("./").to_string();
        if name.is_empty() {
            continue;
        }
        entries.push(Entry {
            name: name.clone(),
            path: format!("{}/{}", root.trim_end_matches('/'), name),
            is_parent: false,
            is_home: false,
        });
    }
    sort_entries(&mut entries);
    Ok(Listed { root, entries })
}

fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by_key(|entry| entry.name.to_lowercase());
}

/// Expand a leading `~` against this machine's home directory.
fn expand_local(path: &str) -> String {
    let home = || std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    if path == "~" {
        return home();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("{}/{}", home().trim_end_matches('/'), rest);
    }
    if path.starts_with('/') {
        return path.to_string();
    }
    match std::env::current_dir() {
        Ok(dir) => dir.join(path).to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
}

/// Build the remote `cd` argument, letting the remote shell expand `~`.
fn remote_cd_arg(path: &str) -> String {
    if path == "~" {
        return "\"$HOME\"".to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", sh_quote(rest));
    }
    sh_quote(path)
}

/// Quote `value` for a POSIX shell as a single argument.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_of_handles_roots_and_nesting() {
        assert_eq!(parent_of("/Users/rizaldy"), Some("/Users".to_string()));
        assert_eq!(parent_of("/Users"), Some("/".to_string()));
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of(""), None);
        assert_eq!(parent_of("/Users/rizaldy/"), Some("/Users".to_string()));
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("/a b"), "'/a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn remote_cd_expands_tilde() {
        assert_eq!(remote_cd_arg("~"), "\"$HOME\"");
        assert_eq!(remote_cd_arg("~/flo"), "\"$HOME\"/'flo'");
        assert_eq!(remote_cd_arg("/a b"), "'/a b'");
    }

    #[test]
    fn expand_local_resolves_tilde_and_keeps_absolute_paths() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        assert_eq!(expand_local("~"), home);
        assert_eq!(
            expand_local("~/flo"),
            format!("{}/flo", home.trim_end_matches('/'))
        );
        assert_eq!(expand_local("/already/absolute"), "/already/absolute");
    }

    #[test]
    fn list_local_returns_child_directories_sorted() {
        let base = std::env::temp_dir().join(format!("l3s-picker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("zeta")).expect("create zeta");
        std::fs::create_dir_all(base.join("alpha")).expect("create alpha");
        std::fs::write(base.join("file.txt"), b"x").expect("write file");
        let listed = list(&DirSource::Local, base.to_str().expect("utf8")).expect("list");
        assert_eq!(listed.root, base.to_string_lossy());
        let names: Vec<&str> = listed
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        let _ = std::fs::remove_dir_all(&base);
    }
}
