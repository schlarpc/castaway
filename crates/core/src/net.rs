//! Socket-and-[`SessionSink`] plumbing shared by the listening adapters.
//!
//! Not protocol logic — nothing here parses a byte. This is the one step between "a
//! TCP listener is bound" and "a connection is being served" that every listening
//! protocol performs identically, extracted once it had been copy-pasted, comment
//! included, between two of them (#224).

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpStream};
use tracing::warn;

use crate::adapter::SessionSink;

/// Accept connections on `listener` forever, serving each in its own task with a
/// [`SessionSink`] tagged with the peer address — so two senders on one listener
/// arrive at the session manager as distinct sources.
///
/// `protocol` labels the log line; `serve` owns the connection from here on and is
/// spawned, not awaited, so a slow handshake never blocks the next sender.
///
/// Binding stays with the caller: the bind error names the protocol's own port, and
/// the port-registry lint expectation sits on the `bind` call site where the registry
/// entry is named.
///
/// Returns [`Infallible`] because it returns only in the type system: one failed
/// accept (fd limit, RST between accept and return) must not take the listener down —
/// the next sender deserves a try.
pub async fn accept_loop<F, Fut>(
    listener: TcpListener,
    sink: SessionSink,
    protocol: &'static str,
    serve: F,
) -> Infallible
where
    F: Fn(TcpStream, SocketAddr, SessionSink) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(protocol, error = %e, "accept failed");
                continue;
            }
        };
        let conn_sink = sink.with_instance(peer.to_string());
        tokio::spawn(serve(stream, peer, conn_sink));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // An ephemeral loopback port for the test's own two connections, not a service
    // surface — the registry in crates/app/src/surface.rs is about what the box offers.
    #![allow(clippy::disallowed_methods)]
    use std::sync::Arc;

    use tokio::io::AsyncReadExt;
    use tokio::sync::mpsc;

    use super::*;
    use crate::event::SessionEvent;
    use crate::types::ProtocolKind;
    use crate::SourceId;

    #[tokio::test]
    async fn each_connection_is_served_under_its_own_peer_tag() {
        // The property the duplicated loops existed for: two senders through one
        // listener must reach the session manager as two sources, or the second
        // phone's events land on the first phone's session.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Cast, "listener"), tx);

        tokio::spawn(accept_loop(
            listener,
            sink,
            "test",
            |mut stream, peer, sink| async move {
                // Hold the connection open until the peer closes so the sink's tag is
                // observed for a live connection, not a raced teardown.
                let _ = sink.emit(SessionEvent::End).await;
                let _ = stream.read_u8().await;
                let _ = peer;
            },
        ));

        let a = TcpStream::connect(addr).await.unwrap();
        let b = TcpStream::connect(addr).await.unwrap();

        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        let tags: Vec<Arc<str>> = vec![
            first.source.instance.clone(),
            second.source.instance.clone(),
        ];
        assert_ne!(
            tags[0], tags[1],
            "two connections must be distinguishable sources"
        );
        for (tag, local) in [&tags[0], &tags[1]]
            .into_iter()
            .zip([a.local_addr().unwrap(), b.local_addr().unwrap()])
        {
            assert!(
                tags.contains(&Arc::from(local.to_string())),
                "the tag must be the peer address, got {tag} for {local}"
            );
        }
        // The protocol half of the tag is inherited, not spoofable.
        assert_eq!(first.source.kind, ProtocolKind::Cast);
    }
}
