//! A `HEAD` at the resource a control point just named, before the transport accepts it.
//!
//! ## Why this exists
//!
//! `SetAVTransportURI` used to be answered from the URI *string* alone. `MediaUri::parse`
//! checks the scheme, so `ftp://…` was refused with 716 — but any well-formed `http://`
//! URL passed, including one pointing at nothing, at a server that is down, or at an HTML
//! error page. The action was answered 200, the control point showed a healthy session,
//! and the fault surfaced seconds later when the decode thread gave up and the transport
//! flipped to `ERROR_OCCURRED`. By then the person in the room has been looking at a blank
//! panel wondering whether they cast to the right device.
//!
//! Rygel's DMR does this HEAD, and it is what makes `714 Illegal MIME-type` reachable at
//! all: before this, the variant existed, had its code, and was constructed nowhere
//! outside its own unit test, because nothing ever looked at what the resource *was*
//! (#99).
//!
//! ## Why leniency is the default and not a compromise
//!
//! The two ways to be wrong are not symmetric. A probe that fails to reject a bad resource
//! costs exactly what we had before — an asynchronous `ERROR_OCCURRED`, which still works.
//! A probe that rejects a *good* one turns a cast that would have played into a phone
//! saying the renderer refused it, with no way for the guest to override it. So every
//! ambiguous answer resolves to [`Verdict::Inconclusive`] and the item plays: a server
//! that will not do `HEAD`, one that refuses this request but not a `GET`, one that names
//! no type, one that names `application/octet-stream`, a timeout, a scheme with nothing to
//! probe. Only two answers are acted on, and both are unambiguous: `404`/`410`, where the
//! server says there is nothing at that URL at all, and a `Content-Type` outside what we
//! told the control point we accept.
//!
//! ## Why `ureq`
//!
//! [`crate::notify`] already hand-rolls an outbound HTTP client for GENA `NOTIFY`, and
//! extending it sideways was the tempting move. It is the wrong one: `notify` is `http`
//! only, with no TLS and no redirects, because a GENA callback URL is plain `http` on the
//! LAN *by construction*. A media URL is neither — a MediaServer may be `https` and may
//! `302` — so reusing that client would mean either a probe that mis-refuses every `https`
//! item or growing a TLS stack inside a module whose whole point is not having one.
//!
//! `ureq` is already a workspace dependency and already used from a `proto-*` crate for
//! exactly this shape (`proto-gamestream`'s NVHTTP client): blocking, run off the runtime
//! on a `spawn_blocking` thread. Same precedent, no new dependency class, and redirects
//! and TLS come with it.

use std::time::Duration;

/// How long the whole probe may take.
///
/// This sits inside the control point's own `SetAVTransportURI` call, so the ceiling is
/// how long a phone will wait for a SOAP response before deciding the renderer is broken —
/// which for the impatient ones is around five seconds. Three leaves room for the response
/// itself, and a server slower than that is answered [`Verdict::Inconclusive`] and played
/// anyway, so the cost of the timeout being too short is only that we learn nothing.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// What the resource turned out to be.
///
/// Three variants and not two, because "we could not tell" is a real answer here and
/// collapsing it into either of the others is the bug: folded into `Playable` it would
/// hide a genuine refusal, folded into a rejection it would refuse working casts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Probed, and nothing was wrong with it.
    Playable,
    /// The server answered, and the resource is not there. Carries the status.
    Missing(u16),
    /// The server answered, and named a content type outside what we advertise.
    /// Carries the type as sent.
    WrongType(String),
    /// Nothing was learned. Play it and let the decoder be the judge, exactly as before
    /// this probe existed.
    Inconclusive,
}

/// `HEAD` `uri` and decide whether the transport should take it.
///
/// Never returns an error: every failure mode is [`Verdict::Inconclusive`], because a
/// probe that cannot reach a conclusion must not be the reason a cast is refused.
pub(crate) async fn probe(uri: &castaway_core::MediaUri) -> Verdict {
    // Only `http`/`https` have anything to HEAD. A `file:` URI is checked by opening it
    // and `data:` carries its own bytes, so both are the decoder's business.
    if !matches!(uri.scheme(), "http" | "https") {
        return Verdict::Inconclusive;
    }

    let uri = uri.to_string();
    // `ureq` is blocking, and this is called from the SOAP handler on the runtime.
    let joined = tokio::task::spawn_blocking(move || head(&uri)).await;
    match joined {
        Ok(verdict) => verdict,
        // A panicking probe is a bug worth a log elsewhere, but it is not grounds to
        // refuse the item.
        Err(_) => Verdict::Inconclusive,
    }
}

/// The blocking half. Runs on a `spawn_blocking` thread.
fn head(uri: &str) -> Verdict {
    let agent = ureq::AgentBuilder::new()
        .timeout(PROBE_TIMEOUT)
        // Both, and this is not belt-and-braces: `ureq`'s own docs say `timeout_connect`
        // *wins* over `timeout` for the connect phase, and it defaults to 30 seconds. With
        // only the overall timeout set, a URL pointing at an address that black-holes —
        // which is the exact case this probe exists for — stalls the control point's
        // `SetAVTransportURI` for half a minute before playing it anyway. Caught by the
        // two `proto-dlna` end-to-end tests going from 6 ms to 30 s.
        .timeout_connect(PROBE_TIMEOUT)
        // A MediaServer that redirects is ordinary; a redirect *loop* must not eat the
        // whole budget before the timeout notices.
        .redirects(4)
        // The same identity the fetch itself uses, so a server that varies its answer by
        // client — some do, to decide whether to transcode — describes to us the resource
        // it would actually serve us.
        .user_agent(castaway_core::MEDIA_USER_AGENT)
        .build();

    let response = match agent
        .head(uri)
        // Asked for the same reason the fetch asks: it is what marks this a renderer
        // rather than an anonymous GET, and some servers answer differently without it.
        .set("getcontentFeatures.dlna.org", "1")
        .set("transferMode.dlna.org", "Streaming")
        .call()
    {
        Ok(response) => response,
        // A status the server chose to send.
        //
        // Only two of them are refused, and the list is short on purpose: this is the
        // asymmetry from the module note applied to status codes. `404`/`410` are the
        // server saying, unconditionally, that there is nothing at that URL — the `GET`
        // that follows cannot succeed where the `HEAD` did not.
        //
        // Every other status is refused *by this request* rather than about this
        // resource, and several of them routinely coexist with a `GET` that works:
        // `405`/`501` is a server that does not implement `HEAD` at all, `403` is a signed
        // URL whose signature covers the method, `401` is auth we are not carrying, `429`
        // is a rate limit that will have passed by the time the decoder asks, `5xx` is a
        // bad minute the decoder's own reconnect may get through. Refusing on any of those
        // is a cast that would have played, refused.
        Err(ureq::Error::Status(code, _)) => {
            if code == 404 || code == 410 {
                return Verdict::Missing(code);
            }
            return Verdict::Inconclusive;
        }
        // Transport failure: DNS, connect refused, TLS, timeout. Tempting to call this
        // `Missing` — it is the case the issue opens with, a URL pointing at nothing — but
        // it is also what a server behind a slow link, or one that refuses HEAD by
        // dropping the connection, looks like. The decoder's own 30 s fetch with reconnect
        // is better placed to tell those apart than a 3 s probe is.
        Err(_) => return Verdict::Inconclusive,
    };

    match response.header("Content-Type") {
        Some(ct) => classify(ct),
        // No type at all. Legal, and common from the simplest servers.
        None => Verdict::Playable,
    }
}

/// Decide a content-type header against what this renderer advertises.
///
/// Pure, so the whole table below is testable without a socket.
pub(crate) fn classify(content_type: &str) -> Verdict {
    // `audio/mpeg; charset=utf-8` — the parameters are not part of the type, and a server
    // that sends them is not naming a different thing.
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if mime.is_empty() {
        return Verdict::Playable;
    }
    // The universal "here are some bytes". Nothing is claimed by it, so nothing can be
    // refused on it — and plenty of real servers send it for an mp4.
    if mime == "application/octet-stream" || mime == "binary/octet-stream" {
        return Verdict::Playable;
    }
    if crate::state::sink_accepts(&mime) {
        return Verdict::Playable;
    }
    Verdict::WrongType(content_type.trim().to_string())
}

#[cfg(test)]
mod tests {
    // Tests bind an ephemeral loopback socket only to learn a port nothing is listening
    // on; the registry in `crates/app/src/surface.rs` governs production binds.
    #![allow(clippy::disallowed_methods)]
    use super::*;

    /// The point of the whole probe: an HTML error page is the thing a control point most
    /// often actually hands us, and it is not media.
    #[test]
    fn an_error_page_is_refused() {
        assert_eq!(
            classify("text/html; charset=utf-8"),
            Verdict::WrongType("text/html; charset=utf-8".into())
        );
        assert_eq!(
            classify("application/json"),
            Verdict::WrongType("application/json".into())
        );
    }

    #[test]
    fn media_types_pass() {
        for ct in [
            "video/mp4",
            "audio/mpeg",
            "audio/x-flac",
            "video/x-matroska",
            "AUDIO/MPEG",
            "audio/mp4; codecs=\"mp4a.40.2\"",
        ] {
            assert_eq!(classify(ct), Verdict::Playable, "{ct}");
        }
    }

    /// Every ambiguous answer plays. Each of these is a real server's real behaviour, and
    /// refusing on any of them would be a working cast lost.
    #[test]
    fn ambiguity_resolves_to_playing_it() {
        for ct in ["application/octet-stream", "binary/octet-stream", "", "   "] {
            assert_eq!(classify(ct), Verdict::Playable, "{ct:?}");
        }
    }

    /// The accept set is read out of the advertised table rather than written twice, so
    /// this holds by construction — but the *reason* it holds is worth a test, because the
    /// day someone drops the globs from `sink_protocol_info` the enumeration alone would
    /// start refusing types we never enumerated.
    #[test]
    fn anything_we_advertise_is_accepted() {
        for entry in crate::state::sink_protocol_info().split(',') {
            let Some(mime) = entry.split(':').nth(2) else {
                continue;
            };
            if mime.contains('*') {
                continue;
            }
            assert_eq!(classify(mime), Verdict::Playable, "{mime}");
        }
    }

    fn uri(s: &str) -> castaway_core::MediaUri {
        castaway_core::MediaUri::parse(s).expect("test URI")
    }

    #[tokio::test]
    async fn a_scheme_with_nothing_to_head_is_not_probed() {
        assert_eq!(
            probe(&uri("file:///tmp/clip.mp4")).await,
            Verdict::Inconclusive
        );
        assert_eq!(
            probe(&uri("rtsp://host/stream")).await,
            Verdict::Inconclusive
        );
    }

    /// A port nothing is listening on is the failure this probe is *not* for: it looks the
    /// same as a slow server, and the decoder's own 30 s fetch is better placed to judge.
    #[tokio::test]
    async fn an_unreachable_host_is_left_to_the_decoder() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        assert_eq!(
            probe(&uri(&format!("http://127.0.0.1:{port}/clip.mp4"))).await,
            Verdict::Inconclusive
        );
    }

    /// The probe sits inside the control point's own SOAP call, so its ceiling is a
    /// correctness property and not a tuning preference: a phone waiting on
    /// `SetAVTransportURI` decides the renderer is broken long before `ureq`'s 30-second
    /// default connect timeout would return. TEST-NET-1 (RFC 5737) is routed nowhere, so
    /// the connect hangs rather than being refused — which is what tells the two timeouts
    /// apart. Generous bound: it distinguishes 3 s from 30 s and nothing finer, and in a
    /// sandbox with no network at all it simply fails fast.
    #[tokio::test]
    async fn a_black_holed_address_gives_up_inside_the_soap_call() {
        let started = std::time::Instant::now();
        let verdict = probe(&uri("http://192.0.2.1/clip.mp4")).await;
        assert_eq!(verdict, Verdict::Inconclusive);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "probe took {:?}; the connect timeout is not bounded",
            started.elapsed()
        );
    }
}
