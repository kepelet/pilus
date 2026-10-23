//! The signed-in GitHub account's repositories, and each repository's open
//! issues, open pull requests and workflow runs, read through `gh` on this
//! machine and cached so the sidebar never costs a request it does not need.
//!
//! [`github`](crate::github) answers about one checkout; this answers about the
//! person: the repositories they pushed to lately, with stars and what is open
//! in each, and what is going on inside one. It only ever runs `gh` on the
//! machine `pilus` runs on, because the account is the host's, not a remote's.
//!
//! GitHub's limits shape the whole module. Search has its own 30-a-minute
//! budget, and a window that polls would spend it. So every list goes through
//! one cache with four rules:
//!
//! 1. **Fresh is free.** A list younger than [`TTL`] is served without a call.
//! 2. **Stale revalidates.** An older REST list is asked for again with its
//!    `ETag`; GitHub answers `304` for an unchanged one, which costs nothing
//!    against the limit. The repository list is one GraphQL query (it is the
//!    only way to get stars and both open counts for fifty repositories in a
//!    single request), which has no `304` but costs one point of the hourly
//!    budget, once per lifetime.
//! 3. **Disk survives a relaunch.** The cache lives in `pilus.db` (the
//!    `github_cache` table and the rate-limit back-off), so opening the app
//!    re-shows the last lists at once and only revalidates. The first open
//!    reads the `github-cache.json` older versions kept and sets it aside.
//! 4. **A limit is respected.** On a rate-limit answer the module stops
//!    calling until GitHub says the window resets, and serves what it holds.
//!
//! A failure with a list on hand is never an empty sidebar: the saved list is
//! returned with a `note` saying why it is old.
//!
//! Detail reads (a pull request's checks, an issue's comments) are
//! kept in memory for [`DETAIL_TTL`], enough that reopening a row is instant
//! without making a stale thread look current.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pilus_db::Db;
use pilus_db::rusqlite::params;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::github::{
    GhRepo, GhState, IssueDetail, PullDetail, array, classify, count, parse_json, text,
};

/// How long a list is trusted before it is revalidated.
const TTL: Duration = Duration::from_secs(300);
/// The shortest gap between two forced refreshes of one list, so a held
/// refresh button cannot hammer GitHub.
const MIN_FORCED_GAP: u64 = 15;
/// How long an in-memory detail read is trusted.
const DETAIL_TTL: Duration = Duration::from_secs(120);
/// How long to stand down after a rate-limit answer that names no reset.
const DEFAULT_BACKOFF: u64 = 60;
/// Items asked for per list. One page: the sidebar is a glance, not a browser.
const PER_PAGE: u32 = 30;
/// Repositories asked for: enough that a pinned one is almost always in it.
const REPO_LIMIT: u32 = 50;
/// The issues endpoint lists pull requests too, so it asks for more.
const ISSUE_PAGE: u32 = 60;
/// How long the account's login is trusted. It rarely changes, and a
/// revalidation costs nothing, so this only bounds a stale sign-out.
const LOGIN_TTL: Duration = Duration::from_secs(3_600);

/// What a probe of the account found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccountState {
    Ready {
        login: String,
    },
    /// `gh` is missing, signed out, or failing; says which and why.
    Unavailable(GhState),
}

impl AccountState {
    /// Whether `gh` is not installed at all, which hides the integration
    /// rather than showing a prompt to install something unasked.
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Unavailable(GhState::Missing))
    }

    /// A sentence saying what to do about it, or `None` when it works.
    pub fn advice(&self) -> Option<String> {
        match self {
            Self::Ready { .. } => None,
            Self::Unavailable(state) => state.advice(),
        }
    }
}

/// One repository the account can push to or belongs to.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepoSummary {
    /// `owner/name`.
    pub name: String,
    pub description: String,
    pub private: bool,
    pub fork: bool,
    pub archived: bool,
    pub language: String,
    pub stars: u64,
    /// Open issues, not counting pull requests.
    #[serde(default)]
    pub open_issues: u64,
    #[serde(default)]
    pub open_pulls: u64,
    pub pushed_at: String,
    pub url: String,
    pub default_branch: String,
}

/// An open issue or pull request in a repository's list.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ItemSummary {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub draft: bool,
    /// The branch a pull request comes from; empty for an issue.
    pub branch: String,
    pub updated_at: String,
    /// Comments on an issue. A pull request list does not say, so it is 0.
    pub comments: u64,
    pub url: String,
}

/// One workflow run of a repository's Actions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: u64,
    /// The workflow's name.
    pub workflow: String,
    /// The commit subject or pull request title that started it.
    pub title: String,
    pub number: u64,
    /// `queued`, `in_progress` or `completed`.
    pub status: String,
    /// `success`, `failure`, `cancelled`, ... once completed; empty before.
    pub conclusion: String,
    pub branch: String,
    /// What started it: `push`, `pull_request`, `schedule`, ...
    pub event: String,
    pub actor: String,
    pub updated_at: String,
    pub url: String,
}

/// A list and how current it is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Listing<T> {
    pub items: Vec<T>,
    /// Unix seconds the list was last confirmed with GitHub.
    pub fetched_at: u64,
    /// Why the list is older than it should be (a rate limit, no network),
    /// when it is. The items are still the last good ones.
    pub note: Option<String>,
}

#[derive(Clone, Deserialize)]
struct Entry {
    fetched_at: u64,
    etag: String,
    /// The list, as the JSON of its typed items, so a `304` can be answered
    /// from disk without keeping GitHub's much larger search payload.
    body: String,
}

/// The cache in memory, read from `pilus.db` when the account opens. It is also
/// the shape of the old `github-cache.json`, which is read once to import.
#[derive(Default, Deserialize)]
struct Store {
    #[serde(default)]
    entries: BTreeMap<String, Entry>,
    /// Unix seconds before which no call is made, after a rate-limit answer.
    #[serde(default)]
    backoff_until: u64,
}

/// The `docs` row holding [`Store::backoff_until`].
const BACKOFF_DOC: &str = "github_backoff";
/// Marks `github-cache.json` as read into the database.
const IMPORT: &str = "github_cache";

impl Store {
    fn read(db: &Db) -> Self {
        let read = || -> Result<Self, pilus_db::Error> {
            let entries = db.with(|conn| {
                let mut statement =
                    conn.prepare("SELECT key, fetched_at, etag, body FROM github_cache")?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        Entry {
                            fetched_at: row.get::<_, i64>(1)?.unsigned_abs(),
                            etag: row.get(2)?,
                            body: row.get(3)?,
                        },
                    ))
                })?;
                Ok(rows.collect::<Result<BTreeMap<_, _>, _>>()?)
            })?;
            Ok(Self {
                entries,
                backoff_until: db.doc(BACKOFF_DOC)?.unwrap_or(0),
            })
        };
        read().unwrap_or_else(|error| {
            tracing::warn!("could not read the GitHub cache: {error}");
            Self::default()
        })
    }

    /// Read `path`, the old cache file, into `db` the first time and set it
    /// aside. It is only a cache, so one that does not parse is just skipped.
    fn import_legacy(db: &Db, path: &Path) {
        if !path.is_file() || db.imported(IMPORT) {
            return;
        }
        let Some(old) = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
        else {
            return;
        };
        let imported = db.import_once(IMPORT, |tx| {
            for (key, entry) in &old.entries {
                write_entry(tx, key, entry)?;
            }
            pilus_db::put_doc_in(tx, BACKOFF_DOC, &old.backoff_until)
        });
        match imported {
            Ok(_) => pilus_db::retire(path),
            Err(error) => tracing::warn!("could not import {}: {error}", path.display()),
        }
    }
}

fn write_entry(
    conn: &pilus_db::rusqlite::Connection,
    key: &str,
    entry: &Entry,
) -> Result<(), pilus_db::Error> {
    conn.execute(
        "INSERT OR REPLACE INTO github_cache (key, fetched_at, etag, body) VALUES (?1, ?2, ?3, ?4)",
        params![key, entry.fetched_at as i64, entry.etag, entry.body],
    )?;
    Ok(())
}

#[derive(Clone)]
enum Detail {
    Pull(Box<PullDetail>),
    Issue(Box<IssueDetail>),
}

/// What to ask GitHub.
enum Call {
    /// A REST path, which can be revalidated by `ETag`.
    Rest(String),
    /// A GraphQL query, which cannot.
    Graphql(String),
}

/// What `gh api -i` printed: the status, the headers and the body.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether this answer says the rate limit, not the request, is the
    /// problem. GitHub uses 403 for it as often as 429.
    fn rate_limited(&self) -> bool {
        match self.status {
            429 => true,
            403 => {
                self.header("x-ratelimit-remaining") == Some("0")
                    || self.header("retry-after").is_some()
                    || self.body.to_lowercase().contains("rate limit")
            }
            _ => false,
        }
    }

    /// Unix seconds this answer says to wait until, or a default.
    fn resume_at(&self, now: u64) -> u64 {
        if let Some(wait) = self
            .header("retry-after")
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            return now + wait;
        }
        self.header("x-ratelimit-reset")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|reset| *reset > now)
            .unwrap_or(now + DEFAULT_BACKOFF)
    }

    /// GitHub's own `message`, which says more than the status does.
    fn message(&self) -> String {
        parse_json(&self.body)
            .ok()
            .map(|value| text(&value, "message"))
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| format!("HTTP {}", self.status))
    }
}

/// The account's GitHub, through `gh` on this machine.
pub struct GhAccount {
    gh: GhRepo,
    program: String,
    store: Mutex<Store>,
    /// Where the store persists.
    db: Db,
    ttl: Duration,
    details: Mutex<HashMap<String, (Instant, Detail)>>,
}

impl GhAccount {
    /// An account whose cache lives in the `pilus.db` under `root`, importing the
    /// `github-cache.json` there the first time. A cache that cannot be read
    /// starts empty.
    pub fn new(root: &Path) -> Self {
        let db = Db::at(root);
        Store::import_legacy(&db, &root.join("github-cache.json"));
        let store = Store::read(&db);
        Self {
            gh: GhRepo::new(None, ""),
            program: "gh".to_string(),
            store: Mutex::new(store),
            db,
            ttl: TTL,
            details: Mutex::new(HashMap::new()),
        }
    }

    /// Run a different `gh`, with a different list lifetime. Tests point it at
    /// a stand-in and shorten the lifetime so staleness can be reached.
    #[cfg(test)]
    fn with_program(mut self, program: &str, ttl: Duration) -> Self {
        self.gh = self.gh.with_program(program);
        self.program = program.to_string();
        self.ttl = ttl;
        self
    }

    /// The login the last successful probe saw, so the sidebar can show saved
    /// lists the moment the window opens, before `gh` has answered.
    pub fn saved_login(&self) -> Option<String> {
        let store = self.store.lock().ok()?;
        let entry = store.entries.get("user")?;
        serde_json::from_str::<String>(&entry.body).ok()
    }

    /// Whether `gh` works here and which account it is signed in as.
    pub fn probe(&self) -> AccountState {
        // A saved login is still the account when GitHub can't be reached, so
        // the sidebar keeps its saved lists instead of going blank offline.
        let body = match self.get("user", "user", LOGIN_TTL, false) {
            Ok((body, _)) | Err(Failed::Stale(_, body, _)) => Ok(body),
            Err(other) => Err(other),
        };
        match body {
            Ok(body) => match serde_json::from_str::<String>(&body) {
                Ok(login) if valid_login(&login) => AccountState::Ready { login },
                _ => AccountState::Unavailable(GhState::Failed(
                    "gh did not say which account it is signed in as.".to_string(),
                )),
            },
            Err(Failed::Unavailable(state)) => AccountState::Unavailable(state),
            Err(Failed::Other(message) | Failed::Stale(message, ..)) => {
                AccountState::Unavailable(GhState::Failed(message))
            }
        }
    }

    /// The repositories `login` pushed to most recently, with stars and what
    /// is open in each, in one request.
    pub fn repos(&self, login: &str, force: bool) -> Result<Listing<RepoSummary>, String> {
        self.listing(
            &format!("{login}|repos2"),
            Call::Graphql(format!(
                "query {{ viewer {{ repositories(first: {REPO_LIMIT}, orderBy: {{field: PUSHED_AT, direction: DESC}}, affiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER], ownerAffiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER]) {{ nodes {{ {REPO_FIELDS} }} }} }} }}"
            )),
            force,
            |value| {
                let nodes = value
                    .pointer("/data/viewer/repositories/nodes")
                    .unwrap_or(&Value::Null);
                array(nodes).iter().filter_map(parse_graphql_repo).collect()
            },
        )
    }

    /// The lists already on disk, without a call, so a launch paints at once.
    pub fn saved_repos(&self, login: &str) -> Option<Listing<RepoSummary>> {
        self.saved(&format!("{login}|repos2"))
    }

    /// One repository by name, for a pinned repository the recent list does
    /// not reach.
    pub fn repo_summary(&self, repo: &str, force: bool) -> Result<RepoSummary, String> {
        let (owner, name) = valid_slug(repo)
            .then(|| repo.split_once('/'))
            .flatten()
            .ok_or_else(|| format!("`{repo}` is not a repository name."))?;
        let listing = self.listing(
            &format!("{repo}|summary"),
            Call::Graphql(format!(
                "query {{ repository(owner: \"{owner}\", name: \"{name}\") {{ {REPO_FIELDS} }} }}"
            )),
            force,
            |value| {
                value
                    .pointer("/data/repository")
                    .and_then(parse_graphql_repo)
                    .into_iter()
                    .collect()
            },
        )?;
        listing
            .items
            .into_iter()
            .next()
            .ok_or_else(|| format!("GitHub has no repository `{repo}` for this account."))
    }

    /// Open issues of `repo`, newest activity first. The issues endpoint also
    /// lists pull requests, so it asks for more than it shows.
    pub fn repo_issues(&self, repo: &str, force: bool) -> Result<Listing<ItemSummary>, String> {
        let repo = checked(repo)?;
        self.listing(
            &format!("{repo}|issues"),
            Call::Rest(format!(
                "repos/{repo}/issues?state=open&sort=updated&direction=desc&per_page={ISSUE_PAGE}"
            )),
            force,
            |value| parse_items(value, repo, false),
        )
    }

    /// Open pull requests of `repo`, newest activity first.
    pub fn repo_pulls(&self, repo: &str, force: bool) -> Result<Listing<ItemSummary>, String> {
        let repo = checked(repo)?;
        self.listing(
            &format!("{repo}|pulls"),
            Call::Rest(format!(
                "repos/{repo}/pulls?state=open&sort=updated&direction=desc&per_page={PER_PAGE}"
            )),
            force,
            |value| parse_items(value, repo, true),
        )
    }

    /// The latest workflow runs of `repo`.
    pub fn repo_runs(&self, repo: &str, force: bool) -> Result<Listing<RunSummary>, String> {
        let repo = checked(repo)?;
        self.listing(
            &format!("{repo}|runs"),
            Call::Rest(format!("repos/{repo}/actions/runs?per_page={PER_PAGE}")),
            force,
            parse_runs,
        )
    }

    /// The list for `repo` already on disk, if any.
    pub fn saved_issues(&self, repo: &str) -> Option<Listing<ItemSummary>> {
        self.saved(&format!("{repo}|issues"))
    }

    pub fn saved_pulls(&self, repo: &str) -> Option<Listing<ItemSummary>> {
        self.saved(&format!("{repo}|pulls"))
    }

    pub fn saved_runs(&self, repo: &str) -> Option<Listing<RunSummary>> {
        self.saved(&format!("{repo}|runs"))
    }

    /// A pull request with its checks and conversation.
    pub fn pull_detail(&self, repo: &str, number: u64, force: bool) -> Result<PullDetail, String> {
        let key = format!("pull:{repo}#{number}");
        if let Some(Detail::Pull(detail)) = self.cached_detail(&key, force) {
            return Ok(*detail);
        }
        let detail = self.scoped(repo)?.pull(number)?;
        self.keep_detail(key, Detail::Pull(Box::new(detail.clone())));
        Ok(detail)
    }

    /// An issue with its conversation.
    pub fn issue_detail(
        &self,
        repo: &str,
        number: u64,
        force: bool,
    ) -> Result<IssueDetail, String> {
        let key = format!("issue:{repo}#{number}");
        if let Some(Detail::Issue(detail)) = self.cached_detail(&key, force) {
            return Ok(*detail);
        }
        let detail = self.scoped(repo)?.issue(number)?;
        self.keep_detail(key, Detail::Issue(Box::new(detail.clone())));
        Ok(detail)
    }

    fn scoped(&self, repo: &str) -> Result<GhRepo, String> {
        if !valid_slug(repo) {
            return Err(format!("`{repo}` is not a repository name."));
        }
        Ok(GhRepo::for_slug(repo).with_program(&self.program))
    }

    fn cached_detail(&self, key: &str, force: bool) -> Option<Detail> {
        if force {
            return None;
        }
        let details = self.details.lock().ok()?;
        let (at, detail) = details.get(key)?;
        (at.elapsed() < DETAIL_TTL).then(|| detail.clone())
    }

    fn keep_detail(&self, key: String, detail: Detail) {
        if let Ok(mut details) = self.details.lock() {
            details.insert(key, (Instant::now(), detail));
        }
    }

    fn saved<T: DeserializeOwned>(&self, key: &str) -> Option<Listing<T>> {
        let entry = self.store.lock().ok()?.entries.get(key)?.clone();
        Some(Listing {
            items: serde_json::from_str(&entry.body).ok()?,
            fetched_at: entry.fetched_at,
            note: None,
        })
    }

    /// One cached list: serve it when fresh, revalidate it when stale, and
    /// fall back to it when GitHub cannot be asked.
    fn listing<T>(
        &self,
        key: &str,
        call: Call,
        force: bool,
        parse: impl Fn(&Value) -> Vec<T>,
    ) -> Result<Listing<T>, String>
    where
        T: Serialize + DeserializeOwned,
    {
        let slim = |value: &Value| {
            serde_json::to_string(&parse(value)).unwrap_or_else(|_| "[]".to_string())
        };
        match self.get_with(key, &call, self.ttl, force, slim) {
            Ok((body, fetched_at)) => Ok(Listing {
                items: serde_json::from_str(&body)
                    .map_err(|error| format!("saved GitHub list is unreadable: {error}"))?,
                fetched_at,
                note: None,
            }),
            Err(Failed::Stale(message, body, fetched_at)) => Ok(Listing {
                items: serde_json::from_str(&body).unwrap_or_default(),
                fetched_at,
                note: Some(message),
            }),
            Err(Failed::Unavailable(state)) => Err(state.advice().unwrap_or_default()),
            Err(Failed::Other(message)) => Err(message),
        }
    }

    /// [`Self::get_with`] for a body kept as GitHub sent it, parsed as the
    /// login: the one request that is not a list.
    fn get(
        &self,
        key: &str,
        path: &str,
        ttl: Duration,
        force: bool,
    ) -> Result<(String, u64), Failed> {
        self.get_with(key, &Call::Rest(path.to_string()), ttl, force, |value| {
            serde_json::to_string(&text(value, "login")).unwrap_or_default()
        })
    }

    /// The cache's one decision procedure: what body to hand back for `key`,
    /// and when it was last confirmed.
    ///
    /// `slim` turns GitHub's payload into the body that is stored and served.
    /// A stale body that had to be served instead of a fresh one comes back as
    /// [`Failed::Stale`], so the caller can say so.
    fn get_with(
        &self,
        key: &str,
        call: &Call,
        ttl: Duration,
        force: bool,
        slim: impl Fn(&Value) -> String,
    ) -> Result<(String, u64), Failed> {
        let now = unix_now();
        let (entry, backoff_until) = {
            let store = self.store.lock().map_err(|_| poisoned())?;
            (store.entries.get(key).cloned(), store.backoff_until)
        };
        if let Some(entry) = &entry {
            let age = now.saturating_sub(entry.fetched_at);
            let fresh = age < ttl.as_secs();
            let just_asked = force && age < MIN_FORCED_GAP;
            if (!force && fresh) || just_asked {
                return Ok((entry.body.clone(), entry.fetched_at));
            }
        }
        if now < backoff_until {
            let message = format!(
                "GitHub's rate limit is reached; trying again in {}.",
                wait_words(backoff_until - now)
            );
            return Err(match entry {
                Some(entry) => Failed::Stale(message, entry.body, entry.fetched_at),
                None => Failed::Other(message),
            });
        }

        let etag = entry.as_ref().map(|entry| entry.etag.as_str());
        let failure = match self.request(call, etag) {
            Ok(response) => match response.status {
                304 => {
                    if let Some(entry) = entry.clone() {
                        self.store_entry(
                            key,
                            Entry {
                                fetched_at: now,
                                ..entry.clone()
                            },
                        );
                        return Ok((entry.body, now));
                    }
                    Failed::Other("GitHub said nothing changed, but nothing was saved.".into())
                }
                200..=299 => match parse_json(&response.body) {
                    // GraphQL answers 200 even when it fails outright.
                    Ok(value)
                        if value.get("data").is_none_or(Value::is_null)
                            && value.get("errors").is_some() =>
                    {
                        Failed::Other(graphql_message(&value))
                    }
                    Ok(value) => {
                        let body = slim(&value);
                        self.store_entry(
                            key,
                            Entry {
                                fetched_at: now,
                                etag: response.header("etag").unwrap_or_default().to_string(),
                                body: body.clone(),
                            },
                        );
                        return Ok((body, now));
                    }
                    Err(error) => Failed::Other(error),
                },
                401 => Failed::Unavailable(GhState::SignedOut(response.message())),
                _ if response.rate_limited() => {
                    let resume = response.resume_at(now);
                    self.stand_down(resume);
                    Failed::Other(format!(
                        "GitHub's rate limit is reached; trying again in {}.",
                        wait_words(resume - now)
                    ))
                }
                _ => Failed::Other(format!("GitHub said: {}", response.message())),
            },
            Err(state) => Failed::Unavailable(state),
        };
        // Whatever went wrong, a list on hand beats an empty sidebar.
        match (entry, failure) {
            (Some(entry), Failed::Other(message)) => {
                Err(Failed::Stale(message, entry.body, entry.fetched_at))
            }
            (Some(entry), Failed::Unavailable(state)) if !matches!(state, GhState::Missing) => {
                Err(Failed::Stale(
                    state.advice().unwrap_or_default(),
                    entry.body,
                    entry.fetched_at,
                ))
            }
            (_, failure) => Err(failure),
        }
    }

    /// `gh api -i`, with the saved `ETag` on a REST call so GitHub can answer
    /// `304`.
    fn request(&self, call: &Call, etag: Option<&str>) -> Result<Response, GhState> {
        let condition = etag
            .filter(|etag| !etag.is_empty())
            .map(|etag| format!("If-None-Match: {etag}"));
        let query;
        let mut args = vec!["api", "-i"];
        match call {
            Call::Rest(path) => {
                args.push(path);
                if let Some(condition) = condition.as_deref() {
                    args.extend(["-H", condition]);
                }
            }
            Call::Graphql(text) => {
                query = format!("query={text}");
                args.extend(["graphql", "-f", &query]);
            }
        }
        let output = self
            .gh
            .run(&self.program, &args, None)
            .map_err(GhState::Failed)?;
        // `gh` exits non-zero for a 4xx but still prints the response, and the
        // response is what says whether it was a rate limit.
        match parse_response(&output.stdout) {
            Some(response) => Ok(response),
            None if output.code == Some(0) => Err(GhState::Failed(
                "gh answered something unexpected.".to_string(),
            )),
            None => Err(match classify(output.code, &output.stderr) {
                GhState::NoRepo(why) => GhState::Failed(why),
                state => state,
            }),
        }
    }

    fn store_entry(&self, key: &str, entry: Entry) {
        if let Ok(mut store) = self.store.lock() {
            self.persist(|conn| write_entry(conn, key, &entry));
            store.entries.insert(key.to_string(), entry);
        }
    }

    fn stand_down(&self, until: u64) {
        if let Ok(mut store) = self.store.lock() {
            store.backoff_until = until;
            self.persist(|conn| pilus_db::put_doc_in(conn, BACKOFF_DOC, &until));
        }
    }

    /// Write one change to the database, best-effort like the app's other state.
    /// It names private repositories, and the database is owner-only.
    fn persist(
        &self,
        write: impl FnOnce(&pilus_db::rusqlite::Connection) -> Result<(), pilus_db::Error>,
    ) {
        if let Err(error) = self.db.with(write) {
            tracing::warn!("could not save the GitHub cache: {error}");
        }
    }
}

/// Why a cached read did not return fresh data.
enum Failed {
    /// `gh` cannot work here at all.
    Unavailable(GhState),
    /// The read failed and nothing was saved to fall back on.
    Other(String),
    /// The read failed, so this saved body (last confirmed at the number) is
    /// served instead, with the message to show beside it.
    Stale(String, String, u64),
}

fn poisoned() -> Failed {
    Failed::Other("the GitHub cache is unavailable".to_string())
}

/// Split `gh api -i` output into status, headers and body. `None` when it
/// does not start with an HTTP status line, as when `gh` failed before asking.
fn parse_response(output: &str) -> Option<Response> {
    let output = output.trim_start();
    let (head, body) = output
        .split_once("\r\n\r\n")
        .or_else(|| output.split_once("\n\n"))
        .unwrap_or((output, ""));
    let mut lines = head.lines();
    let status = lines.next()?;
    let status = status
        .strip_prefix("HTTP/")?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    Some(Response {
        status,
        headers,
        body: body.to_string(),
    })
}

/// The fields a repository row needs, shared by the list and the single read.
const REPO_FIELDS: &str = "nameWithOwner description isPrivate isFork isArchived pushedAt url stargazerCount primaryLanguage { name } defaultBranchRef { name } issues(states: OPEN) { totalCount } pullRequests(states: OPEN) { totalCount }";

/// `repo` if it is a repository name, else the reason it is not.
fn checked(repo: &str) -> Result<&str, String> {
    if valid_slug(repo) {
        Ok(repo)
    } else {
        Err(format!("`{repo}` is not a repository name."))
    }
}

fn graphql_message(value: &Value) -> String {
    let first = array(value.get("errors").unwrap_or(&Value::Null))
        .first()
        .map(|error| text(error, "message"))
        .filter(|message| !message.is_empty());
    format!(
        "GitHub said: {}",
        first.unwrap_or_else(|| "the query failed".into())
    )
}

fn parse_graphql_repo(repo: &Value) -> Option<RepoSummary> {
    let name = text(repo, "nameWithOwner");
    if name.is_empty() {
        return None;
    }
    let flag = |key: &str| repo.get(key).and_then(Value::as_bool).unwrap_or(false);
    let nested = |key: &str, inner: &str| {
        repo.get(key)
            .map(|value| text(value, inner))
            .unwrap_or_default()
    };
    let total = |key: &str| {
        repo.get(key)
            .map(|value| count(value, "totalCount"))
            .unwrap_or(0)
    };
    Some(RepoSummary {
        name,
        description: text(repo, "description"),
        private: flag("isPrivate"),
        fork: flag("isFork"),
        archived: flag("isArchived"),
        language: nested("primaryLanguage", "name"),
        stars: count(repo, "stargazerCount"),
        open_issues: total("issues"),
        open_pulls: total("pullRequests"),
        pushed_at: text(repo, "pushedAt"),
        url: text(repo, "url"),
        default_branch: nested("defaultBranchRef", "name"),
    })
}

/// Issues or pull requests from the REST list endpoints. The issues endpoint
/// mixes in pull requests (they carry a `pull_request` key), which are
/// dropped from an issue list.
fn parse_items(value: &Value, repo: &str, pulls: bool) -> Vec<ItemSummary> {
    array(value)
        .iter()
        .filter(|item| pulls || item.get("pull_request").is_none())
        .filter_map(|item| {
            Some(ItemSummary {
                repo: repo.to_string(),
                number: item.get("number")?.as_u64()?,
                title: text(item, "title"),
                author: item
                    .get("user")
                    .map(|user| text(user, "login"))
                    .filter(|login| !login.is_empty())
                    .unwrap_or_else(|| "ghost".to_string()),
                draft: item.get("draft").and_then(Value::as_bool).unwrap_or(false),
                branch: item
                    .get("head")
                    .map(|head| text(head, "ref"))
                    .unwrap_or_default(),
                updated_at: text(item, "updated_at"),
                comments: count(item, "comments"),
                url: text(item, "html_url"),
            })
        })
        .take(PER_PAGE as usize)
        .collect()
}

fn parse_runs(value: &Value) -> Vec<RunSummary> {
    array(value.get("workflow_runs").unwrap_or(&Value::Null))
        .iter()
        .filter_map(|run| {
            Some(RunSummary {
                id: run.get("id")?.as_u64()?,
                workflow: text(run, "name"),
                title: text(run, "display_title"),
                number: count(run, "run_number"),
                status: text(run, "status"),
                conclusion: text(run, "conclusion"),
                branch: text(run, "head_branch"),
                event: text(run, "event"),
                actor: run
                    .get("actor")
                    .map(|actor| text(actor, "login"))
                    .unwrap_or_default(),
                updated_at: text(run, "updated_at"),
                url: text(run, "html_url"),
            })
        })
        .collect()
}

/// A GitHub login: letters, digits and single hyphens. Checked because it is
/// spliced into a search query.
fn valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `owner/name`, each side the characters GitHub allows. Checked because it
/// reaches `gh` as an argument, where a leading `-` would read as a flag.
fn valid_slug(slug: &str) -> bool {
    let Some((owner, name)) = slug.split_once('/') else {
        return false;
    };
    let part = |part: &str| {
        !part.is_empty()
            && !part.starts_with('-')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    part(owner) && part(name)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn wait_words(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{} s", seconds.max(1)),
        60..=3_599 => format!("{} min", seconds.div_ceil(60)),
        _ => format!("{} h", seconds.div_ceil(3_600)),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-acct-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn program(dir: &Path, body: &str) -> String {
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path.to_string_lossy().into_owned()
    }

    /// A fake `gh` that logs each call and answers `api` paths from `cases`.
    fn fake(dir: &Path, cases: &str) -> String {
        program(
            dir,
            &format!(
                "echo \"$*\" >> {log}\n{cases}",
                log = dir.join("calls").display()
            ),
        )
    }

    fn calls(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    const REPOS_200: &str = r#"case "$*" in
  *graphql*) printf 'HTTP/2.0 200 OK\r\n\r\n{"data":{"viewer":{"repositories":{"nodes":[{"nameWithOwner":"me/app","description":"An app","isPrivate":true,"isFork":false,"isArchived":false,"pushedAt":"2026-10-01T00:00:00Z","url":"https://github.com/me/app","stargazerCount":3,"primaryLanguage":{"name":"Rust"},"defaultBranchRef":{"name":"main"},"issues":{"totalCount":5},"pullRequests":{"totalCount":2}}]}}}}';;
  *) printf 'HTTP/2.0 200 OK\r\nEtag: "u1"\r\n\r\n{"login":"me"}';;
esac"#;

    /// A repository's REST lists, which can be revalidated.
    const REPO_REST: &str = r#"case "$*" in
  *'If-None-Match: "i1"'*) printf 'HTTP/2.0 304 Not Modified\r\nEtag: "i1"\r\n\r\n';;
  *repos/o/r/issues*) printf 'HTTP/2.0 200 OK\r\nEtag: "i1"\r\n\r\n[{"number":1,"title":"A bug","user":{"login":"u"},"updated_at":"2026-10-01T00:00:00Z","comments":2,"html_url":"https://github.com/o/r/issues/1"},{"number":2,"title":"A pull","pull_request":{},"user":{"login":"u"},"updated_at":"2026-10-02T00:00:00Z","html_url":"https://github.com/o/r/pull/2"}]';;
  *repos/o/r/pulls*) printf 'HTTP/2.0 200 OK\r\nEtag: "p1"\r\n\r\n[{"number":2,"title":"A pull","draft":true,"head":{"ref":"feature"},"user":{"login":"u"},"updated_at":"2026-10-02T00:00:00Z","html_url":"https://github.com/o/r/pull/2"}]';;
  *repos/o/r/actions/runs*) printf 'HTTP/2.0 200 OK\r\n\r\n{"workflow_runs":[{"id":9,"name":"CI","display_title":"Fix it","run_number":41,"status":"completed","conclusion":"failure","head_branch":"main","event":"push","actor":{"login":"u"},"updated_at":"2026-10-03T00:00:00Z","html_url":"https://github.com/o/r/actions/runs/9"}]}';;
esac"#;

    #[test]
    fn a_fresh_list_is_served_without_a_call() {
        let dir = dir("fresh");
        let gh = fake(&dir, REPOS_200);
        let account = GhAccount::new(&dir).with_program(&gh, Duration::from_secs(300));

        let first = account.repos("me", false).expect("first");
        assert_eq!(first.items[0].name, "me/app");
        assert!(first.items[0].private);
        assert_eq!(
            (
                first.items[0].stars,
                first.items[0].open_issues,
                first.items[0].open_pulls
            ),
            (3, 5, 2)
        );
        let second = account.repos("me", false).expect("second");
        assert_eq!(second, first);
        assert_eq!(calls(&dir).len(), 1, "{:?}", calls(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_list_revalidates_with_its_etag_and_keeps_the_items_on_304() {
        let dir = dir("etag");
        let gh = fake(&dir, REPO_REST);
        // A zero lifetime makes every read stale; not forcing bypasses the gap.
        let account = GhAccount::new(&dir).with_program(&gh, Duration::ZERO);

        let first = account.repo_issues("o/r", false).expect("first");
        let second = account.repo_issues("o/r", false).expect("second");
        assert_eq!(second.items, first.items);
        let calls = calls(&dir);
        assert_eq!(calls.len(), 2);
        assert!(calls[1].contains("If-None-Match: \"i1\""), "{calls:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_repository_list_never_sends_a_condition() {
        let dir = dir("graphql");
        let gh = fake(&dir, REPOS_200);
        let account = GhAccount::new(&dir).with_program(&gh, Duration::ZERO);
        account.repos("me", false).expect("first");
        account.repos("me", false).expect("second");
        assert!(
            calls(&dir)
                .iter()
                .all(|call| !call.contains("If-None-Match"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_repository_reads_its_issues_pulls_and_runs() {
        let dir = dir("inside");
        let gh = fake(&dir, REPO_REST);
        let account = GhAccount::new(&dir).with_program(&gh, Duration::from_secs(300));

        let issues = account.repo_issues("o/r", false).expect("issues");
        assert_eq!(issues.items.len(), 1, "the pull request is not an issue");
        assert_eq!(issues.items[0].title, "A bug");
        assert_eq!(issues.items[0].comments, 2);

        let pulls = account.repo_pulls("o/r", false).expect("pulls");
        assert_eq!(pulls.items[0].branch, "feature");
        assert!(pulls.items[0].draft);

        let runs = account.repo_runs("o/r", false).expect("runs");
        assert_eq!(runs.items[0].workflow, "CI");
        assert_eq!(runs.items[0].conclusion, "failure");
        assert_eq!(runs.items[0].number, 41);

        assert!(account.saved_issues("o/r").is_some());
        assert!(account.repo_issues("--repo=x/y", false).is_err());
        assert_eq!(calls(&dir).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_graphql_failure_is_an_error_not_an_empty_list() {
        let dir = dir("gqlerr");
        let gh = fake(
            &dir,
            r#"printf 'HTTP/2.0 200 OK\r\n\r\n{"data":null,"errors":[{"message":"Resource not accessible"}]}'"#,
        );
        let account = GhAccount::new(&dir).with_program(&gh, Duration::ZERO);
        let error = account.repos("me", false).expect_err("fails");
        assert!(error.contains("Resource not accessible"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pinned_repository_outside_the_list_reads_by_name() {
        let dir = dir("summary");
        let gh = fake(
            &dir,
            r#"printf 'HTTP/2.0 200 OK\r\n\r\n{"data":{"repository":{"nameWithOwner":"o/old","isPrivate":false,"stargazerCount":9,"issues":{"totalCount":1},"pullRequests":{"totalCount":0}}}}'"#,
        );
        let account = GhAccount::new(&dir).with_program(&gh, Duration::from_secs(300));
        let repo = account.repo_summary("o/old", false).expect("summary");
        assert_eq!(
            (repo.name.as_str(), repo.stars, repo.open_issues),
            ("o/old", 9, 1)
        );
        assert!(account.repo_summary("bad", false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_forced_refresh_inside_the_minimum_gap_does_not_call() {
        let dir = dir("gap");
        let gh = fake(&dir, REPOS_200);
        let account = GhAccount::new(&dir).with_program(&gh, Duration::from_secs(300));

        account.repos("me", false).expect("first");
        account.repos("me", true).expect("forced");
        assert_eq!(calls(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_survives_a_relaunch() {
        let dir = dir("disk");
        let gh = fake(&dir, REPOS_200);
        GhAccount::new(&dir)
            .with_program(&gh, Duration::from_secs(300))
            .repos("me", false)
            .expect("seed");

        let reopened = GhAccount::new(&dir).with_program(&gh, Duration::from_secs(300));
        let saved = reopened.saved_repos("me").expect("saved");
        assert_eq!(saved.items[0].name, "me/app");
        reopened.repos("me", false).expect("fresh from disk");
        assert_eq!(calls(&dir).len(), 1);
        let mode = std::fs::metadata(dir.join(pilus_db::FILE))
            .expect("file")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "private repo names must not be world-readable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rate_limit_stands_down_and_serves_the_saved_list() {
        let dir = dir("limit");
        let flag = dir.join("limited");
        let gh = fake(
            &dir,
            &format!(
                r#"if [ -e {flag} ]; then
  printf 'HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Reset: %s\r\n\r\n{{"message":"API rate limit exceeded"}}' $(( $(date +%s) + 600 ))
  exit 1
fi
{REPOS_200}"#,
                flag = flag.display()
            ),
        );
        let account = GhAccount::new(&dir).with_program(&gh, Duration::ZERO);
        let seeded = account.repos("me", false).expect("seed");

        std::fs::write(&flag, "").expect("flag");
        let limited = account.repos("me", false).expect("falls back");
        assert_eq!(limited.items, seeded.items);
        assert!(
            limited
                .note
                .as_deref()
                .is_some_and(|n| n.contains("rate limit")),
            "{:?}",
            limited.note
        );
        let before = calls(&dir).len();
        account.repos("me", false).expect("still served");
        assert_eq!(calls(&dir).len(), before, "no call while standing down");

        // The back-off is saved, so a relaunch does not walk back into it.
        let reopened = GhAccount::new(&dir).with_program(&gh, Duration::ZERO);
        reopened.repos("me", false).expect("served");
        assert_eq!(calls(&dir).len(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_with_no_saved_list_is_an_error() {
        let dir = dir("none");
        let gh = fake(
            &dir,
            r#"printf 'HTTP/2.0 500 Server Error\r\n\r\n{"message":"boom"}'; exit 1"#,
        );
        let account = GhAccount::new(&dir).with_program(&gh, Duration::ZERO);
        let error = account.repos("me", false).expect_err("fails");
        assert!(error.contains("boom"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn network_failure_serves_the_saved_list_with_a_note() {
        let dir = dir("offline");
        let flag = dir.join("offline");
        let gh = fake(
            &dir,
            &format!(
                "if [ -e {flag} ]; then echo 'error connecting to api.github.com' >&2; exit 1; fi\n{REPOS_200}",
                flag = flag.display()
            ),
        );
        let account = GhAccount::new(&dir).with_program(&gh, Duration::ZERO);
        let seeded = account.repos("me", false).expect("seed");
        std::fs::write(&flag, "").expect("flag");

        let offline = account.repos("me", false).expect("falls back");
        assert_eq!(offline.items, seeded.items);
        assert!(offline.note.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_names_the_account_and_explains_a_signed_out_gh() {
        let dir = dir("probe");
        let gh = fake(&dir, REPOS_200);
        let account = GhAccount::new(&dir).with_program(&gh, Duration::from_secs(300));
        assert_eq!(
            account.probe(),
            AccountState::Ready {
                login: "me".to_string()
            }
        );
        assert_eq!(account.saved_login().as_deref(), Some("me"));

        let signed_out = dir.join("out");
        std::fs::create_dir_all(&signed_out).expect("dir");
        let gh = fake(
            &signed_out,
            "echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4",
        );
        let account = GhAccount::new(&signed_out).with_program(&gh, Duration::from_secs(300));
        assert!(matches!(
            account.probe(),
            AccountState::Unavailable(GhState::SignedOut(_))
        ));

        let account = GhAccount::new(&dir.join("none"))
            .with_program("pilus-no-such-gh", Duration::from_secs(300));
        assert!(account.probe().is_missing());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn details_are_kept_briefly_and_slugs_are_checked() {
        let dir = dir("detail");
        let gh = program(
            &dir,
            &format!(
                r#"echo "$*" >> {log}
case "$*" in
  *"pr view 4"*) echo '{{"number":4,"title":"A pull","state":"OPEN","author":{{"login":"u"}},"headRefName":"f","baseRefName":"main","body":"hi","comments":[],"statusCheckRollup":[]}}';;
esac"#,
                log = dir.join("calls").display()
            ),
        );
        let account = GhAccount::new(&dir).with_program(&gh, Duration::from_secs(300));
        let first = account.pull_detail("o/r", 4, false).expect("detail");
        assert_eq!(first.pull.title, "A pull");
        account.pull_detail("o/r", 4, false).expect("cached");
        let log = calls(&dir);
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(log[0].ends_with("-R o/r"), "{log:?}");
        account.pull_detail("o/r", 4, true).expect("forced");
        assert_eq!(calls(&dir).len(), 2);

        assert!(account.pull_detail("--repo=x/y", 1, false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slugs_and_logins_are_validated() {
        assert!(valid_slug("cli/cli"));
        assert!(valid_slug("a-b/c_d.e"));
        assert!(!valid_slug("cli"));
        assert!(!valid_slug("-x/y"));
        assert!(!valid_slug("a/b c"));
        assert!(!valid_slug("a/b/c;rm"));
        assert!(valid_login("octocat"));
        assert!(!valid_login("a+b"));
        assert!(!valid_login(""));
    }

    #[test]
    fn a_response_parses_with_either_line_ending() {
        let crlf =
            parse_response("HTTP/2.0 304 Not Modified\r\nEtag: \"x\"\r\n\r\n").expect("crlf");
        assert_eq!(crlf.status, 304);
        assert_eq!(crlf.header("ETAG"), Some("\"x\""));
        let lf = parse_response("HTTP/1.1 200 OK\nEtag: a\n\n[]").expect("lf");
        assert_eq!((lf.status, lf.body.as_str()), (200, "[]"));
        assert!(parse_response("not http").is_none());
    }
}
