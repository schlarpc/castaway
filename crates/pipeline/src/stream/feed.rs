//! The encoded output, fanned out to real-time subscribers (#18).
//!
//! HLS and WebRTC want the same pictures and disagree about everything else. HLS wants
//! them boxed into fMP4 and cut into segments a player fetches; WebRTC wants each access
//! unit the moment it exists, in Annex-B, with the parameter sets in band. So the encode
//! loop publishes to both from one encode — the expensive part happens once and the two
//! shapes diverge after it.
//!
//! ## Why Annex-B, and why the parameter sets come along
//!
//! [`Sample`](super::fmp4::Sample) carries AVCC — four-byte length prefixes — because that
//! is what an `mdat` holds, and the SPS/PPS live apart in the init segment where they are
//! sent once. RTP has no init segment. A viewer joins mid-stream and the very first thing
//! it must be able to do is decode, so every keyframe carries its own parameter sets and
//! every NAL is start-code delimited, which is what the H.264 payloader expects.
//!
//! ## Keeping up
//!
//! A subscriber that falls behind loses frames rather than holding them: this is a live
//! duplicate, and a viewer several seconds behind on a control surface is worse than one
//! that skipped. `broadcast` does exactly that — a lagging receiver is told how many it
//! missed and resumes at the newest.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::broadcast;

use super::fmp4::{AvcConfig, Sample, TIMESCALE};

/// How many frames a subscriber may fall behind before it starts losing them.
///
/// Two seconds at 30 fps. Long enough to absorb a scheduling hiccup, short enough that a
/// stalled task cannot hold a second of pictures alive per peer.
const BACKLOG: usize = 64;

/// The Annex-B start code, which is the whole of the AVCC→Annex-B difference.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// One coded picture, ready for an RTP payloader.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// The access unit in Annex-B form, parameter sets included when it is a keyframe.
    pub data: std::sync::Arc<[u8]>,
    /// How long it is shown.
    pub duration: Duration,
    /// Whether a decoder may start here.
    pub keyframe: bool,
}

/// The live fan-out: encoded pictures out, keyframe requests back.
#[derive(Debug)]
pub struct LiveFeed {
    frames: broadcast::Sender<EncodedFrame>,
    /// Set by a subscriber that needs a decodable starting point — a peer that just
    /// joined, or one whose decoder lost sync. Cleared by the encode loop when it acts on
    /// it, so a burst of requests during one GOP costs one keyframe rather than several.
    keyframe_wanted: AtomicBool,
    /// How many subscribers are attached. Counted here rather than derived from
    /// `broadcast::Sender::receiver_count` because a peer holds its receiver across
    /// awaits and the count has to answer "is anyone watching" for tap liveness, which
    /// is asked from a different thread entirely.
    subscribers: AtomicUsize,
}

impl Default for LiveFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveFeed {
    /// An empty feed with nobody attached.
    #[must_use]
    pub fn new() -> Self {
        let (frames, _) = broadcast::channel(BACKLOG);
        Self {
            frames,
            keyframe_wanted: AtomicBool::new(false),
            subscribers: AtomicUsize::new(0),
        }
    }

    /// Attach. The guard decrements the subscriber count when it goes away, including on
    /// a task that was cancelled rather than finished — which is the common case, since a
    /// peer task ends when its connection does.
    #[must_use]
    pub fn subscribe(self: &std::sync::Arc<Self>) -> Subscription {
        self.subscribers.fetch_add(1, Ordering::AcqRel);
        // A joining viewer cannot decode until the next keyframe, so ask for one rather
        // than making it wait out the GOP.
        self.request_keyframe();
        Subscription {
            frames: self.frames.subscribe(),
            feed: std::sync::Arc::clone(self),
        }
    }

    /// Whether anyone is attached.
    #[must_use]
    pub fn watched(&self) -> bool {
        self.subscribers.load(Ordering::Acquire) > 0
    }

    /// Ask the encoder for a keyframe at the next opportunity.
    pub fn request_keyframe(&self) {
        self.keyframe_wanted.store(true, Ordering::Release);
    }

    /// Whether a keyframe was asked for, clearing the request.
    #[must_use]
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_wanted.swap(false, Ordering::AcqRel)
    }

    /// Publish one encoded picture.
    ///
    /// `config` is the track's current parameter sets, prepended when this is a keyframe.
    /// Cheap when nobody is attached: the conversion is skipped entirely rather than done
    /// and thrown away, which matters because this is on the encode thread.
    pub fn publish(&self, sample: &Sample, config: &AvcConfig) {
        if self.frames.receiver_count() == 0 {
            return;
        }
        let data = annexb(sample, config);
        // The only error is "nobody is listening", which the check above already covers
        // and which is not a problem in any case.
        let _ = self.frames.send(EncodedFrame {
            data: data.into(),
            duration: Duration::from_secs_f64(f64::from(sample.duration) / f64::from(TIMESCALE)),
            keyframe: sample.keyframe,
        });
    }
}

/// An attached subscriber. Dropping it detaches.
#[derive(Debug)]
pub struct Subscription {
    frames: broadcast::Receiver<EncodedFrame>,
    feed: std::sync::Arc<LiveFeed>,
}

impl Subscription {
    /// The next picture, or `None` once the feed is gone.
    ///
    /// A subscriber that fell behind is silently resynchronised to the newest frame. That
    /// is the right loss for a live duplicate — the alternative is delivering pictures
    /// whose moment has passed, which on a control surface is worse than a gap.
    pub async fn next(&mut self) -> Option<EncodedFrame> {
        loop {
            match self.frames.recv().await {
                Ok(frame) => return Some(frame),
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "remote viewer fell behind; skipping to live");
                    // Whatever it resumes on is mid-GOP, so it needs a fresh starting
                    // point or it decodes garbage until the next scheduled keyframe.
                    self.feed.request_keyframe();
                }
            }
        }
    }

    /// Ask for a keyframe on this subscriber's behalf — what a PLI from the far side means.
    pub fn request_keyframe(&self) {
        self.feed.request_keyframe();
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.feed.subscribers.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Convert one AVCC access unit to Annex-B, prepending the parameter sets on a keyframe.
///
/// A length prefix that runs past the end of the buffer stops the walk rather than
/// panicking: the samples come from our own encoder, but this is the kind of loop where
/// "cannot happen" and "does not panic" are worth being the same statement.
fn annexb(sample: &Sample, config: &AvcConfig) -> Vec<u8> {
    let mut out = Vec::with_capacity(sample.data.len() + 64);
    if sample.keyframe {
        for parameter_set in config.sps.iter().chain(config.pps.iter()) {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(parameter_set);
        }
    }
    let mut rest = sample.data.as_slice();
    while rest.len() >= 4 {
        let (header, body) = rest.split_at(4);
        let Ok(length) = <[u8; 4]>::try_from(header) else {
            break;
        };
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > body.len() {
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&body[..length]);
        rest = &body[length..];
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;

    fn avcc(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(&u32::try_from(unit.len()).unwrap().to_be_bytes());
            out.extend_from_slice(unit);
        }
        out
    }

    fn config() -> AvcConfig {
        AvcConfig {
            sps: vec![vec![0x67, 0x42]],
            pps: vec![vec![0x68, 0xCE]],
        }
    }

    #[test]
    fn a_keyframe_carries_its_own_parameter_sets() {
        // There is no init segment in RTP. A viewer that joined ten seconds in has never
        // seen an SPS, so a keyframe without them decodes to nothing.
        let sample = Sample {
            data: avcc(&[&[0x65, 0x88, 0x84]]),
            duration: 3000,
            keyframe: true,
        };
        let out = annexb(&sample, &config());
        assert_eq!(
            out,
            [
                &START_CODE[..],
                &[0x67, 0x42],
                &START_CODE[..],
                &[0x68, 0xCE],
                &START_CODE[..],
                &[0x65, 0x88, 0x84],
            ]
            .concat()
        );
    }

    #[test]
    fn a_delta_frame_does_not() {
        // Repeating them on every frame would be bytes on the wire for nothing.
        let sample = Sample {
            data: avcc(&[&[0x41, 0x9A]]),
            duration: 3000,
            keyframe: false,
        };
        assert_eq!(
            annexb(&sample, &config()),
            [&START_CODE[..], &[0x41, 0x9A]].concat()
        );
    }

    #[test]
    fn every_unit_in_an_access_unit_is_delimited() {
        let sample = Sample {
            data: avcc(&[&[0x06, 0x00], &[0x41, 0x9A], &[0x41, 0xFF]]),
            duration: 3000,
            keyframe: false,
        };
        let out = annexb(&sample, &config());
        assert_eq!(
            out.windows(4).filter(|w| *w == START_CODE).count(),
            3,
            "one start code per NAL unit"
        );
    }

    #[test]
    fn a_length_prefix_past_the_end_stops_the_walk() {
        // Not reachable from our own encoder, but this is a length-prefixed parse loop
        // and those should not be able to panic whatever they are handed.
        for data in [
            vec![0xFF, 0xFF, 0xFF, 0xFF, 0x41],
            vec![0x00, 0x00, 0x00, 0x09, 0x41],
            vec![0x00, 0x00, 0x00, 0x00],
            vec![0x00, 0x00],
            vec![],
        ] {
            let sample = Sample {
                data,
                duration: 3000,
                keyframe: false,
            };
            let _ = annexb(&sample, &config());
        }
    }

    #[test]
    fn the_duration_becomes_wall_time() {
        let sample = Sample {
            data: avcc(&[&[0x41]]),
            // TIMESCALE ticks is one second by definition.
            duration: TIMESCALE,
            keyframe: false,
        };
        let feed = Arc::new(LiveFeed::new());
        let mut sub = feed.subscribe();
        feed.publish(&sample, &config());
        let frame = sub.frames.try_recv().unwrap();
        assert!((frame.duration.as_secs_f64() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn nothing_is_converted_when_nobody_is_watching() {
        // The check is on the encode thread, so it has to be the cheap one.
        let feed = LiveFeed::new();
        assert!(!feed.watched());
        feed.publish(
            &Sample {
                data: avcc(&[&[0x41]]),
                duration: 3000,
                keyframe: false,
            },
            &config(),
        );
    }

    #[test]
    fn attaching_asks_for_a_keyframe_and_detaching_is_counted() {
        // A joining viewer cannot decode until one arrives, so it must not have to wait
        // out the GOP.
        let feed = Arc::new(LiveFeed::new());
        assert!(!feed.take_keyframe_request());
        let sub = feed.subscribe();
        assert!(feed.watched());
        assert!(feed.take_keyframe_request());
        assert!(!feed.take_keyframe_request(), "the request is one-shot");
        drop(sub);
        assert!(!feed.watched(), "the guard decremented on drop");
    }

    #[test]
    fn several_viewers_are_counted_independently() {
        let feed = Arc::new(LiveFeed::new());
        let a = feed.subscribe();
        let b = feed.subscribe();
        drop(a);
        assert!(feed.watched(), "one leaving does not detach the other");
        drop(b);
        assert!(!feed.watched());
    }
}
