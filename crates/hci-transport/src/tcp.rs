//! H4 over TCP: the virtual-controller transport.
//!
//! rootcanal — and netsim, which wraps it and is what the Android emulator's Bluetooth
//! has been since emulator 33.1.4 — accepts external host stacks on its HCI port: each
//! new TCP connection becomes a new virtual controller on the shared phy, speaking plain
//! H4 (indicator byte + packet) with no handshake before it (#225). Attaching here puts
//! the entire stack above [`HciTransport`] on the same simulated air as a real Android
//! phone's Bluetooth stack, which is what makes an emulator-driven A2DP/AVRCP session an
//! automatable test rather than an afternoon with a phone.
//!
//! Unlike the raw-HCI socket, TCP is a byte *stream*: a read can hold half a header or
//! three packets and part of a fourth. The framing lives in
//! [`substrate_hci::StreamDeframer`], which is pure and tested at every split point;
//! this file is only the socket around it (ground rule 3).
//!
//! No `cfg` and no cargo feature: the module is pure safe portable Rust, and a gate
//! would only make it something nothing compiles (D55).

use substrate_hci::{HciError, HciPacket, HciTransport, StreamDeframer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tracing::info;

use crate::error::TransportError;

/// One socket read. Larger than any single HCI packet's usual size, so a full A2DP
/// media frame plus its neighbours arrive in one syscall.
const READ_BUF: usize = 4096;

/// A virtual controller reached over TCP, H4-framed.
#[derive(Debug)]
pub struct TcpTransport {
    /// The command/ACL send path. Its own lock so a blocked `recv` never delays a send.
    write: tokio::sync::Mutex<OwnedWriteHalf>,
    /// The receive path and the carry-over between reads, locked together: the deframer
    /// state is meaningless except against the read position it belongs to.
    read: tokio::sync::Mutex<(OwnedReadHalf, StreamDeframer)>,
    peer: String,
}

impl TcpTransport {
    /// Connect to a rootcanal/netsim HCI port — `host:port`, e.g. `127.0.0.1:6402`.
    ///
    /// The connection *is* the controller: rootcanal instantiates one per accepted
    /// socket, so there is nothing to select and nothing to probe. The controller
    /// arrives un-initialised and goes through the same reset/bring-up sequence as any
    /// other.
    ///
    /// # Errors
    /// [`TransportError::Io`] if the connection is refused or cannot be established.
    pub async fn connect(addr: &str) -> Result<Self, TransportError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| TransportError::Io(format!("connecting to hci port {addr}: {e}")))?;
        // HCI is command/response at bring-up: without NODELAY every round trip waits
        // out a Nagle window, and there are dozens of them before the radio is usable.
        stream
            .set_nodelay(true)
            .map_err(|e| TransportError::Io(format!("set_nodelay on {addr}: {e}")))?;
        info!(%addr, "attached to a virtual controller over TCP");
        let (read, write) = stream.into_split();
        Ok(Self {
            write: tokio::sync::Mutex::new(write),
            read: tokio::sync::Mutex::new((read, StreamDeframer::new())),
            peer: addr.to_owned(),
        })
    }

    /// The address this transport is attached to, for logs.
    #[must_use]
    pub fn peer(&self) -> &str {
        &self.peer
    }
}

#[async_trait::async_trait]
impl HciTransport for TcpTransport {
    async fn send(&self, packet: HciPacket) -> Result<(), HciError> {
        let bytes = packet.encode()?;
        let mut write = self.write.lock().await;
        write
            .write_all(&bytes)
            .await
            .map_err(|e| HciError::Transport(format!("tcp write to {}: {e}", self.peer)))
    }

    async fn recv(&self) -> Result<HciPacket, HciError> {
        let mut guard = self.read.lock().await;
        let (read, deframer) = &mut *guard;
        loop {
            // Drain before reading: one read can complete several packets, and a caller
            // taking them one at a time must not block on the socket while packets wait.
            if let Some(packet) = deframer.next_packet()? {
                return Ok(packet);
            }
            let mut buf = [0u8; READ_BUF];
            let n = read
                .read(&mut buf)
                .await
                .map_err(|e| HciError::Transport(format!("tcp read from {}: {e}", self.peer)))?;
            if n == 0 {
                return Err(HciError::Transport(format!(
                    "the virtual controller at {} closed the connection",
                    self.peer
                )));
            }
            deframer.extend(&buf[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // Tests bind ephemeral loopback sockets that never face the LAN (clippy.toml).
    #![allow(clippy::disallowed_methods)]
    use bytes::Bytes;
    use substrate_hci::{Command, OpCode};

    use super::*;

    /// A controller at the far end of a real socket: accepts one connection, answers a
    /// reset, then dribbles a second event across deliberately misaligned writes.
    #[tokio::test]
    async fn a_scripted_tcp_controller_round_trips_and_survives_split_reads() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let controller = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read the reset command: 4 bytes on the wire.
            let mut cmd = [0u8; 4];
            sock.read_exact(&mut cmd).await.unwrap();
            assert_eq!(&cmd, &[0x01, 0x03, 0x0c, 0x00]);

            // Command Complete for the reset, then a Connection Complete — written as
            // one buffer split at a boundary that is *inside* the second packet's
            // header, which is exactly what TCP is allowed to do and a per-read decoder
            // would garble.
            let both: Vec<u8> = [
                &[0x04, 0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00][..],
                &[
                    0x04, 0x03, 0x0b, 0x00, 0x0b, 0x00, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x01,
                    0x00,
                ][..],
            ]
            .concat();
            sock.write_all(&both[..8]).await.unwrap();
            sock.flush().await.unwrap();
            // Let the client observe the partial packet before the rest arrives.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            sock.write_all(&both[8..]).await.unwrap();
            sock.flush().await.unwrap();
            // Hold the socket open until the client is done reading.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let transport = TcpTransport::connect(&addr.to_string()).await.unwrap();
        transport
            .send(Command::Reset.encode().unwrap())
            .await
            .unwrap();

        let HciPacket::Event { code, params } = transport.recv().await.unwrap() else {
            panic!("expected the command complete");
        };
        assert_eq!(code, 0x0e);
        assert_eq!(&params[..], &[0x01, 0x03, 0x0c, 0x00]);

        let HciPacket::Event { code, params } = transport.recv().await.unwrap() else {
            panic!("expected the connection complete");
        };
        assert_eq!(code, 0x03);
        assert_eq!(params.len(), 11);

        controller.await.unwrap();
    }

    #[tokio::test]
    async fn a_controller_that_hangs_up_is_an_error_not_a_wait() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            drop(sock);
        });

        let transport = TcpTransport::connect(&addr.to_string()).await.unwrap();
        server.await.unwrap();
        let err = transport.recv().await.unwrap_err();
        assert!(
            format!("{err}").contains("closed"),
            "the error should name the hang-up: {err}"
        );
    }

    #[tokio::test]
    async fn a_refused_connection_reports_the_address() {
        // Bind-then-drop guarantees a port with no listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let err = TcpTransport::connect(&addr.to_string()).await.unwrap_err();
        assert!(format!("{err}").contains(&addr.port().to_string()), "{err}");
    }

    #[tokio::test]
    async fn sends_do_not_wait_on_a_parked_recv() {
        // The property the two locks exist for: a recv blocked on a silent controller
        // must not stop the command path. One send while recv is parked, with a timeout
        // that fails the test if the send queued behind it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read and discard forever; never write.
            let mut buf = [0u8; 64];
            while sock.read(&mut buf).await.unwrap() > 0 {}
        });

        let transport =
            std::sync::Arc::new(TcpTransport::connect(&addr.to_string()).await.unwrap());
        let parked = {
            let transport = std::sync::Arc::clone(&transport);
            tokio::spawn(async move { transport.recv().await })
        };
        tokio::task::yield_now().await;

        let send = transport.send(HciPacket::Command {
            opcode: OpCode::new(0x0C03),
            params: Bytes::new(),
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), send)
            .await
            .expect("send must not queue behind a parked recv")
            .unwrap();

        parked.abort();
        server.abort();
    }
}
