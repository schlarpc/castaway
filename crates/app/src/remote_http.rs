//! `/remote/*`: driving the panel from a browser (#18).
//!
//! The sibling of `/stream/*` and the answer to a different want. That one duplicates the
//! panel so you can *see* it; this one makes the duplicate touchable, which needs a
//! transport HLS cannot provide — three to six seconds of segment latency is fine for
//! watching and unusable for control.
//!
//! One route. `POST /remote/whep` takes an SDP offer and returns an answer; everything
//! after that is UDP between the peer and `pipeline::remote`, and this host sees none of
//! it. Non-trickle, so one request is the whole negotiation — there is no session to keep,
//! no polling, and nothing to clean up if the peer never comes back.
//!
//! There is deliberately **no page of its own**. The player lives on the landing page
//! (`pipeline::remote::PLAYER`), stopped until somebody presses it, so there is one place
//! to look at the panel rather than a viewer at `/` and a driver somewhere else.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;

/// The handle on the remote-control service, or an uninhabited stand-in.
///
/// The same trick as `Stream` in `stream_http`: making the type uninhabited rather than
/// `cfg`-ing the routes away means the endpoints still exist and still explain themselves
/// in a build that cannot serve them.
#[cfg(feature = "remote")]
pub type Remote = std::sync::Arc<pipeline::remote::RemoteService>;
#[cfg(not(feature = "remote"))]
pub type Remote = std::convert::Infallible;

/// Where an offer goes.
pub const WHEP_PATH: &str = "/remote/whep";

/// The largest SDP offer that will be read.
///
/// A real one is two or three kilobytes. This is generous enough that no browser will hit
/// it and small enough that a stranger on the LAN cannot make the panel allocate.
///
/// Enforced as a body limit on the route rather than by measuring the string afterwards:
/// by the time a handler can check `offer.len()`, axum has already read and buffered the
/// whole thing, so the check would report a size it had just finished allocating.
const MAX_OFFER: usize = 64 * 1024;

/// Mount `/remote/*`.
pub fn routes(remote: Option<Remote>) -> Router {
    Router::new()
        .route(
            WHEP_PATH,
            post(whep_route).layer(axum::extract::DefaultBodyLimit::max(MAX_OFFER)),
        )
        .with_state(remote)
}

/// `POST /remote/whep` — an SDP offer in, an SDP answer out.
#[cfg(feature = "remote")]
async fn whep_route(State(remote): State<Option<Remote>>, offer: String) -> Response {
    let Some(remote) = remote else {
        return unavailable();
    };
    match remote.answer(&offer).await {
        Ok(answer) => (
            StatusCode::CREATED,
            [
                (header::CONTENT_TYPE, "application/sdp"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            answer,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "remote: could not answer an offer");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CACHE_CONTROL, "no-store")],
                format!("the panel could not accept that: {e}\n"),
            )
                .into_response()
        }
    }
}

#[cfg(not(feature = "remote"))]
#[allow(clippy::unused_async)]
async fn whep_route(State(_): State<Option<Remote>>, offer: String) -> Response {
    let _ = offer;
    unavailable()
}

/// 503 with the reason, not 404 — "no such endpoint" and "this binary has no transport in
/// it" are different problems and only one of them is worth chasing.
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CACHE_CONTROL, "no-store")],
        "this build cannot serve the remote UI; rebuild with the `render` feature\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tower::ServiceExt as _;

    async fn body(response: Response) -> (StatusCode, String) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn the_offer_endpoint_exists_even_with_nothing_behind_it() {
        // Same reason `/stream/*` does: a 404 sends whoever is debugging to look for a
        // typo in the URL, and a 503 with a sentence sends them to the build. The player
        // shows that sentence, because it is what the fetch it just made came back with.
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(WHEP_PATH)
            .body(axum::body::Body::from("v=0\r\n"))
            .unwrap();
        let (status, text) = body(routes(None).oneshot(request).await.unwrap()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(text.contains("cannot serve"), "{text}");
    }

    #[tokio::test]
    async fn an_offer_larger_than_an_offer_is_refused_before_it_is_buffered() {
        // The limit is a layer rather than a length check in the handler: by the time a
        // handler can measure the string, axum has already allocated it.
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(WHEP_PATH)
            .body(axum::body::Body::from("v".repeat(MAX_OFFER + 1)))
            .unwrap();
        let (status, _) = body(routes(None).oneshot(request).await.unwrap()).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn the_player_posts_to_the_route_this_module_serves() {
        // Two constants that have to be the same string, in two different crates now, and
        // nothing but this checks it.
        assert!(pipeline::remote::PLAYER.contains(WHEP_PATH));
    }

    #[test]
    fn there_is_no_page_of_its_own() {
        // The player belongs on the landing page. A second URL serving a second copy is
        // how the two drift.
        let router = routes(None);
        for path in ["/remote/", "/remote"] {
            let request = axum::http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            let response = futures_lite_block(router.clone().oneshot(request));
            assert_eq!(response, StatusCode::NOT_FOUND, "{path}");
        }
    }

    fn futures_lite_block<
        F: std::future::Future<Output = Result<Response, std::convert::Infallible>>,
    >(
        f: F,
    ) -> StatusCode {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
            .unwrap()
            .status()
    }
}
