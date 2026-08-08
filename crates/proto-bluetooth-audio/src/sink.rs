//! The A2DP sink session: a sans-I/O state machine over the AVDTP signaling channel.
//!
//! `fn(state, message) -> (state, outputs)`, per ground rule 3. The caller owns the
//! L2CAP channel and does nothing but hand messages in and write [`SinkEvent::Reply`]
//! out, which is what lets the whole discover → configure → open → start flow be tested
//! against a scripted phone with no radio present.

use bytes::{BufMut, Bytes, BytesMut};
use castaway_core::{AudioCodec, AudioFormat};

use crate::avdtp::StreamEndpoint;
use crate::avdtp::{
    category, error_code, find_codec_capability, lists_category, Message, MessageType, Seid,
    Signal, SinkDelay,
};
use crate::codec::CodecCapability;
use crate::error::AudioError;

/// What a sink reports as its own latency when nobody has measured one.
///
/// The output queue is 96 blocks of ~128 frames at 44.1 kHz (`QUEUE_BLOCKS`,
/// `pipeline::audio_out`) — 279 ms — plus decode and whatever the device holds below
/// that. 300 ms is that figure rounded up, and it is a *promise*: the number and the
/// buffer depth have to move together, exactly as AirPlay's `Audio-Latency` and its
/// buffer do. Override it with [`SinkSession::set_reported_delay`] once there is a
/// measurement to override it with.
///
/// Note what is *not* in it. The 250 ms `LEAD` the pull-based sources pace against is not
/// on this path: A2DP arrives already-encoded and the phone is the clock, so nothing
/// paces it (#89).
pub const DEFAULT_SINK_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Where one stream endpoint is in its lifecycle, as an observer sees it.
///
/// AVDTP's own state names, kept verbatim so the spec reads across. The transitions that
/// matter: a stream must be `Configured` before OPEN, and `Open` before START — a sender
/// that skips a step gets a typed reject rather than a stream that half-works.
///
/// This is a *projection* of the session's internal [`Stream`], which carries the
/// negotiated configuration inside each state (the `RaopState` pattern, #212) — a
/// payload-free copy exists only so callers and tests can compare phases without
/// constructing payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamState {
    /// Nothing configured.
    Idle,
    /// A configuration is set, but the media channel isn't open.
    Configured,
    /// The media channel is open; no audio flowing.
    Open,
    /// Audio is flowing.
    Streaming,
}

impl std::fmt::Display for StreamState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl StreamState {
    /// A stable lowercase name, for logs and error messages.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Configured => "configured",
            Self::Open => "open",
            Self::Streaming => "streaming",
        }
    }
}

/// What the session wants the caller to do, or tells it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SinkEvent {
    /// Write this message back on the signaling channel.
    Reply(Message),
    /// Send this command on the signaling channel — a transaction *we* are opening.
    ///
    /// Distinct from [`SinkEvent::Reply`] even though the caller writes both the same
    /// way, because the direction is the whole point: a reply closes a transaction the
    /// phone opened, and a command opens one it must answer. `DELAYREPORT` is the only
    /// one a sink originates, and there was no way to express it at all (#89).
    Command(Message),
    /// A configuration was accepted; the caller should prepare a decoder.
    Configured {
        /// The negotiated codec.
        codec: AudioCodec,
        /// The negotiated rate and channel count, which the decoder must be told: aptX
        /// and aptX HD carry no in-band configuration (#70).
        format: AudioFormat,
        /// The full negotiated capability, for anything the decoder needs.
        configuration: Box<CodecCapability>,
    },
    /// The media transport channel should be accepted now.
    Opened,
    /// Audio is about to flow. The caller starts the audio session here.
    Started,
    /// Audio paused but the configuration survives.
    Suspended,
    /// A `SUSPEND` *we* sent was refused, so the peer is still transmitting.
    ///
    /// Distinct from silence because it is the case with no remaining lever: the phone has
    /// been asked twice, in both the ways this receiver has, and is still holding the
    /// piconet. Nothing here can fix that, but the log should not be the last to know
    /// (#92).
    SuspendRefused,
    /// The stream is finished; release the decoder.
    Closed,
}

/// What `SET_CONFIGURATION` established, carried *inside* every configured state.
///
/// This travels with [`Stream`] rather than beside it — same reasoning as
/// `proto-airplay`'s `RaopState`: carrying the negotiated parameters inside the states
/// is what stops a later stage asking for a configuration that was never set (#212).
#[derive(Debug, Clone)]
struct StreamContext {
    /// Which of our endpoints the sender configured.
    active: Seid,
    /// Which of *the sender's* endpoints is on the other end of that stream.
    ///
    /// The INT SEID from `SET_CONFIGURATION`, which used to be skipped straight past.
    /// Every message this session sends addresses an endpoint, and a command travelling
    /// SNK→SRC addresses the source's — so without this there is nothing to put in a
    /// `SUSPEND` we originate, which is half of why we never sent one (#92). `Option`
    /// because a sender can put an unparseable INT SEID on the wire, and that costs it
    /// our `SUSPEND`, not the stream.
    peer_endpoint: Option<Seid>,
    configuration: CodecCapability,
    /// Whether the source asked for delay reporting on the configured stream.
    ///
    /// Set from the `SET_CONFIGURATION` payload: an initiator that lists the Delay
    /// Reporting category there is saying it will accept `DELAYREPORT`. A source that did
    /// not ask is not told, because a command it never negotiated is one it may reject.
    delay_reporting_configured: bool,
}

/// The stream's actual state, configuration inside rather than beside.
///
/// `Configured`-with-no-configuration was representable when this was a payload-free
/// state plus four parallel `Option` fields, and `on_get_configuration` patrolled it
/// with a BAD_STATE reject; now the impossible combinations do not construct, and
/// teardown is one assignment (#212).
#[derive(Debug)]
enum Stream {
    /// Nothing configured.
    Idle,
    /// A configuration is set, but the media channel isn't open.
    Configured(StreamContext),
    /// The media channel is open; no audio flowing.
    Open(StreamContext),
    /// Audio is flowing.
    Streaming {
        context: StreamContext,
        /// The label of a `SUSPEND` we have sent and not yet had answered.
        ///
        /// The stream stays `Streaming` while this is set. That is the point of keeping
        /// it: a command we send does not change the stream's state, the peer's answer
        /// does — and a session that moved on its own intent would be a state machine
        /// describing what we asked for rather than what is happening on the wire.
        suspending: Option<u8>,
    },
}

impl Stream {
    /// The negotiated context, in any configured state.
    const fn context(&self) -> Option<&StreamContext> {
        match self {
            Stream::Idle => None,
            Stream::Configured(context)
            | Stream::Open(context)
            | Stream::Streaming { context, .. } => Some(context),
        }
    }
}

/// One A2DP sink session over a single signaling channel.
#[derive(Debug)]
pub struct SinkSession {
    endpoints: Vec<StreamEndpoint>,
    stream: Stream,
    delay_reporting: bool,
    /// How long this sink holds audio before it is heard, as reported to the source.
    reported_delay: SinkDelay,
    /// The transaction label for the next command *we* send.
    ///
    /// Ours alone: AVDTP labels are chosen by whoever opens the transaction, and every
    /// other message this session emits is a response carrying the phone's label back.
    next_transaction: u8,
}

impl SinkSession {
    /// Build a session advertising `capabilities`, one endpoint per codec.
    ///
    /// SEIDs are assigned in table order, so the preference order in
    /// [`crate::codec::advertised`] is also the order a sender sees — which matters
    /// because senders generally take the first endpoint they also support.
    #[must_use]
    pub fn new(capabilities: Vec<CodecCapability>) -> Self {
        let endpoints = capabilities
            .into_iter()
            .enumerate()
            .filter_map(|(i, capability)| {
                // SEIDs start at 1; 0 is reserved. An endpoint that can't be numbered is
                // dropped rather than papered over with a wrong id.
                let seid = Seid::new(u8::try_from(i + 1).ok()?).ok()?;
                Some(StreamEndpoint {
                    seid,
                    capability,
                    in_use: false,
                })
            })
            .collect();
        Self {
            endpoints,
            stream: Stream::Idle,
            delay_reporting: true,
            reported_delay: SinkDelay::from_duration(DEFAULT_SINK_DELAY),
            next_transaction: 0,
        }
    }

    /// Set the latency this sink reports to a source (#89).
    ///
    /// A promise about the output path, so it and that path have to agree — the same
    /// contract as AirPlay's `Audio-Latency`. See [`DEFAULT_SINK_DELAY`] for where the
    /// default comes from.
    pub fn set_reported_delay(&mut self, delay: std::time::Duration) {
        self.reported_delay = SinkDelay::from_duration(delay);
    }

    /// The endpoints this session advertises.
    #[must_use]
    pub fn endpoints(&self) -> &[StreamEndpoint] {
        &self.endpoints
    }

    /// The current stream state (the payload-free projection of the internal state).
    #[must_use]
    pub const fn state(&self) -> StreamState {
        match &self.stream {
            Stream::Idle => StreamState::Idle,
            Stream::Configured(_) => StreamState::Configured,
            Stream::Open(_) => StreamState::Open,
            Stream::Streaming { .. } => StreamState::Streaming,
        }
    }

    /// The negotiated configuration, once one is set.
    #[must_use]
    pub const fn configuration(&self) -> Option<&CodecCapability> {
        match self.stream.context() {
            Some(context) => Some(&context.configuration),
            None => None,
        }
    }

    /// Handle one signaling message.
    ///
    /// Never returns `Err` for a peer-caused problem: a malformed or out-of-state
    /// command becomes a reject *reply*, because the sender is waiting on this
    /// transaction and dropping it presents as a hung link rather than a refusal.
    #[must_use]
    pub fn handle(&mut self, msg: &Message) -> Vec<SinkEvent> {
        if msg.message_type != MessageType::Command {
            // Almost a pure responder: the two commands this session originates are
            // `DELAYREPORT`, whose answer carries no information, and `SUSPEND`, whose
            // answer is what actually stops the stream. Anything else is a response to a
            // command we never sent, which is noise rather than an error.
            return self.on_response(msg);
        }
        match msg.signal {
            Signal::Discover => self.on_discover(msg),
            Signal::GetCapabilities => self.on_get_capabilities(msg, false),
            Signal::GetAllCapabilities => self.on_get_capabilities(msg, true),
            Signal::SetConfiguration => self.on_set_configuration(msg),
            Signal::GetConfiguration => self.on_get_configuration(msg),
            Signal::Open => self.on_open(msg),
            Signal::Start => self.on_start(msg),
            Signal::Suspend => self.on_suspend(msg),
            Signal::Close => self.on_close(msg),
            Signal::Abort => self.on_abort(msg),
            Signal::Reconfigure => self.on_reconfigure(msg),
            // SecurityControl and DelayReport are answered but do nothing: accepting is
            // correct for a sink with no content protection, DELAYREPORT travels SNK→SRC
            // so an inbound one should not happen, and a general reject makes some
            // senders retry.
            Signal::SecurityControl | Signal::DelayReport => {
                vec![SinkEvent::Reply(Message::accept(msg, Bytes::new()))]
            }
        }
    }

    fn on_discover(&self, msg: &Message) -> Vec<SinkEvent> {
        let mut body = BytesMut::with_capacity(self.endpoints.len() * 2);
        for sep in &self.endpoints {
            body.put_slice(&sep.discover_bytes());
        }
        vec![SinkEvent::Reply(Message::accept(msg, body.freeze()))]
    }

    fn on_get_capabilities(&self, msg: &Message, all: bool) -> Vec<SinkEvent> {
        let Some(seid) = msg
            .payload
            .first()
            .and_then(|b| Seid::from_shifted(*b).ok())
        else {
            return reject(msg, error_code::BAD_ACP_SEID);
        };
        let Some(sep) = self.endpoints.iter().find(|s| s.seid == seid) else {
            return reject(msg, error_code::BAD_ACP_SEID);
        };
        // Delay reporting is an AVDTP 1.3 capability, so it belongs only in the
        // GET_ALL_CAPABILITIES answer. Returning it from the 1.0 command confuses
        // senders that asked precisely because they don't speak 1.3.
        let include_delay = all && self.delay_reporting;
        vec![SinkEvent::Reply(Message::accept(
            msg,
            sep.capability_bytes(include_delay),
        ))]
    }

    fn on_set_configuration(&mut self, msg: &Message) -> Vec<SinkEvent> {
        if msg.payload.len() < 2 {
            return reject_config(msg, 0, error_code::BAD_ACP_SEID);
        }
        let Ok(seid) = Seid::from_shifted(msg.payload[0]) else {
            return reject_config(msg, 0, error_code::BAD_ACP_SEID);
        };
        let Some(index) = self.endpoints.iter().position(|s| s.seid == seid) else {
            return reject_config(msg, 0, error_code::BAD_ACP_SEID);
        };
        if !matches!(self.stream, Stream::Idle) {
            return reject_config(msg, 0, error_code::SEP_IN_USE);
        }

        let capability = match find_codec_capability(&msg.payload[2..]) {
            Ok(cap) => cap,
            // Media Codec is service category 0x07, and a SET_CONFIGURATION reject names
            // the *failing category* before the error code — a different payload shape
            // from every other reject, and one senders do read.
            Err(_) => return reject_config(msg, 0x07, error_code::UNSUPPORTED_CONFIGURATION),
        };

        // The sender must have narrowed every field. A configuration that still names a
        // set is ambiguous, and guessing a rate plays the stream at the wrong pitch
        // instead of failing.
        if !capability.is_configuration() {
            return reject_config(msg, 0x07, error_code::INVALID_CODEC_PARAMETER);
        }
        // …and it must be the codec this endpoint actually advertised. A sender that
        // configures SBC parameters onto our LDAC endpoint would otherwise be accepted
        // and then decoded with the wrong decoder.
        if capability.audio_codec() != self.endpoints[index].capability.audio_codec() {
            return reject_config(msg, 0x07, error_code::UNSUPPORTED_CONFIGURATION);
        }
        // Both halves of the format, not just the rate: a configuration we cannot
        // resolve to one rate *and* one channel count is one we cannot open a decoder
        // for, and refusing it here is the last point at which the sender can be told.
        let Some(format) = capability.format() else {
            return reject_config(msg, 0x07, error_code::INVALID_CODEC_PARAMETER);
        };

        self.endpoints[index].in_use = true;
        self.stream = Stream::Configured(StreamContext {
            active: seid,
            // The second byte is the INT SEID: the sender's own endpoint for this
            // stream. It is not needed to *answer* anything — every response carries the
            // ACP SEID back — which is why it went unread for so long. It is needed to
            // *ask*: see `suspend_peer`.
            peer_endpoint: Seid::from_shifted(msg.payload[1]).ok(),
            configuration: capability.clone(),
            // The initiator names the Delay Reporting category here when it wants
            // `DELAYREPORT` on this stream. That is the protocol's own answer to "may we
            // send one", and it is why this is read from the payload rather than assumed
            // from our having advertised the capability.
            delay_reporting_configured: self.delay_reporting
                && lists_category(&msg.payload[2..], category::DELAY_REPORTING),
        });
        let mut events = vec![
            SinkEvent::Reply(Message::accept(msg, Bytes::new())),
            SinkEvent::Configured {
                codec: capability.audio_codec(),
                format,
                configuration: Box::new(capability),
            },
        ];
        events.extend(self.delay_report());
        events
    }

    /// Tell the source how long we hold audio, if this stream negotiated it.
    ///
    /// **The advertisement without this was a promise we never kept.** The capability is
    /// in GET_ALL_CAPABILITIES and the SDP record claims A2DP 1.3, but no `DELAYREPORT`
    /// ever left the box, so every phone assumed a sink latency of zero and every video
    /// watched through this speaker was out of lip-sync by the whole depth of the output
    /// path — silently, since the pairing works and the stream plays (#89).
    ///
    /// Sent after the `SET_CONFIGURATION` response, which is where the ordering matters:
    /// the source is still in the middle of that transaction until it sees the accept.
    fn delay_report(&mut self) -> Option<SinkEvent> {
        let context = self.stream.context()?;
        if !context.delay_reporting_configured {
            return None;
        }
        let seid = context.active;
        let transaction = self.next_transaction;
        // Four bits on the wire, so it wraps at 16 rather than at 256.
        self.next_transaction = (self.next_transaction + 1) % 16;
        Some(SinkEvent::Command(Message::delay_report(
            transaction,
            seid,
            self.reported_delay,
        )))
    }

    /// Ask the peer to stop sending audio.
    ///
    /// The initiator half of the session, and the only one. Everything else here is driven
    /// by a decoded peer command, which is what made preemption advisory: a phone that
    /// lost the panel got an AVRCP `PAUSE` keypress and nothing else, so a phone with no
    /// AVCTP channel — or one that ignores the key — went on transmitting into a session
    /// that had already been torn down, holding its share of the piconet against the phone
    /// that actually won (#92).
    ///
    /// `SUSPEND` rather than `CLOSE`: the configuration survives, so the phone can resume
    /// with a `START` on the same stream rather than renegotiating from `DISCOVER`.
    ///
    /// Returns `None` — nothing to send — when there is no stream to suspend, when the
    /// peer never told us its endpoint, or when a `SUSPEND` is already outstanding.
    #[must_use]
    pub fn suspend_peer(&mut self) -> Option<SinkEvent> {
        let Stream::Streaming {
            context,
            suspending: suspending @ None,
        } = &mut self.stream
        else {
            return None;
        };
        let seid = context.peer_endpoint?;
        let transaction = self.next_transaction;
        // Four bits on the wire, so it wraps at 16 rather than at 256.
        self.next_transaction = (self.next_transaction + 1) % 16;
        *suspending = Some(transaction);
        Some(SinkEvent::Command(Message::suspend(transaction, seid)))
    }

    /// Whether we are waiting on a peer to answer a `SUSPEND` we sent.
    #[must_use]
    pub const fn suspend_outstanding(&self) -> bool {
        matches!(
            self.stream,
            Stream::Streaming {
                suspending: Some(_),
                ..
            }
        )
    }

    /// A response to a command this session originated.
    ///
    /// An outstanding `SUSPEND` label only exists while streaming — carried inside
    /// [`Stream::Streaming`], so an answer arriving after the stream already left that
    /// state (an inbound SUSPEND won the race, a CLOSE tore it down) finds nothing
    /// outstanding and changes nothing, which is the same outcome the old parallel
    /// field reached by hand.
    fn on_response(&mut self, msg: &Message) -> Vec<SinkEvent> {
        let Stream::Streaming {
            suspending: suspending @ Some(_),
            ..
        } = &mut self.stream
        else {
            return Vec::new();
        };
        if msg.signal != Signal::Suspend || *suspending != Some(msg.transaction) {
            return Vec::new();
        }
        *suspending = None;
        match msg.message_type {
            // The peer has stopped. Now — and only now — the stream is `Open` again, which
            // is exactly the transition an inbound SUSPEND would have made.
            MessageType::ResponseAccept => {
                match std::mem::replace(&mut self.stream, Stream::Idle) {
                    Stream::Streaming { context, .. } => {
                        self.stream = Stream::Open(context);
                        vec![SinkEvent::Suspended]
                    }
                    // Unreachable — the outer `let` matched `Streaming` — but restoring
                    // is free and truthful, and this stays panic-free (ground rule 7).
                    other => {
                        self.stream = other;
                        Vec::new()
                    }
                }
            }
            // Refused, or the signal is not implemented. The stream is still streaming, on
            // the wire and here, and saying otherwise is how the two ends come to disagree.
            // Nothing about our state changes; the caller is told so it can say so.
            _ => vec![SinkEvent::SuspendRefused],
        }
    }

    /// RECONFIGURE: the sender is changing the codec block mid-session.
    ///
    /// This used to be lumped in with SecurityControl and DelayReport and answered with a
    /// bare ACCEPT, on the reasoning that a sink "has no reconfigurable parameters". That
    /// is not true — the codec block is exactly what RECONFIGURE carries, and AOSP sends
    /// one when the rate or bitpool changes from Developer Options, as do some stacks on
    /// stream restart. The sender switched its encoder and we kept decoding at the old
    /// rate: wrong pitch, or noise, with nothing logged. The same failure class as #70,
    /// through a door #70 did not close.
    ///
    /// Validated exactly as SET_CONFIGURATION is, because it is the same decision being
    /// made a second time — and answered with the same category-first reject shape, which
    /// is what senders read.
    fn on_reconfigure(&mut self, msg: &Message) -> Vec<SinkEvent> {
        // Only legal in OPEN. In STREAMING the sender must SUSPEND first, and accepting
        // it there would swap the decoder out from under audio that is still arriving.
        let Stream::Open(context) = &self.stream else {
            return reject_config(msg, 0, error_code::BAD_STATE);
        };
        if msg.payload.is_empty() {
            return reject_config(msg, 0, error_code::BAD_ACP_SEID);
        }
        let Ok(seid) = Seid::from_shifted(msg.payload[0]) else {
            return reject_config(msg, 0, error_code::BAD_ACP_SEID);
        };
        if context.active != seid {
            return reject_config(msg, 0, error_code::BAD_ACP_SEID);
        }
        let Some(index) = self.endpoints.iter().position(|s| s.seid == seid) else {
            return reject_config(msg, 0, error_code::BAD_ACP_SEID);
        };

        let capability = match find_codec_capability(&msg.payload[1..]) {
            Ok(cap) => cap,
            Err(_) => return reject_config(msg, 0x07, error_code::UNSUPPORTED_CONFIGURATION),
        };
        if !capability.is_configuration() {
            return reject_config(msg, 0x07, error_code::INVALID_CODEC_PARAMETER);
        }
        // RECONFIGURE may change the codec's *parameters*, never the codec itself — that
        // needs a CLOSE and a new SET_CONFIGURATION. Accepting a codec switch here would
        // leave the endpoint describing one thing and the decoder doing another.
        if capability.audio_codec() != self.endpoints[index].capability.audio_codec() {
            return reject_config(msg, 0x07, error_code::UNSUPPORTED_CONFIGURATION);
        }
        let Some(format) = capability.format() else {
            return reject_config(msg, 0x07, error_code::INVALID_CODEC_PARAMETER);
        };

        if let Stream::Open(context) = &mut self.stream {
            context.configuration = capability.clone();
        }
        vec![
            SinkEvent::Reply(Message::accept(msg, Bytes::new())),
            // Same event as the first configuration: the caller's job is identical —
            // rebuild the decoder for these parameters — and giving it a second name
            // would just be a second path to keep correct.
            SinkEvent::Configured {
                codec: capability.audio_codec(),
                format,
                configuration: Box::new(capability),
            },
        ]
    }

    fn on_get_configuration(&self, msg: &Message) -> Vec<SinkEvent> {
        // Any configured state has a configuration to report — by construction, not by
        // patrol: the old parallel-`Option` shape had to reject `Configured`-with-no-
        // configuration here as a BAD_STATE that "should not happen" (#212).
        let Some(config) = self.stream.context().map(|c| &c.configuration) else {
            return reject(msg, error_code::BAD_STATE);
        };
        let mut body = BytesMut::with_capacity(16);
        let codec = config.encode();
        body.put_u8(0x07); // media codec category
        body.put_u8(u8::try_from(codec.len()).unwrap_or(u8::MAX));
        body.extend_from_slice(&codec);
        vec![SinkEvent::Reply(Message::accept(msg, body.freeze()))]
    }

    fn on_open(&mut self, msg: &Message) -> Vec<SinkEvent> {
        match std::mem::replace(&mut self.stream, Stream::Idle) {
            Stream::Configured(context) => {
                self.stream = Stream::Open(context);
                vec![
                    SinkEvent::Reply(Message::accept(msg, Bytes::new())),
                    SinkEvent::Opened,
                ]
            }
            other => {
                self.stream = other;
                reject(msg, error_code::BAD_STATE)
            }
        }
    }

    fn on_start(&mut self, msg: &Message) -> Vec<SinkEvent> {
        match std::mem::replace(&mut self.stream, Stream::Idle) {
            Stream::Open(context) => {
                self.stream = Stream::Streaming {
                    context,
                    suspending: None,
                };
                vec![
                    SinkEvent::Reply(Message::accept(msg, Bytes::new())),
                    SinkEvent::Started,
                ]
            }
            other => {
                let active = other.context().map(|c| c.active);
                self.stream = other;
                // START's reject payload names the offending SEID before the error code,
                // because one START may list several streams.
                reject_stream(msg, active, error_code::BAD_STATE)
            }
        }
    }

    fn on_suspend(&mut self, msg: &Message) -> Vec<SinkEvent> {
        match std::mem::replace(&mut self.stream, Stream::Idle) {
            // An outstanding SUSPEND of our own is dropped with the transition: its
            // answer will find the stream already suspended, which is the outcome we
            // were asking for.
            Stream::Streaming { context, .. } => {
                self.stream = Stream::Open(context);
                vec![
                    SinkEvent::Reply(Message::accept(msg, Bytes::new())),
                    SinkEvent::Suspended,
                ]
            }
            other => {
                let active = other.context().map(|c| c.active);
                self.stream = other;
                reject_stream(msg, active, error_code::BAD_STATE)
            }
        }
    }

    fn on_close(&mut self, msg: &Message) -> Vec<SinkEvent> {
        if matches!(self.stream, Stream::Idle) {
            return reject(msg, error_code::BAD_STATE);
        }
        self.teardown();
        vec![
            SinkEvent::Reply(Message::accept(msg, Bytes::new())),
            SinkEvent::Closed,
        ]
    }

    fn on_abort(&mut self, msg: &Message) -> Vec<SinkEvent> {
        // ABORT is never rejected: it exists precisely for the case where the two ends
        // disagree about state, so refusing it would strand the disagreement forever.
        let was_active = !matches!(self.stream, Stream::Idle);
        self.teardown();
        let mut out = vec![SinkEvent::Reply(Message::accept(msg, Bytes::new()))];
        if was_active {
            out.push(SinkEvent::Closed);
        }
        out
    }

    /// Drop back to idle and release the endpoint.
    ///
    /// One assignment, plus the endpoint bookkeeping — no longer four parallel fields
    /// to remember (#212).
    fn teardown(&mut self) {
        if let Some(context) = self.stream.context() {
            let seid = context.active;
            if let Some(sep) = self.endpoints.iter_mut().find(|s| s.seid == seid) {
                sep.in_use = false;
            }
        }
        self.stream = Stream::Idle;
    }

    /// The link dropped without a teardown handshake.
    ///
    /// # Errors
    /// Never; returns a `Result` for symmetry with the fallible paths callers drive.
    pub fn link_down(&mut self) -> Result<Vec<SinkEvent>, AudioError> {
        if matches!(self.stream, Stream::Idle) {
            return Ok(Vec::new());
        }
        self.teardown();
        Ok(vec![SinkEvent::Closed])
    }
}

fn reject(msg: &Message, code: u8) -> Vec<SinkEvent> {
    vec![SinkEvent::Reply(Message::reject(msg, code))]
}

/// SET_CONFIGURATION / RECONFIGURE reject: failing category, then error code.
fn reject_config(msg: &Message, category: u8, code: u8) -> Vec<SinkEvent> {
    vec![SinkEvent::Reply(Message {
        transaction: msg.transaction,
        message_type: MessageType::ResponseReject,
        signal: msg.signal,
        payload: Bytes::copy_from_slice(&[category, code]),
    })]
}

/// START / SUSPEND reject: offending SEID, then error code.
fn reject_stream(msg: &Message, seid: Option<Seid>, code: u8) -> Vec<SinkEvent> {
    let acp = seid.map_or(0, Seid::shifted);
    vec![SinkEvent::Reply(Message {
        transaction: msg.transaction,
        message_type: MessageType::ResponseReject,
        signal: msg.signal,
        payload: Bytes::copy_from_slice(&[acp, code]),
    })]
}

/// A rejected message's error code, wherever it sits in that signal's payload shape.
#[must_use]
pub fn reject_code(msg: &Message) -> Option<u8> {
    if msg.message_type != MessageType::ResponseReject {
        return None;
    }
    match msg.signal {
        // These two put a category or SEID first, so the code is the second byte.
        Signal::SetConfiguration | Signal::Reconfigure | Signal::Start | Signal::Suspend => {
            msg.payload.get(1).copied()
        }
        _ => msg.payload.first().copied(),
    }
}
