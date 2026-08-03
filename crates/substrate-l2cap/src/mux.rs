//! The channel multiplexer: a sans-I/O state machine for one ACL link.
//!
//! `fn(state, input) -> (state, outputs)` exactly as ground rule 3 asks. Nothing here
//! touches a socket; the caller feeds it reassembled PDUs and writes out whatever
//! [`L2capEvent::Send`] it produces. That is what makes the whole connect → configure →
//! stream → disconnect flow testable with no radio.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use tracing::debug;

use crate::error::L2capError;
use crate::ertm::{
    ChannelMode, Ertm, ErtmParameters, FcsType, RetransmissionConfig, DEFAULT_MAX_TRANSMIT,
    DEFAULT_MONITOR_TIMEOUT, DEFAULT_RETRANSMISSION_TIMEOUT, DEFAULT_TX_WINDOW, MAX_OVERHEAD,
};
use crate::pdu::{Cid, L2capPdu, Psm};
use crate::signaling::{ConfigOption, ConfigResult, ConnectionResult, Signal};

/// Default MTU for basic mode when the peer proposes nothing.
pub const DEFAULT_MTU: u16 = 672;

/// The SDU ceiling an Enhanced Retransmission channel advertises.
///
/// Deliberately far larger than the basic-mode MTU, and the reason segmentation is worth
/// having: in basic mode the MTU has to fit one ACL packet, because there is nothing to
/// split an SDU with. ERTM has, so the *frame* size stays ACL-sized (see
/// [`Multiplexer::receive_mps`]) while the object it carries need not — which is what
/// lets a whole cover-art thumbnail arrive as one SDU instead of the layer above having
/// to chunk it.
pub const ERTM_MTU: u16 = 8192;

/// How many times we will re-propose a configuration the peer called unacceptable.
///
/// Bounded because the exchange is a negotiation, not a conversation: two ends that keep
/// counter-proposing are not converging, and a channel that never opens should fail
/// visibly rather than trade signalling forever.
const MAX_CONFIG_ATTEMPTS: u8 = 4;

/// Where a channel is in its lifecycle.
///
/// Configuration is genuinely two-sided — each end configures the direction it receives
/// on — so `WaitConfig` tracks both halves rather than one flag. Opening the channel when
/// only one direction is configured is the bug that produces "connected, then the first
/// data PDU is rejected".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChannelState {
    /// We sent a connection request and are waiting for the response.
    WaitConnectRsp,
    /// Connected, configuring.
    WaitConfig {
        /// Our configuration request has been accepted by the peer.
        outgoing_done: bool,
        /// We have accepted the peer's configuration request.
        incoming_done: bool,
    },
    /// Open for data in both directions.
    Open,
    /// We asked to disconnect and are waiting for the acknowledgement.
    WaitDisconnect,
}

impl ChannelState {
    const fn label(self) -> &'static str {
        match self {
            Self::WaitConnectRsp => "waiting for connection response",
            Self::WaitConfig { .. } => "configuring",
            Self::Open => "open",
            Self::WaitDisconnect => "disconnecting",
        }
    }
}

/// The retransmission parameters a channel has agreed so far.
///
/// Each field says which side it came from, because that is the whole difficulty: the
/// same nine-byte option means different things in a request and in a response, and
/// reading one as the other yields a channel that negotiates cleanly and then transfers
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeParameters {
    /// Whether frames carry a checksum. Agreed, not per-direction.
    pub fcs: FcsType,
    /// Largest segment we may put in one frame — the peer's *request* said what it can
    /// receive, and that is our ceiling.
    pub send_mps: u16,
    /// Frames we may have unacknowledged — ours to propose, theirs to reduce in the
    /// *response*.
    pub send_window: u8,
    /// Total transmissions of one frame before we give up on the channel (1 = no
    /// retransmission), per the Core spec's `MaxTransmit`. Ours.
    pub max_transmit: u8,
    /// How long an unacknowledged frame waits before we poll. The responder's choice, so
    /// this comes out of their *response* to our request.
    pub retransmission_timeout: Duration,
    /// How long a poll waits before being repeated. Also theirs.
    pub monitor_timeout: Duration,
}

impl Default for ModeParameters {
    fn default() -> Self {
        Self {
            fcs: FcsType::Crc16,
            send_mps: DEFAULT_MTU,
            send_window: DEFAULT_TX_WINDOW,
            max_transmit: DEFAULT_MAX_TRANSMIT,
            retransmission_timeout: DEFAULT_RETRANSMISSION_TIMEOUT,
            monitor_timeout: DEFAULT_MONITOR_TIMEOUT,
        }
    }
}

/// One L2CAP channel.
///
/// Both CIDs are named. Each side allocates its own identifier for the same channel, and
/// using the wrong one addresses a different channel entirely (or none) — so there is no
/// field called just `cid`.
#[derive(Debug, Clone)]
pub struct Channel {
    /// The identifier *we* allocated; PDUs arriving for this channel carry it.
    pub local_cid: Cid,
    /// The identifier the *peer* allocated; PDUs we send carry it.
    pub remote_cid: Cid,
    /// Which service this channel serves.
    pub psm: Psm,
    /// Lifecycle position.
    pub state: ChannelState,
    /// Largest SDU we are willing to receive.
    pub local_mtu: u16,
    /// Largest SDU the peer is willing to receive — our send ceiling.
    pub remote_mtu: u16,
    /// Basic, or Enhanced Retransmission.
    pub mode: ChannelMode,
    /// What ERTM negotiated. Meaningless in basic mode.
    pub parameters: ModeParameters,
    /// The identifier of the configuration request we currently have outstanding.
    ///
    /// Responses that name a different one are stale: they answer a proposal we have
    /// already replaced, and acting on them is how a channel ends up believing it agreed
    /// to a mode it withdrew.
    config_id: Option<u8>,
    /// The mode that outstanding request proposed. Diverges from `mode` exactly when we
    /// have adopted the peer's mode from its own request and not yet re-proposed.
    proposed_mode: ChannelMode,
    /// Whether *we* opened this channel.
    ///
    /// The asymmetry that makes mode negotiation converge. Both ends propose at once, so
    /// if both are willing to move to whatever the other asked for they swap modes
    /// forever and the channel never opens. The rule is therefore: the side that listened
    /// holds the mode its service was registered with, and the side that dialled adapts.
    initiator: bool,
    /// Configuration requests we have sent, so a negotiation that will not converge fails
    /// rather than loops.
    config_attempts: u8,
}

/// What the multiplexer wants the caller to do, or tells it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum L2capEvent {
    /// Write this PDU to the link.
    Send(L2capPdu),
    /// A channel finished configuring and is ready for data.
    ChannelOpen {
        /// Our identifier for it.
        cid: Cid,
        /// Which service.
        psm: Psm,
        /// The peer's receive MTU — do not send SDUs larger than this.
        peer_mtu: u16,
    },
    /// A channel went away.
    ChannelClosed {
        /// Our identifier for it.
        cid: Cid,
        /// Which service.
        psm: Psm,
    },
    /// Data arrived on an open channel.
    Data {
        /// Our identifier for it.
        cid: Cid,
        /// Which service.
        psm: Psm,
        /// The payload.
        payload: Bytes,
    },
    /// A connection request we made was refused.
    ConnectFailed {
        /// The service we asked for.
        psm: Psm,
        /// Why.
        result: ConnectionResult,
    },
}

/// Multiplexes L2CAP channels over one ACL link.
#[derive(Debug)]
pub struct Multiplexer {
    channels: HashMap<u16, Channel>,
    /// Retransmission engines, keyed by local CID. Kept beside the channel table rather
    /// than inside it so [`Channel`] stays a plain, copyable description of the channel
    /// rather than a live state machine callers could accidentally clone.
    ertm: HashMap<u16, Ertm>,
    listening: Vec<(Psm, ChannelMode)>,
    next_cid: u16,
    next_id: u8,
    /// Local CID awaiting a response, keyed by the signaling id we used.
    pending: HashMap<u8, u16>,
    /// Requests we have sent and not yet had answered, with their response timers.
    outstanding: HashMap<u8, Outstanding>,
    local_mtu: u16,
}

/// A signalling request awaiting its response.
///
/// Nothing timed these before, so a `ConnectionRequest` or `ConfigurationRequest` the peer
/// simply never answered left the channel in `WaitConnectRsp`/`WaitConfig` forever: no
/// retransmission, no `ChannelClosed`, the CID never freed, the caller never told. That is
/// a hang rather than a failure, and it bit the channels *we* dial — a phone that ignores
/// our SDP or AVCTP connect left cover art and the outbound AVRCP channel permanently
/// dead for that link, because the code that opens them waits on a flag that never clears.
#[derive(Debug)]
struct Outstanding {
    /// The local channel this request belongs to, so a give-up can tear it down.
    raw_cid: u16,
    /// The request itself, kept so it can be sent again — the spec wants at least one
    /// retransmission before the channel is abandoned, and a lost packet is much more
    /// likely than a peer that will never answer.
    request: Signal,
    /// Time left on the current attempt.
    remaining: Duration,
    /// Retransmissions used so far.
    retries: u8,
}

/// Bit 0 of a configuration request's flags: "more options follow in another request".
///
/// Uncommon but legal, and the failure it produces when ignored is the confusing kind —
/// a channel both ends believe is configured, differently.
const CONTINUATION_FLAG: u16 = 0x0001;

/// The response timeout (RTX). The spec allows 1–60 seconds.
///
/// Toward the short end: everything we send a request for is on a local radio link with a
/// device in the same room, and the cost of being wrong is one retransmission.
const RTX: Duration = Duration::from_secs(4);

/// How many times to resend before concluding the peer is not going to answer.
const RTX_RETRIES: u8 = 2;

impl Default for Multiplexer {
    fn default() -> Self {
        Self::new(DEFAULT_MTU)
    }
}

impl Multiplexer {
    /// A multiplexer with no channels, advertising `local_mtu` as our receive size.
    #[must_use]
    pub fn new(local_mtu: u16) -> Self {
        Self {
            channels: HashMap::new(),
            ertm: HashMap::new(),
            listening: Vec::new(),
            next_cid: Cid::DYNAMIC_START,
            next_id: 1,
            pending: HashMap::new(),
            outstanding: HashMap::new(),
            local_mtu,
        }
    }

    /// Accept incoming connections to `psm` in basic mode. Anything else is refused with
    /// [`ConnectionResult::PsmNotSupported`].
    pub fn listen(&mut self, psm: Psm) {
        self.listen_with(psm, ChannelMode::Basic);
    }

    /// Accept incoming connections to `psm`, running them in `mode`.
    ///
    /// The mode is a property of the *service*, decided before either end has proposed
    /// anything, because both directions of a channel must run in the same one and the
    /// two configuration exchanges race. Deciding up front makes the outcome deterministic
    /// instead of dependent on which of the peer's two signalling commands we see first.
    pub fn listen_with(&mut self, psm: Psm, mode: ChannelMode) {
        if let Some(entry) = self.listening.iter_mut().find(|(p, _)| *p == psm) {
            entry.1 = mode;
        } else {
            self.listening.push((psm, mode));
        }
    }

    /// An open channel by local CID.
    #[must_use]
    pub fn channel(&self, cid: Cid) -> Option<&Channel> {
        self.channels.get(&cid.raw())
    }

    /// Every channel currently tracked.
    pub fn channels(&self) -> impl Iterator<Item = &Channel> {
        self.channels.values()
    }

    /// The first open channel serving `psm`, if any.
    #[must_use]
    pub fn channel_for(&self, psm: Psm) -> Option<&Channel> {
        self.channels
            .values()
            .find(|c| c.psm == psm && c.state == ChannelState::Open)
    }

    fn alloc_cid(&mut self) -> Result<Cid, L2capError> {
        for _ in 0..(u32::from(u16::MAX - Cid::DYNAMIC_START)) {
            let candidate = self.next_cid;
            self.next_cid = self.next_cid.checked_add(1).unwrap_or(Cid::DYNAMIC_START);
            if !self.channels.contains_key(&candidate) {
                return Ok(Cid::new(candidate));
            }
        }
        Err(L2capError::OutOfCids)
    }

    fn alloc_id(&mut self) -> u8 {
        let id = self.next_id;
        // Signaling ids wrap, and zero is reserved as "invalid".
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        id
    }

    fn signal(sig: &Signal) -> Result<L2capEvent, L2capError> {
        Ok(L2capEvent::Send(L2capPdu::new(
            Cid::SIGNALING,
            sig.encode()?,
        )))
    }

    /// The SDU ceiling to advertise for a channel in `mode`.
    fn mode_mtu(&self, mode: ChannelMode) -> u16 {
        ceiling_for(mode, self.local_mtu)
    }

    /// The largest frame payload we can receive: an SDU that still lands in one ACL
    /// packet, less what ERTM spends on framing it. Advertising the MTU itself would put
    /// every full-size frame six bytes over the buffer it has to land in.
    fn receive_mps(&self) -> u16 {
        let overhead = u16::try_from(MAX_OVERHEAD).unwrap_or(u16::MAX);
        self.local_mtu.saturating_sub(overhead).max(1)
    }

    /// Open a channel to `psm` on the peer in basic mode.
    ///
    /// # Errors
    /// [`L2capError::OutOfCids`] if no identifier is free.
    pub fn connect(&mut self, psm: Psm) -> Result<(Cid, Vec<L2capEvent>), L2capError> {
        self.connect_with(psm, ChannelMode::Basic)
    }

    /// Open a channel to `psm` on the peer, running it in `mode` — used for the AVRCP
    /// cover-art fetch, where *we* are the one connecting out and GOEP 2.0 requires
    /// Enhanced Retransmission Mode (#74).
    ///
    /// Returns the identifier we allocated along with the request to send. The caller
    /// needs it: an outgoing channel is only recognisable later by the id it was given,
    /// and a client that cannot tell its own channel from an inbound one routes the
    /// peer's *response* into its own server.
    ///
    /// # Errors
    /// [`L2capError::OutOfCids`] if no identifier is free.
    pub fn connect_with(
        &mut self,
        psm: Psm,
        mode: ChannelMode,
    ) -> Result<(Cid, Vec<L2capEvent>), L2capError> {
        let local_cid = self.alloc_cid()?;
        let id = self.alloc_id();
        let local_mtu = self.mode_mtu(mode);
        self.channels.insert(
            local_cid.raw(),
            Channel {
                local_cid,
                remote_cid: Cid::NULL,
                psm,
                state: ChannelState::WaitConnectRsp,
                local_mtu,
                remote_mtu: DEFAULT_MTU,
                mode,
                parameters: ModeParameters::default(),
                config_id: None,
                proposed_mode: mode,
                initiator: true,
                config_attempts: 0,
            },
        );
        self.pending.insert(id, local_cid.raw());
        Ok((
            local_cid,
            vec![{
                let request = Signal::ConnectionRequest {
                    id,
                    psm,
                    source_cid: local_cid,
                };
                let event = Self::signal(&request)?;
                self.awaiting(id, local_cid.raw(), request);
                event
            }],
        ))
    }

    /// Queue an SDU for an open channel.
    ///
    /// # Errors
    /// [`L2capError::UnknownChannel`] if the CID isn't ours, [`L2capError::WrongState`]
    /// if the channel isn't open, or [`L2capError::TooLong`] if the SDU exceeds the
    /// peer's advertised MTU.
    pub fn send(&mut self, cid: Cid, payload: Bytes) -> Result<Vec<L2capEvent>, L2capError> {
        let ch = self
            .channels
            .get(&cid.raw())
            .ok_or(L2capError::UnknownChannel(cid))?;
        if ch.state != ChannelState::Open {
            return Err(L2capError::WrongState {
                cid,
                state: ch.state.label(),
                action: "send",
            });
        }
        if payload.len() > usize::from(ch.remote_mtu) {
            // The MTU is the *SDU* ceiling in both modes — ERTM segments an SDU across
            // frames, but it does not make the peer willing to reassemble a larger one.
            return Err(L2capError::TooLong {
                len: payload.len(),
                max: usize::from(ch.remote_mtu),
            });
        }
        let remote_cid = ch.remote_cid;
        let Some(ertm) = self.ertm.get_mut(&cid.raw()) else {
            return Ok(vec![L2capEvent::Send(L2capPdu::new(remote_cid, payload))]);
        };
        let out = ertm.send(payload)?;
        let mut events: Vec<L2capEvent> = out
            .frames
            .into_iter()
            .map(|frame| L2capEvent::Send(L2capPdu::new(remote_cid, frame)))
            .collect();
        if out.failed {
            events.extend(self.fail_channel(cid));
        }
        Ok(events)
    }

    /// Begin tearing a channel down.
    ///
    /// # Errors
    /// [`L2capError::UnknownChannel`] if the CID isn't ours.
    pub fn disconnect(&mut self, cid: Cid) -> Result<Vec<L2capEvent>, L2capError> {
        let id = self.alloc_id();
        let ch = self
            .channels
            .get_mut(&cid.raw())
            .ok_or(L2capError::UnknownChannel(cid))?;
        ch.state = ChannelState::WaitDisconnect;
        let (dest, src) = (ch.remote_cid, ch.local_cid);
        self.ertm.remove(&cid.raw());
        // Whatever we were still timing for this channel is moot: we are tearing it down.
        // Only the disconnection request itself deserves a timer from here on.
        self.retire_timers(cid.raw());
        let request = Signal::DisconnectionRequest {
            id,
            dest_cid: dest,
            source_cid: src,
        };
        let event = Self::signal(&request)?;
        self.awaiting(id, cid.raw(), request);
        Ok(vec![event])
    }

    /// The ACL link went away: every channel on it is gone.
    pub fn link_down(&mut self) -> Vec<L2capEvent> {
        let closed = self
            .channels
            .drain()
            .map(|(_, ch)| L2capEvent::ChannelClosed {
                cid: ch.local_cid,
                psm: ch.psm,
            })
            .collect();
        self.pending.clear();
        self.outstanding.clear();
        self.ertm.clear();
        closed
    }

    /// How long until a retransmission timer needs attention, if any channel has one.
    ///
    /// `None` means nothing is waiting on time — which is the ordinary case, and the
    /// reason the caller can sleep on its socket rather than spinning on a tick.
    #[must_use]
    pub fn next_timeout(&self) -> Option<Duration> {
        self.ertm
            .values()
            .filter_map(Ertm::next_timeout)
            .chain(self.outstanding.values().map(|o| o.remaining))
            .min()
    }

    /// Note that we have sent `request` and are waiting for its answer.
    fn awaiting(&mut self, id: u8, raw_cid: u16, request: Signal) {
        self.outstanding.insert(
            id,
            Outstanding {
                raw_cid,
                request,
                remaining: RTX,
                retries: 0,
            },
        );
    }

    /// Forget every request still being timed for one channel.
    ///
    /// A response timer is only meaningful while the thing it times can still be answered.
    /// Once a channel is *open* its configuration is settled — by definition, since that is
    /// what opened it — and once it is gone there is nothing left to configure. Leaving the
    /// timer running in either case arms a delayed teardown of a channel that is working,
    /// which is precisely how a receiver drops a phone's audio twelve seconds into a track.
    fn retire_timers(&mut self, raw_cid: u16) {
        self.outstanding.retain(|_, out| out.raw_cid != raw_cid);
    }

    /// Advance every retransmission engine by `elapsed`.
    ///
    /// Time is passed in rather than read from a clock so the whole retransmission path
    /// stays deterministic and testable (ground rule 3).
    pub fn tick(&mut self, elapsed: Duration) -> Vec<L2capEvent> {
        let mut events = Vec::new();
        let mut failed = Vec::new();

        // Response timers first: a peer that has stopped answering should be given up on
        // in bounded time rather than held open against a CID nobody will ever free.
        let mut expired = Vec::new();
        for (id, out) in &mut self.outstanding {
            out.remaining = out.remaining.saturating_sub(elapsed);
            if out.remaining.is_zero() {
                expired.push(*id);
            }
        }
        for id in expired {
            let Some(out) = self.outstanding.get_mut(&id) else {
                continue;
            };
            if out.retries < RTX_RETRIES {
                out.retries += 1;
                out.remaining = RTX;
                let request = out.request.clone();
                let (raw_cid, retries) = (out.raw_cid, out.retries);
                // Loud, because a *healthy* link should never produce one of these: every
                // request we send is answered by the next packet on the wire. A run of them
                // four seconds apart is the whole signature of a request whose answer was
                // never recorded, and it took a log with none of these to find that.
                debug!(
                    cid = %Cid::new(raw_cid),
                    attempt = retries,
                    request = ?request,
                    "l2cap: no answer within the response timeout; asking again"
                );
                if let Ok(event) = Self::signal(&request) {
                    events.push(event);
                }
            } else {
                let raw_cid = out.raw_cid;
                self.outstanding.remove(&id);
                self.pending.remove(&id);
                debug!(
                    cid = %Cid::new(raw_cid),
                    "l2cap: giving up on a request the peer never answered; failing the channel"
                );
                failed.push(Cid::new(raw_cid));
            }
        }

        for (raw, ertm) in &mut self.ertm {
            let out = ertm.tick(elapsed);
            let Some(ch) = self.channels.get(raw) else {
                continue;
            };
            for frame in out.frames {
                events.push(L2capEvent::Send(L2capPdu::new(ch.remote_cid, frame)));
            }
            if out.failed {
                failed.push(ch.local_cid);
            }
        }
        for cid in failed {
            events.extend(self.fail_channel(cid));
        }
        events
    }

    /// Feed one reassembled PDU.
    ///
    /// # Errors
    /// Parse failures, or [`L2capError::UnknownChannel`] for data on a channel we don't
    /// have open.
    pub fn handle_pdu(&mut self, pdu: &L2capPdu) -> Result<Vec<L2capEvent>, L2capError> {
        if pdu.cid == Cid::SIGNALING {
            // Per-command, not per-frame. Failing the whole C-frame on one bad command
            // discarded well-formed commands packed alongside it — the packing this crate
            // went to the trouble of supporting — and left the peer with silence where
            // the spec requires `Command Reject`. A phone that packs an unrecognised
            // command with a `ConnectionRequest` never got its channel and never learned
            // why.
            let frame = Signal::decode_frame(&pdu.payload);
            let mut out = Vec::new();
            for refusal in &frame.rejects {
                debug!(
                    id = refusal.id,
                    reason = refusal.reason,
                    "l2cap: rejecting a command"
                );
                out.push(Self::signal(&Signal::CommandReject {
                    id: refusal.id,
                    reason: refusal.reason,
                    data: Bytes::new(),
                })?);
            }
            for sig in frame.signals {
                out.extend(self.handle_signal(sig)?);
            }
            return Ok(out);
        }
        let ch = self
            .channels
            .get(&pdu.cid.raw())
            .ok_or(L2capError::UnknownChannel(pdu.cid))?;
        if ch.state != ChannelState::Open {
            return Err(L2capError::WrongState {
                cid: pdu.cid,
                state: ch.state.label(),
                action: "receive data",
            });
        }
        let (local_cid, remote_cid, psm) = (ch.local_cid, ch.remote_cid, ch.psm);
        let Some(ertm) = self.ertm.get_mut(&pdu.cid.raw()) else {
            return Ok(vec![L2capEvent::Data {
                cid: local_cid,
                psm,
                payload: pdu.payload.clone(),
            }]);
        };
        let out = ertm.receive(&pdu.payload)?;
        let mut events = Vec::with_capacity(out.frames.len() + out.sdus.len());
        for frame in out.frames {
            events.push(L2capEvent::Send(L2capPdu::new(remote_cid, frame)));
        }
        for sdu in out.sdus {
            events.push(L2capEvent::Data {
                cid: local_cid,
                psm,
                payload: sdu,
            });
        }
        if out.failed {
            events.extend(self.fail_channel(local_cid));
        }
        Ok(events)
    }

    /// A retransmission engine gave up: tell the peer and the caller, once.
    fn fail_channel(&mut self, cid: Cid) -> Vec<L2capEvent> {
        self.ertm.remove(&cid.raw());
        self.retire_timers(cid.raw());
        let Some(ch) = self.channels.remove(&cid.raw()) else {
            return Vec::new();
        };
        let id = self.alloc_id();
        let mut events = Vec::with_capacity(2);
        if let Ok(event) = Self::signal(&Signal::DisconnectionRequest {
            id,
            dest_cid: ch.remote_cid,
            source_cid: ch.local_cid,
        }) {
            events.push(event);
        }
        events.push(L2capEvent::ChannelClosed {
            cid: ch.local_cid,
            psm: ch.psm,
        });
        events
    }

    fn handle_signal(&mut self, sig: Signal) -> Result<Vec<L2capEvent>, L2capError> {
        // Every answer retires the request it answers, here and only here. Doing it in the
        // individual handlers is what let `ConfigurationResponse` forget, and a forgotten
        // response timer is not inert: it re-sends the request at four and eight seconds
        // and tears the channel down at twelve, which on a phone streaming audio reads as
        // "the receiver hung up on me".
        let retired = sig.answers().and_then(|id| self.outstanding.remove(&id));
        match sig {
            Signal::ConnectionRequest {
                id,
                psm,
                source_cid,
            } => self.on_connection_request(id, psm, source_cid),
            Signal::ConnectionResponse {
                id,
                dest_cid,
                source_cid,
                result,
                ..
            } => self.on_connection_response(id, dest_cid, source_cid, result),
            Signal::ConfigurationRequest {
                id,
                dest_cid,
                flags,
                options,
            } => self.on_config_request(id, dest_cid, flags, &options),
            Signal::ConfigurationResponse {
                id,
                source_cid,
                result,
                options,
                ..
            } => self.on_config_response(id, source_cid, result, &options),
            Signal::DisconnectionRequest {
                id,
                dest_cid,
                source_cid,
            } => {
                let mut out = vec![Self::signal(&Signal::DisconnectionResponse {
                    id,
                    dest_cid,
                    source_cid,
                })?];
                self.ertm.remove(&dest_cid.raw());
                self.retire_timers(dest_cid.raw());
                if let Some(ch) = self.channels.remove(&dest_cid.raw()) {
                    out.push(L2capEvent::ChannelClosed {
                        cid: ch.local_cid,
                        psm: ch.psm,
                    });
                }
                Ok(out)
            }
            Signal::DisconnectionResponse { source_cid, .. } => {
                // The peer named our channel by *its* source CID in the response's
                // dest field; our own CID is the one we sent as source.
                let mut out = Vec::new();
                self.ertm.remove(&source_cid.raw());
                self.retire_timers(source_cid.raw());
                if let Some(ch) = self.channels.remove(&source_cid.raw()) {
                    out.push(L2capEvent::ChannelClosed {
                        cid: ch.local_cid,
                        psm: ch.psm,
                    });
                }
                Ok(out)
            }
            Signal::EchoRequest { id, data } => {
                Ok(vec![Self::signal(&Signal::EchoResponse { id, data })?])
            }
            Signal::InformationRequest { id, info_type } => {
                // The extended-features mask is what tells a peer whether it is worth
                // proposing Enhanced Retransmission Mode. Answering zero here is what
                // made cover art unreachable: a GOEP 2.0 responder that believes we have
                // no ERTM never gets as far as OBEX (#74).
                let (result, data) = match info_type {
                    0x0002 => (0x0000, Bytes::from_static(&[0x28, 0, 0, 0])),
                    0x0003 => (0x0000, Bytes::from_static(&[0x02, 0, 0, 0, 0, 0, 0, 0])),
                    _ => (0x0001, Bytes::new()),
                };
                Ok(vec![Self::signal(&Signal::InformationResponse {
                    id,
                    info_type,
                    result,
                    data,
                })?])
            }
            Signal::CommandReject { id, reason, .. } => {
                // The peer refused something we sent. Swallowing this meant waiting out
                // the response timer for an answer that is never coming — and before
                // there *was* a response timer, waiting forever.
                let Some(out) = retired else {
                    return Ok(Vec::new());
                };
                let cid = Cid::new(out.raw_cid);
                self.pending.remove(&id);
                debug!(%cid, reason, "l2cap: peer rejected our command");
                Ok(self.fail_channel(cid))
            }
            Signal::EchoResponse { .. } | Signal::InformationResponse { .. } => Ok(Vec::new()),
        }
    }

    fn on_connection_request(
        &mut self,
        id: u8,
        psm: Psm,
        source_cid: Cid,
    ) -> Result<Vec<L2capEvent>, L2capError> {
        let Some((_, mode)) = self.listening.iter().find(|(p, _)| *p == psm).copied() else {
            return Ok(vec![Self::signal(&Signal::ConnectionResponse {
                id,
                dest_cid: Cid::NULL,
                source_cid,
                result: ConnectionResult::PsmNotSupported,
                status: 0,
            })?]);
        };
        let local_cid = self.alloc_cid()?;
        let local_mtu = self.mode_mtu(mode);
        self.channels.insert(
            local_cid.raw(),
            Channel {
                local_cid,
                remote_cid: source_cid,
                psm,
                state: ChannelState::WaitConfig {
                    outgoing_done: false,
                    incoming_done: false,
                },
                local_mtu,
                remote_mtu: DEFAULT_MTU,
                mode,
                parameters: ModeParameters::default(),
                config_id: None,
                proposed_mode: mode,
                initiator: false,
                config_attempts: 0,
            },
        );
        let mut out = vec![Self::signal(&Signal::ConnectionResponse {
            id,
            dest_cid: local_cid,
            source_cid,
            result: ConnectionResult::Success,
            status: 0,
        })?];
        // Configure immediately rather than waiting to be asked: the peer is entitled to
        // wait for us, and two stacks each waiting is a hung channel.
        out.push(self.request_configuration(local_cid.raw())?);
        Ok(out)
    }

    fn on_connection_response(
        &mut self,
        id: u8,
        dest_cid: Cid,
        source_cid: Cid,
        result: ConnectionResult,
    ) -> Result<Vec<L2capEvent>, L2capError> {
        if result == ConnectionResult::Pending {
            return Ok(Vec::new()); // a final response will follow
        }
        let local_raw = self
            .pending
            .get(&id)
            .copied()
            .unwrap_or_else(|| source_cid.raw());
        let Some(ch) = self.channels.get_mut(&local_raw) else {
            return Ok(Vec::new());
        };
        if result != ConnectionResult::Success {
            let psm = ch.psm;
            self.channels.remove(&local_raw);
            self.pending.remove(&id);
            return Ok(vec![L2capEvent::ConnectFailed { psm, result }]);
        }
        ch.remote_cid = dest_cid;
        ch.state = ChannelState::WaitConfig {
            outgoing_done: false,
            incoming_done: false,
        };
        self.pending.remove(&id);
        Ok(vec![self.request_configuration(local_raw)?])
    }

    /// Propose our side of the configuration, in whatever mode the channel is set to.
    fn request_configuration(&mut self, raw_cid: u16) -> Result<L2capEvent, L2capError> {
        let config_id = self.alloc_id();
        let receive_mps = self.receive_mps();
        // A new proposal withdraws the previous one, so the previous one's timer has
        // nothing left to wait for. `config_id` already encodes that a stale *response* is
        // ignored; this is the same statement about a stale *request*.
        self.retire_timers(raw_cid);
        let Some(ch) = self.channels.get_mut(&raw_cid) else {
            return Err(L2capError::UnknownChannel(Cid::new(raw_cid)));
        };
        ch.config_id = Some(config_id);
        ch.proposed_mode = ch.mode;
        ch.config_attempts = ch.config_attempts.saturating_add(1);
        let mut options = vec![ConfigOption::Mtu(ch.local_mtu)];
        if ch.mode == ChannelMode::EnhancedRetransmission {
            // The MPS names what *we* can receive; the peer's own request names what it
            // can, and that is the number we segment against.
            options.push(ConfigOption::Retransmission(RetransmissionConfig::ertm(
                receive_mps,
            )));
            options.push(ConfigOption::Fcs(ch.parameters.fcs));
        }
        let request = Signal::ConfigurationRequest {
            id: config_id,
            dest_cid: ch.remote_cid,
            flags: 0,
            options,
        };
        let event = Self::signal(&request)?;
        self.awaiting(config_id, raw_cid, request);
        Ok(event)
    }

    fn on_config_request(
        &mut self,
        id: u8,
        dest_cid: Cid,
        flags: u16,
        options: &[ConfigOption],
    ) -> Result<Vec<L2capEvent>, L2capError> {
        // Bit 0 is the continuation flag: the peer is sending its option list across
        // several requests because it did not fit in one. It was being destructured away,
        // so a partial list was answered `Success` with C=0 and the channel opened while
        // the peer was still describing it — both ends then believing different things
        // about a channel that is nominally up, which shows as "connects, then the first
        // data PDU is dropped".
        //
        // The options in a continued request still apply; what must not happen is
        // *completing* the configuration. So accumulate, answer with the same flag, and
        // let the final C=0 request finish the exchange.
        let continues = flags & CONTINUATION_FLAG != 0;
        let receive_mps = self.receive_mps();
        let basic_mtu = self.local_mtu;
        let Some(ch) = self.channels.get(&dest_cid.raw()) else {
            return Ok(vec![Self::signal(&Signal::ConfigurationResponse {
                id,
                source_cid: dest_cid,
                flags: 0,
                result: ConfigResult::Rejected,
                options: Vec::new(),
            })?]);
        };
        let (remote_cid, our_mode, initiator) = (ch.remote_cid, ch.mode, ch.initiator);
        // Once our own direction is agreed the mode is no longer ours to move: both
        // directions of a channel run in the same one, and agreeing to a second would
        // leave the two ends framing differently.
        let settled = matches!(
            ch.state,
            ChannelState::WaitConfig {
                outgoing_done: true,
                ..
            } | ChannelState::Open
        );

        // An unknown option without the hint bit must be refused by name; one with the
        // hint bit may be ignored. Getting this backwards either breaks peers that send
        // hints or accepts a mode we don't implement.
        let unsupported: Vec<ConfigOption> = options
            .iter()
            .filter(|o| !o.is_ignorable())
            .cloned()
            .collect();
        if !unsupported.is_empty() {
            return Ok(vec![Self::signal(&Signal::ConfigurationResponse {
                id,
                source_cid: remote_cid,
                flags: 0,
                result: ConfigResult::UnknownOptions,
                options: unsupported,
            })?]);
        }

        // Extended window size comes with the four-byte control field, which we do not
        // implement. Counter-proposing zero says "use the standard one" — a refusal the
        // peer can act on, rather than one that fails the channel.
        if options
            .iter()
            .any(|o| matches!(o, ConfigOption::ExtendedWindowSize(n) if *n != 0))
        {
            return Ok(vec![Self::signal(&Signal::ConfigurationResponse {
                id,
                source_cid: remote_cid,
                flags: 0,
                result: ConfigResult::Unacceptable,
                options: vec![ConfigOption::ExtendedWindowSize(0)],
            })?]);
        }

        // An absent retransmission option means basic mode; that is what makes it the
        // default rather than an error.
        let proposed = options
            .iter()
            .find_map(|o| match o {
                ConfigOption::Retransmission(config) => Some(*config),
                _ => None,
            })
            .unwrap_or_else(RetransmissionConfig::basic);

        if proposed.mode != our_mode {
            let adoptable = matches!(
                proposed.mode,
                ChannelMode::Basic | ChannelMode::EnhancedRetransmission
            );
            if !adoptable || settled || !initiator {
                // Refuse by naming the mode we are running, which is what lets a peer that
                // can do both fall back rather than give up.
                let counter = if our_mode == ChannelMode::EnhancedRetransmission {
                    RetransmissionConfig::ertm(receive_mps)
                } else {
                    RetransmissionConfig::basic()
                };
                return Ok(vec![Self::signal(&Signal::ConfigurationResponse {
                    id,
                    source_cid: remote_cid,
                    flags: 0,
                    result: ConfigResult::Unacceptable,
                    options: vec![ConfigOption::Retransmission(counter)],
                })?]);
            }
        }

        let Some(ch) = self.channels.get_mut(&dest_cid.raw()) else {
            return Ok(Vec::new());
        };
        // We dialled, they listened, and they have named a mode: theirs wins. The old
        // proposal then has to be withdrawn as well as replaced — a Success for the
        // superseded one would otherwise mark us configured in a mode we no longer
        // intend, which is why `config_id` exists.
        // Dropping to basic mode drops the SDU ceiling with it: without segmentation an
        // MTU larger than one ACL packet is a promise we cannot keep.
        ch.mode = proposed.mode;
        ch.local_mtu = ch.local_mtu.min(ceiling_for(proposed.mode, basic_mtu));

        // The MTU in *their* request is what they can receive, so it bounds what we send.
        for opt in options {
            match opt {
                ConfigOption::Mtu(mtu) => ch.remote_mtu = *mtu,
                ConfigOption::Fcs(fcs) => ch.parameters.fcs = *fcs,
                _ => {}
            }
        }

        let mut response = vec![ConfigOption::Mtu(ch.remote_mtu)];
        if ch.mode == ChannelMode::EnhancedRetransmission {
            // Their MPS is what they can receive; ours is the smaller of that and what we
            // are willing to put on the air.
            let send_mps = proposed.mps.min(receive_mps).max(1);
            ch.parameters.send_mps = send_mps;
            response.push(ConfigOption::Retransmission(RetransmissionConfig {
                mode: ChannelMode::EnhancedRetransmission,
                // Bounds *their* transmit window, so it is ours to reduce.
                tx_window: proposed.tx_window.clamp(1, DEFAULT_TX_WINDOW),
                max_transmit: proposed.max_transmit,
                // Zero in their request; the responder is the one that picks these.
                retransmission_timeout_ms: ms(DEFAULT_RETRANSMISSION_TIMEOUT),
                monitor_timeout_ms: ms(DEFAULT_MONITOR_TIMEOUT),
                mps: send_mps,
            }));
            response.push(ConfigOption::Fcs(ch.parameters.fcs));
        }

        // A continued request is answered — with the flag echoed, so the peer knows we
        // followed — but does not *complete* the incoming direction. Only the final
        // request, the one with C=0, does that.
        if !continues {
            if let ChannelState::WaitConfig { incoming_done, .. } = &mut ch.state {
                *incoming_done = true;
            }
        }
        let repropose = !continues && ch.mode != ch.proposed_mode;
        let attempts = ch.config_attempts;

        let mut out = vec![Self::signal(&Signal::ConfigurationResponse {
            id,
            source_cid: remote_cid,
            flags: if continues { CONTINUATION_FLAG } else { 0 },
            result: ConfigResult::Success,
            options: response,
        })?];
        if continues {
            // Nothing else to do until the rest arrives: promoting now would open a
            // channel the peer is still describing.
            return Ok(out);
        }
        // Promote *before* re-proposing, not after: promotion retires the timers of the
        // proposals it settles, and a re-proposal issued first would have its own brand-new
        // timer retired along with them.
        out.extend(self.promote_if_configured(dest_cid.raw()));
        if repropose {
            if attempts >= MAX_CONFIG_ATTEMPTS {
                return Ok(self.fail_configuration(dest_cid));
            }
            out.push(self.request_configuration(dest_cid.raw())?);
        }
        Ok(out)
    }

    fn on_config_response(
        &mut self,
        id: u8,
        source_cid: Cid,
        result: ConfigResult,
        options: &[ConfigOption],
    ) -> Result<Vec<L2capEvent>, L2capError> {
        let basic_mtu = self.local_mtu;
        let Some(ch) = self.channels.get_mut(&source_cid.raw()) else {
            return Ok(Vec::new());
        };
        if ch.config_id.is_some_and(|outstanding| outstanding != id) {
            // A response to a proposal we have already withdrawn. Acting on it would mark
            // us configured in a mode we changed our mind about.
            return Ok(Vec::new());
        }

        if result == ConfigResult::Unacceptable {
            // A counter-proposal, not a refusal: the peer has told us what it *would*
            // accept, and the exchange converges by asking again with those numbers. A
            // peer that answers our ERTM proposal with "basic" is not saying no to the
            // channel — GOEP 1.x is a perfectly good way to move a thumbnail.
            let counter = options.iter().find_map(|o| match o {
                ConfigOption::Retransmission(config) => Some(*config),
                _ => None,
            });
            if let Some(counter) = counter.filter(|c| {
                matches!(
                    c.mode,
                    ChannelMode::Basic | ChannelMode::EnhancedRetransmission
                )
            }) {
                ch.mode = counter.mode;
                ch.local_mtu = ch.local_mtu.min(ceiling_for(counter.mode, basic_mtu));
            }
            for opt in options {
                if let ConfigOption::Mtu(mtu) = opt {
                    ch.local_mtu = *mtu;
                }
            }
            let attempts = ch.config_attempts;
            if attempts >= MAX_CONFIG_ATTEMPTS {
                return Ok(self.fail_configuration(source_cid));
            }
            return Ok(vec![self.request_configuration(source_cid.raw())?]);
        }

        if result != ConfigResult::Success {
            return Ok(self.fail_configuration(source_cid));
        }

        // Their response is where the timers and our send window are finally settled.
        for opt in options {
            match opt {
                ConfigOption::Retransmission(config) => {
                    ch.parameters.send_window = config.tx_window.clamp(1, DEFAULT_TX_WINDOW);
                    if config.retransmission_timeout_ms != 0 {
                        ch.parameters.retransmission_timeout =
                            Duration::from_millis(u64::from(config.retransmission_timeout_ms));
                    }
                    if config.monitor_timeout_ms != 0 {
                        ch.parameters.monitor_timeout =
                            Duration::from_millis(u64::from(config.monitor_timeout_ms));
                    }
                    ch.parameters.max_transmit = config.max_transmit.max(1);
                }
                // **Not** the FCS. A `Success` response carrying "No FCS" does not turn
                // the frame check sequence off, however much it looks like agreement.
                //
                // The option is decided by *requests*: FCS is omitted only when a peer
                // asks for its omission in a Configuration Request, which is the branch in
                // `on_config_request`. Linux is explicit about the asymmetry — it sets its
                // `CONF_RECV_NO_FCS` flag from a request unconditionally, and from a
                // response only when the result is `PENDING`, never on success
                // (`net/bluetooth/l2cap_core.c`).
                //
                // Adopting it here cost a live session. An iPhone answered our 16-bit
                // request with a `Success` naming No FCS; we believed it and stopped
                // appending the checksum, while the phone went on expecting one. Every
                // frame after that failed its check at the far end, the cover-art channel
                // went silent mid-session, and nothing in the log said why — the exchange
                // simply stopped. The session that worked, minutes earlier on the same
                // phone, had negotiated 16-bit on both sides and moved 108 KB.
                ConfigOption::Fcs(_) => {}
                _ => {}
            }
        }
        ch.config_id = None;
        if let ChannelState::WaitConfig { outgoing_done, .. } = &mut ch.state {
            *outgoing_done = true;
        }
        Ok(self.promote_if_configured(source_cid.raw()))
    }

    /// Give up on a channel whose configuration will not converge.
    fn fail_configuration(&mut self, cid: Cid) -> Vec<L2capEvent> {
        self.ertm.remove(&cid.raw());
        self.retire_timers(cid.raw());
        let Some(ch) = self.channels.remove(&cid.raw()) else {
            return Vec::new();
        };
        let mut events = Vec::with_capacity(2);
        // Tell the peer, rather than just forgetting locally. `fail_channel` already did
        // this and these two paths disagreed: a channel abandoned here left the phone
        // holding a half-open one until its own RTX gave up, and a retry in the meantime
        // collides with a CID we now consider free.
        let id = self.alloc_id();
        let request = Signal::DisconnectionRequest {
            id,
            dest_cid: ch.remote_cid,
            source_cid: ch.local_cid,
        };
        if let Ok(event) = Self::signal(&request) {
            events.push(event);
            self.awaiting(id, cid.raw(), request);
        }
        events.push(L2capEvent::ChannelClosed {
            cid: ch.local_cid,
            psm: ch.psm,
        });
        events
    }

    /// Open the channel only once *both* directions are configured.
    fn promote_if_configured(&mut self, raw_cid: u16) -> Vec<L2capEvent> {
        let Some(ch) = self.channels.get_mut(&raw_cid) else {
            return Vec::new();
        };
        let ChannelState::WaitConfig {
            outgoing_done,
            incoming_done,
        } = ch.state
        else {
            return Vec::new();
        };
        if !(outgoing_done && incoming_done) {
            return Vec::new();
        }
        ch.state = ChannelState::Open;
        // An open channel has no unanswered configuration: that is what "open" means. Say
        // so by construction rather than trusting each response handler to have retired its
        // own timer — the belt to `Signal::answers`'s braces, and the one that makes it
        // impossible for a *working* channel to be sitting on a fuse.
        self.retire_timers(raw_cid);
        let Some(ch) = self.channels.get_mut(&raw_cid) else {
            return Vec::new();
        };
        if ch.mode == ChannelMode::EnhancedRetransmission {
            self.ertm.insert(
                raw_cid,
                Ertm::new(ErtmParameters {
                    local_cid: ch.local_cid,
                    remote_cid: ch.remote_cid,
                    fcs: ch.parameters.fcs,
                    send_mps: ch.parameters.send_mps,
                    send_window: ch.parameters.send_window,
                    max_transmit: ch.parameters.max_transmit,
                    retransmission_timeout: ch.parameters.retransmission_timeout,
                    monitor_timeout: ch.parameters.monitor_timeout,
                    local_mtu: ch.local_mtu,
                }),
            );
        }
        vec![L2capEvent::ChannelOpen {
            cid: ch.local_cid,
            psm: ch.psm,
            peer_mtu: ch.remote_mtu,
        }]
    }
}

/// The SDU ceiling a channel in `mode` may advertise, given the basic-mode one.
fn ceiling_for(mode: ChannelMode, basic_mtu: u16) -> u16 {
    match mode {
        ChannelMode::EnhancedRetransmission => ERTM_MTU.max(basic_mtu),
        _ => basic_mtu,
    }
}

/// A duration as the milliseconds the wire field wants.
fn ms(duration: Duration) -> u16 {
    u16::try_from(duration.as_millis()).unwrap_or(u16::MAX)
}
