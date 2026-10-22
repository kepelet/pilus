//! GitHub issues and pull requests for a checkout, through the `gh` CLI.
//!
//! The daemon has a GitHub screen of its own (prefix + g) but exposes none of
//! it over UHP: its automation surface stops at `git.status`, `git.log` and
//! `git.branches`. So `pilus` asks `gh` itself, on the machine that owns the
//! checkout, the way it already asks `git`, `lsof` and `ssh`: here for this
//! Mac, over the saved SSH profile for a remote machine. The host needs
//! nothing beyond a signed-in `gh`, and [`GhRepo::probe`] says which of those
//! is missing so a window can explain instead of failing.
//!
//! Every call is one process that runs in the checkout's directory, so `gh`
//! resolves the repository from the remote exactly as it does in a terminal.
//! Text a person typed (a comment, a pull request body) reaches `gh` on stdin
//! through `--body-file -`, never on a command line, so it needs no escaping
//! and stays out of the process list.
//!
//! Everything returned is plain data that serializes to JSON, which is also
//! the shape `pilus-agent`'s hub hands the phone.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::git::sh_quote;
use crate::netlog;
use crate::transport::SshTarget;

/// How long one `gh` or `git` call may run before it is killed. `gh` talks to
/// the network, and a hung connection must not hold a poller forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A push moves objects, not a few kilobytes of JSON, so it gets longer.
const PUSH_TIMEOUT: Duration = Duration::from_secs(90);
/// The most of an issue's body that is handed to an agent as context.
const CONTEXT_BODY_LIMIT: usize = 8_000;
/// Exit status the shell gives a command it cannot find.
const NOT_FOUND: i32 = 127;
/// Exit status of the script when the checkout directory is missing.
const NO_DIRECTORY: i32 = 66;

/// The shell preamble every call starts with.
///
/// `PATH` gains the usual install directories because a window launched from
/// the Dock, and a non-interactive SSH shell, both carry almost none. The user's
/// own `PATH` still comes first, so their `gh` wins. The prompts and pager are
/// turned off so `gh` never waits on a terminal that is not there.
const PREAMBLE: &str = r#"PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:/usr/bin:$HOME/.local/bin:$HOME/.local/share/mise/shims:$HOME/.cargo/bin"
export PATH GH_PROMPT_DISABLED=1 GH_NO_UPDATE_NOTIFIER=1 GH_PAGER=cat PAGER=cat NO_COLOR=1 GIT_TERMINAL_PROMPT=0
"#;

const ISSUE_FIELDS: &str = "number,title,state,author,labels,updatedAt,url,body";
const ISSUE_DETAIL_FIELDS: &str =
    "number,title,state,author,labels,updatedAt,url,body,assignees,comments,createdAt";
const PULL_FIELDS: &str =
    "number,title,state,author,labels,updatedAt,url,isDraft,headRefName,baseRefName,reviewDecision";
const PULL_DETAIL_FIELDS: &str = "number,title,state,author,labels,updatedAt,url,isDraft,headRefName,baseRefName,reviewDecision,body,comments,additions,deletions,changedFiles,mergeable,statusCheckRollup";

/// Which items a list asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateFilter {
    Open,
    Closed,
    /// Pull requests only; an issue list reads it as closed.
    Merged,
    All,
}

impl StateFilter {
    /// The value `gh --state` takes.
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed | Self::Merged => "closed",
            Self::All => "all",
        }
    }

    fn pull_arg(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            other => other.as_arg(),
        }
    }

    /// The filter named by a wire string; anything unknown reads as open.
    pub fn from_wire(text: &str) -> Self {
        match text {
            "closed" => Self::Closed,
            "merged" => Self::Merged,
            "all" => Self::All,
            _ => Self::Open,
        }
    }
}

/// The repository `gh` resolved for a checkout.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepoInfo {
    /// `owner/name`.
    pub name: String,
    pub url: String,
    pub default_branch: String,
    /// Whether the signed-in account may push, so comment and submit controls
    /// can be offered only where they can work.
    pub can_write: bool,
    pub issues_enabled: bool,
}

/// What a probe of a checkout found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GhState {
    Ready(RepoInfo),
    /// `gh` is not installed on the machine.
    Missing,
    /// `gh` is installed but holds no login, in the shell pilus ran it in.
    /// Carries the first line `gh` said, because that shell is not the one a
    /// person logs in to: a token in a shell profile or a locked keyring
    /// reads as signed out here and signed in there.
    SignedOut(String),
    /// The directory is not a checkout with a GitHub remote; carries why.
    NoRepo(String),
    Failed(String),
}

impl GhState {
    /// A stable word for the wire.
    pub fn wire(&self) -> &'static str {
        match self {
            Self::Ready(_) => "ready",
            Self::Missing => "missing",
            Self::SignedOut(_) => "signed_out",
            Self::NoRepo(_) => "no_repo",
            Self::Failed(_) => "failed",
        }
    }

    /// A sentence saying what to do about it, or `None` when it works.
    pub fn advice(&self) -> Option<String> {
        match self {
            Self::Ready(_) => None,
            Self::Missing => Some(
                "GitHub CLI is not installed on this machine. Install it from https://cli.github.com (`brew install gh`), then run `gh auth login`."
                    .to_string(),
            ),
            Self::SignedOut(said) => {
                let mut advice = "GitHub CLI is not signed in on this machine. Run `gh auth login` in a terminal there.".to_string();
                if !said.is_empty() {
                    advice.push_str(&format!(" gh said: {said}"));
                }
                Some(advice)
            }
            Self::NoRepo(why) | Self::Failed(why) => Some(why.clone()),
        }
    }

    pub fn repo(&self) -> Option<&RepoInfo> {
        match self {
            Self::Ready(repo) => Some(repo),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
    /// Hex without the `#`.
    pub color: String,
}

/// One issue, with the body so it can be attached to a prompt without a
/// second round trip.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    /// `OPEN` or `CLOSED`.
    pub state: String,
    pub author: String,
    pub labels: Vec<Label>,
    pub updated_at: String,
    pub url: String,
    pub body: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct IssueDetail {
    pub issue: Issue,
    pub assignees: Vec<String>,
    pub created_at: String,
    pub comments: Vec<Comment>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Pull {
    pub number: u64,
    pub title: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    pub draft: bool,
    pub author: String,
    pub head: String,
    pub base: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, or empty.
    pub review: String,
    pub labels: Vec<Label>,
    pub updated_at: String,
    pub url: String,
}

/// How a pull request's checks stand.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Checks {
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PullDetail {
    pub pull: Pull,
    pub body: String,
    pub comments: Vec<Comment>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    pub mergeable: String,
    pub checks: Checks,
}

/// What submitting the checkout's branch as a pull request starts from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PullDraft {
    pub branch: String,
    /// The branch the pull request would merge into.
    pub base: String,
    /// Subject of the one commit, or the branch name made readable.
    pub title: String,
    /// The commit subjects that would be proposed, newest first.
    pub commits: Vec<String>,
    /// Whether creating it must first push the branch.
    pub needs_push: bool,
    /// The open pull request this branch already has, which replaces creating
    /// a second one.
    pub existing: Option<Pull>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewPull {
    pub title: String,
    pub body: String,
    pub base: String,
    pub draft: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CreatedPull {
    pub url: String,
    pub number: Option<u64>,
    /// Whether the branch was pushed on the way.
    pub pushed: bool,
}

/// A checkout on this Mac or on a saved machine, and everything `gh` can say
/// about its repository.
#[derive(Clone, Debug)]
pub struct GhRepo {
    target: Option<SshTarget>,
    /// The checkout, as the machine writes it (`~/code/app` is fine).
    dir: String,
    /// The `gh` to run. Tests point it at a stand-in.
    program: String,
    /// `owner/name` to address with `-R` instead of resolving a checkout, for
    /// a repository that may not be cloned on the machine at all.
    slug: Option<String>,
}

impl GhRepo {
    /// A checkout at `dir` on `target`'s host, or on this Mac for `None`.
    pub fn new(target: Option<SshTarget>, dir: impl Into<String>) -> Self {
        Self {
            target,
            dir: dir.into(),
            program: "gh".to_string(),
            slug: None,
        }
    }

    /// The repository `owner/name` on this Mac, whether or not it is cloned
    /// here. Only the calls that accept `-R` (issue and pull request reads)
    /// work on one.
    pub fn for_slug(slug: impl Into<String>) -> Self {
        Self {
            slug: Some(slug.into()),
            ..Self::new(None, "")
        }
    }

    /// Run a different `gh`. Tests point it at a stand-in; the account reads
    /// share it so one fake covers both.
    pub(crate) fn with_program(mut self, program: &str) -> Self {
        self.program = program.to_string();
        self
    }

    /// Whether `gh` works here, and which repository it resolves.
    pub fn probe(&self) -> GhState {
        let args = [
            "repo",
            "view",
            "--json",
            "nameWithOwner,url,defaultBranchRef,viewerPermission,hasIssuesEnabled",
        ];
        match self.run(&self.program, &args, None) {
            Ok(output) if output.code == Some(0) => match parse_json(&output.stdout) {
                Ok(value) => GhState::Ready(parse_repo(&value)),
                Err(error) => GhState::Failed(error),
            },
            Ok(output) => classify(output.code, &output.stderr),
            Err(error) => GhState::Failed(error),
        }
    }

    pub fn issues(&self, filter: StateFilter, limit: u32) -> Result<Vec<Issue>, String> {
        let limit = limit.to_string();
        let value = self.gh_json(&[
            "issue",
            "list",
            "--state",
            filter.as_arg(),
            "--limit",
            &limit,
            "--json",
            ISSUE_FIELDS,
        ])?;
        Ok(array(&value).iter().filter_map(parse_issue).collect())
    }

    pub fn issue(&self, number: u64) -> Result<IssueDetail, String> {
        let number = number.to_string();
        let value = self.gh_json(&["issue", "view", &number, "--json", ISSUE_DETAIL_FIELDS])?;
        Ok(IssueDetail {
            issue: parse_issue(&value).ok_or("gh returned an issue without a number")?,
            assignees: array(value.get("assignees").unwrap_or(&Value::Null))
                .iter()
                .map(|who| text(who, "login"))
                .filter(|login| !login.is_empty())
                .collect(),
            created_at: text(&value, "createdAt"),
            comments: parse_comments(&value),
        })
    }

    /// Post `body` as a comment on issue `number`; returns the comment's URL.
    pub fn comment_issue(&self, number: u64, body: &str) -> Result<String, String> {
        self.comment("issue", number, body)
    }

    pub fn pulls(&self, filter: StateFilter, limit: u32) -> Result<Vec<Pull>, String> {
        let limit = limit.to_string();
        let value = self.gh_json(&[
            "pr",
            "list",
            "--state",
            filter.pull_arg(),
            "--limit",
            &limit,
            "--json",
            PULL_FIELDS,
        ])?;
        Ok(array(&value).iter().filter_map(parse_pull).collect())
    }

    pub fn pull(&self, number: u64) -> Result<PullDetail, String> {
        let number = number.to_string();
        let value = self.gh_json(&["pr", "view", &number, "--json", PULL_DETAIL_FIELDS])?;
        parse_pull_detail(&value)
            .ok_or_else(|| "gh returned a pull request without a number".into())
    }

    pub fn comment_pull(&self, number: u64, body: &str) -> Result<String, String> {
        self.comment("pr", number, body)
    }

    /// What submitting the current branch would do.
    pub fn pull_draft(&self) -> Result<PullDraft, String> {
        let branch = self.git_line(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        if branch == "HEAD" {
            return Err("The checkout is on a detached HEAD; switch to a branch first.".into());
        }
        let base = self.default_branch()?;
        if branch == base {
            return Err(format!(
                "You're on `{base}`; switch to a branch with changes to submit a pull request."
            ));
        }
        // Compare against the remote's copy of the base when there is one: a
        // local base that was never updated would list commits already merged.
        let remote_base = format!("origin/{base}");
        let against = if self
            .git(&["rev-parse", "--verify", "--quiet", &remote_base])
            .is_ok()
        {
            remote_base
        } else {
            base.clone()
        };
        let range = format!("{against}..HEAD");
        let commits: Vec<String> = self
            .git(&["log", "--format=%s", &range])
            .map(|log| log.lines().map(str::to_string).collect())
            .unwrap_or_default();
        let title = match commits.as_slice() {
            [only] => only.clone(),
            _ => readable_branch(&branch),
        };
        let existing = self.pull_for_branch(&branch);
        Ok(PullDraft {
            needs_push: self.needs_push(),
            branch,
            base,
            title,
            commits,
            existing,
        })
    }

    /// Open a pull request from the current branch, pushing it first when the
    /// remote does not have its commits yet. Only that one branch is pushed.
    pub fn create_pull(&self, new: &NewPull) -> Result<CreatedPull, String> {
        let title = new.title.trim();
        if title.is_empty() {
            return Err("A pull request needs a title.".to_string());
        }
        let branch = self.git_line(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        if branch == "HEAD" || branch == new.base {
            return Err(format!(
                "`{branch}` can't be submitted into `{}`; switch to another branch.",
                new.base
            ));
        }
        let pushed = self.needs_push();
        if pushed {
            let remote = self.push_remote(&branch);
            let args = ["push", "--set-upstream", &remote, "HEAD"];
            let output = self.run_for(PUSH_TIMEOUT, "git", &args, None)?;
            if output.code != Some(0) {
                return Err(failure("git push", &output));
            }
        }
        let mut args = vec![
            "pr",
            "create",
            "--title",
            title,
            "--body-file",
            "-",
            "--base",
            &new.base,
            "--head",
            &branch,
        ];
        if new.draft {
            args.push("--draft");
        }
        let output = self.run(&self.program, &args, Some(&new.body))?;
        if output.code != Some(0) {
            return Err(failure("gh pr create", &output));
        }
        // `gh` prints the new pull request's URL as the last line.
        let url = output
            .stdout
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| line.starts_with("http"))
            .unwrap_or_default()
            .to_string();
        Ok(CreatedPull {
            number: number_from_url(&url),
            url,
            pushed,
        })
    }

    fn comment(&self, kind: &str, number: u64, body: &str) -> Result<String, String> {
        if body.trim().is_empty() {
            return Err("A comment can't be empty.".to_string());
        }
        let number = number.to_string();
        let output = self.run(
            &self.program,
            &[kind, "comment", &number, "--body-file", "-"],
            Some(body),
        )?;
        if output.code != Some(0) {
            return Err(failure(&format!("gh {kind} comment"), &output));
        }
        Ok(output.stdout.trim().to_string())
    }

    fn default_branch(&self) -> Result<String, String> {
        let value = self.gh_json(&["repo", "view", "--json", "defaultBranchRef"])?;
        let name = value
            .get("defaultBranchRef")
            .map(|branch| text(branch, "name"))
            .unwrap_or_default();
        if name.is_empty() {
            return Err("The repository has no default branch yet.".to_string());
        }
        Ok(name)
    }

    /// The open pull request whose head is `branch`.
    fn pull_for_branch(&self, branch: &str) -> Option<Pull> {
        let value = self
            .gh_json(&[
                "pr",
                "list",
                "--head",
                branch,
                "--state",
                "open",
                "--limit",
                "1",
                "--json",
                PULL_FIELDS,
            ])
            .ok()?;
        array(&value).iter().find_map(parse_pull)
    }

    /// Whether the branch has commits the remote lacks, or no remote branch.
    fn needs_push(&self) -> bool {
        match self.git(&["rev-list", "--count", "@{upstream}..HEAD"]) {
            Ok(count) => count.trim() != "0",
            Err(_) => true,
        }
    }

    /// Where the branch is pushed: its upstream's remote, else `origin`.
    fn push_remote(&self, branch: &str) -> String {
        self.git_line(&["config", "--get", &format!("branch.{branch}.remote")])
            .ok()
            .filter(|remote| !remote.is_empty())
            .unwrap_or_else(|| "origin".to_string())
    }

    fn gh_json(&self, args: &[&str]) -> Result<Value, String> {
        let mut args = args.to_vec();
        if let Some(slug) = self.slug.as_deref() {
            args.extend(["-R", slug]);
        }
        let output = self.run(&self.program, &args, None)?;
        if output.code != Some(0) {
            // A checkout with several remotes (a fork and its upstream) has no
            // default repository until someone runs `gh repo set-default`, so
            // `gh` refuses to guess. `origin` is the one the person pushes to.
            if self.slug.is_none()
                && needs_default_repo(&output.stderr)
                && let Ok(slug) = self.origin_slug()
            {
                args.extend(["-R", slug.as_str()]);
                let retry = self.run(&self.program, &args, None)?;
                if retry.code == Some(0) {
                    return parse_json(&retry.stdout);
                }
                return Err(self.explain(&retry));
            }
            return Err(self.explain(&output));
        }
        parse_json(&output.stdout)
    }

    /// `owner/name` of the checkout's `origin` remote when that is on
    /// github.com, whichever machine the checkout is on. With it the repository
    /// can be read through *this* Mac's `gh` login (`-R`), so a checkout on a
    /// machine where `gh` is not signed in still lists its issues.
    pub fn origin_slug(&self) -> Result<String, String> {
        let url = self.git_line(&["remote", "get-url", "origin"])?;
        slug_of_github_remote(&url)
            .ok_or_else(|| format!("This checkout's remote is not on github.com: {url}"))
    }

    fn git(&self, args: &[&str]) -> Result<String, String> {
        let output = self.run("git", args, None)?;
        if output.code != Some(0) {
            return Err(failure("git", &output));
        }
        Ok(output.stdout)
    }

    fn git_line(&self, args: &[&str]) -> Result<String, String> {
        self.git(args).map(|out| out.trim().to_string())
    }

    /// A failed call as one sentence: the probe's wording when it is a
    /// setup problem, `gh`'s own words otherwise.
    fn explain(&self, output: &Output) -> String {
        match classify(output.code, &output.stderr) {
            GhState::Failed(_) => failure("gh", output),
            state => state.advice().unwrap_or_default(),
        }
    }

    /// Run `program args` in the checkout and capture it, killing it after
    /// [`CALL_TIMEOUT`] (or `timeout` for [`Self::run_for`]).
    pub(crate) fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<Output, String> {
        self.run_for(CALL_TIMEOUT, program, args, stdin)
    }

    fn run_for(
        &self,
        timeout: Duration,
        program: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<Output, String> {
        let script = script(&self.dir, program, args);
        let mut command = match &self.target {
            None => {
                let mut command = Command::new("/bin/sh");
                command.arg("-c").arg(&script);
                command
            }
            Some(target) => target.shell_multiplexed(&script),
        };
        command
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|error| format!("couldn't reach the host: {error}"))?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A process that exits before reading closes the pipe; its exit
            // status says why, so the write error is not the story.
            let _ = pipe.write_all(input.as_bytes());
        }
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("GitHub did not answer in time.".to_string());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(15)),
                Err(error) => return Err(format!("couldn't wait for gh: {error}")),
            }
        };
        // Only `gh` talks to GitHub; the `git` calls beside it are local.
        if program == "gh" {
            let host = self
                .target
                .as_ref()
                .map_or("local", |t| t.destination.as_str());
            let what = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
            netlog::record(netlog::Kind::Github, host, &what, started, status.success());
        }
        Ok(Output {
            code: status.code(),
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        })
    }
}

pub(crate) struct Output {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Read a pipe to its end on its own thread, so a full pipe cannot stall the
/// child while the caller waits on its exit.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// The shell script that runs `program args` in `dir`.
fn script(dir: &str, program: &str, args: &[&str]) -> String {
    let mut script = String::from(PREAMBLE);
    if !dir.trim().is_empty() {
        script.push_str(&format!(
            "dir={}\ncase $dir in \"~\") dir=$HOME;; \"~/\"*) dir=$HOME/${{dir#\"~/\"}};; esac\ncd -- \"$dir\" 2>/dev/null || {{ echo \"no such directory: $dir\" >&2; exit {NO_DIRECTORY}; }}\n",
            sh_quote(dir.trim())
        ));
    }
    script.push_str("exec ");
    script.push_str(&sh_quote(program));
    for arg in args {
        script.push(' ');
        script.push_str(&sh_quote(arg));
    }
    script
}

/// Turn a failed `gh repo view` into the state it means.
///
/// The order matters: the "no GitHub remote" message also tells the reader to
/// run `gh auth login`, and it must not read as signed out.
/// Whether `gh` stopped for want of a default repository, which an explicit
/// `-R` answers.
fn needs_default_repo(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("set-default") || lower.contains("multiple remotes")
}

/// The host and `owner/name` of a remote URL: `git@github.com:o/r.git`,
/// `https://github.com/o/r`, or `ssh://git@github.com/o/r.git`.
fn remote_parts(url: &str) -> Option<(String, String)> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let (host, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => url.split_once(':')?,
    };
    // `git@` and `user:password@` are not part of the host.
    let host = host.rsplit('@').next()?;
    let mut parts = path.trim_start_matches('/').split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (!host.is_empty() && !owner.is_empty() && !name.is_empty() && parts.next().is_none())
        .then(|| (host.to_lowercase(), format!("{owner}/{name}")))
}

/// `owner/name` from a remote URL on github.com, the host `-R` addresses.
fn slug_of_github_remote(url: &str) -> Option<String> {
    remote_parts(url)
        .filter(|(host, _)| host == "github.com")
        .map(|(_, slug)| slug)
}

pub(crate) fn classify(code: Option<i32>, stderr: &str) -> GhState {
    let message = stderr.trim();
    let lower = message.to_lowercase();
    if code == Some(NOT_FOUND) {
        return GhState::Missing;
    }
    let first = message.lines().next().unwrap_or_default().to_string();
    if code == Some(NO_DIRECTORY) {
        return GhState::NoRepo(first);
    }
    if lower.contains("not a git repository") {
        return GhState::NoRepo("This folder is not a git repository.".to_string());
    }
    if lower.contains("no git remotes") {
        return GhState::NoRepo(
            "This repository has no remote to find a GitHub project from.".to_string(),
        );
    }
    if lower.contains("none of the git remotes") {
        return GhState::NoRepo(
            "None of this repository's remotes point at a GitHub host `gh` is signed in to."
                .to_string(),
        );
    }
    if lower.contains("gh auth login")
        || lower.contains("not logged in")
        || lower.contains("http 401")
        || lower.contains("bad credentials")
        || lower.contains("not logged into")
    {
        return GhState::SignedOut(first);
    }
    GhState::Failed(if first.is_empty() {
        "gh failed without saying why.".to_string()
    } else {
        first
    })
}

pub(crate) fn failure(what: &str, output: &Output) -> String {
    let stderr = output.stderr.trim();
    if stderr.is_empty() {
        format!(
            "{what} failed{}",
            output
                .code
                .map(|code| format!(" (exit {code})"))
                .unwrap_or_default()
        )
    } else {
        stderr.to_string()
    }
}

pub(crate) fn parse_json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|error| format!("gh answered something unexpected: {error}"))
}

pub(crate) fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

pub(crate) fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub(crate) fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// `author.login`, which `gh` nests; a deleted account has none.
fn login(value: &Value) -> String {
    value
        .get("author")
        .map(|author| text(author, "login"))
        .filter(|login| !login.is_empty())
        .unwrap_or_else(|| "ghost".to_string())
}

pub(crate) fn parse_labels(value: &Value) -> Vec<Label> {
    array(value.get("labels").unwrap_or(&Value::Null))
        .iter()
        .map(|label| Label {
            name: text(label, "name"),
            color: text(label, "color"),
        })
        .filter(|label| !label.name.is_empty())
        .collect()
}

fn parse_repo(value: &Value) -> RepoInfo {
    let permission = text(value, "viewerPermission");
    RepoInfo {
        name: text(value, "nameWithOwner"),
        url: text(value, "url"),
        default_branch: value
            .get("defaultBranchRef")
            .map(|branch| text(branch, "name"))
            .unwrap_or_default(),
        can_write: matches!(permission.as_str(), "WRITE" | "MAINTAIN" | "ADMIN"),
        issues_enabled: value
            .get("hasIssuesEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    }
}

fn parse_issue(value: &Value) -> Option<Issue> {
    Some(Issue {
        number: value.get("number")?.as_u64()?,
        title: text(value, "title"),
        state: text(value, "state"),
        author: login(value),
        labels: parse_labels(value),
        updated_at: text(value, "updatedAt"),
        url: text(value, "url"),
        body: text(value, "body").replace("\r\n", "\n"),
    })
}

fn parse_pull(value: &Value) -> Option<Pull> {
    Some(Pull {
        number: value.get("number")?.as_u64()?,
        title: text(value, "title"),
        state: text(value, "state"),
        draft: value
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        author: login(value),
        head: text(value, "headRefName"),
        base: text(value, "baseRefName"),
        review: text(value, "reviewDecision"),
        labels: parse_labels(value),
        updated_at: text(value, "updatedAt"),
        url: text(value, "url"),
    })
}

fn parse_pull_detail(value: &Value) -> Option<PullDetail> {
    Some(PullDetail {
        pull: parse_pull(value)?,
        body: text(value, "body").replace("\r\n", "\n"),
        comments: parse_comments(value),
        additions: count(value, "additions"),
        deletions: count(value, "deletions"),
        changed_files: count(value, "changedFiles"),
        mergeable: text(value, "mergeable"),
        checks: tally_checks(value.get("statusCheckRollup").unwrap_or(&Value::Null)),
    })
}

/// Comments in order, without the ones GitHub hid as spam or abuse.
fn parse_comments(value: &Value) -> Vec<Comment> {
    array(value.get("comments").unwrap_or(&Value::Null))
        .iter()
        .filter(|comment| {
            !comment
                .get("isMinimized")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .map(|comment| Comment {
            author: login(comment),
            body: text(comment, "body").replace("\r\n", "\n"),
            created_at: text(comment, "createdAt"),
            url: text(comment, "url"),
        })
        .collect()
}

/// Fold `statusCheckRollup` into counts. A check run reports a `conclusion`
/// once it completes; a commit status reports a `state`.
fn tally_checks(rollup: &Value) -> Checks {
    let mut checks = Checks::default();
    for item in array(rollup) {
        let verdict = {
            let conclusion = text(item, "conclusion");
            let state = text(item, "state");
            let status = text(item, "status");
            if !conclusion.is_empty() {
                conclusion
            } else if !state.is_empty() {
                state
            } else {
                status
            }
        };
        match verdict.as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => checks.passed += 1,
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" => checks.failed += 1,
            _ => checks.pending += 1,
        }
    }
    checks
}

/// `feat/add-login_form` → `Add login form`.
fn readable_branch(branch: &str) -> String {
    let tail = branch.rsplit('/').next().unwrap_or(branch);
    let spaced = tail.replace(['-', '_'], " ");
    let mut chars = spaced.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => branch.to_string(),
    }
}

/// Seconds since the Unix epoch for a UTC RFC 3339 timestamp such as
/// `2026-10-08T19:21:25Z`, which is how `gh` words every time. `None` for
/// anything else, so a caller shows no age rather than a wrong one.
pub fn unix_seconds(timestamp: &str) -> Option<u64> {
    let (date, time) = timestamp.trim().strip_suffix('Z')?.split_once('T')?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let mut time = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from the civil calendar (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

fn number_from_url(url: &str) -> Option<u64> {
    url.rsplit('/').next()?.parse().ok()
}

impl Issue {
    /// The issue as context for an agent's prompt: what it is, where it lives,
    /// and what it says. Shared by every surface that hands an issue to an
    /// agent, so they all word it the same way.
    pub fn as_context(&self) -> String {
        let mut context = format!("GitHub issue #{}: {}", self.number, self.title);
        if !self.url.is_empty() {
            context.push('\n');
            context.push_str(&self.url);
        }
        let body = self.body.trim();
        if !body.is_empty() {
            context.push_str("\n\n");
            match body.char_indices().nth(CONTEXT_BODY_LIMIT) {
                Some((cut, _)) => {
                    context.push_str(&body[..cut]);
                    context.push_str("\n… (issue body truncated)");
                }
                None => context.push_str(body),
            }
        }
        context
    }

    /// `prompt` with this issue attached beneath it.
    pub fn attach_to(&self, prompt: &str) -> String {
        format!("{}\n\n---\n{}", prompt.trim_end(), self.as_context())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_remote_url_names_its_repository() {
        for url in [
            "git@github.com:kepelet/pilus.git",
            "https://github.com/kepelet/pilus",
            "https://github.com/kepelet/pilus.git/",
            "ssh://git@github.com/kepelet/pilus.git",
        ] {
            assert_eq!(
                slug_of_github_remote(url).as_deref(),
                Some("kepelet/pilus"),
                "{url}"
            );
        }
        assert_eq!(slug_of_github_remote("/srv/git/pilus.git"), None);
        assert_eq!(
            slug_of_github_remote("git@github.com:kepelet/pilus.git").as_deref(),
            Some("kepelet/pilus")
        );
        // Another host, or an alias for one, is not something `-R` can address.
        assert_eq!(
            slug_of_github_remote("https://seed.radicle.dev/z4V1/x.git"),
            None
        );
        assert_eq!(slug_of_github_remote("git@github.com-work:o/r.git"), None);
        assert_eq!(slug_of_github_remote("https://github.com/only"), None);
    }

    #[test]
    fn only_a_missing_default_repository_is_retried() {
        assert!(needs_default_repo(
            "X No default remote repository has been set. To learn more about the default repository, run: gh repo set-default --help"
        ));
        assert!(needs_default_repo("Multiple remotes detected."));
        assert!(!needs_default_repo("HTTP 401: Bad credentials"));
    }

    use super::*;
    use serde_json::json;

    fn issue() -> Issue {
        Issue {
            number: 42,
            title: "Login fails".to_string(),
            state: "OPEN".to_string(),
            author: "sam".to_string(),
            labels: Vec::new(),
            updated_at: String::new(),
            url: "https://github.com/o/r/issues/42".to_string(),
            body: "Steps:\n1. open".to_string(),
        }
    }

    #[test]
    fn issue_parses_gh_json_and_defaults_a_deleted_author() {
        let value = json!({
            "number": 7, "title": "Hi", "state": "OPEN", "url": "u", "updatedAt": "t",
            "body": "a\r\nb",
            "author": { "login": "" },
            "labels": [{ "name": "bug", "color": "D6393F", "id": "x" }, { "name": "" }],
        });
        let parsed = parse_issue(&value).expect("issue");
        assert_eq!(parsed.author, "ghost");
        assert_eq!(parsed.body, "a\nb");
        assert_eq!(parsed.labels.len(), 1);
        assert_eq!(parsed.labels[0].name, "bug");
        assert!(parse_issue(&json!({ "title": "no number" })).is_none());
    }

    #[test]
    fn minimized_comments_are_dropped() {
        let value = json!({ "comments": [
            { "author": { "login": "a" }, "body": "ok", "createdAt": "1", "url": "u1" },
            { "author": { "login": "spam" }, "body": "buy", "isMinimized": true },
        ]});
        let comments = parse_comments(&value);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "a");
    }

    #[test]
    fn checks_fold_runs_and_statuses() {
        let rollup = json!([
            { "__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS" },
            { "__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE" },
            { "__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": "" },
            { "__typename": "StatusContext", "state": "PENDING" },
            { "__typename": "StatusContext", "state": "SUCCESS" },
        ]);
        assert_eq!(
            tally_checks(&rollup),
            Checks {
                passed: 2,
                failed: 1,
                pending: 2
            }
        );
        assert_eq!(tally_checks(&Value::Null), Checks::default());
    }

    #[test]
    fn repo_write_access_follows_the_viewer_permission() {
        let write = |permission: &str| {
            parse_repo(&json!({ "viewerPermission": permission, "nameWithOwner": "o/r" })).can_write
        };
        assert!(write("ADMIN") && write("WRITE") && write("MAINTAIN"));
        assert!(!write("READ") && !write("TRIAGE"));
    }

    #[test]
    fn failures_are_told_apart_by_what_gh_says() {
        assert_eq!(classify(Some(127), "sh: gh: not found"), GhState::Missing);
        assert_eq!(
            classify(
                Some(4),
                "To get started with GitHub CLI, please run:  gh auth login"
            ),
            GhState::SignedOut(
                "To get started with GitHub CLI, please run:  gh auth login".to_string()
            )
        );
        // The no-remote message also mentions `gh auth login`; it is not a sign-out.
        assert!(matches!(
            classify(
                Some(1),
                "none of the git remotes configured for this repository point to a known GitHub host. To tell gh about a new GitHub host, please use `gh auth login`"
            ),
            GhState::NoRepo(_)
        ));
        assert!(matches!(
            classify(Some(1), "no git remotes found"),
            GhState::NoRepo(_)
        ));
        assert!(matches!(
            classify(Some(1), "failed to run git: fatal: not a git repository"),
            GhState::NoRepo(_)
        ));
        assert!(matches!(
            classify(Some(NO_DIRECTORY), "no such directory: /x"),
            GhState::NoRepo(_)
        ));
        assert_eq!(
            classify(Some(1), "GraphQL: rate limited\nmore"),
            GhState::Failed("GraphQL: rate limited".to_string())
        );
    }

    #[test]
    fn script_quotes_every_argument_and_expands_a_tilde_dir() {
        let script = script("~/my code", "gh", &["issue", "view", "1; rm -rf /", "it's"]);
        assert!(script.contains("dir='~/my code'"));
        assert!(script.contains("exec 'gh' 'issue' 'view' '1; rm -rf /' 'it'\\''s'"));
        assert!(script.contains("exit 66"));
        // No directory means no `cd`.
        assert!(!super::script("", "gh", &[]).contains("cd --"));
    }

    #[test]
    fn github_timestamps_become_epoch_seconds() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2026-10-08T19:21:25Z"), Some(1_791_487_285));
        // A leap day, and a date before March in the shifted-year branch.
        assert_eq!(unix_seconds("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(unix_seconds("2026-10-08 19:21:25"), None);
        assert_eq!(unix_seconds("2026-13-08T19:21:25Z"), None);
        assert_eq!(unix_seconds(""), None);
    }

    #[test]
    fn branch_names_read_as_titles() {
        assert_eq!(readable_branch("feat/add-login_form"), "Add login form");
        assert_eq!(readable_branch("main"), "Main");
        assert_eq!(number_from_url("https://github.com/o/r/pull/12"), Some(12));
        assert_eq!(number_from_url("nonsense"), None);
    }

    #[test]
    fn an_issue_attaches_beneath_the_prompt_and_long_bodies_are_cut() {
        let attached = issue().attach_to("Fix this.  \n");
        assert_eq!(
            attached,
            "Fix this.\n\n---\nGitHub issue #42: Login fails\nhttps://github.com/o/r/issues/42\n\nSteps:\n1. open"
        );
        let long = Issue {
            body: "é".repeat(CONTEXT_BODY_LIMIT + 10),
            ..issue()
        };
        let context = long.as_context();
        assert!(context.ends_with("(issue body truncated)"));
        assert_eq!(context.matches('é').count(), CONTEXT_BODY_LIMIT);
    }

    /// A stand-in `gh` and `git` that record how they were called, so the
    /// write paths run for real without touching GitHub.
    #[cfg(unix)]
    mod fake {
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};

        pub fn dir(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("pilus-gh-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            dir
        }

        pub fn program(dir: &Path, name: &str, body: &str) -> String {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            path.to_string_lossy().into_owned()
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_comment_reaches_gh_on_stdin_and_never_on_the_command_line() {
        let dir = fake::dir("comment");
        let log = dir.join("log");
        let gh = fake::program(
            &dir,
            "gh",
            &format!(
                "echo \"$@\" > {log}\ncat >> {log}\necho https://github.com/o/r/issues/3#issuecomment-9",
                log = log.display()
            ),
        );
        let repo = GhRepo::new(None, dir.to_string_lossy()).with_program(&gh);
        let url = repo
            .comment_issue(3, "line one\n$(touch pwned) 'two'")
            .expect("comment");
        assert_eq!(url, "https://github.com/o/r/issues/3#issuecomment-9");
        let recorded = std::fs::read_to_string(&log).expect("log");
        assert!(
            recorded.starts_with("issue comment 3 --body-file -\n"),
            "{recorded}"
        );
        assert!(recorded.contains("line one\n$(touch pwned) 'two'"));
        assert!(repo.comment_issue(3, "   ").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn probe_reports_a_missing_directory_and_a_missing_gh() {
        let dir = fake::dir("probe");
        let gone = GhRepo::new(None, dir.join("nope").to_string_lossy());
        assert!(matches!(gone.probe(), GhState::NoRepo(_)));
        let absent = GhRepo::new(None, dir.to_string_lossy()).with_program("pilus-no-such-gh");
        assert_eq!(absent.probe(), GhState::Missing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_call_surfaces_ghs_message() {
        let dir = fake::dir("fail");
        let gh = fake::program(
            &dir,
            "gh",
            "echo 'GraphQL: Could not resolve to an issue' >&2; exit 1",
        );
        let repo = GhRepo::new(None, dir.to_string_lossy()).with_program(&gh);
        assert_eq!(
            repo.issue(9).expect_err("fails"),
            "GraphQL: Could not resolve to an issue"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
