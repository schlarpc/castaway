//! The receiving peer connection: answer an offer, then turn the tracks it brings into
//! [`castaway_core::EncodedFrame`]s. See [`super`] for why this points the opposite way
//! to [`crate::remote`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use castaway_core::{AudioFormat, CoreError, EncodedFrame, FrameSource, MirrorAnswer, MirrorAudio};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8,
};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::runtime::{default_runtime, Runtime};

use super::assemble::{MirrorCodec, TrackAssembler};
use crate::error::PipelineError;
use crate::ice_ports::PortPool;

/// How long to wait for ICE gathering before answering with what we have.
///
/// The whole candidate list has to be in the answer — FCast signals the answer once and
/// has no second message — so this sits on the critical path of a sender connecting. Host
/// candidates on a LAN are gathered in milliseconds; the timeout is for when something is
/// wrong, and a partial list beats refusing the offer.
const GATHER_TIMEOUT: Duration = Duration::from_secs(3);

/// Frames queued from the network before the oldest is dropped.
///
/// Live mirroring: latency beats freshness (ground rule 4), so a decoder that has fallen
/// behind loses pictures rather than growing a queue. Two seconds of 60 fps video would be
/// 120; this is a third of a second, which is already more than a viewer would accept.
const FRAME_QUEUE: usize = 20;

/// How often to ask a silent sender for a keyframe, until one arrives.
///
/// A receiver that joins mid-stream sees only inter-coded pictures and decodes nothing,
/// with a healthy connection and no explanation. The PLI is the standard way to say so;
/// it stops as soon as the first keyframe lands, so a well-behaved sender pays for one.
const PLI_INTERVAL: Duration = Duration::from_millis(500);

/// How many keyframe requests to make before concluding the sender will not send one.
const PLI_ATTEMPTS: usize = 12;

/// What the mirroring receiver needs to exist.
#[derive(Debug, Clone)]
pub struct MirrorReceiverConfig {
    /// The UDP range ICE may bind — the *same pool object* the remote-control service
    /// uses, not a second allocator over the same numbers. `crates/app/src/surface.rs`
    /// declares the range once, and a socket outside it is one the deployed box's
    /// firewall silently drops.
    pub ice_ports: Arc<PortPool>,
    /// The addresses to offer candidates on, one socket each on the same port.
    ///
    /// Plural for the same reason as the remote-control service: a peer pairs a candidate
    /// of ours with one of its own, so offering only the advertised interface leaves a
    /// sender running *on the panel itself* with nothing to pair against.
    pub bind_ips: Vec<std::net::IpAddr>,
}

/// Answers mirroring offers and turns the tracks they bring into frames.
pub struct MirrorReceiver {
    config: MirrorReceiverConfig,
    shared: Arc<Shared>,
}

/// What the receiver and the connections it opened both hold.
///
/// Split out rather than putting the whole receiver behind an `Arc` because the trait
/// hands out `&self`: a handler that needed `Arc<MirrorReceiver>` would force
/// [`castaway_core::MirrorBackend`] to be `Arc`-taking for one implementation's
/// convenience. The handler needs the ports and the session slot, and nothing else.
struct Shared {
    runtime: Arc<dyn Runtime>,
    ports: Arc<PortPool>,
    /// The live session, if any. One at a time: a mirror fills the panel, so a second
    /// offer replaces the first rather than racing it for the screen.
    active: Mutex<Option<Session>>,
}

struct Session {
    connection: Arc<dyn PeerConnection>,
    port: u16,
}

impl std::fmt::Debug for MirrorReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorReceiver")
            .field("bind_ips", &self.config.bind_ips)
            .finish_non_exhaustive()
    }
}

impl MirrorReceiver {
    /// Build the receiver.
    ///
    /// # Errors
    /// [`PipelineError::Remote`] if no async runtime is available to drive peer
    /// connections — which means the crate was built without one, not that the box is
    /// short of anything.
    pub fn new(config: MirrorReceiverConfig) -> Result<Arc<Self>, PipelineError> {
        let runtime = default_runtime()
            .ok_or_else(|| PipelineError::Remote("no async runtime for WebRTC".into()))?;
        let ports = Arc::clone(&config.ice_ports);
        Ok(Arc::new(Self {
            config,
            shared: Arc::new(Shared {
                runtime,
                ports,
                active: Mutex::new(None),
            }),
        }))
    }

    /// Whether a mirroring session is up.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.shared.active.lock().is_ok_and(|held| held.is_some())
    }

    async fn build(&self, offer_sdp: &str) -> Result<MirrorAnswer, PipelineError> {
        let offer = RTCSessionDescription::offer(offer_sdp.to_owned())
            .map_err(|e| PipelineError::Remote(format!("offer: {e}")))?;
        let wants_audio = offer_has_audio(offer_sdp);

        let port = self
            .shared
            .ports
            .take()
            .ok_or_else(|| PipelineError::Remote("every mirroring port is in use".into()))?;
        match self.negotiate(offer, wants_audio, port).await {
            Ok(answer) => Ok(answer),
            Err(e) => {
                // A negotiation that failed must not keep its port, or a run of bad offers
                // exhausts the range and the next real sender is refused.
                self.shared.ports.give_back(port);
                Err(e)
            }
        }
    }

    async fn negotiate(
        &self,
        offer: RTCSessionDescription,
        wants_audio: bool,
        port: u16,
    ) -> Result<MirrorAnswer, PipelineError> {
        let mut media_engine = MediaEngine::default();
        for (kind, codec) in receivable_codecs() {
            media_engine
                .register_codec(codec, kind)
                .map_err(|e| PipelineError::Remote(format!("codec: {e}")))?;
        }
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|e| PipelineError::Remote(format!("interceptors: {e}")))?;

        // The channels are made *before* the answer goes out, because `on_track` fires on
        // the first RTP packet — which can arrive before the caller has finished sending
        // our answer, let alone emitted the session event.
        let (video_tx, video_rx) = mpsc::channel(FRAME_QUEUE);
        let (audio_tx, audio_rx) = mpsc::channel(FRAME_QUEUE);
        let gathered = Arc::new(tokio::sync::Notify::new());
        let handler = Arc::new(TrackHandler {
            shared: Arc::clone(&self.shared),
            gathered: Arc::clone(&gathered),
            video: video_tx,
            audio: audio_tx,
        });

        // No ICE servers: a LAN receiver, whose peer is on the same network.
        let configuration = RTCConfigurationBuilder::new().build();
        let connection = PeerConnectionBuilder::new()
            .with_configuration(configuration)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_runtime(Arc::clone(&self.shared.runtime))
            .with_udp_addrs(
                self.config
                    .bind_ips
                    .iter()
                    .map(|ip| std::net::SocketAddr::new(*ip, port))
                    .collect(),
            )
            .build()
            .await
            .map_err(|e| PipelineError::Remote(format!("peer connection: {e}")))?;
        let connection: Arc<dyn PeerConnection> = Arc::from(Box::new(connection) as Box<_>);

        // No local track is added: the offer's media sections become receive-only
        // transceivers, which is the whole shape of this direction.
        connection
            .set_remote_description(offer)
            .await
            .map_err(|e| PipelineError::Remote(format!("remote description: {e}")))?;
        let answer = connection
            .create_answer(None)
            .await
            .map_err(|e| PipelineError::Remote(format!("answer: {e}")))?;
        connection
            .set_local_description(answer)
            .await
            .map_err(|e| PipelineError::Remote(format!("local description: {e}")))?;

        if tokio::time::timeout(GATHER_TIMEOUT, gathered.notified())
            .await
            .is_err()
        {
            warn!("mirror: ICE gathering did not complete; answering with what we have");
        }
        let local = connection
            .local_description()
            .await
            .ok_or_else(|| PipelineError::Remote("no local description after gathering".into()))?;

        // Only now is the session worth keeping: everything above could have failed, and
        // a half-built connection in the slot is one `forget` would have to reason about.
        //
        // The previous session is replaced and *then* closed, rather than closed first, so
        // the new one never binds the port the old one is still holding. That costs a
        // second port for as long as a teardown takes, which the 32-port default range has
        // and to spare.
        let previous = self.shared.active.lock().ok().and_then(|mut held| {
            held.replace(Session {
                connection: Arc::clone(&connection),
                port,
            })
        });
        if let Some(previous) = previous {
            self.shared.close(previous);
        }
        info!(
            port,
            audio = wants_audio,
            "mirror: answered a sender's offer"
        );

        Ok(MirrorAnswer {
            sdp: local.sdp,
            video: FrameSource::Encoded(video_rx),
            // Declared from the offer rather than from the first packet: the session event
            // carries it, and it is sent before any audio has arrived. An offer with no
            // audio section gets `None` and the receiver plays a silent mirror, which is
            // what the sender asked for.
            audio: wants_audio.then(|| MirrorAudio {
                source: FrameSource::Encoded(audio_rx),
                // WebRTC Opus is 48 kHz, always, and the SDP's `opus/48000/2` says so.
                // `expect` is unreachable: both are non-zero literals.
                format: AudioFormat::from_hz(48_000, 2).unwrap_or_else(|| {
                    unreachable!("48 kHz stereo is a valid format");
                }),
                // Opus describes itself in-band; there is no out-of-band configuration a
                // decoder needs before it will open.
                config: None,
            }),
        })
    }

    /// Close whatever is up. Called when the process is going away, so nothing is left
    /// holding a socket the next run wants.
    pub async fn shutdown(&self) {
        let session = self
            .shared
            .active
            .lock()
            .ok()
            .and_then(|mut held| held.take());
        if let Some(session) = session {
            self.shared.ports.give_back(session.port);
            let _ = session.connection.close().await;
        }
    }
}

impl Shared {
    /// Take the live session out of the slot, if there is one.
    fn take_active(&self) -> Option<Session> {
        self.active.lock().ok().and_then(|mut held| held.take())
    }

    /// Tear a session down, returning its port to the pool **once the socket is shut**.
    ///
    /// The ordering is the whole content. Returning the port first and closing after is
    /// what the obvious code does, and it hands the next session a port whose UDP socket
    /// the previous connection has not released yet: the bind fails, and the failure looks
    /// like "the offer was not answerable" with nothing to say why. Three offers over a
    /// four-port pool is enough to hit it on a loaded box, which is how it was found.
    fn close(&self, session: Session) {
        let Session { connection, port } = session;
        let ports = Arc::clone(&self.ports);
        self.runtime.spawn(Box::pin(async move {
            let _ = connection.close().await;
            ports.give_back(port);
        }));
    }

    /// The sender's connection is over: drop it and free the port.
    ///
    /// Idempotent, because it is reached from two directions — the connection state
    /// machine and a track ending — and which arrives first is not ours to decide.
    fn forget(&self) {
        let session = self.take_active();
        if let Some(session) = session {
            info!(port = session.port, "mirror: sender gone");
            self.close(session);
        }
    }
}

#[async_trait::async_trait]
impl castaway_core::MirrorBackend for MirrorReceiver {
    async fn answer(&self, offer_sdp: &str) -> Result<MirrorAnswer, CoreError> {
        self.build(offer_sdp)
            .await
            .map_err(|e| CoreError::Pipeline(e.to_string()))
    }
}

/// Whether the offer has an audio section at all.
///
/// A scan rather than a full SDP parse because it is one question about one line type, and
/// the answer decides only whether the session event carries an audio half. A section
/// offered on port 0 is a *rejected* one — senders that renegotiate away their microphone
/// send exactly that — and must read as "no audio", which is the case a naive
/// `contains("m=audio")` gets wrong.
fn offer_has_audio(sdp: &str) -> bool {
    sdp.lines().any(|line| {
        let Some(rest) = line.trim_end().strip_prefix("m=audio ") else {
            return false;
        };
        rest.split_whitespace()
            .next()
            .is_some_and(|port| port != "0")
    })
}

/// The codecs the answer will accept.
///
/// H.264 and VP8 are WebRTC's mandatory-to-implement video codecs and Opus its mandatory
/// audio one, so every sender has all three; anything else is refused by not being
/// registered, which is a codec that is never negotiated rather than one that arrives and
/// is dropped. The payload types are the ones a sender is used to seeing — and are *not*
/// what packets arrive stamped with, because an answerer takes the offerer's numbering.
fn receivable_codecs() -> Vec<(RtpCodecKind, RTCRtpCodecParameters)> {
    vec![
        (
            RtpCodecKind::Video,
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 102,
            },
        ),
        (
            RtpCodecKind::Video,
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_VP8.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: String::new(),
                    rtcp_feedback: vec![],
                },
                payload_type: 96,
            },
        ),
        (
            RtpCodecKind::Audio,
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
            },
        ),
    ]
}

struct TrackHandler {
    shared: Arc<Shared>,
    /// Signalled with `notify_one`, not `notify_waiters`: on a LAN, host candidates are
    /// gathered before the answer path gets as far as waiting, and `notify_waiters` wakes
    /// only whoever is *already* enrolled — so the answer sat out the whole
    /// [`GATHER_TIMEOUT`] before going out. `notify_one` leaves a permit, and the wait
    /// then returns immediately whichever order the two arrive in.
    gathered: Arc<tokio::sync::Notify>,
    video: mpsc::Sender<EncodedFrame>,
    audio: mpsc::Sender<EncodedFrame>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for TrackHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.notify_one();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        debug!(?state, "mirror: connection state");
        if matches!(
            state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Closed
        ) {
            self.shared.forget();
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let shared = Arc::clone(&self.shared);
        let video = self.video.clone();
        let audio = self.audio.clone();
        self.shared.runtime.spawn(Box::pin(async move {
            pump(&track, &video, &audio).await;
            // A track ending is the session ending: a sender that stops sending pictures
            // has stopped mirroring, whatever the ICE state still says.
            shared.forget();
        }));
    }
}

/// Poll one remote track to exhaustion, turning its packets into frames.
async fn pump(
    track: &Arc<dyn TrackRemote>,
    video: &mpsc::Sender<EncodedFrame>,
    audio: &mpsc::Sender<EncodedFrame>,
) {
    let mut assembler: Option<TrackAssembler> = None;
    let mut is_video = false;
    // Requesting a keyframe is what makes a mid-stream join show anything at all; it stops
    // as soon as one arrives, so the common case costs one RTCP packet.
    let seen_keyframe = Arc::new(AtomicBool::new(false));
    let mut pli_task: Option<tokio::task::JoinHandle<()>> = None;

    while let Some(event) = track.poll().await {
        match event {
            TrackRemoteEvent::OnOpen(init) => {
                let Some(codec) = track
                    .codec(init.ssrc)
                    .await
                    .and_then(|codec| MirrorCodec::from_mime(&codec.mime_type))
                else {
                    // Only codecs we registered can be negotiated, so this is a
                    // never-taken branch rather than a case to handle — but it is the
                    // branch that would otherwise be an `unwrap` on a peer's SDP.
                    warn!("mirror: a track arrived in a codec we never offered");
                    return;
                };
                info!(?codec, ssrc = init.ssrc, "mirror: track open");
                is_video = codec.is_video();
                if is_video {
                    pli_task = Some(spawn_keyframe_requests(
                        Arc::clone(track),
                        init.ssrc,
                        Arc::clone(&seen_keyframe),
                    ));
                }
                assembler = Some(TrackAssembler::new(codec));
            }
            TrackRemoteEvent::OnRtpPacket(packet) => {
                let Some(assembler) = assembler.as_mut() else {
                    continue;
                };
                let Some(frame) = assembler.push(
                    packet.header.timestamp,
                    packet.header.marker,
                    &packet.payload,
                ) else {
                    continue;
                };
                if frame.keyframe && is_video {
                    seen_keyframe.store(true, Ordering::Relaxed);
                }
                let sink = if is_video { video } else { audio };
                // Drop the *oldest*, by refusing the newest only when the consumer is
                // gone: `try_send` on a full queue means the decoder is behind, and for a
                // live mirror the honest answer is to lose the picture (ground rule 4).
                match sink.try_send(frame) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        debug!(video = is_video, "mirror: decoder behind; frame dropped");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
            TrackRemoteEvent::OnEnded | TrackRemoteEvent::OnError => break,
            TrackRemoteEvent::OnEnding
            | TrackRemoteEvent::OnMute
            | TrackRemoteEvent::OnUnmute
            | TrackRemoteEvent::OnRtcpPacket(_) => {}
        }
    }
    if let Some(task) = pli_task {
        task.abort();
    }
    // Whatever was half-assembled when the sender stopped is not a picture.
    debug!("mirror: track ended");
}

/// Ask the sender for a keyframe until one arrives, or until we give up.
fn spawn_keyframe_requests(
    track: Arc<dyn TrackRemote>,
    ssrc: u32,
    seen: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        for _ in 0..PLI_ATTEMPTS {
            if seen.load(Ordering::Relaxed) {
                return;
            }
            let pli =
                rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication {
                    sender_ssrc: 0,
                    media_ssrc: ssrc,
                };
            if track.write_rtcp(vec![Box::new(pli)]).await.is_err() {
                return;
            }
            tokio::time::sleep(PLI_INTERVAL).await;
        }
        if !seen.load(Ordering::Relaxed) {
            warn!(
                ssrc,
                "mirror: the sender never produced a keyframe; nothing can be decoded"
            );
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A section offered on port 0 is a rejected one — what a sender sends after
    /// renegotiating its microphone away — and must read as "no audio". The naive
    /// `contains("m=audio")` gets exactly this wrong, and the cost is a session event
    /// promising an audio track that never carries a packet.
    #[test]
    fn a_rejected_audio_section_is_not_audio() {
        let with_audio =
            "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
        let rejected =
            "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\nm=audio 0 UDP/TLS/RTP/SAVPF 111\r\n";
        let video_only = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\n";
        assert!(offer_has_audio(with_audio));
        assert!(!offer_has_audio(rejected));
        assert!(!offer_has_audio(video_only));
        // …and an `a=` line that merely mentions audio is not a media section.
        assert!(!offer_has_audio("v=0\r\na=group:BUNDLE m=audio 9\r\n"));
    }

    /// Every codec the answer registers is one the assembler can actually take apart.
    /// The two lists are the same decision written twice, and a codec in one but not the
    /// other is negotiated and then dropped — a healthy connection carrying nothing.
    #[test]
    fn every_registered_codec_can_be_depacketized() {
        for (kind, codec) in receivable_codecs() {
            let mime = &codec.rtp_codec.mime_type;
            let parsed = MirrorCodec::from_mime(mime)
                .unwrap_or_else(|| panic!("{mime} is registered but cannot be depacketized"));
            assert_eq!(
                parsed.is_video(),
                kind == RtpCodecKind::Video,
                "{mime} is registered as the wrong kind"
            );
            assert_eq!(
                parsed.clock_rate(),
                codec.rtp_codec.clock_rate,
                "{mime}'s clock rate disagrees with what the answer offers"
            );
        }
    }
}
