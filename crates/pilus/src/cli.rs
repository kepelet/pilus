//! Command-line entry point and launch modes.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use pilus_client::ConnectTarget;

/// How the process was asked to run.
pub enum Launch {
    /// Open the desktop window.
    Gui(ConnectTarget),
    /// Connect once, then print the surface and exit. `workspaces` also claims
    /// the workspaces dock; `open_menu` drives the Settings chord.
    Probe {
        /// Which daemon to attach to.
        target: ConnectTarget,
        /// Whether to claim the client-owned workspaces dock.
        workspaces: bool,
        /// Whether to send the prefix chord that opens Settings.
        open_menu: bool,
    },
    /// Stream one pane through UHP and print the parsed screen. `pane` selects
    /// a pane id, or `None` for the first live terminal.
    Uhp {
        /// Which daemon to attach to.
        target: ConnectTarget,
        /// Pane id to observe, or `None` for the first live terminal.
        pane: Option<String>,
    },
    /// Run the background feed for a few seconds and print the snapshots it
    /// forwards, so the event-driven refresh can be checked without a window.
    Feed {
        /// Which daemon to attach to.
        target: ConnectTarget,
    },
    /// Check the OTA release source and print the result.
    CheckUpdates {
        /// Channel override (`stable`/`preview`); `None` uses settings/env.
        channel: Option<String>,
        /// Feed URL override; `None` uses settings/env or the channel default.
        feed_url: Option<String>,
    },
    /// Attach to one saved machine over SSH and print its workspace catalog.
    ProbeMachine {
        /// Stable machine profile id.
        id: String,
    },
    /// Play every notification cue once, so the audio path can be heard
    /// without waiting for an agent to block.
    TestSound,
    /// Capture headless screenshots of every native surface (development aid).
    #[cfg(feature = "visual-test")]
    UiShots {
        /// Also capture the main window against the local daemon.
        main: bool,
    },
}

/// Parse arguments and run the requested mode.
pub fn run() -> Result<()> {
    let launch = parse(std::env::args().skip(1))?;
    match launch {
        Launch::Gui(target) => run_gui(target),
        Launch::Probe {
            target,
            workspaces,
            open_menu,
        } => run_probe(target, workspaces, open_menu),
        Launch::Uhp { target, pane } => run_uhp(target, pane),
        Launch::Feed { target } => run_feed(target),
        Launch::CheckUpdates { channel, feed_url } => run_check_updates(channel, feed_url),
        Launch::ProbeMachine { id } => run_probe_machine(id),
        Launch::TestSound => run_test_sound(),
        #[cfg(feature = "visual-test")]
        Launch::UiShots { main } => run_ui_shots(main),
    }
}

/// Parse `pilus` arguments.
pub fn parse(args: impl Iterator<Item = String>) -> Result<Launch> {
    let mut target = ConnectTarget::local();
    let mut probe = false;
    let mut workspaces = false;
    let mut open_menu = false;
    let mut uhp = false;
    let mut feed = false;
    let mut check_updates = false;
    #[cfg(feature = "visual-test")]
    let (mut ui_shots, mut ui_shots_main) = (false, false);
    let mut machine_id = None;
    let mut pane_id = None;
    let mut channel = None;
    let mut feed_url = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                let path = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--socket needs a path"))?;
                target = ConnectTarget::socket(path);
            }
            "--session" => {
                let name = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--session needs a name"))?;
                target = ConnectTarget::session(name);
            }
            "--probe" => probe = true,
            "--probe-workspaces" => {
                probe = true;
                workspaces = true;
            }
            "--probe-menu" => {
                probe = true;
                workspaces = true;
                open_menu = true;
            }
            "--probe-uhp" => uhp = true,
            "--probe-feed" => feed = true,
            "--probe-machine" => {
                machine_id = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--probe-machine needs an id"))?,
                );
            }
            "--check-updates" => check_updates = true,
            "--test-sound" => return Ok(Launch::TestSound),
            #[cfg(feature = "visual-test")]
            "--ui-shots" => ui_shots = true,
            #[cfg(feature = "visual-test")]
            "--ui-shots-main" => {
                ui_shots = true;
                ui_shots_main = true;
            }
            "--channel" => {
                channel = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--channel needs a name"))?,
                );
            }
            "--feed-url" => {
                feed_url = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--feed-url needs a URL"))?,
                );
            }
            "--pane" => {
                pane_id = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--pane needs an id"))?,
                );
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unrecognized argument: {other}"),
        }
    }
    if check_updates {
        return Ok(Launch::CheckUpdates { channel, feed_url });
    }
    #[cfg(feature = "visual-test")]
    if ui_shots {
        return Ok(Launch::UiShots {
            main: ui_shots_main,
        });
    }
    if channel.is_some() || feed_url.is_some() {
        bail!(
            "--channel and --feed-url apply to --check-updates; for the window, \
             set PILUS_UPDATE_CHANNEL / PILUS_UPDATE_FEED_URL instead"
        );
    }
    if let Some(id) = machine_id {
        return Ok(Launch::ProbeMachine { id });
    }
    if uhp {
        return Ok(Launch::Uhp {
            target,
            pane: pane_id,
        });
    }
    if feed {
        return Ok(Launch::Feed { target });
    }
    if probe {
        Ok(Launch::Probe {
            target,
            workspaces,
            open_menu,
        })
    } else {
        Ok(Launch::Gui(target))
    }
}

fn print_help() {
    println!(
        "pilus — native desktop client for a Luvus daemon\n\n\
         USAGE:\n    pilus [--session NAME | --socket PATH] [--probe]\n\n\
         OPTIONS:\n\
         \x20   --session NAME   Attach to a named Luvus session\n\
         \x20   --socket PATH    Attach to an explicit client-socket path\n\
         \x20   --probe          Connect once and print the first frame\n\
         \x20   --probe-workspaces  Probe while claiming the workspaces dock\n\
         \x20   --probe-menu     Probe, then open the daemon's Menu by click\n\
         \x20   --probe-uhp      Stream one pane via UHP and print the screen\n\
         \x20   --probe-feed     Run the event-driven feed briefly and print it\n\
         \x20   --probe-machine ID  Attach to a saved machine over SSH and print it\n\
         \x20   --check-updates  Check the OTA release source and print it\n\
         \x20   --channel NAME   Channel for --check-updates (stable or preview)\n\
         \x20   --feed-url URL   Static release manifest for --check-updates\n\
         \x20   --pane ID        Pane to observe with --probe-uhp\n\
         \x20   --test-sound     Play every notification cue and exit\n\
         \x20   -h, --help       Print this help"
    );
}

/// Play every notification cue once, then exit.
///
/// The daemon only sends a cue when an agent blocks or finishes, which makes
/// the audio hard to exercise on demand. This drives the same client player
/// directly so a missing cue can be diagnosed without a live agent.
#[cfg(feature = "gui")]
fn run_test_sound() -> Result<()> {
    use pilus_protocol::{SoundCue, SoundSignal, SoundStyle};
    for style in [SoundStyle::Soft, SoundStyle::Retro, SoundStyle::Pulse] {
        for cue in [SoundCue::Done, SoundCue::Blocked] {
            println!("{style:?} · {cue:?}");
            crate::gui::sound::play(SoundSignal { cue, style });
            std::thread::sleep(Duration::from_millis(700));
        }
    }
    Ok(())
}

/// `--test-sound` needs the window player, which the gui feature provides.
#[cfg(not(feature = "gui"))]
fn run_test_sound() -> Result<()> {
    bail!("--test-sound needs the gui feature")
}

/// Capture headless UI screenshots; a development aid behind `visual-test`.
#[cfg(feature = "visual-test")]
fn run_ui_shots(main: bool) -> Result<()> {
    if main {
        crate::gui::shots::run_main()
    } else {
        crate::gui::shots::run()
    }
}

/// Attach to a saved machine over SSH and print its identity and workspaces.
///
/// The headless proof of the remote path: locate the profile, launch the same
/// `remote-client-bridge` the window uses, claim the workspaces dock, and fold
/// the catalog the remote server projects.
fn run_probe_machine(id: String) -> Result<()> {
    let root = pilus_client::config_dir();
    let machine = pilus_client::list_machines(&root)
        .into_iter()
        .find(|machine| machine.id == id)
        .ok_or_else(|| anyhow::anyhow!("no saved machine `{id}`"))?;
    println!(
        "machine {} → {} (binary {})",
        machine.id, machine.destination, machine.remote_binary
    );
    let connection = pilus_client::connect_machine(&machine, 120, 40)?;
    connection.request_workspace_dock()?;
    let identity = connection.identity();
    println!(
        "attached: boot {} session {}",
        identity.boot_id, identity.session
    );

    let mut surface = pilus_client::Surface::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && surface.workspaces().is_empty() {
        match connection.recv_timeout(Duration::from_millis(200)) {
            Some(pilus_client::ClientEvent::Message(message)) => {
                surface.apply(&message);
            }
            Some(pilus_client::ClientEvent::Disconnected { reason }) => {
                bail!("disconnected: {reason}");
            }
            _ => {}
        }
    }
    let workspaces = surface.workspaces();
    println!("{} workspace(s):", workspaces.len());
    for workspace in workspaces {
        println!(
            "  {:<4} {:<28} {}",
            workspace.index, workspace.name, workspace.cwd
        );
    }
    Ok(())
}

/// Stream one pane through UHP and print the parsed screen.
///
/// The headless proof of the UHP render path: inventory the live PTYs, observe
/// one of them, parse the first frame's styled text, and print its rows. The
/// daemon does the VT emulation, so the frame arrives as finished rows and no
/// terminal emulator is needed here.
fn run_uhp(target: ConnectTarget, pane: Option<String>) -> Result<()> {
    pilus_client::ensure_server(&target)?;
    let socket = target
        .socket
        .clone()
        .unwrap_or_else(|| pilus_client::api_socket_path_for(target.session.as_deref()));
    let mut client = pilus_client::UhpClient::connect(&socket);
    let (generation, terminals) = client.inventory()?;
    println!(
        "uhp: {} live terminal(s) at {}",
        terminals.len(),
        socket.display()
    );
    for terminal in &terminals {
        let id: String = terminal.terminal_id.chars().take(8).collect();
        println!(
            "  pane {:<4} {id:<8} {:<28} {}",
            terminal.pane_id, terminal.title, terminal.cwd
        );
    }
    let selected = match pane {
        Some(id) => terminals
            .iter()
            .find(|terminal| terminal.pane_id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no live terminal in pane {id}"))?,
        None => terminals
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no live terminals to observe"))?,
    };
    let mut observer = client.observe(&generation, &selected, 60, true)?;
    let frame = observer
        .next_frame()?
        .ok_or_else(|| anyhow::anyhow!("the observe stream ended before a frame"))?;
    println!(
        "frame rev {} lines {} bytes {} truncated {}",
        frame.revision,
        frame.lines,
        frame.text.len(),
        frame.truncated
    );
    let screen = pilus_client::ansi::parse(&frame.text, frame.lines);
    for (index, row) in screen.plain().iter().enumerate() {
        println!("{:>3}|{}", index + 1, row.trim_end());
    }
    Ok(())
}

/// Run the background feed briefly and print what it forwards.
///
/// The feed is normally invisible: the window drains it during its render
/// pass. This drives the same `AgentFeed` headlessly so the event-driven
/// refresh can be checked from a terminal.
fn run_feed(target: ConnectTarget) -> Result<()> {
    pilus_client::ensure_server(&target)?;
    let socket = target
        .socket
        .clone()
        .unwrap_or_else(|| pilus_client::api_socket_path_for(target.session.as_deref()));
    let feed = pilus_client::AgentFeed::spawn(socket.clone());
    println!("feed: watching {}", socket.display());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut updates = 0;
    while Instant::now() < deadline {
        if let Some(update) = feed.try_recv() {
            updates += 1;
            let agents: Vec<String> = update
                .agents
                .iter()
                .map(|agent| format!("{}/{}", agent.agent, agent.status))
                .collect();
            let tabs: Vec<String> = update
                .tabs
                .iter()
                .map(|tab| format!("{}{}", tab.label(), if tab.active { "*" } else { "" }))
                .collect();
            println!(
                "update {updates}: agents [{}] bar [{}] tabs [{}]",
                agents.join(", "),
                update.bar.len(),
                tabs.join(", ")
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("{updates} update(s) in 3s");
    Ok(())
}

/// Check the OTA release source without touching the GUI.
///
/// Reads the same `settings.json` the window does, then layers `--channel` /
/// `--feed-url` on top; `PILUS_UPDATE_CHANNEL` / `PILUS_UPDATE_FEED_URL` sit
/// between the two. Uses the same engine the window drives, so a failing
/// preview feed or a mismatched asset name shows up here first.
fn run_check_updates(channel: Option<String>, feed_url: Option<String>) -> Result<()> {
    let settings = crate::settings::Settings::load(&pilus_client::pilus_config_dir());
    let mut request = crate::update::UpdateRequest::from_settings(
        &settings.update_channel,
        settings.update_feed_url.clone(),
    );
    if let Some(channel) = channel {
        request.channel = crate::update::UpdateChannel::parse(&channel);
    }
    if let Some(url) = feed_url {
        request.feed_url = Some(url);
    }
    println!("channel: {}", request.channel.label());
    println!("source: {}", crate::update::describe_source(&request));
    let engine = crate::update::engine(&request);
    match engine.check() {
        Ok(Some(release)) => {
            println!(
                "update available: {} ({})",
                release.version, release.asset.name
            );
            if let Some(notes) = release.notes.filter(|notes| !notes.trim().is_empty()) {
                let first = notes.lines().next().unwrap_or("");
                println!("notes: {first}");
            }
            println!("{}", crate::update::PROTOCOL_NOTE);
        }
        Ok(None) => println!("up to date ({})", crate::update::current_version()),
        Err(error) => bail!("update check failed: {error}"),
    }
    Ok(())
}

#[cfg(feature = "gui")]
fn run_gui(target: ConnectTarget) -> Result<()> {
    crate::gui::run(target)
}

#[cfg(not(feature = "gui"))]
fn run_gui(_target: ConnectTarget) -> Result<()> {
    bail!("this build has no GUI; rebuild with --features gui")
}

fn run_probe(target: ConnectTarget, workspaces: bool, open_menu: bool) -> Result<()> {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    let socket = target.resolve();
    let connection = pilus_client::connect(&target, 159, 43)?; // TEMP wide probe
    if workspaces {
        connection.request_workspace_dock()?;
    }
    let mut clicked_menu = false;

    println!("attached to {} ({})", socket.display(), target.label());
    let identity = connection.identity();
    println!(
        "server boot {} session {}",
        identity.boot_id, identity.session
    );

    if workspaces {
        let api = pilus_client::api_socket_path_for(target.session.as_deref());
        match pilus_client::ControlClient::connect(&api) {
            Ok(mut client) => {
                match client.agents() {
                    Ok(agents) => {
                        println!("control API agents: {}", agents.len());
                        for agent in &agents {
                            println!(
                                "  {} {:<8} {:<16} {}",
                                agent.status,
                                agent.agent,
                                agent.label(),
                                agent.workspace_name,
                            );
                        }
                    }
                    Err(error) => println!("control API agents error: {error}"),
                }
                match client.bar_widgets() {
                    Ok(bar) => println!("control API bar widgets: {}", bar.len()),
                    Err(error) => println!("control API bar error: {error}"),
                }
                match client.tabs() {
                    Ok(tabs) => {
                        println!("control API tabs: {}", tabs.len());
                        for tab in &tabs {
                            println!(
                                "  {} {:<12} {}",
                                tab.index,
                                tab.label(),
                                if tab.active { "[active]" } else { "" },
                            );
                        }
                    }
                    Err(error) => println!("control API tabs error: {error}"),
                }
            }
            Err(error) => println!("control API connect error: {error}"),
        }
    }

    let mut surface = pilus_client::Surface::new();
    let mut took_sidebars = false;
    let deadline = Instant::now() + Duration::from_secs(if workspaces { 3 } else { 5 });
    while Instant::now() < deadline {
        match connection.recv_timeout(Duration::from_millis(200)) {
            Some(pilus_client::ClientEvent::Message(message)) => {
                match &message {
                    pilus_protocol::ServerMessage::ShellWorkspaces(list) => {
                        println!("workspace projection: {} entries", list.len());
                        for workspace in list {
                            println!(
                                "  {} {:<16} {} {}{}",
                                workspace.dot,
                                workspace.name,
                                workspace.branch.as_deref().unwrap_or("-"),
                                workspace.cwd,
                                if workspace.active { "  [active]" } else { "" },
                            );
                        }
                    }
                    pilus_protocol::ServerMessage::ShellDock(Some(dock)) => {
                        println!(
                            "dock rect: x={} y={} w={} h={} paths={}",
                            dock.x, dock.y, dock.width, dock.height, dock.show_paths
                        );
                        println!(
                            "  session slot: {:?} button: {:?}",
                            dock.session_slot, dock.session_button
                        );
                        if open_menu && !clicked_menu {
                            let cell = (dock.width.saturating_sub(4), 0);
                            println!("-> click Menu at {cell:?}");
                            for kind in [
                                MouseEventKind::Down(MouseButton::Left),
                                MouseEventKind::Up(MouseButton::Left),
                            ] {
                                let _ = connection.send(pilus_protocol::ClientMessage::Mouse(
                                    MouseEvent {
                                        kind,
                                        column: cell.0,
                                        row: cell.1,
                                        modifiers: KeyModifiers::NONE,
                                    },
                                ));
                            }
                            clicked_menu = true;
                        }
                    }
                    pilus_protocol::ServerMessage::ShellDock(None) => {
                        println!("dock rect: none");
                    }
                    pilus_protocol::ServerMessage::ShellSidebars(sidebars) => {
                        println!(
                            "sidebars: left docks {:?} width {} visible {}",
                            sidebars.layout.left.docks,
                            sidebars.layout.left.width,
                            sidebars.layout.left.visible,
                        );
                        if workspaces && !took_sidebars {
                            let mut layout = sidebars.layout.clone();
                            without_dock(&mut layout.left, "agents");
                            without_dock(&mut layout.right, "agents");
                            let taken = pilus_protocol::ShellSidebars {
                                revision: sidebars.revision.saturating_add(1),
                                layout,
                                workspace_paths: sidebars.workspace_paths,
                            };
                            let _ =
                                connection.send(pilus_protocol::ClientMessage::ShellSidebars(taken));
                            took_sidebars = true;
                            println!("-> sent sidebar takeover (agents dock removed)");
                        }
                    }
                    _ => {}
                }
                surface.apply(&message);
                if !workspaces && surface.frame().is_some() {
                    break;
                }
            }
            Some(pilus_client::ClientEvent::Disconnected { reason }) => {
                bail!("disconnected: {reason}")
            }
            _ => {}
        }
    }
    if let Some(frame) = surface.frame() {
        print_frame(frame);
    }
    Ok(())
}

/// Remove one dock id and its parallel weight from a sidebar config.
fn without_dock(side: &mut pilus_protocol::SideConfig, name: &str) {
    if let Some(index) = side.docks.iter().position(|dock| dock == name) {
        side.docks.remove(index);
        if index < side.dock_weights.len() {
            side.dock_weights.remove(index);
        }
    }
}

fn print_frame(frame: &pilus_protocol::FrameData) {
    for row in 0..frame.height {
        let start = usize::from(row) * usize::from(frame.width);
        let end = start + usize::from(frame.width);
        let line: String = frame.cells[start..end]
            .iter()
            .map(|cell| {
                if cell.symbol.is_empty() {
                    " "
                } else {
                    cell.symbol.as_str()
                }
            })
            .collect();
        println!("{}", line.trim_end());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Launch> {
        parse(args.iter().map(ToString::to_string))
    }

    #[test]
    fn check_updates_takes_channel_and_feed_overrides() {
        let launch = parse_args(&[
            "--check-updates",
            "--channel",
            "nightly",
            "--feed-url",
            "http://127.0.0.1:8000/latest.json",
        ])
        .expect("parse");
        match launch {
            Launch::CheckUpdates { channel, feed_url } => {
                assert_eq!(channel.as_deref(), Some("nightly"));
                assert_eq!(
                    feed_url.as_deref(),
                    Some("http://127.0.0.1:8000/latest.json")
                );
            }
            _ => panic!("expected CheckUpdates"),
        }
    }

    #[test]
    fn channel_overrides_require_check_updates() {
        assert!(parse_args(&["--channel", "preview"]).is_err());
        assert!(parse_args(&["--feed-url", "http://127.0.0.1:8000/latest.json"]).is_err());
    }

    #[test]
    fn a_bare_check_updates_uses_settings() {
        let launch = parse_args(&["--check-updates"]).expect("parse");
        match launch {
            Launch::CheckUpdates { channel, feed_url } => {
                assert!(channel.is_none());
                assert!(feed_url.is_none());
            }
            _ => panic!("expected CheckUpdates"),
        }
    }
}
