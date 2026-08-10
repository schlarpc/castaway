//! The peer connections themselves: one per remote, carrying the panel's duplicate out
//! and that peer's contacts back. See [`super`] for why WebRTC and why one connection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use input_touch::{InputOrigin, RemoteId, RemoteInputQueue};
use tracing::{debug, info, warn};

use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS,
};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use rtc::rtp_transceiver::PayloadType;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::rtp_transceiver::RtpSender;
use webrtc::runtime::{default_runtime, Runtime};

use crate::error::PipelineError;
use crate::stream::feed::{LiveFeed, Subscription};

use crate::ice_ports::PortPool;

/// The H.264 payload type this end *registers*.
///
/// Not the one packets go out stamped with — an answerer uses the offerer's numbering, and
/// a browser picks its own. See [`negotiated_payload_type`], which is what the pump asks.
/// 102 with `packetization-mode=1` and the constrained-baseline profile is what every
/// browser accepts and what our encoders emit.
const H264_PAYLOAD_TYPE: PayloadType = 102;

/// The Opus payload type this end registers (#259). Same caveat as
/// [`H264_PAYLOAD_TYPE`]: what goes on the wire is the offerer's number.
const OPUS_PAYLOAD_TYPE: PayloadType = 111;

/// How long to wait for ICE gathering before answering anyway.
///
/// Non-trickle means the answer has to carry every candidate, so this is on the critical
/// path of a peer connecting. Host candidates on a LAN are gathered in milliseconds; the
/// timeout is for the case where something is wrong, and answering with what we have
/// beats failing the request.
const GATHER_TIMEOUT: Duration = Duration::from_secs(3);

/// What the remote-control service needs to exist.
pub struct RemoteConfig {
    /// The UDP range ICE may bind, from `[remote.ice_ports]`.
    ///
    /// Shared with FCast's mirroring receiver (#248) rather than allocated per service:
    /// one range with two allocators is two services confidently handing out the same
    /// port, and the second bind fails at the moment a real peer connects.
    pub ice_ports: std::sync::Arc<PortPool>,
    /// The addresses to offer candidates on, one socket each, all on the same port.
    ///
    /// Plural because a peer pairs a candidate of ours with one of its own, and a browser
    /// does not offer a loopback candidate at all — it gathers its real interfaces and
    /// nothing else. Binding only the advertised interface therefore leaves a browser *on
    /// the panel itself* with nothing to pair against: it offers 10.x and Tailscale
    /// addresses, we offer 127.0.0.1, and ICE reports "no candidate pairs" and stops.
    /// Binding both is what makes the panel's own page work as well as a phone's.
    pub bind_ips: Vec<std::net::IpAddr>,
    /// Whether a peer's input reaches the panel at all (`remote.input`).
    ///
    /// Enforced here rather than at the client, obviously, and enforced by *not queueing*
    /// rather than by not opening the channel: the page stays identical either way, which
    /// makes "watch but do not touch" a configuration rather than a second build.
    pub accept_input: bool,
}

/// Serves the panel's duplicate to remote peers, and routes what they send back.
pub struct RemoteService {
    config: RemoteConfig,
    feed: Arc<LiveFeed>,
    input: Arc<RemoteInputQueue>,
    /// Starts the encoder if it is not already running. The first peer to connect is what
    /// wakes the tap, exactly as the first playlist fetch is for HLS.
    start: Arc<dyn Fn() + Send + Sync>,
    runtime: Arc<dyn Runtime>,
    ports: Arc<PortPool>,
    /// The peer counter behind [`RemoteId`]. Never reset, so a reconnecting peer is a new
    /// origin and cannot inherit the contacts its previous connection left behind.
    next_peer: AtomicU64,
    /// Live peers, kept alive by being here — a `PeerConnection` dropped is a connection
    /// closed. Pruned when a peer's state machine says it is over.
    peers: Mutex<Vec<Peer>>,
}

/// One connected peer, for as long as it lasts.
struct Peer {
    id: RemoteId,
    connection: Arc<dyn PeerConnection>,
    /// The port its ICE socket took, returned to the pool when it goes.
    port: u16,
    /// The track its pictures go out on, and the SSRC they go out under. Held until the
    /// transport comes up, because that is when the pump may start.
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    /// The sender `add_track` returned, kept for one reason: it is the only thing that
    /// knows the payload type this peer actually negotiated. See [`RemoteService::spawn_pump`].
    sender: Arc<dyn RtpSender>,
    /// The track its sound goes out on (#259), with its own SSRC and sender — same
    /// reasons as the video triple, one media kind over.
    audio_track: Arc<TrackLocalStaticSample>,
    audio_ssrc: u32,
    audio_sender: Arc<dyn RtpSender>,
    /// Whether the peer's offer carried an audio m-line at all (#259). Read from the
    /// *offer*, not from the sender's parameters: an unmatched sender still reports the
    /// codec it was registered with, so its parameters cannot distinguish "negotiated
    /// Opus" from "was never asked".
    wants_audio: bool,
    /// Whether the pump has been started. `Connected` can be reported more than once —
    /// an ICE restart passes back through it — and two pumps on one track would
    /// interleave two copies of every frame into one sequence number space.
    pumping: bool,
}

impl std::fmt::Debug for RemoteService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteService")
            .field("peers", &self.peer_count())
            .field("accept_input", &self.config.accept_input)
            .finish_non_exhaustive()
    }
}

impl RemoteService {
    /// Build the service.
    ///
    /// # Errors
    /// [`PipelineError::Remote`] if no async runtime is available to drive peer
    /// connections — which means the crate was built without one, not that the box is
    /// short of anything.
    pub fn new(
        config: RemoteConfig,
        feed: Arc<LiveFeed>,
        input: Arc<RemoteInputQueue>,
        start: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Arc<Self>, PipelineError> {
        let runtime = default_runtime()
            .ok_or_else(|| PipelineError::Remote("no async runtime for WebRTC".into()))?;
        let ports = Arc::clone(&config.ice_ports);
        Ok(Arc::new(Self {
            config,
            feed,
            input,
            start,
            runtime,
            ports,
            next_peer: AtomicU64::new(1),
            peers: Mutex::new(Vec::new()),
        }))
    }

    /// How many peers are connected.
    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.peers.lock().map_or(0, |peers| peers.len())
    }

    /// What each connected peer's video actually goes out stamped with.
    ///
    /// A diagnostic, and the answer to the one question a silent-but-connected peer
    /// raises: an answerer uses the *offerer's* payload numbering, so this is whatever the
    /// browser picked and not what this end registers. When they disagreed, every packet
    /// was rejected by the transceiver as "unsupported codec type" while the connection
    /// itself stayed healthy — no video, and nothing but log noise to say why.
    pub async fn peer_payload_types(&self) -> Vec<(RemoteId, Option<PayloadType>)> {
        let senders: Vec<(RemoteId, Arc<dyn RtpSender>)> = match self.peers.lock() {
            Ok(peers) => peers
                .iter()
                .map(|p| (p.id, Arc::clone(&p.sender)))
                .collect(),
            Err(_) => Vec::new(),
        };
        let mut out = Vec::with_capacity(senders.len());
        for (id, sender) in senders {
            out.push((id, negotiated_payload_type(&sender).await));
        }
        out
    }

    /// The same diagnostic for the audio track (#259). `None` for a peer whose offer
    /// carried no audio m-line — its pictures still flow; only the sound is absent.
    ///
    /// Gated on [`Peer::wants_audio`] before the sender is asked, because an unmatched
    /// sender answers with the codec it was *registered* with — the same trap the video
    /// diagnostic exists to catch, one step earlier.
    pub async fn peer_audio_payload_types(&self) -> Vec<(RemoteId, Option<PayloadType>)> {
        let senders: Vec<(RemoteId, bool, Arc<dyn RtpSender>)> = match self.peers.lock() {
            Ok(peers) => peers
                .iter()
                .map(|p| (p.id, p.wants_audio, Arc::clone(&p.audio_sender)))
                .collect(),
            Err(_) => Vec::new(),
        };
        let mut out = Vec::with_capacity(senders.len());
        for (id, wants_audio, sender) in senders {
            let payload_type = if wants_audio {
                negotiated_payload_type(&sender).await
            } else {
                None
            };
            out.push((id, payload_type));
        }
        out
    }

    /// Answer a peer's offer, standing up its connection.
    ///
    /// # Errors
    /// [`PipelineError::Remote`] if the offer is not usable, no port is free, or the
    /// connection cannot be built.
    pub async fn answer(self: &Arc<Self>, offer_sdp: &str) -> Result<String, PipelineError> {
        // Starting the encoder before the negotiation rather than after: opening it takes
        // a moment — a candidate probe and a driver call — and doing it now means the
        // first frame is ready about when the peer is.
        (self.start)();

        let id = RemoteId::new(self.next_peer.fetch_add(1, Ordering::AcqRel));
        let port = self
            .ports
            .take()
            .ok_or_else(|| PipelineError::Remote("every remote port is in use".into()))?;

        match self.build_peer(id, port, offer_sdp).await {
            Ok(answer) => Ok(answer),
            Err(e) => {
                // A negotiation that failed must not keep its port, or a run of bad
                // offers exhausts the range and the next real peer is refused.
                self.ports.give_back(port);
                Err(e)
            }
        }
    }

    async fn build_peer(
        self: &Arc<Self>,
        id: RemoteId,
        port: u16,
        offer_sdp: &str,
    ) -> Result<String, PipelineError> {
        let offer = RTCSessionDescription::offer(offer_sdp.to_owned())
            .map_err(|e| PipelineError::Remote(format!("offer: {e}")))?;
        // Decided from the offer because nothing downstream can say it: see
        // [`Peer::wants_audio`].
        let wants_audio = offer
            .unmarshal()
            .map(|parsed| {
                parsed
                    .media_descriptions
                    .iter()
                    .any(|media| media.media_name.media == "audio")
            })
            .unwrap_or(false);

        let mut media_engine = MediaEngine::default();
        let codec = video_codec();
        media_engine
            .register_codec(codec.clone(), RtpCodecKind::Video)
            .map_err(|e| PipelineError::Remote(format!("codec: {e}")))?;
        let opus = audio_codec();
        media_engine
            .register_codec(opus.clone(), RtpCodecKind::Audio)
            .map_err(|e| PipelineError::Remote(format!("audio codec: {e}")))?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|e| PipelineError::Remote(format!("interceptors: {e}")))?;

        let gathered = Arc::new(tokio::sync::Notify::new());
        let handler = Arc::new(PeerHandler {
            id,
            service: Arc::clone(self),
            gathered: Arc::clone(&gathered),
        });

        // No ICE servers. This is a LAN receiver: a STUN round trip to a public server
        // would add latency to every connection and can only ever return an address that
        // is useless to a peer on the same network.
        let configuration = RTCConfigurationBuilder::new().build();
        let connection = PeerConnectionBuilder::new()
            .with_configuration(configuration)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_runtime(Arc::clone(&self.runtime))
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

        let ssrc = ssrc_for(id);
        let track = Arc::new(
            TrackLocalStaticSample::new(MediaStreamTrack::new(
                "castaway".to_owned(),
                "panel".to_owned(),
                "panel".to_owned(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: codec.rtp_codec.clone(),
                    ..Default::default()
                }],
            ))
            .map_err(|e| PipelineError::Remote(format!("track: {e}")))?,
        );
        let sender = connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|e| PipelineError::Remote(format!("add_track: {e}")))?;

        // The sound beside the pictures (#259): its own track, SSRC and sender, in the
        // same stream (`"castaway"`) so a browser's `ontrack` hands both to one
        // `MediaStream` and the `<video>` element carries them together.
        let audio_ssrc = audio_ssrc_for(id);
        let audio_track = Arc::new(
            TrackLocalStaticSample::new(MediaStreamTrack::new(
                "castaway".to_owned(),
                "panel-audio".to_owned(),
                "panel-audio".to_owned(),
                RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(audio_ssrc),
                        ..Default::default()
                    },
                    codec: opus.rtp_codec.clone(),
                    ..Default::default()
                }],
            ))
            .map_err(|e| PipelineError::Remote(format!("audio track: {e}")))?,
        );
        let audio_sender = connection
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|e| PipelineError::Remote(format!("add_track (audio): {e}")))?;

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

        Self::await_gathering(&gathered).await;
        let local = connection
            .local_description()
            .await
            .ok_or_else(|| PipelineError::Remote("no local description after gathering".into()))?;

        // Only now is the peer worth keeping: everything above could have failed, and a
        // half-built connection in the list would be one the pruner had to reason about.
        if let Ok(mut peers) = self.peers.lock() {
            peers.push(Peer {
                id,
                connection: Arc::clone(&connection),
                port,
                track,
                ssrc,
                sender,
                audio_track,
                audio_ssrc,
                audio_sender,
                wants_audio,
                pumping: false,
            });
        }
        info!(peer = id.get(), port, "remote: peer answered");

        // The pump does *not* start here. `write_sample` fails until the track is bound
        // and the transport is up, and the pump treats a write error as "this peer is
        // gone" — so starting it now would tear down every peer on its first frame,
        // before the connection it is waiting for had a chance to come up. It starts on
        // `Connected` instead.
        Ok(local.sdp)
    }

    /// Wait for non-trickle gathering, or give up and answer with what we have.
    ///
    /// The answer has to carry every candidate, so this is on the critical path of a peer
    /// connecting. On a LAN host candidates are gathered in milliseconds; the timeout is
    /// for when something is wrong, and answering with a partial candidate list beats
    /// failing the request outright.
    async fn await_gathering(gathered: &tokio::sync::Notify) {
        if tokio::time::timeout(GATHER_TIMEOUT, gathered.notified())
            .await
            .is_err()
        {
            warn!("remote: ICE gathering did not complete; answering with what we have");
        }
    }

    /// Start feeding a peer's track, once its transport is up.
    ///
    /// Idempotent: `Connected` can be reported more than once, and two pumps on one track
    /// would interleave two copies of every frame into a single sequence number space.
    fn start_pump(self: &Arc<Self>, id: RemoteId) {
        let started = match self.peers.lock() {
            Ok(mut peers) => peers
                .iter_mut()
                .find(|p| p.id == id)
                .filter(|p| !p.pumping)
                .map(|p| {
                    p.pumping = true;
                    (
                        Arc::clone(&p.track),
                        p.ssrc,
                        Arc::clone(&p.sender),
                        p.wants_audio.then(|| {
                            (
                                Arc::clone(&p.audio_track),
                                p.audio_ssrc,
                                Arc::clone(&p.audio_sender),
                            )
                        }),
                    )
                }),
            Err(_) => None,
        };
        let Some((track, ssrc, sender, audio)) = started else {
            return;
        };
        info!(peer = id.get(), "remote: streaming to peer");
        // Asked for again, not only at `answer`. The tap retires when nothing has wanted
        // it for ten seconds, and a peer that took longer than that to come up would
        // otherwise subscribe to a feed nothing is publishing to. Idempotent — the claim
        // is a compare-exchange — so this costs nothing in the normal case.
        (self.start)();
        self.spawn_pump(id, track, ssrc, sender);
        if let Some((audio_track, audio_ssrc, audio_sender)) = audio {
            self.spawn_audio_pump(id, audio_track, audio_ssrc, audio_sender);
        }
    }

    /// Feed one peer's track from the live encoder output until either goes away.
    fn spawn_pump(
        self: &Arc<Self>,
        id: RemoteId,
        track: Arc<TrackLocalStaticSample>,
        ssrc: u32,
        sender: Arc<dyn RtpSender>,
    ) {
        let mut subscription: Subscription = self.feed.subscribe();
        let service = Arc::clone(self);
        self.runtime.spawn(Box::pin(async move {
            // **Not the payload type we registered.** An answerer must use the *offerer's*
            // numbering, and a browser picks its own — Chromium's H.264 is rarely the 102
            // this end registers. Stamping our own number produced a stream every packet of
            // which the transceiver rejected as "unsupported codec type", at the full frame
            // rate, with the connection otherwise perfectly healthy: no video and a torrent
            // of log lines. The sender is the only thing that knows what was agreed.
            let payload_type = match negotiated_payload_type(&sender).await {
                Some(payload_type) => payload_type,
                None => {
                    warn!(
                        peer = id.get(),
                        "remote: peer negotiated no codec; nothing to send"
                    );
                    service.forget(id);
                    return;
                }
            };
            debug!(peer = id.get(), payload_type, "remote: sending video");
            while let Some(frame) = subscription.next().await {
                let sample = rtc::media::Sample {
                    data: bytes::Bytes::copy_from_slice(&frame.data),
                    duration: frame.duration,
                    ..Default::default()
                };
                if let Err(e) = track.write_sample(ssrc, payload_type, &sample, &[]).await {
                    debug!(peer = id.get(), error = %e, "remote: track closed");
                    break;
                }
            }
            // The subscription's guard detaches the feed here, which is what lets the tap
            // retire once the last peer has gone.
            drop(subscription);
            service.forget(id);
        }));
    }

    /// Feed one peer's audio track from the Opus fan-out until either goes away (#259).
    ///
    /// The sound's lifecycle rides the video's: this pump ending does *not* forget the
    /// peer — a remote with pictures and no sound is degraded, not gone — and the peer
    /// going away errors the write here, which is what ends the task. A peer whose offer
    /// carried no audio m-line negotiates no codec and simply never gets a pump.
    fn spawn_audio_pump(
        self: &Arc<Self>,
        id: RemoteId,
        track: Arc<TrackLocalStaticSample>,
        ssrc: u32,
        sender: Arc<dyn RtpSender>,
    ) {
        let mut subscription = self.feed.subscribe_audio();
        let service = Arc::clone(self);
        self.runtime.spawn(Box::pin(async move {
            // The offerer's number, exactly as the video pump asks — see `spawn_pump`
            // for what stamping our own registered 111 would produce.
            let payload_type = match negotiated_payload_type(&sender).await {
                Some(payload_type) => payload_type,
                None => {
                    debug!(
                        peer = id.get(),
                        "remote: peer negotiated no audio codec; the track stays silent"
                    );
                    return;
                }
            };
            debug!(peer = id.get(), payload_type, "remote: sending audio");
            loop {
                // The video pump learns of a dead peer from its writes, because frames
                // always flow while a peer is up. Sound does not have that property — a
                // build with no audio path publishes nothing, ever — so a pump parked on
                // an empty feed re-checks that its peer still exists rather than
                // outliving it by the life of the process.
                let frame =
                    tokio::time::timeout(std::time::Duration::from_secs(10), subscription.next())
                        .await;
                let frame = match frame {
                    Err(_) => {
                        if service.knows(id) {
                            continue;
                        }
                        break;
                    }
                    Ok(None) => break,
                    Ok(Some(frame)) => frame,
                };
                let sample = rtc::media::Sample {
                    data: bytes::Bytes::copy_from_slice(&frame.data),
                    duration: frame.duration,
                    ..Default::default()
                };
                if let Err(e) = track.write_sample(ssrc, payload_type, &sample, &[]).await {
                    debug!(peer = id.get(), error = %e, "remote: audio track closed");
                    break;
                }
            }
        }));
    }

    /// Whether a peer is still in the list — the audio pump's liveness re-check.
    fn knows(&self, id: RemoteId) -> bool {
        self.peers
            .lock()
            .is_ok_and(|peers| peers.iter().any(|p| p.id == id))
    }

    /// A peer is over: cancel whatever it was holding and let its port go.
    ///
    /// Idempotent, because it is reached from two directions — the connection state
    /// machine and the pump ending — and which arrives first is not ours to decide.
    fn forget(&self, id: RemoteId) {
        let removed = match self.peers.lock() {
            Ok(mut peers) => {
                let at = peers.iter().position(|p| p.id == id);
                at.map(|at| peers.remove(at))
            }
            Err(_) => None,
        };
        let Some(peer) = removed else {
            return;
        };
        self.ports.give_back(peer.port);
        // Cancelled, not released. A dropped connection did not *finish* a gesture, and
        // synthesising the release would commit whatever it was over — on the transport
        // strip, that means seeking to wherever the finger was when Wi-Fi died.
        self.input.push_gone(InputOrigin::Remote(id));
        info!(peer = id.get(), port = peer.port, "remote: peer gone");
        let connection = peer.connection;
        self.runtime.spawn(Box::pin(async move {
            let _ = connection.close().await;
        }));
    }

    /// Route one message from a peer's data channel.
    fn on_message(&self, id: RemoteId, text: &str) {
        match input_touch::wire::parse(id, text) {
            Ok(input_touch::RemoteCommand::Input(input)) => {
                if self.config.accept_input {
                    self.input.push_input(input);
                }
            }
            Ok(input_touch::RemoteCommand::Home) => {
                // Queued rather than called, so it keeps its place against the contacts
                // around it: home cancels whatever is down, and a press applied after it
                // would be stranded.
                if self.config.accept_input {
                    self.input.push_home();
                }
            }
            // Same gate as every other input: `remote.input = false` keeps the viewing
            // half and drops the driving half at this boundary (#260).
            Ok(input_touch::RemoteCommand::Key(key)) => {
                if self.config.accept_input {
                    self.input.push_key(key);
                }
            }
            Ok(input_touch::RemoteCommand::Text(text)) => {
                if self.config.accept_input {
                    self.input.push_text(text);
                }
            }
            // A keepalive says the peer is alive, which the connection already says. It
            // exists so a client behind something that reaps idle flows has a reason to
            // send anything at all.
            Ok(input_touch::RemoteCommand::Ping | input_touch::RemoteCommand::Unknown) => {}
            Err(e) => {
                debug!(peer = id.get(), error = %e, "remote: unusable message");
            }
        }
    }

    /// Close every peer. Called when the process is going away, so nothing is left
    /// holding a socket the next run wants.
    pub async fn shutdown(&self) {
        let peers: Vec<Peer> = match self.peers.lock() {
            Ok(mut peers) => peers.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        for peer in peers {
            self.ports.give_back(peer.port);
            let _ = peer.connection.close().await;
        }
    }
}

/// What this peer actually agreed to send under, which is the offerer's numbering and not
/// ours. `None` if the negotiation left the sender with no codec at all.
async fn negotiated_payload_type(sender: &Arc<dyn RtpSender>) -> Option<PayloadType> {
    sender
        .get_parameters()
        .await
        .ok()?
        .rtp_parameters
        .codecs
        .first()
        .map(|codec| codec.payload_type)
}

/// The codec the answer offers: H.264 constrained baseline, packetization mode 1.
fn video_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: H264_PAYLOAD_TYPE,
    }
}

/// The codec the answer offers for sound: Opus, stereo, at WebRTC's one true rate
/// (#259).
///
/// The fmtp line matches what every browser offers — 20 ms minimum ptime, in-band FEC
/// tolerated — so the codec matcher's strict (mime + fmtp) pass succeeds against a
/// Chromium offer instead of falling through to the mime-only pass.
fn audio_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: OPUS_PAYLOAD_TYPE,
    }
}

/// A stable, distinct SSRC per peer.
///
/// Derived from the peer counter rather than random so a capture is readable and two
/// concurrent peers cannot collide. Never zero, which some stacks treat as unset.
fn ssrc_for(id: RemoteId) -> u32 {
    let n = u32::try_from(id.get() & u64::from(u32::MAX)).unwrap_or(1);
    n.max(1)
}

/// The audio track's SSRC for the same peer: the video SSRC with the top bit set, so
/// the two streams of one connection cannot collide with each other *or* with any other
/// peer's, and a capture still reads as "peer n" at a glance (#259).
fn audio_ssrc_for(id: RemoteId) -> u32 {
    ssrc_for(id) | 0x8000_0000
}

/// One peer's events.
struct PeerHandler {
    id: RemoteId,
    service: Arc<RemoteService>,
    /// Fired once gathering completes, which is what lets the answer be sent.
    ///
    /// Signalled with `notify_one`, not `notify_waiters`: on a LAN, host candidates are
    /// gathered before the answer path gets as far as waiting, and `notify_waiters` wakes
    /// only tasks *already* parked — the notification would be dropped on the floor and
    /// every single connection would sit out the full timeout before answering.
    /// `notify_one` stores a permit, so arriving late costs nothing.
    gathered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for PeerHandler {
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        debug!(peer = self.id.get(), ?state, "remote: connection state");
        match state {
            // The transport is up, so the track is writable. Until this moment every
            // `write_sample` would fail.
            RTCPeerConnectionState::Connected => self.service.start_pump(self.id),
            // `Disconnected` can recover, but a stuck finger is worse than a cancelled
            // gesture, so the contacts go now and the connection is given its chance.
            RTCPeerConnectionState::Disconnected => {
                self.service.input.push_gone(InputOrigin::Remote(self.id));
            }
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                self.service.forget(self.id);
            }
            _ => {}
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.notify_one();
        }
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let id = self.id;
        let service = Arc::clone(&self.service);
        info!(peer = id.get(), "remote: input channel");
        // The channel is polled rather than handed a callback, so it needs a task. It
        // ends when the channel does, which is when the connection does.
        self.service.runtime.spawn(Box::pin(async move {
            while let Some(event) = channel.poll().await {
                match event {
                    DataChannelEvent::OnMessage(message) => {
                        match std::str::from_utf8(&message.data) {
                            Ok(text) => service.on_message(id, text),
                            // Binary is not part of the protocol. Ignored rather than
                            // fatal, like an unknown message type.
                            Err(_) => debug!(peer = id.get(), "remote: non-text message"),
                        }
                    }
                    DataChannelEvent::OnClose => break,
                    _ => {}
                }
            }
            debug!(peer = id.get(), "remote: input channel closed");
        }));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn every_peer_gets_a_distinct_nonzero_ssrc() {
        // Two peers sharing an SSRC would have their streams demuxed into one.
        let a = ssrc_for(RemoteId::new(1));
        let b = ssrc_for(RemoteId::new(2));
        assert_ne!(a, b);
        assert_ne!(a, 0);
        assert_ne!(ssrc_for(RemoteId::new(0)), 0, "zero reads as unset");
    }

    #[test]
    fn a_peers_audio_ssrc_collides_with_nobody() {
        // Not its own video stream, and not any neighbour's either stream (#259).
        let ssrcs = [
            ssrc_for(RemoteId::new(1)),
            audio_ssrc_for(RemoteId::new(1)),
            ssrc_for(RemoteId::new(2)),
            audio_ssrc_for(RemoteId::new(2)),
        ];
        for (i, a) in ssrcs.iter().enumerate() {
            assert_ne!(*a, 0, "zero reads as unset");
            for b in &ssrcs[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn the_codec_is_the_one_every_browser_accepts() {
        let codec = video_codec();
        assert_eq!(codec.payload_type, H264_PAYLOAD_TYPE);
        assert!(codec
            .rtp_codec
            .sdp_fmtp_line
            .contains("packetization-mode=1"));
        assert_eq!(codec.rtp_codec.clock_rate, 90_000);
    }

    #[test]
    fn the_audio_codec_is_opus_at_the_rate_webrtc_requires() {
        // 48 kHz stereo is the only Opus a browser will negotiate for a music-capable
        // track, and it is also the mixer's native rate — no resample on this path.
        let codec = audio_codec();
        assert_eq!(codec.payload_type, OPUS_PAYLOAD_TYPE);
        assert_eq!(codec.rtp_codec.mime_type, MIME_TYPE_OPUS);
        assert_eq!(codec.rtp_codec.clock_rate, 48_000);
        assert_eq!(codec.rtp_codec.channels, 2);
        assert!(codec.rtp_codec.sdp_fmtp_line.contains("minptime=10"));
    }
}
