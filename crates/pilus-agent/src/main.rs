//! Command-line entry point for the host-side connector.
//!
//! `serve` forwards remote clients to a `luvus uhp access` gateway. It has two
//! transports: a plain TCP listener for a trusted network, and `--iroh`, a
//! QUIC endpoint reachable by `EndpointId` from a device anywhere. `descriptor`
//! is a smoke test that prints the gateway's one-line descriptor and exits.
//! `service` installs, removes, or reports the background service that keeps
//! `serve --iroh` running after the window closes.

use std::net::TcpListener;
use std::path::PathBuf;

use anyhow::{Context, bail};
use pilus_agent::{AccessOptions, Gateway, p2p, serve, service};

const USAGE: &str = "\
pilus-agent - expose a local Luvus session to remote UHP clients

USAGE:
    pilus-agent serve [--iroh] [--listen ADDR] [--control] [--machines] [--session NAME]
    pilus-agent descriptor [--control] [--machines] [--session NAME]
    pilus-agent service install [--session NAME] [--binary PATH]
    pilus-agent service uninstall
    pilus-agent service status

OPTIONS:
    --listen ADDR     TCP forward address (default 127.0.0.1:7878); ignored with --iroh
    --iroh            serve over iroh/QUIC and print an enrollment ticket
    --relay URL       self-hosted relay instead of the n0 public relays (with --iroh)
    --allow ID        allow only this device EndpointId; repeatable (with --iroh)
    --identity PATH   path to the persistent iroh identity key
    --control         advertise the bounded control surface
    --machines        advertise the owner-local machine catalog
    --session NAME    target a named Luvus session
    --ttl SECONDS     bound the gateway's authority lifetime (1..=86400)
    --no-expiry       keep authority until this process exits
    --service         run as the background service: keep the gateway alive,
                      and never write enrollment codes to the log
    --binary PATH     path to the luvus binary (default: discovered)";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut command = String::new();
    let mut options = AccessOptions::default();
    let mut listen = "127.0.0.1:7878".to_string();
    let mut binary: Option<PathBuf> = None;
    let mut identity: Option<PathBuf> = None;
    let mut iroh = false;
    let mut relay: Option<String> = None;
    let mut allow: Vec<String> = Vec::new();
    let mut background = false;
    let mut subcommand = String::new();

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "serve" | "descriptor" | "service" if command.is_empty() => {
                command = args[index].clone()
            }
            "install" | "uninstall" | "status" if command == "service" && subcommand.is_empty() => {
                subcommand = args[index].clone();
            }
            "--service" => background = true,
            "--listen" => listen = value(&args, &mut index, "--listen")?,
            "--session" => options.session = Some(value(&args, &mut index, "--session")?),
            "--relay" => relay = Some(value(&args, &mut index, "--relay")?),
            "--allow" => allow.push(value(&args, &mut index, "--allow")?),
            "--identity" => identity = Some(PathBuf::from(value(&args, &mut index, "--identity")?)),
            "--ttl" => {
                options.ttl_seconds = Some(
                    value(&args, &mut index, "--ttl")?
                        .parse()
                        .context("--ttl must be an integer")?,
                );
            }
            "--binary" => binary = Some(PathBuf::from(value(&args, &mut index, "--binary")?)),
            "--iroh" => iroh = true,
            "--control" => options.control = true,
            "--machines" => options.machines = true,
            "--no-expiry" => options.no_expiry = true,
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument `{other}`\n\n{USAGE}"),
        }
        index += 1;
    }

    if command == "service" {
        return run_service(&subcommand, options.session.clone(), binary);
    }
    let pinned = binary.clone();
    let binary = binary
        .or_else(pilus_client::luvus_binary)
        .context("could not find the `luvus` binary; pass --binary")?;

    match command.as_str() {
        "descriptor" => {
            let gateway = Gateway::start(&binary, &options)?;
            let descriptor = gateway.descriptor();
            println!("address:   {}", descriptor.address);
            println!(
                "mode:      {}",
                descriptor.mode.as_deref().unwrap_or("read_only")
            );
            println!("scopes:    {}", descriptor.scopes.join(", "));
            match descriptor.expires_at {
                Some(expiry) => println!("expires:   unix {expiry}"),
                None if descriptor.expires_on_close => println!("expires:   on close"),
                None => println!("expires:   unknown"),
            }
        }
        "serve" => {
            if iroh {
                let key_path = identity.unwrap_or_else(default_identity_path);
                if background {
                    trim_log(&key_path.with_file_name("agent.log"));
                }
                let mut access = options.clone();
                // The connector pairs once per gateway and holds the token for
                // its whole run, so process-bound authority is the only bound
                // a device can inherit.
                if access.ttl_seconds.is_none() {
                    access.no_expiry = true;
                }
                // The hub reads the machine catalog itself; the gateway's own
                // `--machines` scope would let a paired device edit it.
                access.machines = false;
                let config = p2p::ServeConfig {
                    p2p: p2p::P2pOptions {
                        gateway: String::new(),
                        relay,
                        allow,
                    },
                    key_path,
                    // Pinned only when the user passed `--binary`; otherwise the
                    // supervisor rediscovers `luvus` so an update cannot strand it.
                    binary: pinned,
                    access,
                    quiet: background,
                };
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .context("could not start the async runtime")?;
                runtime.block_on(p2p::serve(config))?;
            } else {
                let gateway = Gateway::start(&binary, &options)?;
                let address = gateway.descriptor().address.clone();
                let mode = gateway
                    .descriptor()
                    .mode
                    .clone()
                    .unwrap_or_else(|| "read_only".to_string());
                let listener = TcpListener::bind(&listen)
                    .with_context(|| format!("could not bind {listen}"))?;
                eprintln!("pilus-agent: forwarding {listen} -> {address} ({mode})");
                serve(listener, address)?;
            }
        }
        _ => bail!("no subcommand given\n\n{USAGE}"),
    }
    Ok(())
}

/// Install, remove, or report the background service.
fn run_service(
    subcommand: &str,
    session: Option<String>,
    luvus: Option<PathBuf>,
) -> anyhow::Result<()> {
    match subcommand {
        "install" => {
            let agent = std::env::current_exe().context("could not find this binary's path")?;
            let spec = service::Spec {
                agent,
                session,
                luvus: luvus.or_else(pilus_client::luvus_binary),
                // Pin the state root only when it is not the default, so a
                // service follows `~/.luvus` like every other client.
                home: std::env::var_os(pilus_client::HOME_ENV)
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from),
                log: default_identity_path().with_file_name("agent.log"),
            };
            match service::install(&spec)? {
                service::Installed::Started => println!("installed and started"),
                service::Installed::Unchanged => println!("already installed and running"),
            }
        }
        "uninstall" => {
            service::uninstall()?;
            println!("removed");
        }
        "status" => match service::state() {
            service::State::Unsupported => println!("unsupported"),
            service::State::NotInstalled => println!("not installed"),
            service::State::Installed { running: true } => println!("installed, running"),
            service::State::Installed { running: false } => println!("installed, not running"),
        },
        _ => bail!("service needs install, uninstall, or status\n\n{USAGE}"),
    }
    Ok(())
}

/// Start a service log over once it grows past a megabyte.
///
/// The service only logs state changes, so this is rarely hit, but a launchd
/// log has no rotation of its own.
fn trim_log(path: &std::path::Path) {
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > 1_000_000) {
        let _ = std::fs::write(path, b"");
    }
}

/// The default iroh identity path, under the Luvus state root.
fn default_identity_path() -> PathBuf {
    pilus_client::config_dir()
        .join("agent")
        .join("identity.key")
}

fn value(args: &[String], index: &mut usize, flag: &str) -> anyhow::Result<String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .with_context(|| format!("{flag} needs a value"))
}
