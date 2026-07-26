//! The SponsorBlock actor: attach to our own screen as a remote control, watch what it
//! plays, and seek past the sponsors.
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

use std::time::{Duration, Instant};

use castaway_core::OsdSink;
use proto_dial::lounge::sender::{SenderIdentity, Unbound};
use proto_dial::{parse_chunks, LoungeCommand, ScreenSlot};
use sponsorblock::{Decision, Planner, Segment, VideoId};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::SponsorBlock as SponsorBlockConfig;

const LOUNGE: &str = "https://www.youtube.com/api/lounge";
/// How long a skip toast stays up. Long enough to read, short enough not to sit over the
/// video someone actually wants to watch.
const TOAST: Duration = Duration::from_secs(3);

/// Run the actor until the process ends. Never returns an error to its caller: a receiver
/// that cannot reach SponsorBlock is a receiver that plays sponsors, not a broken one.
pub async fn run(config: SponsorBlockConfig, screen: ScreenSlot, osd: OsdSink) {
    info!(
        categories = ?config.categories,
        "SponsorBlock: {}",
        sponsorblock::ATTRIBUTION
    );
    loop {
        match session(&config, &screen, &osd).await {
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
) -> anyhow::Result<()> {
    let screen_id = wait_for_screen(screen).await;
    let token = lounge_token(&screen_id).await?;

    let identity = SenderIdentity {
        device_id: uuid::Uuid::new_v4().to_string(),
        name: "castaway SponsorBlock".to_string(),
    };
    // The RID seed is ours to choose; the Lounge only requires that it advance.
    let mut unbound = Unbound::new(token, identity, 20000);
    let request = unbound.bind_request();
    let response = post(&format!("{LOUNGE}/bc/bind?{}", request.query), request.body).await?;
    let mut sender = unbound.bound(&response)?;
    info!(screen = screen_id, "SponsorBlock attached to the screen");

    let (commands_tx, mut commands) = mpsc::channel::<LoungeCommand>(64);
    let receive = sender.receive_request();
    tokio::task::spawn_blocking(move || stream_channel(&receive.query, &commands_tx));

    let mut planner = Planner::new();
    let mut clock: Option<PlaybackClock> = None;
    // `None` means "nothing scheduled"; a far-future sleep is simpler than juggling
    // Option<Sleep> in the select below.
    let mut due = Duration::from_secs(3600);

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()) };
                sender.observe(&command);
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

/// What a `nowPlaying`/`onStateChange` tells us about playback.
struct PlaybackUpdate {
    video: VideoId,
    position: Duration,
    playing: bool,
}

/// Read a command for a video id and a position. Commands that carry neither are not
/// playback news.
fn playback_update(command: &LoungeCommand) -> Option<PlaybackUpdate> {
    if !matches!(command.name.as_str(), "nowPlaying" | "onStateChange") {
        return None;
    }
    let payload = command.payload.as_object()?;
    let video = payload.get("videoId").and_then(|v| v.as_str())?;
    let position = payload
        .get("currentTime")
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| v.as_f64())
        })
        .unwrap_or(0.0);
    // State 1 is playing. The leanback app also reports states outside the documented
    // set (1081 has been seen with playback plainly running), so treat anything that is
    // not an explicit pause/buffer as running and let the clock be corrected by the next
    // report — a paused clock that should be running only delays a skip.
    let state = payload.get("state").and_then(|v| v.as_str()).unwrap_or("1");
    let playing = !matches!(state, "2" | "3" | "-1" | "0" | "5");
    Some(PlaybackUpdate {
        video: VideoId::parse(video).ok()?,
        position: Duration::from_secs_f64(position.max(0.0)),
        playing,
    })
}

/// Dead reckoning between position reports.
struct PlaybackClock {
    reported: Duration,
    at: Instant,
    playing: bool,
}

impl PlaybackClock {
    fn new(reported: Duration, playing: bool) -> Self {
        Self {
            reported,
            at: Instant::now(),
            playing,
        }
    }

    fn position(&self) -> Duration {
        if self.playing {
            self.reported + self.at.elapsed()
        } else {
            self.reported
        }
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

// --- blocking HTTP, kept off the runtime (ground rule 4) ---

// `ureq::Error` is 272 bytes, so it is flattened into `anyhow` inside each closure rather
// than carried out through the `Result` (clippy's `result_large_err`).
async fn post(url: &str, body: String) -> anyhow::Result<String> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let response = ureq::post(&url)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&body)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(response.into_string()?)
    })
    .await?
}

/// `Ok(None)` for a 404 — "no segments for this video", not a failure.
async fn get(url: &str) -> anyhow::Result<Option<String>> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
        match ureq::get(&url).set("User-Agent", USER_AGENT).call() {
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
    let response = match ureq::get(&url).call() {
        Ok(response) => response,
        Err(e) => {
            warn!(error = %e, "SponsorBlock receive channel refused");
            return;
        }
    };
    let mut reader = response.into_reader();
    let mut buffered = String::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buffered.push_str(&String::from_utf8_lossy(&chunk[..read]));
        let Ok(commands) = parse_chunks(&buffered) else {
            // A partial chunk is not a protocol error; wait for the rest.
            continue;
        };
        if commands.is_empty() {
            continue;
        }
        buffered.clear();
        for command in commands {
            if out.blocking_send(command).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    fn command(name: &str, payload: serde_json::Value) -> LoungeCommand {
        LoungeCommand {
            aid: 1,
            name: name.into(),
            payload,
        }
    }

    #[test]
    fn reads_position_and_video_out_of_now_playing() {
        // The Lounge sends numbers as strings.
        let update = playback_update(&command(
            "nowPlaying",
            json!({"videoId": "dQw4w9WgXcQ", "currentTime": "42.5", "state": "1"}),
        ))
        .unwrap();
        assert_eq!(update.video.as_str(), "dQw4w9WgXcQ");
        assert_eq!(update.position, Duration::from_secs_f64(42.5));
        assert!(update.playing);
    }

    #[test]
    fn an_undocumented_state_still_counts_as_playing() {
        // 1081 is real and means playing; treating it as stopped would freeze the clock
        // and skip nothing for the whole video.
        let update = playback_update(&command(
            "onStateChange",
            json!({"videoId": "dQw4w9WgXcQ", "currentTime": "3", "state": "1081"}),
        ))
        .unwrap();
        assert!(update.playing);

        let paused = playback_update(&command(
            "onStateChange",
            json!({"videoId": "dQw4w9WgXcQ", "currentTime": "3", "state": "2"}),
        ))
        .unwrap();
        assert!(!paused.playing);
    }

    #[test]
    fn commands_without_playback_news_are_ignored() {
        assert!(playback_update(&command("onVolumeChanged", json!({"volume": "100"}))).is_none());
        // A playlist id is not a video id, and hashing one would look up an empty bucket.
        assert!(playback_update(&command(
            "nowPlaying",
            json!({"videoId": "RQ_notavideoid", "currentTime": "0"})
        ))
        .is_none());
        assert!(playback_update(&command("nowPlaying", json!({}))).is_none());
    }

    #[test]
    fn a_paused_clock_does_not_advance() {
        let clock = PlaybackClock::new(Duration::from_secs(10), false);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(clock.position(), Duration::from_secs(10));
    }

    #[test]
    fn a_playing_clock_runs_between_reports() {
        // The whole point of dead reckoning: positions arrive on change, not on a tick.
        let clock = PlaybackClock::new(Duration::from_secs(10), true);
        std::thread::sleep(Duration::from_millis(30));
        assert!(clock.position() > Duration::from_secs(10));
    }
}
