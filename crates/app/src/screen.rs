//! Learning the screen id of the page we just launched, so senders can attach to it.
//!
//! A DIAL launch hands the page a pairing code and the page registers itself with
//! YouTube's Lounge under that code. The same lookup a sender would do — `get_screen`
//! with the pairing code — tells *us* which screen the page became, and that id is what
//! `proto-dial` publishes in its app-info XML so a sender arriving later can attach
//! without a launch of its own (see `ScreenId`).
//!
//! This is the I/O half of that, kept out of `proto-dial` on purpose (ground rule 3): the
//! protocol crate holds and renders the id, and the network call to find it lives here,
//! in the crate that is allowed to do wiring and `anyhow`.

#[cfg(any(feature = "cef", test))]
use proto_dial::ScreenId;
use proto_dial::ScreenSlot;

#[cfg(feature = "cef")]
use std::time::Duration;
#[cfg(feature = "cef")]
use tracing::{debug, info, warn};

/// YouTube's screen lookup — the same endpoint a phone uses to turn a pairing code into
/// a screen.
#[cfg(feature = "cef")]
const GET_SCREEN: &str = "https://www.youtube.com/api/lounge/pairing/get_screen";

/// How long to keep asking. The page has to load `youtube.com/tv` and register before
/// there is anything to find, which on a cold start is seconds, not milliseconds — and
/// on a slow link, more. Until then the lookup answers 404, which is not an error.
#[cfg(feature = "cef")]
const ATTEMPTS: u32 = 20;
#[cfg(feature = "cef")]
const RETRY_DELAY: Duration = Duration::from_secs(3);

/// Resolve the launched page's screen id and publish it into `slot`.
///
/// Best-effort by design: failing to find it costs a sender the attach-without-launch
/// path, not the cast. Anything that launches normally still works, so this logs and
/// gives up rather than failing a launch that already succeeded.
#[cfg(feature = "cef")]
pub async fn publish_screen_id(pairing_code: String, slot: ScreenSlot) {
    for attempt in 1..=ATTEMPTS {
        match fetch(pairing_code.clone()).await {
            Ok(Some(id)) => {
                info!(
                    screen = id.as_str(),
                    attempt, "screen registered; senders can now attach without a launch"
                );
                slot.set(id).await;
                return;
            }
            Ok(None) => debug!(attempt, "page has not registered with the Lounge yet"),
            Err(e) => warn!(error = %e, attempt, "screen lookup failed"),
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
    warn!(
        "gave up resolving the screen id; a sender that finds YouTube already running \
         will have no way to attach to it"
    );
}

/// One lookup. `Ok(None)` means "not registered yet" — the 404 this answers with until
/// the page has claimed the code.
#[cfg(feature = "cef")]
async fn fetch(pairing_code: String) -> anyhow::Result<Option<ScreenId>> {
    // ureq is blocking, so it does not belong on the runtime (ground rule 4).
    let body = tokio::task::spawn_blocking(move || {
        match ureq::post(GET_SCREEN).send_form(&[("pairing_code", pairing_code.as_str())]) {
            Ok(response) => response
                .into_string()
                .map(Some)
                .map_err(anyhow::Error::from),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(anyhow::Error::from(e)),
        }
    })
    .await??;

    let Some(body) = body else { return Ok(None) };
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    let Some(raw) = parsed
        .get("screen")
        .and_then(|s| s.get("screenId"))
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    Ok(Some(ScreenId::parse(raw)?))
}

/// Without a browser there is no page, so there is no screen to find — and DIAL is not
/// mounted at all in that build. Present so the call site needs no `cfg`.
#[cfg(not(feature = "cef"))]
pub async fn publish_screen_id(_pairing_code: String, _slot: ScreenSlot) {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn an_unresolved_slot_publishes_nothing() {
        // The no-op build must still leave the slot empty rather than, say, a placeholder
        // a sender would try to attach to.
        let slot = ScreenSlot::default();
        assert!(slot.get().await.is_none());
        slot.set(ScreenId::parse("abc123").unwrap()).await;
        assert_eq!(slot.get().await.unwrap().as_str(), "abc123");
        slot.clear().await;
        assert!(slot.get().await.is_none());
    }
}
