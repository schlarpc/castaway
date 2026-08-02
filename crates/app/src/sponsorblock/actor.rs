//! The I/O half: HTTP, the long poll, and the timer that fires a skip.
//!
//! Everything it decides with lives next door in the parent module (pure, always compiled
//! and always tested) or in the `sponsorblock` crate. This file is what needs a network.
//!
//! The receiver drives the page the same way a phone does — there is no privileged path
//! into the leanback player, and injecting JavaScript into YouTube's minified app would
//! break on their schedule rather than ours. So we bind a second Lounge session as a
//! `REMOTE_CONTROL`, using the screen id we already resolve for DIAL, and send `seekTo`.
//!
//! Everything decidable is decided elsewhere: `sponsorblock` owns which segments matter
//! and when a position is inside one, `proto_dial::lounge` owns the framing in both
//! directions. What is left here is genuinely I/O — HTTP, a long poll, and a timer.
//!
//! Position tracking is dead reckoning, and has to be: the screen pushes `nowPlaying` on
//! change, not on a tick, so between reports we extrapolate from the last known position
//! and the wall clock. That is what lets a skip fire *at* a segment boundary instead of
//! whenever the next event happens to arrive.

use std::time::Duration;

use castaway_core::OsdSink;
use proto_dial::lounge::sender::{SenderIdentity, Unbound};
use proto_dial::{parse_chunks, LoungeCommand, ScreenSlot};
use sponsorblock::{Decision, Planner, Segment, VideoId};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{ad_update, playback_update, PlaybackClock};
use crate::config::SponsorBlock as SponsorBlockConfig;

const LOUNGE: &str = "https://www.youtube.com/api/lounge";
/// How long a skip toast stays up. Long enough to read, short enough not to sit over the
/// video someone actually wants to watch.
const TOAST: Duration = Duration::from_secs(3);

/// How long the screen may sit with no video before the panel takes itself back.
///
/// "Ready to cast" with an empty queue is nobody watching anything. The clock is on the
/// *video-less* state, not on channel silence: browsing the leanback UI by remote
/// generates plenty of traffic and none of it resets this, so three minutes is the budget
/// for picking something to watch, not for idling. Only a `nowPlaying` that names a video
/// clears it. A *paused* video never counts as idle — the screen still has a video, and
/// someone means to come back.
const IDLE_EXIT: Duration = Duration::from_secs(180);

/// Run the actor until the process ends. Never returns an error to its caller: a receiver
/// that cannot reach SponsorBlock is a receiver that plays sponsors, not a broken one.
pub async fn run(
    config: SponsorBlockConfig,
    screen: ScreenSlot,
    osd: OsdSink,
    on_idle: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
) {
    info!(
        categories = ?config.categories,
        "SponsorBlock: {}",
        sponsorblock::ATTRIBUTION
    );
    // One identity for the life of the process, not one per attach.
    //
    // The channel's long poll returns EOF routinely — BrowserChannel does it as a matter
    // of course — so a fresh `Uuid::new_v4()` per cycle meant the Lounge's connected-device
    // list filled up with "castaway SponsorBlock" entries, each visible in the phone's cast
    // UI, and YouTube may put a connect toast on screen for every one. A stable id makes a
    // reattach look like what it is: the same remote, still here.
    let identity = SenderIdentity {
        device_id: uuid::Uuid::new_v4().to_string(),
        name: "castaway SponsorBlock".to_string(),
    };
    // Segment state outlives the channel for the same reason. It belongs to the *video*,
    // not to the connection: rebuilding it per reattach re-fetched the segments and
    // forgot which had already been skipped, so a reattach mid-video could skip a
    // sponsor the viewer had already sat through.
    let mut planner = Planner::new();
    // When the screen last became video-less, carried *across* channel re-attaches: the
    // long-poll EOFs and rebinds every couple of minutes as a matter of course, and an
    // idle clock that restarted with the channel would never reach its deadline.
    let mut idle_since: Option<tokio::time::Instant> = None;
    loop {
        match session(
            &config,
            &screen,
            &osd,
            &identity,
            &mut planner,
            on_idle.as_deref(),
            &mut idle_since,
        )
        .await
        {
            Ok(()) => debug!("SponsorBlock session ended; will re-attach"),
            Err(e) => warn!(error = %e, "SponsorBlock session failed; will re-attach"),
        }
        // A dropped channel is normal — the screen restarts on every DIAL launch.
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// One attach-and-watch cycle: wait for a screen, bind to it, and follow it until the
/// channel closes.
async fn session(
    config: &SponsorBlockConfig,
    screen: &ScreenSlot,
    osd: &OsdSink,
    identity: &SenderIdentity,
    planner: &mut Planner,
    on_idle: Option<&(dyn Fn() + Send + Sync)>,
    idle_since: &mut Option<tokio::time::Instant>,
) -> anyhow::Result<()> {
    let screen_id = wait_for_screen(screen).await;
    let token = lounge_token(&screen_id).await?;

    // The RID seed is ours to choose; the Lounge only requires that it advance.
    let mut unbound = Unbound::new(token, identity.clone(), 20000);
    let request = unbound.bind_request();
    let response = post(&format!("{LOUNGE}/bc/bind?{}", request.query), request.body).await?;
    let mut sender = unbound.bound(&response)?;
    info!(screen = screen_id, "SponsorBlock attached to the screen");

    let (commands_tx, mut commands) = mpsc::channel::<LoungeCommand>(64);
    let receive = sender.receive_request();
    tokio::task::spawn_blocking(move || stream_channel(&receive.query, &commands_tx));

    let mut clock: Option<PlaybackClock> = None;
    // One press per ad: the screen reports the state repeatedly while the button is up,
    // and a press per report would be a burst of commands for one skip.
    let mut ad_skip_sent = false;
    // `None` means "nothing scheduled"; a far-future sleep is simpler than juggling
    // Option<Sleep> in the select below.
    let mut due = Duration::from_secs(3600);
    // A screen that just appeared with nothing on it starts its idle clock now — the
    // launch splash counts. `get_or_insert` and not `=`: a re-attach mid-idle must not
    // grant the splash another three minutes.
    idle_since.get_or_insert_with(tokio::time::Instant::now);

    loop {
        let idle_deadline = idle_since.map(|since| since + IDLE_EXIT);
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()) };
                // `nowPlaying` states whether the screen holds a video at all, every
                // time it fires. With one, the screen is not idle — a *paused* video is
                // someone coming back. Without one — the "Ready to cast" splash, or a
                // queue that ran out — the idle clock starts, once: later video-less
                // reports do not push the deadline back.
                if command.name == "nowPlaying" {
                    let has_video = command
                        .payload
                        .get("videoId")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|id| !id.is_empty());
                    if has_video {
                        *idle_since = None;
                    } else {
                        idle_since.get_or_insert_with(tokio::time::Instant::now);
                    }
                }
                sender.observe(&command);
                if let Some(ad) = ad_update(&command) {
                    if ad.started {
                        ad_skip_sent = false;
                    }
                    // Freeze the content clock. Positions reported during an ad belong to
                    // the *ad*, and dead reckoning straight through a 15-second break
                    // would leave our idea of the content position that far ahead of the
                    // video — skipping a segment that has not been reached, or missing one
                    // it thinks is behind us.
                    if let Some(clock) = clock.as_mut() {
                        clock.pause();
                    }
                    if config.skip_ads && ad.skip_enabled && !ad_skip_sent {
                        ad_skip_sent = true;
                        skip_ad(&mut sender, osd, config).await;
                    }
                    continue;
                }
                if let Some(update) = playback_update(&command) {
                    if planner.video() != Some(&update.video) {
                        let segments = lookup(config, &update.video).await;
                        info!(
                            video = update.video.as_str(),
                            segments = segments.len(),
                            "SponsorBlock segments loaded"
                        );
                        planner.load(update.video.clone(), segments);
                    }
                    clock = Some(PlaybackClock::new(update.position, update.playing));
                }
            }
            () = tokio::time::sleep(due) => {}
            () = async {
                match idle_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            }, if on_idle.is_some() => {
                info!(
                    idle = ?IDLE_EXIT,
                    "YouTube screen has no video and nobody is driving it; \
                     returning the panel to the home screen"
                );
                *idle_since = None;
                if let Some(idle) = on_idle {
                    idle();
                }
                return Ok(());
            }
        }

        let Some(current) = clock.as_ref().map(PlaybackClock::position) else {
            continue;
        };
        match planner.decide(current) {
            Decision::Skip { to, segment } => {
                skip(&mut sender, to, &segment, osd, config).await;
                // Believe the seek immediately rather than waiting to be told: the next
                // report is one round trip away, and a stale position would re-evaluate
                // against where we just left.
                clock = Some(PlaybackClock::new(to, true));
                due = Duration::from_secs(3600);
            }
            Decision::WaitUntil(position) => {
                due = position
                    .saturating_sub(current)
                    .max(Duration::from_millis(50));
            }
            Decision::Idle => due = Duration::from_secs(3600),
        }
    }
}

/// Press the screen's skip button and say so on the overlay.
async fn skip_ad(
    sender: &mut proto_dial::lounge::sender::Bound,
    osd: &OsdSink,
    config: &SponsorBlockConfig,
) {
    let request = sender.skip_ad();
    match post(&format!("{LOUNGE}/bc/bind?{}", request.query), request.body).await {
        Ok(_) => {
            info!("skipped a YouTube ad");
            if config.toast {
                osd.banner("Skipped ad", TOAST);
            }
        }
        Err(e) => warn!(error = %e, "skipAd failed; the ad plays"),
    }
}

/// Seek past a segment and say so on the overlay.
async fn skip(
    sender: &mut proto_dial::lounge::sender::Bound,
    to: Duration,
    segment: &Segment,
    osd: &OsdSink,
    config: &SponsorBlockConfig,
) {
    let request = sender.seek_to(to.as_secs_f64());
    match post(&format!("{LOUNGE}/bc/bind?{}", request.query), request.body).await {
        Ok(_) => {
            let seconds = segment.len().as_secs().max(1);
            info!(
                category = segment.category.describe(),
                seconds, "SponsorBlock skipped a segment"
            );
            if config.toast {
                osd.banner(
                    format!("Skipped {} · {seconds}s", segment.category.describe()),
                    TOAST,
                );
            }
        }
        // The planner has already marked it skipped, which is the right call: retrying a
        // seek whose window has passed would yank the video backwards.
        Err(e) => warn!(error = %e, "SponsorBlock seek failed; leaving the segment be"),
    }
}

// --- blocking HTTP, kept off the runtime (ground rule 4) ---

// `ureq::Error` is 272 bytes, so it is flattened into `anyhow` inside each closure rather
// than carried out through the `Result` (clippy's `result_large_err`).
async fn post(url: &str, body: String) -> anyhow::Result<String> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let response = request_agent()
            .post(&url)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&body)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(response.into_string()?)
    })
    .await?
}

/// How long a one-shot request may take before we treat it as lost.
///
/// The default `ureq` agent has *no* timeout at all, which is not a slow request — it is
/// a blocking thread parked in `read()` for the rest of the process. That thread is the
/// one feeding `commands`, so the receiver never errors, the session loop never returns,
/// and the reattach path never fires: sponsor skipping stops for good, with at most one
/// warning line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A long poll idles by design, so it gets a longer leash than a one-shot request — but
/// still a finite one. The Lounge sends noop heartbeats, so silence past this is a
/// connection that went away without saying so.
const CHANNEL_READ_TIMEOUT: Duration = Duration::from_secs(90);

fn request_agent() -> ureq::Agent {
    ureq::builder().timeout(REQUEST_TIMEOUT).build()
}

/// `Ok(None)` for a 404 — "no segments for this video", not a failure.
async fn get(url: &str) -> anyhow::Result<Option<String>> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
        match request_agent()
            .get(&url)
            .set("User-Agent", USER_AGENT)
            .call()
        {
            Ok(response) => Ok(Some(response.into_string()?)),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    })
    .await?
}

/// Identify ourselves to SponsorBlock's API, as its docs ask clients to.
const USER_AGENT: &str = concat!("castaway/", env!("CARGO_PKG_VERSION"));

/// Stream the receive channel on a blocking thread, forwarding each parsed command.
///
/// The channel is a long poll that never closes on its own, so this reads incrementally
/// and keeps a partial trailing chunk buffered rather than waiting for EOF.
fn stream_channel(query: &str, out: &mpsc::Sender<LoungeCommand>) {
    use std::io::Read as _;

    let url = format!("{LOUNGE}/bc/bind?{query}");
    let agent = ureq::builder()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(CHANNEL_READ_TIMEOUT)
        .build();
    let response = match agent.get(&url).call() {
        Ok(response) => response,
        Err(e) => {
            warn!(error = %e, "SponsorBlock receive channel refused");
            return;
        }
    };
    let mut reader = response.into_reader();
    // Bytes, not a String, and that is the whole point. The framing is *character*
    // counted, and `String::from_utf8_lossy` over an arbitrary 8192-byte read boundary
    // replaces a split multi-byte sequence with U+FFFD — one character where there should
    // have been one character, but the wrong one, and the bytes after it shifted. Every
    // subsequent length prefix is then misaligned, permanently. A phone with an emoji in
    // its name is enough to trigger it, via the device list in `loungeStatus`.
    let mut buffered: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buffered.extend_from_slice(&chunk[..read]);
        if buffered.len() > MAX_BUFFERED {
            // Previously this grew without limit: a parse failure `continue`d without
            // clearing, so a genuinely malformed frame was re-parsed forever against an
            // ever-larger buffer. Bailing out lets the session reattach, which is the
            // recovery path that already exists.
            warn!(
                buffered = buffered.len(),
                "SponsorBlock channel is not framed as expected; reattaching"
            );
            return;
        }

        // Only decode as far as the last *complete* character.
        let Some(valid_to) = decodable_prefix(&buffered) else {
            warn!("SponsorBlock channel sent invalid UTF-8; reattaching");
            return;
        };
        let Ok(text) = std::str::from_utf8(&buffered[..valid_to]) else {
            return;
        };
        let Ok(commands) = parse_chunks(text) else {
            // A partial chunk is not a protocol error; wait for the rest.
            continue;
        };
        if commands.is_empty() {
            continue;
        }
        // `parse_chunks` only succeeds when it consumed everything it was given, so what
        // remains is exactly the incomplete tail.
        buffered.drain(..valid_to);
        for command in commands {
            if out.blocking_send(command).is_err() {
                return;
            }
        }
    }
}

/// How much unparsed channel data to hold before concluding the stream is not framed the
/// way we think it is.
const MAX_BUFFERED: usize = 1 << 20;

/// How many leading bytes of `buffered` form complete characters.
///
/// `None` means the stream is not UTF-8 at all, which is not something waiting will fix.
///
/// Split out from the read loop because the bug it prevents is invisible at the call
/// site: decoding a partial multi-byte sequence lossily yields a *plausible* string with
/// one wrong character in it, and the Lounge framing counts characters, so every length
/// prefix after that point is off by however many bytes were swallowed. There is no
/// recovery — the channel is desynchronised for as long as it stays open.
fn decodable_prefix(buffered: &[u8]) -> Option<usize> {
    match std::str::from_utf8(buffered) {
        Ok(_) => Some(buffered.len()),
        // Cut short mid-character: the rest is coming in the next read.
        Err(e) if e.error_len().is_none() => Some(e.valid_up_to()),
        Err(_) => None,
    }
}

/// Block until the DIAL side has a screen for us to attach to.
async fn wait_for_screen(screen: &ScreenSlot) -> String {
    loop {
        if let Some(id) = screen.get().await {
            return id.as_str().to_string();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Mint a lounge token for a screen id — the same call a returning phone makes.
async fn lounge_token(screen_id: &str) -> anyhow::Result<String> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("screen_ids", screen_id)
        .finish();
    let response = post(&format!("{LOUNGE}/pairing/get_lounge_token_batch"), body).await?;
    let parsed: serde_json::Value = serde_json::from_str(&response)?;
    parsed
        .get("screens")
        .and_then(|s| s.get(0))
        .and_then(|s| s.get("loungeToken"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no lounge token for screen {screen_id}"))
}

/// Look up a video's segments. Failure means "skip nothing", never "stop playing".
async fn lookup(config: &SponsorBlockConfig, video: &VideoId) -> Vec<Segment> {
    let url = sponsorblock::lookup_url(video, &config.categories);
    let minimum = Duration::from_secs_f64(config.minimum_seconds.max(0.0));
    match get(&url).await {
        // 404 is the ordinary answer for a video nobody has submitted.
        Ok(None) => Vec::new(),
        Ok(Some(body)) => {
            match sponsorblock::segment::parse_response(&body, video, &config.categories, minimum) {
                Ok(segments) => segments,
                Err(e) => {
                    warn!(error = %e, "SponsorBlock response did not parse");
                    Vec::new()
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "SponsorBlock lookup failed");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{decodable_prefix, MAX_BUFFERED};
    use proto_dial::parse_chunks;

    /// Feed `body` through the read loop's buffering in `size`-byte reads and return what
    /// the parser ends up seeing.
    fn stream_in_reads(body: &str, size: usize) -> Vec<String> {
        let mut buffered: Vec<u8> = Vec::new();
        let mut seen = Vec::new();
        for piece in body.as_bytes().chunks(size) {
            buffered.extend_from_slice(piece);
            assert!(buffered.len() <= MAX_BUFFERED);
            let valid_to = decodable_prefix(&buffered).expect("valid utf-8 overall");
            let text = std::str::from_utf8(&buffered[..valid_to]).expect("prefix decodes");
            let Ok(commands) = parse_chunks(text) else {
                continue;
            };
            if commands.is_empty() {
                continue;
            }
            buffered.drain(..valid_to);
            seen.extend(commands.into_iter().map(|c| c.name));
        }
        seen
    }

    /// A chunk is a character count, a newline, then that many characters of JSON.
    fn chunk(json: &str) -> String {
        format!("{}\n{json}", json.chars().count())
    }

    #[test]
    fn a_multi_byte_character_split_across_reads_does_not_desynchronise_the_channel() {
        // The failure this exists to prevent, and it is permanent rather than transient:
        // `from_utf8_lossy` on a read boundary turns half an emoji into U+FFFD, the
        // framing counts characters, and every length prefix after that is misaligned for
        // as long as the channel stays open. Sponsor skipping stops for the rest of the
        // process. A phone with an emoji in its name is enough, via `loungeStatus`.
        let body = format!(
            "{}{}",
            chunk(r#"[[1,["loungeStatus",{"devices":"🎧 Chaz's Phone"}]]]"#),
            chunk(r#"[[2,["nowPlaying",{"videoId":"dQw4w9WgXcQ"}]]]"#),
        );
        // One byte at a time is the worst case, and guarantees the split.
        assert_eq!(
            stream_in_reads(&body, 1),
            vec!["loungeStatus".to_owned(), "nowPlaying".to_owned()]
        );
    }

    #[test]
    fn the_same_body_parses_the_same_however_the_reads_fall() {
        let body = format!(
            "{}{}",
            chunk(r#"[[7,["onStateChange",{"state":"1","current_time":"12.5"}]]]"#),
            chunk(r#"[[8,["nowPlaying",{"videoId":"héllo·wörld"}]]]"#),
        );
        let whole = stream_in_reads(&body, body.len());
        for size in [1, 2, 3, 5, 8, 13, 64] {
            assert_eq!(stream_in_reads(&body, size), whole, "reads of {size} bytes");
        }
    }

    #[test]
    fn invalid_utf_8_is_reported_rather_than_waited_on() {
        // Distinct from a split character: no amount of further reading fixes this, so
        // the loop has to give up and let the session reattach instead of buffering to
        // the cap.
        assert_eq!(decodable_prefix(b"ok"), Some(2));
        assert_eq!(decodable_prefix(&[0xE2, 0x9C]), Some(0), "split, wait");
        assert_eq!(decodable_prefix(&[b'h', b'i', 0xE2, 0x9C]), Some(2));
        assert_eq!(decodable_prefix(&[0xFF, 0xFE]), None, "not utf-8 at all");
    }
}
