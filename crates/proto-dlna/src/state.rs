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
    /// A session event to forward to the session manager, if this action produced one.
    pub event: Option<SessionEvent>,
}

impl Outcome {
    fn empty() -> Self {
        Self::default()
    }

    fn args(out_args: Vec<(String, String)>) -> Self {
        Self {
            out_args,
            event: None,
        }
    }
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
    /// Dispatch an AVTransport action.
    ///
    /// # Errors
    /// [`DlnaError`] for unknown actions, missing/invalid arguments.
    pub fn av_transport(&mut self, action: &SoapAction) -> Result<Outcome, DlnaError> {
        match action.name.as_str() {
            "SetAVTransportURI" => {
                let uri = action.require("CurrentURI")?.to_string();
                self.current_uri_metadata = action.arg("CurrentURIMetaData").unwrap_or("").into();
                self.current_uri = Some(uri);
                if self.state == TransportState::NoMediaPresent {
                    self.state = TransportState::Stopped;
                }
                Ok(Outcome::empty())
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
                let event = if resuming {
                    SessionEvent::Control(ControlTxn::Play)
                } else {
                    let source = MediaUri::parse(&uri)
                        .map_err(|_| DlnaError::InvalidArgument("CurrentURI not a valid URI"))?;
                    SessionEvent::Play {
                        source,
                        start: None,
                    }
                };
                Ok(Outcome {
                    out_args: vec![],
                    event: Some(event),
                })
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
                ("TrackDuration".into(), "0:00:00".into()),
                ("TrackMetaData".into(), self.current_uri_metadata.clone()),
                (
                    "TrackURI".into(),
                    self.current_uri.clone().unwrap_or_default(),
                ),
                ("RelTime".into(), "0:00:00".into()),
                ("AbsTime".into(), "0:00:00".into()),
                ("RelCount".into(), "0".into()),
                ("AbsCount".into(), "0".into()),
            ])),
            "GetMediaInfo" => Ok(Outcome::args(vec![
                ("NrTracks".into(), "1".into()),
                ("MediaDuration".into(), "0:00:00".into()),
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
        const SINK: &str = "http-get:*:video/*:*,http-get:*:audio/*:*,http-get:*:image/*:*";
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
        event: Some(SessionEvent::Control(txn)),
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
        assert!(matches!(out.event, Some(SessionEvent::Play { .. })));
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
            out.event,
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
            out.event,
            Some(SessionEvent::Control(ControlTxn::Seek(d))) if d == Duration::from_secs(90)
        ));
    }

    #[test]
    fn set_volume_emits_scaled_control() {
        let mut r = Renderer::default();
        let out = r
            .rendering_control(&action("SetVolume", &[("DesiredVolume", "75")]))
            .unwrap();
        assert_eq!(r.volume, 75);
        match out.event {
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
