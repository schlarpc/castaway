//! Reading what the screen is doing, and skipping what nobody wants to watch.
//!
//! The receiver drives the page the same way a phone does — there is no privileged path
//! into the leanback player, and injecting JavaScript into YouTube's minified app would
//! break on their schedule rather than ours. So we bind a second Lounge session as a
//! `REMOTE_CONTROL`, using the screen id we already resolve for DIAL, and send `seekTo`
//! for sponsor segments and `skipAd` for ads that let themselves be skipped.
//!
//! This module is the part with no network in it: turning Lounge payloads into playback
//! and ad observations, and keeping a clock between them. It is compiled and tested in
//! every build, including the browser-less one, because getting these payloads wrong is
//! not something to discover on a wall display. [`actor`] is the half that needs a
//! socket, and only exists where there is a page to drive.

// Nothing in a browser-less build calls these — there is no page to watch — but they are
// still compiled and still tested, because misreading a payload is not something to find
// out on a wall display.
#![cfg_attr(not(feature = "electron"), allow(dead_code))]

use std::time::{Duration, Instant};

use proto_dial::LoungeCommand;
use sponsorblock::VideoId;

#[cfg(feature = "electron")]
mod actor;
#[cfg(feature = "electron")]
pub use actor::run;

/// What an ad event tells us. Field names and their string-typed booleans are taken from
/// a captured session, not from documentation:
///
/// ```text
/// adPlaying {"duration":"15.241","adState":"-1","isBumper":"false","isSkippable":"false",
///            "isSkipEnabled":"false","adVideoId":"…","contentVideoId":"…"}
/// onAdStateChange {"currentTime":"0","adState":"1","isSkipEnabled":"false"}
/// ```
struct AdUpdate {
    /// The skip button is live *now*. Before the countdown elapses this is false even for
    /// an ad that will become skippable, and `skipAd` is ignored until it flips.
    skip_enabled: bool,
    /// A new ad started (rather than a state change within one).
    started: bool,
}

/// Read an ad event. `None` for anything that is not one.
fn ad_update(command: &LoungeCommand) -> Option<AdUpdate> {
    if !matches!(command.name.as_str(), "adPlaying" | "onAdStateChange") {
        return None;
    }
    let payload = command.payload.as_object()?;
    // These arrive as the strings "true"/"false", not JSON booleans.
    let flag = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|v| v == "true")
    };
    Some(AdUpdate {
        skip_enabled: flag("isSkipEnabled"),
        started: command.name == "adPlaying",
    })
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
        Self::new_at(reported, playing, Instant::now())
    }

    /// [`PlaybackClock::new`] with the instant supplied, so tests anchor the reckoning at
    /// a chosen moment and assert positions exactly (#236).
    fn new_at(reported: Duration, playing: bool, at: Instant) -> Self {
        Self {
            reported,
            at,
            playing,
        }
    }

    fn position(&self) -> Duration {
        self.position_at(Instant::now())
    }

    /// [`PlaybackClock::position`] as of `now`.
    fn position_at(&self, now: Instant) -> Duration {
        if self.playing {
            self.reported + now.saturating_duration_since(self.at)
        } else {
            self.reported
        }
    }

    /// Stop advancing — the content is not what is on screen right now.
    fn pause(&mut self) {
        self.pause_at(Instant::now());
    }

    /// [`PlaybackClock::pause`] as of `now`.
    fn pause_at(&mut self, now: Instant) {
        self.reported = self.position_at(now);
        self.playing = false;
    }
}

/// How long the screen may sit with no video before the panel takes itself back.
///
/// "Ready to cast" with an empty queue is nobody watching anything. The clock is on the
/// *video-less* state, not on channel silence: browsing the leanback UI by remote
/// generates plenty of traffic and none of it resets this, so three minutes is the budget
/// for picking something to watch, not for idling. Only a `nowPlaying` that names a video
/// clears it. A *paused* video never counts as idle — the screen still has a video, and
/// someone means to come back.
const IDLE_EXIT: Duration = Duration::from_secs(180);

/// The panel's own answer to "is sound coming out of it right now".
///
/// The second witness [`IdleClock`] needs, and the reason it needs one: the Lounge is
/// otherwise the *only* thing that can clear the idle clock, and a Lounge that goes quiet
/// under playing media is indistinguishable from an empty screen. That is not
/// hypothetical — a three-minute music mix at −4.7 dBFS was returned to the home screen
/// mid-song, with the mixer reporting 100% of frames written and zero starvation
/// throughout (#362).
///
/// Audio, specifically, because it is the one signal that tells the two states apart. The
/// paint counters do not: the "Ready to cast" splash animates, so frames keep arriving for
/// a screen nobody is watching. The splash is *silent*, and a video is not.
pub trait PanelAudio: Send + Sync {
    /// Frames handed to the mixer over the life of the process. Monotonic.
    fn frames_written(&self) -> u64;
}

impl PanelAudio for pipeline::mixer::AudioMixer {
    fn frames_written(&self) -> u64 {
        self.counters().written
    }
}

/// The idle watchdog, as the two halves that only mean anything together: what it would
/// do, and what it checks before doing it.
///
/// One struct rather than two arguments because a half-armed watchdog is precisely the
/// defect (#362) — an exit callback with no second witness acts on the Lounge's silence
/// alone, which is what hung up on a playing session. A build with no audio path can
/// therefore express "no watchdog", and cannot express "a watchdog that cannot check".
pub struct IdleWatch {
    /// What the panel's speakers have been given.
    pub audio: std::sync::Arc<dyn PanelAudio>,
    /// Hand the panel back to the home screen.
    pub on_idle: std::sync::Arc<dyn Fn() + Send + Sync>,
}

/// What the idle deadline expiring turned out to mean.
#[derive(Debug, PartialEq, Eq)]
enum IdleVerdict {
    /// Video-less and silent for the whole window: hand the panel back.
    ReturnHome,
    /// The panel was making sound the entire time the Lounge implied it was empty. The
    /// clock has been re-armed; `frames` is what the window measured.
    StillPlaying { frames: u64 },
}

/// The watchdog behind the idle exit: when the screen last became video-less, and what the
/// panel's audio had done by then.
///
/// Pure, and holding the audio sample rather than reading it, so the whole decision is
/// `(now, frames) -> verdict` and the shipped [`IDLE_EXIT`] is assertable in virtual time
/// instead of waited out (rule 6, #236).
#[derive(Debug, Default)]
struct IdleClock {
    armed: Option<Armed>,
}

#[derive(Debug, Clone, Copy)]
struct Armed {
    since: tokio::time::Instant,
    /// [`PanelAudio::frames_written`] as of [`Armed::since`], so the verdict compares a
    /// window rather than a level.
    frames: u64,
}

impl IdleClock {
    /// Start the clock, if it is not already running.
    ///
    /// Deliberately not a reset: a screen that reports itself video-less over and over —
    /// and a channel that re-attaches mid-idle — must not each grant another three
    /// minutes, or the deadline is never reached.
    fn arm(&mut self, now: tokio::time::Instant, frames: u64) {
        self.armed.get_or_insert(Armed { since: now, frames });
    }

    /// Stop the clock: there is a video on screen.
    fn clear(&mut self) {
        self.armed = None;
    }

    /// When the panel should be handed back, if the clock is running.
    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.armed.map(|armed| armed.since + IDLE_EXIT)
    }

    /// The deadline has arrived. Ask the panel whether it agrees that nothing is playing.
    ///
    /// A window in which the mixer was handed audio is a window somebody was listening to
    /// something, whatever the Lounge did or did not say. That re-arms rather than clears:
    /// re-arming is bounded and self-correcting — the watchdog asks again one window later
    /// and fires the first time the panel really does fall silent — where clearing would
    /// hand the decision back to the same witness that just failed to make it.
    fn expired(&mut self, now: tokio::time::Instant, frames: u64) -> IdleVerdict {
        let played = self
            .armed
            .map_or(0, |armed| frames.saturating_sub(armed.frames));
        if played > 0 {
            self.armed = Some(Armed { since: now, frames });
            return IdleVerdict::StillPlaying { frames: played };
        }
        self.armed = None;
        IdleVerdict::ReturnHome
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
    fn an_ad_is_only_skippable_once_its_countdown_says_so() {
        // Verbatim from a captured session: an unskippable 15-second pre-roll. Pressing
        // skip here does nothing, and the flags are *strings*, not JSON booleans — read
        // them as booleans and every ad looks skippable.
        let starting = ad_update(&command(
            "adPlaying",
            json!({"duration": "15.241", "adState": "-1", "isBumper": "false",
                   "contentVideoId": "JGwWNGJdvx8", "isSkipEnabled": "false",
                   "adVideoId": "ZKcMbf7cR4I", "isSkippable": "false"}),
        ))
        .unwrap();
        assert!(!starting.skip_enabled);
        assert!(
            starting.started,
            "adPlaying is a new ad, not a state change"
        );

        let waiting = ad_update(&command(
            "onAdStateChange",
            json!({"currentTime": "3", "adState": "1", "isSkipEnabled": "false"}),
        ))
        .unwrap();
        assert!(!waiting.skip_enabled);
        assert!(!waiting.started);

        // The countdown elapses and the button lights up.
        let ready = ad_update(&command(
            "onAdStateChange",
            json!({"currentTime": "5", "adState": "1", "isSkipEnabled": "true"}),
        ))
        .unwrap();
        assert!(ready.skip_enabled);
    }

    #[test]
    fn an_ad_event_is_not_playback_news() {
        // Positions inside an ad belong to the ad. Feeding one to the content clock puts
        // it seconds ahead of the video for the rest of the session.
        assert!(playback_update(&command(
            "onAdStateChange",
            json!({"currentTime": "7", "adState": "1", "isSkipEnabled": "true"})
        ))
        .is_none());
        assert!(ad_update(&command("nowPlaying", json!({"videoId": "dQw4w9WgXcQ"}))).is_none());
    }

    #[test]
    fn pausing_the_clock_keeps_the_position_it_had_reached() {
        // Exact, in chosen instants — this used to sleep 60 ms of wall time and assert
        // inequalities (#236).
        let t0 = Instant::now();
        let mut clock = PlaybackClock::new_at(Duration::from_secs(10), true, t0);
        // Three seconds in, an ad break pauses the reckoning at exactly thirteen.
        clock.pause_at(t0 + Duration::from_secs(3));
        let held = Duration::from_secs(13);
        assert_eq!(clock.position_at(t0 + Duration::from_secs(3)), held);
        assert_eq!(
            clock.position_at(t0 + Duration::from_secs(120)),
            held,
            "an ad break must not advance the content"
        );
    }

    #[test]
    fn a_paused_clock_does_not_advance() {
        let t0 = Instant::now();
        let clock = PlaybackClock::new_at(Duration::from_secs(10), false, t0);
        assert_eq!(
            clock.position_at(t0 + Duration::from_secs(20)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn a_playing_clock_runs_between_reports() {
        // The whole point of dead reckoning: positions arrive on change, not on a tick.
        let t0 = Instant::now();
        let clock = PlaybackClock::new_at(Duration::from_secs(10), true, t0);
        assert_eq!(
            clock.position_at(t0 + Duration::from_millis(1_500)),
            Duration::from_millis(11_500)
        );
    }

    /// Frames of stereo audio at 48 kHz, so the counts below read as durations.
    fn seconds_of_audio(seconds: u64) -> u64 {
        seconds * 48_000
    }

    #[test]
    fn a_silent_window_returns_the_panel() {
        let t0 = tokio::time::Instant::now();
        let mut clock = IdleClock::default();
        clock.arm(t0, 0);
        assert_eq!(clock.deadline(), Some(t0 + IDLE_EXIT));
        // The shipped constant, asserted rather than waited out (#236).
        assert_eq!(clock.expired(t0 + IDLE_EXIT, 0), IdleVerdict::ReturnHome);
        assert_eq!(clock.deadline(), None, "a fired clock stops");
    }

    #[test]
    fn a_window_the_panel_played_through_does_not_return_it() {
        // #362 exactly: the Lounge said nothing for the whole window — so nothing cleared
        // the clock — while the mixer was handed three minutes of audio. The old watchdog
        // had only the Lounge to ask and hung up on a playing session.
        let t0 = tokio::time::Instant::now();
        let mut clock = IdleClock::default();
        clock.arm(t0, seconds_of_audio(10));
        let played = seconds_of_audio(190);
        assert_eq!(
            clock.expired(t0 + IDLE_EXIT, played),
            IdleVerdict::StillPlaying {
                frames: seconds_of_audio(180)
            }
        );
        assert_eq!(
            clock.deadline(),
            Some(t0 + IDLE_EXIT + IDLE_EXIT),
            "a vetoed exit re-arms from the veto, so the watchdog asks again"
        );
    }

    #[test]
    fn the_window_after_the_music_stops_still_returns_the_panel() {
        // The veto is bounded: it buys one more window, not immunity. Without this the
        // fix for #362 would be a panel that never goes home once anything has played.
        let t0 = tokio::time::Instant::now();
        let mut clock = IdleClock::default();
        clock.arm(t0, 0);
        let played = seconds_of_audio(180);
        assert!(matches!(
            clock.expired(t0 + IDLE_EXIT, played),
            IdleVerdict::StillPlaying { .. }
        ));
        // The music ends the moment the veto re-arms; the next window is silent.
        assert_eq!(
            clock.expired(t0 + IDLE_EXIT + IDLE_EXIT, played),
            IdleVerdict::ReturnHome
        );
    }

    #[test]
    fn re_arming_mid_idle_does_not_grant_another_window() {
        // The screen reports itself video-less repeatedly, and the channel re-attaches
        // every couple of minutes as a matter of course. Either one resetting the clock
        // means the deadline is never reached.
        let t0 = tokio::time::Instant::now();
        let mut clock = IdleClock::default();
        clock.arm(t0, 0);
        clock.arm(t0 + Duration::from_secs(60), 0);
        clock.arm(t0 + Duration::from_secs(120), 0);
        assert_eq!(clock.deadline(), Some(t0 + IDLE_EXIT));
    }

    #[test]
    fn a_video_on_screen_stops_the_clock() {
        let t0 = tokio::time::Instant::now();
        let mut clock = IdleClock::default();
        clock.arm(t0, 0);
        clock.clear();
        assert_eq!(clock.deadline(), None);
        // And the next video-less report starts a fresh window rather than resuming the
        // old one.
        clock.arm(t0 + Duration::from_secs(60), 0);
        assert_eq!(
            clock.deadline(),
            Some(t0 + Duration::from_secs(60) + IDLE_EXIT)
        );
    }
}
