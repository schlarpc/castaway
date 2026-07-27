//! The MediaRenderer state machine — pure and socket-free (ground rule 3). Maps a
//! parsed [`SoapAction`] to output arguments plus an optional [`SessionEvent`] to emit.
//! The three UPnP services (AVTransport, RenderingControl, ConnectionManager) share
//! one [`Renderer`] value because their state is coupled (transport ↔ volume ↔ URI).

use std::time::Duration;

use castaway_core::{ControlTxn, MediaUri, SessionEvent};

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
        }
    }
}

impl Renderer {
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
                ("CurrentTransportStatus".into(), "OK".into()),
                ("CurrentSpeed".into(), "1".into()),
            ])),
            "GetPositionInfo" => Ok(Outcome::args(vec![
                ("Track".into(), "1".into()),
                // `NOT_IMPLEMENTED`, not `0:00:00`, and the distinction is the spec's own:
                // AVTransport:1 §2.2.22 requires that sentinel when a service cannot supply
                // the value. A control point honours it by drawing no progress bar
                // (`async_upnp_client` maps it to `None`); `0:00:00` parses as a real zero,
                // so every control point drew `0:00 / 0:00` with the scrubber pinned left
                // for the whole item — and a client that advances its queue on
                // `RelTime >= TrackDuration` sees `0 >= 0`.
                //
                // Position is *never* evented (§2.3.1 excludes it from LastChange), so this
                // action is the entire position channel and control points poll it once a
                // second while playing. The media clock that could answer it now exists in
                // `pipeline::clock`, but no seam carries it back across the `Pipeline`
                // trait — that is G69, and this sentinel is the honest answer until then.
                ("TrackDuration".into(), NOT_IMPLEMENTED.into()),
                ("TrackMetaData".into(), self.current_uri_metadata.clone()),
                (
                    "TrackURI".into(),
                    self.current_uri.clone().unwrap_or_default(),
                ),
                ("RelTime".into(), NOT_IMPLEMENTED.into()),
                ("AbsTime".into(), NOT_IMPLEMENTED.into()),
                // The counter sentinel is a number, not a string: §2.2.24/§2.2.25 specify
                // the maximum `i4` for a service that does not track counters.
                ("RelCount".into(), NO_COUNTER.into()),
                ("AbsCount".into(), NO_COUNTER.into()),
            ])),
            "GetMediaInfo" => Ok(Outcome::args(vec![
                ("NrTracks".into(), "1".into()),
                ("MediaDuration".into(), NOT_IMPLEMENTED.into()),
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

    #[test]
    fn unknown_action_is_invalid_action_fault() {
        let mut r = Renderer::default();
        let err = r.av_transport(&action("Frobnicate", &[])).unwrap_err();
        assert_eq!(err.upnp_code(), 401);
    }
}
