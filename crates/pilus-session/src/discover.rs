//! Finding a session file on disk.
//!
//! Pi groups sessions by working directory under
//! `~/.pi/agent/sessions/--<slug>--/`, where the slug is the cwd with its
//! leading separator removed and `/`, `\`, and `:` replaced by `-`. The
//! directory can be overridden with `PI_CODING_AGENT_SESSION_DIR`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Filename suffix pi writes.
const EXTENSION: &str = "jsonl";

/// The root pi stores sessions under.
///
/// Honors `PI_CODING_AGENT_SESSION_DIR`, else `~/.pi/agent/sessions`.
pub fn sessions_root() -> PathBuf {
    if let Some(root) = std::env::var_os("PI_CODING_AGENT_SESSION_DIR") {
        return PathBuf::from(root);
    }
    home().join(".pi/agent/sessions")
}

/// The per-working-directory folder name pi uses for `cwd`.
pub fn dir_name_for(cwd: &str) -> String {
    let slug = cwd
        .trim_start_matches(['/', '\\'])
        .replace(['/', '\\', ':'], "-");
    format!("--{slug}--")
}

/// The folder pi stores sessions for `cwd` under `root`.
pub fn sessions_dir(root: &Path, cwd: &str) -> PathBuf {
    root.join(dir_name_for(cwd))
}

/// Every session file for `cwd`, newest first by modification time.
pub fn list(root: &Path, cwd: &str) -> Vec<PathBuf> {
    let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(sessions_dir(root, cwd))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(EXTENSION) {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, path))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    files.into_iter().map(|(_, path)| path).collect()
}

/// The most recently written session file for `cwd`.
pub fn latest(root: &Path, cwd: &str) -> Option<PathBuf> {
    list(root, cwd).into_iter().next()
}

/// The session file for `cwd` whose name carries `session_id`.
///
/// Pi names files `<timestamp>_<session-id>.jsonl`, so the exact id is a
/// suffix match; this is how a live agent's session is found when the daemon
/// reports the id it is running.
pub fn find_by_id(root: &Path, cwd: &str, session_id: &str) -> Option<PathBuf> {
    let suffix = format!("_{session_id}.{EXTENSION}");
    list(root, cwd)
        .into_iter()
        .find(|path| path.to_string_lossy().ends_with(&suffix))
}

/// The session file carrying `session_id`, wherever under `root` it sits.
///
/// A pane reports its session id, not its cwd dir, and an agent can record
/// a session under a different cwd than the pane reports; scanning every
/// subdirectory finds it anyway. The newest mtime wins on a collision.
pub fn find_any_by_id(root: &Path, session_id: &str) -> Option<PathBuf> {
    let suffix = format!("_{session_id}.{EXTENSION}");
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            dir.is_dir().then_some(dir)
        })
        .flat_map(|dir| {
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    if !path.to_string_lossy().ends_with(&suffix) {
                        return None;
                    }
                    let modified = entry
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    Some((modified, path))
                })
        })
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, path)| path)
}

/// Resolve a `--session-ui` argument to a session file.
///
/// An existing file is used as-is; anything else is treated as a working
/// directory and resolved to its latest session.
pub fn resolve(spec: &str) -> Option<PathBuf> {
    let path = PathBuf::from(spec);
    if path.is_file() {
        return Some(path);
    }
    let cwd = if path.is_dir() {
        path.to_string_lossy().into_owned()
    } else {
        spec.to_string()
    };
    latest(&sessions_root(), &cwd)
}

/// The user's home directory, or the current directory when unset.
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_pis_encoding() {
        assert_eq!(
            dir_name_for("/Users/rizaldy/.luvus/worktrees/l3s/pi-gui"),
            "--Users-rizaldy-.luvus-worktrees-l3s-pi-gui--"
        );
        // A Windows-style path and a path with a drive colon both collapse.
        assert_eq!(dir_name_for("C:\\work\\app"), "--C--work-app--");
    }

    #[test]
    fn list_orders_newest_first() {
        let root = std::env::temp_dir().join(format!("pilus-session-list-{}", std::process::id()));
        let dir = sessions_dir(&root, "/tmp/app");
        std::fs::create_dir_all(&dir).expect("session dir");
        let older = dir.join("2020-01-01T00-00-00-000Z_a.jsonl");
        let newer = dir.join("2024-01-01T00-00-00-000Z_b.jsonl");
        std::fs::write(&older, "{}\n").expect("write older");
        std::fs::write(&newer, "{}\n").expect("write newer");
        // The filename tiebreak (descending) keeps the order deterministic
        // even when both writes land in the same mtime tick.
        let files = list(&root, "/tmp/app");
        assert_eq!(files.first().map(PathBuf::as_path), Some(newer.as_path()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_any_by_id_crosses_cwd_dirs() {
        let root = std::env::temp_dir().join(format!("pilus-session-any-{}", std::process::id()));
        let other = sessions_dir(&root, "/elsewhere");
        std::fs::create_dir_all(&other).expect("session dir");
        let found = other.join("2024-01-01T00-00-00-000Z_reported.jsonl");
        std::fs::write(&found, "{}\n").expect("write session");
        assert_eq!(find_any_by_id(&root, "reported").as_deref(), Some(found.as_path()));
        assert_eq!(find_any_by_id(&root, "absent"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
