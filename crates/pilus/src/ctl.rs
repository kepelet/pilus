//! `pilus ctl`: drive a running window from a script or an agent.
//!
//! The window listens on `~/.pilus/pilus.sock`. A client connects, sends one
//! line of JSON (a [`Request`]), reads one line back (a [`Reply`]) and
//! disconnects. [`serve`] runs on its own thread, parses the line and hands
//! the request to the GUI over a channel; the GUI applies it on its own thread
//! (`gui::ctl`) and answers through the call's reply slot. This module has no
//! GUI types so the wire format and the client can be tested, and the `ctl`
//! subcommand works in a build without the window.
//!
//! Some verbs finish later than the GUI thread's turn: a page loading, a script
//! answering from inside the page, an SSH forward coming up. Those hand their
//! [`Replier`] to whoever will know, and the connection's own thread waits for
//! it, so each client gets its own thread (capped) and a slow one holds no one
//! else up.
//!
//! The listener blocks in `accept`, so an idle window pays nothing for it. See
//! [idle cost](../../../docs/agents/idle-cost.md). The rules for the socket
//! itself are in [security](../../../docs/agents/security.md#the-control-socket).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The longest request line read. A request is a verb and a URL, or a short
/// script.
const MAX_REQUEST: u64 = 16 * 1024;
/// The longest reply a client reads. A page snapshot or a request log is far
/// bigger than a request; the window caps what it sends well below this.
const MAX_REPLY: u64 = 2 * 1024 * 1024;
/// Clients served at once. A client that waits on a page holds its thread for
/// as long as it waits, so the count is bounded.
const MAX_CONNECTIONS: usize = 16;
/// How long a client may take to send its line, so a stalled one cannot hold
/// the accept loop.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the server waits for the window to answer. Most verbs answer at
/// once; a page load, a forward or a `wait` can take most of this.
const APPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the server takes to write an answer to a client that reads slowly.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the client waits for the whole exchange: the server's wait and
/// a little over.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(35);

/// Where the window listens.
pub fn socket_path() -> PathBuf {
    pilus_client::pilus_config_dir().join("pilus.sock")
}

/// Where a tunnel from another Mac's window ends on this machine
/// (`pilus_client::REMOTE_SOCKET`), for an agent that runs here.
pub fn tunnel_socket_path() -> PathBuf {
    pilus_client::pilus_config_dir().join(pilus_client::REMOTE_SOCKET)
}

/// The environment variable that names the socket a client should use.
pub const SOCKET_ENV: &str = "PILUS_CTL_SOCKET";

/// The socket `pilus ctl` talks to: `$PILUS_CTL_SOCKET` if set; else a window
/// on this machine, if one answers; else the tunnel from the Mac the agent's
/// window is on. With neither, the window's own path, so the error names it.
pub fn client_socket() -> PathBuf {
    if let Some(path) = std::env::var_os(SOCKET_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    pick_socket(socket_path(), tunnel_socket_path(), |path| {
        UnixStream::connect(path).is_ok()
    })
}

/// `own` if a window answers on it, else `tunnel` if one does, else `own`.
fn pick_socket(own: PathBuf, tunnel: PathBuf, answers: impl Fn(&Path) -> bool) -> PathBuf {
    if answers(&own) {
        own
    } else if answers(&tunnel) {
        tunnel
    } else {
        own
    }
}

/// A column of the right sidebar, by the name its tab shows.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tab {
    #[serde(alias = "diff")]
    Changes,
    #[serde(alias = "git")]
    History,
    Files,
    Ports,
    Browser,
    Sipil,
}

impl Tab {
    /// Every name a tab answers to, for the usage text.
    pub const NAMES: &'static str = "changes|history|files|ports|browser|sipil";

    fn parse(text: &str) -> Result<Self> {
        serde_json::from_value(Value::String(text.to_ascii_lowercase()))
            .map_err(|_| anyhow!("unknown sidebar tab {text:?}; use {}", Self::NAMES))
    }
}

/// One thing a client can ask the window to do. The verb is the `cmd` field.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Answer without touching the window; tells a client the window is up.
    Ping,
    /// Report what the focused window is showing.
    State,
    /// Open the right sidebar, on `tab` when one is named.
    SidebarOpen { tab: Option<Tab> },
    /// Close the right sidebar.
    SidebarClose,
    /// Open the right sidebar if it is closed, close it if it is open.
    SidebarToggle,
    /// Load `url` in a Browser tab of the agent's, opening the sidebar on it:
    /// the 1-based `tab` if one is named, else the selected tab when the agent
    /// opened it, else a new tab. Answers once the page has loaded, unless
    /// `wait` is off.
    BrowserOpen {
        url: String,
        #[serde(default = "yes")]
        wait: bool,
        #[serde(default)]
        tab: Option<u16>,
    },
    /// Go to the workspace tab at the 1-based `index`.
    TabFocus { index: u16 },
    /// Kept so an agent written against the old Browser grant still runs. No
    /// one has to say yes any more: an agent uses the tabs it opens and no
    /// others, so this answers at once and changes nothing. `reason` is
    /// ignored.
    BrowserControl { reason: Option<String> },
    /// Ask the person to lend the agent the 1-based `tab`, which they opened.
    /// Answers when they decide; the agent may use the tab until it releases
    /// it or they take it back. `reason` is shown to them.
    #[serde(rename = "browser_request")]
    BrowserBorrow { tab: u16, reason: Option<String> },
    /// Let go of the agent's tabs, so the person can use them: the 1-based
    /// `tab`, else every tab the agent holds.
    BrowserRelease {
        #[serde(default)]
        tab: Option<u16>,
    },
    /// List the Browser's page tabs, and who opened each.
    BrowserTabs,
    /// Open a page tab of the agent's own, on `url` when one is given.
    BrowserNew {
        url: Option<String>,
        #[serde(default = "yes")]
        wait: bool,
    },
    /// Close one of the agent's page tabs: the 1-based `tab`, else the
    /// selected one.
    BrowserClose { tab: Option<u16> },
    /// Select the agent's page tab at the 1-based index `tab`.
    BrowserFocus { tab: u16 },
    /// Look at or act on a page of the agent's: the 1-based `tab`, else the
    /// selected one.
    BrowserPage {
        tab: Option<u16>,
        #[serde(flatten)]
        op: PageOp,
    },
    /// List what the active machine is listening on, and what is forwarded.
    PortsList,
    /// Forward a port the active machine is listening on to this Mac.
    PortsForward { port: u16 },
    /// End the forward of a remote port.
    PortsStop { port: u16 },
}

fn yes() -> bool {
    true
}

/// Which way `browser scroll` goes. `Top` and `Bottom` jump to the end.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
    Top,
    Bottom,
}

impl ScrollDirection {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "top" => Self::Top,
            "bottom" => Self::Bottom,
            _ => return None,
        })
    }
}

/// What to do to one Browser page. The verb is the `op` field.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PageOp {
    /// An outline of the page: headings, text and the controls, each control
    /// with an `@N` handle the other ops accept in place of a selector.
    Snapshot,
    /// The markup of `selector`, else the whole document.
    Html {
        selector: Option<String>,
    },
    /// The rendered text of `selector`, else the whole body.
    Text {
        selector: Option<String>,
    },
    /// Run JavaScript in the page and return its value. `code` is an
    /// expression, or with `body` the body of an async function.
    Eval {
        code: String,
        #[serde(default)]
        body: bool,
    },
    Click {
        selector: String,
    },
    /// Scroll the page (or one scrollable element, `within`) a step in
    /// `direction`, by `pixels` else most of a screen, or with no direction
    /// bring `selector` into view. The answer says where the page now is, so
    /// an agent knows when it has reached the end.
    Scroll {
        direction: Option<ScrollDirection>,
        pixels: Option<u32>,
        selector: Option<String>,
        within: Option<String>,
    },
    /// Press on `from`, move to the element `to` or by `by` (`[dx, dy]` pixels
    /// from `from`'s centre), and let go. Drives what a person would do with a
    /// slider, a sortable list or a draggable card.
    Drag {
        from: String,
        to: Option<String>,
        by: Option<[i32; 2]>,
    },
    /// Replace the value of a field, as typing would.
    Fill {
        selector: String,
        text: String,
    },
    /// Press a key on `selector`, else on the focused element.
    Press {
        key: String,
        selector: Option<String>,
    },
    /// Wait for `selector` to show up, or with `gone` to go away.
    Wait {
        selector: String,
        #[serde(default)]
        gone: bool,
        timeout_ms: Option<u32>,
    },
    /// The requests the page made, newest last.
    Network {
        #[serde(default)]
        clear: bool,
        filter: Option<String>,
        limit: Option<u32>,
    },
    /// The page's console output and uncaught errors, newest last.
    Console {
        #[serde(default)]
        clear: bool,
        filter: Option<String>,
        limit: Option<u32>,
    },
    Back,
    Forward,
    /// Reload, and answer once the page has loaded again.
    Reload,
}

/// The answer to a [`Request`].
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Reply {
    pub fn ok() -> Self {
        Self {
            ok: true,
            ..Self::default()
        }
    }

    pub fn data(data: Value) -> Self {
        Self {
            ok: true,
            error: None,
            data: Some(data),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            data: None,
        }
    }
}

/// Where a request's answer goes. It clones, so a verb that finishes later
/// can hand it to a worker thread or a webview callback; the first answer
/// wins. When every clone is dropped unanswered the waiting client is told at
/// once rather than left to time out.
#[derive(Clone)]
pub struct Replier(Arc<Mutex<Option<SyncSender<Reply>>>>);

impl Replier {
    /// A slot and the end that waits on it.
    pub fn pair() -> (Self, std::sync::mpsc::Receiver<Reply>) {
        let (sender, answer) = sync_channel(1);
        (Self(Arc::new(Mutex::new(Some(sender)))), answer)
    }

    /// Answer the client. A client that has gone away, or one already
    /// answered, is not an error.
    pub fn send(&self, reply: Reply) {
        let sender = self.0.lock().ok().and_then(|mut slot| slot.take());
        if let Some(sender) = sender {
            let _ = sender.send(reply);
        }
    }
}

/// A request waiting for the window, with the slot its answer goes in.
pub struct Call {
    pub request: Request,
    reply: Replier,
}

impl Call {
    /// A handle to answer with after the handler has returned.
    pub fn replier(&self) -> Replier {
        self.reply.clone()
    }

    /// Answer the client now.
    pub fn respond(self, reply: Reply) {
        self.reply.send(reply);
    }
}

/// Parse the words after `pilus ctl`.
pub fn parse_args(args: &[String]) -> Result<Request> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["ping"] => Ok(Request::Ping),
        ["state"] => Ok(Request::State),
        ["sidebar", "open"] => Ok(Request::SidebarOpen { tab: None }),
        ["sidebar", "open", tab] => Ok(Request::SidebarOpen {
            tab: Some(Tab::parse(tab)?),
        }),
        ["sidebar", "close"] => Ok(Request::SidebarClose),
        ["sidebar", "toggle"] => Ok(Request::SidebarToggle),
        ["tab", "focus", index] => Ok(Request::TabFocus {
            index: index
                .parse::<u16>()
                .ok()
                .filter(|index| *index >= 1)
                .ok_or_else(|| anyhow!("tab focus needs a number from 1, not {index:?}"))?,
        }),
        ["ports", "list"] => Ok(Request::PortsList),
        ["ports", "forward", port] => Ok(Request::PortsForward {
            port: parse_port(port)?,
        }),
        ["ports", "stop", port] => Ok(Request::PortsStop {
            port: parse_port(port)?,
        }),
        ["browser", rest @ ..] => parse_browser(rest),
        [] => bail!("pilus ctl needs a command\n\n{USAGE}"),
        _ => bail!("unrecognized command: {}\n\n{USAGE}", words.join(" ")),
    }
}

fn parse_port(word: &str) -> Result<u16> {
    word.parse::<u16>()
        .ok()
        .filter(|port| *port >= 1)
        .ok_or_else(|| anyhow!("a port is a number from 1 to 65535, not {word:?}"))
}

/// A 1-based tab number.
fn parse_tab(word: &str) -> Result<u16> {
    word.parse::<u16>()
        .ok()
        .filter(|tab| *tab >= 1)
        .ok_or_else(|| anyhow!("a tab is a number from 1, not {word:?}"))
}

/// The flags of a `browser` verb and the words left over.
#[derive(Default)]
struct Flags {
    tab: Option<u16>,
    clear: bool,
    gone: bool,
    body: bool,
    no_wait: bool,
    timeout_ms: Option<u32>,
    filter: Option<String>,
    limit: Option<u32>,
    within: Option<String>,
    by: Option<[i32; 2]>,
    positional: Vec<String>,
}

impl Flags {
    /// Split `words` into flags and positional words. A verb that takes free
    /// text (`eval`, `fill`) stops reading flags at its first positional word,
    /// so the text may hold anything, `--` words included.
    fn take(words: &[&str], free_text: bool) -> Result<Self> {
        let mut flags = Self::default();
        let mut words = words.iter().copied();
        while let Some(word) = words.next() {
            if free_text && !flags.positional.is_empty() {
                flags.positional.push(word.to_string());
                continue;
            }
            let mut value =
                |name: &str| words.next().ok_or_else(|| anyhow!("{name} needs a value"));
            match word {
                "--tab" => flags.tab = Some(parse_tab(value("--tab")?)?),
                "--clear" => flags.clear = true,
                "--gone" => flags.gone = true,
                "--body" => flags.body = true,
                "--no-wait" => flags.no_wait = true,
                "--timeout" => {
                    let text = value("--timeout")?;
                    flags.timeout_ms = Some(text.parse().map_err(|_| {
                        anyhow!("--timeout is a number of milliseconds, not {text:?}")
                    })?);
                }
                "--filter" => flags.filter = Some(value("--filter")?.to_string()),
                "--by" => {
                    let text = value("--by")?;
                    flags.by = Some(parse_offset(text)?);
                }
                "--in" => flags.within = Some(value("--in")?.to_string()),
                "--limit" => {
                    let text = value("--limit")?;
                    flags.limit = Some(
                        text.parse()
                            .map_err(|_| anyhow!("--limit is a number, not {text:?}"))?,
                    );
                }
                other if other.starts_with("--") => bail!("unknown flag {other}\n\n{USAGE}"),
                other => flags.positional.push(other.to_string()),
            }
        }
        Ok(flags)
    }

    /// The one positional word the verb needs.
    fn one(&self, verb: &str, what: &str) -> Result<String> {
        match self.positional.as_slice() {
            [only] => Ok(only.clone()),
            _ => bail!("browser {verb} needs {what}\n\n{USAGE}"),
        }
    }

    /// At most one positional word.
    fn maybe_one(&self, verb: &str) -> Result<Option<String>> {
        match self.positional.as_slice() {
            [] => Ok(None),
            [only] => Ok(Some(only.clone())),
            _ => bail!("browser {verb} takes at most one argument\n\n{USAGE}"),
        }
    }

    fn page(&self, op: PageOp) -> Request {
        Request::BrowserPage { tab: self.tab, op }
    }
}

fn parse_browser(words: &[&str]) -> Result<Request> {
    let Some((verb, rest)) = words.split_first() else {
        bail!("pilus ctl browser needs a command\n\n{USAGE}");
    };
    let flags = Flags::take(
        rest,
        matches!(*verb, "eval" | "fill" | "control" | "request"),
    )?;
    match *verb {
        "control" => Ok(Request::BrowserControl {
            // The rest of the line is the reason, so it needs no quoting.
            reason: Some(flags.positional.join(" ")).filter(|reason| !reason.is_empty()),
        }),
        "request" => {
            let [tab, reason @ ..] = flags.positional.as_slice() else {
                bail!("browser request needs a tab number and maybe a reason\n\n{USAGE}");
            };
            Ok(Request::BrowserBorrow {
                tab: parse_tab(tab)?,
                // The rest of the line is the reason, so it needs no quoting.
                reason: Some(reason.join(" ")).filter(|reason| !reason.is_empty()),
            })
        }
        "release" => Ok(Request::BrowserRelease {
            tab: flags
                .maybe_one("release")?
                .map(|word| parse_tab(&word))
                .transpose()?,
        }),
        "tabs" => Ok(Request::BrowserTabs),
        "open" => Ok(Request::BrowserOpen {
            url: flags.one("open", "a URL")?,
            wait: !flags.no_wait,
            tab: flags.tab,
        }),
        "new" => Ok(Request::BrowserNew {
            url: flags.maybe_one("new")?,
            wait: !flags.no_wait,
        }),
        "close" => Ok(Request::BrowserClose {
            tab: flags
                .maybe_one("close")?
                .map(|word| parse_tab(&word))
                .transpose()?,
        }),
        "focus" => Ok(Request::BrowserFocus {
            tab: parse_tab(&flags.one("focus", "a tab number")?)?,
        }),
        "snapshot" => Ok(flags.page(PageOp::Snapshot)),
        "html" => Ok(flags.page(PageOp::Html {
            selector: flags.maybe_one("html")?,
        })),
        "text" => Ok(flags.page(PageOp::Text {
            selector: flags.maybe_one("text")?,
        })),
        "eval" => {
            if flags.positional.is_empty() {
                bail!("browser eval needs some JavaScript\n\n{USAGE}");
            }
            Ok(flags.page(PageOp::Eval {
                code: flags.positional.join(" "),
                body: flags.body,
            }))
        }
        "click" => Ok(flags.page(PageOp::Click {
            selector: flags.one("click", "a selector or @N")?,
        })),
        "scroll" => parse_scroll(&flags),
        "drag" => parse_drag(&flags),
        "fill" => {
            // An empty text is fine (it clears the field), but it must be
            // given, so a forgotten argument is not a silent clear.
            let [selector, text @ ..] = flags.positional.as_slice() else {
                bail!("browser fill needs a selector and the text\n\n{USAGE}");
            };
            if text.is_empty() {
                bail!("browser fill needs the text; pass '' to clear the field\n\n{USAGE}");
            }
            Ok(flags.page(PageOp::Fill {
                selector: selector.clone(),
                text: text.join(" "),
            }))
        }
        "press" => match flags.positional.as_slice() {
            [key] => Ok(flags.page(PageOp::Press {
                key: key.clone(),
                selector: None,
            })),
            [key, selector] => Ok(flags.page(PageOp::Press {
                key: key.clone(),
                selector: Some(selector.clone()),
            })),
            _ => bail!("browser press needs a key and maybe a selector\n\n{USAGE}"),
        },
        "wait" => Ok(flags.page(PageOp::Wait {
            selector: flags.one("wait", "a selector or @N")?,
            gone: flags.gone,
            timeout_ms: flags.timeout_ms,
        })),
        "network" => Ok(flags.page(PageOp::Network {
            clear: flags.clear,
            filter: flags.filter.clone(),
            limit: flags.limit,
        })),
        "console" => Ok(flags.page(PageOp::Console {
            clear: flags.clear,
            filter: flags.filter.clone(),
            limit: flags.limit,
        })),
        "back" => Ok(flags.page(PageOp::Back)),
        "forward" => Ok(flags.page(PageOp::Forward)),
        "reload" => Ok(flags.page(PageOp::Reload)),
        other => bail!("unknown browser command {other:?}\n\n{USAGE}"),
    }
}

/// `DX,DY`, each a whole number of pixels, either may be negative.
fn parse_offset(text: &str) -> Result<[i32; 2]> {
    let bad = || anyhow!("--by is DX,DY in pixels, like 120,0 or -40,25, not {text:?}");
    let (dx, dy) = text.split_once(',').ok_or_else(bad)?;
    Ok([
        dx.trim().parse().map_err(|_| bad())?,
        dy.trim().parse().map_err(|_| bad())?,
    ])
}

/// `drag FROM TO` or `drag FROM --by DX,DY`.
fn parse_drag(flags: &Flags) -> Result<Request> {
    let op = match (flags.positional.as_slice(), flags.by) {
        ([from, to], None) => PageOp::Drag {
            from: from.clone(),
            to: Some(to.clone()),
            by: None,
        },
        ([from], Some(by)) => PageOp::Drag {
            from: from.clone(),
            to: None,
            by: Some(by),
        },
        _ => bail!(
            "browser drag takes FROM TO (selectors or @N), or FROM --by DX,DY\n\n{USAGE}"
        ),
    };
    Ok(flags.page(op))
}

/// `scroll down [PIXELS] [--in SELECTOR]`, or `scroll SELECTOR` to bring one
/// element into view. A direction word wins over a selector of the same name,
/// and no real selector is spelled `up` or `bottom`.
fn parse_scroll(flags: &Flags) -> Result<Request> {
    const HELP: &str = "browser scroll takes up|down|left|right|top|bottom [PIXELS] [--in SELECTOR], \
                        or a SELECTOR|@N to bring into view";
    let words: Vec<&str> = flags.positional.iter().map(String::as_str).collect();
    let op = match words.as_slice() {
        [word, rest @ ..] if ScrollDirection::parse(word).is_some() => {
            let pixels = match rest {
                [] => None,
                [text] => Some(text.parse::<u32>().map_err(|_| {
                    anyhow!("browser scroll's distance is a number of pixels, not {text:?}")
                })?),
                _ => bail!("{HELP}\n\n{USAGE}"),
            };
            PageOp::Scroll {
                direction: ScrollDirection::parse(word),
                pixels,
                selector: None,
                within: flags.within.clone(),
            }
        }
        [selector] => {
            if flags.within.is_some() {
                bail!("browser scroll --in goes with a direction\n\n{USAGE}");
            }
            PageOp::Scroll {
                direction: None,
                pixels: None,
                selector: Some((*selector).to_string()),
                within: None,
            }
        }
        _ => bail!("{HELP}\n\n{USAGE}"),
    };
    Ok(flags.page(op))
}

/// The text `pilus ctl` and `--help` print.
pub const USAGE: &str = "USAGE:\n\
    \x20   pilus ctl ping\n\
    \x20   pilus ctl state\n\
    \x20   pilus ctl sidebar open [changes|history|files|ports|browser|sipil]\n\
    \x20   pilus ctl sidebar close\n\
    \x20   pilus ctl sidebar toggle\n\
    \x20   pilus ctl tab focus N\n\n\
    \x20   pilus ctl ports list\n\
    \x20   pilus ctl ports forward PORT      # then open localhost:<local_port> it prints\n\
    \x20   pilus ctl ports stop PORT\n\n\
    BROWSER (no one has to say yes: you open tabs of your own, marked as an agent's,\n\
    and use only those; the person's tabs are theirs, and your tab is locked to them\n\
    while you work in it until you release it or they press Take over):\n\
    \x20   pilus ctl browser new [URL] [--no-wait]   # a tab of your own\n\
    \x20   pilus ctl browser open URL [--tab N] [--no-wait]  # in your tab N, else a new one\n\
    \x20   pilus ctl browser tabs                    # who opened each, and which you hold\n\
    \x20   pilus ctl browser close [N]\n\
    \x20   pilus ctl browser focus N\n\
    \x20   pilus ctl browser request N [WHY]         # ask to use a tab the person opened;\n\
    \x20                                              # waits for their yes (allowed, denied)\n\
    \x20   pilus ctl browser release [N]             # unlock your tabs when done\n\n\
    PAGE COMMANDS (your tabs, any address; --tab N picks one, else the selected):\n\
    \x20   pilus ctl browser snapshot        # the page as text, controls numbered @1, @2, ...\n\
    \x20   pilus ctl browser html [SELECTOR]\n\
    \x20   pilus ctl browser text [SELECTOR]\n\
    \x20   pilus ctl browser click SELECTOR|@N\n\
    \x20   pilus ctl browser scroll up|down|left|right|top|bottom [PIXELS] [--in SELECTOR|@N]\n\
    \x20   pilus ctl browser scroll SELECTOR|@N   # bring it into view\n\
    \x20   pilus ctl browser drag SELECTOR|@N SELECTOR|@N   # press, move to it, let go\n\
    \x20   pilus ctl browser drag SELECTOR|@N --by DX,DY    # or move by pixels\n\
    \x20   pilus ctl browser fill SELECTOR|@N TEXT\n\
    \x20   pilus ctl browser press KEY [SELECTOR|@N]\n\
    \x20   pilus ctl browser wait SELECTOR|@N [--gone] [--timeout MS]\n\
    \x20   pilus ctl browser eval [--body] JAVASCRIPT\n\
    \x20   pilus ctl browser network [--filter TEXT] [--limit N] [--clear]\n\
    \x20   pilus ctl browser console [--filter TEXT] [--limit N] [--clear]\n\
    \x20   pilus ctl browser back|forward|reload";

/// Send `request` to the window and wait for its answer.
pub fn send(path: &Path, request: &Request) -> Result<Reply> {
    let mut stream = UnixStream::connect(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            anyhow!(
                "pilus is not running (nothing is listening on {})",
                path.display()
            )
        }
        _ => anyhow!(error).context(format!("connect to {}", path.display())),
    })?;
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line).context("send the request")?;
    let mut answer = String::new();
    BufReader::new(stream.take(MAX_REPLY))
        .read_line(&mut answer)
        .context("read the reply")?;
    serde_json::from_str(answer.trim()).context("the reply was not understood")
}

/// Run `pilus ctl …`: send the request and print the answer.
pub fn run_client(args: &[String]) -> Result<()> {
    let request = parse_args(args)?;
    let reply = send(&client_socket(), &request).map_err(|error| {
        if error.to_string().contains("not running") {
            error.context(
                "if this is a host and the window is on another Mac, tick \"Let agents on \
                 <this machine> drive this window\" in that window's Ports tab",
            )
        } else {
            error
        }
    })?;
    if !reply.ok {
        bail!(
            reply
                .error
                .unwrap_or_else(|| "the window refused".to_string())
        );
    }
    match reply.data {
        // A snapshot or a page's text is for reading, not for parsing: print
        // it as written instead of as a JSON string full of `\n`.
        Some(Value::String(text)) => println!("{text}"),
        Some(data) => println!("{}", serde_json::to_string_pretty(&data)?),
        None => println!("ok"),
    }
    Ok(())
}

/// Start listening on `path` and return the stream of calls for the GUI.
///
/// A second pilus finding the socket live leaves it to the first. A socket
/// left behind by a crash refuses connections, and is replaced.
pub fn serve(path: &Path) -> Result<UnboundedReceiver<Call>> {
    if let Some(dir) = path.parent() {
        pilus_db::private_dir(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let listener = bind(path)?;
    // The folder is `0700`, so the mode only matters if it was loosened; the
    // socket is still never group- or world-writable.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict {}", path.display()))?;
    let (calls, incoming) = unbounded();
    std::thread::Builder::new()
        .name("pilus-ctl".to_string())
        .spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if calls.is_closed() {
                    return;
                }
                // A thread per client, so one waiting on a page does not hold
                // up the next. The cap and the read timeout bound what a bad
                // client can tie up.
                if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    active.fetch_sub(1, Ordering::SeqCst);
                    refuse(stream, "too many ctl clients at once");
                    continue;
                }
                let calls = calls.clone();
                let slot = Slot(Arc::clone(&active));
                let spawned = std::thread::Builder::new()
                    .name("pilus-ctl-client".to_string())
                    .spawn(move || {
                        let _slot = slot;
                        let result = handle(stream, |request| {
                            let (reply, answer) = Replier::pair();
                            calls
                                .unbounded_send(Call { request, reply })
                                .map_err(|_| anyhow!("the window has closed"))?;
                            answer
                                .recv_timeout(APPLY_TIMEOUT)
                                .map_err(|error| match error {
                                    RecvTimeoutError::Timeout => {
                                        anyhow!("the window did not answer in time")
                                    }
                                    RecvTimeoutError::Disconnected => {
                                        anyhow!("the window dropped the request")
                                    }
                                })
                        });
                        if let Err(error) = result {
                            tracing::debug!("ctl: {error:#}");
                        }
                    });
                if let Err(error) = spawned {
                    tracing::debug!("ctl: could not start a client thread: {error}");
                }
            }
        })
        .context("start the ctl listener")?;
    Ok(incoming)
}

/// Releases a client's place in the count when its thread ends, however it ends.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Turn a client away with a reason it can read.
fn refuse(mut stream: UnixStream, why: &str) {
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    if let Ok(mut out) = serde_json::to_vec(&Reply::err(why)) {
        out.push(b'\n');
        let _ = stream.write_all(&out);
    }
}

fn bind(path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                bail!("another pilus already listens on {}", path.display());
            }
            std::fs::remove_file(path)
                .with_context(|| format!("remove the stale {}", path.display()))?;
            UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))
        }
        Err(error) => Err(anyhow!(error).context(format!("bind {}", path.display()))),
    }
}

/// Read one request from `stream`, answer it with `apply`, and write the reply.
fn handle(mut stream: UnixStream, apply: impl FnOnce(Request) -> Result<Reply>) -> Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(READ_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line)?;
    let reply = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => apply(request).unwrap_or_else(|error| Reply::err(format!("{error:#}"))),
        Err(error) => Reply::err(format!("bad request: {error}")),
    };
    let mut out = serde_json::to_vec(&reply)?;
    if out.len() as u64 >= MAX_REPLY {
        // The client would cut it off mid-string and fail to parse it.
        out = serde_json::to_vec(&Reply::err("the answer is too large to send; narrow it"))?;
    }
    out.push(b'\n');
    // The request was read under the short timeout; a big answer to a slow
    // reader gets longer.
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.write_all(&out)?;
    Ok(())
}

/// The reply `State` builds, kept here so its shape is documented once.
pub fn state_reply(
    visible: bool,
    tab: &str,
    browser_tabs: usize,
    browser_url: &str,
    agent_tabs: usize,
    held: usize,
) -> Reply {
    Reply::data(json!({
        "sidebar": { "visible": visible, "tab": tab },
        "browser": {
            "tabs": browser_tabs,
            "url": browser_url,
            "agent_tabs": agent_tabs,
            "held": held,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn verbs_parse() {
        assert_eq!(parse_args(&words("ping")).ok(), Some(Request::Ping));
        assert_eq!(
            parse_args(&words("sidebar open changes")).ok(),
            Some(Request::SidebarOpen {
                tab: Some(Tab::Changes)
            })
        );
        assert_eq!(
            parse_args(&words("sidebar open")).ok(),
            Some(Request::SidebarOpen { tab: None })
        );
        assert_eq!(
            parse_args(&words("browser open localhost:3000")).ok(),
            Some(Request::BrowserOpen {
                url: "localhost:3000".to_string(),
                wait: true,
                tab: None,
            })
        );
        assert_eq!(
            parse_args(&words("tab focus 2")).ok(),
            Some(Request::TabFocus { index: 2 })
        );
    }

    fn page(args: &str) -> Request {
        parse_args(&words(args)).expect("parse")
    }

    #[test]
    fn browser_verbs_parse() {
        assert_eq!(page("browser tabs"), Request::BrowserTabs);
        assert_eq!(
            page("browser open localhost:8787 --no-wait"),
            Request::BrowserOpen {
                url: "localhost:8787".to_string(),
                wait: false,
                tab: None
            }
        );
        assert_eq!(
            page("browser open localhost:8787 --tab 2"),
            Request::BrowserOpen {
                url: "localhost:8787".to_string(),
                wait: true,
                tab: Some(2)
            }
        );
        assert_eq!(
            page("browser new"),
            Request::BrowserNew {
                url: None,
                wait: true
            }
        );
        assert_eq!(
            page("browser close 2"),
            Request::BrowserClose { tab: Some(2) }
        );
        assert_eq!(page("browser focus 3"), Request::BrowserFocus { tab: 3 });
        assert_eq!(
            page("browser click #go --tab 2"),
            Request::BrowserPage {
                tab: Some(2),
                op: PageOp::Click {
                    selector: "#go".to_string()
                }
            }
        );
        assert_eq!(
            page("browser wait @3 --gone --timeout 500"),
            Request::BrowserPage {
                tab: None,
                op: PageOp::Wait {
                    selector: "@3".to_string(),
                    gone: true,
                    timeout_ms: Some(500)
                }
            }
        );
        assert_eq!(
            page("browser network --filter /api --limit 5 --clear"),
            Request::BrowserPage {
                tab: None,
                op: PageOp::Network {
                    clear: true,
                    filter: Some("/api".to_string()),
                    limit: Some(5)
                }
            }
        );
        assert_eq!(
            page("browser press Enter input[name=q]"),
            Request::BrowserPage {
                tab: None,
                op: PageOp::Press {
                    key: "Enter".to_string(),
                    selector: Some("input[name=q]".to_string())
                }
            }
        );
    }

    #[test]
    fn free_text_keeps_its_dashes() {
        // Flags come first; after the first word everything is the text.
        assert_eq!(
            page("browser eval --tab 2 --body return a --tab b"),
            Request::BrowserPage {
                tab: Some(2),
                op: PageOp::Eval {
                    code: "return a --tab b".to_string(),
                    body: true
                }
            }
        );
        assert_eq!(
            page("browser fill #q --force me"),
            Request::BrowserPage {
                tab: None,
                op: PageOp::Fill {
                    selector: "#q".to_string(),
                    text: "--force me".to_string()
                }
            }
        );
    }

    #[test]
    fn release_names_a_tab_or_lets_go_of_all() {
        assert_eq!(
            page("browser release"),
            Request::BrowserRelease { tab: None }
        );
        assert_eq!(
            page("browser release 3"),
            Request::BrowserRelease { tab: Some(3) }
        );
        assert!(parse_args(&words("browser release 0")).is_err());
        assert!(parse_args(&words("browser release 1 2")).is_err());
    }

    #[test]
    fn scroll_takes_a_direction_or_a_selector() {
        assert_eq!(
            page("browser scroll down"),
            Request::BrowserPage {
                tab: None,
                op: PageOp::Scroll {
                    direction: Some(ScrollDirection::Down),
                    pixels: None,
                    selector: None,
                    within: None,
                }
            }
        );
        assert_eq!(
            page("browser scroll up 300 --in .list --tab 2"),
            Request::BrowserPage {
                tab: Some(2),
                op: PageOp::Scroll {
                    direction: Some(ScrollDirection::Up),
                    pixels: Some(300),
                    selector: None,
                    within: Some(".list".to_string()),
                }
            }
        );
        assert_eq!(
            page("browser scroll @4"),
            Request::BrowserPage {
                tab: None,
                op: PageOp::Scroll {
                    direction: None,
                    pixels: None,
                    selector: Some("@4".to_string()),
                    within: None,
                }
            }
        );
        for bad in [
            "browser scroll",
            "browser scroll down far",
            "browser scroll down 1 2",
            "browser scroll #a --in #b",
            "browser scroll #a #b",
        ] {
            assert!(parse_args(&words(bad)).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn drag_takes_a_target_or_an_offset() {
        assert_eq!(
            page("browser drag #card @3"),
            Request::BrowserPage {
                tab: None,
                op: PageOp::Drag {
                    from: "#card".to_string(),
                    to: Some("@3".to_string()),
                    by: None,
                }
            }
        );
        assert_eq!(
            page("browser drag .knob --by -40,25 --tab 2"),
            Request::BrowserPage {
                tab: Some(2),
                op: PageOp::Drag {
                    from: ".knob".to_string(),
                    to: None,
                    by: Some([-40, 25]),
                }
            }
        );
        for bad in [
            "browser drag",
            "browser drag #a",
            "browser drag #a #b --by 1,1",
            "browser drag #a --by 1",
            "browser drag #a --by x,y",
            "browser drag #a #b #c",
        ] {
            assert!(parse_args(&words(bad)).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn asking_for_a_tab_names_it_and_takes_the_rest_as_the_reason() {
        assert_eq!(
            page("browser request 2 check the todo app"),
            Request::BrowserBorrow {
                tab: 2,
                reason: Some("check the todo app".to_string())
            }
        );
        assert_eq!(
            page("browser request 1"),
            Request::BrowserBorrow {
                tab: 1,
                reason: None
            }
        );
        for bad in ["browser request", "browser request 0", "browser request x"] {
            assert!(parse_args(&words(bad)).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn asking_for_a_tab_keeps_the_verb_name_on_the_wire() {
        let text = serde_json::to_string(&page("browser request 2 why")).expect("encode");
        assert_eq!(text, r#"{"cmd":"browser_request","tab":2,"reason":"why"}"#);
    }

    #[test]
    fn the_old_ask_for_the_browser_still_parses() {
        // An agent written against the grant keeps working: the window
        // answers it at once and changes nothing.
        assert_eq!(
            page("browser control to check the todo app"),
            Request::BrowserControl {
                reason: Some("to check the todo app".to_string())
            }
        );
        assert_eq!(
            page("browser control"),
            Request::BrowserControl { reason: None }
        );
    }

    #[test]
    fn ports_verbs_parse() {
        assert_eq!(page("ports list"), Request::PortsList);
        assert_eq!(
            page("ports forward 8787"),
            Request::PortsForward { port: 8787 }
        );
        assert_eq!(page("ports stop 8787"), Request::PortsStop { port: 8787 });
        assert!(parse_args(&words("ports forward 0")).is_err());
        assert!(parse_args(&words("ports forward 70000")).is_err());
    }

    #[test]
    fn bad_browser_input_is_refused() {
        for bad in [
            "browser",
            "browser nope",
            "browser click",
            "browser click a b",
            "browser eval",
            "browser fill #q",
            "browser focus",
            "browser focus 0",
            "browser snapshot --tab 0",
            "browser snapshot --tab",
            "browser snapshot --sideways",
            "browser wait #a --timeout soon",
        ] {
            assert!(parse_args(&words(bad)).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn a_page_verb_has_a_flat_wire_form() {
        let request = page("browser click #go --tab 2");
        let text = serde_json::to_string(&request).expect("encode");
        assert_eq!(
            text,
            r##"{"cmd":"browser_page","tab":2,"op":"click","selector":"#go"}"##
        );
        let back: Request = serde_json::from_str(&text).expect("decode");
        assert_eq!(back, request);
        // A client written before `wait` existed still opens pages.
        let old: Request =
            serde_json::from_str(r#"{"cmd":"browser_open","url":"localhost:1"}"#).expect("old");
        assert_eq!(
            old,
            Request::BrowserOpen {
                url: "localhost:1".to_string(),
                wait: true,
                tab: None
            }
        );
        // And so is one written before `release` named a tab.
        let old: Request = serde_json::from_str(r#"{"cmd":"browser_release"}"#).expect("old");
        assert_eq!(old, Request::BrowserRelease { tab: None });
    }

    #[test]
    fn a_late_answer_reaches_the_client() {
        let dir = scratch("late");
        let path = dir.join("pilus.sock");
        let mut calls = serve(&path).expect("serve");
        let answering = std::thread::spawn(move || {
            use futures::StreamExt;
            let call = futures::executor::block_on(calls.next()).expect("a call");
            let replier = call.replier();
            // The handler returns without answering; a worker answers later.
            drop(call);
            std::thread::sleep(Duration::from_millis(150));
            replier.send(Reply::data(json!("late")));
            replier.send(Reply::err("a second answer is ignored"));
        });
        let reply = send(&path, &Request::PortsList).expect("send");
        assert_eq!(reply.data, Some(json!("late")));
        answering.join().expect("join");
    }

    #[test]
    fn a_dropped_request_is_reported_at_once() {
        let dir = scratch("dropped");
        let path = dir.join("pilus.sock");
        let mut calls = serve(&path).expect("serve");
        let answering = std::thread::spawn(move || {
            use futures::StreamExt;
            drop(futures::executor::block_on(calls.next()).expect("a call"));
        });
        let started = std::time::Instant::now();
        let reply = send(&path, &Request::PortsList).expect("send");
        assert!(!reply.ok);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no timeout wait"
        );
        answering.join().expect("join");
    }

    #[test]
    fn a_slow_client_does_not_hold_up_another() {
        let dir = scratch("slow");
        let path = dir.join("pilus.sock");
        let mut calls = serve(&path).expect("serve");
        // Connects and says nothing, so its thread waits out the read timeout.
        let _idle = UnixStream::connect(&path).expect("connect");
        let answering = std::thread::spawn(move || {
            use futures::StreamExt;
            let call = futures::executor::block_on(calls.next()).expect("a call");
            call.respond(Reply::ok());
        });
        let started = std::time::Instant::now();
        assert!(send(&path, &Request::Ping).expect("send").ok);
        assert!(
            started.elapsed() < READ_TIMEOUT,
            "served while the other idles"
        );
        answering.join().expect("join");
    }

    #[test]
    fn a_client_prefers_its_own_window_then_the_tunnel() {
        let own = PathBuf::from("/h/.pilus/pilus.sock");
        let tunnel = PathBuf::from("/h/.pilus/pilus-remote.sock");
        let live = |paths: &'static [&'static str]| {
            move |path: &Path| paths.iter().any(|live| Path::new(live) == path)
        };
        let both = live(&["/h/.pilus/pilus.sock", "/h/.pilus/pilus-remote.sock"]);
        assert_eq!(pick_socket(own.clone(), tunnel.clone(), both), own);
        let only_tunnel = live(&["/h/.pilus/pilus-remote.sock"]);
        assert_eq!(
            pick_socket(own.clone(), tunnel.clone(), only_tunnel),
            tunnel
        );
        // Neither: the window's own path, so the error names it.
        assert_eq!(pick_socket(own.clone(), tunnel, live(&[])), own);
    }

    #[test]
    fn tabs_answer_to_their_old_names() {
        assert_eq!(Tab::parse("diff").ok(), Some(Tab::Changes));
        assert_eq!(Tab::parse("Git").ok(), Some(Tab::History));
        assert!(Tab::parse("nope").is_err());
    }

    #[test]
    fn bad_input_is_refused() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&words("sidebar open nope")).is_err());
        assert!(parse_args(&words("tab focus 0")).is_err());
        assert!(parse_args(&words("tab focus x")).is_err());
        assert!(parse_args(&words("browser open")).is_err());
    }

    #[test]
    fn the_wire_form_is_stable() {
        let request = Request::SidebarOpen {
            tab: Some(Tab::History),
        };
        let text = serde_json::to_string(&request).expect("encode");
        assert_eq!(text, r#"{"cmd":"sidebar_open","tab":"history"}"#);
        let back: Request =
            serde_json::from_str(r#"{"cmd":"sidebar_open","tab":"git"}"#).expect("decode");
        assert_eq!(back, request);
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"rm_rf"}"#).is_err());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-ctl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn a_request_round_trips_through_the_socket() {
        let dir = scratch("round");
        let path = dir.join("pilus.sock");
        let mut calls = serve(&path).expect("serve");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o077, 0, "the socket is owner-only");
        let answering = std::thread::spawn(move || {
            use futures::StreamExt;
            let call = futures::executor::block_on(calls.next()).expect("a call");
            assert_eq!(call.request, Request::SidebarToggle);
            call.respond(Reply::ok());
        });
        let reply = send(&path, &Request::SidebarToggle).expect("send");
        assert!(reply.ok);
        answering.join().expect("join");
    }

    #[test]
    fn garbage_gets_an_error_reply_and_does_not_reach_the_window() {
        let dir = scratch("garbage");
        let path = dir.join("pilus.sock");
        let _calls = serve(&path).expect("serve");
        let mut stream = UnixStream::connect(&path).expect("connect");
        stream.write_all(b"{\"cmd\":\"nope\"}\n").expect("write");
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer).expect("read");
        let reply: Reply = serde_json::from_str(answer.trim()).expect("reply");
        assert!(!reply.ok);
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_kept() {
        let dir = scratch("stale");
        let path = dir.join("pilus.sock");
        // A socket file nothing listens on, as a crash leaves.
        drop(UnixListener::bind(&path).expect("bind"));
        assert!(path.exists());
        let first = serve(&path).expect("replaces the stale socket");
        assert!(serve(&path).is_err(), "a live socket is not taken over");
        drop(first);
    }

    #[test]
    fn no_listener_reads_as_not_running() {
        let dir = scratch("none");
        let error = send(&dir.join("pilus.sock"), &Request::Ping).expect_err("nothing there");
        assert!(error.to_string().contains("not running"));
    }
}
