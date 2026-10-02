//! The iroh transport: a QUIC endpoint that forwards inbound streams to the
//! host's local gateway loopback.
//!
//! iroh replaces [`crate::proxy`]'s TCP listener. Each bidirectional QUIC
//! stream maps to its own gateway connection, exactly like one TCP client, and
//! the identity is a persistent Ed25519 key so a device can dial the host by a
//! stable `EndpointId`. The relay carries the connection only when direct hole
//! punching fails, and it cannot read the traffic.

use std::path::Path;
use std::sync::Arc;

use iroh::endpoint::{
    AfterHandshakeOutcome, Connection, EndpointHooks, RecvStream, SendStream, Side, presets,
};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, RelayMode, RelayUrl, SecretKey};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::io::AsyncWriteExt;

use crate::enroll::{AuthState, CONTROL_ALPN, Control, ENROLLMENT_TTL};
use crate::{Descriptor, Error, Result};

/// The ALPN every `pilus` peer speaks.
pub const ALPN: &[u8] = b"luvus/uhp/1";

/// How the iroh listener is configured.
#[derive(Clone, Debug)]
pub struct P2pOptions {
    /// The gateway loopback address each stream is bridged to.
    pub gateway: String,
    /// Self-hosted or dedicated relay URL; `None` uses the n0 public relays.
    pub relay: Option<String>,
    /// Device `EndpointId`s allowed to connect; empty allows any peer.
    pub allow: Vec<String>,
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
pub async fn bind(
    key_path: &Path,
    relay: Option<String>,
    auth: Arc<AuthState>,
) -> Result<Endpoint> {
    let secret_key = load_or_create_key(key_path)?;
    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .hooks(Gate { auth });
    if let Some(relay) = &relay {
        let url: RelayUrl = relay
            .parse()
            .map_err(|error| Error::P2p(format!("invalid relay `{relay}`: {error}")))?;
        builder = builder.relay_mode(RelayMode::custom([url]));
    }
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
pub async fn run(endpoint: Endpoint, gateway: String, auth: Arc<AuthState>) -> Result<()> {
    let router = Router::builder(endpoint)
        .accept(ALPN, Uhp { gateway })
        .accept(CONTROL_ALPN, Control::new(auth))
        .spawn();
    tokio::signal::ctrl_c().await.map_err(Error::Io)?;
    let _ = router.shutdown().await;
    Ok(())
}

/// Run the iroh listener until the process is interrupted.
///
/// Pairs with the gateway once, then prints the `EndpointId`, the
/// `EndpointTicket` a device scans, and the one-time `enroll` code that admits
/// it.
pub async fn serve(options: P2pOptions, key_path: &Path, descriptor: &Descriptor) -> Result<()> {
    let token = crate::pair(descriptor)?;
    let devices_path = key_path.with_file_name("devices.json");
    let auth = Arc::new(AuthState::load(
        token,
        devices_path,
        &options.allow,
        ENROLLMENT_TTL,
    )?);
    let endpoint = bind(key_path, options.relay.clone(), Arc::clone(&auth)).await?;
    let ticket = EndpointTicket::new(endpoint.addr());
    println!("endpoint id: {}", endpoint.id());
    println!("ticket:      {ticket}");
    match auth.enrollment() {
        Some((code, expires_at)) => {
            println!("enroll:      {code}");
            eprintln!("pilus-agent: the enrollment code expires at unix {expires_at}");
        }
        None => eprintln!("pilus-agent: enrollment is disabled by --allow"),
    }
    run(endpoint, options.gateway, auth).await
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
    gateway: String,
}

impl ProtocolHandler for Uhp {
    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        while let Ok((send, recv)) = connection.accept_bi().await {
            let gateway = self.gateway.clone();
            tokio::spawn(async move {
                if let Err(error) = bridge(&gateway, send, recv).await {
                    eprintln!("pilus-agent: stream bridge failed: {error}");
                }
            });
        }
        Ok(())
    }
}

/// Pump one QUIC bidirectional stream to one gateway connection and back.
async fn bridge(gateway: &str, mut send: SendStream, mut recv: RecvStream) -> std::io::Result<()> {
    let tcp = tokio::net::TcpStream::connect(gateway).await?;
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
}
