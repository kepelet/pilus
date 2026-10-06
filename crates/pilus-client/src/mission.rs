//! Mission Control data from the daemon's UHP surface.
//!
//! `mission.snapshot` is the daemon's own dashboard model: live and resumable
//! agent rows with best-effort usage (tokens, context fill, cost), fleet
//! totals, and automation health. pilus draws that model natively instead of
//! opening the daemon's `mission` tab, so this module carries the read side
//! and every row action the daemon's dashboard offers — answer, approve,
//! interrupt, fork, resume, and the automation run/pause controls — as plain
//! [`ControlClient`] calls. Parsing is lenient: a field an older daemon omits
//! reads as unknown rather than failing the whole snapshot.

use serde_json::{Value, json};

use crate::control::ControlClient;
use crate::error::Result;

/// Which workspaces contribute rows to the dashboard.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MissionScope {
    /// The workspace the request names (the daemon's default).
    #[default]
    Workspace,
    /// Every open workspace.
    All,
}

impl MissionScope {
    fn wire(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::All => "all",
        }
    }
}

/// One session's usage counters. Every figure is the daemon's best-effort
/// read of the agent's own store; `None` means unknown, not zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MissionUsage {
    /// The model id the session last ran on.
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_tokens: u64,
    /// Fraction of the model's context window in use, `0.0..=1.0`.
    pub context: Option<f32>,
    /// Estimated USD cost; informational, never a bill.
    pub cost_usd: Option<f64>,
}

impl MissionUsage {
    /// Billed (non-cache) tokens, the figure the dashboard's TOKENS column shows.
    pub fn total_tokens(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }

    fn from_json(value: &Value) -> Option<Self> {
        let value = value.as_object()?;
        let count = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
        Some(Self {
            model: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tokens_in: count("tokens_in"),
            tokens_out: count("tokens_out"),
            cache_tokens: count("cache_tokens"),
            context: value
                .get("context")
                .and_then(Value::as_f64)
                .map(|context| context as f32),
            cost_usd: value.get("cost_usd").and_then(Value::as_f64),
        })
    }
}

/// One dashboard row: a live agent pane, or a session on disk that can be
/// resumed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MissionRow {
    /// The live pane, or `None` for a resumable session.
    pub pane: Option<String>,
    /// The agent kind, e.g. `claude`.
    pub agent: String,
    /// `blocked`, `working`, `done`, `idle`, or `unknown`.
    pub state: String,
    pub workspace_id: String,
    pub workspace_name: String,
    /// 1-based tab position of a live row.
    pub tab: Option<u16>,
    /// The daemon's location label: `t2 p1/3 · t4`, or a workspace name.
    pub location: String,
    pub usage: Option<MissionUsage>,
}

impl MissionRow {
    /// Whether the row is a session on disk rather than a running pane.
    pub fn resumable(&self) -> bool {
        self.pane.is_none()
    }

    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let live = value.get("kind").and_then(Value::as_str) != Some("resumable");
        Some(Self {
            pane: live
                .then(|| {
                    value
                        .get("pane")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .flatten(),
            agent: value.get("agent")?.as_str()?.to_string(),
            state: value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            workspace_id: text("workspace_id"),
            workspace_name: text("workspace_name"),
            tab: value
                .get("tab")
                .and_then(Value::as_str)
                .and_then(|tab| tab.parse().ok()),
            location: text("location"),
            usage: value.get("usage").and_then(MissionUsage::from_json),
        })
    }
}

/// The scheduler's run counts, as `automation.health` reports them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AutomationHealth {
    pub definitions: u32,
    pub enabled: u32,
    /// Armed: enabled with a next deadline.
    pub scheduled: u32,
    /// Runs pending, starting, or running.
    pub running: u32,
    pub review: u32,
    pub failed: u32,
    /// The soonest deadline, in Unix seconds.
    pub next_run_at: Option<u64>,
}

impl AutomationHealth {
    fn from_json(value: &Value) -> Self {
        let count = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|count| u32::try_from(count).ok())
                .unwrap_or(0)
        };
        Self {
            definitions: count("definitions"),
            enabled: count("enabled"),
            scheduled: count("scheduled"),
            running: count("running"),
            review: count("review"),
            failed: count("failed"),
            next_run_at: value.get("next_run_at").and_then(Value::as_u64),
        }
    }
}

/// One automation definition as the dashboard lists it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AutomationRow {
    pub id: String,
    pub name: String,
    /// `scheduled`, `running`, `review`, `failed`, `paused`, `completed`,
    /// `unavailable`, `restoring`, or `needs_rebind`.
    pub state: String,
    pub next_run_at: Option<u64>,
    pub latest_error: Option<String>,
    pub agent_id: String,
    /// Whether the definition is armed; a paused row offers Enable.
    pub enabled: bool,
}

impl AutomationRow {
    fn from_json(value: &Value) -> Option<Self> {
        let state = value.get("state")?.as_str()?.to_string();
        Some(Self {
            id: value.get("id")?.as_str()?.to_string(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            // The view carries no flag of its own; only these two states
            // come from a disabled definition.
            enabled: !matches!(state.as_str(), "paused" | "completed"),
            state,
            next_run_at: value.get("next_run_at").and_then(Value::as_u64),
            latest_error: value
                .get("latest_error")
                .and_then(Value::as_str)
                .map(str::to_string),
            agent_id: value
                .get("agent_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }
}

/// The whole dashboard at one instant.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MissionSnapshot {
    /// Whether a usage scan is still running; the counters may change.
    pub refreshing: bool,
    pub tokens: u64,
    pub cost_usd: f64,
    /// Fleet spend rate between the last two usage scans.
    pub burn_usd_per_hour: Option<f64>,
    pub automation: AutomationHealth,
    pub automations: Vec<AutomationRow>,
    /// Live rows by attention (blocked, working, done, idle), then
    /// resumable sessions.
    pub rows: Vec<MissionRow>,
}

impl MissionSnapshot {
    /// Parse a `mission_snapshot` result.
    pub fn from_json(value: &Value) -> Self {
        let summary = value.get("summary").unwrap_or(&Value::Null);
        let automation = value.get("automation").unwrap_or(&Value::Null);
        let list = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        Self {
            refreshing: value
                .get("refreshing")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tokens: summary.get("tokens").and_then(Value::as_u64).unwrap_or(0),
            cost_usd: summary
                .get("cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            burn_usd_per_hour: summary.get("burn_usd_per_hour").and_then(Value::as_f64),
            automation: AutomationHealth::from_json(
                automation.get("summary").unwrap_or(&Value::Null),
            ),
            automations: list(automation, "rows")
                .iter()
                .filter_map(AutomationRow::from_json)
                .collect(),
            rows: list(value, "rows")
                .iter()
                .filter_map(MissionRow::from_json)
                .collect(),
        }
    }
}

/// One recorded run of an automation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AutomationRun {
    pub id: String,
    /// `pending`, `running`, `review`, `succeeded`, `failed`, `skipped`, ….
    pub status: String,
    pub scheduled_at: u64,
    pub finished_at: Option<u64>,
    pub task_id: Option<String>,
    pub error: Option<String>,
}

impl AutomationRun {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_string(),
            status: value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            scheduled_at: value
                .get("scheduled_at")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            finished_at: value.get("finished_at").and_then(Value::as_u64),
            task_id: value
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            error: value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

/// An automation's full definition, schedule, and recent runs: what the
/// daemon's automation details popup shows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AutomationDetail {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// The trigger in words: `Daily at 08:00 (Asia/Makassar)`.
    pub schedule: String,
    /// `Skip missed runs · Skip overlapping`, from the execution policy.
    pub policy: String,
    /// `New worker` or `Active agent`.
    pub target: String,
    pub title: String,
    pub prompt: String,
    pub agent_id: String,
    /// `worktree` or `workspace`.
    pub mode: String,
    /// `read_only`, `workspace`, or `full_access`.
    pub access: String,
    pub paths: Vec<String>,
    pub gate: Option<String>,
    /// The next deadlines, in Unix seconds.
    pub occurrences: Vec<u64>,
    /// Newest first.
    pub runs: Vec<AutomationRun>,
}

impl AutomationDetail {
    fn from_json(automation: &Value, runs: &Value, occurrences: &Value) -> Self {
        let text = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let task = automation.get("task").unwrap_or(&Value::Null);
        let policy = automation.get("policy").unwrap_or(&Value::Null);
        let mut runs: Vec<AutomationRun> = runs
            .get("runs")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(AutomationRun::from_json)
            .collect();
        runs.sort_by_key(|run| std::cmp::Reverse(run.scheduled_at));
        Self {
            id: text(automation, "id"),
            name: text(automation, "name"),
            enabled: automation
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            schedule: describe_trigger(automation.get("trigger").unwrap_or(&Value::Null)),
            policy: describe_policy(policy),
            target: match automation
                .get("target")
                .and_then(|target| target.get("kind"))
                .and_then(Value::as_str)
            {
                Some("active_agent" | "existing_agent") => "Active agent".to_string(),
                _ => "New worker".to_string(),
            },
            title: text(task, "title"),
            prompt: text(task, "prompt"),
            agent_id: text(task, "agent_id"),
            mode: text(task, "mode"),
            access: text(task, "access"),
            paths: task
                .get("paths")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            gate: task
                .get("gate")
                .and_then(Value::as_str)
                .filter(|gate| !gate.is_empty())
                .map(str::to_string),
            occurrences: occurrences
                .get("occurrences_utc")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|value| {
                    value
                        .as_u64()
                        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                })
                .take(5)
                .collect(),
            runs,
        }
    }
}

/// A trigger in words, from the daemon's tagged `{kind, ...}` form.
fn describe_trigger(trigger: &Value) -> String {
    let clock = |trigger: &Value| {
        let seconds = trigger
            .get("second_of_day")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        format!("{:02}:{:02}", seconds / 3600, seconds % 3600 / 60)
    };
    let zone = |trigger: &Value| {
        trigger
            .get("timezone")
            .and_then(Value::as_str)
            .map(|zone| format!(" ({zone})"))
            .unwrap_or_default()
    };
    match trigger.get("kind").and_then(Value::as_str) {
        Some("once") => "Once".to_string(),
        Some("interval") => {
            let every = trigger
                .get("every_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            match every {
                3600 => "Hourly".to_string(),
                every if every % 3600 == 0 && every > 0 => format!("Every {}h", every / 3600),
                every if every % 60 == 0 && every > 0 => format!("Every {}m", every / 60),
                every => format!("Every {every}s"),
            }
        }
        Some("daily") => format!("Daily at {}{}", clock(trigger), zone(trigger)),
        Some("weekly") => {
            const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
            let days: Vec<&str> = trigger
                .get("weekdays")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(Value::as_u64)
                .filter_map(|day| DAYS.get(usize::try_from(day).ok()?.checked_sub(1)?))
                .copied()
                .collect();
            format!(
                "Weekly on {} at {}{}",
                days.join(", "),
                clock(trigger),
                zone(trigger)
            )
        }
        _ => "Unknown schedule".to_string(),
    }
}

/// The execution policy in words.
fn describe_policy(policy: &Value) -> String {
    let misfire = match policy.get("misfire").and_then(Value::as_str) {
        Some("run_latest") => "Run latest missed",
        _ => "Skip missed runs",
    };
    let overlap = match policy.get("overlap").and_then(Value::as_str) {
        Some("queue_one") => "Queue one overlap",
        _ => "Skip overlapping",
    };
    format!("{misfire} · {overlap}")
}

/// Request params naming the dashboard's scope and anchor workspace.
fn scope_params(scope: MissionScope, workspace_id: Option<&str>) -> Value {
    let mut params = json!({ "scope": scope.wire() });
    if let Some(workspace_id) = workspace_id {
        params["workspace_id"] = json!(workspace_id);
    }
    params
}

impl ControlClient {
    /// The dashboard's rows and totals for `scope`, anchored at
    /// `workspace_id` (the daemon's active workspace when `None`). Reading
    /// never changes the daemon's own focus.
    pub fn mission_snapshot(
        &mut self,
        scope: MissionScope,
        workspace_id: Option<&str>,
    ) -> Result<MissionSnapshot> {
        let result = self.request("mission.snapshot", scope_params(scope, workspace_id))?;
        Ok(MissionSnapshot::from_json(&result))
    }

    /// Queue one usage scan; the next snapshot carries the new counters.
    pub fn mission_refresh(
        &mut self,
        scope: MissionScope,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        self.request("mission.refresh", scope_params(scope, workspace_id))
            .map(drop)
    }

    /// The user's spend ceiling for Mission Control, if configured.
    pub fn mission_budget(&mut self) -> Result<Option<f64>> {
        let result = self.request("config.get", json!({}))?;
        Ok(result
            .get("config")
            .and_then(|config| config.get("mission_budget"))
            .and_then(Value::as_f64))
    }

    /// Send named keys (`y`, `enter`, `escape`, `ctrl+c`, …) to an agent.
    pub fn agent_keys(&mut self, target: &str, keys: &[&str]) -> Result<()> {
        self.request("agent.keys", json!({ "target": target, "keys": keys }))
            .map(drop)
    }

    /// Fork a supported agent's session into a sibling pane, leaving the
    /// daemon's focus where it is.
    pub fn fork_agent(&mut self, target: &str) -> Result<()> {
        self.request("agent.fork", json!({ "target": target, "focus": false }))
            .map(drop)
    }

    /// Reopen a resumable session (from [`Self::agent_sessions`]) in a pane.
    pub fn resume_agent(&mut self, session_id: &str) -> Result<()> {
        self.request("agent.resume", json!({ "session_id": session_id }))
            .map(drop)
    }

    /// The last `lines` of an agent's visible output.
    pub fn read_agent(&mut self, target: &str, lines: u16) -> Result<String> {
        let result = self.request("agent.read", json!({ "target": target, "lines": lines }))?;
        Ok(result
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// An automation's definition, its next five deadlines, and its recent
    /// runs, in three reads.
    pub fn automation_detail(&mut self, id: &str) -> Result<AutomationDetail> {
        let got = self.request("automation.get", json!({ "id": id }))?;
        let automation = got.get("automation").cloned().unwrap_or(got);
        let runs = self.request("automation.history", json!({ "id": id, "limit": 20 }))?;
        // A schedule the preview cannot expand (a past one-shot) still has a
        // definition worth showing.
        let occurrences = match automation.get("trigger") {
            Some(trigger) => self
                .request("automation.preview", json!({ "trigger": trigger }))
                .unwrap_or(Value::Null),
            None => Value::Null,
        };
        Ok(AutomationDetail::from_json(
            &automation,
            &runs,
            &occurrences,
        ))
    }

    /// Request one run now; the schedule does not advance.
    pub fn run_automation(&mut self, id: &str) -> Result<()> {
        self.request("automation.run", json!({ "id": id }))
            .map(drop)
    }

    /// Arm or pause an automation definition.
    pub fn set_automation_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        let method = if enabled {
            "automation.enable"
        } else {
            "automation.disable"
        };
        self.request(method, json!({ "id": id })).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AutomationDetail, MissionSnapshot, describe_trigger};

    #[test]
    fn snapshot_reads_rows_usage_and_automation_health() {
        let snapshot = MissionSnapshot::from_json(&json!({
            "type": "mission_snapshot",
            "refreshing": true,
            "summary": {"agents": 2, "tokens": 1500, "cost_usd": 3.25, "burn_usd_per_hour": null},
            "automation": {
                "summary": {"definitions": 2, "enabled": 1, "scheduled": 1, "running": 0,
                            "review": 1, "failed": 0, "next_run_at": 1_800_000_000u64},
                "rows": [
                    {"id": "a1", "name": "Morning review", "state": "scheduled",
                     "next_run_at": 1_800_000_000u64, "agent_id": "codex"},
                    {"id": "a2", "name": "Nightly", "state": "paused", "agent_id": "claude"}
                ]
            },
            "rows": [
                {"kind": "live", "pane": "4", "agent": "claude", "state": "blocked",
                 "workspace": "0", "workspace_id": "ws1", "workspace_name": "api",
                 "tab": "2", "location": "t2 p1/2",
                 "usage": {"model": "claude-opus-4", "tokens_in": 1000, "tokens_out": 500,
                           "cache_tokens": 9000, "total_tokens": 1500,
                           "context": 0.6, "cost_usd": 3.25}},
                {"kind": "resumable", "pane": null, "agent": "codex", "state": "idle",
                 "workspace": "0", "workspace_id": "ws1", "workspace_name": "api",
                 "tab": null, "location": "—", "usage": null}
            ]
        }));
        assert!(snapshot.refreshing);
        assert_eq!(snapshot.burn_usd_per_hour, None);
        assert_eq!(snapshot.automation.review, 1);
        assert_eq!(snapshot.automation.next_run_at, Some(1_800_000_000));
        assert!(snapshot.automations[0].enabled);
        assert!(!snapshot.automations[1].enabled);
        let live = &snapshot.rows[0];
        assert_eq!(live.pane.as_deref(), Some("4"));
        assert_eq!(live.tab, Some(2));
        let usage = live.usage.as_ref().expect("usage");
        assert_eq!(usage.total_tokens(), 1500);
        assert_eq!(usage.context, Some(0.6));
        assert!(snapshot.rows[1].resumable());
        assert!(snapshot.rows[1].usage.is_none());
    }

    #[test]
    fn triggers_read_as_words() {
        assert_eq!(
            describe_trigger(&json!({"kind": "daily", "timezone": "UTC", "second_of_day": 28_800})),
            "Daily at 08:00 (UTC)"
        );
        assert_eq!(
            describe_trigger(&json!({"kind": "weekly", "timezone": "Asia/Makassar",
                                     "weekdays": [1, 3, 5], "second_of_day": 34_200})),
            "Weekly on Mon, Wed, Fri at 09:30 (Asia/Makassar)"
        );
        assert_eq!(
            describe_trigger(&json!({"kind": "interval", "every_seconds": 3600, "anchor_utc": 0})),
            "Hourly"
        );
        assert_eq!(describe_trigger(&json!(null)), "Unknown schedule");
    }

    #[test]
    fn detail_orders_runs_newest_first_and_keeps_five_deadlines() {
        let detail = AutomationDetail::from_json(
            &json!({"id": "a1", "name": "Review", "enabled": true,
                    "trigger": {"kind": "once", "at_utc": 10},
                    "target": {"kind": "new_worker"},
                    "task": {"title": "Review", "prompt": "Look", "agent_id": "codex",
                             "mode": "workspace", "access": "read_only", "paths": ["src/**"],
                             "gate": ""},
                    "policy": {"misfire": "run_latest", "overlap": "skip"}}),
            &json!({"runs": [
                {"id": "r1", "status": "succeeded", "scheduled_at": 100},
                {"id": "r2", "status": "failed", "scheduled_at": 200, "error": "gate failed"}
            ]}),
            &json!({"occurrences_utc": [1, 2, 3, 4, 5, 6]}),
        );
        assert_eq!(detail.runs[0].id, "r2");
        assert_eq!(detail.occurrences.len(), 5);
        assert_eq!(detail.gate, None);
        assert_eq!(detail.policy, "Run latest missed · Skip overlapping");
        assert_eq!(detail.target, "New worker");
    }
}
