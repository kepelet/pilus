//! Running the connector as a background service.
//!
//! "Remote access" means the phone can reach this machine whether or not the
//! pilus window is open, so the connector cannot be a child of the window. On
//! macOS it is a per-user LaunchAgent, on Linux a user systemd unit; both start
//! at login, restart on failure, and are removed again when the user turns
//! remote access off. Nothing here needs root.
//!
//! [`install`] is idempotent and deliberately gentle: the window calls it on
//! every launch, and a running service whose definition has not changed is left
//! alone, because bouncing it would drop every connected device.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, Result};

/// The launchd label and systemd unit stem.
pub const LABEL: &str = "dev.pilus.agent";

/// What to run.
#[derive(Clone, Debug)]
pub struct Spec {
    /// The `pilus-agent` binary the service executes.
    pub agent: PathBuf,
    /// A named Luvus session to expose, or `None` for the default.
    pub session: Option<String>,
    /// The `luvus` binary, recorded so a service's minimal environment still
    /// finds it. The agent rediscovers it if this path later disappears.
    pub luvus: Option<PathBuf>,
    /// The state root (`LUVUS_HOME`), when it is not the default.
    pub home: Option<PathBuf>,
    /// Where the service writes its log.
    pub log: PathBuf,
}

/// The result of [`install`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Installed {
    /// The service was written and started.
    Started,
    /// An identical, running service was left as it was.
    Unchanged,
}

/// Whether, and how, the service exists on this machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    /// This platform has no service support here.
    Unsupported,
    /// No service definition is installed.
    NotInstalled,
    /// Installed; `running` says whether the supervisor has it up right now.
    Installed {
        /// The service manager reports the process as running.
        running: bool,
    },
}

/// The arguments the service passes to the agent.
pub fn agent_arguments(spec: &Spec) -> Vec<String> {
    let mut args = vec![
        "serve".to_string(),
        "--iroh".to_string(),
        "--control".to_string(),
        "--service".to_string(),
    ];
    if let Some(session) = &spec.session {
        args.push("--session".to_string());
        args.push(session.clone());
    }
    args
}

/// The `PATH` a service gets. Launchd and systemd hand services almost
/// nothing, and the agent needs `luvus` and `ssh` to be found.
pub fn service_path(spec: &Spec, home: &Path) -> String {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = spec.luvus.as_deref().and_then(Path::parent) {
        dirs.push(parent.to_path_buf());
    }
    dirs.push(home.join(".local/bin"));
    dirs.push(home.join(".cargo/bin"));
    for fixed in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        dirs.push(PathBuf::from(fixed));
    }
    dirs.dedup();
    dirs.iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(":")
}

/// Install (or refresh) and start the service.
pub fn install(spec: &Spec) -> Result<Installed> {
    platform::install(spec)
}

/// Stop and remove the service. Not being installed is not an error.
pub fn uninstall() -> Result<()> {
    platform::uninstall()
}

/// Whether the service is installed and running.
pub fn state() -> State {
    platform::state()
}

fn run(command: &mut Command) -> Result<std::process::Output> {
    let output = command.output()?;
    Ok(output)
}

fn failed(what: &str, output: &std::process::Output) -> Error {
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    Error::Service(if detail.is_empty() {
        format!("{what} failed ({})", output.status)
    } else {
        format!("{what} failed: {detail}")
    })
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn plist_path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::Service("HOME is not set".to_string()))?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    fn domain() -> String {
        format!("gui/{}", pilus_client::current_uid())
    }

    fn target() -> String {
        format!("{}/{LABEL}", domain())
    }

    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    /// The LaunchAgent definition for `spec`.
    pub(super) fn plist(spec: &Spec, home: &Path) -> String {
        let mut arguments = format!(
            "    <string>{}</string>\n",
            escape(&spec.agent.display().to_string())
        );
        for argument in agent_arguments(spec) {
            arguments.push_str(&format!("    <string>{}</string>\n", escape(&argument)));
        }
        let mut environment = format!(
            "    <key>PATH</key><string>{}</string>\n",
            escape(&service_path(spec, home))
        );
        if let Some(luvus) = &spec.luvus {
            environment.push_str(&format!(
                "    <key>LUVUS_BIN</key><string>{}</string>\n",
                escape(&luvus.display().to_string())
            ));
        }
        if let Some(root) = &spec.home {
            environment.push_str(&format!(
                "    <key>LUVUS_HOME</key><string>{}</string>\n",
                escape(&root.display().to_string())
            ));
        }
        let log = escape(&spec.log.display().to_string());
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key><string>{LABEL}</string>\n\
  <key>ProgramArguments</key>\n  <array>\n{arguments}  </array>\n\
  <key>EnvironmentVariables</key>\n  <dict>\n{environment}  </dict>\n\
  <key>RunAtLoad</key><true/>\n\
  <key>KeepAlive</key><true/>\n\
  <key>ThrottleInterval</key><integer>5</integer>\n\
  <key>ProcessType</key><string>Background</string>\n\
  <key>StandardOutPath</key><string>{log}</string>\n\
  <key>StandardErrorPath</key><string>{log}</string>\n\
</dict>\n</plist>\n"
        )
    }

    fn loaded() -> bool {
        Command::new("launchctl")
            .arg("print")
            .arg(target())
            .output()
            .is_ok_and(|output| output.status.success())
    }

    pub(super) fn install(spec: &Spec) -> Result<Installed> {
        let path = plist_path()?;
        let home = path
            .ancestors()
            .nth(3)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let wanted = plist(spec, &home);
        let current = std::fs::read_to_string(&path).ok();
        if current.as_deref() == Some(wanted.as_str()) && loaded() {
            return Ok(Installed::Unchanged);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = spec.log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, wanted)?;
        // `bootout` is asynchronous and `bootstrap` fails while the old job is
        // still unloading, so retry briefly instead of reporting a race.
        let _ = run(Command::new("launchctl").arg("bootout").arg(target()));
        let mut last = None;
        for _ in 0..10 {
            let output = run(Command::new("launchctl")
                .arg("bootstrap")
                .arg(domain())
                .arg(&path))?;
            if output.status.success() {
                return Ok(Installed::Started);
            }
            last = Some(output);
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        match last {
            Some(output) => Err(failed("launchctl bootstrap", &output)),
            None => Err(Error::Service(
                "launchctl bootstrap did not run".to_string(),
            )),
        }
    }

    pub(super) fn uninstall() -> Result<()> {
        let _ = run(Command::new("launchctl").arg("bootout").arg(target()));
        match std::fs::remove_file(plist_path()?) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    pub(super) fn state() -> State {
        let Ok(path) = plist_path() else {
            return State::Unsupported;
        };
        if !path.exists() {
            return State::NotInstalled;
        }
        let running = Command::new("launchctl")
            .arg("print")
            .arg(target())
            .output()
            .map(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("state = running")
            })
            .unwrap_or(false);
        State::Installed { running }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_plist_runs_the_agent_as_a_restarting_login_service() {
            let spec = Spec {
                agent: PathBuf::from("/Applications/pilus.app/Contents/MacOS/pilus-agent"),
                session: Some("work & play".to_string()),
                luvus: Some(PathBuf::from("/opt/homebrew/bin/luvus")),
                home: None,
                log: PathBuf::from("/Users/me/.luvus/agent/agent.log"),
            };
            let text = plist(&spec, Path::new("/Users/me"));
            assert!(text.contains("<key>RunAtLoad</key><true/>"));
            assert!(text.contains("<key>KeepAlive</key><true/>"));
            assert!(text.contains("<string>--service</string>"));
            assert!(
                text.contains("<string>work &amp; play</string>"),
                "arguments are escaped"
            );
            assert!(text.contains("<key>LUVUS_BIN</key><string>/opt/homebrew/bin/luvus</string>"));
            assert!(
                !text.contains("LUVUS_HOME"),
                "default state root is not pinned"
            );
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    fn unit_path() -> Result<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or_else(|| Error::Service("HOME is not set".to_string()))?;
        Ok(config.join("systemd/user").join(format!("{LABEL}.service")))
    }

    fn quote(text: &str) -> String {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// The user unit for `spec`.
    pub(super) fn unit(spec: &Spec, home: &Path) -> String {
        let mut exec = quote(&spec.agent.display().to_string());
        for argument in agent_arguments(spec) {
            exec.push(' ');
            exec.push_str(&quote(&argument));
        }
        let mut environment = format!("Environment=\"PATH={}\"\n", service_path(spec, home));
        if let Some(luvus) = &spec.luvus {
            environment.push_str(&format!("Environment=\"LUVUS_BIN={}\"\n", luvus.display()));
        }
        if let Some(root) = &spec.home {
            environment.push_str(&format!("Environment=\"LUVUS_HOME={}\"\n", root.display()));
        }
        format!(
            "[Unit]\nDescription=pilus remote access\nAfter=network-online.target\n\n\
[Service]\nExecStart={exec}\n{environment}Restart=always\nRestartSec=5\n\
StandardOutput=append:{log}\nStandardError=append:{log}\n\n\
[Install]\nWantedBy=default.target\n",
            log = spec.log.display()
        )
    }

    fn systemctl(args: &[&str]) -> Result<std::process::Output> {
        run(Command::new("systemctl").arg("--user").args(args))
    }

    pub(super) fn install(spec: &Spec) -> Result<Installed> {
        let path = unit_path()?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let wanted = unit(spec, &home);
        let unchanged = std::fs::read_to_string(&path).ok().as_deref() == Some(wanted.as_str());
        let active =
            systemctl(&["is-active", "--quiet", LABEL]).is_ok_and(|out| out.status.success());
        if unchanged && active {
            return Ok(Installed::Unchanged);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = spec.log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, wanted)?;
        for args in [
            vec!["daemon-reload"],
            vec!["enable", "--now", LABEL],
            vec!["restart", LABEL],
        ] {
            let output = systemctl(&args)?;
            if !output.status.success() {
                return Err(failed(&format!("systemctl {}", args.join(" ")), &output));
            }
        }
        Ok(Installed::Started)
    }

    pub(super) fn uninstall() -> Result<()> {
        let _ = systemctl(&["disable", "--now", LABEL]);
        match std::fs::remove_file(unit_path()?) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let _ = systemctl(&["daemon-reload"]);
        Ok(())
    }

    pub(super) fn state() -> State {
        let Ok(path) = unit_path() else {
            return State::Unsupported;
        };
        if !path.exists() {
            return State::NotInstalled;
        }
        let running =
            systemctl(&["is-active", "--quiet", LABEL]).is_ok_and(|out| out.status.success());
        State::Installed { running }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::*;

    pub(super) fn install(_spec: &Spec) -> Result<Installed> {
        Err(Error::Service(
            "background service is only supported on macOS and Linux".to_string(),
        ))
    }

    pub(super) fn uninstall() -> Result<()> {
        Ok(())
    }

    pub(super) fn state() -> State {
        State::Unsupported
    }
}
