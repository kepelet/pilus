//! Listening ports on a machine, and forwarding one of them to this Mac.
//!
//! A dev server on a remote host (`npm run dev` on the Mac mini) listens on
//! the host's loopback, out of reach of a browser here. [`list_ports`] asks the
//! host what its account is listening on, and [`PortForward`] holds an
//! `ssh -N -L` open so `localhost:<port>` here reaches it. Both go through the
//! local OpenSSH client and the saved SSH profile, as an upload or a commit
//! read does, so the host needs nothing installed beyond `lsof` or `ss`.
//!
//! Only the account's own listeners are reported: `lsof` shows no one else's
//! to a non-root user, and an `ss` row without a process is somebody else's.
//! That keeps `sshd` and the resolver out of a list meant for dev servers.

use std::collections::HashMap;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::transport::SshTarget;

/// How long a forward may take to start listening, which covers the SSH
/// handshake on a slow link.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(50);
/// Bound on captured ssh stderr, so a chatty host cannot grow without limit.
const MAX_STDERR: usize = 4 * 1024;

/// The scan the host runs. `PATH` gains the sbin directories because a
/// non-interactive SSH shell often lacks them and `lsof` lives there on Linux.
/// The first line names the tool so the parser knows the format; after the
/// listeners comes [`PS_MARK`] and one `pid args` line per process found, the
/// full command line a listener row can show. The script always exits 0, since
/// `lsof` exits 1 when nothing listens.
const SCAN: &str = r#"PATH="$PATH:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin"
if command -v lsof >/dev/null 2>&1; then
  echo lsof
  out=$(lsof -nP -iTCP -sTCP:LISTEN -Fpcn 2>/dev/null)
  pids=$(printf '%s\n' "$out" | sed -n 's/^p//p' | sort -un | paste -sd, -)
elif command -v ss >/dev/null 2>&1; then
  echo ss
  out=$(ss -ltnpH 2>/dev/null)
  pids=$(printf '%s\n' "$out" | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -un | paste -sd, -)
else
  echo none
  exit 0
fi
printf '%s\n' "$out"
echo --ps--
[ -n "$pids" ] && ps -ww -o pid= -o args= -p "$pids" 2>/dev/null
exit 0"#;

/// The line between the listeners and the process table in [`SCAN`]'s output.
const PS_MARK: &str = "--ps--";

/// One TCP port a process on the machine is listening on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListeningPort {
    pub port: u16,
    /// The name of the process holding it.
    pub process: String,
    /// Its process id, which the kill is aimed at.
    pub pid: Option<u32>,
    /// The process's full command line (`python3 -m http.server 1337`); empty
    /// when the host did not report it.
    pub command: String,
    /// The address the host connects to for this listener: `localhost` for a
    /// wildcard or loopback bind, the literal address for any other.
    pub host: String,
}

/// What the machine's account is listening on, by port. `target` is `None` for
/// this Mac.
pub fn list_ports(target: Option<&SshTarget>) -> Result<Vec<ListeningPort>, String> {
    let output = match target {
        None => Command::new("/bin/sh").arg("-c").arg(SCAN).output(),
        Some(target) => target.shell_multiplexed(SCAN).output(),
    }
    .map_err(|error| format!("couldn't reach the host: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "couldn't list the host's ports".to_string()
        } else {
            stderr
        });
    }
    parse_scan(&String::from_utf8_lossy(&output.stdout))
}

fn parse_scan(text: &str) -> Result<Vec<ListeningPort>, String> {
    let (tool, rest) = text.split_once('\n').unwrap_or((text, ""));
    let (body, table) = rest.split_once(PS_MARK).unwrap_or((rest, ""));
    let mut ports = match tool.trim() {
        "lsof" => parse_lsof(body),
        "ss" => parse_ss(body),
        _ => return Err("the host has neither lsof nor ss to list ports".to_string()),
    };
    let commands = parse_ps(table);
    for port in &mut ports {
        if let Some(command) = port.pid.and_then(|pid| commands.get(&pid)) {
            port.command.clone_from(command);
        }
    }
    ports.sort_by_key(|port| port.port);
    ports.dedup_by_key(|port| port.port);
    Ok(ports)
}

/// `ps -o pid= -o args=`: the pid, then the command line to the end of the row.
fn parse_ps(table: &str) -> HashMap<u32, String> {
    table
        .lines()
        .filter_map(|line| {
            let (pid, args) = line.trim().split_once(char::is_whitespace)?;
            Some((pid.parse().ok()?, args.trim().to_string()))
        })
        .collect()
}

/// `lsof -Fpcn`: a `p` line opens a process, `c` names it, and each `n` line is
/// one listening socket of it (`*:5173`, `127.0.0.1:5173`, `[::1]:5173`).
fn parse_lsof(body: &str) -> Vec<ListeningPort> {
    let mut ports = Vec::new();
    let mut process = String::new();
    let mut pid = None;
    for line in body.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                process.clear();
                pid = value.parse().ok();
            }
            // lsof escapes a space in a command name as `\x20`.
            'c' => process = value.replace("\\x20", " "),
            'n' => {
                if let Some(port) = listener(value, &process, pid) {
                    ports.push(port);
                }
            }
            _ => {}
        }
    }
    ports
}

/// `ss -ltnpH`: `LISTEN 0 511 127.0.0.1:5173 0.0.0.0:* users:(("node",pid=1,fd=2))`.
fn parse_ss(body: &str) -> Vec<ListeningPort> {
    body.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let local = fields.get(3)?;
            let users = fields.iter().find_map(|f| f.strip_prefix("users:"))?;
            let name = users.strip_prefix("((\"")?.split('"').next()?;
            let pid = users
                .split("pid=")
                .nth(1)
                .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|digits| digits.parse().ok());
            listener(local, name, pid)
        })
        .collect()
}

/// A `host:port` socket name as a listener, if it reads as one.
fn listener(socket: &str, process: &str, pid: Option<u32>) -> Option<ListeningPort> {
    let (address, port) = socket.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some(ListeningPort {
        port,
        process: process.to_string(),
        pid,
        command: String::new(),
        host: connect_host(address)?,
    })
}

/// Stop the process `pid` that holds `port`, with SIGTERM. The host checks
/// first that the pid still holds the port, so a process that exited since the
/// last scan, and whose pid was reused, is never signalled.
pub fn kill_process(target: Option<&SshTarget>, pid: u32, port: u16) -> Result<(), String> {
    let script = format!(
        r#"PATH="$PATH:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin"
if command -v lsof >/dev/null 2>&1; then
  held=$(lsof -nP -iTCP:{port} -sTCP:LISTEN -t 2>/dev/null)
else
  held=$(ss -ltnpH "sport = :{port}" 2>/dev/null | grep -o 'pid=[0-9]*' | cut -d= -f2)
fi
printf '%s\n' "$held" | grep -qx {pid} || {{ echo "port {port} is no longer held by process {pid}" >&2; exit 1; }}
kill {pid}"#
    );
    let output = match target {
        None => Command::new("/bin/sh").arg("-c").arg(&script).output(),
        Some(target) => target.shell(&script).output(),
    }
    .map_err(|error| format!("couldn't reach the host: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("couldn't stop process {pid}")
    } else {
        stderr
    })
}

/// The host a forward connects to for a listener bound to `address`. `None`
/// for text that is no address, since the result ends up in an ssh argument.
fn connect_host(address: &str) -> Option<String> {
    match address {
        "*" | "0.0.0.0" | "[::]" | "::" | "127.0.0.1" | "[::1]" | "::1" => {
            return Some("localhost".to_string());
        }
        _ => {}
    }
    // `lsof -n` never prints a hostname, and a zone id (`fe80::1%en0`) is not
    // valid in a forward target, so anything past hex, dots and colons is out.
    let bare = address.trim_start_matches('[').trim_end_matches(']');
    let valid = !bare.is_empty()
        && bare
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':');
    valid.then(|| {
        if bare.contains(':') {
            format!("[{bare}]")
        } else {
            bare.to_string()
        }
    })
}

/// A local port to listen on for a remote `preferred`: the same number when it
/// is free, so `localhost:5173` stays `localhost:5173`, else any free one.
pub fn free_local_port(preferred: u16) -> Option<u16> {
    if port_free(preferred) {
        return Some(preferred);
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
    listener.local_addr().ok().map(|addr| addr.port())
}

/// Whether nothing here holds `port`. Binding alone is not enough: on macOS a
/// loopback bind succeeds beside a wildcard listener, so a connect checks too.
fn port_free(port: u16) -> bool {
    if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
        return false;
    }
    let busy =
        |addr: SocketAddr| TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok();
    !busy(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        && !busy(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)))
}

/// A running forward. Dropping it ends the SSH process, so the forward lasts
/// as long as the handle.
pub struct PortForward {
    child: Child,
    local_port: u16,
}

impl PortForward {
    /// Open the forward and wait until `localhost:local_port` is listening.
    /// This blocks on the SSH handshake: call it off the GUI thread.
    pub fn start(
        target: &SshTarget,
        local_port: u16,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<Self, String> {
        let mut command = target.forward_command(local_port, remote_host, remote_port);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("couldn't run ssh: {error}"))?;
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let stderr = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 512];
                while let Ok(n) = pipe.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut guard) = stderr.lock()
                        && guard.len() < MAX_STDERR
                    {
                        guard.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                }
            });
        }
        let forward = Self { child, local_port };
        forward.wait_listening(stderr)
    }

    /// Wait for ssh to take the local port. The port was free when chosen, so
    /// a bind that now fails means ssh holds it; connecting instead would
    /// open a channel to the dev server on every probe.
    fn wait_listening(mut self, stderr: Arc<Mutex<String>>) -> Result<Self, String> {
        let started = Instant::now();
        while started.elapsed() < READY_TIMEOUT {
            if let Ok(Some(status)) = self.child.try_wait() {
                // Let the drain thread take the last of stderr.
                std::thread::sleep(Duration::from_millis(50));
                let text = stderr
                    .lock()
                    .map(|g| g.trim().to_string())
                    .unwrap_or_default();
                return Err(if text.is_empty() {
                    format!("ssh exited ({status})")
                } else {
                    text
                });
            }
            if TcpListener::bind((Ipv4Addr::LOCALHOST, self.local_port)).is_err() {
                return Ok(self);
            }
            std::thread::sleep(READY_POLL);
        }
        Err("timed out waiting for the forward to open".to_string())
    }

    /// Hold an already-running process as a forward, for the visual-test
    /// captures, which cannot reach a host.
    #[doc(hidden)]
    pub fn adopt(child: Child, local_port: u16) -> Self {
        Self { child, local_port }
    }

    /// The port on this Mac that reaches the remote one.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Whether the SSH process has ended, as it does when the link drops.
    pub fn has_exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_groups_sockets_under_their_process() {
        let text = "lsof\np101\ncnode\nn*:5173\nn[::1]:5173\np202\ncGoogle\\x20Chrome\nn127.0.0.1:9222\np303\ncpostgres\nn10.0.0.5:5432\n--ps--\n  101 node /app/node_modules/.bin/vite --port 5173\n  303 /usr/bin/postgres -D /data\n";
        let ports = parse_scan(text).expect("parsed");
        let seen: Vec<_> = ports
            .iter()
            .map(|p| {
                (
                    p.port,
                    p.process.as_str(),
                    p.host.as_str(),
                    p.pid,
                    p.command.as_str(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (
                    5173,
                    "node",
                    "localhost",
                    Some(101),
                    "node /app/node_modules/.bin/vite --port 5173"
                ),
                (
                    5432,
                    "postgres",
                    "10.0.0.5",
                    Some(303),
                    "/usr/bin/postgres -D /data"
                ),
                (9222, "Google Chrome", "localhost", Some(202), ""),
            ]
        );
    }

    #[test]
    fn ss_keeps_only_rows_with_a_process() {
        let text = "ss\n\
            LISTEN 0 511 127.0.0.1:5173 0.0.0.0:* users:((\"node\",pid=1234,fd=20))\n\
            LISTEN 0 128 0.0.0.0:22 0.0.0.0:*\n\
            LISTEN 0 511 [::]:3000 [::]:* users:((\"next-server\",pid=9,fd=3))\n";
        let ports = parse_scan(text).expect("parsed");
        let seen: Vec<_> = ports
            .iter()
            .map(|p| (p.port, p.process.as_str(), p.pid))
            .collect();
        assert_eq!(
            seen,
            [(3000, "next-server", Some(9)), (5173, "node", Some(1234))]
        );
    }

    #[test]
    fn a_host_without_either_tool_says_so() {
        assert!(parse_scan("none\n").is_err());
        assert!(parse_scan("").is_err());
    }

    #[test]
    fn an_empty_scan_is_no_ports() {
        assert_eq!(parse_scan("lsof\n").expect("parsed"), Vec::new());
    }

    #[test]
    fn hostile_addresses_never_reach_an_ssh_argument() {
        assert_eq!(connect_host("a:b -oProxyCommand=x"), None);
        assert_eq!(connect_host("fe80::1%en0"), None);
        assert_eq!(connect_host("[fd00::7]").as_deref(), Some("[fd00::7]"));
    }

    #[test]
    fn forward_command_carries_the_port_pair() {
        let target = SshTarget {
            destination: "mini".to_string(),
            binary: "/usr/local/bin/luvus".to_string(),
            session: None,
        };
        let command = target.forward_command(5174, "localhost", 5173);
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = args.iter().position(|a| a == "-L").expect("-L");
        assert_eq!(args[at + 1], "127.0.0.1:5174:localhost:5173");
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("mini"));
    }

    /// Whether `pid` still names a process, zombie or not: `kill -0` fails
    /// only once the process has been reaped.
    fn exists(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn dropping_a_forward_kills_and_reaps_its_process() {
        let child = Command::new("sleep").arg("30").spawn().expect("sleep");
        let pid = child.id();
        let forward = PortForward {
            child,
            local_port: 0,
        };
        assert!(exists(pid));
        drop(forward);
        assert!(!exists(pid), "a dropped forward left a process behind");
    }

    #[test]
    fn a_forward_that_died_on_its_own_is_noticed_and_reaped() {
        let child = Command::new("true").spawn().expect("true");
        let pid = child.id();
        let mut forward = PortForward {
            child,
            local_port: 0,
        };
        let started = Instant::now();
        while !forward.has_exited() {
            assert!(started.elapsed() < Duration::from_secs(5), "never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!exists(pid), "an exited forward stayed defunct");
    }

    #[test]
    fn this_process_is_found_listening_with_its_command_line() {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = held.local_addr().expect("addr").port();
        // A host with neither lsof nor ss has nothing to check.
        let Ok(ports) = list_ports(None) else {
            return;
        };
        let found = ports.iter().find(|p| p.port == port).expect("our port");
        assert_eq!(found.pid, Some(std::process::id()));
        assert!(!found.command.is_empty(), "no command line for our own pid");
    }

    #[test]
    fn a_kill_aimed_at_a_pid_that_does_not_hold_the_port_is_refused() {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = held.local_addr().expect("addr").port();
        // The parent shell is alive and ours, but holds no such port.
        let error = kill_process(None, std::process::id().wrapping_add(1), port)
            .expect_err("must not signal a stranger");
        assert!(error.contains("no longer held"), "{error}");
    }

    #[test]
    fn a_listening_server_is_found_by_command_and_killed() {
        let port = {
            let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
            probe.local_addr().expect("addr").port()
        };
        let Ok(mut server) = Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let started = Instant::now();
        let found = loop {
            let listed = list_ports(None).unwrap_or_default();
            if let Some(found) = listed.into_iter().find(|p| p.port == port) {
                break found;
            }
            if started.elapsed() > Duration::from_secs(10) {
                let _ = server.kill();
                panic!("the server never showed up in the scan");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(found.pid, Some(server.id()));
        assert!(found.command.contains("http.server"), "{}", found.command);
        kill_process(None, server.id(), port).expect("kill");
        assert!(server.wait().is_ok_and(|status| !status.success()));
    }

    #[test]
    fn a_taken_port_falls_back_to_another() {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let taken = held.local_addr().expect("addr").port();
        let chosen = free_local_port(taken).expect("a free port");
        assert_ne!(chosen, taken);
    }
}
