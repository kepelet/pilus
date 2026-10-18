//! Titles and metadata out of the few transcript lines worth reading.
//!
//! The functions here are pure: [`scan`](crate::scan) decides which lines to
//! fetch (and, for a remote machine, filters them where they live), so a
//! parser sees only [`Evidence`] and never a file. A line that does not parse
//! is skipped, because a transcript being written can end mid-line.

use serde_json::Value;

use crate::model::Agent;

/// The lines of one transcript that can name it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Evidence {
    /// The first lines that look like a session header or a user message.
    pub head: Vec<String>,
    /// The last line of each kind that renames the session.
    pub titles: Vec<String>,
}

/// What a transcript says about itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Parsed {
    pub session_id: String,
    pub cwd: String,
    /// Empty for a session nobody has written to; the store hides those.
    pub title: String,
    pub branch: String,
    /// Unix seconds of the first prompt; `0` when the transcript has none.
    pub started: i64,
}

/// Longest title kept, in characters.
const TITLE_CHARS: usize = 140;

/// Read `evidence` for `agent`. `file_stem` is the transcript's name without
/// `.jsonl`, which is the id where the file does not carry one.
pub fn parse(agent: Agent, file_stem: &str, evidence: &Evidence) -> Parsed {
    match agent {
        Agent::Claude => claude(file_stem, evidence),
        Agent::Pi => pi(file_stem, evidence),
    }
}

fn claude(file_stem: &str, evidence: &Evidence) -> Parsed {
    let mut parsed = Parsed {
        session_id: file_stem.to_string(),
        ..Parsed::default()
    };
    let mut prompt = None;
    let mut tagged = None;
    for line in &evidence.head {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("user")
            || flag(&value, "isSidechain")
            || flag(&value, "isMeta")
        {
            continue;
        }
        if parsed.cwd.is_empty() {
            parsed.cwd = text_of(&value, "cwd");
        }
        if parsed.branch.is_empty() {
            parsed.branch = text_of(&value, "gitBranch");
        }
        let Some(text) = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(prompt_text)
        else {
            continue;
        };
        if parsed.started == 0 {
            parsed.started = iso_seconds(&text_of(&value, "timestamp")).unwrap_or(0);
        }
        // Slash commands and hooks arrive as tagged pseudo-prompts; a real
        // prompt after them is a better name, so they only fill a gap.
        if text.trim_start().starts_with('<') {
            tagged.get_or_insert(text);
        } else {
            prompt = Some(text);
            break;
        }
    }
    let mut custom = None;
    let mut generated = None;
    for line in &evidence.titles {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("custom-title") => custom = Some(text_of(&value, "customTitle")),
            Some("ai-title") => generated = Some(text_of(&value, "aiTitle")),
            _ => {}
        }
    }
    parsed.title = [custom, generated, prompt, tagged]
        .into_iter()
        .flatten()
        .map(|title| tidy(&title))
        .find(|title| !title.is_empty())
        .unwrap_or_default();
    parsed
}

fn pi(file_stem: &str, evidence: &Evidence) -> Parsed {
    let mut parsed = Parsed {
        session_id: file_stem
            .rsplit('_')
            .next()
            .unwrap_or(file_stem)
            .to_string(),
        ..Parsed::default()
    };
    let mut prompt = None;
    for line in &evidence.head {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                let id = text_of(&value, "id");
                if !id.is_empty() {
                    parsed.session_id = id;
                }
                parsed.cwd = text_of(&value, "cwd");
                parsed.started = iso_seconds(&text_of(&value, "timestamp")).unwrap_or(0);
            }
            Some("message") if prompt.is_none() => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                if message.get("role").and_then(Value::as_str) == Some("user") {
                    prompt = message.get("content").and_then(prompt_text);
                }
            }
            _ => {}
        }
    }
    let named = evidence
        .titles
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("session_info"))
        .map(|value| text_of(&value, "name"))
        .next_back();
    parsed.title = [named, prompt]
        .into_iter()
        .flatten()
        .map(|title| tidy(&title))
        .find(|title| !title.is_empty())
        .unwrap_or_default();
    parsed
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The prompt's text: a bare string, or the first text block of a list (a
/// tool result is a block of another type, so it never becomes a title).
fn prompt_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => blocks.iter().find_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
                .map(str::to_string)
        }),
        _ => None,
    }
}

/// One line of at most [`TITLE_CHARS`] characters.
fn tidy(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= TITLE_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(TITLE_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// Unix seconds of an RFC 3339 UTC timestamp (`2026-10-09T14:15:31.131Z`).
fn iso_seconds(text: &str) -> Option<i64> {
    let (date, time) = text.split_once('T')?;
    let mut ymd = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let time = time.trim_end_matches('Z');
    let time = time.split(['.', '+']).next()?;
    let mut hms = time.splitn(3, ':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (hms.next()??, hms.next()??, hms.next()??);
    // Days since 1970-01-01 by the civil-calendar algorithm, so no date crate
    // is needed for one conversion.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted = (month + 9) % 12;
    let day_of_year = (153 * shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(head: &[&str], titles: &[&str]) -> Evidence {
        Evidence {
            head: head.iter().map(|line| (*line).to_string()).collect(),
            titles: titles.iter().map(|line| (*line).to_string()).collect(),
        }
    }

    #[test]
    fn iso_seconds_matches_the_epoch() {
        assert_eq!(iso_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_seconds("2026-10-09T14:15:31.131Z"), Some(1_791_555_331));
        assert_eq!(iso_seconds("2024-03-01T00:00:00Z"), Some(1_709_251_200));
        assert_eq!(iso_seconds("nonsense"), None);
    }

    #[test]
    fn claude_prefers_a_custom_title_then_a_generated_one_then_the_prompt() {
        let user = r#"{"type":"user","cwd":"/work/app","gitBranch":"main","timestamp":"2026-10-09T14:15:31.131Z","message":{"role":"user","content":"check the relay\n  config"}}"#;
        let custom = r#"{"type":"custom-title","customTitle":"relay audit"}"#;
        let ai = r#"{"type":"ai-title","aiTitle":"Custom relay URL"}"#;

        let parsed = parse(Agent::Claude, "abc", &evidence(&[user], &[ai, custom]));
        assert_eq!(parsed.title, "relay audit");
        assert_eq!(parsed.cwd, "/work/app");
        assert_eq!(parsed.branch, "main");
        assert_eq!(parsed.session_id, "abc");
        assert_eq!(parsed.started, 1_791_555_331);

        let parsed = parse(Agent::Claude, "abc", &evidence(&[user], &[ai]));
        assert_eq!(parsed.title, "Custom relay URL");

        let parsed = parse(Agent::Claude, "abc", &evidence(&[user], &[]));
        assert_eq!(parsed.title, "check the relay config");
    }

    #[test]
    fn claude_skips_tool_results_hooks_and_sidechains() {
        let result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#;
        let meta = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"caveat"}}"#;
        let side = r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"agent work"}}"#;
        let tagged = r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#;
        let real = r#"{"type":"user","cwd":"/w","message":{"role":"user","content":[{"type":"text","text":"fix the build"}]}}"#;

        let parsed = parse(
            Agent::Claude,
            "s",
            &evidence(&[result, meta, side, tagged, real], &[]),
        );
        assert_eq!(parsed.title, "fix the build");

        let parsed = parse(Agent::Claude, "s", &evidence(&[tagged], &[]));
        assert_eq!(parsed.title, "<command-name>/clear</command-name>");
    }

    #[test]
    fn claude_without_a_prompt_has_no_title() {
        assert_eq!(parse(Agent::Claude, "s", &evidence(&[], &[])).title, "");
    }

    #[test]
    fn pi_reads_the_header_and_the_first_user_message() {
        let header = r#"{"type":"session","version":3,"id":"01a1","timestamp":"2026-10-06T12:38:33.370Z","cwd":"/Users/me"}"#;
        let asked = r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"hi there"}]}}"#;
        let parsed = parse(Agent::Pi, "2026_01a1", &evidence(&[header, asked], &[]));
        assert_eq!(parsed.session_id, "01a1");
        assert_eq!(parsed.cwd, "/Users/me");
        assert_eq!(parsed.title, "hi there");
        assert_eq!(parsed.started, 1_791_290_313);

        let named = r#"{"type":"session_info","name":"Refactor"}"#;
        let parsed = parse(Agent::Pi, "x", &evidence(&[header, asked], &[named]));
        assert_eq!(parsed.title, "Refactor");
    }

    #[test]
    fn pi_takes_the_id_from_the_file_name_when_the_header_is_missing() {
        let parsed = parse(
            Agent::Pi,
            "2026-04-29T02-22-33-442Z_019dd70b",
            &evidence(&[], &[]),
        );
        assert_eq!(parsed.session_id, "019dd70b");
    }

    #[test]
    fn a_torn_last_line_is_skipped() {
        let torn = r#"{"type":"user","message":{"role":"us"#;
        assert_eq!(parse(Agent::Claude, "s", &evidence(&[torn], &[])).title, "");
    }

    #[test]
    fn a_long_title_is_cut_on_a_character() {
        let long = "é".repeat(300);
        let line = format!(r#"{{"type":"user","message":{{"role":"user","content":"{long}"}}}}"#);
        let title = parse(Agent::Claude, "s", &evidence(&[&line], &[])).title;
        assert_eq!(title.chars().count(), TITLE_CHARS);
        assert!(title.ends_with('…'));
    }
}
