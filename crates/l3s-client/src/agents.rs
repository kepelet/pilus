//! Coding-agent availability and model catalogs.
//!
//! A launch composer needs to know which agents are installed on a machine and
//! which models each offers, including the reasoning levels a model accepts.
//! Presence is proven by a config directory or a binary on `PATH`. The catalog
//! comes from the agent's own runtime where one exists — a local store, a CLI,
//! or a JSON-RPC server — and otherwise falls back to a single sane default.
//! Reads run locally or in one SSH round trip, so the GUI never blocks on them.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::transport::SshTarget;

/// How long a live model probe may take before it is abandoned.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Reasoning levels offered when an agent exposes no list of its own.
const FALLBACK_EFFORTS: [&str; 5] = ["Minimal", "Low", "Medium", "High", "Max"];

/// Canonical order for the levels agents use, so every picker reads the same.
const EFFORT_ORDER: [&str; 5] = ["minimal", "low", "medium", "high", "max"];

/// One model an agent can run, with the reasoning levels it accepts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentModel {
    /// Stable id passed back to the agent; empty when the name is the id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Supported reasoning levels; `None` when the agent exposes no list, and
    /// an empty list when the model has no reasoning mode at all.
    pub efforts: Option<Vec<String>>,
    /// The model's own default reasoning level.
    pub default_effort: Option<String>,
    /// Whether the agent marks this as its current default.
    pub is_default: bool,
}

/// One coding agent's presence and model catalog on a machine.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentConfig {
    /// Stable kind id, e.g. `pi`.
    pub kind: String,
    /// Display label, e.g. `Pi`.
    pub label: String,
    /// Where presence was proven: `~/dir` or `bin:name`.
    pub source: String,
    /// The catalog, the agent's default first.
    pub models: Vec<AgentModel>,
    /// Whether `models` came from the agent's own runtime.
    pub discovered: bool,
    /// The agent's configured default model name, when known.
    pub default_model: Option<String>,
}

impl AgentConfig {
    /// The model display names to offer, never empty.
    #[must_use]
    pub fn choices(&self) -> Vec<String> {
        if self.models.is_empty() {
            vec!["Default".to_string()]
        } else {
            self.models.iter().map(|model| model.name.clone()).collect()
        }
    }

    /// The catalog entry for a display name.
    #[must_use]
    pub fn model(&self, name: &str) -> Option<&AgentModel> {
        self.models.iter().find(|model| model.name == name)
    }

    /// The reasoning levels a model accepts, always starting with `Default`.
    ///
    /// A model with an empty level list runs without reasoning; a model the
    /// agent does not describe gets the generic set.
    #[must_use]
    pub fn efforts_for(&self, name: &str) -> Vec<String> {
        let mut levels = vec!["Default".to_string()];
        match self.model(name).and_then(|model| model.efforts.as_ref()) {
            Some(list) if list.is_empty() => {}
            Some(list) => levels.extend(list.iter().cloned()),
            None => levels.extend(FALLBACK_EFFORTS.iter().map(|level| (*level).to_string())),
        }
        levels
    }

    /// The model's own default reasoning level, when it names one.
    #[must_use]
    pub fn default_effort_for(&self, name: &str) -> Option<String> {
        self.model(name)
            .and_then(|model| model.default_effort.clone())
    }
}

/// A built-in agent's on-disk fingerprints and discovery method.
struct AgentSpec {
    kind: &'static str,
    label: &'static str,
    /// `$HOME`-relative config directories that prove the agent is installed.
    dirs: &'static [&'static str],
    /// Commands on `PATH` that prove the agent is installed.
    binaries: &'static [&'static str],
}

/// The agents Luvus knows how to launch, most relevant first.
const SPECS: &[AgentSpec] = &[
    AgentSpec {
        kind: "pi",
        label: "Pi",
        dirs: &[".pi/agent"],
        binaries: &["pi"],
    },
    AgentSpec {
        kind: "claude",
        label: "Claude",
        dirs: &[".claude"],
        binaries: &["claude"],
    },
    AgentSpec {
        kind: "codex",
        label: "Codex",
        dirs: &[".codex"],
        binaries: &["codex"],
    },
    AgentSpec {
        kind: "gemini",
        label: "Gemini",
        dirs: &[".gemini"],
        binaries: &["gemini"],
    },
    AgentSpec {
        kind: "cursor",
        label: "Cursor",
        dirs: &[".cursor"],
        binaries: &["cursor"],
    },
    AgentSpec {
        kind: "opencode",
        label: "OpenCode",
        dirs: &[".config/opencode", ".opencode"],
        binaries: &["opencode"],
    },
    AgentSpec {
        kind: "amp",
        label: "Amp",
        dirs: &[".amp"],
        binaries: &["amp"],
    },
    AgentSpec {
        kind: "aider",
        label: "Aider",
        dirs: &[".aider"],
        binaries: &["aider"],
    },
    AgentSpec {
        kind: "kimi",
        label: "Kimi",
        dirs: &[".kimi"],
        binaries: &["kimi"],
    },
    AgentSpec {
        kind: "qwen",
        label: "Qwen",
        dirs: &[".qwen"],
        binaries: &["qwen"],
    },
    AgentSpec {
        kind: "grok",
        label: "Grok",
        dirs: &[".grok"],
        binaries: &["grok"],
    },
    AgentSpec {
        kind: "omp",
        label: "Oh My Pi",
        dirs: &[".omp", ".oh-my-pi"],
        binaries: &["omp"],
    },
    AgentSpec {
        kind: "copilot",
        label: "Copilot",
        dirs: &[".copilot"],
        binaries: &["copilot"],
    },
];

/// Every installed agent on this machine, with its live model catalog.
#[must_use]
pub fn local() -> Vec<AgentConfig> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    SPECS
        .iter()
        .filter_map(|spec| {
            let mut config = local_presence(&home, spec)?;
            enrich_local(&mut config, &home);
            Some(config)
        })
        .collect()
}

/// Every installed agent on a remote machine, in one SSH round trip.
#[must_use]
pub fn remote(target: &SshTarget) -> Vec<AgentConfig> {
    let script = remote_script();
    let Ok(output) = target.shell(&script).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_remote(&output.stdout)
}

fn local_presence(home: &Path, spec: &AgentSpec) -> Option<AgentConfig> {
    let source = spec
        .dirs
        .iter()
        .find(|dir| home.join(dir).is_dir())
        .map(|dir| format!("~/{dir}"))
        .or_else(|| {
            spec.binaries
                .iter()
                .find(|bin| binary_on_path(bin))
                .map(|bin| format!("bin:{bin}"))
        })?;
    Some(base(spec, source))
}

/// Add the agent's own model catalog to a locally detected config.
fn enrich_local(config: &mut AgentConfig, home: &Path) {
    match config.kind.as_str() {
        "pi" => {
            let store = read_json(home, ".pi/agent/models-store.json");
            let settings = read_json(home, ".pi/agent/settings.json");
            apply_pi(config, store.as_ref(), settings.as_ref());
        }
        "opencode" => {
            if let Some(stdout) = run_stdout(&["opencode", "models", "--verbose"]) {
                config.models = parse_opencode_models(&stdout);
                config.discovered = !config.models.is_empty();
                mark_default(config);
            }
        }
        "codex" => {
            config.models = probe_codex("codex");
            config.discovered = !config.models.is_empty();
            mark_default(config);
        }
        _ => {}
    }
}

// -- Pi ------------------------------------------------------------------

/// Fill a Pi config from its model store and settings.
///
/// The store is a provider map; the settings name the default provider, model,
/// and thinking level. Models list the default first, then the default
/// provider's, then the rest, so the most relevant rows sit on top.
fn apply_pi(config: &mut AgentConfig, store: Option<&Value>, settings: Option<&Value>) {
    let default_provider = settings
        .and_then(|value| value.get("defaultProvider"))
        .and_then(Value::as_str);
    let default_id = settings
        .and_then(|value| value.get("defaultModel"))
        .and_then(Value::as_str);
    let default_level = settings
        .and_then(|value| value.get("defaultThinkingLevel"))
        .and_then(Value::as_str)
        .map(effort_label);
    let mut preferred: Vec<AgentModel> = Vec::new();
    let mut others: Vec<AgentModel> = Vec::new();
    if let Some(Value::Object(providers)) = store {
        for (provider, value) in providers {
            let bucket = if Some(provider.as_str()) == default_provider {
                &mut preferred
            } else {
                &mut others
            };
            let Some(models) = value.get("models").and_then(Value::as_array) else {
                continue;
            };
            for model in models {
                let id = model.get("id").and_then(Value::as_str).unwrap_or_default();
                if id.is_empty() || bucket.iter().any(|existing| existing.id == id) {
                    continue;
                }
                let name = model
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(id);
                bucket.push(AgentModel {
                    id: id.to_string(),
                    name: name.to_string(),
                    efforts: pi_efforts(model),
                    default_effort: None,
                    is_default: false,
                });
            }
        }
    }
    let mut models = preferred;
    models.extend(others);
    config.models = models;
    config.discovered = !config.models.is_empty();
    if let Some(id) = default_id
        && let Some(model) = config.models.iter_mut().find(|model| model.id == id)
    {
        model.is_default = true;
        model.default_effort = default_level.clone();
        let name = model.name.clone();
        if let Some(index) = config.models.iter().position(|model| model.name == name) {
            let model = config.models.remove(index);
            config.models.insert(0, model);
        }
    }
    config.default_model = config
        .models
        .iter()
        .find(|model| model.is_default)
        .or_else(|| config.models.first())
        .map(|model| model.name.clone());
}

/// The reasoning levels a Pi model accepts, from its `thinkingLevelMap`.
fn pi_efforts(model: &Value) -> Option<Vec<String>> {
    if model.get("reasoning").and_then(Value::as_bool) == Some(false) {
        return Some(Vec::new());
    }
    let map = model.get("thinkingLevelMap");
    let map = map.and_then(Value::as_object)?;
    let mut levels: Vec<String> = map
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(level, _)| effort_label(level))
        .collect();
    sort_efforts(&mut levels);
    Some(levels)
}

// -- OpenCode ------------------------------------------------------------

/// Parse `opencode models --verbose`: a bare `provider/model` slug line
/// followed by the model's JSON body on the next line.
fn parse_opencode_models(stdout: &str) -> Vec<AgentModel> {
    let mut models = Vec::new();
    let mut slug: Option<String> = None;
    let mut body = String::new();
    let flush = |slug: &Option<String>, body: &str, models: &mut Vec<AgentModel>| {
        let Some(slug) = slug else {
            return;
        };
        let id = slug.rsplit('/').next().unwrap_or(slug).to_string();
        let name = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.clone());
        if models.iter().any(|model: &AgentModel| model.id == id) {
            return;
        }
        models.push(AgentModel {
            id,
            name,
            efforts: None,
            default_effort: None,
            is_default: false,
        });
    };
    for line in stdout.lines() {
        let trimmed = line.trim();
        // A JSON body starts with `{`; a slug line is a bare `provider/model`.
        if !trimmed.starts_with('{') && trimmed.contains('/') && !trimmed.contains(char::is_whitespace)
        {
            flush(&slug, body.trim(), &mut models);
            slug = Some(trimmed.to_string());
            body.clear();
        } else if slug.is_some() {
            body.push_str(line);
        }
    }
    flush(&slug, body.trim(), &mut models);
    models
}

// -- Codex ---------------------------------------------------------------

/// List Codex models by running a short-lived `codex app-server` and calling
/// the `model/list` JSON-RPC method over its newline-delimited stdio.
fn probe_codex(binary: &str) -> Vec<AgentModel> {
    let mut command = Command::new(binary);
    command
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return Vec::new();
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        return Vec::new();
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return Vec::new();
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let models = codex_handshake(&mut stdin, &rx);
    let _ = child.kill();
    let _ = child.wait();
    models
}

/// Drive the initialize then `model/list` exchange, returning the catalog.
fn codex_handshake(
    stdin: &mut impl Write,
    rx: &std::sync::mpsc::Receiver<String>,
) -> Vec<AgentModel> {
    let initialize = serde_json::json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "l3s", "title": "l3s", "version": env!("CARGO_PKG_VERSION") },
        },
    });
    if writeln!(stdin, "{initialize}").is_err() || stdin.flush().is_err() {
        return Vec::new();
    }
    if await_response(rx, 1).is_none() {
        return Vec::new();
    }
    let list = serde_json::json!({ "id": 2, "method": "model/list", "params": {} });
    if writeln!(stdin, "{list}").is_err() || stdin.flush().is_err() {
        return Vec::new();
    }
    await_response(rx, 2)
        .map(|response| parse_codex_models(&response))
        .unwrap_or_default()
}

/// Read lines until the response with `id` arrives, ignoring notifications.
fn await_response(rx: &std::sync::mpsc::Receiver<String>, id: u64) -> Option<Value> {
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
        let line = rx.recv_timeout(remaining).ok()?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if value.get("error").is_some() {
            return None;
        }
        return value.get("result").cloned();
    }
}

/// Parse a `model/list` result into models with their reasoning levels.
fn parse_codex_models(result: &Value) -> Vec<AgentModel> {
    let Some(data) = result.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|entry| {
            let id = entry.get("model").and_then(Value::as_str)?;
            let name = entry
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            let mut efforts: Vec<String> = entry
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| {
                            option.get("reasoningEffort").and_then(Value::as_str)
                        })
                        .map(effort_label)
                        .collect()
                })
                .unwrap_or_default();
            sort_efforts(&mut efforts);
            let default_effort = entry
                .get("defaultReasoningEffort")
                .and_then(Value::as_str)
                .map(effort_label);
            Some(AgentModel {
                id: id.to_string(),
                name: name.to_string(),
                efforts: Some(efforts),
                default_effort,
                is_default: entry
                    .get("isDefault")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .filter(|model| !model.name.is_empty())
        .collect()
}

// -- Remote --------------------------------------------------------------

/// A POSIX script that prints one `<kind>\t<source>` line per installed agent,
/// then NUL-separated catalogs for the agents it can read in place.
fn remote_script() -> String {
    let mut script = String::new();
    for spec in SPECS {
        let dirs = join_or(spec.dirs, |dir| format!("[ -d \"$HOME/{dir}\" ]"));
        let bins = join_or(spec.binaries, |bin| {
            format!("command -v {bin} >/dev/null 2>&1")
        });
        script.push_str(&format!(
            "if {dirs}; then printf '{kind}\\tdir\\n'; elif {bins}; then printf '{kind}\\tbin\\n'; fi\n",
            kind = spec.kind
        ));
    }
    script.push_str("printf '\\0'\n");
    script.push_str("cat \"$HOME/.pi/agent/models-store.json\" 2>/dev/null\n");
    script.push_str("printf '\\0'\n");
    script.push_str("cat \"$HOME/.pi/agent/settings.json\" 2>/dev/null\n");
    script.push_str("printf '\\0'\n");
    script.push_str(
        "if command -v opencode >/dev/null 2>&1; then opencode models --verbose 2>/dev/null; fi\n",
    );
    script
}

fn join_or(parts: &[&str], render: impl Fn(&str) -> String) -> String {
    let joined = parts
        .iter()
        .map(|part| render(part))
        .collect::<Vec<_>>()
        .join(" || ");
    if joined.is_empty() {
        "false".to_string()
    } else {
        joined
    }
}

fn parse_remote(bytes: &[u8]) -> Vec<AgentConfig> {
    let mut chunks = bytes.split(|byte| *byte == 0);
    let listing = chunks.next().unwrap_or_default();
    let store = chunks
        .next()
        .and_then(|chunk| serde_json::from_slice::<Value>(chunk).ok());
    let settings = chunks
        .next()
        .and_then(|chunk| serde_json::from_slice::<Value>(chunk).ok());
    let opencode = chunks
        .next()
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .unwrap_or_default();
    let found: HashMap<String, String> = String::from_utf8_lossy(listing)
        .lines()
        .filter_map(|line| {
            let (kind, source) = line.split_once('\t')?;
            Some((kind.to_string(), source.to_string()))
        })
        .collect();
    SPECS
        .iter()
        .filter_map(|spec| {
            let source = found.get(spec.kind)?.clone();
            let mut config = base(spec, source);
            if spec.kind == "pi" {
                apply_pi(&mut config, store.as_ref(), settings.as_ref());
            } else if spec.kind == "opencode" && !opencode.trim().is_empty() {
                config.models = parse_opencode_models(&opencode);
                config.discovered = !config.models.is_empty();
                mark_default(&mut config);
            }
            Some(config)
        })
        .collect()
}

// -- Shared helpers ------------------------------------------------------

fn base(spec: &AgentSpec, source: String) -> AgentConfig {
    AgentConfig {
        kind: spec.kind.to_string(),
        label: spec.label.to_string(),
        source,
        models: Vec::new(),
        discovered: false,
        default_model: None,
    }
}

/// Point `default_model` at the model the agent flagged, else the first.
fn mark_default(config: &mut AgentConfig) {
    config.default_model = config
        .models
        .iter()
        .find(|model| model.is_default)
        .or_else(|| config.models.first())
        .map(|model| model.name.clone());
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn read_json(home: &Path, relative: &str) -> Option<Value> {
    let text = std::fs::read_to_string(home.join(relative)).ok()?;
    serde_json::from_str(&text).ok()
}

fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

/// Run `binary args…`, returning stdout only if it exits within the timeout.
fn run_stdout(args: &[&str]) -> Option<String> {
    let (binary, rest) = args.split_first()?;
    let mut command = Command::new(binary);
    command
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = tx.send(text);
    });
    match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(text) => {
            let _ = child.wait();
            Some(text)
        }
        Err(_) => {
            let _ = child.kill();
            None
        }
    }
}

/// Order reasoning levels by the canonical scale, then alphabetically.
fn sort_efforts(levels: &mut Vec<String>) {
    levels.sort_by(|left, right| {
        let rank = |value: &str| {
            EFFORT_ORDER
                .iter()
                .position(|level| value.eq_ignore_ascii_case(level))
                .unwrap_or(EFFORT_ORDER.len())
        };
        rank(left).cmp(&rank(right)).then_with(|| left.cmp(right))
    });
    levels.dedup();
}

fn effort_label(level: &str) -> String {
    match level.to_ascii_lowercase().as_str() {
        "xhigh" | "extra-high" | "extra_high" => "Extra High".to_string(),
        "none" => "None".to_string(),
        other => title_case(other),
    }
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pi_orders_the_default_first_and_reads_thinking_levels() {
        let store = json!({
            "other": { "models": [{ "id": "x", "name": "X", "reasoning": true }] },
            "main": { "models": [
                { "id": "a", "name": "A", "reasoning": true,
                  "thinkingLevelMap": { "low": "l", "high": "h", "max": null } },
                { "id": "b", "name": "B", "reasoning": false }
            ] }
        });
        let settings = json!({
            "defaultProvider": "main",
            "defaultModel": "b",
            "defaultThinkingLevel": "high"
        });
        let mut config = AgentConfig::default();
        apply_pi(&mut config, Some(&store), Some(&settings));
        assert_eq!(config.choices(), vec!["B", "A", "X"]);
        assert_eq!(config.default_model.as_deref(), Some("B"));
        assert_eq!(config.models[0].efforts, Some(Vec::new()));
        let a = config.model("A").expect("model A");
        assert_eq!(a.efforts.as_deref(), Some(&["Low".to_string(), "High".to_string()][..]));
        assert_eq!(config.model("X").expect("model X").efforts, None);
    }

    #[test]
    fn efforts_always_start_with_default() {
        let mut config = AgentConfig::default();
        config.models.push(AgentModel {
            id: "a".into(),
            name: "A".into(),
            efforts: Some(vec!["Low".into(), "High".into()]),
            default_effort: Some("High".into()),
            is_default: true,
        });
        assert_eq!(config.efforts_for("A"), vec!["Default", "Low", "High"]);
        assert_eq!(config.default_effort_for("A").as_deref(), Some("High"));
        assert_eq!(config.efforts_for("missing")[0], "Default");
    }

    #[test]
    fn opencode_slug_and_body_parse() {
        let stdout = "acme/alpha\n{\n  \"name\": \"Alpha\"\n}\nacme/beta\n{\"name\":\"\"}\n";
        let models = parse_opencode_models(stdout);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "Alpha");
        assert_eq!(models[1].id, "beta");
        assert_eq!(models[1].name, "beta");
    }

    #[test]
    fn codex_list_reads_reasoning_efforts() {
        let result = json!({ "data": [
            { "model": "gpt-x", "displayName": "GPT X", "isDefault": true,
              "supportedReasoningEfforts": [
                { "reasoningEffort": "low" }, { "reasoningEffort": "high" }
              ],
              "defaultReasoningEffort": "high" }
        ]});
        let models = parse_codex_models(&result);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "GPT X");
        assert_eq!(models[0].efforts.as_deref(), Some(&["Low".to_string(), "High".to_string()][..]));
        assert_eq!(models[0].default_effort.as_deref(), Some("High"));
        assert!(models[0].is_default);
    }

    #[test]
    fn remote_listing_marks_presence_and_reads_pi() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"pi\tdir\nopencode\tbin\n");
        bytes.push(0);
        bytes.extend_from_slice(br#"{"p":{"models":[{"id":"m","name":"M"}]}}"#);
        bytes.push(0);
        bytes.extend_from_slice(br#"{"defaultModel":"m"}"#);
        bytes.push(0);
        bytes.extend_from_slice(b"acme/code\n{\"name\":\"Code\"}\n");
        let configs = parse_remote(&bytes);
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].kind, "pi");
        assert_eq!(configs[0].models[0].name, "M");
        assert_eq!(configs[1].kind, "opencode");
        assert_eq!(configs[1].models[0].name, "Code");
    }
}
