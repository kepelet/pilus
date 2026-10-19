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
use std::path::{Path, PathBuf};
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
    /// Note that an SSH command to this host is about to be run. The command is
    /// built here and spawned by the caller, so only the intent is seen.
    fn noted(&self, what: &str) {
        crate::netlog::note(crate::netlog::Kind::Ssh, &self.destination, what);
    }

    /// The SSH client with its connection options, before any remote command.
    fn base(&self) -> Command {
        self.base_with_control_path(None)
    }

    fn base_with_control_path(&self, control_path: Option<&Path>) -> Command {
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
            .arg("ServerAliveCountMax=3");
        if let Some(control_path) = control_path {
            command
                .arg("-o")
                .arg("ControlMaster=auto")
                .arg("-o")
                .arg(format!("ControlPath={}/cm-%C", control_path.display()))
                .arg("-o")
                .arg("ControlPersist=60");
        }
        command.arg(&self.destination);
        command
    }

    /// Build the SSH invocation for a remote subcommand of the Luvus binary.
    ///
    /// The options match the reference client: no TTY, batch mode so a
    /// password prompt fails fast instead of hanging the connection worker,
    /// and keepalives so a dead link is noticed.
    pub fn command(&self, tail: &[&str]) -> Command {
        self.noted(tail.first().copied().unwrap_or("luvus"));
        let mut command = self.base();
        self.append_remote_command(&mut command, tail);
        command
    }

    /// Build a multiplexed SSH invocation for short-lived control requests.
    ///
    /// The render bridge uses [`Self::command`] so it cannot share a master
    /// process with independent control requests.
    pub fn multiplexed_command(&self, tail: &[&str]) -> Command {
        let Some(control_path) = control_dir() else {
            return self.command(tail);
        };
        self.noted(tail.first().copied().unwrap_or("luvus"));
        let mut command = self.base_with_control_path(Some(&control_path));
        self.append_remote_command(&mut command, tail);
        command
    }

    /// OpenSSH joins these words with spaces and the host's login shell splits
    /// them again, so each is quoted for that shell when it needs to be.
    fn append_remote_command(&self, command: &mut Command, tail: &[&str]) {
        command.arg(login_shell_word(&self.binary).as_ref());
        if let Some(session) = self.session.as_deref().filter(|name| !name.is_empty()) {
            command
                .arg("--session")
                .arg(login_shell_word(session).as_ref());
        }
        for word in tail {
            command.arg(login_shell_word(word).as_ref());
        }
    }

    /// Build an SSH invocation that runs one shell command on the host.
    ///
    /// `remote_command` is a complete POSIX shell string; it is quoted for the
    /// host's login shell and handed to `/bin/sh -c`, so it runs the same
    /// whatever that login shell is. The server runs the remote command
    /// through the user's login shell, and fish (a common default) rejects
    /// POSIX expansions like `${VAR:-default}`; pinning `/bin/sh` keeps host
    /// operations such as listing a directory or reading a session file
    /// working on every host.
    pub fn shell(&self, remote_command: &str) -> Command {
        self.noted("shell");
        let mut command = self.base();
        command.arg(posix_command(remote_command));
        command
    }

    /// [`Self::shell`] over the shared master, for a command that repeats: a
    /// poll then costs a round trip instead of a handshake.
    pub fn shell_multiplexed(&self, remote_command: &str) -> Command {
        let Some(control_path) = control_dir() else {
            return self.shell(remote_command);
        };
        self.noted("shell");
        let mut command = self.base_with_control_path(Some(&control_path));
        command.arg(posix_command(remote_command));
        command
    }

    /// The `ssh -N -L` invocation that carries `127.0.0.1:local_port` here to
    /// `remote_host:remote_port` as the host sees it.
    ///
    /// `ControlPath=none` keeps it a process of its own: riding a master a
    /// user's `ssh_config` enables would leave the forward alive after this
    /// process is killed. `ExitOnForwardFailure` makes a refused listener end
    /// the process, which is how a failed forward is noticed.
    pub(crate) fn forward_command(
        &self,
        local_port: u16,
        remote_host: &str,
        remote_port: u16,
    ) -> Command {
        let mut command = Command::new(ssh_binary());
        command
            .arg("-N")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ControlPath=none")
            .arg("-L")
            .arg(format!(
                "127.0.0.1:{local_port}:{remote_host}:{remote_port}"
            ))
            .arg(&self.destination);
        command
    }

    /// Ask OpenSSH to close the shared master without blocking the caller.
    pub fn release_control_master(&self) {
        let destination = self.destination.clone();
        std::thread::spawn(move || {
            let Some(control_path) = control_dir() else {
                return;
            };
            let _ = Command::new(ssh_binary())
                .arg("-o")
                .arg(format!("ControlPath={}/cm-%C", control_path.display()))
                .arg("-O")
                .arg("exit")
                .arg(destination)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    }
}

#[cfg(unix)]
fn control_dir() -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let uid = crate::discovery::current_uid();
    #[cfg(target_os = "macos")]
    let root = "/private/tmp";
    #[cfg(not(target_os = "macos"))]
    let root = "/tmp";
    let path = PathBuf::from(format!("{root}/pilus-{uid}"));
    std::fs::create_dir_all(&path).ok()?;

    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_dir() || metadata.uid() != uid {
        return None;
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).ok()?;
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    (metadata.file_type().is_dir()
        && metadata.uid() == uid
        && metadata.permissions().mode() & 0o077 == 0)
        .then_some(path)
}

#[cfg(not(unix))]
fn control_dir() -> Option<PathBuf> {
    None
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

/// Wrap a host script so it runs under `/bin/sh`, not the login shell.
fn posix_command(remote_command: &str) -> String {
    format!("/bin/sh -c {}", quote_for_login_shell(remote_command))
}

/// Single-quote `value` for the host's login shell, whichever it is.
///
/// `sh` reads a single-quoted run literally, but `fish` treats `\\` and `\'`
/// inside one as escapes, so a value holding `\'` would end the quote early
/// there and the rest would run as a command. Quotes and backslashes are
/// therefore written outside the quoted runs, as `\'` and `\\`, which both
/// shells read the same way. The server's login shell then reconstructs
/// `value` as one argument and passes it to `/bin/sh -c`.
fn quote_for_login_shell(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for c in value.chars() {
        match c {
            '\'' => quoted.push_str("'\\''"),
            '\\' => quoted.push_str("'\\\\'"),
            _ => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

/// One word of a remote command line, quoted only when it needs to be, so the
/// usual binary path and subcommands stay as they were written.
fn login_shell_word(value: &str) -> std::borrow::Cow<'_, str> {
    let plain = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:@%+=,-".contains(&b));
    if plain {
        std::borrow::Cow::Borrowed(value)
    } else {
        std::borrow::Cow::Owned(quote_for_login_shell(value))
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
        assert!(!args.iter().any(|arg| arg.contains("ControlMaster")));
    }

    #[test]
    fn multiplexed_ssh_options_precede_destination_and_remote_command() {
        let target = SshTarget {
            destination: "dev@buildbox".to_string(),
            binary: "/home/dev/.local/bin/luvus".to_string(),
            session: Some("review".to_string()),
        };
        let command = target.multiplexed_command(&["uhp", "proxy"]);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let destination = args
            .iter()
            .position(|arg| arg == "dev@buildbox")
            .expect("destination");
        assert!(
            args[..destination]
                .windows(2)
                .any(|pair| { pair == ["-o", "ControlMaster=auto"] })
        );
        assert!(
            args[..destination]
                .windows(2)
                .any(|pair| pair[0] == "-o" && pair[1].starts_with("ControlPath="))
        );
        assert!(
            args[..destination]
                .windows(2)
                .any(|pair| pair == ["-o", "ControlPersist=60"])
        );
        assert_eq!(
            &args[destination + 1..],
            &[
                "/home/dev/.local/bin/luvus",
                "--session",
                "review",
                "uhp",
                "proxy"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn control_directory_has_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = control_dir().expect("private control directory");
        let metadata = std::fs::metadata(path).expect("control directory metadata");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn shell_runs_the_command_under_a_posix_shell() {
        let target = SshTarget {
            destination: "buildbox".to_string(),
            binary: "/usr/local/bin/luvus".to_string(),
            session: None,
        };
        // A fish login shell rejects `${VAR:-default}`; the wrapper keeps the
        // script for `/bin/sh`, which accepts it.
        let command = target.shell("printf %s \"${PI_DIR:-$HOME/.pi}\"");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.last().expect("remote command"),
            "/bin/sh -c 'printf %s \"${PI_DIR:-$HOME/.pi}\"'"
        );
        // The binary path belongs to `command`, not to a raw shell invocation.
        assert!(!args.contains(&"/usr/local/bin/luvus".to_string()));
    }

    #[test]
    fn login_shell_quoting_round_trips_a_quoted_path() {
        // A script that already quotes its path must survive the outer quote.
        assert_eq!(
            posix_command("find '/tmp/a b' -print0"),
            "/bin/sh -c 'find '\\''/tmp/a b'\\'' -print0'"
        );
    }

    /// Run `script` through `shell` the way the host's login shell would, and
    /// return what `printf` printed. Skips a shell that is not installed.
    fn through(shell: &str, script: &str) -> Option<String> {
        let output = Command::new(shell).arg("-c").arg(script).output().ok()?;
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    #[test]
    fn login_shell_quoting_holds_in_sh_and_fish() {
        // A file name can hold anything; under fish a `\'` used to end the quote
        // early and run the rest.
        for value in [
            "plain",
            "/tmp/a b",
            "it's",
            "a\\b",
            "\\';echo PWNED;#",
            "x\\\\'$(echo PWNED)`echo PWNED`",
            "trailing\\",
            "",
        ] {
            let script = format!("printf '[%s]' {}", quote_for_login_shell(value));
            for shell in ["/bin/sh", "fish", "/opt/homebrew/bin/fish", "/usr/bin/fish"] {
                if let Some(out) = through(shell, &script) {
                    assert_eq!(
                        out,
                        format!("[{value}]"),
                        "{shell} read {value:?} back wrong"
                    );
                }
            }
        }
    }

    #[test]
    fn a_remote_command_word_is_quoted_only_when_it_needs_it() {
        assert_eq!(
            login_shell_word("/usr/local/bin/luvus"),
            "/usr/local/bin/luvus"
        );
        assert_eq!(login_shell_word("uhp"), "uhp");
        assert_eq!(
            login_shell_word("/Users/me/My Tools/luvus"),
            "'/Users/me/My Tools/luvus'"
        );
        assert_eq!(login_shell_word("a;b"), "'a;b'");
        assert_eq!(login_shell_word(""), "''");
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
