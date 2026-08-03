//! AirPlay **video** — the media-URL path, which is a different protocol from mirroring.
//!
//! Tapping AirPlay inside an app that has its own video (YouTube's AirPlay button rather
//! than Screen Mirroring) starts a session this receiver advertised and never served. The
//! feature bits that invite it are UxPlay's whole mask, adopted as a unit, so the
//! invitation cannot be withdrawn without diverging from a mask that is otherwise proven —
//! which made "advertised and not implemented" the status quo (#80).
//!
//! ## Why this is much smaller than it looks
//!
//! UxPlay's `airplay_video.c` is ~1000 lines because it implements HLS: fetching
//! playlists, selecting a language, feeding segments. None of that is needed here.
//! `POST /play` hands over **a URL**, and "play the media at this URL" is already a
//! first-class operation — it is what DLNA's `SetAVTransportURI` and Cast's `LOAD` both
//! reduce to, through [`castaway_core::MediaUri`] and the same pipeline. What was missing
//! is the *control surface*: six small HTTP endpoints and the state behind them.
//!
//! ## How a media session is told from a mirroring one
//!
//! Captured 2026-07-31 from iOS 26.5.2 (`docs/`-recorded in #80): the RTSP half is the one
//! already served, and the first `SETUP` carries `ekey`/`eiv`/`timingProtocol` exactly as
//! mirroring does — but **has no `isScreenMirroringSession`**. That absence is the
//! discriminator, and it is the only one: the audio stream it negotiates is ordinary ALAC
//! RAOP, and no type-110 mirroring stream appears anywhere in the session.
//!
//! The video never arrives over RTSP at all. It comes as HTTP/1.1 on the same port, which
//! is why this module is dispatched from the same request table.
//!
//! ## Bodies, of which there are two shapes
//!
//! `POST /play` arrives either as a binary plist or as `text/parameters` — a
//! `Key: value` block, the same shape `SET_PARAMETER` uses. Both are accepted, because
//! which one a sender picks depends on its vintage rather than on anything negotiable.

use std::time::Duration;

use castaway_core::MediaUri;
use tracing::{debug, warn};

use crate::error::AirPlayError;

/// What the sender asked the receiver to do.
///
/// Emitted by [`parse`] and drained by the actor, which turns it into the
/// [`castaway_core::SessionEvent`] or [`castaway_core::ControlTxn`] it already knows how
/// to route. Nothing here talks to a pipeline.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum VideoCommand {
    /// Play the media at this URL.
    Play {
        /// Where the media is.
        source: MediaUri,
        /// Where to start, if the sender said.
        start: Option<StartPosition>,
    },
    /// Seek to an absolute position.
    Scrub(Duration),
    /// Pause (`0.0`) or resume (`1.0`).
    ///
    /// A *rate*, not a toggle, and the distinction is the protocol's: a sender that has
    /// paused sends `rate=0` again when it means "still paused", and treating that as a
    /// toggle plays a video nobody asked to resume.
    Rate(Rate),
    /// End the session.
    Stop,
}

/// The playback rate a sender asked for.
///
/// Only the two values Apple's senders use. A rate in between is a scrubbing preview that
/// this receiver does not serve, and is rounded rather than refused — a video that plays
/// is better than a session torn down over a speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    /// `rate=0`.
    Paused,
    /// `rate=1`.
    Playing,
}

/// Where a sender asked playback to begin.
///
/// Two units, and they are not interchangeable: AirPlay 1's `Start-Position` is a
/// *fraction* of the item, which cannot be resolved to a time until the duration is known,
/// while `Start-Position-Seconds` is absolute. Kept apart so the difference is the
/// caller's to resolve rather than a float whose meaning depends on where it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum StartPosition {
    /// A fraction of the item's duration, `0.0..=1.0`.
    Fraction(f64),
    /// An absolute offset.
    Seconds(Duration),
}

impl StartPosition {
    /// The absolute offset, given a duration to resolve a fraction against.
    ///
    /// `None` for a fraction with no duration to resolve it against, which is the honest
    /// answer: starting at "40% of an unknown length" is not something that can be done.
    #[must_use]
    pub fn resolve(self, duration: Option<Duration>) -> Option<Duration> {
        match self {
            Self::Seconds(at) => Some(at),
            Self::Fraction(f) if f <= 0.0 => Some(Duration::ZERO),
            Self::Fraction(f) => duration.map(|total| total.mul_f64(f.clamp(0.0, 1.0))),
        }
    }
}

/// What the receiver is doing, as `/playback-info` reports it.
///
/// Held by the session and refreshed by the actor from the pipeline, because a sender
/// polls this endpoint for the position it draws in its own scrubber — the receiver is
/// authoritative and the sender is a mirror of it, which is the reverse of every other
/// AirPlay surface.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PlaybackInfo {
    /// Item length, when known.
    pub duration: Option<Duration>,
    /// Where playback has reached.
    pub position: Duration,
    /// Whether it is advancing.
    pub rate: Option<Rate>,
    /// Whether the receiver has enough to start.
    pub ready: bool,
}

impl PlaybackInfo {
    /// The plist a sender polls for.
    ///
    /// The keys are the ones iOS reads; anything it does not find, it assumes. `duration`
    /// and `position` are seconds as reals, which is the one place in AirPlay where a time
    /// is not an NTP-style fixed point.
    ///
    /// **An empty `200` is not an acceptable substitute**, and this is the same lesson the
    /// rest of this crate learned the hard way: a sender that asks for a value and gets a
    /// body it cannot parse concludes the item failed rather than that the receiver is
    /// terse.
    ///
    /// # Errors
    /// [`AirPlayError::Plist`] if the plist will not serialize, which it cannot.
    pub fn to_plist(self) -> Result<Vec<u8>, AirPlayError> {
        let mut dict = plist::Dictionary::new();
        let seconds = |d: Duration| plist::Value::Real(d.as_secs_f64());
        // A duration of zero is how "not known yet" is spelled here; there is no absent
        // form, and omitting the key makes some senders wait for one forever.
        dict.insert(
            "duration".into(),
            seconds(self.duration.unwrap_or(Duration::ZERO)),
        );
        dict.insert("position".into(), seconds(self.position));
        dict.insert(
            "rate".into(),
            plist::Value::Real(match self.rate {
                Some(Rate::Playing) => 1.0,
                _ => 0.0,
            }),
        );
        dict.insert("readyToPlay".into(), plist::Value::Boolean(self.ready));
        // Both false rather than absent: a sender that finds neither key assumes the
        // worst of them, and "the buffer is empty" is a spinner over a playing video.
        dict.insert(
            "playbackBufferEmpty".into(),
            plist::Value::Boolean(!self.ready),
        );
        dict.insert("playbackBufferFull".into(), plist::Value::Boolean(false));
        dict.insert("playbackLikelyToKeepUp".into(), plist::Value::Boolean(true));
        // The scrubbable range, which is what a sender draws its track from. One entry
        // covering the whole item; a live stream with no duration gets a zero-length one,
        // which is how "not scrubbable" is spelled.
        let mut range = plist::Dictionary::new();
        range.insert("start".into(), plist::Value::Real(0.0));
        range.insert(
            "duration".into(),
            seconds(self.duration.unwrap_or(Duration::ZERO)),
        );
        dict.insert(
            "loadedTimeRanges".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(range.clone())]),
        );
        dict.insert(
            "seekableTimeRanges".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(range)]),
        );
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &dict).map_err(|e| AirPlayError::Plist(e.to_string()))?;
        Ok(buf)
    }

    /// The `text/parameters` form `GET /scrub` answers with.
    ///
    /// A different shape for the same two numbers, because `/scrub` predates the plist
    /// endpoints and senders still ask it that way.
    #[must_use]
    pub fn to_scrub_body(self) -> String {
        format!(
            "duration: {}\r\nposition: {}\r\n",
            self.duration.unwrap_or(Duration::ZERO).as_secs_f64(),
            self.position.as_secs_f64()
        )
    }
}

/// The endpoints this module serves.
///
/// A closed set rather than a string match at the call site, so adding one is a change the
/// compiler asks about (ground rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VideoEndpoint {
    /// `POST /play` — the media URL.
    Play,
    /// `POST /scrub` sets, `GET /scrub` reports.
    Scrub,
    /// `POST /rate` — pause or resume.
    Rate,
    /// `POST /stop`.
    Stop,
    /// `GET /playback-info`.
    PlaybackInfo,
    /// `POST /action` — a grab-bag the sender uses for things a receiver may ignore.
    Action,
    /// `POST /reverse` — the sender opening an event channel back to itself.
    Reverse,
}

impl VideoEndpoint {
    /// Which endpoint a method and path name, if any.
    ///
    /// The query string is stripped first: `/scrub?position=12.5` is `/scrub`, and the
    /// parameters are the body's job to find.
    #[must_use]
    pub fn route(method: &str, path: &str) -> Option<Self> {
        let bare = path.split('?').next().unwrap_or(path);
        match (method, bare) {
            ("POST", "/play") => Some(Self::Play),
            ("POST" | "GET", "/scrub") => Some(Self::Scrub),
            ("POST", "/rate") => Some(Self::Rate),
            ("POST", "/stop") => Some(Self::Stop),
            ("GET", "/playback-info") => Some(Self::PlaybackInfo),
            ("POST", "/action") => Some(Self::Action),
            ("POST", "/reverse") => Some(Self::Reverse),
            _ => None,
        }
    }
}

/// Parse a request into the command it carries, if it carries one.
///
/// `Ok(None)` is a request that is understood and asks for nothing — a `GET /scrub`, a
/// `/playback-info`, an `/action` this receiver has no use for. That is different from an
/// error, and conflating them is how a receiver comes to refuse things it merely does not
/// need.
///
/// # Errors
/// [`AirPlayError`] for a body that names a media URL that will not parse, which is the
/// one failure a sender can act on.
pub fn parse(
    endpoint: VideoEndpoint,
    path: &str,
    body: &[u8],
) -> Result<Option<VideoCommand>, AirPlayError> {
    match endpoint {
        VideoEndpoint::Play => parse_play(body).map(Some),
        VideoEndpoint::Scrub => {
            // The position rides in the query string, not the body. `GET /scrub` has
            // neither and is a read.
            Ok(query(path, "position")
                .and_then(|raw| raw.parse::<f64>().ok())
                .map(|seconds| VideoCommand::Scrub(Duration::from_secs_f64(seconds.max(0.0)))))
        }
        VideoEndpoint::Rate => {
            let Some(value) = query(path, "value").and_then(|raw| raw.parse::<f64>().ok()) else {
                warn!(%path, "AirPlay /rate with no value");
                return Ok(None);
            };
            // Rounded rather than refused: a rate between the two is a scrubbing preview
            // this receiver does not serve, and a session torn down over a speed is worse
            // than a video that plays.
            Ok(Some(VideoCommand::Rate(if value >= 0.5 {
                Rate::Playing
            } else {
                Rate::Paused
            })))
        }
        VideoEndpoint::Stop => Ok(Some(VideoCommand::Stop)),
        // Understood, and asks for nothing this receiver does. `/action` carries playlist
        // manipulation and `unhandledURLResponse`, neither of which applies to a receiver
        // that plays one URL at a time.
        VideoEndpoint::Action => {
            debug!(
                body = body.len(),
                "AirPlay /action, which this receiver ignores"
            );
            Ok(None)
        }
        VideoEndpoint::PlaybackInfo | VideoEndpoint::Reverse => Ok(None),
    }
}

/// `POST /play`, in either of its two body shapes.
fn parse_play(body: &[u8]) -> Result<VideoCommand, AirPlayError> {
    let (location, fraction, seconds) = if body.starts_with(b"bplist00") {
        let value = plist::Value::from_reader(std::io::Cursor::new(body))
            .map_err(|e| AirPlayError::Plist(e.to_string()))?;
        let dict = value
            .as_dictionary()
            .ok_or_else(|| AirPlayError::Plist("a /play plist that is not a dictionary".into()))?;
        (
            dict.get("Content-Location")
                .and_then(plist::Value::as_string)
                .map(str::to_owned),
            dict.get("Start-Position").and_then(as_real),
            dict.get("Start-Position-Seconds").and_then(as_real),
        )
    } else {
        // `text/parameters`: `Key: value` a line at a time, the same shape
        // `SET_PARAMETER` uses. Header names are compared case-insensitively because
        // senders disagree about `Start-Position` versus `start-position`.
        let text = std::str::from_utf8(body).map_err(|_| {
            AirPlayError::Plist("a /play body that is neither a plist nor text".into())
        })?;
        let mut location = None;
        let mut fraction = None;
        let mut seconds = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "content-location" => location = Some(value.to_owned()),
                "start-position" => fraction = value.parse().ok(),
                "start-position-seconds" => seconds = value.parse().ok(),
                _ => {}
            }
        }
        (location, fraction, seconds)
    };

    let Some(location) = location else {
        return Err(AirPlayError::Plist(
            "a /play body with no Content-Location".into(),
        ));
    };
    let source = MediaUri::parse(&location)
        .map_err(|e| AirPlayError::Plist(format!("a /play URL that will not parse: {e}")))?;
    // Seconds win when both are present: an absolute offset needs no duration to resolve
    // it, and a sender that sends both means the same instant twice.
    let start = seconds
        .map(|s| StartPosition::Seconds(Duration::from_secs_f64(s.max(0.0))))
        .or_else(|| fraction.map(StartPosition::Fraction))
        // Zero is not "start at the beginning" for a fraction — it is, but it is also
        // what a sender sends when it means "wherever". Dropped so the pipeline's own
        // default applies rather than an explicit seek to 0.
        .filter(|start| *start != StartPosition::Fraction(0.0));
    Ok(VideoCommand::Play { source, start })
}

/// Both integer and real, because senders send `Start-Position` as either.
fn as_real(value: &plist::Value) -> Option<f64> {
    value.as_real().or_else(|| {
        value.as_signed_integer().map(|i| {
            #[allow(clippy::cast_precision_loss)]
            {
                i as f64
            }
        })
    })
}

/// One query parameter's raw value.
fn query<'a>(path: &'a str, key: &str) -> Option<&'a str> {
    path.split_once('?')?
        .1
        .split('&')
        .find_map(|pair| pair.split_once('=').filter(|(k, _)| *k == key))
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn play_plist(location: &str, extra: &[(&str, plist::Value)]) -> Vec<u8> {
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "Content-Location".into(),
            plist::Value::String(location.into()),
        );
        for (k, v) in extra {
            dict.insert((*k).into(), v.clone());
        }
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &dict).unwrap();
        buf
    }

    #[test]
    fn a_play_plist_yields_the_url_the_sender_named() {
        // The whole feature in one assertion: `/play` hands over a URL, and a URL is
        // already a first-class thing this receiver plays.
        let body = play_plist("http://10.0.0.5:8080/master.m3u8", &[]);
        let command = parse(VideoEndpoint::Play, "/play", &body).unwrap().unwrap();
        assert_eq!(
            command,
            VideoCommand::Play {
                source: MediaUri::parse("http://10.0.0.5:8080/master.m3u8").unwrap(),
                start: None,
            }
        );
    }

    #[test]
    fn the_text_parameters_form_works_too() {
        // Which shape a sender picks depends on its vintage, not on anything negotiable,
        // so both have to work or a whole generation of senders silently does not.
        let body = b"Content-Location: http://10.0.0.5/v.mp4\r\nStart-Position: 0.5\r\n";
        let command = parse(VideoEndpoint::Play, "/play", body).unwrap().unwrap();
        assert_eq!(
            command,
            VideoCommand::Play {
                source: MediaUri::parse("http://10.0.0.5/v.mp4").unwrap(),
                start: Some(StartPosition::Fraction(0.5)),
            }
        );
    }

    #[test]
    fn a_fraction_and_an_absolute_offset_are_not_the_same_number() {
        // `Start-Position` is a fraction of the item and `Start-Position-Seconds` is a
        // time. Treating one as the other starts a two-hour film half a second in, or
        // half way through.
        let half = StartPosition::Fraction(0.5);
        assert_eq!(
            half.resolve(Some(Duration::from_secs(200))),
            Some(Duration::from_secs(100))
        );
        assert_eq!(
            half.resolve(None),
            None,
            "40% of an unknown length is not a position"
        );
        let absolute = StartPosition::Seconds(Duration::from_secs(100));
        assert_eq!(absolute.resolve(None), Some(Duration::from_secs(100)));
    }

    #[test]
    fn seconds_win_over_a_fraction_when_a_sender_sends_both() {
        let body = play_plist(
            "http://10.0.0.5/v.mp4",
            &[
                ("Start-Position", plist::Value::Real(0.5)),
                ("Start-Position-Seconds", plist::Value::Real(42.0)),
            ],
        );
        let VideoCommand::Play { start, .. } =
            parse(VideoEndpoint::Play, "/play", &body).unwrap().unwrap()
        else {
            panic!("a /play is a play");
        };
        assert_eq!(
            start,
            Some(StartPosition::Seconds(Duration::from_secs(42))),
            "an absolute offset needs no duration to resolve it"
        );
    }

    #[test]
    fn a_start_position_of_zero_is_not_a_seek() {
        // Senders send `0.0` to mean "wherever", and an explicit seek to zero on a live
        // stream is a seek that can fail.
        let body = play_plist(
            "http://10.0.0.5/v.mp4",
            &[("Start-Position", plist::Value::Real(0.0))],
        );
        let VideoCommand::Play { start, .. } =
            parse(VideoEndpoint::Play, "/play", &body).unwrap().unwrap()
        else {
            panic!("a /play is a play");
        };
        assert_eq!(start, None);
    }

    #[test]
    fn a_play_with_no_url_is_an_error_rather_than_a_lenient_ok() {
        // The lesson the rest of this crate learned the hard way: answering "fine" to a
        // request we cannot serve produces a session that negotiates cleanly and then
        // shows nothing, which is the exact failure #80 is about.
        let mut dict = plist::Dictionary::new();
        dict.insert("Start-Position".into(), plist::Value::Real(0.5));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &dict).unwrap();
        assert!(parse(VideoEndpoint::Play, "/play", &body).is_err());
    }

    #[test]
    fn scrub_reads_its_position_from_the_query_string() {
        assert_eq!(
            parse(VideoEndpoint::Scrub, "/scrub?position=12.5", b"")
                .unwrap()
                .unwrap(),
            VideoCommand::Scrub(Duration::from_millis(12_500))
        );
        // …and a GET is a read, not a seek to nowhere.
        assert_eq!(parse(VideoEndpoint::Scrub, "/scrub", b"").unwrap(), None);
    }

    #[test]
    fn rate_is_a_rate_and_not_a_toggle() {
        // A sender that has paused sends `rate=0` again when it means "still paused".
        // Toggling on that plays a video nobody asked to resume.
        let rate = |q: &str| match parse(VideoEndpoint::Rate, q, b"").unwrap().unwrap() {
            VideoCommand::Rate(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(rate("/rate?value=0.000000"), Rate::Paused);
        assert_eq!(rate("/rate?value=0"), Rate::Paused);
        assert_eq!(rate("/rate?value=1.000000"), Rate::Playing);
        assert_eq!(rate("/rate?value=1"), Rate::Playing);
    }

    #[test]
    fn the_routing_table_strips_the_query_string() {
        assert_eq!(
            VideoEndpoint::route("POST", "/scrub?position=1"),
            Some(VideoEndpoint::Scrub)
        );
        assert_eq!(
            VideoEndpoint::route("GET", "/playback-info"),
            Some(VideoEndpoint::PlaybackInfo)
        );
        // Not ours: these belong to the mirroring and RAOP halves on the same socket.
        assert_eq!(VideoEndpoint::route("GET", "/info"), None);
        assert_eq!(VideoEndpoint::route("POST", "/fp-setup"), None);
        assert_eq!(VideoEndpoint::route("GET", "/play"), None);
    }

    #[test]
    fn playback_info_carries_the_numbers_a_sender_draws_its_scrubber_from() {
        let info = PlaybackInfo {
            duration: Some(Duration::from_secs(200)),
            position: Duration::from_secs(40),
            rate: Some(Rate::Playing),
            ready: true,
        };
        let bytes = info.to_plist().unwrap();
        let value: plist::Value = plist::from_bytes(&bytes).unwrap();
        let dict = value.as_dictionary().unwrap();
        assert_eq!(dict.get("duration").and_then(as_real), Some(200.0));
        assert_eq!(dict.get("position").and_then(as_real), Some(40.0));
        assert_eq!(dict.get("rate").and_then(as_real), Some(1.0));
        assert_eq!(
            dict.get("readyToPlay").and_then(plist::Value::as_boolean),
            Some(true)
        );
        // The buffer keys are present and false rather than absent: a sender that finds
        // neither assumes the worst of them, and "the buffer is empty" is a spinner over
        // a playing video.
        assert_eq!(
            dict.get("playbackBufferEmpty")
                .and_then(plist::Value::as_boolean),
            Some(false)
        );
        assert!(dict.contains_key("seekableTimeRanges"));
    }

    #[test]
    fn a_paused_receiver_reports_rate_zero() {
        let info = PlaybackInfo {
            duration: Some(Duration::from_secs(10)),
            position: Duration::from_secs(1),
            rate: Some(Rate::Paused),
            ready: true,
        };
        let value: plist::Value = plist::from_bytes(&info.to_plist().unwrap()).unwrap();
        assert_eq!(
            value.as_dictionary().unwrap().get("rate").and_then(as_real),
            Some(0.0)
        );
    }

    #[test]
    fn the_scrub_body_is_the_two_numbers_in_the_older_shape() {
        let info = PlaybackInfo {
            duration: Some(Duration::from_secs(200)),
            position: Duration::from_millis(40_500),
            rate: Some(Rate::Playing),
            ready: true,
        };
        assert_eq!(info.to_scrub_body(), "duration: 200\r\nposition: 40.5\r\n");
    }

    #[test]
    fn random_bodies_do_not_panic() {
        // These endpoints face the LAN on the same socket everything else does.
        let mut seed = 0xdead_beef_1234_5678u64;
        for endpoint in [
            VideoEndpoint::Play,
            VideoEndpoint::Scrub,
            VideoEndpoint::Rate,
            VideoEndpoint::Action,
        ] {
            for _ in 0..2_000 {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let len = (seed >> 33) as usize % 48;
                let body: Vec<u8> = (0..len)
                    .map(|i| ((seed >> (i % 8 * 8)) & 0xff) as u8)
                    .collect();
                let _ = parse(endpoint, "/x?value=&position=", &body);
            }
        }
    }
}
