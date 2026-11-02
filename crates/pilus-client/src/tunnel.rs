//! A reverse tunnel that carries this Mac's `pilus ctl` socket to an SSH
//! machine, so an agent running there can drive the window it is shown in.
//!
//! The agent runs on the host, the window on this Mac, and a unix socket does
//! not cross machines. `ssh -R` can publish one: [`CtlTunnel`] holds an
//! `ssh -N -R <host>:~/.pilus/pilus-remote.sock:<here>` open, the way
//! [`crate::PortForward`] holds a `-L`.
//!
//! The socket gets a name of its own, not `pilus.sock`: a host that runs a
//! pilus window of its own listens there, and the tunnel must never replace it.
//! A stale socket from an earlier tunnel is removed before the new one binds,
//! because only the server's `StreamLocalBindUnlink` could do it for `-R`, and
//! that defaults to off. So two Macs tunnelling to one host take the socket in
//! turn, and the last one wins.
//!
//! Nothing here polls. A tunnel's reader thread blocks on ssh's stderr until
//! the process ends; killing the process is how it is stopped.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::git::sh_quote;
use crate::transport::SshTarget;

/// The socket's name in the host's `~/.pilus`.
pub const REMOTE_SOCKET: &str = "pilus-remote.sock";

/// How long a tunnel may take to come up, which covers the SSH handshake.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(250);
/// Bound on captured ssh stderr.
const MAX_STDERR: usize = 4 * 1024;

/// Make `~/.pilus` on the host owner-only, clear a stale tunnel socket, and
/// say what `$HOME` is. The folder's mode is what keeps other accounts off the
/// socket, so it is set before the socket exists.
const PREPARE: &str = "umask 077; mkdir -p \"$HOME/.pilus\" && chmod 700 \"$HOME/.pilus\" \
     && rm -f \"$HOME/.pilus/pilus-remote.sock\" && printf '%s' \"$HOME\"";

/// A running tunnel. Dropping it does not end it; use [`CtlKiller::kill`].
pub struct CtlTunnel {
    child: Arc<Mutex<Child>>,
    ended: Receiver<String>,
}

/// A handle that can end a tunnel from another thread while one is blocked in
/// [`CtlTunnel::wait_exit`].
#[derive(Clone)]
pub struct CtlKiller(Arc<Mutex<Child>>);

impl CtlKiller {
    pub fn kill(&self) {
        let mut child = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = child.kill();
    }
}

impl CtlTunnel {
    /// Bring the tunnel up and wait until the host has the socket. This blocks
    /// on SSH: call it off the GUI thread.
    pub fn start(target: &SshTarget, local_socket: &Path) -> Result<Self, String> {
        let prepared = target
            .shell_multiplexed(PREPARE)
            .output()
            .map_err(|error| format!("couldn't run ssh: {error}"))?;
        if !prepared.status.success() {
            let why = String::from_utf8_lossy(&prepared.stderr).trim().to_string();
            return Err(if why.is_empty() {
                "couldn't prepare ~/.pilus on the host".to_string()
            } else {
                why
            });
        }
        let home = String::from_utf8_lossy(&prepared.stdout).trim().to_string();
        if !home.starts_with('/') {
            return Err("the host did not say where its home is".to_string());
        }
        let remote = format!("{home}/.pilus/{REMOTE_SOCKET}");
        let mut command = target.ctl_tunnel_command(&remote, local_socket)?;
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("couldn't run ssh: {error}"))?;
        let stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(child));
        let (ended_tx, ended) = channel();
        let reaper = Arc::clone(&child);
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = stderr {
                let mut chunk = [0u8; 512];
                while let Ok(n) = pipe.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    if text.len() < MAX_STDERR {
                        text.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                }
            }
            // The pipe closes when ssh ends; reap it.
            let status = reaper.lock().unwrap_or_else(PoisonError::into_inner).wait();
            let text = text.trim().to_string();
            let why = match (text.is_empty(), status) {
                (false, _) => text,
                (true, Ok(status)) => format!("ssh exited ({status})"),
                (true, Err(error)) => format!("ssh ended: {error}"),
            };
            let _ = ended_tx.send(why);
        });
        let tunnel = Self { child, ended };
        tunnel.wait_ready(target, &remote)?;
        Ok(tunnel)
    }

    /// Wait for the host to show the socket. A tunnel that dies first says why.
    fn wait_ready(&self, target: &SshTarget, remote: &str) -> Result<(), String> {
        let started = Instant::now();
        let check = format!("test -S {}", sh_quote(remote));
        while started.elapsed() < READY_TIMEOUT {
            if let Ok(why) = self.ended.try_recv() {
                return Err(why);
            }
            let there = target
                .shell_multiplexed(&check)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if there {
                return Ok(());
            }
            std::thread::sleep(READY_POLL);
        }
        self.killer().kill();
        Err("timed out waiting for the tunnel to open".to_string())
    }

    /// A handle that ends the tunnel.
    pub fn killer(&self) -> CtlKiller {
        CtlKiller(Arc::clone(&self.child))
    }

    /// Block until the tunnel ends, and say why. Nothing is polled: this
    /// returns when ssh closes its stderr and has been reaped.
    pub fn wait_exit(self) -> String {
        self.ended
            .recv()
            .unwrap_or_else(|_| "the tunnel ended".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            destination: "me@host".to_string(),
            binary: "/x/luvus".to_string(),
            session: None,
        }
    }

    fn words(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_tunnel_publishes_a_socket_and_dies_if_it_cannot() {
        let command = target()
            .ctl_tunnel_command(
                "/home/me/.pilus/pilus-remote.sock",
                Path::new("/Users/me/.pilus/pilus.sock"),
            )
            .expect("command");
        let args = words(&command);
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert!(args.contains(&"ControlPath=none".to_string()));
        let at = args.iter().position(|word| word == "-R").expect("-R");
        assert_eq!(
            args[at + 1],
            "/home/me/.pilus/pilus-remote.sock:/Users/me/.pilus/pilus.sock"
        );
        assert_eq!(args.last().map(String::as_str), Some("me@host"));
    }

    #[test]
    fn a_colon_in_a_path_is_refused() {
        // The -R spec has no way to quote one.
        assert!(
            target()
                .ctl_tunnel_command("/a:b/s.sock", Path::new("/l/s.sock"))
                .is_err()
        );
        assert!(
            target()
                .ctl_tunnel_command("/a/s.sock", Path::new("/l:x/s.sock"))
                .is_err()
        );
    }

    #[test]
    fn the_prepared_folder_is_private_and_the_socket_is_ours_alone() {
        // The name matches `REMOTE_SOCKET`, and nothing else is removed.
        assert!(PREPARE.contains(REMOTE_SOCKET));
        assert!(PREPARE.contains("chmod 700"));
        assert_eq!(PREPARE.matches("rm -f").count(), 1);
    }

    /// Needs a host this machine can `ssh` to without a prompt, named in
    /// `PILUS_TEST_SSH_HOST`; `LUVUS_SSH` may point at a wrapper that adds
    /// ssh options. It publishes `~/.pilus/pilus-remote.sock` on that host,
    /// so use a host whose account you can spare that name on.
    ///
    /// `cargo test -p pilus-client tunnel -- --ignored`
    #[test]
    #[ignore = "needs an SSH host: set PILUS_TEST_SSH_HOST"]
    fn a_tunnel_carries_a_request_to_this_side_and_back() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::{UnixListener, UnixStream};

        let Some(host) = std::env::var_os("PILUS_TEST_SSH_HOST") else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("pilus-tunnel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let local = dir.join("pilus.sock");
        let listener = UnixListener::bind(&local).expect("bind");
        let serving = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).expect("read");
            (&stream)
                .write_all(format!("{{\"echo\":{}}}\n", line.trim()).as_bytes())
                .expect("write");
        });
        let target = SshTarget {
            destination: host.to_string_lossy().into_owned(),
            binary: "/unused".to_string(),
            session: None,
        };
        let tunnel = CtlTunnel::start(&target, &local).expect("the tunnel comes up");
        // Where the host puts it; on this machine, when the host is this machine.
        let remote = std::path::PathBuf::from(std::env::var("HOME").expect("home"))
            .join(".pilus")
            .join(REMOTE_SOCKET);
        let mut stream = UnixStream::connect(&remote).expect("the host's socket answers");
        stream.write_all(b"{\"cmd\":\"ping\"}\n").expect("send");
        let mut answer = String::new();
        BufReader::new(&stream)
            .read_line(&mut answer)
            .expect("reply");
        assert_eq!(answer.trim(), "{\"echo\":{\"cmd\":\"ping\"}}");
        serving.join().expect("join");
        // Ending the tunnel ends the wait, and says why.
        tunnel.killer().kill();
        assert!(!tunnel.wait_exit().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
