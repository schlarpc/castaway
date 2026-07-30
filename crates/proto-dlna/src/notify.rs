//! Delivering a GENA `NOTIFY` to a subscriber's callback URL.
//!
//! The I/O half of [`crate::gena`], and deliberately the whole of it: one request, one
//! status line, no body to read, no redirects, no TLS. GENA callbacks are plain `http` on
//! the LAN by construction — the subscriber hands us the URL and it is its own listener —
//! so an HTTP client library here would be a dependency bought for a request that fits on
//! a screen.
//!
//! Everything about this is bounded. A subscriber that accepts a connection and then says
//! nothing must not hold up delivery to the ones behind it, and a phone that left the
//! building must not be waited on at all — so the connect, the write and the read each get
//! a deadline, and [`crate::gena::Subscribers::delivery_failed`] retires a callback that
//! keeps missing.

use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use crate::gena::EventedService;

/// How long one delivery may take, end to end.
///
/// Short, because the cost of being wrong in each direction is not symmetric: too long and
/// one unresponsive subscriber delays every event to every other one, too short and a busy
/// control point occasionally misses an event it would have taken — and the next state
/// change carries the whole state again, so a missed one is self-healing where a stalled
/// queue is not.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of the subscriber's response is read before giving up on it.
///
/// Only the status line matters, and a subscriber that answers with a header block far
/// larger than this is one whose answer we would not act on anyway.
const MAX_RESPONSE: usize = 1024;

/// Why a delivery did not land.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NotifyError {
    /// The callback URL was not something we could dial.
    #[error("unusable callback URL: {0}")]
    BadCallback(String),
    /// The connection failed, timed out, or was cut.
    #[error("could not reach the subscriber: {0}")]
    Unreachable(String),
    /// The subscriber answered, and said no.
    #[error("the subscriber answered {0}")]
    Refused(u16),
}

/// A callback URL, split into what a request needs.
struct Callback {
    host: String,
    port: u16,
    /// The request target, always beginning with `/`.
    path: String,
}

/// Split `http://host[:port]/path?query` without a URL crate.
///
/// Hand-parsed because the shape is fixed by the header that carried it: `CALLBACK`
/// entries are absolute `http` URLs with no credentials and no fragment, already filtered
/// to that in [`crate::gena`]. Anything else is refused rather than guessed at.
fn split(url: &str) -> Result<Callback, NotifyError> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("HTTP://"))
        .ok_or_else(|| NotifyError::BadCallback(url.to_string()))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    // An IPv6 literal is bracketed, and its colons are not the port separator.
    let (host, port) = if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
        let host = &authority[..=end + 1];
        let port = authority[end + 2..].strip_prefix(':');
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (authority.to_string(), None),
        }
    };
    let port = match port {
        Some(p) => p
            .parse()
            .map_err(|_| NotifyError::BadCallback(url.to_string()))?,
        None => 80,
    };
    if host.is_empty() {
        return Err(NotifyError::BadCallback(url.to_string()));
    }
    Ok(Callback { host, port, path })
}

/// Send one `NOTIFY` to `url`.
///
/// # Errors
/// [`NotifyError`] if the URL is unusable, the subscriber is unreachable, or it answers
/// with anything other than success.
pub(crate) async fn deliver(
    url: &str,
    service: EventedService,
    sid: &str,
    seq: u32,
    body: &str,
) -> Result<(), NotifyError> {
    let cb = split(url)?;
    // The bracketed form is for the URL; a socket address wants it bracketed too, and
    // `to_socket_addrs` inside tokio handles both that and a name.
    let target = format!("{}:{}", cb.host, cb.port);

    tokio::time::timeout(DELIVERY_TIMEOUT, async {
        let mut socket = TcpStream::connect(&target)
            .await
            .map_err(|e| NotifyError::Unreachable(e.to_string()))?;

        // `NT: upnp:event` and `NTS: upnp:propchange` are both required and both are
        // constants — the only thing a subscriber correlates on is the SID, and the only
        // thing it orders by is the SEQ. `Connection: close` because this is one request:
        // keeping the socket alive would mean owning a pool for a message sent when the
        // volume changes.
        let request = format!(
            "NOTIFY {path} HTTP/1.1\r\n\
             HOST: {host}\r\n\
             CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
             NT: upnp:event\r\n\
             NTS: upnp:propchange\r\n\
             SVCID: {svcid}\r\n\
             SID: {sid}\r\n\
             SEQ: {seq}\r\n\
             CONTENT-LENGTH: {len}\r\n\
             CONNECTION: close\r\n\
             \r\n\
             {body}",
            path = cb.path,
            host = target,
            svcid = service.service_type(),
            len = body.len(),
        );
        socket
            .write_all(request.as_bytes())
            .await
            .map_err(|e| NotifyError::Unreachable(e.to_string()))?;
        socket
            .flush()
            .await
            .map_err(|e| NotifyError::Unreachable(e.to_string()))?;

        let mut response = Vec::with_capacity(128);
        let mut buf = [0u8; 256];
        // Read until the status line is complete or the peer hangs up. Not to EOF: a
        // subscriber that answers and then holds the socket open would cost the whole
        // timeout for a message it already accepted.
        loop {
            let n = socket
                .read(&mut buf)
                .await
                .map_err(|e| NotifyError::Unreachable(e.to_string()))?;
            if n == 0 {
                break;
            }
            response.extend_from_slice(&buf[..n]);
            if response.windows(2).any(|w| w == b"\r\n") || response.len() >= MAX_RESPONSE {
                break;
            }
        }
        status_of(&response)
    })
    .await
    .map_err(|_| NotifyError::Unreachable("the subscriber did not answer in time".into()))?
}

/// The status code from a response's first line.
fn status_of(response: &[u8]) -> Result<(), NotifyError> {
    let text = String::from_utf8_lossy(response);
    let first = text.lines().next().unwrap_or_default();
    let code = first
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| NotifyError::Unreachable(format!("unparseable response: {first:?}")))?;
    if (200..300).contains(&code) {
        Ok(())
    } else {
        Err(NotifyError::Refused(code))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // Tests bind ephemeral loopback sockets; the registry governs production binds.
    #![allow(clippy::disallowed_methods)]
    use super::*;

    #[test]
    fn a_callback_url_is_split_into_what_a_request_needs() {
        let cb = split("http://10.0.0.5:49152/notify/avt").unwrap();
        assert_eq!(cb.host, "10.0.0.5");
        assert_eq!(cb.port, 49152);
        assert_eq!(cb.path, "/notify/avt");

        // No port means 80, and no path means the root — both of which a subscriber is
        // entitled to send and neither of which produces a dialable address by accident.
        let bare = split("http://nas.local").unwrap();
        assert_eq!(
            (bare.host.as_str(), bare.port, bare.path.as_str()),
            ("nas.local", 80, "/")
        );

        // An IPv6 literal's colons are not a port separator, which is the one way a
        // hand-written split gets this wrong.
        let v6 = split("http://[fe80::1]:8080/cb").unwrap();
        assert_eq!(v6.host, "[fe80::1]");
        assert_eq!(v6.port, 8080);

        for bad in ["ftp://h/cb", "http://:9/cb", "http://h:notaport/cb", "h/cb"] {
            assert!(split(bad).is_err(), "{bad} should not be dialable");
        }
    }

    #[test]
    fn only_a_success_status_counts_as_delivered() {
        assert!(status_of(b"HTTP/1.1 200 OK\r\n\r\n").is_ok());
        assert!(status_of(b"HTTP/1.0 204 No Content\r\n").is_ok());
        assert!(matches!(
            status_of(b"HTTP/1.1 412 Precondition Failed\r\n"),
            Err(NotifyError::Refused(412))
        ));
        // A peer that closed without answering is unreachable, not refusing: the
        // difference decides whether we keep trying.
        assert!(matches!(status_of(b""), Err(NotifyError::Unreachable(_))));
    }

    /// The whole point of the bookkeeping around this: a subscriber that vanished must
    /// cost one timeout, not a stalled queue.
    #[tokio::test]
    async fn a_subscriber_that_is_not_there_fails_rather_than_hanging() {
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);

        let started = std::time::Instant::now();
        let result = deliver(
            &format!("http://127.0.0.1:{port}/cb"),
            EventedService::AvTransport,
            "uuid:a",
            0,
            "<x/>",
        )
        .await;
        assert!(matches!(result, Err(NotifyError::Unreachable(_))));
        assert!(started.elapsed() < DELIVERY_TIMEOUT * 2);
    }

    /// The headers a subscriber correlates on, over a real socket. `SID` is how it knows
    /// which of its subscriptions this is, and `SEQ` is how it knows whether it missed
    /// anything — an event with either wrong is one it is entitled to discard.
    #[tokio::test]
    async fn a_delivery_carries_the_headers_a_subscriber_reads() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 2048];
            // One read is enough: the whole request is written in a single `write_all`.
            let n = sock.read(&mut buf).await.unwrap();
            seen.extend_from_slice(&buf[..n]);
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&seen).into_owned()
        });

        deliver(
            &format!("http://127.0.0.1:{port}/cb"),
            EventedService::RenderingControl,
            "uuid:abc-123",
            7,
            "<e:propertyset/>",
        )
        .await
        .unwrap();

        let request = server.await.unwrap();
        assert!(request.starts_with("NOTIFY /cb HTTP/1.1\r\n"));
        assert!(request.contains("NT: upnp:event\r\n"));
        assert!(request.contains("NTS: upnp:propchange\r\n"));
        assert!(request.contains("SID: uuid:abc-123\r\n"));
        assert!(request.contains("SEQ: 7\r\n"));
        assert!(request.contains("CONTENT-LENGTH: 16\r\n"));
        assert!(request.contains("urn:schemas-upnp-org:service:RenderingControl:1"));
        assert!(request.ends_with("<e:propertyset/>"));
    }
}
