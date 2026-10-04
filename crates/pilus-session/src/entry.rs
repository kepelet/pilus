//! The transcript model and the JSONL fold that builds it.
//!
//! Pi's file is a tree of typed entries; a native transcript only needs the
//! active conversation in order, so [`Transcript::push_line`] projects each
//! entry into one or more flat [`Item`]s as it is read. Entry types and
//! message roles the client does not know are counted and dropped rather than
//! failing the whole file, so a newer pi still renders.

use serde_json::Value;

/// Session metadata from the file's first line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// Session format version (1 linear, 2 tree, 3 `custom` role).
    pub version: u32,
    /// Session id from the filename/header.
    pub id: String,
    /// Working directory the session was recorded in.
    pub cwd: String,
    /// ISO 8601 timestamp the session was created.
    pub timestamp: String,
    /// A user-assigned display name, from a later `session_info` entry.
    pub name: Option<String>,
    /// The session this one was forked from, when it has a parent.
    pub parent_session: Option<String>,
}

/// Token and cost totals attributed to a session.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
    /// Total cost in USD, summed across every usage-bearing entry.
    pub cost: f64,
}

impl Usage {
    /// Add one usage object, ignoring fields the object omits.
    fn add(&mut self, usage: &Value) {
        self.input += field_u64(usage, "input");
        self.output += field_u64(usage, "output");
        self.cache_read += field_u64(usage, "cacheRead");
        self.cache_write += field_u64(usage, "cacheWrite");
        self.total += field_u64(usage, "totalTokens");
        if let Some(cost) = usage.get("cost") {
            self.cost += field_f64(cost, "total");
        }
    }
}

/// One ordered transcript item.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Stable entry id, for element keys.
    pub id: String,
    /// ISO 8601 entry timestamp.
    pub timestamp: String,
    /// What the item says.
    pub kind: ItemKind,
}

/// The shape of one transcript item.
#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    /// A prompt typed by the user.
    User {
        text: String,
        /// Number of attached images; their data is not decoded here.
        images: usize,
    },
    /// Prose produced by the model.
    Assistant { text: String, model: Option<String> },
    /// The model's reasoning, shown muted and secondary.
    Thinking { text: String },
    /// A tool the model asked to run, with what it returned.
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
        /// The matching result, attached when its entry arrives.
        outcome: Option<ToolOutcome>,
    },
    /// A tool result whose call is not in the transcript (e.g. compacted away).
    ToolResult {
        call_id: String,
        name: String,
        text: String,
        images: usize,
        is_error: bool,
    },
    /// A direct shell command, not an LLM tool result.
    Bash {
        command: String,
        output: String,
        exit_code: Option<i64>,
        cancelled: bool,
        truncated: bool,
    },
    /// A structural note: model/thinking change, compaction, branch summary.
    Note { title: String, text: String },
}

/// What a tool call returned, paired with its call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcome {
    /// The tool's textual output.
    pub text: String,
    /// Number of attached images; their data is not decoded here.
    pub images: usize,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

/// A rendered conversation: the header plus flat items in file order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transcript {
    pub header: Header,
    /// Whether a `session` header line has been seen.
    pub has_header: bool,
    pub items: Vec<Item>,
    pub usage: Usage,
    /// The model selected by the latest `model_change` or assistant message.
    pub model: Option<String>,
    /// Every model spelling seen in this session, in first-seen order, for
    /// the composer's model picker.
    pub models: Vec<String>,
    /// The thinking level selected by the latest `thinking_level_change`.
    pub thinking_level: Option<String>,
    /// Count of entry types this build did not recognize.
    pub unknown: usize,
}

impl Transcript {
    /// Fold one JSONL line into the transcript.
    ///
    /// Returns whether anything changed, so a tailing caller only repaints
    /// when the file advanced. A blank or malformed line is ignored.
    pub fn push_line(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        self.apply(&value)
    }

    /// Fold an already-decoded entry object.
    fn apply(&mut self, value: &Value) -> bool {
        let id = str_field(value, "id");
        let timestamp = str_field(value, "timestamp");
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                self.header = Header {
                    version: value.get("version").and_then(Value::as_u64).unwrap_or(1) as u32,
                    id: str_field(value, "id"),
                    cwd: str_field(value, "cwd"),
                    timestamp,
                    name: self.header.name.clone(),
                    parent_session: opt_str(value, "parentSession"),
                };
                self.has_header = true;
                true
            }
            Some("message") => self.apply_message(value, &id, &timestamp),
            Some("model_change") => {
                let provider = str_field(value, "provider");
                let model = str_field(value, "modelId");
                let display = if provider.is_empty() {
                    model.clone()
                } else {
                    format!("{provider}/{model}")
                };
                self.model = Some(display.clone());
                self.track_model(display);
                self.items.push(Item {
                    id,
                    timestamp,
                    kind: ItemKind::Note {
                        title: "model".to_string(),
                        text: self.model.clone().unwrap_or_default(),
                    },
                });
                true
            }
            Some("thinking_level_change") => {
                let level = str_field(value, "thinkingLevel");
                self.thinking_level = Some(level.clone());
                self.items.push(Item {
                    id,
                    timestamp,
                    kind: ItemKind::Note {
                        title: "thinking".to_string(),
                        text: level,
                    },
                });
                true
            }
            Some("usage") => {
                if let Some(usage) = value.get("usage") {
                    self.usage.add(usage);
                    return true;
                }
                false
            }
            Some("compaction") => {
                if let Some(usage) = value.get("usage") {
                    self.usage.add(usage);
                }
                self.items.push(Item {
                    id,
                    timestamp,
                    kind: ItemKind::Note {
                        title: "compaction".to_string(),
                        text: str_field(value, "summary"),
                    },
                });
                true
            }
            Some("branch_summary") => {
                if let Some(usage) = value.get("usage") {
                    self.usage.add(usage);
                }
                self.items.push(Item {
                    id,
                    timestamp,
                    kind: ItemKind::Note {
                        title: "branch summary".to_string(),
                        text: str_field(value, "summary"),
                    },
                });
                true
            }
            Some("custom_message") => {
                if value.get("display").and_then(Value::as_bool) == Some(false) {
                    return false;
                }
                let title = str_field(value, "customType");
                self.items.push(Item {
                    id,
                    timestamp,
                    kind: ItemKind::Note {
                        title: if title.is_empty() {
                            "extension".to_string()
                        } else {
                            title
                        },
                        text: content_text(value.get("content")),
                    },
                });
                true
            }
            Some("session_info") => {
                let name = str_field(value, "name");
                self.header.name = Some(name.clone());
                self.items.push(Item {
                    id,
                    timestamp,
                    kind: ItemKind::Note {
                        title: "name".to_string(),
                        text: name,
                    },
                });
                true
            }
            // Structural bookkeeping with nothing to paint.
            Some("custom" | "label" | "context_edit") => false,
            Some(_) => {
                self.unknown += 1;
                false
            }
            None => {
                self.unknown += 1;
                false
            }
        }
    }

    /// Remember one model spelling for the picker, keeping first-seen order.
    fn track_model(&mut self, model: String) {
        if !model.is_empty() && !self.models.contains(&model) {
            self.models.push(model);
        }
    }

    /// Project one `message` entry into flat items.
    fn apply_message(&mut self, value: &Value, id: &str, timestamp: &str) -> bool {
        let Some(message) = value.get("message") else {
            return false;
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let mut changed = false;
        match role {
            "user" => {
                let text = content_text(message.get("content"));
                let images = content_images(message.get("content"));
                if !text.is_empty() || images > 0 {
                    self.push(id, timestamp, ItemKind::User { text, images });
                    changed = true;
                }
            }
            "assistant" => {
                let model = opt_str(message, "model");
                // The header shows the latest model as `provider/model`, the
                // same spelling a `model_change` uses.
                let display = match (opt_str(message, "provider"), model.clone()) {
                    (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
                    (_, model) => model,
                };
                if let Some(model) = &display {
                    self.model = Some(model.clone());
                    self.track_model(model.clone());
                }
                // Newer messages record the thinking level requested for the
                // response, which keeps the picker current without waiting
                // for an explicit change entry.
                if let Some(level) = opt_str(message, "thinkingLevel") {
                    self.thinking_level = Some(level);
                }
                if let Some(usage) = message.get("usage") {
                    self.usage.add(usage);
                    changed = true;
                }
                // Assistant content is normally an array of blocks; tolerate a
                // plain string so a hand-written or older line still renders.
                if let Some(Value::String(text)) = message.get("content")
                    && !text.is_empty()
                {
                    self.push(
                        id,
                        timestamp,
                        ItemKind::Assistant {
                            text: text.clone(),
                            model: model.clone(),
                        },
                    );
                    changed = true;
                }
                for block in blocks(message.get("content")) {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let text = str_field(block, "text");
                            if !text.is_empty() {
                                self.push(
                                    id,
                                    timestamp,
                                    ItemKind::Assistant {
                                        text,
                                        model: model.clone(),
                                    },
                                );
                                changed = true;
                            }
                        }
                        Some("thinking") => {
                            let text = str_field(block, "thinking");
                            if !text.is_empty() {
                                self.push(id, timestamp, ItemKind::Thinking { text });
                                changed = true;
                            }
                        }
                        Some("toolCall") => {
                            self.push(
                                id,
                                timestamp,
                                ItemKind::ToolCall {
                                    call_id: str_field(block, "id"),
                                    name: str_field(block, "name"),
                                    arguments: block
                                        .get("arguments")
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                    outcome: None,
                                },
                            );
                            changed = true;
                        }
                        Some("image") => {}
                        _ => {}
                    }
                }
            }
            "toolResult" => {
                if let Some(usage) = message.get("usage") {
                    self.usage.add(usage);
                }
                let call_id = str_field(message, "toolCallId");
                let text = content_text(message.get("content"));
                let images = content_images(message.get("content"));
                let is_error = message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // Attach the result to its call so the UI can show one row per
                // call; a result whose call is gone stays its own item.
                if !self.attach_outcome(
                    &call_id,
                    ToolOutcome {
                        text: text.clone(),
                        images,
                        is_error,
                    },
                ) {
                    self.push(
                        id,
                        timestamp,
                        ItemKind::ToolResult {
                            call_id,
                            name: str_field(message, "toolName"),
                            text,
                            images,
                            is_error,
                        },
                    );
                }
                changed = true;
            }
            "bashExecution" => {
                self.push(
                    id,
                    timestamp,
                    ItemKind::Bash {
                        command: str_field(message, "command"),
                        output: str_field(message, "output"),
                        exit_code: message.get("exitCode").and_then(Value::as_i64),
                        cancelled: message
                            .get("cancelled")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        truncated: message
                            .get("truncated")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    },
                );
                changed = true;
            }
            "branchSummary" => {
                self.push(
                    id,
                    timestamp,
                    ItemKind::Note {
                        title: "branch summary".to_string(),
                        text: str_field(message, "summary"),
                    },
                );
                changed = true;
            }
            "compactionSummary" => {
                self.push(
                    id,
                    timestamp,
                    ItemKind::Note {
                        title: "compaction".to_string(),
                        text: str_field(message, "summary"),
                    },
                );
                changed = true;
            }
            "custom" => {
                if message.get("display").and_then(Value::as_bool) == Some(false) {
                    return false;
                }
                self.push(
                    id,
                    timestamp,
                    ItemKind::Note {
                        title: str_field(message, "customType"),
                        text: content_text(message.get("content")),
                    },
                );
                changed = true;
            }
            // The system prompt and tool loadout are plumbing, not
            // conversation; they stay out of the transcript.
            "system" => {}
            _ => {
                self.unknown += 1;
            }
        }
        changed
    }

    /// Append one item under an entry id and timestamp.
    fn push(&mut self, id: &str, timestamp: &str, kind: ItemKind) {
        self.items.push(Item {
            id: id.to_string(),
            timestamp: timestamp.to_string(),
            kind,
        });
    }

    /// Attach a tool result to its pending call, newest first.
    ///
    /// Returns whether a call was found; results are usually written directly
    /// after their call, so the search starts at the end.
    fn attach_outcome(&mut self, call_id: &str, outcome: ToolOutcome) -> bool {
        if call_id.is_empty() {
            return false;
        }
        for item in self.items.iter_mut().rev() {
            if let ItemKind::ToolCall {
                call_id: id,
                outcome: slot,
                ..
            } = &mut item.kind
                && id.as_str() == call_id
                && slot.is_none()
            {
                *slot = Some(outcome);
                return true;
            }
        }
        false
    }
}

/// The content blocks of a message, or an empty slice for a plain string.
fn blocks(content: Option<&Value>) -> &[Value] {
    content.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Flatten a message body into text, joining text blocks with blank lines.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .map(|block| str_field(block, "text"))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Count image blocks in a message body.
fn content_images(content: Option<&Value>) -> usize {
    blocks(content)
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .count()
}

/// A string field, or empty when missing.
fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// An optional string field.
fn opt_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// A number field as `u64`, accepting floats that are whole.
fn field_u64(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// A number field as `f64`.
fn field_f64(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}
