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
//! The listener blocks in `accept`, so an idle window pays nothing for it. See
//! [idle cost](../../../docs/agents/idle-cost.md). The rules for the socket
//! itself are in [security](../../../docs/agents/security.md#the-control-socket).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The longest request line read. A request is a verb and a URL.
const MAX_REQUEST: u64 = 16 * 1024;
/// How long a client may take to send its line, so a stalled one cannot hold
/// the accept loop.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the server waits for the window to answer.
const APPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the client waits for the whole exchange.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(8);

/// Where the window listens.
pub fn socket_path() -> PathBuf {
    pilus_client::pilus_config_dir().join("pilus.sock")
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
    /// Load `url` in the Browser tab, opening the sidebar on it.
    BrowserOpen { url: String },
    /// Go to the workspace tab at the 1-based `index`.
    TabFocus { index: u16 },
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

/// A request waiting for the window, with the slot its answer goes in.
pub struct Call {
    pub request: Request,
    reply: SyncSender<Reply>,
}

impl Call {
    /// Answer the client. A client that has gone away is not an error.
    pub fn respond(self, reply: Reply) {
        let _ = self.reply.send(reply);
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
        ["browser", "open", url] => Ok(Request::BrowserOpen {
            url: (*url).to_string(),
        }),
        ["tab", "focus", index] => Ok(Request::TabFocus {
            index: index
                .parse::<u16>()
                .ok()
                .filter(|index| *index >= 1)
                .ok_or_else(|| anyhow!("tab focus needs a number from 1, not {index:?}"))?,
        }),
        [] => bail!("pilus ctl needs a command\n\n{USAGE}"),
        _ => bail!("unrecognized command: {}\n\n{USAGE}", words.join(" ")),
    }
}

/// The text `pilus ctl` and `--help` print.
pub const USAGE: &str = "USAGE:\n\
    \x20   pilus ctl ping\n\
    \x20   pilus ctl state\n\
    \x20   pilus ctl sidebar open [changes|history|files|ports|browser|sipil]\n\
    \x20   pilus ctl sidebar close\n\
    \x20   pilus ctl sidebar toggle\n\
    \x20   pilus ctl browser open URL\n\
    \x20   pilus ctl tab focus N";

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
    BufReader::new(stream.take(MAX_REQUEST))
        .read_line(&mut answer)
        .context("read the reply")?;
    serde_json::from_str(answer.trim()).context("the reply was not understood")
}

/// Run `pilus ctl …`: send the request and print the answer.
pub fn run_client(args: &[String]) -> Result<()> {
    let request = parse_args(args)?;
    let reply = send(&socket_path(), &request)?;
    if !reply.ok {
        bail!(
            reply
                .error
                .unwrap_or_else(|| "the window refused".to_string())
        );
    }
    match reply.data {
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
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                // One at a time: a request is a click's worth of work, and
                // the timeouts bound how long a bad client can hold the loop.
                if let Err(error) = handle(stream, |request| {
                    let (reply, answer) = sync_channel(1);
                    calls
                        .unbounded_send(Call { request, reply })
                        .map_err(|_| anyhow!("the window has closed"))?;
                    answer
                        .recv_timeout(APPLY_TIMEOUT)
                        .map_err(|_| anyhow!("the window did not answer"))
                }) {
                    tracing::debug!("ctl: {error:#}");
                }
                if calls.is_closed() {
                    return;
                }
            }
        })
        .context("start the ctl listener")?;
    Ok(incoming)
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
    out.push(b'\n');
    stream.write_all(&out)?;
    Ok(())
}

/// The reply `State` builds, kept here so its shape is documented once.
pub fn state_reply(visible: bool, tab: &str, browser_tabs: usize, browser_url: &str) -> Reply {
    Reply::data(json!({
        "sidebar": { "visible": visible, "tab": tab },
        "browser": { "tabs": browser_tabs, "url": browser_url },
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
                url: "localhost:3000".to_string()
            })
        );
        assert_eq!(
            parse_args(&words("tab focus 2")).ok(),
            Some(Request::TabFocus { index: 2 })
        );
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
