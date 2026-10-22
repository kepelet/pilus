//! Finding transcripts and reading the lines that name them.
//!
//! A machine is listed first (path, size, modified time, which is all it takes
//! to know what changed), and evidence is read only for the files that did.
//! The local reader and the remote script pick the same lines, so a parser
//! cannot tell which one fed it.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Stdio;

use pilus_client::SshTarget;

use crate::error::{Error, Result};
use crate::model::Agent;
use crate::parse::{Evidence, Reply, reply_in_line};
use crate::usage;

/// A transcript as a listing shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Probe {
    pub path: String,
    pub agent: Agent,
    pub size: u64,
    pub mtime: i64,
}

/// Markers of the lines that start a transcript: a header or a user message.
const HEAD_NEEDLES: [&str; 3] = [
    r#""type":"user""#,
    r#""type":"session""#,
    r#""role":"user""#,
];
/// Markers of the lines that rename a session, one kept per marker.
const TITLE_NEEDLES: [&str; 3] = [
    r#""type":"ai-title""#,
    r#""type":"custom-title""#,
    r#""type":"session_info""#,
];
/// How many head lines are worth reading: the first prompt is among them.
const HEAD_LINES: usize = 12;
/// A longer line is a pasted blob, not a title, and is never parsed.
const LINE_CAP: usize = 64 * 1024;
/// Files per remote read, so one answer stays a modest size.
const REMOTE_BATCH: usize = 60;

/// Every transcript under `home`.
pub fn list_local(home: &Path) -> Result<Vec<Probe>> {
    let mut found = Vec::new();
    for agent in Agent::ALL {
        let Ok(projects) = fs::read_dir(home.join(agent.root())) else {
            continue;
        };
        for project in projects.flatten() {
            let Ok(files) = fs::read_dir(project.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if let Some(probe) = probe(agent, &path) {
                    found.push(probe);
                }
            }
        }
    }
    Ok(found)
}

fn probe(agent: Agent, path: &Path) -> Option<Probe> {
    let name = path.file_name()?.to_str()?;
    if !wanted(agent, name) {
        return None;
    }
    let meta = fs::metadata(path).ok().filter(fs::Metadata::is_file)?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Probe {
        path: path.to_str()?.to_string(),
        agent,
        size: meta.len(),
        mtime: i64::try_from(mtime).ok()?,
    })
}

/// Whether `name` is a session transcript rather than a helper file: Claude's
/// older sidechain logs are named `agent-*` and are not sessions of their own.
fn wanted(agent: Agent, name: &str) -> bool {
    name.ends_with(".jsonl") && !(agent == Agent::Claude && name.starts_with("agent-"))
}

/// The evidence lines and usage turns of one local transcript, in one pass.
pub fn read_local(path: &str, agent: Agent) -> Result<Evidence> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut evidence = Evidence::default();
    let mut titles: [Option<String>; 3] = [None, None, None];
    let mut turns = Vec::new();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end();
        // An assistant line is long (it holds the reply) and is the only
        // place usage lives, so it is read whole; every other line obeys the
        // cap that keeps a pasted blob out of a title.
        if line.contains(r#""usage":"#) {
            let turn = match agent {
                Agent::Claude => usage::claude(line),
                Agent::Pi => usage::pi(line),
            };
            turns.extend(turn);
        }
        if buffer.len() > LINE_CAP {
            continue;
        }
        if evidence.head.len() < HEAD_LINES
            && HEAD_NEEDLES.iter().any(|needle| line.contains(needle))
        {
            evidence.head.push(line.to_string());
        }
        if let Some(at) = TITLE_NEEDLES
            .iter()
            .position(|needle| line.contains(needle))
        {
            titles[at] = Some(line.to_string());
        }
    }
    evidence.titles = titles.into_iter().flatten().collect();
    evidence.turns = usage::dedupe(turns);
    Ok(evidence)
}

/// How much of the end of a transcript is looked through for a reply.
const TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// The newest reply in a local transcript, found by reading only its end: a
/// transcript of many megabytes costs one seek. `None` when the end holds no
/// reply (one very long line can be larger than the window).
pub fn last_reply_local(path: &str, agent: Agent) -> Result<Option<Reply>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    // A window that starts mid-file starts mid-line.
    if start > 0 {
        lines.next();
    }
    Ok(lines
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .find_map(|line| reply_in_line(agent, line)))
}

/// Run `script` on the host with `input` on its standard input.
pub(crate) fn run_remote(target: &SshTarget, script: &str, input: &str) -> Result<Vec<u8>> {
    let mut child = target
        .shell_multiplexed(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::Remote(format!("couldn't reach the host: {error}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A script that ends early closes the pipe; its status says why.
        let _ = stdin.write_all(input.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|error| Error::Remote(error.to_string()))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(Error::Remote(if stderr.is_empty() {
        "the host refused the listing".to_string()
    } else {
        stderr
    }))
}

/// Lists `size mtime path` per transcript. `stat` spells its format flag two
/// ways (GNU and BSD), so the one that works on the host is picked once.
fn listing_script() -> String {
    let mut script = String::from(
        "if stat -c %s / >/dev/null 2>&1; then GNU=1; else GNU=; fi\n\
         list() {\n\
           [ -d \"$1\" ] || return 0\n\
           if [ -n \"$GNU\" ]; then\n\
             find \"$1\" -mindepth 2 -maxdepth 2 -type f -name '*.jsonl' -exec stat -c '%s %Y %n' {} +\n\
           else\n\
             find \"$1\" -mindepth 2 -maxdepth 2 -type f -name '*.jsonl' -exec stat -f '%z %m %N' {} +\n\
           fi\n\
         }\n",
    );
    for agent in Agent::ALL {
        script.push_str(&format!("list \"$HOME/{}\"\n", agent.root()));
    }
    script.push_str("exit 0\n");
    script
}

/// Every transcript on a remote host.
pub fn list_remote(target: &SshTarget) -> Result<Vec<Probe>> {
    let out = run_remote(target, &listing_script(), "")?;
    Ok(parse_listing(&String::from_utf8_lossy(&out)))
}

fn parse_listing(text: &str) -> Vec<Probe> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            let size = parts.next()?.parse().ok()?;
            let mtime = parts.next()?.parse().ok()?;
            let path = parts.next()?;
            let agent = Agent::from_path(path)?;
            let name = path.rsplit('/').next()?;
            wanted(agent, name).then(|| Probe {
                path: path.to_string(),
                agent,
                size,
                mtime,
            })
        })
        .collect()
}

/// Reads, for each path on stdin, the same lines [`read_local`] keeps. `grep`
/// stops early for the head and `tail` keeps the last rename of each kind, so
/// a multi-megabyte transcript costs a scan on the host and a few lines on
/// the wire. `cut` caps a pasted blob the way `LINE_CAP` does locally.
fn evidence_script() -> String {
    let mut script = String::from("while IFS= read -r p; do\n  printf '@@FILE %s\\n' \"$p\"\n");
    let head = HEAD_NEEDLES
        .iter()
        .map(|needle| format!("-e '{needle}'"))
        .collect::<Vec<_>>()
        .join(" ");
    script.push_str(&format!(
        "  grep -a -m {HEAD_LINES} {head} \"$p\" 2>/dev/null | cut -b1-{LINE_CAP}\n"
    ));
    script.push_str("  printf '@@TITLES\\n'\n");
    for needle in TITLE_NEEDLES {
        script.push_str(&format!(
            "  grep -a -e '{needle}' \"$p\" 2>/dev/null | tail -n 1 | cut -b1-4096\n"
        ));
    }
    // Usage is cut out on the host with awk, one program per agent, so a
    // transcript's message text never crosses the wire.
    script.push_str("  printf '@@USAGE\\n'\n  case \"$p\" in\n");
    script.push_str(&format!(
        "    */{claude}/*) awk '{shared}{program}' \"$p\" 2>/dev/null ;;\n",
        claude = Agent::Claude.root(),
        shared = usage::REMOTE_AWK.shared,
        program = usage::REMOTE_AWK.claude,
    ));
    script.push_str(&format!(
        "    *) awk '{shared}{program}' \"$p\" 2>/dev/null ;;\n  esac\n",
        shared = usage::REMOTE_AWK.shared,
        program = usage::REMOTE_AWK.pi,
    ));
    script.push_str("  printf '@@END\\n'\ndone\n");
    script
}

/// Evidence for `paths` on a remote host, keyed by path.
pub fn read_remote(target: &SshTarget, paths: &[String]) -> Result<Vec<(String, Evidence)>> {
    let script = evidence_script();
    let mut all = Vec::new();
    for batch in paths.chunks(REMOTE_BATCH) {
        let input: String = batch
            .iter()
            .filter(|path| !path.contains('\n'))
            .map(|path| format!("{path}\n"))
            .collect();
        let out = run_remote(target, &script, &input)?;
        all.extend(parse_evidence(&String::from_utf8_lossy(&out)));
    }
    Ok(all)
}

fn parse_evidence(text: &str) -> Vec<(String, Evidence)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Section {
        Head,
        Titles,
        Usage,
    }
    let mut found = Vec::new();
    let mut current: Option<(String, Evidence)> = None;
    let mut section = Section::Head;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("@@FILE ") {
            current = Some((path.to_string(), Evidence::default()));
            section = Section::Head;
        } else if line == "@@TITLES" {
            section = Section::Titles;
        } else if line == "@@USAGE" {
            section = Section::Usage;
        } else if line == "@@END" {
            found.extend(current.take().map(|(path, mut evidence)| {
                evidence.turns = usage::dedupe(std::mem::take(&mut evidence.turns));
                (path, evidence)
            }));
        } else if let Some((_, evidence)) = current.as_mut()
            && !line.is_empty()
        {
            match section {
                Section::Head => evidence.head.push(line.to_string()),
                Section::Titles => evidence.titles.push(line.to_string()),
                Section::Usage => evidence.turns.extend(usage::from_tsv(line)),
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_newest_reply_is_found_from_the_end_of_a_transcript() {
        let dir = std::env::temp_dir().join(format!("pilus-reply-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("t.jsonl");
        let line = |text: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
            )
        };
        let tool = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}]}}"#;
        let body = [line("first"), line("second"), tool.to_string()].join("\n");
        fs::write(&path, body + "\n").expect("write");
        let reply = last_reply_local(&path.to_string_lossy(), Agent::Claude)
            .expect("read")
            .expect("a reply");
        // The tool-only line after it says nothing, so the answer is the one before.
        assert_eq!(reply.text, "second");
        assert!(last_reply_local("/no/such/file.jsonl", Agent::Pi).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-index-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn local_listing_finds_both_agents_and_skips_helpers() {
        let home = scratch("list");
        let claude = home.join(".claude/projects/-work-app");
        let pi = home.join(".pi/agent/sessions/--work-app--");
        fs::create_dir_all(&claude).expect("claude dir");
        fs::create_dir_all(&pi).expect("pi dir");
        fs::write(claude.join("abc.jsonl"), "{}\n").expect("claude");
        fs::write(claude.join("agent-1.jsonl"), "{}\n").expect("sidechain");
        fs::write(claude.join("notes.txt"), "x").expect("other");
        fs::write(pi.join("2026_01a1.jsonl"), "{}\n").expect("pi");

        let mut found = list_local(&home).expect("list");
        found.sort_by(|a, b| a.path.cmp(&b.path));
        let kinds: Vec<_> = found.iter().map(|probe| probe.agent).collect();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(kinds.contains(&Agent::Claude) && kinds.contains(&Agent::Pi));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn local_evidence_keeps_the_head_and_the_last_title_of_each_kind() {
        let home = scratch("evidence");
        let path = home.join("s.jsonl");
        let lines = [
            r#"{"type":"mode"}"#,
            r#"{"type":"user","message":{"role":"user","content":"first"}}"#,
            r#"{"type":"ai-title","aiTitle":"old"}"#,
            r#"{"type":"assistant","message":{"role":"assistant"}}"#,
            r#"{"type":"ai-title","aiTitle":"new"}"#,
        ];
        fs::write(&path, lines.join("\n")).expect("write");
        let evidence = read_local(path.to_str().expect("utf8"), Agent::Claude).expect("read");
        assert_eq!(evidence.head, vec![lines[1].to_string()]);
        assert_eq!(evidence.titles, vec![lines[4].to_string()]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_pasted_blob_line_is_ignored() {
        let home = scratch("blob");
        let path = home.join("s.jsonl");
        let blob = format!(
            r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
            "x".repeat(LINE_CAP)
        );
        fs::write(&path, format!("{blob}\n")).expect("write");
        let evidence = read_local(path.to_str().expect("utf8"), Agent::Claude).expect("read");
        assert!(evidence.head.is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_remote_listing_is_classified_by_directory() {
        let text = "120 1790000000 /home/me/.claude/projects/-w/abc.jsonl\n\
                    9 1790000001 /home/me/.pi/agent/sessions/--w--/2026_01.jsonl\n\
                    5 1790000002 /home/me/.claude/projects/-w/agent-9.jsonl\n\
                    7 1790000003 /home/me/other/x.jsonl\n\
                    junk\n";
        let found = parse_listing(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].agent, Agent::Claude);
        assert_eq!(found[0].size, 120);
        assert_eq!(found[1].agent, Agent::Pi);
        assert_eq!(found[1].mtime, 1_790_000_001);
    }

    #[test]
    fn a_remote_answer_is_split_per_file() {
        let text = "@@FILE /a.jsonl\n{\"h\":1}\n@@TITLES\n{\"t\":1}\n@@USAGE\n\
                    m1\tmodel-a\t2026-10-09T14:15:31Z\t1\t2\t3\t4\n\
                    m1\tmodel-a\t2026-10-09T14:15:40Z\t1\t5\t3\t4\n@@END\n\
                    @@FILE /b.jsonl\n@@TITLES\n@@END\n";
        let found = parse_evidence(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "/a.jsonl");
        assert_eq!(found[0].1.head, vec!["{\"h\":1}"]);
        assert_eq!(found[0].1.titles, vec!["{\"t\":1}"]);
        // A streamed message repeats its id; only the last line counts.
        assert_eq!(found[0].1.turns.len(), 1);
        assert_eq!(found[0].1.turns[0].output, 5);
        assert!(found[1].1.head.is_empty());
    }

    #[test]
    fn the_remote_scripts_run_under_a_posix_shell() {
        // Not a parse of the output: only that sh accepts the syntax, so a typo
        // in a script nobody can run without a second machine is still caught.
        for script in [listing_script(), evidence_script()] {
            let status = std::process::Command::new("sh")
                .arg("-n")
                .arg("-c")
                .arg(&script)
                .status()
                .expect("sh");
            assert!(status.success(), "{script}");
        }
    }
}
