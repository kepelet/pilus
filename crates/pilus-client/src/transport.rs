//! The platform transport under a connection.
//!
//! A display connection reaches its server one of two ways: over the local
//! Unix-domain socket the daemon derives from its state directory, or through
//! the local OpenSSH client, whose stdin/stdout carry the same bytes as that
//! socket. [`Stream`] hides the difference behind one `Read + Write` handle so
//! the handshake and frame loop never learn where the bytes came from.
//!
//! Remote streams own a child process; the last handle to drop terminates the
//! SSH client, which closes its pipes and unblocks the reader thread.

use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

/// Environment override for the OpenSSH client binary.
pub const SSH_ENV: &str = "LUVUS_SSH";
/// Bound on captured stderr, so a chatty host cannot grow without limit.
const MAX_STDERR: usize = 16 * 1024;

/// The OpenSSH client to invoke. `$LUVUS_SSH` wins, then plain `ssh` on `PATH`.
pub fn ssh_binary() -> std::ffi::OsString {
    std::env::var_os(SSH_ENV)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ssh".into())
}

/// A remote host reached through the local OpenSSH client.
///
/// The daemon's saved machine profile supplies all three fields; the command
/// shape matches what the reference client runs. The remote binary is an
/// absolute path so the non-interactive SSH `PATH` cannot pick a stale build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshTarget {
    /// Destination or `~/.ssh/config` alias.
    pub destination: String,
    /// Absolute path to the remote `luvus` binary.
    pub binary: String,
    /// Remote named session; `None` is the remote default session.
    pub session: Option<String>,
}

impl SshTarget {
    /// The SSH client with its connection options, before any remote command.
    fn base(&self) -> Command {
        let mut command = Command::new(ssh_binary());
        command
            .arg("-T")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .arg(&self.destination);
        command
    }

    /// Build the SSH invocation for a remote subcommand of the Luvus binary.
    ///
    /// The options match the reference client: no TTY, batch mode so a
    /// password prompt fails fast instead of hanging the connection worker,
    /// and keepalives so a dead link is noticed.
    pub fn command(&self, tail: &[&str]) -> Command {
        let mut command = self.base();
        command.arg(&self.binary);
        if let Some(session) = self.session.as_deref().filter(|name| !name.is_empty()) {
            command.arg("--session").arg(session);
        }
        command.args(tail);
        command
    }

    /// Build an SSH invocation that runs one shell command on the host.
    ///
    /// `remote_command` is a complete shell string, already quoted for the
    /// remote shell; this is used for host operations the Luvus binary does not
    /// expose, such as listing a directory for the native picker.
    pub fn shell(&self, remote_command: &str) -> Command {
        let mut command = self.base();
        command.arg(remote_command);
        command
    }
}

/// A connected client stream.
pub struct Stream {
    inner: Inner,
}

enum Inner {
    Local(interprocess::local_socket::Stream),
    /// A TCP connection to a UHP access gateway's loopback listener. The
    /// gateway speaks the same newline-delimited UHP frames as the owner
    /// socket, so the rest of the client is unchanged.
    Tcp(std::net::TcpStream),
    Remote(Remote),
}

struct Remote {
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<ChildStdout>,
    /// Kept alive so the process outlives its standard handles, and killed
    /// when the last clone drops.
    child: Arc<ChildGuard>,
    /// Bounded tail of the child's stderr, used to explain early exits.
    stderr: Arc<Mutex<String>>,
}

/// Kills and reaps the child process when the stream is dropped.
struct ChildGuard(Mutex<Child>);

impl ChildGuard {
    fn terminate(&self) {
        if let Ok(mut child) = self.0.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

impl Stream {
    /// Connect to the local socket at `path`.
    ///
    /// Returns [`Error::NotFound`] when nothing is listening, so the caller can
    /// suggest starting the daemon instead of surfacing a raw IO error.
    pub fn connect(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use interprocess::local_socket::prelude::*;

            if !is_socket(path)? {
                return Err(Error::NotFound {
                    path: path.to_path_buf(),
                });
            }
            let name = path
                .to_fs_name::<interprocess::local_socket::GenericFilePath>()
                .map_err(|error| Error::Io(std::io::Error::other(error.to_string())))?;
            let stream = interprocess::local_socket::Stream::connect(name).map_err(|error| {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) {
                    Error::NotFound {
                        path: path.to_path_buf(),
                    }
                } else {
                    Error::Io(error)
                }
            })?;
            Ok(Self {
                inner: Inner::Local(stream),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "named-pipe transport is not implemented yet",
            )))
        }
    }

    /// Connect to a TCP endpoint, e.g. a `luvus uhp access` loopback address.
    ///
    /// Unlike [`Stream::connect`], a refused connection is a plain IO error:
    /// a gateway is an ephemeral process, not the daemon whose absence maps to
    /// [`Error::NotFound`].
    pub fn connect_tcp(address: &str) -> Result<Self> {
        let stream = std::net::TcpStream::connect(address)?;
        Ok(Self {
            inner: Inner::Tcp(stream),
        })
    }

    /// Spawn `command` and bridge its standard IO as a stream.
    ///
    /// Stdin and stdout carry protocol bytes; stderr is drained on a helper
    /// thread so a full pipe cannot stall the child.
    pub fn spawn(command: &mut Command) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            Error::Machine("could not capture the remote command's stdin".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Error::Machine("could not capture the remote command's stdout".to_string())
        })?;
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(pipe) = child.stderr.take() {
            let stderr = Arc::clone(&stderr);
            std::thread::Builder::new()
                .name("pilus-ssh-stderr".to_string())
                .spawn(move || {
                    let mut reader = BufReader::new(pipe);
                    let mut chunk = [0u8; 1024];
                    loop {
                        match reader.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let text = String::from_utf8_lossy(&chunk[..n]);
                                if let Ok(mut guard) = stderr.lock()
                                    && guard.len() < MAX_STDERR
                                {
                                    guard.push_str(&text);
                                }
                            }
                        }
                    }
                })
                .ok();
        }
        Ok(Self {
            inner: Inner::Remote(Remote {
                stdin: Mutex::new(stdin),
                stdout: Mutex::new(stdout),
                child: Arc::new(ChildGuard(Mutex::new(child))),
                stderr,
            }),
        })
    }

    /// The captured stderr tail, when this is a spawned remote stream.
    pub fn stderr_tail(&self) -> Option<String> {
        match &self.inner {
            Inner::Remote(remote) => match remote.stderr.lock() {
                Ok(guard) if !guard.trim().is_empty() => Some(guard.trim().to_string()),
                _ => None,
            },
            Inner::Local(_) | Inner::Tcp(_) => None,
        }
    }

    /// Close the underlying link. Remote streams terminate their SSH child so
    /// a reader blocked on stdout unblocks instead of leaking a thread.
    pub fn terminate(&self) {
        if let Inner::Remote(remote) = &self.inner {
            remote.child.terminate();
        }
    }
}

fn poisoned() -> std::io::Error {
    std::io::Error::other("transport mutex poisoned")
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            Inner::Local(stream) => stream.read(buffer),
            Inner::Tcp(stream) => stream.read(buffer),
            Inner::Remote(remote) => remote.stdout.lock().map_err(|_| poisoned())?.read(buffer),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            Inner::Local(stream) => stream.write(buffer),
            Inner::Tcp(stream) => stream.write(buffer),
            Inner::Remote(remote) => remote.stdin.lock().map_err(|_| poisoned())?.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.inner {
            Inner::Local(stream) => stream.flush(),
            Inner::Tcp(stream) => stream.flush(),
            Inner::Remote(remote) => remote.stdin.lock().map_err(|_| poisoned())?.flush(),
        }
    }
}

impl Read for &Stream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match &self.inner {
            Inner::Local(stream) => (&*stream).read(buffer),
            Inner::Tcp(stream) => (&*stream).read(buffer),
            Inner::Remote(remote) => remote.stdout.lock().map_err(|_| poisoned())?.read(buffer),
        }
    }
}

impl Write for &Stream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match &self.inner {
            Inner::Local(stream) => (&*stream).write(buffer),
            Inner::Tcp(stream) => (&*stream).write(buffer),
            Inner::Remote(remote) => remote.stdin.lock().map_err(|_| poisoned())?.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &self.inner {
            Inner::Local(stream) => (&*stream).flush(),
            Inner::Tcp(stream) => (&*stream).flush(),
            Inner::Remote(remote) => remote.stdin.lock().map_err(|_| poisoned())?.flush(),
        }
    }
}

#[cfg(unix)]
fn is_socket(path: &Path) -> Result<bool> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_socket()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawned_stream_captures_stderr_and_reads_stdout() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("printf oops >&2; printf ok; sleep 1");
        let mut stream = Stream::spawn(&mut command).expect("spawn");
        let mut buffer = [0u8; 2];
        stream.read_exact(&mut buffer).expect("read stdout");
        assert_eq!(&buffer, b"ok");
        // The stderr drain runs on its own thread; give it a beat to land.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(stream.stderr_tail().as_deref(), Some("oops"));
    }

    #[test]
    fn ssh_command_shape_matches_the_reference_client() {
        let target = SshTarget {
            destination: "dev@buildbox".to_string(),
            binary: "/home/dev/.local/bin/luvus".to_string(),
            session: None,
        };
        let command = target.command(&["remote-client-bridge"]);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-T");
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"ConnectTimeout=10".to_string()));
        assert_eq!(args[args.len() - 1], "remote-client-bridge");
        // Destination and binary precede the remote subcommand.
        assert_eq!(args[args.len() - 3], "dev@buildbox");
        assert_eq!(args[args.len() - 2], "/home/dev/.local/bin/luvus");
    }

    #[test]
    fn shell_command_runs_a_host_shell_command() {
        let target = SshTarget {
            destination: "buildbox".to_string(),
            binary: "/usr/local/bin/luvus".to_string(),
            session: None,
        };
        let command = target.shell("find '/tmp' -maxdepth 1 -print0");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.last().expect("remote command"),
            "find '/tmp' -maxdepth 1 -print0"
        );
        // The binary path belongs to `command`, not to a raw shell invocation.
        assert!(!args.contains(&"/usr/local/bin/luvus".to_string()));
    }

    #[test]
    fn named_session_is_passed_before_the_remote_subcommand() {
        let target = SshTarget {
            destination: "buildbox".to_string(),
            binary: "/usr/local/bin/luvus".to_string(),
            session: Some("review".to_string()),
        };
        let command = target.command(&["uhp", "proxy"]);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let session = args
            .iter()
            .position(|arg| arg == "--session")
            .expect("session");
        assert_eq!(args[session + 1], "review");
        assert_eq!(&args[session + 2..], &["uhp", "proxy"]);
    }
}
