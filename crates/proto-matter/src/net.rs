//! `rs-matter`'s transport, over this project's sockets.
//!
//! `rs-matter` ships its own UDP stack behind an `async-io` reactor, and this project
//! runs one tokio runtime (ground rule 4). Its transport is trait-based for exactly this
//! reason, so the crate's `os` feature is off and the socket is ours: `NetworkSend` and
//! `NetworkReceive` over a `tokio::net::UdpSocket`, and nothing else changes.
//!
//! One socket, cloned into both halves. Matter is request/response over a single
//! rendezvous port and the transport expects to answer on the address it was reached at,
//! so send and receive must be the same socket rather than a pair.

use std::net::SocketAddr;
use std::sync::Arc;

use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::{Address, NetworkReceive, NetworkSend};
use tokio::net::UdpSocket;

use crate::error::MatterError;

/// The send half.
#[derive(Debug, Clone)]
pub struct UdpSend(Arc<UdpSocket>);

/// The receive half.
#[derive(Debug, Clone)]
pub struct UdpRecv(Arc<UdpSocket>);

/// Bind the Matter operational socket and split it into the two halves the transport
/// wants.
///
/// # Errors
/// [`MatterError::Io`] if the port is taken.
#[expect(
    clippy::disallowed_methods,
    reason = "registered: the matter operational socket (5540/udp), in the listener \
              table of crates/app/src/surface.rs"
)]
pub async fn bind(addr: SocketAddr) -> Result<(UdpSend, UdpRecv), MatterError> {
    let socket = UdpSocket::bind(addr)
        .await
        .map_err(|source| MatterError::Io {
            context: "binding the matter operational socket",
            source,
        })?;

    let socket = Arc::new(socket);
    Ok((UdpSend(Arc::clone(&socket)), UdpRecv(socket)))
}

impl NetworkSend for UdpSend {
    async fn send_to(&mut self, data: &[u8], addr: Address) -> Result<(), Error> {
        let Address::Udp(addr) = addr else {
            // The transport only ever hands back an address it was given, and everything
            // reaching this socket arrived over UDP. A TCP or BTP address here would mean
            // the session table had crossed two transports.
            return Err(ErrorCode::NoNetworkInterface.into());
        };

        self.0
            .send_to(data, addr)
            .await
            .map_err(|_| Error::from(ErrorCode::StdIoError))?;

        Ok(())
    }
}

impl NetworkReceive for UdpRecv {
    async fn wait_available(&mut self) -> Result<(), Error> {
        // `readable()` is level-triggered against the socket's readiness, so this returns
        // without consuming the datagram — which is the contract: the transport wants to
        // pick its own buffer before the read happens.
        self.0
            .readable()
            .await
            .map_err(|_| Error::from(ErrorCode::StdIoError))
    }

    async fn recv_from(&mut self, buffer: &mut [u8]) -> Result<(usize, Address), Error> {
        let (len, addr) = self
            .0
            .recv_from(buffer)
            .await
            .map_err(|_| Error::from(ErrorCode::StdIoError))?;

        Ok((len, Address::Udp(addr)))
    }
}

#[cfg(test)]
mod tests {
    // Ephemeral loopback sockets that never face the LAN, which is the standing carve-out
    // for the network-surface lint.
    #![allow(clippy::unwrap_used, clippy::disallowed_methods)]
    use super::*;

    /// The two halves must be the same socket: Matter answers on the port it was reached
    /// at, and a second socket would answer from a port the peer's session table has
    /// never heard of.
    #[tokio::test]
    async fn the_halves_share_one_socket() {
        let (mut send, mut recv) = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let local = recv.0.local_addr().unwrap();

        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();

        send.send_to(b"ping", Address::Udp(peer_addr))
            .await
            .unwrap();

        let mut buf = [0u8; 16];
        let (len, from) = peer.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"ping");
        assert_eq!(from, local, "the reply address is the one we receive on");

        peer.send_to(b"pong", local).await.unwrap();
        recv.wait_available().await.unwrap();
        let (len, addr) = recv.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"pong");
        assert_eq!(addr, Address::Udp(peer_addr));
    }
}
