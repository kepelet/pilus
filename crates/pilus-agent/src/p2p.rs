//! The iroh transport: a QUIC endpoint that forwards inbound streams to the
//! host's local gateway loopback.
//!
//! iroh replaces [`crate::proxy`]'s TCP listener. Each bidirectional QUIC
//! stream maps to its own gateway connection, exactly like one TCP client, and
//! the identity is a persistent Ed25519 key so a device can dial the host by a
//! stable `EndpointId`. The relay carries the connection only when direct hole
//! punching fails, and it cannot read the traffic.
//!
//! [`serve`] is the long-running entry point: it binds the persistent identity,
//! keeps the gateway alive through [`crate::supervisor`], and serves three
//! ALPNs (pairing, raw UHP, and the [`crate::hub`]) plus the local
//! [`crate::admin`] socket until it is told to stop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use iroh::endpoint::{
    AfterHandshakeOutcome, Connection, EndpointHooks, RecvStream, SendStream, Side, presets,
};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, RelayMode, RelayUrl, SecretKey};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::io::AsyncWriteExt;

use crate::admin::{self, Admin};
use crate::enroll::{AuthState, CONTROL_ALPN, Control, ENROLLMENT_TTL};
use crate::hub::{HUB_ALPN, Hub};
use crate::push::{self, PushStore};
use crate::supervisor::{self, GatewayLink};
use crate::{AccessOptions, Error, Result};

/// The ALPN every `pilus` peer speaks.
pub const ALPN: &[u8] = b"luvus/uhp/1";

/// How the iroh listener is configured.
#[derive(Clone, Debug)]
pub struct P2pOptions {
    /// The gateway loopback address each stream is bridged to. Ignored by
    /// [`serve`], which supervises its own gateway.
    pub gateway: String,
    /// Which relays the endpoint uses, and how it authenticates to them.
    pub relay: RelayConfig,
    /// Device `EndpointId`s allowed to connect; empty allows any peer.
    pub allow: Vec<String>,
}

/// Which relays the endpoint dials out through.
///
/// A managed relay needs both halves: the URLs say where it is, and the API
/// secret lets the endpoint mint the relay token the server checks. The secret
/// alone does not locate a relay, so with no URLs the endpoint keeps the n0
/// public relays and only gains the token.
#[derive(Clone, Debug, Default)]
pub struct RelayConfig {
    /// Self-hosted or dedicated relay URLs; empty uses the n0 public relays.
    pub urls: Vec<String>,
    /// An Iroh Services project API secret.
    pub api_secret: Option<String>,
}

impl RelayConfig {
    /// The `relay` block of `<root>/settings.json`, with `urls` (from
    /// `--relay`) replacing the file's URLs when any are given.
    pub fn load(root: &Path, urls: Vec<String>) -> Result<Self> {
        let settings = pilus_client::RelaySettings::load(root).map_err(Error::P2p)?;
        Ok(Self {
            urls: if urls.is_empty() { settings.url } else { urls },
            api_secret: settings.secret,
        })
    }
}

/// Load the persistent identity at `path`, creating it on first run.
///
/// The key is the host's stable address: losing it changes the `EndpointId`
/// and invalidates every ticket a device has stored.
pub fn load_or_create_key(path: &Path) -> Result<SecretKey> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let array: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::P2p("identity key is not 32 bytes".to_string()))?;
            Ok(SecretKey::from_bytes(&array))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let key = SecretKey::generate();
            crate::write_private(path, &key.to_bytes())?;
            Ok(key)
        }
        Err(error) => Err(Error::Io(error)),
    }
}

/// Bind the iroh endpoint with the host's persistent identity.
///
/// Kept separate from [`run`] so the transport can be exercised without a
/// signal loop.
pub async fn bind(key_path: &Path, relay: &RelayConfig, auth: Arc<AuthState>) -> Result<Endpoint> {
    let secret_key = load_or_create_key(key_path)?;
    let hooks = Gate { auth };
    let builder = match &relay.api_secret {
        Some(api_secret) => {
            // The relay token is bound to the endpoint's key, so the preset
            // must be handed the persistent identity rather than its own.
            let mut preset = iroh_services::preset()
                .secret_key(secret_key)
                .api_secret_from_str(api_secret)
                .map_err(|error| {
                    Error::P2p(format!("invalid relay.secret in settings.json: {error}"))
                })?;
            if !relay.urls.is_empty() {
                preset = preset
                    .relays(&relay.urls)
                    .map_err(|error| Error::P2p(format!("invalid relay: {error}")))?;
            }
            let preset = preset
                .build()
                .map_err(|error| Error::P2p(error.to_string()))?;
            Endpoint::builder(preset).hooks(hooks)
        }
        None => {
            let mut builder = Endpoint::builder(presets::N0)
                .secret_key(secret_key)
                .hooks(hooks);
            if !relay.urls.is_empty() {
                let urls = relay
                    .urls
                    .iter()
                    .map(|relay| {
                        relay.parse::<RelayUrl>().map_err(|error| {
                            Error::P2p(format!("invalid relay `{relay}`: {error}"))
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                builder = builder.relay_mode(RelayMode::custom(urls));
            }
            builder
        }
    };
    let endpoint = builder
        .bind()
        .await
        .map_err(|error| Error::P2p(error.to_string()))?;
    tokio::time::timeout(std::time::Duration::from_secs(5), endpoint.online())
        .await
        .map_err(|_| Error::Timeout("iroh endpoint startup"))?;
    Ok(endpoint)
}

/// Serve inbound UHP and control streams until the process is interrupted.
///
/// This is the fixed-gateway form the tests use: the caller owns the gateway.
pub async fn run(endpoint: Endpoint, gateway: String, auth: Arc<AuthState>) -> Result<()> {
    let router = Router::builder(endpoint)
        .accept(
            ALPN,
            Uhp {
                link: GatewayLink::fixed(gateway),
            },
        )
        .accept(CONTROL_ALPN, Control::new(auth))
        .spawn();
    tokio::signal::ctrl_c().await.map_err(Error::Io)?;
    let _ = router.shutdown().await;
    Ok(())
}

/// How [`serve`] runs.
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// Relay and allow-list.
    pub p2p: P2pOptions,
    /// Where the iroh identity lives; `devices.json` and `agent.sock` sit
    /// beside it.
    pub key_path: PathBuf,
    /// The `luvus` binary, or `None` to discover it on every attempt.
    pub binary: Option<PathBuf>,
    /// How the gateway is started.
    pub access: AccessOptions,
    /// Run as a background service: never write enrollment codes to the log.
    pub quiet: bool,
}

/// Run the connector until it receives SIGINT or SIGTERM.
///
/// Prints the endpoint id and ticket (never the enrollment code in `quiet`
/// mode; the window mints codes through the admin socket instead).
pub async fn serve(config: ServeConfig) -> Result<()> {
    let devices_path = config.key_path.with_file_name("devices.json");
    // The token is empty until the supervisor pairs; the control channel
    // refuses to hand out an empty one.
    let auth = Arc::new(AuthState::load(
        String::new(),
        devices_path,
        &config.p2p.allow,
        ENROLLMENT_TTL,
    )?);
    let endpoint = bind(&config.key_path, &config.p2p.relay, Arc::clone(&auth)).await?;
    let ticket = EndpointTicket::new(endpoint.addr());
    println!("endpoint id: {}", endpoint.id());
    println!("ticket:      {ticket}");
    if !config.quiet {
        match auth.enrollment() {
            Some((code, expires_at)) => {
                println!("enroll:      {code}");
                eprintln!("pilus-agent: the enrollment code expires at unix {expires_at}");
            }
            None => eprintln!("pilus-agent: enrollment is disabled by --allow"),
        }
    }

    let link = GatewayLink::down();
    let stop = Arc::new(AtomicBool::new(false));
    let supervisor = {
        let (binary, access) = (config.binary.clone(), config.access.clone());
        let (auth, link, stop) = (Arc::clone(&auth), Arc::clone(&link), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("pilus-gateway-supervisor".to_string())
            .spawn(move || supervisor::supervise(binary, access, auth, link, stop))
            .map_err(Error::Io)?
    };

    let label = machine_label();
    let mut hub = Hub::new(
        Arc::clone(&link),
        label.clone(),
        Arc::clone(&auth),
        config.access.session.clone(),
    );
    // Push alerts for a phone that is not looking; `PILUS_PUSH_URL=` turns them off.
    let push_task = push::relay_url().map(|relay| {
        let store = Arc::new(PushStore::load(config.key_path.with_file_name("push.json")));
        hub = hub.clone().with_push(Arc::clone(&store));
        tokio::spawn(push::run(
            Arc::clone(&link),
            Arc::clone(&auth),
            store,
            relay,
        ))
    });
    let router = Router::builder(endpoint.clone())
        .accept(
            ALPN,
            Uhp {
                link: Arc::clone(&link),
            },
        )
        .accept(HUB_ALPN, hub)
        .accept(
            CONTROL_ALPN,
            if config.quiet {
                Control::quiet(Arc::clone(&auth))
            } else {
                Control::new(Arc::clone(&auth))
            },
        )
        .spawn();

    let admin_path = config.key_path.with_file_name("agent.sock");
    let admin_task = tokio::spawn({
        let admin = Admin {
            endpoint: endpoint.clone(),
            auth: Arc::clone(&auth),
            link: Arc::clone(&link),
            label,
        };
        let path = admin_path.clone();
        async move {
            if let Err(error) = admin::serve(&path, admin).await {
                eprintln!("pilus-agent: admin socket stopped: {error}");
            }
        }
    });

    wait_for_shutdown().await?;
    admin_task.abort();
    if let Some(task) = push_task {
        task.abort();
    }
    let _ = std::fs::remove_file(&admin_path);
    let _ = router.shutdown().await;
    // The supervisor owns the gateway child; letting it exit drops (kills) it.
    stop.store(true, Ordering::Relaxed);
    let _ = tokio::task::spawn_blocking(move || supervisor.join()).await;
    Ok(())
}

/// Resolve on SIGINT, or SIGTERM where the service manager sends it.
async fn wait_for_shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).map_err(Error::Io)?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(Error::Io),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map_err(Error::Io)
    }
}

/// The name a phone should show for this machine.
pub fn machine_label() -> String {
    #[cfg(target_os = "macos")]
    if let Some(name) = command_output("scutil", &["--get", "ComputerName"]) {
        return name;
    }
    command_output("hostname", &[]).unwrap_or_else(|| "this machine".to_string())
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (output.status.success() && !text.is_empty()).then(|| text.to_string())
}

/// Rejects unenrolled peers from the UHP surface.
///
/// Encryption already authenticates a peer by its `EndpointId`; this hook
/// decides which authenticated peers may reach the gateway. The control ALPN
/// stays open so an unenrolled device can present its code.
#[derive(Clone, Debug)]
struct Gate {
    auth: Arc<AuthState>,
}

impl EndpointHooks for Gate {
    async fn after_handshake(&self, conn: &Connection) -> AfterHandshakeOutcome {
        if conn.side() != Side::Server || conn.alpn() == CONTROL_ALPN {
            return AfterHandshakeOutcome::Accept;
        }
        if self.auth.is_allowed(&conn.remote_id().to_string()) {
            AfterHandshakeOutcome::Accept
        } else {
            AfterHandshakeOutcome::Reject {
                error_code: 403u32.into(),
                reason: b"device not enrolled".into(),
            }
        }
    }
}

/// One ALPN handler: each accepted bidirectional stream is one UHP connection.
#[derive(Clone, Debug)]
struct Uhp {
    link: Arc<GatewayLink>,
}

impl ProtocolHandler for Uhp {
    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        while let Ok((send, recv)) = connection.accept_bi().await {
            let link = Arc::clone(&self.link);
            tokio::spawn(async move {
                if let Err(error) = bridge(&link, send, recv).await {
                    eprintln!("pilus-agent: stream bridge failed: {error}");
                }
            });
        }
        Ok(())
    }
}

/// Pump one QUIC bidirectional stream to one gateway connection and back.
///
/// While the gateway is down the peer gets a UHP-shaped error instead of a
/// stalled stream, so a client can tell "host is up, Luvus is not" from "host
/// is unreachable".
async fn bridge(
    link: &GatewayLink,
    mut send: SendStream,
    mut recv: RecvStream,
) -> std::io::Result<()> {
    let Some(gateway) = link.address() else {
        let why = link
            .error()
            .unwrap_or_else(|| "the Luvus gateway is not running".to_string());
        let reply = serde_json::json!({ "error": { "code": "unavailable", "message": why } });
        send.write_all(reply.to_string().as_bytes()).await?;
        send.write_all(b"\n").await?;
        let _ = send.finish();
        return Ok(());
    };
    let tcp = tokio::net::TcpStream::connect(&gateway).await?;
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let upload = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut recv, &mut tcp_write).await;
        let _ = tcp_write.shutdown().await;
    });
    tokio::io::copy(&mut tcp_read, &mut send).await?;
    let _ = send.finish();
    let _ = upload.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn identity_key_persists_and_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested/identity.key");

        let first = load_or_create_key(&path).expect("create");
        let second = load_or_create_key(&path).expect("load");
        assert_eq!(first.to_bytes(), second.to_bytes());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_rejects_a_malformed_api_secret_before_touching_the_network() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-relay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let auth = Arc::new(
            AuthState::load(String::new(), dir.join("devices.json"), &[], ENROLLMENT_TTL)
                .expect("auth"),
        );
        let relay = RelayConfig {
            urls: Vec::new(),
            api_secret: Some("not-a-secret".to_string()),
        };
        let error = bind(&dir.join("identity.key"), &relay, auth)
            .await
            .expect_err("a malformed secret must not bind");
        assert!(error.to_string().contains("relay.secret"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
