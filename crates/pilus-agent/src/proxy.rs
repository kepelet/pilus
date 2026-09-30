//! The connector's first transport: a TCP listener that forwards remote
//! clients to the gateway's loopback address.
//!
//! This is a stand-in for the real provider. iroh replaces it by handing each
//! accepted QUIC stream to [`forward`] instead of a `TcpStream`; the
//! forwarding rule is the same either way — bytes move, UHP is never parsed.

use std::io::copy;
use std::net::{Shutdown, TcpListener, TcpStream};

/// Forward every accepted client to `gateway_address` until the listener dies.
///
/// Each connection gets its own upstream connection, matching UHP's
/// one-request-per-connection contract, and each direction is copied on its own
/// thread so a half-close reaches the other side.
pub fn serve(listener: TcpListener, gateway_address: String) -> std::io::Result<()> {
    for client in listener.incoming() {
        let client = client?;
        let gateway_address = gateway_address.clone();
        std::thread::spawn(move || {
            if let Err(error) = forward(client, &gateway_address) {
                eprintln!("pilus-agent: connection failed: {error}");
            }
        });
    }
    Ok(())
}

/// Pipe one client connection to the gateway and back.
pub(crate) fn forward(client: TcpStream, gateway_address: &str) -> std::io::Result<()> {
    let upstream = TcpStream::connect(gateway_address)?;
    let mut client_read = client.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let upload = std::thread::spawn(move || {
        let _ = copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });
    let mut upstream_read = upstream;
    let mut client_write = client;
    let _ = copy(&mut upstream_read, &mut client_write)?;
    let _ = client_write.shutdown(Shutdown::Write);
    let _ = upload.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use std::io::{BufRead, BufReader, Write};
    use std::thread;

    use super::*;

    #[test]
    fn forwards_bytes_both_ways() {
        // A fake gateway that answers one request and closes.
        let gateway = TcpListener::bind("127.0.0.1:0").expect("gateway bind");
        let gateway_address = gateway.local_addr().expect("gateway addr").to_string();
        let server = thread::spawn(move || {
            let (stream, _) = gateway.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            writeln!(writer, "{{\"id\":\"1\",\"result\":{{\"type\":\"ok\"}}}}").expect("write");
        });

        // A front listener that forwards to the fake gateway.
        let front = TcpListener::bind("127.0.0.1:0").expect("front bind");
        let front_address = front.local_addr().expect("front addr");
        let forwarder = thread::spawn(move || {
            let (client, _) = front.accept().expect("accept front");
            forward(client, &gateway_address).expect("forward");
        });

        let mut client = TcpStream::connect(front_address).expect("connect front");
        client
            .write_all(b"{\"id\":\"1\",\"method\":\"ping\",\"params\":{}}\n")
            .expect("write request");
        let mut reply = String::new();
        BufReader::new(client.try_clone().expect("clone"))
            .read_line(&mut reply)
            .expect("read reply");
        assert!(reply.contains("\"result\""));
        assert!(reply.contains("\"ok\""));

        client.shutdown(Shutdown::Write).expect("half-close");
        server.join().expect("server thread");
        forwarder.join().expect("forwarder thread");
    }
}
