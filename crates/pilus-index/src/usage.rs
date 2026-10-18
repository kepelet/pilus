//! Token usage out of the assistant lines of a transcript.
//!
//! Each assistant turn carries the model that answered and the tokens it used.
//! A [`Turn`] is that one fact, whichever agent wrote it. Two readers produce
//! them and must agree: [`claude`] and [`pi`] read a whole line with a JSON
//! parser (this Mac), and [`REMOTE_AWK`] cuts the same fields out of the line on
//! the host so a remote transcript crosses the wire as a few tab-separated
//! fields instead of its message text. The test at the bottom runs both over
//! one set of lines.

use std::collections::HashMap;

use serde_json::Value;

use crate::parse::iso_seconds;

/// One assistant turn's usage.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Turn {
    /// The message id, which Claude repeats on every line a streamed message
    /// is written as; empty when the agent has no such id.
    pub id: String,
    pub model: String,
    /// Unix seconds.
    pub at: i64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// What the agent itself recorded as this turn's cost, in millionths of a
    /// dollar. Pi writes one beside every turn; Claude writes none, so its
    /// turns are priced from [`crate::pricing`] when they are asked about.
    pub cost_micros: Option<u64>,
}

impl Turn {
    /// Whether the turn used nothing. Claude writes a zero-token turn for a
    /// local error, under a placeholder model, which is not usage.
    fn empty(&self) -> bool {
        self.input + self.output + self.cache_read + self.cache_write == 0
    }
}

/// One hour of one model's use in one transcript: what the index stores.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Bucket {
    /// Unix hours (`at / 3600`), so a window is exact to the hour and the
    /// caller picks the time zone when it groups hours into days.
    pub hour: i64,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub turns: u64,
    /// The recorded cost of the turns that had one, in millionths of a dollar.
    pub cost_micros: u64,
    /// How many of `turns` carried a recorded cost. A transcript's turns all
    /// do or none do, so this tells a recorded bucket from one to estimate.
    pub recorded: u64,
}

/// A Claude `assistant` line, or `None` for any other line.
pub fn claude(line: &str) -> Option<Turn> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let message = value.get("message")?;
    let usage = message.get("usage")?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let turn = Turn {
        id: text(message, "id"),
        model: text(message, "model"),
        at: iso_seconds(&text(&value, "timestamp"))?,
        input: count("input_tokens"),
        output: count("output_tokens"),
        cache_read: count("cache_read_input_tokens"),
        cache_write: count("cache_creation_input_tokens"),
        cost_micros: None,
    };
    (!turn.empty() && !turn.model.starts_with('<')).then_some(turn)
}

/// A Pi `message` line from the assistant, or `None` for any other line.
pub fn pi(line: &str) -> Option<Turn> {
    let value: Value = serde_json::from_str(line).ok()?;
    let message = value.get("message")?;
    if message.get("role")?.as_str()? != "assistant" {
        return None;
    }
    let usage = message.get("usage")?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let turn = Turn {
        id: text(&value, "id"),
        model: text(message, "model"),
        at: iso_seconds(&text(&value, "timestamp"))?,
        input: count("input"),
        output: count("output"),
        cache_read: count("cacheRead"),
        cache_write: count("cacheWrite"),
        cost_micros: usage
            .get("cost")
            .and_then(|cost| cost.get("total"))
            .and_then(Value::as_f64)
            .filter(|dollars| *dollars >= 0.0)
            // Dollars to millionths: the nearest whole one, rounding half up.
            .map(|dollars| (dollars * 1_000_000.0 + 0.5).floor() as u64),
    };
    (!turn.empty()).then_some(turn)
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A turn from the tab-separated line [`REMOTE_AWK`] prints:
/// `id model timestamp input output cache_read cache_write cost`, the last
/// empty when the agent recorded no cost.
pub fn from_tsv(line: &str) -> Option<Turn> {
    let mut fields = line.split('\t');
    let id = fields.next()?.to_string();
    let model = fields.next()?.to_string();
    let at = iso_seconds(fields.next()?)?;
    let mut count = || fields.next()?.parse::<u64>().ok();
    let turn = Turn {
        id,
        model,
        at,
        input: count()?,
        output: count()?,
        cache_read: count()?,
        cache_write: count()?,
        cost_micros: fields.next().and_then(|cost| cost.parse().ok()),
    };
    (!turn.empty() && !turn.model.starts_with('<')).then_some(turn)
}

/// Keep the last line of each message id. Claude writes a streamed message
/// once per content block with the same usage block, and counting each would
/// repeat its tokens; a line without an id stands alone.
pub fn dedupe(turns: Vec<Turn>) -> Vec<Turn> {
    let mut last: HashMap<&str, usize> = HashMap::new();
    for (at, turn) in turns.iter().enumerate() {
        if !turn.id.is_empty() {
            last.insert(&turn.id, at);
        }
    }
    let keep: Vec<bool> = turns
        .iter()
        .enumerate()
        .map(|(at, turn)| turn.id.is_empty() || last.get(turn.id.as_str()) == Some(&at))
        .collect();
    turns
        .into_iter()
        .zip(keep)
        .filter_map(|(turn, keep)| keep.then_some(turn))
        .collect()
}

/// Sum `turns` by hour and model.
pub fn buckets(turns: &[Turn]) -> Vec<Bucket> {
    let mut sums: HashMap<(i64, &str), Bucket> = HashMap::new();
    for turn in turns {
        let hour = turn.at.div_euclid(3600);
        let bucket = sums.entry((hour, &turn.model)).or_insert_with(|| Bucket {
            hour,
            model: turn.model.clone(),
            ..Bucket::default()
        });
        bucket.input += turn.input;
        bucket.output += turn.output;
        bucket.cache_read += turn.cache_read;
        bucket.cache_write += turn.cache_write;
        bucket.turns += 1;
        if let Some(cost) = turn.cost_micros {
            bucket.cost_micros += cost;
            bucket.recorded += 1;
        }
    }
    let mut all: Vec<Bucket> = sums.into_values().collect();
    all.sort_by(|a, b| (a.hour, &a.model).cmp(&(b.hour, &b.model)));
    all
}

/// The awk programs the remote script runs, one per agent, each printing the
/// tab-separated line [`from_tsv`] reads. They cut fields out of the raw line
/// rather than parse it, so each leans on where its agent writes them:
/// Claude's `message` leads with its model and id and puts the token counts
/// ahead of the nested copies; Pi writes the content first, so the model and
/// the usage block are taken from the *last* place they appear. Quotes inside
/// message text are escaped in JSON, so a `"model":"` match is always a key.
pub const REMOTE_AWK: RemoteAwk = RemoteAwk {
    shared: r#"
function first(s, re) { if (match(s, re)) return substr(s, RSTART, RLENGTH); return "" }
function last(s, re,   out) { out = ""; while (match(s, re)) { out = substr(s, RSTART, RLENGTH); s = substr(s, RSTART + RLENGTH) } return out }
function value(m) { sub(/^[^:]*:"?/, "", m); sub(/"$/, "", m); return m }
function num(s, key,   m) { m = first(s, "\"" key "\":[0-9]+"); if (m == "") return 0; return value(m) }
function from_last(s, marker,   rest, i) { rest = ""; while ((i = index(s, marker)) > 0) { rest = substr(s, i); s = substr(s, i + 1) } return rest }
"#,
    claude: r#"
/"type":"assistant"/ && /"usage":\{/ {
  u = from_last($0, "\"usage\":{")
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t\n", value(first($0, "\"id\":\"msg_[^\"]*\"")), value(first($0, "\"model\":\"[^\"]*\"")), value(first($0, "\"timestamp\":\"[^\"]*\"")), num(u, "input_tokens"), num(u, "output_tokens"), num(u, "cache_read_input_tokens"), num(u, "cache_creation_input_tokens")
}
"#,
    pi: r#"
/"role":"assistant"/ && /"usage":\{"input":/ {
  u = from_last($0, "\"usage\":{\"input\":")
  c = first(u, "\"total\":[0-9.eE+-]+")
  cost = ""
  if (c != "") cost = sprintf("%.0f", int(value(c) * 1000000 + 0.5))
  printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", value(first($0, "\"id\":\"[^\"]*\"")), value(last($0, "\"model\":\"[^\"]*\"")), value(first($0, "\"timestamp\":\"[^\"]*\"")), num(u, "input"), num(u, "output"), num(u, "cacheRead"), num(u, "cacheWrite"), cost
}
"#,
};

/// See [`REMOTE_AWK`].
pub struct RemoteAwk {
    pub shared: &'static str,
    pub claude: &'static str,
    pub pi: &'static str,
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::*;

    const CLAUDE: &str = r#"{"parentUuid":null,"isSidechain":false,"message":{"model":"claude-sonnet-5-5","id":"msg_01A","type":"message","role":"assistant","content":[{"type":"text","text":"say \"model\":\"fake\" and \"input_tokens\":999"}],"stop_reason":"tool_use","usage":{"input_tokens":2,"cache_creation_input_tokens":15924,"cache_read_input_tokens":23422,"output_tokens":209,"output_tokens_details":{"thinking_tokens":26},"iterations":[{"input_tokens":2,"output_tokens":209}]}},"requestedModel":"claude-sonnet-5-5","type":"assistant","uuid":"u1","timestamp":"2026-10-09T14:15:31.131Z","sessionId":"s"}"#;
    const CLAUDE_AGAIN: &str = r#"{"message":{"model":"claude-sonnet-5-5","id":"msg_01A","type":"message","role":"assistant","content":[],"usage":{"input_tokens":2,"cache_creation_input_tokens":15924,"cache_read_input_tokens":23422,"output_tokens":300}},"type":"assistant","timestamp":"2026-10-09T14:15:40.000Z"}"#;
    const SYNTHETIC: &str = r#"{"message":{"model":"<synthetic>","id":"msg_x","role":"assistant","content":[],"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"type":"assistant","timestamp":"2026-10-09T14:15:41.000Z"}"#;
    const PI: &str = r#"{"type":"message","id":"06ebc82b","parentId":"x","timestamp":"2026-10-06T12:38:33.482Z","message":{"role":"assistant","content":[{"type":"toolCall","arguments":{"model":"decoy","usage":{"input":1}}}],"api":"openai-completions","provider":"opencode-go","model":"deepseek-v4.1-flash","usage":{"input":6153,"output":195,"cacheRead":12,"cacheWrite":3,"reasoning":47,"totalTokens":6348,"cost":{"input":0.0009,"total":0.001}},"stopReason":"toolUse","timestamp":1791290313907}}"#;
    const PI_USER: &str = r#"{"type":"message","id":"a","timestamp":"2026-10-06T12:38:30.000Z","message":{"role":"user","content":"hi"}}"#;

    #[test]
    fn claude_reads_the_model_the_usage_and_the_time() {
        let turn = claude(CLAUDE).expect("turn");
        assert_eq!(
            turn,
            Turn {
                id: "msg_01A".into(),
                model: "claude-sonnet-5-5".into(),
                at: 1_791_555_331,
                input: 2,
                output: 209,
                cache_read: 23_422,
                cache_write: 15_924,
                cost_micros: None,
            }
        );
        assert_eq!(claude(SYNTHETIC), None);
        assert_eq!(claude(PI_USER), None);
    }

    #[test]
    fn pi_reads_the_model_the_usage_and_the_time() {
        let turn = pi(PI).expect("turn");
        assert_eq!(turn.model, "deepseek-v4.1-flash");
        assert_eq!((turn.input, turn.output), (6153, 195));
        assert_eq!((turn.cache_read, turn.cache_write), (12, 3));
        assert_eq!(turn.cost_micros, Some(1000));
        assert_eq!(claude(CLAUDE).unwrap().cost_micros, None);
        assert_eq!(pi(PI_USER), None);
    }

    #[test]
    fn a_streamed_message_counts_once_with_its_last_usage() {
        let turns = dedupe(vec![claude(CLAUDE).unwrap(), claude(CLAUDE_AGAIN).unwrap()]);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].output, 300);
    }

    #[test]
    fn buckets_sum_by_hour_and_model() {
        let turn = |at, model: &str, output| Turn {
            at,
            model: model.into(),
            output,
            ..Turn::default()
        };
        let sums = buckets(&[
            turn(3600, "a", 1),
            turn(3601, "a", 2),
            turn(3602, "b", 4),
            turn(7200, "a", 8),
        ]);
        assert_eq!(sums.len(), 3);
        let costed = buckets(&[Turn {
            at: 0,
            cost_micros: Some(250),
            ..Turn::default()
        }]);
        assert_eq!((costed[0].cost_micros, costed[0].recorded), (250, 1));
        assert_eq!((sums[0].hour, sums[0].output, sums[0].turns), (1, 3, 2));
        assert_eq!((sums[1].model.as_str(), sums[1].output), ("b", 4));
        assert_eq!(sums[2].hour, 2);
    }

    /// The awk the remote script runs must read what the JSON parser reads.
    #[test]
    fn the_remote_awk_agrees_with_the_parsers() {
        let run = |program: &str, lines: &[&str]| -> Vec<String> {
            let mut child = Command::new("awk")
                .arg(format!("{}{}", REMOTE_AWK.shared, program))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("awk");
            let mut stdin = child.stdin.take().expect("stdin");
            for line in lines {
                writeln!(stdin, "{line}").expect("write");
            }
            drop(stdin);
            let out = child.wait_with_output().expect("awk output");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        };

        let lines = run(
            REMOTE_AWK.claude,
            &[CLAUDE, CLAUDE_AGAIN, SYNTHETIC, PI_USER],
        );
        let from_awk: Vec<Turn> = lines.iter().filter_map(|line| from_tsv(line)).collect();
        assert_eq!(
            from_awk,
            vec![claude(CLAUDE).unwrap(), claude(CLAUDE_AGAIN).unwrap()]
        );

        let lines = run(REMOTE_AWK.pi, &[PI, PI_USER]);
        let from_awk: Vec<Turn> = lines.iter().filter_map(|line| from_tsv(line)).collect();
        assert_eq!(from_awk, vec![pi(PI).unwrap()]);
    }
}
