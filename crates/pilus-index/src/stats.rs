//! Questions the Stats page asks of the usage the index holds.
//!
//! Every query takes a [`Scope`] (one machine or all, and how far back) and
//! answers from the hourly buckets, so the same rows serve a total, a ranking
//! and a heatmap. Token counts are what the agents reported. Cost is the
//! agent's own recorded figure where it wrote one (Pi) and a list-price
//! estimate where it did not (Claude): an estimate, not a bill.

use std::collections::{BTreeMap, HashMap};

use rusqlite::params_from_iter;
use rusqlite::types::Value;

use crate::error::Result;
use crate::model::Agent;
use crate::pricing;
use crate::store::Index;

/// What a query covers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Scope {
    /// One machine (`local` or a profile id), or all of them.
    pub machine: Option<String>,
    /// The first unix hour included, or all time.
    pub since_hour: Option<i64>,
}

impl Scope {
    /// The `WHERE` clause (with a leading `WHERE`) and its parameters, for a
    /// query that names the usage table `u`.
    fn clause(&self) -> (String, Vec<Value>) {
        let mut parts = Vec::new();
        let mut args = Vec::new();
        if let Some(machine) = &self.machine {
            args.push(Value::Text(machine.clone()));
            parts.push(format!("u.machine = ?{}", args.len()));
        }
        if let Some(hour) = self.since_hour {
            args.push(Value::Integer(hour));
            parts.push(format!("u.hour >= ?{}", args.len()));
        }
        if parts.is_empty() {
            (String::new(), args)
        } else {
            (format!(" WHERE {}", parts.join(" AND ")), args)
        }
    }
}

/// Token counts by kind.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Assistant turns, which is how many times a model answered.
    pub turns: u64,
}

impl Tokens {
    /// Every token the models processed, cached or not.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn add(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.turns += other.turns;
    }
}

/// What the use cost.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cost {
    /// Millionths of a dollar: recorded by the agent where it did, estimated
    /// at list price where it did not.
    pub micros: u64,
    /// Tokens whose model has no recorded cost and no known price, which are
    /// left out of `micros` rather than guessed at.
    pub unpriced: u64,
}

impl Cost {
    /// The cost in dollars.
    #[must_use]
    pub fn dollars(&self) -> f64 {
        // Millionths of a dollar fit f64 exactly well past any real total.
        #[allow(clippy::cast_precision_loss)]
        let micros = self.micros as f64;
        micros / 1_000_000.0
    }

    fn add(&mut self, other: &Self) {
        self.micros += other.micros;
        self.unpriced += other.unpriced;
    }
}

/// The headline figures of a scope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Overview {
    pub tokens: Tokens,
    pub cost: Cost,
    /// Transcripts with any usage in the scope.
    pub sessions: u64,
}

/// One row of a ranking.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Ranked {
    /// An agent kind, a model, a folder or a machine, by the ranking asked for.
    pub name: String,
    /// The machine a folder is on; empty for any other ranking.
    pub machine: String,
    pub sessions: u64,
    pub tokens: Tokens,
    pub cost: Cost,
}

/// One hour's use, for grouping into days or months in the caller's time zone.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Hourly {
    pub hour: i64,
    pub turns: u64,
    pub tokens: u64,
    pub cost: Cost,
}

/// One group of the usage table: a key, a model, and what they used. The cost
/// is split by where it comes from so that a model priced by the agent and the
/// same model priced from the table are never mixed up.
struct Group {
    key: String,
    model: String,
    tokens: Tokens,
    /// The cost the agent recorded for the buckets that had one.
    recorded_micros: u64,
    /// The tokens of the buckets with no recorded cost, to estimate.
    unrecorded: Tokens,
}

impl Group {
    fn cost(&self) -> Cost {
        let estimate = pricing::estimate(&self.model, &self.unrecorded);
        Cost {
            micros: self.recorded_micros + estimate.unwrap_or(0),
            unpriced: if estimate.is_some() {
                0
            } else {
                self.unrecorded.total()
            },
        }
    }
}

fn count(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<u64> {
    Ok(row.get::<_, i64>(at)?.unsigned_abs())
}

impl Index {
    /// Totals for `scope`.
    pub fn overview(&self, scope: &Scope) -> Result<Overview> {
        let mut overview = Overview::default();
        for group in self.groups("''", false, scope)? {
            overview.tokens.add(&group.tokens);
            overview.cost.add(&group.cost());
        }
        overview.sessions = self.session_counts("''", false, scope)?.values().sum();
        Ok(overview)
    }

    /// Agents by tokens, most first.
    pub fn top_agents(&self, scope: &Scope) -> Result<Vec<Ranked>> {
        let rows = self.ranked("s.agent", true, scope, usize::MAX)?;
        // A row from an agent this build does not read is left out, as it is
        // from a search.
        Ok(rows
            .into_iter()
            .filter(|row| Agent::from_kind(&row.name).is_some())
            .collect())
    }

    /// Models by tokens, most first.
    pub fn top_models(&self, scope: &Scope, limit: usize) -> Result<Vec<Ranked>> {
        self.ranked("u.model", false, scope, limit)
    }

    /// Folders by tokens, most first. A transcript with no known folder is
    /// grouped under an empty name. The same path on two machines is two
    /// folders, so each row names its machine.
    pub fn top_projects(&self, scope: &Scope, limit: usize) -> Result<Vec<Ranked>> {
        let rows = self.ranked(
            "u.machine || char(9) || COALESCE(s.cwd, '')",
            true,
            scope,
            limit,
        )?;
        Ok(rows
            .into_iter()
            .map(|mut row| {
                if let Some((machine, cwd)) = row.name.split_once('\t') {
                    row.machine = machine.to_string();
                    row.name = cwd.to_string();
                }
                row
            })
            .collect())
    }

    /// Machines by tokens, most first.
    pub fn top_machines(&self, scope: &Scope) -> Result<Vec<Ranked>> {
        self.ranked("u.machine", false, scope, usize::MAX)
    }

    /// Use by hour, oldest first, for the hours that had any.
    pub fn hourly(&self, scope: &Scope) -> Result<Vec<Hourly>> {
        let mut hours: BTreeMap<i64, Hourly> = BTreeMap::new();
        for group in self.groups("CAST(u.hour AS TEXT)", false, scope)? {
            let Ok(hour) = group.key.parse::<i64>() else {
                continue;
            };
            let entry = hours.entry(hour).or_insert(Hourly {
                hour,
                ..Hourly::default()
            });
            entry.turns += group.tokens.turns;
            entry.tokens += group.tokens.total();
            entry.cost.add(&group.cost());
        }
        Ok(hours.into_values().collect())
    }

    fn ranked(
        &self,
        column: &str,
        joined: bool,
        scope: &Scope,
        limit: usize,
    ) -> Result<Vec<Ranked>> {
        let mut rows: BTreeMap<String, Ranked> = BTreeMap::new();
        for group in self.groups(column, joined, scope)? {
            let row = rows.entry(group.key.clone()).or_insert_with(|| Ranked {
                name: group.key.clone(),
                ..Ranked::default()
            });
            row.tokens.add(&group.tokens);
            row.cost.add(&group.cost());
        }
        let sessions = self.session_counts(column, joined, scope)?;
        let mut ranked: Vec<Ranked> = rows
            .into_values()
            .map(|mut row| {
                row.sessions = sessions.get(&row.name).copied().unwrap_or(0);
                row
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.tokens
                .total()
                .cmp(&a.tokens.total())
                .then_with(|| a.name.cmp(&b.name))
        });
        ranked.truncate(limit);
        Ok(ranked)
    }

    /// How many distinct transcripts each `key` has usage in. Counted apart
    /// from [`Self::groups`] because a transcript that used two models is two
    /// groups but one session.
    fn session_counts(
        &self,
        key: &str,
        joined: bool,
        scope: &Scope,
    ) -> Result<HashMap<String, u64>> {
        let (clause, args) = scope.clause();
        let from = if joined {
            "usage u JOIN sessions s ON s.machine = u.machine AND s.path = u.path"
        } else {
            "usage u"
        };
        let sql = format!(
            "SELECT {key}, COUNT(DISTINCT u.machine || char(9) || u.path)
             FROM {from}{clause} GROUP BY {key}"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(args.iter()), |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                count(row, 1)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The usage table grouped by `key` (a SQL expression) and model. `joined`
    /// names the sessions table `s`, for a key that lives there.
    fn groups(&self, key: &str, joined: bool, scope: &Scope) -> Result<Vec<Group>> {
        let (clause, args) = scope.clause();
        let from = if joined {
            "usage u JOIN sessions s ON s.machine = u.machine AND s.path = u.path"
        } else {
            "usage u"
        };
        let unrecorded = |column: &str| {
            format!("COALESCE(SUM(CASE WHEN u.recorded = 0 THEN u.{column} ELSE 0 END), 0)")
        };
        let sql = format!(
            "SELECT {key}, u.model,
                    COALESCE(SUM(u.input), 0), COALESCE(SUM(u.output), 0),
                    COALESCE(SUM(u.cache_read), 0), COALESCE(SUM(u.cache_write), 0),
                    COALESCE(SUM(u.turns), 0),
                    COALESCE(SUM(CASE WHEN u.recorded > 0 THEN u.cost_micros ELSE 0 END), 0),
                    {}, {}, {}, {}
             FROM {from}{clause} GROUP BY {key}, u.model",
            unrecorded("input"),
            unrecorded("output"),
            unrecorded("cache_read"),
            unrecorded("cache_write"),
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(args.iter()), |row| {
            Ok(Group {
                key: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                model: row.get(1)?,
                tokens: Tokens {
                    input: count(row, 2)?,
                    output: count(row, 3)?,
                    cache_read: count(row, 4)?,
                    cache_write: count(row, 5)?,
                    turns: count(row, 6)?,
                },
                recorded_micros: count(row, 7)?,
                unrecorded: Tokens {
                    input: count(row, 8)?,
                    output: count(row, 9)?,
                    cache_read: count(row, 10)?,
                    cache_write: count(row, 11)?,
                    turns: 0,
                },
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Session;
    use crate::usage::Bucket;

    fn session(machine: &str, agent: Agent, id: &str, cwd: &str) -> Session {
        Session {
            machine: machine.to_string(),
            agent,
            session_id: id.to_string(),
            cwd: cwd.to_string(),
            title: id.to_string(),
            branch: String::new(),
            started: 0,
            updated: 0,
            path: format!("/{machine}/{id}.jsonl"),
            size: 1,
        }
    }

    fn bucket(hour: i64, model: &str, input: u64, output: u64) -> Bucket {
        Bucket {
            hour,
            model: model.to_string(),
            input,
            output,
            cache_read: 100,
            cache_write: 10,
            turns: 2,
            cost_micros: 0,
            recorded: 0,
        }
    }

    fn seeded() -> Index {
        let mut index = Index::in_memory().expect("index");
        index
            .upsert_with_usage(&[
                (
                    session("local", Agent::Claude, "a", "/work/app"),
                    vec![bucket(10, "opus", 5, 50), bucket(11, "opus", 5, 50)],
                ),
                (
                    session("local", Agent::Pi, "b", "/work/lib"),
                    vec![bucket(11, "flash", 1, 1)],
                ),
                (
                    session("box", Agent::Claude, "c", "/work/app"),
                    vec![bucket(50, "sonnet", 20, 20)],
                ),
            ])
            .expect("upsert");
        index
    }

    #[test]
    fn overview_sums_tokens_and_counts_sessions() {
        let index = seeded();
        let all = index.overview(&Scope::default()).expect("overview");
        assert_eq!(all.sessions, 3);
        assert_eq!(all.tokens.input, 31);
        assert_eq!(all.tokens.output, 121);
        assert_eq!(all.tokens.cache_read, 400);
        assert_eq!(all.tokens.turns, 8);
        assert_eq!(all.tokens.total(), 31 + 121 + 400 + 40);

        let local = Scope {
            machine: Some("local".into()),
            since_hour: None,
        };
        assert_eq!(index.overview(&local).expect("overview").sessions, 2);
    }

    #[test]
    fn a_window_keeps_only_the_hours_inside_it() {
        let index = seeded();
        let recent = Scope {
            machine: None,
            since_hour: Some(11),
        };
        let overview = index.overview(&recent).expect("overview");
        assert_eq!(overview.sessions, 3);
        assert_eq!(overview.tokens.turns, 6);
        let none = Scope {
            machine: None,
            since_hour: Some(1000),
        };
        assert_eq!(
            index.overview(&none).expect("overview"),
            Overview::default()
        );
    }

    #[test]
    fn rankings_put_the_heaviest_first() {
        let index = seeded();
        let scope = Scope::default();
        let agents = index.top_agents(&scope).expect("agents");
        assert_eq!(agents[0].name, "claude");
        assert_eq!(agents[0].sessions, 2);
        assert_eq!(agents[1].name, "pi");

        let models = index.top_models(&scope, 2).expect("models");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "opus");

        let projects = index.top_projects(&scope, 5).expect("projects");
        // The same folder on two machines is two rows, each with its machine.
        assert_eq!(projects[0].name, "/work/app");
        assert_eq!(projects[0].machine, "local");
        assert_eq!(projects[0].sessions, 1);
        assert_eq!(projects.len(), 3);
        assert!(projects.iter().any(|row| row.machine == "box"));

        let machines = index.top_machines(&scope).expect("machines");
        assert_eq!(machines.len(), 2);
        assert_eq!(machines[0].name, "local");
        assert_eq!(machines[0].sessions, 2);
    }

    #[test]
    fn hourly_groups_across_sessions() {
        let index = seeded();
        let hours = index.hourly(&Scope::default()).expect("hourly");
        let at = |hour: i64| hours.iter().find(|row| row.hour == hour).copied();
        assert_eq!(at(11).expect("hour 11").turns, 4);
        assert_eq!(hours.first().map(|row| row.hour), Some(10));
    }

    #[test]
    fn a_rewritten_transcript_replaces_its_usage_and_removal_drops_it() {
        let mut index = seeded();
        let again = session("local", Agent::Claude, "a", "/work/app");
        index
            .upsert_with_usage(&[(again, vec![bucket(10, "opus", 1, 1)])])
            .expect("upsert");
        let local = Scope {
            machine: Some("local".into()),
            since_hour: None,
        };
        assert_eq!(index.overview(&local).expect("overview").tokens.turns, 4);
        index
            .remove("local", &["/local/a.jsonl".to_string()])
            .expect("remove");
        assert_eq!(index.overview(&local).expect("overview").sessions, 1);
        index.forget("local").expect("forget");
        assert_eq!(
            index.overview(&local).expect("overview"),
            Overview::default()
        );
    }

    #[test]
    fn cost_is_recorded_where_the_agent_wrote_it_and_estimated_where_not() {
        let mut index = Index::in_memory().expect("index");
        let tokens = |model: &str, recorded: u64, cost_micros: u64| Bucket {
            hour: 5,
            model: model.to_string(),
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 0,
            cache_write: 0,
            turns: 1,
            cost_micros,
            recorded,
        };
        index
            .upsert_with_usage(&[
                // Claude records no cost: priced from the table, 2 + 10 dollars.
                (
                    session("local", Agent::Claude, "a", "/w"),
                    vec![tokens("claude-sonnet-5-5", 0, 0)],
                ),
                // Pi's own figure stands, whatever the model is called.
                (
                    session("local", Agent::Pi, "b", "/w"),
                    vec![tokens("deepseek-v4.1-flash", 1, 1_500_000)],
                ),
                // No record and no price: counted as unpriced, not as free.
                (
                    session("local", Agent::Claude, "c", "/w"),
                    vec![tokens("claude-mystery-9", 0, 0)],
                ),
            ])
            .expect("upsert");

        let all = index.overview(&Scope::default()).expect("overview");
        assert_eq!(all.cost.micros, 12_000_000 + 1_500_000);
        assert_eq!(all.cost.unpriced, 2_000_000);
        assert!((all.cost.dollars() - 13.5).abs() < 1e-9);

        let models = index.top_models(&Scope::default(), 5).expect("models");
        let cost_of = |name: &str| {
            models
                .iter()
                .find(|row| row.name == name)
                .map(|row| row.cost)
        };
        assert_eq!(
            cost_of("claude-sonnet-5-5").expect("sonnet").micros,
            12_000_000
        );
        assert_eq!(
            cost_of("deepseek-v4.1-flash").expect("pi").micros,
            1_500_000
        );
        assert_eq!(cost_of("claude-mystery-9").expect("mystery").micros, 0);

        let hours = index.hourly(&Scope::default()).expect("hourly");
        assert_eq!(hours.len(), 1);
        assert_eq!(hours[0].cost.micros, 13_500_000);
        let agents = index.top_agents(&Scope::default()).expect("agents");
        let claude = agents
            .iter()
            .find(|row| row.name == "claude")
            .expect("claude");
        assert_eq!((claude.sessions, claude.cost.micros), (2, 12_000_000));
    }
}
