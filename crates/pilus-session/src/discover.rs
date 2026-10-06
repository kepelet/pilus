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

/// Lines scanned for a session's first user prompt before giving up. A
/// session begins with its header and a handful of structural entries, so
/// the first prompt is always near the top; the cap keeps a pathological
/// file from stalling a history listing.
const PROMPT_SCAN_LINES: usize = 2000;

/// One session file's summary, for a session picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionListing {
    /// The session file.
    pub path: PathBuf,
    /// The session id the file name carries.
    pub id: String,
    /// The session's first user prompt, one collapsed line; empty when the
    /// session has no user message yet.
    pub title: String,
}

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

/// The session id a pi filename carries: the part after the first `_` in
/// `<timestamp>_<id>.jsonl`.
pub fn id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let (_, id) = stem.split_once('_')?;
    (!id.is_empty()).then(|| id.to_string())
}

/// The newest session file whose id starts with `partial`, wherever under
/// `root` it sits. Pi's `--session <id>` accepts a partial UUID; this is its
/// discovery side. A path separator never matches, so a cwd spec falls
/// through to [`resolve`]'s latest-session lookup.
pub fn find_by_partial_id(root: &Path, partial: &str) -> Option<PathBuf> {
    if partial.is_empty() || partial.contains(['/', '\\']) {
        return None;
    }
    let needle = partial.to_lowercase();
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = file.path();
            let Some(id) = id_from_path(&path) else {
                continue;
            };
            if !id.to_lowercase().starts_with(&needle) {
                continue;
            }
            let modified = file
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(seen, _)| modified > *seen) {
                best = Some((modified, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// The most recent `limit` sessions for `cwd`, newest first, each with the
/// display title a picker shows.
pub fn listing(root: &Path, cwd: &str, limit: usize) -> Vec<SessionListing> {
    list(root, cwd)
        .into_iter()
        .take(limit)
        .filter_map(|path| {
            let id = id_from_path(&path)?;
            let title = first_prompt(&path).unwrap_or_default();
            Some(SessionListing { path, id, title })
        })
        .collect()
}

/// The first user message's text in a session file, collapsed to one line.
///
/// Read line by line instead of folding the whole file: a history listing
/// only needs the opener, and a long session should not be parsed twice.
fn first_prompt(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    use std::io::BufRead;
    reader
        .lines()
        .take(PROMPT_SCAN_LINES)
        .map_while(Result::ok)
        .find_map(|line| prompt_of_line(&line))
}

/// The first user prompt in a chunk of session JSONL, for a caller that has
/// already read the bytes (a remote sample, say).
pub fn first_prompt_in(text: &str) -> Option<String> {
    text.lines()
        .take(PROMPT_SCAN_LINES)
        .find_map(prompt_of_line)
}

/// One session line projected to a user prompt, when it is one.
fn prompt_of_line(line: &str) -> Option<String> {
    let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
        return truncated_prompt(line);
    };
    let message = entry.get("message")?;
    if message.get("role").and_then(|role| role.as_str()) != Some("user") {
        return None;
    }
    message_text(message)
}

/// The prompt in a user line that was cut short — a remote listing caps
/// each line it ships, and a pasted prompt can run past the cap. Reads the
/// first text block's string up to its closing quote or the cut, whichever
/// comes first.
fn truncated_prompt(line: &str) -> Option<String> {
    if !line.contains("\"role\":\"user\"") {
        return None;
    }
    let start = line.find("\"text\":\"")? + "\"text\":\"".len();
    let mut text = String::new();
    let mut chars = line[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n' | 't' | 'r') => text.push(' '),
                Some('u') => {
                    let code: String = chars.by_ref().take(4).collect();
                    if let Some(decoded) =
                        u32::from_str_radix(&code, 16).ok().and_then(char::from_u32)
                    {
                        text.push(decoded);
                    }
                }
                Some(escaped) => text.push(escaped),
                None => break,
            },
            c => text.push(c),
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// A message's text blocks joined and collapsed to one line; `None` when it
/// carries no prose (an image-only user turn).
fn message_text(message: &serde_json::Value) -> Option<String> {
    let content = message.get("content")?;
    let text = match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                (block.get("type")?.as_str()? == "text")
                    .then(|| block.get("text")?.as_str().map(str::to_string))?
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
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

/// Resolve a `pilus --session-ui` argument to a session file.
///
/// Mirrors pi's own `--session <path|id>`: an existing file is used as-is, a
/// directory resolves to its latest session, a separator-free spec is tried
/// as a full or partial session id, and anything else is a working directory
/// whose newest session is returned.
pub fn resolve(spec: &str) -> Option<PathBuf> {
    let path = PathBuf::from(spec);
    if path.is_file() {
        return Some(path);
    }
    if path.is_dir() {
        return latest(&sessions_root(), &path.to_string_lossy());
    }
    // A spec with a path separator can only be a cwd that does not exist
    // yet; only a separator-free spec is a candidate for a session id.
    if !spec.contains(['/', '\\'])
        && let Some(found) = find_by_partial_id(&sessions_root(), spec)
    {
        return Some(found);
    }
    latest(&sessions_root(), spec)
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
    fn a_cut_user_line_still_yields_its_prompt() {
        let line = r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"fix the \"gauge\"\nplease and also the very long tail that got cu"#;
        assert_eq!(
            first_prompt_in(line).as_deref(),
            Some("fix the \"gauge\" please and also the very long tail that got cu")
        );
        // A cut line that is not a user turn is no prompt.
        assert_eq!(
            first_prompt_in(r#"{"message":{"role":"system","content":"x"#),
            None
        );
    }

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
        assert_eq!(
            find_any_by_id(&root, "reported").as_deref(),
            Some(found.as_path())
        );
        assert_eq!(find_any_by_id(&root, "absent"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn id_from_path_reads_the_suffix_after_the_first_underscore() {
        assert_eq!(
            id_from_path(Path::new(
                "/s/--repo--/2026-08-26T18-07-57-407Z_01a03f41-e1df-7a41-b4a8.jsonl"
            ))
            .as_deref(),
            Some("01a03f41-e1df-7a41-b4a8")
        );
        assert_eq!(id_from_path(Path::new("/s/nounderscore.jsonl")), None);
    }

    #[test]
    fn find_by_partial_id_matches_prefixes_across_cwds() {
        let root = std::env::temp_dir().join(format!("pilus-partial-{}", std::process::id()));
        let dir = sessions_dir(&root, "/tmp/app");
        std::fs::create_dir_all(&dir).expect("session dir");
        let wanted = dir.join("2024-01-01T00-00-00-000Z_abcd-1234.jsonl");
        std::fs::write(&wanted, "{}\n").expect("write session");
        std::fs::write(dir.join("2024-01-01T00-00-00-000Z_zzzz-9999.jsonl"), "{}\n")
            .expect("write other");
        assert_eq!(
            find_by_partial_id(&root, "abcd").as_deref(),
            Some(wanted.as_path())
        );
        // A prefix that is a path is never an id.
        assert_eq!(find_by_partial_id(&root, "ab/cd"), None);
        assert_eq!(find_by_partial_id(&root, "nope"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_uses_an_existing_file_as_is() {
        let dir = std::env::temp_dir().join(format!("pilus-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("2024-01-01T00-00-00-000Z_pick-me.jsonl");
        std::fs::write(&file, "{}\n").expect("write session");
        assert_eq!(
            resolve(file.to_string_lossy().as_ref()).as_deref(),
            Some(file.as_path())
        );
        // A path that does not exist and has no matching sessions is nothing.
        assert_eq!(resolve("/definitely/not/a/session"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_carries_the_first_prompt_as_a_title() {
        let root = std::env::temp_dir().join(format!("pilus-listing-{}", std::process::id()));
        let dir = sessions_dir(&root, "/tmp/app");
        std::fs::create_dir_all(&dir).expect("session dir");
        let file = dir.join("2024-01-01T00-00-00-000Z_listed.jsonl");
        let lines = [
            r#"{"type":"session","id":"listed","cwd":"/tmp/app"}"#,
            r#"{"type":"message","message":{"role":"user","content":"fix the   parser bugs"}}"#,
        ];
        std::fs::write(&file, lines.join("\n")).expect("write session");
        let entries = listing(&root, "/tmp/app", 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "listed");
        assert_eq!(entries[0].title, "fix the parser bugs");
        // An empty session lists with no title rather than failing.
        let empty = dir.join("2024-01-01T00-00-01-000Z_empty.jsonl");
        std::fs::write(&empty, "{}\n").expect("write empty session");
        let entries = listing(&root, "/tmp/app", 10);
        assert!(
            entries
                .iter()
                .any(|entry| entry.id == "empty" && entry.title.is_empty())
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
