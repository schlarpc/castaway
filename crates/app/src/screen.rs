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

#[cfg(any(feature = "electron", test))]
use proto_dial::ScreenId;
use proto_dial::ScreenSlot;

#[cfg(feature = "electron")]
use std::time::Duration;
#[cfg(feature = "electron")]
use tracing::{debug, info, warn};

/// YouTube's screen lookup — the same endpoint a phone uses to turn a pairing code into
/// a screen.
#[cfg(feature = "electron")]
const GET_SCREEN: &str = "https://www.youtube.com/api/lounge/pairing/get_screen";

/// How long to keep asking. The page has to load `youtube.com/tv` and register before
/// there is anything to find, which on a cold start is seconds, not milliseconds — and
/// on a slow link, more. Until then the lookup answers 404, which is not an error.
#[cfg(feature = "electron")]
const ATTEMPTS: u32 = 20;
#[cfg(feature = "electron")]
const RETRY_DELAY: Duration = Duration::from_secs(3);

/// A single lookup's ceiling, chosen so a stalled one costs one attempt rather than all
/// of them.
#[cfg(feature = "electron")]
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve the launched page's screen id and publish it into `slot`.
///
/// Best-effort by design: failing to find it costs a sender the attach-without-launch
/// path, not the cast. Anything that launches normally still works, so this logs and
/// gives up rather than failing a launch that already succeeded.
#[cfg(feature = "electron")]
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
#[cfg(feature = "electron")]
async fn fetch(pairing_code: String) -> anyhow::Result<Option<ScreenId>> {
    // ureq is blocking, so it does not belong on the runtime (ground rule 4).
    let body = tokio::task::spawn_blocking(move || {
        // With no timeout — `ureq`'s default — one hung attempt parks this thread for the
        // rest of the process and eats the entire retry budget, so a launch that could
        // have resolved on attempt 2 never gets there.
        let agent = ureq::builder().timeout(LOOKUP_TIMEOUT).build();
        match agent
            .post(GET_SCREEN)
            .send_form(&[("pairing_code", pairing_code.as_str())])
        {
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
#[cfg(not(feature = "electron"))]
pub async fn publish_screen_id(_pairing_code: String, _slot: ScreenSlot) {}

/// Drive DIAL's event stream, keeping at most one screen-id resolver alive.
///
/// The policy, and the reason it is a function rather than a loop inside `main`: each
/// launch used to spawn a resolver with no handle, so a relaunch inside the ~60 s budget
/// left the *old* task polling the *old* pairing code, and whichever finished last won
/// the slot. A stale writer could overwrite the fresh screen id, or refill a slot the stop
/// route had just cleared — which reproduces the exact D28 symptom the slot exists to
/// prevent: a phone that is connected and cannot queue.
///
/// It was fixed in code and exercised by nothing (#96): the only thing that drives a
/// YouTube launch end to end is `yt-selfplay`, which needs the real internet and a person.
/// Everything here is between the receiver and itself, so it needs neither.
///
/// `resolver_for` starts the lookup for a launch and hands back its task, or `None` where
/// there is nothing to resolve (a launch with no pairing code). `on_event` is what the
/// panel does about it — navigate the browser, or hide it.
pub async fn pump_dial(
    events: &mut tokio::sync::mpsc::Receiver<proto_dial::DialEvent>,
    mut resolver_for: impl FnMut(&proto_dial::LaunchParams) -> Option<tokio::task::JoinHandle<()>>,
    mut on_event: impl FnMut(proto_dial::DialEvent),
) {
    let mut resolver: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(event) = events.recv().await {
        match &event {
            proto_dial::DialEvent::Launched(params) => {
                if let Some(task) = resolver.take() {
                    task.abort();
                }
                resolver = resolver_for(params);
            }
            // The page is going away, so a resolver still hunting for its id is hunting
            // for a screen that will not exist.
            proto_dial::DialEvent::Stopped => {
                if let Some(task) = resolver.take() {
                    task.abort();
                }
            }
        }
        on_event(event);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use castaway_test_support::eventually;
    use proto_dial::{DialEvent, DialService};
    use tokio::sync::mpsc;
    use tower::ServiceExt as _;

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

    // -----------------------------------------------------------------------------------
    // A YouTube launch, driven end to end without the internet or a person (#96).
    //
    // Everything positive about DIAL lived in `nix run .#yt-selfplay`, which needs the real
    // internet and an operator — and whose `--reconnect` mode additionally requires a human
    // to have launched a cast by hand first, so the D28 regression test was not
    // self-contained. The tier-2 VM asserts only DIAL's *absence*.
    //
    // A YouTube cast has two halves and the split is structural rather than an oversight:
    // the media plane is a page talking to a third party's Lounge servers, and nothing here
    // can stand in for that. But everything between the phone and the receiver — the
    // launch, the relaunch inside the screen-id window, the `DELETE`, the surface being
    // dismissed and the resolver lifecycle behind them — is ours, and needs no network.
    //
    // The consequence being covered is the silent one: a YouTube cast can regress into
    // connected-but-never-plays — DIAL answers 201, the app reads `running`, the phone
    // shows a session — with no gate noticing.
    // -----------------------------------------------------------------------------------

    /// What the panel was told to do with the browser.
    ///
    /// Stands in for `BrowserCommand`, which is behind the `electron` feature. What is
    /// asserted is the *sequence* — a launch navigates, a stop hides — which is the same
    /// claim either way, and this way the case runs in a default build.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Shown {
        Page(String),
        Nothing,
    }

    fn service() -> (DialService, mpsc::Receiver<DialEvent>) {
        let (tx, rx) = mpsc::channel(8);
        (
            DialService::new(
                "Test TV",
                "http://127.0.0.1:8080/dial",
                "0f8c2e10-0000-4000-8000-0000000c0571",
                tx,
            ),
            rx,
        )
    }

    async fn post(app: &axum::Router, body: &'static str) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dial/apps/YouTube")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    async fn delete(app: &axum::Router) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/dial/apps/YouTube")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    async fn app_info(app: &axum::Router) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/dial/apps/YouTube")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// A resolver that takes a while and then writes its own answer — the shape of the
    /// real one, which waits for a page to register with the Lounge.
    fn slow_resolver(
        slot: ScreenSlot,
        seen: Arc<Mutex<Vec<String>>>,
    ) -> impl FnMut(&proto_dial::LaunchParams) -> Option<tokio::task::JoinHandle<()>> {
        move |params| {
            let code = params.pairing_code.clone()?;
            seen.lock().unwrap().push(code.clone());
            let slot = slot.clone();
            Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                slot.set(ScreenId::parse(&format!("screenfor{code}")).unwrap())
                    .await;
            }))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_launch_puts_the_page_up_and_a_stop_takes_it_down() {
        let (svc, mut events) = service();
        let app = svc.router();

        let shown: Arc<Mutex<Vec<Shown>>> = Arc::default();
        let sink = Arc::clone(&shown);
        tokio::spawn(async move {
            pump_dial(
                &mut events,
                |_| None,
                move |event| {
                    let told = match event {
                        DialEvent::Launched(params) => Shown::Page(params.leanback_url()),
                        DialEvent::Stopped => Shown::Nothing,
                    };
                    sink.lock().unwrap().push(told);
                },
            )
            .await;
        });

        assert_eq!(
            post(&app, "pairingCode=abcd1234").await,
            StatusCode::CREATED
        );
        let opened = eventually("the page being opened", || {
            shown.lock().unwrap().first().cloned()
        })
        .await;
        match opened {
            Shown::Page(url) => assert!(
                url.contains("youtube.com") && url.contains("abcd1234"),
                "the launch has to carry the pairing code to the page: {url}"
            ),
            other => panic!("a launch must open a page, got {other:?}"),
        }

        // …and the phone's stop button takes it away again. Without this the page stays on
        // a two-metre panel after the person who cast to it has left the room.
        assert_eq!(delete(&app).await, StatusCode::OK);
        eventually("the page being dismissed", || {
            shown
                .lock()
                .unwrap()
                .contains(&Shown::Nothing)
                .then_some(())
        })
        .await;
        assert!(
            !app_info(&app).await.contains("running"),
            "a stopped app must not still read as running to the next sender"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_relaunch_inside_the_window_leaves_the_new_screen_id_standing() {
        // G20, fixed in code and exercised by nothing until now. A screen id takes seconds
        // to resolve, so a second launch inside that window used to leave *two* resolvers
        // racing: the old one still polling the old pairing code, the new one polling the
        // new. Whichever finished last decided what the receiver published, and a stale
        // winner is the D28 symptom exactly — the phone sees a screen, attaches to it, and
        // can never queue.
        let (svc, mut events) = service();
        let slot = svc.screen_slot();
        let app = svc.router();

        let codes: Arc<Mutex<Vec<String>>> = Arc::default();
        let resolver = slow_resolver(slot.clone(), Arc::clone(&codes));
        tokio::spawn(async move {
            pump_dial(&mut events, resolver, |_| {}).await;
        });

        assert_eq!(
            post(&app, "pairingCode=stale111").await,
            StatusCode::CREATED
        );
        eventually("the first resolver starting", || {
            (!codes.lock().unwrap().is_empty()).then_some(())
        })
        .await;

        // A second sender arrives before the first resolver has finished. 200 rather
        // than 201: DIAL says a launch of an app that is already running is not a
        // creation, and `proto-dial` answers accordingly — which is exactly the case
        // that leaves two resolvers in flight.
        assert_eq!(post(&app, "pairingCode=fresh222").await, StatusCode::OK);
        eventually("the second resolver starting", || {
            (codes.lock().unwrap().len() == 2).then_some(())
        })
        .await;

        // Past the resolver's 300 ms write — virtual, so this costs nothing and cannot
        // be outrun by a loaded box: a first resolver still running *has* written by now.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let published = app_info(&app).await;
        assert!(
            published.contains("screenforfresh222"),
            "the live page's screen must be the one published: {published}"
        );
        assert!(
            !published.contains("screenforstale111"),
            "an aborted resolver must not win the slot: {published}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_sender_attaches_to_the_running_page_without_disturbing_it() {
        // The attach-without-launch path, which is the whole reason the screen id is
        // published at all (#96). One phone casts; a second person opens YouTube, finds
        // the receiver already running it, reads the screen id out of the app-info XML and
        // joins that Lounge session directly — no `POST`, so no relaunch.
        //
        // Two things have to hold, and only one of them is about the XML. The id has to be
        // there, or the second phone has no way in and the feature silently is not one.
        // And reading it must not *do* anything: a `GET` that disturbed the session would
        // interrupt the person already watching, which is a worse failure than not
        // offering the attach at all.
        let (svc, mut events) = service();
        let slot = svc.screen_slot();
        let app = svc.router();

        let shown: Arc<Mutex<Vec<Shown>>> = Arc::default();
        let sink = Arc::clone(&shown);
        let resolver = slow_resolver(slot.clone(), Arc::default());
        tokio::spawn(async move {
            pump_dial(&mut events, resolver, move |event| {
                let told = match event {
                    DialEvent::Launched(params) => Shown::Page(params.leanback_url()),
                    DialEvent::Stopped => Shown::Nothing,
                };
                sink.lock().unwrap().push(told);
            })
            .await;
        });

        assert_eq!(
            post(&app, "pairingCode=firstcast").await,
            StatusCode::CREATED
        );
        eventually("the screen id being resolved", || {
            shown.lock().unwrap().first().cloned()
        })
        .await;
        // Past the resolver's 300 ms write, in virtual time.
        tokio::time::sleep(Duration::from_millis(600)).await;

        // What the second sender reads.
        let info = app_info(&app).await;
        assert!(
            info.contains("running"),
            "a second sender has to see the app running, or it launches its own: {info}"
        );
        assert!(
            info.contains("screenforfirstcast"),
            "…and the screen id it needs to attach: {info}"
        );

        // Reading it twice more is what a second and third sender actually do.
        let again = app_info(&app).await;
        let _third = app_info(&app).await;
        assert_eq!(again, info, "app-info must be a read, not a transition");
        assert_eq!(
            *shown.lock().unwrap(),
            vec![Shown::Page(
                "https://www.youtube.com/tv?pairingCode=firstcast".into()
            )],
            "an attaching sender must not navigate or hide the page the first one is \
             watching"
        );
        assert_eq!(
            slot.get().await.map(|id| id.as_str().to_owned()),
            Some("screenforfirstcast".into()),
            "and the id it attached to must still be the live page's"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_calls_off_the_resolver_still_hunting_for_a_screen() {
        // The other half of the same rule. A `DELETE` clears the slot; a resolver still
        // running would refill it seconds later, and the receiver would then advertise a
        // screen for a page that is not on the panel — a sender attaches to it and waits
        // for a video with nowhere to play.
        let (svc, mut events) = service();
        let slot = svc.screen_slot();
        let app = svc.router();

        let resolver = slow_resolver(slot.clone(), Arc::default());
        tokio::spawn(async move {
            pump_dial(&mut events, resolver, |_| {}).await;
        });

        assert_eq!(
            post(&app, "pairingCode=doomed11").await,
            StatusCode::CREATED
        );
        assert_eq!(delete(&app).await, StatusCode::OK);

        // Past when that resolver would have written — virtual, so waiting out the whole
        // window is free.
        tokio::time::sleep(Duration::from_millis(600)).await;
        // Asserted on the *slot*, not on the app info. A stopped app publishes no screen
        // id whatever the slot holds, so reading the XML here would pass on a receiver
        // whose slot had been quietly refilled — and the refill is the bug: the id sits
        // there until the next launch, which then publishes the dead page's screen to the
        // sender that just arrived.
        assert!(
            slot.get().await.is_none(),
            "a stopped page's resolver must not refill the slot: {:?}",
            slot.get().await.map(|id| id.as_str().to_owned())
        );
        let after = app_info(&app).await;
        assert!(
            !after.contains("screenfordoomed11"),
            "and nothing may publish it either: {after}"
        );
    }
}
