//! The MediaRenderer state machine — pure and socket-free (ground rule 3). Maps a
//! parsed [`SoapAction`] to output arguments plus an optional [`SessionEvent`] to emit.
//! The three UPnP services (AVTransport, RenderingControl, ConnectionManager) share
//! one [`Renderer`] value because their state is coupled (transport ↔ volume ↔ URI).

use std::time::Duration;

use castaway_core::{ControlTxn, MediaUri, PlaybackEnd, PlaybackProgress, SessionEvent};

use crate::error::DlnaError;
use crate::soap::SoapAction;

/// UPnP `TransportState` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    /// No media set.
    NoMediaPresent,
    /// Media set but stopped.
    Stopped,
    /// Playing.
    Playing,
    /// Paused.
    PausedPlayback,
    /// Between states (loading).
    Transitioning,
}

impl TransportState {
    /// The exact string UPnP control points expect.
    #[must_use]
    pub const fn as_upnp(self) -> &'static str {
        match self {
            TransportState::NoMediaPresent => "NO_MEDIA_PRESENT",
            TransportState::Stopped => "STOPPED",
            TransportState::Playing => "PLAYING",
            TransportState::PausedPlayback => "PAUSED_PLAYBACK",
            TransportState::Transitioning => "TRANSITIONING",
        }
    }
}

/// UPnP `TransportStatus` — whether the last thing the transport was asked to do worked.
///
/// Two values, and only two: §2.2.2 defines `OK` and `ERROR_OCCURRED` and leaves the rest
/// vendor-specific. Modelled rather than hardcoded to `"OK"` because a URL the box cannot
/// fetch is precisely what the other value is for, and a receiver that only ever says `OK`
/// leaves the phone showing a healthy session over a blank panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportStatus {
    /// Everything the transport was asked to do, it did.
    #[default]
    Ok,
    /// The last operation failed — the fetch, or the decode.
    ErrorOccurred,
}

impl TransportStatus {
    /// The exact string UPnP control points expect.
    #[must_use]
    pub const fn as_upnp(self) -> &'static str {
        match self {
            TransportStatus::Ok => "OK",
            TransportStatus::ErrorOccurred => "ERROR_OCCURRED",
        }
    }
}

/// The result of applying one action: response arguments plus an optional event.
#[derive(Debug, Default)]
pub struct Outcome {
    /// `(name, value)` output arguments for the SOAP response.
    pub out_args: Vec<(String, String)>,
    /// Session events to forward to the session manager, in order.
    ///
    /// A list because one action really does mean two things: `Play` starts the media
    /// *and* publishes what the control point said it is, and the card would otherwise
    /// have to wait for a second action that never comes.
    pub events: Vec<SessionEvent>,
}

impl Outcome {
    fn empty() -> Self {
        Self::default()
    }

    /// An outcome carrying several events.
    fn with_events(events: Vec<SessionEvent>) -> Self {
        Self {
            out_args: Vec::new(),
            events,
        }
    }

    fn args(out_args: Vec<(String, String)>) -> Self {
        Self {
            out_args,
            events: Vec::new(),
        }
    }
}

/// What AVTransport requires a service to report for a value it cannot supply
/// (§2.2.22–§2.2.23). A control point reads it as "no such information" and draws no
/// progress bar; a plausible-looking zero is read as a real position.
const NOT_IMPLEMENTED: &str = "NOT_IMPLEMENTED";

/// The counter equivalent (§2.2.24–§2.2.25): the maximum `i4`.
const NO_COUNTER: &str = "2147483647";

/// A [`Duration`] as the `H+:MM:SS` a control point parses.
///
/// Not the same grammar as `res@duration` in DIDL, which is why this is not
/// `didl::parse_duration` run backwards: AVTransport's `RelTime`/`TrackDuration` take no
/// sign and no fractional part in practice, and hours are unpadded and unbounded.
fn upnp_time(d: Duration) -> String {
    let total = d.as_secs();
    format!(
        "{}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

/// The MediaRenderer state. One instance per `InstanceID 0` (we support a single one,
/// which covers every real control point).
#[derive(Debug, Clone)]
pub struct Renderer {
    /// Current transport state.
    pub state: TransportState,
    /// The current media URI string (as the control point set it).
    pub current_uri: Option<String>,
    /// DIDL-Lite metadata blob the control point supplied, echoed back verbatim.
    pub current_uri_metadata: String,
    /// The queued next URI, if any.
    pub next_uri: Option<String>,
    /// Volume, 0–100 (UPnP scale).
    pub volume: u8,
    /// Mute flag.
    pub muted: bool,
    /// Whether the last thing the transport was asked to do worked.
    ///
    /// Sticky until the next `SetAVTransportURI` or `Play`: a control point may poll long
    /// after the failure, and clearing it on read would mean whichever of a phone's two
    /// pollers got there first saw the error and the other saw a healthy device.
    pub status: TransportStatus,
    /// Where the pipeline says the item has got to. Set on the last poll rather than
    /// stored: see [`Renderer::position`].
    position: Option<PlaybackProgress>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self {
            state: TransportState::NoMediaPresent,
            current_uri: None,
            current_uri_metadata: String::new(),
            next_uri: None,
            volume: 50,
            muted: false,
            status: TransportStatus::default(),
            position: None,
        }
    }
}

impl Renderer {
    /// Record where the pipeline says playback has reached.
    ///
    /// Pushed in rather than pulled out, so this module stays a pure function of its
    /// inputs (ground rule 3): the I/O shell reads the pipeline's report and hands it over
    /// with the request, which is also what lets the position tests drive an exact
    /// position instead of racing a real decoder.
    ///
    /// [`None`] means nothing is playing from a URL — the honest answer while a fetch is
    /// still in flight, and the only answer for a session some other protocol is pacing.
    pub fn observe_progress(&mut self, progress: Option<PlaybackProgress>) {
        self.position = progress;
    }

    /// The pipeline has finished with the item, or failed to play it.
    ///
    /// This is the whole reason the transport state stops lying. Before it existed the
    /// decode thread logged and exited, and a control point went on being told
    /// `PLAYING` / `OK` for a URL the box could not fetch — so the phone showed a healthy
    /// session over a blank panel, and a queued playlist waiting for the item to end
    /// waited for the life of the process.
    pub fn media_ended(&mut self, end: &PlaybackEnd) {
        self.state = TransportState::Stopped;
        self.position = None;
        self.status = if end.is_failure() {
            TransportStatus::ErrorOccurred
        } else {
            TransportStatus::Ok
        };
    }

    /// How long the item is, when anything knows.
    ///
    /// The container is the only party that has it, and a live stream genuinely does not
    /// have one — which is exactly the case a progress bar must not be drawn for, so the
    /// absence is carried through rather than filled in with a zero.
    fn duration(&self) -> Option<Duration> {
        self.position.and_then(|p| p.duration)
    }

    /// Begin playing `uri`, with whatever the control point said it is.
    ///
    /// Shared by `Play` and by a mid-playback `SetAVTransportURI`, because they mean the
    /// same thing to everything downstream — `RenderPipeline::play` already preempts the
    /// session in flight.
    fn start(&self, uri: &str) -> Result<Vec<SessionEvent>, DlnaError> {
        let source = MediaUri::parse(uri)
            .map_err(|_| DlnaError::InvalidArgument("CurrentURI not a valid URI"))?;
        let mut events = vec![SessionEvent::Play {
            source,
            start: None,
        }];
        // What the control point said this is, published right behind the play that makes
        // us the active source — the session manager drops metadata from a source that
        // does not hold the screen, so it cannot go earlier.
        let didl = crate::didl::parse(&self.current_uri_metadata);
        if !didl.is_empty() {
            let track = didl.apply_to(castaway_core::NowPlaying::new(
                castaway_core::PlaybackState::Playing,
            ));
            events.push(SessionEvent::NowPlaying(track));
        }
        Ok(events)
    }

    /// Dispatch an AVTransport action.
    ///
    /// # Errors
    /// [`DlnaError`] for unknown actions, missing/invalid arguments.
    pub fn av_transport(&mut self, action: &SoapAction) -> Result<Outcome, DlnaError> {
        match action.name.as_str() {
            "SetAVTransportURI" => {
                let uri = action.require("CurrentURI")?.to_string();
                self.current_uri_metadata = action.arg("CurrentURIMetaData").unwrap_or("").into();
                self.current_uri = Some(uri.clone());
                // A new item is a new verdict: whatever went wrong with the last URL says
                // nothing about this one, and leaving `ERROR_OCCURRED` up would have a
                // control point refuse to start a track that would have played.
                self.status = TransportStatus::Ok;
                self.position = None;
                match self.state {
                    // §2.4.1.3: "If the current transport state is 'NO MEDIA PRESENT' the
                    // transport state changes to 'STOPPED'."
                    TransportState::NoMediaPresent => {
                        self.state = TransportState::Stopped;
                        Ok(Outcome::empty())
                    }
                    // "If the current transport state is 'PLAYING' … this action does not
                    // change the transport state" — meaning it goes on playing, but *the
                    // new resource*. This is precisely how control points advance a queue:
                    // they set the next URI and never send a second Play.
                    //
                    // Treating it as a no-op meant album track 1 → track 2 showed the new
                    // title and PLAYING on the phone while the panel played track 1 to the
                    // end and froze on its last frame.
                    TransportState::Playing => Ok(Outcome::with_events(self.start(&uri)?)),
                    _ => Ok(Outcome::empty()),
                }
            }
            "SetNextAVTransportURI" => {
                self.next_uri = Some(action.require("NextURI")?.to_string());
                Ok(Outcome::empty())
            }
            "Play" => {
                let uri = self
                    .current_uri
                    .clone()
                    .ok_or(DlnaError::InvalidArgument("Play without media"))?;
                let resuming = self.state == TransportState::PausedPlayback;
                self.state = TransportState::Playing;
                self.status = TransportStatus::Ok;
                if resuming {
                    return Ok(Outcome::with_events(vec![SessionEvent::Control(
                        ControlTxn::Play,
                    )]));
                }
                Ok(Outcome::with_events(self.start(&uri)?))
            }
            "Pause" => {
                self.state = TransportState::PausedPlayback;
                Ok(with_event(ControlTxn::Pause))
            }
            "Stop" => {
                self.state = TransportState::Stopped;
                Ok(with_event(ControlTxn::Stop))
            }
            "Seek" => {
                let target = action.require("Target")?;
                let pos = parse_upnp_time(target)
                    .ok_or(DlnaError::InvalidArgument("Seek Target not H:MM:SS"))?;
                Ok(with_event(ControlTxn::Seek(pos)))
            }
            "Next" => Ok(with_event(ControlTxn::Next)),
            "Previous" => Ok(with_event(ControlTxn::Previous)),
            "GetTransportInfo" => Ok(Outcome::args(vec![
                ("CurrentTransportState".into(), self.state.as_upnp().into()),
                // Not hardcoded `OK` any more. A URL the box cannot fetch used to leave a
                // control point reading PLAYING / OK forever, which is the state a healthy
                // device is in — so there was nothing anywhere, on the panel or on the
                // phone, that said the cast had failed.
                (
                    "CurrentTransportStatus".into(),
                    self.status.as_upnp().into(),
                ),
                ("CurrentSpeed".into(), "1".into()),
            ])),
            "GetPositionInfo" => Ok(Outcome::args(vec![
                ("Track".into(), "1".into()),
                // The sentinel when there is nothing to report, the real value when there
                // is, and the distinction is the spec's own: AVTransport:1 §2.2.22 requires
                // `NOT_IMPLEMENTED` when a service cannot supply the value. A control point
                // honours it by drawing no progress bar (`async_upnp_client` maps it to
                // `None`); `0:00:00` parses as a real zero, so a plausible-looking zero had
                // every control point drawing `0:00 / 0:00` with the scrubber pinned left
                // for the whole item — and a client that advances its queue on
                // `RelTime >= TrackDuration` saw `0 >= 0`.
                //
                // Position is *never* evented (§2.3.1 excludes it from LastChange), so this
                // action is the entire position channel and control points poll it once a
                // second while playing.
                (
                    "TrackDuration".into(),
                    self.duration()
                        .map_or_else(|| NOT_IMPLEMENTED.to_string(), upnp_time),
                ),
                ("TrackMetaData".into(), self.current_uri_metadata.clone()),
                (
                    "TrackURI".into(),
                    self.current_uri.clone().unwrap_or_default(),
                ),
                (
                    "RelTime".into(),
                    self.position
                        .map_or_else(|| NOT_IMPLEMENTED.to_string(), |p| upnp_time(p.position)),
                ),
                // The same number: `AbsTime` is a position in the *media*, and for a
                // single-resource renderer with no ABS_TIME seek that is the same place
                // `RelTime` names. Reporting one and not the other loses the control
                // points that read only the other one.
                (
                    "AbsTime".into(),
                    self.position
                        .map_or_else(|| NOT_IMPLEMENTED.to_string(), |p| upnp_time(p.position)),
                ),
                // The counter sentinel is a number, not a string: §2.2.24/§2.2.25 specify
                // the maximum `i4` for a service that does not track counters.
                ("RelCount".into(), NO_COUNTER.into()),
                ("AbsCount".into(), NO_COUNTER.into()),
            ])),
            "GetMediaInfo" => Ok(Outcome::args(vec![
                ("NrTracks".into(), "1".into()),
                (
                    "MediaDuration".into(),
                    self.duration()
                        .map_or_else(|| NOT_IMPLEMENTED.to_string(), upnp_time),
                ),
                (
                    "CurrentURI".into(),
                    self.current_uri.clone().unwrap_or_default(),
                ),
                (
                    "CurrentURIMetaData".into(),
                    self.current_uri_metadata.clone(),
                ),
                ("NextURI".into(), self.next_uri.clone().unwrap_or_default()),
                ("NextURIMetaData".into(), String::new()),
                ("PlayMedium".into(), "NETWORK".into()),
                ("RecordMedium".into(), "NOT_IMPLEMENTED".into()),
                ("WriteStatus".into(), "NOT_IMPLEMENTED".into()),
            ])),
            "GetDeviceCapabilities" => Ok(Outcome::args(vec![
                ("PlayMedia".into(), "NETWORK,HTTP-GET".into()),
                ("RecMedia".into(), "NOT_IMPLEMENTED".into()),
                ("RecQualityModes".into(), "NOT_IMPLEMENTED".into()),
            ])),
            "GetTransportSettings" => Ok(Outcome::args(vec![
                ("PlayMode".into(), "NORMAL".into()),
                ("RecQualityMode".into(), "NOT_IMPLEMENTED".into()),
            ])),
            other => Err(DlnaError::InvalidAction(other.to_string())),
        }
    }

    /// Dispatch a RenderingControl action.
    ///
    /// # Errors
    /// [`DlnaError`] for unknown actions or invalid arguments.
    pub fn rendering_control(&mut self, action: &SoapAction) -> Result<Outcome, DlnaError> {
        match action.name.as_str() {
            "GetVolume" => Ok(Outcome::args(vec![(
                "CurrentVolume".into(),
                self.volume.to_string(),
            )])),
            "SetVolume" => {
                let v: u8 = action
                    .require("DesiredVolume")?
                    .parse()
                    .map_err(|_| DlnaError::InvalidArgument("DesiredVolume not 0-100"))?;
                self.volume = v.min(100);
                let scaled = f32::from(self.volume) / 100.0;
                Ok(with_event(ControlTxn::Volume(scaled)))
            }
            "GetMute" => Ok(Outcome::args(vec![(
                "CurrentMute".into(),
                if self.muted { "1" } else { "0" }.into(),
            )])),
            "SetMute" => {
                let m = matches!(action.require("DesiredMute")?, "1" | "true" | "True");
                self.muted = m;
                Ok(with_event(ControlTxn::Mute(m)))
            }
            "ListPresets" => Ok(Outcome::args(vec![(
                "CurrentPresetNameList".into(),
                "FactoryDefaults".into(),
            )])),
            "SelectPreset" => Ok(Outcome::empty()),
            other => Err(DlnaError::InvalidAction(other.to_string())),
        }
    }

    /// Dispatch a ConnectionManager action.
    ///
    /// # Errors
    /// [`DlnaError::InvalidAction`] for unknown actions.
    pub fn connection_manager(&self, action: &SoapAction) -> Result<Outcome, DlnaError> {
        // What this renderer will actually take.
        //
        // Two things are load-bearing here, and both were learned the hard way by
        // gmrender-resurrect rather than by us:
        //
        // 1. **Globs are not enough.** `upnp_connmgr.c` says it verbatim — "BubbleUPnP
        //    does not seem to match generic `audio/*` types, but only matches mime-types
        //    _exactly_". A glob-only sink therefore matches *nothing* on one of the most
        //    widely used control points there is: the panel appears in its picker and
        //    then refuses every item, which is the exact failure shape this project keeps
        //    finding. So the common types are enumerated as well as globbed.
        // 2. **`x-` and non-`x-` are both needed.** The same file documents controllers
        //    disagreeing about `audio/x-m4a` vs `audio/m4a` vs `audio/mp4`, and about
        //    `audio/mpeg` vs `audio/x-mpeg`. Emitting one spelling loses whichever half
        //    of the field picked the other.
        //
        // The globs stay in front because controllers that *do* honour them get the
        // widest answer, and the enumeration is what the strict ones read. `image/*` is
        // absent because nothing renders a still (G65).
        //
        // These are MIME-only entries with no `DLNA.ORG_PN` profile, which is legal and
        // testable — but note it obliges us to decode everything in the certification
        // table for each MIME we name, so this list should grow only as the decoder does.
        const SINK: &str = concat!(
            "http-get:*:video/*:*,http-get:*:audio/*:*,",
            // Audio, enumerated for the exact-matchers.
            "http-get:*:audio/mpeg:*,http-get:*:audio/x-mpeg:*,",
            "http-get:*:audio/mp4:*,http-get:*:audio/m4a:*,http-get:*:audio/x-m4a:*,",
            "http-get:*:audio/aac:*,http-get:*:audio/x-aac:*,",
            "http-get:*:audio/flac:*,http-get:*:audio/x-flac:*,",
            "http-get:*:audio/ogg:*,http-get:*:audio/x-ogg:*,",
            "http-get:*:audio/vorbis:*,http-get:*:audio/opus:*,",
            "http-get:*:audio/wav:*,http-get:*:audio/x-wav:*,",
            "http-get:*:audio/L16:*,",
            // Video.
            "http-get:*:video/mp4:*,http-get:*:video/x-matroska:*,",
            "http-get:*:video/mpeg:*,http-get:*:video/quicktime:*,",
            "http-get:*:video/x-msvideo:*,http-get:*:video/avi:*,",
            "http-get:*:video/webm:*,http-get:*:video/x-m4v:*",
        );
        match action.name.as_str() {
            "GetProtocolInfo" => Ok(Outcome::args(vec![
                ("Source".into(), String::new()),
                ("Sink".into(), SINK.into()),
            ])),
            "GetCurrentConnectionIDs" => {
                Ok(Outcome::args(vec![("ConnectionIDs".into(), "0".into())]))
            }
            "GetCurrentConnectionInfo" => Ok(Outcome::args(vec![
                ("RcsID".into(), "0".into()),
                ("AVTransportID".into(), "0".into()),
                ("ProtocolInfo".into(), SINK.into()),
                ("PeerConnectionManager".into(), String::new()),
                ("PeerConnectionID".into(), "-1".into()),
                ("Direction".into(), "Input".into()),
                ("Status".into(), "OK".into()),
            ])),
            other => Err(DlnaError::InvalidAction(other.to_string())),
        }
    }
}

fn with_event(txn: ControlTxn) -> Outcome {
    Outcome {
        out_args: vec![],
        events: vec![SessionEvent::Control(txn)],
    }
}

/// Parse a UPnP time value (`H:MM:SS`, `HH:MM:SS`, optionally with `.fff`) to a
/// [`Duration`]. Returns `None` on malformed input.
fn parse_upnp_time(s: &str) -> Option<Duration> {
    let mut parts = s.trim().split(':');
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let sec_part = parts.next()?;
    if parts.next().is_some() {
        return None; // too many fields
    }
    let (secs, millis) = match sec_part.split_once('.') {
        Some((s, frac)) => {
            let secs: u64 = s.parse().ok()?;
            // Take up to 3 fractional digits as milliseconds.
            let frac: String = frac.chars().take(3).collect();
            let scale = 10u64.pow(3 - u32::try_from(frac.len()).ok()?);
            let millis: u64 = frac.parse().ok()?;
            (secs, millis * scale)
        }
        None => (sec_part.parse().ok()?, 0),
    };
    if m >= 60 || secs >= 60 {
        return None;
    }
    Some(Duration::from_millis(
        ((h * 3600 + m * 60 + secs) * 1000) + millis,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn action(name: &str, args: &[(&str, &str)]) -> SoapAction {
        SoapAction {
            name: name.into(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn set_uri_then_play_emits_play_event() {
        let mut r = Renderer::default();
        r.av_transport(&action(
            "SetAVTransportURI",
            &[("CurrentURI", "http://10.0.0.9/v.mp4")],
        ))
        .unwrap();
        assert_eq!(r.state, TransportState::Stopped);
        let out = r.av_transport(&action("Play", &[("Speed", "1")])).unwrap();
        assert_eq!(r.state, TransportState::Playing);
        assert!(matches!(
            out.events.first(),
            Some(SessionEvent::Play { .. })
        ));
    }

    #[test]
    fn pause_then_play_resumes_via_control() {
        let mut r = Renderer::default();
        r.av_transport(&action(
            "SetAVTransportURI",
            &[("CurrentURI", "http://x/v.mp4")],
        ))
        .unwrap();
        r.av_transport(&action("Play", &[])).unwrap();
        r.av_transport(&action("Pause", &[])).unwrap();
        assert_eq!(r.state, TransportState::PausedPlayback);
        let out = r.av_transport(&action("Play", &[])).unwrap();
        assert!(matches!(
            out.events.first(),
            Some(SessionEvent::Control(ControlTxn::Play))
        ));
    }

    #[test]
    fn play_without_media_is_error() {
        let mut r = Renderer::default();
        assert!(r.av_transport(&action("Play", &[])).is_err());
    }

    #[test]
    fn seek_parses_target_time() {
        let mut r = Renderer::default();
        let out = r
            .av_transport(&action(
                "Seek",
                &[("Unit", "REL_TIME"), ("Target", "0:01:30")],
            ))
            .unwrap();
        assert!(matches!(
            out.events.first(),
            Some(SessionEvent::Control(ControlTxn::Seek(d))) if *d == Duration::from_secs(90)
        ));
    }

    #[test]
    fn set_volume_emits_scaled_control() {
        let mut r = Renderer::default();
        let out = r
            .rendering_control(&action("SetVolume", &[("DesiredVolume", "75")]))
            .unwrap();
        assert_eq!(r.volume, 75);
        match out.events.first() {
            Some(SessionEvent::Control(ControlTxn::Volume(v))) => {
                assert!((v - 0.75).abs() < 1e-6);
            }
            _ => panic!("expected volume control"),
        }
    }

    #[test]
    fn get_transport_info_reports_state() {
        let mut r = Renderer::default();
        let out = r.av_transport(&action("GetTransportInfo", &[])).unwrap();
        assert!(out
            .out_args
            .contains(&("CurrentTransportState".into(), "NO_MEDIA_PRESENT".into())));
    }

    /// How every control point advances a queue: set the next URI and send no second
    /// Play. Treating that as a no-op meant album track 1 → track 2 showed the new title
    /// and PLAYING on the phone while the panel played track 1 to the end and froze.
    #[test]
    fn setting_a_new_uri_while_playing_switches_track() {
        let mut r = Renderer::default();
        r.av_transport(&action(
            "SetAVTransportURI",
            &[("CurrentURI", "http://h/1.mp3")],
        ))
        .unwrap();
        r.av_transport(&action("Play", &[("Speed", "1")])).unwrap();
        assert_eq!(r.state, TransportState::Playing);

        let out = r
            .av_transport(&action(
                "SetAVTransportURI",
                &[("CurrentURI", "http://h/2.mp3")],
            ))
            .unwrap();
        match out.events.first() {
            Some(SessionEvent::Play { source, .. }) => {
                assert_eq!(source.to_string(), "http://h/2.mp3");
            }
            other => panic!("expected a play for the new URI, got {other:?}"),
        }
        // §2.4.1.3: the transport state does not change — it goes on playing, the new item.
        assert_eq!(r.state, TransportState::Playing);
    }

    /// Setting a URI while stopped stages it and waits for Play, per the same section.
    #[test]
    fn setting_a_uri_while_stopped_waits_for_play() {
        let mut r = Renderer::default();
        let out = r
            .av_transport(&action(
                "SetAVTransportURI",
                &[("CurrentURI", "http://h/1.mp3")],
            ))
            .unwrap();
        assert!(out.events.is_empty());
        assert_eq!(r.state, TransportState::Stopped);
    }

    /// The nesting that decides whether a DLNA cast ever shows a title.
    ///
    /// `CurrentURIMetaData` is an XML document travelling as *text* inside another XML
    /// document, so it arrives escaped and has to be unescaped exactly once before it can
    /// be parsed. Once too few and the parser sees `&lt;DIDL-Lite&gt;` and finds nothing;
    /// once too many and a title containing an ampersand corrupts the document. This
    /// drives the whole path — a real SOAP envelope in, a `NowPlaying` out — because the
    /// two halves are correct separately and it is the join that goes wrong.
    #[test]
    fn escaped_didl_inside_a_soap_body_reaches_the_card() {
        let didl = concat!(
            r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
            r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item>"#,
            "<dc:title>Rock &amp; Roll</dc:title>",
            "<upnp:artist>Aphex Twin</upnp:artist>",
            "<upnp:class>object.item.audioItem.musicTrack</upnp:class>",
            r#"<res duration="0:06:06.000">http://h/a.mp3</res>"#,
            "</item></DIDL-Lite>",
        );
        // Escaped the way a control point escapes it to put it in the envelope.
        let escaped = didl
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let envelope = format!(
            concat!(
                r#"<?xml version="1.0"?><s:Envelope "#,
                r#"xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>"#,
                r#"<u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">"#,
                "<InstanceID>0</InstanceID><CurrentURI>http://h/a.mp3</CurrentURI>",
                "<CurrentURIMetaData>{}</CurrentURIMetaData>",
                "</u:SetAVTransportURI></s:Body></s:Envelope>",
            ),
            escaped
        );

        let set = crate::soap::SoapAction::parse(&envelope).unwrap();
        let mut r = Renderer::default();
        r.av_transport(&set).unwrap();
        let out = r.av_transport(&action("Play", &[("Speed", "1")])).unwrap();

        let track = out
            .events
            .iter()
            .find_map(|e| match e {
                SessionEvent::NowPlaying(t) => Some(t.clone()),
                _ => None,
            })
            .expect("Play should publish what the control point said this is");
        // The ampersand survived exactly one round of escaping in each direction.
        assert_eq!(track.title.as_deref(), Some("Rock & Roll"));
        assert_eq!(track.artist.as_deref(), Some("Aphex Twin"));
        assert_eq!(track.duration, Some(Duration::from_secs(366)));
    }

    /// A control point reads `GetProtocolInfo` to decide what it may send. Claiming a
    /// type nothing can render gets it sent — and a blank panel back.
    #[test]
    fn the_sink_claims_only_what_can_be_rendered() {
        let r = Renderer::default();
        let out = r
            .connection_manager(&action("GetProtocolInfo", &[]))
            .unwrap();
        let sink = out
            .out_args
            .iter()
            .find(|(k, _)| k == "Sink")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(sink.contains("video/*"));
        assert!(sink.contains("audio/*"));
        assert!(
            !sink.contains("image/"),
            "nothing renders a still, so nothing should ask us to (G62)"
        );
    }

    #[test]
    fn connection_manager_protocol_info() {
        let r = Renderer::default();
        let out = r
            .connection_manager(&action("GetProtocolInfo", &[]))
            .unwrap();
        assert!(out
            .out_args
            .iter()
            .any(|(k, v)| k == "Sink" && v.contains("http-get")));
    }

    #[test]
    fn upnp_time_parsing() {
        assert_eq!(parse_upnp_time("1:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(
            parse_upnp_time("0:00:01.500"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(parse_upnp_time("bad"), None);
        assert_eq!(parse_upnp_time("0:99:00"), None);
    }

    /// A control point draws its scrubber from `GetPositionInfo` and nothing else —
    /// position is excluded from `LastChange`, so this action *is* the position channel.
    #[test]
    fn a_playing_item_reports_a_real_position_and_length() {
        let mut r = Renderer::default();
        r.av_transport(&action(
            "SetAVTransportURI",
            &[("CurrentURI", "http://h/film.mp4")],
        ))
        .unwrap();
        r.av_transport(&action("Play", &[])).unwrap();
        r.observe_progress(Some(
            PlaybackProgress::at(Duration::from_secs(95)).of(Duration::from_secs(5_425)),
        ));

        let out = r.av_transport(&action("GetPositionInfo", &[])).unwrap();
        let arg = |name: &str| {
            out.out_args
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(arg("RelTime"), "0:01:35");
        // Both, not one: control points disagree about which they read, and a renderer
        // that answers only the other one shows no progress on half of them.
        assert_eq!(arg("AbsTime"), "0:01:35");
        assert_eq!(arg("TrackDuration"), "1:30:25");

        // The same length reaches `GetMediaInfo`, which is where several control points
        // take the total from instead.
        let media = r.av_transport(&action("GetMediaInfo", &[])).unwrap();
        assert!(media
            .out_args
            .contains(&("MediaDuration".into(), "1:30:25".into())));
    }

    /// A live stream has no end, and one invented for it draws a bar that lies. The
    /// sentinel is also what a control point reads before the fetch has produced anything.
    #[test]
    fn an_unknown_length_stays_unknown_and_no_position_stays_the_sentinel() {
        let mut r = Renderer::default();
        r.observe_progress(Some(PlaybackProgress::at(Duration::from_secs(30))));
        let out = r.av_transport(&action("GetPositionInfo", &[])).unwrap();
        assert!(out.out_args.contains(&("RelTime".into(), "0:00:30".into())));
        assert!(out
            .out_args
            .contains(&("TrackDuration".into(), NOT_IMPLEMENTED.into())));

        r.observe_progress(None);
        let out = r.av_transport(&action("GetPositionInfo", &[])).unwrap();
        assert!(out
            .out_args
            .contains(&("RelTime".into(), NOT_IMPLEMENTED.into())));
    }

    /// The failure a receiver used to have no way of admitting: a URL it cannot fetch.
    /// PLAYING / OK is the state a *healthy* device is in, so a control point stuck on it
    /// had nothing to show the person holding the phone, and a queue never advanced.
    #[test]
    fn a_failed_fetch_stops_the_transport_and_says_why() {
        let mut r = Renderer::default();
        r.av_transport(&action(
            "SetAVTransportURI",
            &[("CurrentURI", "http://gone.invalid/v.mp4")],
        ))
        .unwrap();
        r.av_transport(&action("Play", &[])).unwrap();
        assert_eq!(r.state, TransportState::Playing);

        r.media_ended(&PlaybackEnd::Failed("connection refused".into()));

        let out = r.av_transport(&action("GetTransportInfo", &[])).unwrap();
        assert!(out
            .out_args
            .contains(&("CurrentTransportState".into(), "STOPPED".into())));
        assert!(out
            .out_args
            .contains(&("CurrentTransportStatus".into(), "ERROR_OCCURRED".into())));

        // …and the next item is judged on its own merits. A sticky error would have a
        // control point refuse to start a track that would have played perfectly.
        r.av_transport(&action(
            "SetAVTransportURI",
            &[("CurrentURI", "http://h/fine.mp4")],
        ))
        .unwrap();
        let out = r.av_transport(&action("GetTransportInfo", &[])).unwrap();
        assert!(out
            .out_args
            .contains(&("CurrentTransportStatus".into(), "OK".into())));
    }

    /// An item that simply ended is not an error, and a control point that reads it as one
    /// stops the playlist rather than advancing it.
    #[test]
    fn a_finished_item_stops_without_an_error() {
        let mut r = Renderer::default();
        r.av_transport(&action(
            "SetAVTransportURI",
            &[("CurrentURI", "http://h/a.mp3")],
        ))
        .unwrap();
        r.av_transport(&action("Play", &[])).unwrap();
        r.media_ended(&PlaybackEnd::Finished);

        let out = r.av_transport(&action("GetTransportInfo", &[])).unwrap();
        assert!(out
            .out_args
            .contains(&("CurrentTransportState".into(), "STOPPED".into())));
        assert!(out
            .out_args
            .contains(&("CurrentTransportStatus".into(), "OK".into())));
        // The position goes with the item: a scrubber left at the last position of a
        // track that has ended is a bar that never reaches its end.
        let out = r.av_transport(&action("GetPositionInfo", &[])).unwrap();
        assert!(out
            .out_args
            .contains(&("RelTime".into(), NOT_IMPLEMENTED.into())));
    }

    #[test]
    fn upnp_time_rendering_pads_minutes_and_seconds_but_not_hours() {
        assert_eq!(upnp_time(Duration::ZERO), "0:00:00");
        assert_eq!(upnp_time(Duration::from_secs(9)), "0:00:09");
        assert_eq!(upnp_time(Duration::from_secs(3_599)), "0:59:59");
        assert_eq!(upnp_time(Duration::from_secs(3_600)), "1:00:00");
        // Unbounded hours, because a renderer has no business truncating a long item.
        assert_eq!(upnp_time(Duration::from_secs(360_000)), "100:00:00");
        // Round-trips through our own reader, which is the pairing that matters: a Seek
        // target is written by a control point in the grammar we answer in.
        assert_eq!(
            parse_upnp_time(&upnp_time(Duration::from_secs(4_211))),
            Some(Duration::from_secs(4_211))
        );
    }

    #[test]
    fn unknown_action_is_invalid_action_fault() {
        let mut r = Renderer::default();
        let err = r.av_transport(&action("Frobnicate", &[])).unwrap_err();
        assert_eq!(err.upnp_code(), 401);
    }
}
