//! Command-line entry point for the host-side connector.
//!
//! `serve` forwards remote clients to a `luvus uhp access` gateway. It has two
//! transports: a plain TCP listener for a trusted network, and `--iroh`, a
//! QUIC endpoint reachable by `EndpointId` from a device anywhere. `descriptor`
//! is a smoke test that prints the gateway's one-line descriptor and exits.

use std::net::TcpListener;
use std::path::PathBuf;

use anyhow::{Context, bail};
use pilus_agent::{AccessOptions, Gateway, p2p, serve};

const USAGE: &str = "\
pilus-agent - expose a local Luvus session to remote UHP clients

USAGE:
    pilus-agent serve [--iroh] [--listen ADDR] [--control] [--machines] [--session NAME]
    pilus-agent descriptor [--control] [--machines] [--session NAME]

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

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "serve" | "descriptor" if command.is_empty() => command = args[index].clone(),
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
            if iroh && options.ttl_seconds.is_none() {
                // The connector pairs once and holds the token for its whole
                // run, so process-bound authority is the only bound a device
                // can inherit.
                options.no_expiry = true;
            }
            let gateway = Gateway::start(&binary, &options)?;
            let address = gateway.descriptor().address.clone();
            if iroh {
                let p2p_options = p2p::P2pOptions {
                    gateway: address,
                    relay,
                    allow,
                };
                let key_path = identity.unwrap_or_else(default_identity_path);
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .context("could not start the async runtime")?;
                runtime.block_on(p2p::serve(p2p_options, &key_path, gateway.descriptor()))?;
            } else {
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
