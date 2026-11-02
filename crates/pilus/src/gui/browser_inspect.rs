//! What `pilus ctl browser ...` knows about one Browser page, and the script
//! that lets it look and act.
//!
//! Each page tab owns an [`Inspect`], shared with the handlers of its webview.
//! `browser_inspect.js` runs at document start in every tab an agent may use
//! (one it opened, or one the person lent it). It streams console output and
//! requests over `window.ipc` into the rings here, and answers the calls the
//! window sends it. So a read of the
//! console or the request log never touches the page, survives a navigation,
//! and costs nothing while nobody asks. A call that needs the page (a click, a
//! snapshot, a script) is `evaluate_script` plus an id: the page answers over
//! IPC and [`Inspect::on_message`] hands the answer to the waiting
//! [`Replier`]. No timer runs in the window; a wait is a timer in the page,
//! for as long as that call lasts.
//!
//! Page code is not trusted. Whatever arrives is parsed field by field, capped
//! and stored as data; it never decides what the window does beyond answering
//! the call that is waiting for it. A tab the person keeps gets no script
//! (see [security](../../../docs/agents/security.md#the-control-socket)).
//!
//! The same `window.ipc` carries the annotate picker's messages
//! (`browser_notes`), which this queues for the window to take.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use super::browser_notes::{NoteEvent, Pick};
use crate::ctl::{PageOp, Replier, Reply};

/// Injected into a tab only while an agent may use it.
pub(super) const INIT_SCRIPT: &str = include_str!("browser_inspect.js");

/// Entries kept per ring. The oldest go first.
const KEEP: usize = 500;
/// How many entries a read shows when it is not given a limit.
const DEFAULT_SHOWN: usize = 100;
/// The longest message taken from a page. A snapshot is the largest.
const MAX_MESSAGE: usize = 1024 * 1024;
/// Calls waiting on a page at once. A page that never answers cannot grow
/// the table without bound; the oldest call is dropped, which tells its
/// client.
const MAX_CALLS: usize = 32;

/// Which of the two rings a read is about.
#[derive(Clone, Copy)]
enum Ring {
    Net,
    Console,
}

/// A page tab's record of what happened in it and who is waiting to hear.
#[derive(Default)]
pub(super) struct Inspect {
    state: Mutex<State>,
    /// A real click or keypress reached the page since this was last taken.
    user_input: AtomicBool,
}

#[derive(Default)]
struct State {
    net: VecDeque<Value>,
    console: VecDeque<Value>,
    /// Calls the page has yet to answer, by id.
    calls: HashMap<u64, Replier>,
    next_id: u64,
    /// Clients waiting for the page to finish loading, with the tab they
    /// asked about.
    loads: Vec<(Replier, Option<usize>)>,
    loading: bool,
    title: String,
    /// What the annotate picker reported since the window last looked.
    notes: Vec<NoteEvent>,
}

impl Inspect {
    fn state(&self) -> MutexGuard<'_, State> {
        // The data is plain; a panic elsewhere must not take the page's
        // handlers down with it.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a load is in flight.
    pub(super) fn loading(&self) -> bool {
        self.state().loading
    }

    /// The title the page last reported.
    pub(super) fn title(&self) -> String {
        self.state().title.clone()
    }

    /// What the annotate picker reported, oldest first.
    pub(super) fn take_note_events(&self) -> Vec<NoteEvent> {
        std::mem::take(&mut self.state().notes)
    }

    pub(super) fn set_title(&self, title: &str) {
        self.state().title = title.to_string();
    }

    /// Whether the person touched the page since the last call. The page's
    /// script reports only input the browser itself generated, never the
    /// agent's synthetic events.
    pub(super) fn take_user_input(&self) -> bool {
        self.user_input.swap(false, Ordering::Relaxed)
    }

    /// End every call waiting on the page with `why`.
    pub(super) fn fail_calls(&self, why: &str) {
        let calls: Vec<Replier> = self
            .state()
            .calls
            .drain()
            .map(|(_, replier)| replier)
            .collect();
        for replier in calls {
            replier.send(Reply::err(why));
        }
    }

    /// A load began, from the page or from us. Calls in flight belonged to the
    /// document that is going away and will never be answered.
    pub(super) fn load_started(&self) {
        let mut state = self.state();
        state.loading = true;
        let calls: Vec<Replier> = state.calls.drain().map(|(_, replier)| replier).collect();
        drop(state);
        for replier in calls {
            replier.send(Reply::err(
                "the page navigated before the script finished; look at it again",
            ));
        }
    }

    /// The page finished loading `url`: answer whoever waited.
    pub(super) fn load_finished(&self, url: &str) {
        let mut state = self.state();
        state.loading = false;
        let waiting = std::mem::take(&mut state.loads);
        let title = state.title.clone();
        drop(state);
        for (replier, tab) in waiting {
            replier.send(Reply::data(summary(tab, url, &title, false)));
        }
    }

    /// Answer `replier` when the page next finishes loading.
    pub(super) fn answer_on_load(&self, replier: Replier, tab: Option<usize>) {
        let mut state = self.state();
        state.loading = true;
        state.loads.push((replier, tab));
    }

    /// Take a call's place in the table and get its id. The oldest call goes
    /// if the table is full.
    pub(super) fn begin_call(&self, replier: Replier) -> u64 {
        let mut state = self.state();
        state.next_id += 1;
        let id = state.next_id;
        if state.calls.len() >= MAX_CALLS
            && let Some(oldest) = state.calls.keys().min().copied()
        {
            state.calls.remove(&oldest);
        }
        state.calls.insert(id, replier);
        id
    }

    /// Answer call `id` with an error, if it is still waiting.
    pub(super) fn fail_call(&self, id: u64, why: &str) {
        let replier = self.state().calls.remove(&id);
        if let Some(replier) = replier {
            replier.send(Reply::err(why));
        }
    }

    /// The tab is closed: nothing will answer.
    pub(super) fn reset(&self) {
        let mut state = self.state();
        let calls: Vec<Replier> = state.calls.drain().map(|(_, replier)| replier).collect();
        let loads = std::mem::take(&mut state.loads);
        *state = State::default();
        drop(state);
        for replier in calls {
            replier.send(Reply::err("the tab was closed"));
        }
        for (replier, _) in loads {
            replier.send(Reply::err("the tab was closed"));
        }
    }

    /// Fold in a message the page posted over `window.ipc`.
    pub(super) fn on_message(&self, body: &str) {
        if body.len() > MAX_MESSAGE {
            return;
        }
        let Ok(message) = serde_json::from_str::<Value>(body) else {
            return;
        };
        match message.get("t").and_then(Value::as_str) {
            Some("r") => self.answer_call(&message),
            Some("user") => self.user_input.store(true, Ordering::Relaxed),
            Some("pick") => {
                if let Some(pick) = Pick::parse(&message) {
                    let mut state = self.state();
                    // A person picks one element at a time; a page that floods
                    // the queue does not grow it.
                    if state.notes.len() < 8 {
                        state.notes.push(NoteEvent::Picked(pick));
                    }
                }
            }
            Some("pick_end") => {
                let mut state = self.state();
                if state.notes.len() < 8 {
                    state.notes.push(NoteEvent::Ended);
                }
            }
            Some("log") => {
                let level = message
                    .get("level")
                    .and_then(Value::as_str)
                    .filter(|level| matches!(*level, "log" | "info" | "warn" | "error" | "debug"))
                    .unwrap_or("log");
                let entry = json!({
                    "at": message.get("at").and_then(Value::as_u64).unwrap_or_default(),
                    "level": level,
                    "text": text_field(&message, "text", 2000),
                });
                push_ring(&mut self.state().console, entry);
            }
            Some("net") => {
                let mut entry = json!({
                    "at": message.get("at").and_then(Value::as_u64).unwrap_or_default(),
                    "kind": text_field(&message, "kind", 24),
                    "method": text_field(&message, "method", 12),
                    "url": text_field(&message, "url", 1000),
                    "status": message
                        .get("status")
                        .and_then(Value::as_u64)
                        .filter(|status| *status < 1000),
                    "ms": message.get("ms").and_then(Value::as_u64),
                });
                for (name, max) in [("type", 100), ("error", 200)] {
                    if let Some(text) = message.get(name).and_then(Value::as_str) {
                        entry[name] = json!(clip(text, max));
                    }
                }
                if let Some(size) = message.get("size").and_then(Value::as_u64) {
                    entry["size"] = json!(size);
                }
                push_ring(&mut self.state().net, entry);
            }
            _ => {}
        }
    }

    fn answer_call(&self, message: &Value) {
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(replier) = self.state().calls.remove(&id) else {
            return;
        };
        let reply = if message.get("ok").and_then(Value::as_bool) == Some(true) {
            Reply::data(message.get("v").cloned().unwrap_or(Value::Null))
        } else {
            Reply::err(clip(
                message
                    .get("e")
                    .and_then(Value::as_str)
                    .unwrap_or("the script failed"),
                500,
            ))
        };
        replier.send(reply);
    }

    /// The network log or the console, as `network` and `console` show it.
    pub(super) fn read(&self, op: &PageOp) -> Option<Reply> {
        let (ring, clear, filter, limit) = match op {
            PageOp::Network {
                clear,
                filter,
                limit,
            } => (Ring::Net, *clear, filter, limit),
            PageOp::Console {
                clear,
                filter,
                limit,
            } => (Ring::Console, *clear, filter, limit),
            _ => return None,
        };
        let mut state = self.state();
        let entries = match ring {
            Ring::Net => &mut state.net,
            Ring::Console => &mut state.console,
        };
        let key = match ring {
            Ring::Net => "url",
            Ring::Console => "text",
        };
        let needle = filter.as_deref().map(str::to_lowercase);
        let matching: Vec<&Value> = entries
            .iter()
            .filter(|entry| {
                needle.as_deref().is_none_or(|needle| {
                    entry
                        .get(key)
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.to_lowercase().contains(needle))
                })
            })
            .collect();
        let total = matching.len();
        let shown = limit
            .map_or(DEFAULT_SHOWN, |limit| limit as usize)
            .clamp(1, KEEP);
        let tail: Vec<Value> = matching
            .into_iter()
            .skip(total.saturating_sub(shown))
            .cloned()
            .collect();
        if clear {
            entries.clear();
        }
        let name = match ring {
            Ring::Net => "requests",
            Ring::Console => "messages",
        };
        Some(Reply::data(json!({
            "total": total,
            "shown": tail.len(),
            name: tail,
        })))
    }
}

fn push_ring(ring: &mut VecDeque<Value>, entry: Value) {
    if ring.len() >= KEEP {
        ring.pop_front();
    }
    ring.push_back(entry);
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn text_field(message: &Value, name: &str, max: usize) -> String {
    clip(
        message
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default(),
        max,
    )
}

/// A page tab as `tabs`, `open` and `new` describe it.
pub(super) fn summary(tab: Option<usize>, url: &str, title: &str, loading: bool) -> Value {
    let mut value = json!({ "url": url, "title": title, "loading": loading });
    if let Some(tab) = tab {
        value["tab"] = json!(tab);
    }
    value
}

/// The script that makes the page do `op` and answer call `id`, or `None` for
/// an op the window answers itself.
pub(super) fn call_script(id: u64, op: &PageOp) -> Option<String> {
    let run = |name: &str, args: Value| format!("__pilus.run({id},{},{args})", json!(name));
    Some(match op {
        PageOp::Snapshot => run("snapshot", json!([])),
        PageOp::Html { selector } => run("html", json!([selector])),
        PageOp::Text { selector } => run("text", json!([selector])),
        PageOp::Click { selector } => run("click", json!([selector])),
        PageOp::Scroll {
            direction,
            pixels,
            selector,
            within,
        } => run("scroll", json!([direction, pixels, selector, within])),
        PageOp::Fill { selector, text } => run("fill", json!([selector, text])),
        PageOp::Press { key, selector } => run("press", json!([key, selector])),
        PageOp::Wait {
            selector,
            gone,
            timeout_ms,
        } => run("wait", json!([selector, gone, timeout_ms])),
        // The code is spliced into a function the window builds, so the page's
        // CSP, which stops the page's own `eval`, does not apply. The newlines
        // keep a trailing `//` comment from eating the closing bracket.
        PageOp::Eval { code, body: false } => {
            format!("__pilus.eval({id},async()=>(\n{code}\n))")
        }
        PageOp::Eval { code, body: true } => {
            format!("__pilus.eval({id},async()=>{{\n{code}\n}})")
        }
        PageOp::Network { .. }
        | PageOp::Console { .. }
        | PageOp::Back
        | PageOp::Forward
        | PageOp::Reload => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Replier, std::sync::mpsc::Receiver<Reply>) {
        Replier::pair()
    }

    #[test]
    fn a_page_answer_reaches_the_call_that_asked() {
        let inspect = Inspect::default();
        let (replier, answer) = pair();
        let id = inspect.begin_call(replier);
        // A different id is not this call's.
        inspect.on_message(&json!({"t":"r","id":id + 1,"ok":true,"v":1}).to_string());
        assert!(answer.try_recv().is_err());
        inspect.on_message(&json!({"t":"r","id":id,"ok":true,"v":{"a":1}}).to_string());
        assert_eq!(
            answer.try_recv().expect("answered").data,
            Some(json!({"a":1}))
        );
        // Answered once only.
        inspect.on_message(&json!({"t":"r","id":id,"ok":true,"v":2}).to_string());
        assert!(answer.try_recv().is_err());
    }

    #[test]
    fn a_page_failure_is_an_error_reply() {
        let inspect = Inspect::default();
        let (replier, answer) = pair();
        let id = inspect.begin_call(replier);
        inspect
            .on_message(&json!({"t":"r","id":id,"ok":false,"e":"nothing matches #x"}).to_string());
        let reply = answer.try_recv().expect("answered");
        assert!(!reply.ok);
        assert_eq!(reply.error.as_deref(), Some("nothing matches #x"));
    }

    #[test]
    fn the_pickers_messages_queue_for_the_window() {
        let inspect = Inspect::default();
        inspect.on_message(r##"{"t":"pick","selector":"#go","label":"button#go"}"##);
        inspect.on_message(r#"{"t":"pick","selector":""}"#);
        inspect.on_message(r#"{"t":"pick_end"}"#);
        let events = inspect.take_note_events();
        assert_eq!(events.len(), 2, "a pick with nothing to name it is dropped");
        assert!(matches!(&events[0], NoteEvent::Picked(pick) if pick.selector == "#go"));
        assert_eq!(events[1], NoteEvent::Ended);
        assert!(inspect.take_note_events().is_empty(), "taken");
        for _ in 0..50 {
            inspect.on_message(r#"{"t":"pick_end"}"#);
        }
        assert_eq!(inspect.take_note_events().len(), 8, "a flood is capped");
    }

    #[test]
    fn the_persons_input_is_noted_once() {
        let inspect = Inspect::default();
        assert!(!inspect.take_user_input());
        inspect.on_message(r#"{"t":"user"}"#);
        assert!(inspect.take_user_input());
        assert!(!inspect.take_user_input(), "taken");
    }

    #[test]
    fn failing_the_calls_tells_each_waiter() {
        let inspect = Inspect::default();
        let (first, first_answer) = pair();
        let (second, second_answer) = pair();
        inspect.begin_call(first);
        inspect.begin_call(second);
        inspect.fail_calls("the person took over");
        for answer in [first_answer, second_answer] {
            let reply = answer.try_recv().expect("told");
            assert_eq!(reply.error.as_deref(), Some("the person took over"));
        }
    }

    #[test]
    fn a_navigation_ends_the_calls_in_flight() {
        let inspect = Inspect::default();
        let (replier, answer) = pair();
        inspect.begin_call(replier);
        inspect.load_started();
        assert!(!answer.try_recv().expect("told").ok);
        assert!(inspect.loading());
    }

    #[test]
    fn loads_answer_the_waiters_with_the_page() {
        let inspect = Inspect::default();
        let (replier, answer) = pair();
        inspect.answer_on_load(replier, Some(2));
        assert!(inspect.loading(), "loading from the moment we ask");
        inspect.set_title("Todo");
        inspect.load_finished("http://localhost:8787/");
        let data = answer.try_recv().expect("answered").data.expect("data");
        assert_eq!(data["tab"], 2);
        assert_eq!(data["url"], "http://localhost:8787/");
        assert_eq!(data["title"], "Todo");
        assert!(!inspect.loading());
    }

    #[test]
    fn closing_tells_everyone_waiting() {
        let inspect = Inspect::default();
        let (call, call_answer) = pair();
        let (load, load_answer) = pair();
        inspect.begin_call(call);
        inspect.answer_on_load(load, None);
        inspect.reset();
        assert!(!call_answer.try_recv().expect("told").ok);
        assert!(!load_answer.try_recv().expect("told").ok);
    }

    #[test]
    fn a_full_table_drops_the_oldest_call_and_says_so() {
        let inspect = Inspect::default();
        let (first, first_answer) = pair();
        inspect.begin_call(first);
        let keep: Vec<_> = (0..MAX_CALLS).map(|_| pair()).collect();
        for (replier, _) in &keep {
            inspect.begin_call(replier.clone());
        }
        // The first call's last handle was in the table, and it is gone.
        assert!(first_answer.try_recv().is_err());
        assert!(matches!(
            first_answer.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn the_rings_keep_the_newest_and_ignore_nonsense() {
        let inspect = Inspect::default();
        for n in 0..(KEEP + 20) {
            inspect.on_message(
                &json!({"t":"net","at":n,"method":"GET","url":format!("http://localhost/{n}"),"status":200,"ms":3})
                    .to_string(),
            );
        }
        inspect.on_message("not json");
        inspect.on_message(r#"{"t":"nope"}"#);
        inspect.on_message(&"x".repeat(MAX_MESSAGE + 1));
        let all = PageOp::Network {
            clear: false,
            filter: None,
            limit: Some(1000),
        };
        let data = inspect.read(&all).expect("a read").data.expect("data");
        assert_eq!(data["total"], KEEP);
        assert_eq!(data["requests"][0]["url"], "http://localhost/20");
    }

    #[test]
    fn a_read_filters_limits_and_clears() {
        let inspect = Inspect::default();
        for (level, text) in [("log", "hello"), ("error", "Boom at /api"), ("weird", "x")] {
            inspect.on_message(&json!({"t":"log","at":1,"level":level,"text":text}).to_string());
        }
        let read = |filter: Option<&str>, limit, clear| {
            inspect
                .read(&PageOp::Console {
                    clear,
                    filter: filter.map(str::to_string),
                    limit,
                })
                .expect("a read")
                .data
                .expect("data")
        };
        let found = read(Some("BOOM"), None, false);
        assert_eq!(found["total"], 1);
        assert_eq!(found["messages"][0]["level"], "error");
        // An unknown level is read as plain log, not trusted as given.
        let last = read(None, Some(1), false);
        assert_eq!(last["total"], 3);
        assert_eq!(last["messages"][0]["level"], "log");
        assert_eq!(read(None, None, true)["total"], 3);
        assert_eq!(read(None, None, false)["total"], 0);
    }

    #[test]
    fn requests_keep_only_the_fields_we_read() {
        let inspect = Inspect::default();
        inspect.on_message(
            &json!({"t":"net","at":9,"kind":"fetch","method":"POST","url":"http://localhost/api",
                "status":500,"ms":12,"type":"application/json","evil":{"deep":1}})
            .to_string(),
        );
        let data = inspect
            .read(&PageOp::Network {
                clear: false,
                filter: Some("/API".into()),
                limit: None,
            })
            .expect("a read")
            .data
            .expect("data");
        let entry = &data["requests"][0];
        assert_eq!(entry["status"], 500);
        assert_eq!(entry["type"], "application/json");
        assert!(entry.get("evil").is_none());
    }

    #[test]
    fn scripts_carry_their_arguments_as_data() {
        let script = call_script(
            7,
            &PageOp::Fill {
                selector: "#q\"); alert(1); (\"".to_string(),
                text: "a\nb".to_string(),
            },
        )
        .expect("a script");
        assert!(script.starts_with(r#"__pilus.run(7,"fill",["#));
        // The quote that would end the argument is escaped.
        assert!(script.contains(r#"#q\"); alert(1); (\""#));
        assert!(!script.contains('\n'), "newlines are escaped too");
        let eval = call_script(
            8,
            &PageOp::Eval {
                code: "1 + 1 // note".to_string(),
                body: false,
            },
        )
        .expect("a script");
        assert!(eval.ends_with("// note\n))"));
        assert!(
            call_script(
                1,
                &PageOp::Network {
                    clear: false,
                    filter: None,
                    limit: None
                }
            )
            .is_none()
        );
    }
}
