//! Command-line entry point and launch modes.

use anyhow::{Result, bail};
use l3s_client::ConnectTarget;

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
    }
}

/// Parse `l3s` arguments.
pub fn parse(args: impl Iterator<Item = String>) -> Result<Launch> {
    let mut target = ConnectTarget::local();
    let mut probe = false;
    let mut workspaces = false;
    let mut open_menu = false;
    let mut uhp = false;
    let mut feed = false;
    let mut pane_id = None;
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
        "l3s — native desktop client for a Luvus daemon\n\n\
         USAGE:\n    l3s [--session NAME | --socket PATH] [--probe]\n\n\
         OPTIONS:\n\
         \x20   --session NAME   Attach to a named Luvus session\n\
         \x20   --socket PATH    Attach to an explicit client-socket path\n\
         \x20   --probe          Connect once and print the first frame\n\
         \x20   --probe-workspaces  Probe while claiming the workspaces dock\n\
         \x20   --probe-menu     Probe, then open the daemon's Menu by click\n\
         \x20   --probe-uhp      Stream one pane via UHP and print the screen\n\
         \x20   --probe-feed     Run the event-driven feed briefly and print it\n\
         \x20   --pane ID        Pane to observe with --probe-uhp\n\
         \x20   -h, --help       Print this help"
    );
}

/// Stream one pane through UHP and print the parsed screen.
///
/// The headless proof of the UHP render path: inventory the live PTYs, observe
/// one of them, parse the first frame's styled text, and print its rows. The
/// daemon does the VT emulation, so the frame arrives as finished rows and no
/// terminal emulator is needed here.
fn run_uhp(target: ConnectTarget, pane: Option<String>) -> Result<()> {
    let socket = target
        .socket
        .clone()
        .unwrap_or_else(|| l3s_client::api_socket_path_for(target.session.as_deref()));
    let mut client = l3s_client::UhpClient::connect(&socket);
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
    let screen = l3s_client::ansi::parse(&frame.text, frame.lines);
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
    let socket = target
        .socket
        .clone()
        .unwrap_or_else(|| l3s_client::api_socket_path_for(target.session.as_deref()));
    let feed = l3s_client::AgentFeed::spawn(socket.clone());
    println!("feed: watching {}", socket.display());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut updates = 0;
    while std::time::Instant::now() < deadline {
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
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("{updates} update(s) in 3s");
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
    let connection = l3s_client::connect(&target, 80, 24)?;
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
        let api = l3s_client::api_socket_path_for(target.session.as_deref());
        match l3s_client::ControlClient::connect(&api) {
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

    let mut surface = l3s_client::Surface::new();
    let mut took_sidebars = false;
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(if workspaces { 3 } else { 5 });
    while std::time::Instant::now() < deadline {
        match connection.recv_timeout(std::time::Duration::from_millis(200)) {
            Some(l3s_client::ClientEvent::Message(message)) => {
                match &message {
                    l3s_protocol::ServerMessage::ShellWorkspaces(list) => {
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
                    l3s_protocol::ServerMessage::ShellDock(Some(dock)) => {
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
                                let _ = connection.send(l3s_protocol::ClientMessage::Mouse(
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
                    l3s_protocol::ServerMessage::ShellDock(None) => {
                        println!("dock rect: none");
                    }
                    l3s_protocol::ServerMessage::ShellSidebars(sidebars) => {
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
                            let taken = l3s_protocol::ShellSidebars {
                                revision: sidebars.revision.saturating_add(1),
                                layout,
                                workspace_paths: sidebars.workspace_paths,
                            };
                            let _ =
                                connection.send(l3s_protocol::ClientMessage::ShellSidebars(taken));
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
            Some(l3s_client::ClientEvent::Disconnected { reason }) => {
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
fn without_dock(side: &mut l3s_protocol::SideConfig, name: &str) {
    if let Some(index) = side.docks.iter().position(|dock| dock == name) {
        side.docks.remove(index);
        if index < side.dock_weights.len() {
            side.dock_weights.remove(index);
        }
    }
}

fn print_frame(frame: &l3s_protocol::FrameData) {
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
