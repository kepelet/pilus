//! One commit's message and patch, read with `git show`.
//!
//! The daemon lists commits (`git.log`) but has no call for what a commit
//! changed, and its `diff.*` methods describe the working tree only. So the
//! Git tab asks Git itself, on this machine or over the saved SSH target,
//! for the repository the daemon reports. The read is bounded: past
//! [`MAX_BYTES`] of output or [`MAX_LINES`] changed lines the rest is dropped
//! and the commit is marked truncated, so one vendored lockfile cannot stall
//! the window or fill its memory. Nothing here writes to the repository.

use std::io::Read;
use std::process::Stdio;

use crate::diff::{DiffHunk, DiffLine, DiffLineKind};
use crate::git::sh_quote;
use crate::transport::SshTarget;

/// Most of Git's output read for one commit.
const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Most hunk lines kept across a commit's files.
const MAX_LINES: usize = 4000;
/// Longest line kept whole; minified bundles would otherwise lay out as one
/// enormous row.
const MAX_LINE_CHARS: usize = 2000;
/// Separates the commit's header fields from its patch in Git's output; no
/// message or path contains it.
const END_OF_HEADER: char = '\u{1e}';
const FIELD: char = '\u{1f}';

/// One file a commit touched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitFile {
    pub path: String,
    /// The pre-rename path; equal to `path` when the file was not renamed.
    pub old_path: String,
    /// `A`dded, `D`eleted, `R`enamed, or `M`odified.
    pub status: String,
    pub binary: bool,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<DiffHunk>,
}

impl CommitFile {
    fn new(path: String) -> Self {
        Self {
            old_path: path.clone(),
            path,
            status: "M".to_string(),
            binary: false,
            additions: 0,
            deletions: 0,
            hunks: Vec::new(),
        }
    }

    pub fn renamed(&self) -> bool {
        self.old_path != self.path
    }
}

/// A commit with what it changed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommitDetail {
    /// The full hash.
    pub sha: String,
    pub author: String,
    /// The author date as `YYYY-MM-DD HH:MM`.
    pub date: String,
    /// The whole message: subject, then any body.
    pub message: String,
    pub files: Vec<CommitFile>,
    /// Output or lines were dropped to stay inside the bounds.
    pub truncated: bool,
}

impl CommitDetail {
    pub fn additions(&self) -> u32 {
        self.files.iter().map(|file| file.additions).sum()
    }

    pub fn deletions(&self) -> u32 {
        self.files.iter().map(|file| file.deletions).sum()
    }
}

/// Read `sha` in the repository at `repo`, on this machine or on `target`'s.
///
/// A merge shows its changes against the first parent, which is what the
/// branch it landed on gained. Failures carry Git's own message.
pub fn show_commit(
    target: Option<&SshTarget>,
    repo: &str,
    sha: &str,
) -> Result<CommitDetail, String> {
    // The hash comes from a daemon; refuse anything Git could read as an
    // option or a path rather than trusting it.
    if sha.len() < 4 || sha.len() > 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("`{sha}` is not a commit hash"));
    }
    let mut command = match target {
        None => {
            let mut command = std::process::Command::new("git");
            command.arg("-C").arg(repo).args(SHOW_ARGS);
            command.arg(FORMAT).arg(sha).arg("--");
            command
        }
        Some(target) => target.shell(&remote_command(repo, sha)),
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("couldn't run git: {error}"))?;
    let mut bytes = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("couldn't read git's output: {error}"))?;
    }
    let cut = bytes.len() > MAX_BYTES;
    if cut {
        // Stop Git writing into a pipe nobody reads.
        child.kill().ok();
        bytes.truncate(MAX_BYTES);
        // Drop the half line the cut landed in.
        if let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') {
            bytes.truncate(end);
        }
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr).ok();
    }
    let status = child
        .wait()
        .map_err(|error| format!("couldn't run git: {error}"))?;
    if !cut && !status.success() {
        let stderr = stderr.trim();
        return Err(if stderr.is_empty() {
            "git show failed".to_string()
        } else {
            stderr.to_string()
        });
    }
    let mut detail = parse_show(&String::from_utf8_lossy(&bytes));
    detail.truncated |= cut;
    Ok(detail)
}

/// The `git show` line the remote shell runs. Every argument is quoted: the
/// date format holds a space, which unquoted splits in two and leaves Git
/// reading `%H:%M` as a revision.
fn remote_command(repo: &str, sha: &str) -> String {
    let mut words = vec!["git".to_string(), "-C".to_string(), sh_quote(repo)];
    words.extend(SHOW_ARGS.iter().map(|arg| sh_quote(arg)));
    words.extend([sh_quote(FORMAT), sh_quote(sha), "--".to_string()]);
    words.join(" ")
}

/// The header fields, ended by [`END_OF_HEADER`], then the patch.
const FORMAT: &str = "--format=%H%x1f%an%x1f%ad%x1f%B%x1e";

/// Quoting off keeps non-ASCII paths readable; renames are detected so a
/// moved file is one entry rather than a deletion and an addition.
const SHOW_ARGS: [&str; 10] = [
    "-c",
    "core.quotepath=off",
    "show",
    "--no-color",
    "--no-ext-diff",
    "-M",
    "-m",
    "--first-parent",
    "--date=format:%Y-%m-%d %H:%M",
    "--patch",
];

/// Read what [`show_commit`] asks Git for: the header fields, a
/// [`END_OF_HEADER`], then a unified patch.
fn parse_show(output: &str) -> CommitDetail {
    let (header, patch) = output.split_once(END_OF_HEADER).unwrap_or((output, ""));
    let mut fields = header.splitn(4, FIELD);
    let mut detail = CommitDetail {
        sha: fields.next().unwrap_or_default().trim().to_string(),
        author: fields.next().unwrap_or_default().to_string(),
        date: fields.next().unwrap_or_default().to_string(),
        message: fields.next().unwrap_or_default().trim_end().to_string(),
        ..CommitDetail::default()
    };
    let mut lines_left = MAX_LINES;
    let mut current: Option<CommitFile> = None;
    // Where the next line numbers; `None` until a file's first hunk, since
    // `--- ` and `+++ ` only mean headers before it.
    let mut numbers: Option<(u32, u32)> = None;
    for raw in patch.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            detail.files.extend(current.take());
            current = Some(CommitFile::new(path_of_header(rest)));
            numbers = None;
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };
        if let Some((old, new)) = numbers.as_mut() {
            let hunk = file.hunks.last_mut();
            let (kind, text, bump) = match raw.chars().next() {
                Some('+') => (DiffLineKind::Addition, &raw[1..], (0, 1)),
                Some('-') => (DiffLineKind::Deletion, &raw[1..], (1, 0)),
                Some(' ') => (DiffLineKind::Context, &raw[1..], (1, 1)),
                // "\ No newline at end of file" annotates the line above.
                Some('\\') => continue,
                None => (DiffLineKind::Context, "", (1, 1)),
                Some(_) => {
                    // A header or a new hunk: fall out to the header rules.
                    numbers = None;
                    header_line(file, raw, &mut numbers);
                    continue;
                }
            };
            let Some(hunk) = hunk else { continue };
            if lines_left == 0 {
                detail.truncated = true;
                continue;
            }
            lines_left -= 1;
            match kind {
                DiffLineKind::Addition => file.additions += 1,
                DiffLineKind::Deletion => file.deletions += 1,
                DiffLineKind::Context => {}
            }
            hunk.lines.push(DiffLine {
                kind,
                old_line: (bump.0 > 0).then_some(*old),
                new_line: (bump.1 > 0).then_some(*new),
                text: clip(text),
            });
            *old += bump.0;
            *new += bump.1;
        } else {
            header_line(file, raw, &mut numbers);
        }
    }
    detail.files.extend(current);
    detail
}

/// Apply one line of a file's header, or open a hunk when it is one.
fn header_line(file: &mut CommitFile, raw: &str, numbers: &mut Option<(u32, u32)>) {
    if raw.starts_with("@@") {
        *numbers = hunk_start(raw);
        file.hunks.push(DiffHunk {
            header: raw.to_string(),
            lines: Vec::new(),
        });
    } else if raw.starts_with("new file mode") {
        file.status = "A".to_string();
    } else if raw.starts_with("deleted file mode") {
        file.status = "D".to_string();
    } else if let Some(from) = raw.strip_prefix("rename from ") {
        file.status = "R".to_string();
        file.old_path = from.to_string();
    } else if let Some(to) = raw.strip_prefix("rename to ") {
        file.path = to.to_string();
    } else if raw.starts_with("Binary files ") || raw.starts_with("GIT binary patch") {
        file.binary = true;
    }
}

/// `@@ -12,3 +14,5 @@ fn name` gives (12, 14). A hunk Git could not number
/// still opens, with its lines unnumbered at zero.
fn hunk_start(header: &str) -> Option<(u32, u32)> {
    let mut parts = header.split_whitespace().skip(1);
    let start = |part: Option<&str>, sign: char| -> Option<u32> {
        part?.strip_prefix(sign)?.split(',').next()?.parse().ok()
    };
    let old = start(parts.next(), '-')?;
    let new = start(parts.next(), '+')?;
    Some((old, new))
}

/// The path in `a/<path> b/<path>`. Both halves match unless the file was
/// renamed, and a rename's `rename to` line overrides this, so the equal
/// halves are the common answer and anything else falls back to the second.
fn path_of_header(rest: &str) -> String {
    let rest = rest.trim_end();
    if let Some(total) = rest.len().checked_sub(5)
        && total % 2 == 0
    {
        let half = total / 2;
        if let (Some(old), Some(new)) = (rest.get(2..2 + half), rest.get(rest.len() - half..))
            && old == new
            && rest.starts_with("a/")
        {
            return new.to_string();
        }
    }
    rest.split_once(" b/")
        .map_or_else(|| rest.to_string(), |(_, new)| new.to_string())
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_LINE_CHARS {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(MAX_LINE_CHARS).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(repo: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(output.status.success(), "{args:?}: {output:?}");
    }

    #[test]
    fn a_patch_reads_into_files_hunks_and_numbered_lines() {
        let output = "abc123\u{1f}Ada\u{1f}2026-10-09 10:00\u{1f}fix: tidy\n\nlonger body\n\u{1e}\n\
diff --git a/src/a.rs b/src/a.rs\n\
index 111..222 100644\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -3,4 +3,4 @@ fn main\n \
keep\n\
-old\n\
--- looks like a header\n\
+new\n\
+++ also content\n \
tail\n\
\\ No newline at end of file\n\
diff --git a/new.txt b/new.txt\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/new.txt\n\
@@ -0,0 +1,1 @@\n\
+hello\n\
diff --git a/img.png b/img.png\n\
Binary files a/img.png and b/img.png differ\n";
        let detail = parse_show(output);
        assert_eq!(
            (detail.sha.as_str(), detail.author.as_str()),
            ("abc123", "Ada")
        );
        assert_eq!(detail.message, "fix: tidy\n\nlonger body");
        assert_eq!(detail.files.len(), 3);

        let a = &detail.files[0];
        assert_eq!((a.path.as_str(), a.status.as_str()), ("src/a.rs", "M"));
        assert_eq!(
            (a.additions, a.deletions),
            (2, 2),
            "header lookalikes are lines"
        );
        let lines = &a.hunks[0].lines;
        assert_eq!(lines.len(), 6);
        assert_eq!((lines[0].old_line, lines[0].new_line), (Some(3), Some(3)));
        assert_eq!((lines[1].old_line, lines[1].new_line), (Some(4), None));
        assert_eq!(lines[2].text, "-- looks like a header");
        assert_eq!((lines[3].old_line, lines[3].new_line), (None, Some(4)));
        assert_eq!((lines[5].old_line, lines[5].new_line), (Some(6), Some(6)));

        assert_eq!(detail.files[1].status, "A");
        assert_eq!(detail.files[1].hunks[0].lines[0].new_line, Some(1));
        assert!(detail.files[2].binary && detail.files[2].hunks.is_empty());
        assert_eq!((detail.additions(), detail.deletions()), (3, 2));
        assert!(!detail.truncated);
    }

    #[test]
    fn renames_keep_both_paths() {
        let output = "h\u{1f}a\u{1f}d\u{1f}m\u{1e}\n\
diff --git a/old name.rs b/new name.rs\n\
similarity index 90%\n\
rename from old name.rs\n\
rename to new name.rs\n\
index 1..2 100644\n\
--- a/old name.rs\n\
+++ b/new name.rs\n\
@@ -1 +1 @@\n\
-x\n\
+y\n";
        let file = &parse_show(output).files[0];
        assert_eq!(file.status, "R");
        assert_eq!(
            (file.old_path.as_str(), file.path.as_str()),
            ("old name.rs", "new name.rs")
        );
        assert!(file.renamed());
    }

    #[test]
    fn header_path_survives_spaces_and_unequal_halves() {
        assert_eq!(
            path_of_header("a/dir/my file.rs b/dir/my file.rs"),
            "dir/my file.rs"
        );
        assert_eq!(path_of_header("a/x.rs b/y.rs"), "y.rs");
    }

    #[test]
    fn lines_past_the_bound_mark_the_commit_truncated() {
        let mut output =
            String::from("h\u{1f}a\u{1f}d\u{1f}m\u{1e}\ndiff --git a/f b/f\n@@ -0,0 +1,5000 @@\n");
        for n in 0..5000 {
            output.push_str(&format!("+line {n}\n"));
        }
        let detail = parse_show(&output);
        assert!(detail.truncated);
        assert_eq!(detail.files[0].hunks[0].lines.len(), MAX_LINES);
        assert_eq!(detail.files[0].additions as usize, MAX_LINES);
    }

    #[test]
    fn a_very_long_line_is_clipped() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), MAX_LINE_CHARS + 1);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip("short"), "short");
    }

    /// What the remote shell hands Git, word for word: a stub `git` prints its
    /// arguments one per line.
    #[test]
    fn the_remote_command_keeps_every_argument_whole() {
        let command = remote_command("/srv/my repo's", "abc1234");
        let script = format!("git() {{ printf '%s\\n' \"$@\"; }}; {command}");
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .output()
            .expect("sh runs");
        let argv: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let mut expected = vec!["-C".to_string(), "/srv/my repo's".to_string()];
        expected.extend(SHOW_ARGS.iter().map(|arg| arg.to_string()));
        expected.extend([FORMAT.to_string(), "abc1234".to_string(), "--".to_string()]);
        assert_eq!(argv, expected);
        assert!(argv.iter().any(|arg| arg == "--date=format:%Y-%m-%d %H:%M"));
    }

    #[test]
    fn anything_but_a_hash_is_refused_before_git_runs() {
        for bad in ["", "abc", "--help", "HEAD", "abc; rm -rf", "-p", "zzzzzzz"] {
            assert!(show_commit(None, ".", bad).is_err(), "{bad}");
        }
    }

    /// The real thing: a repository with an addition, a rename with an edit,
    /// a deletion, a binary file, and a merge.
    #[test]
    fn reads_a_real_repository_commit_by_commit() {
        let dir = std::env::temp_dir().join(format!("pilus-commit-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("temp dir");
        let write =
            |name: &str, bytes: &[u8]| std::fs::write(dir.join(name), bytes).expect("write");
        run(&dir, &["init", "--quiet", "-b", "main"]);
        write("a.txt", b"one\ntwo\nthree\n");
        write("gone.txt", b"bye\n");
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "--quiet", "-m", "first"]);
        run(&dir, &["mv", "a.txt", "b.txt"]);
        write("b.txt", b"one\nTWO\nthree\n");
        write("blob.bin", &[0, 159, 146, 150, 0, 1]);
        run(&dir, &["rm", "--quiet", "gone.txt"]);
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "--quiet", "-m", "second\n\nwith a body"]);
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

        let detail = show_commit(None, dir.to_str().expect("utf-8"), &sha[..7]).expect("show");
        assert_eq!(detail.sha, sha);
        assert_eq!(detail.author, "Ada");
        assert_eq!(detail.message, "second\n\nwith a body");
        let by = |path: &str| {
            detail
                .files
                .iter()
                .find(|file| file.path == path)
                .expect(path)
        };
        let moved = by("b.txt");
        assert_eq!(
            (moved.status.as_str(), moved.old_path.as_str()),
            ("R", "a.txt")
        );
        assert_eq!((moved.additions, moved.deletions), (1, 1));
        assert_eq!(by("gone.txt").status, "D");
        assert!(by("blob.bin").binary);

        // A merge shows what it brought against its first parent.
        run(&dir, &["checkout", "--quiet", "-b", "side", "HEAD~1"]);
        write("side.txt", b"side\n");
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "--quiet", "-m", "side work"]);
        run(&dir, &["checkout", "--quiet", "main"]);
        run(
            &dir,
            &["merge", "--quiet", "--no-ff", "-m", "merge side", "side"],
        );
        let tip = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .expect("rev-parse");
        let tip = String::from_utf8_lossy(&tip.stdout).trim().to_string();
        let merge = show_commit(None, dir.to_str().expect("utf-8"), &tip).expect("merge");
        assert_eq!(merge.message, "merge side");
        assert!(
            merge
                .files
                .iter()
                .any(|file| file.path == "side.txt" && file.status == "A")
        );

        assert!(show_commit(None, dir.to_str().expect("utf-8"), "deadbeef").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
