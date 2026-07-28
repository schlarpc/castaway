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

    /// Stop advancing — the content is not what is on screen right now.
    fn pause(&mut self) {
        self.reported = self.position();
        self.playing = false;
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
        let mut clock = PlaybackClock::new(Duration::from_secs(10), true);
        std::thread::sleep(Duration::from_millis(30));
        clock.pause();
        let held = clock.position();
        assert!(held > Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            clock.position(),
            held,
            "an ad break must not advance the content"
        );
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
