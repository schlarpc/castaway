//! The channel multiplexer: a sans-I/O state machine for one ACL link.
//!
//! `fn(state, input) -> (state, outputs)` exactly as ground rule 3 asks. Nothing here
//! touches a socket; the caller feeds it reassembled PDUs and writes out whatever
//! [`L2capEvent::Send`] it produces. That is what makes the whole connect → configure →
//! stream → disconnect flow testable with no radio.

use std::collections::HashMap;

use bytes::Bytes;

use crate::error::L2capError;
use crate::pdu::{Cid, L2capPdu, Psm};
use crate::signaling::{ConfigOption, ConfigResult, ConnectionResult, Signal};

/// Default MTU for basic mode when the peer proposes nothing.
pub const DEFAULT_MTU: u16 = 672;

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
    listening: Vec<Psm>,
    next_cid: u16,
    next_id: u8,
    /// Local CID awaiting a response, keyed by the signaling id we used.
    pending: HashMap<u8, u16>,
    local_mtu: u16,
}

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
            listening: Vec::new(),
            next_cid: Cid::DYNAMIC_START,
            next_id: 1,
            pending: HashMap::new(),
            local_mtu,
        }
    }

    /// Accept incoming connections to `psm`. Anything else is refused with
    /// [`ConnectionResult::PsmNotSupported`].
    pub fn listen(&mut self, psm: Psm) {
        if !self.listening.contains(&psm) {
            self.listening.push(psm);
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

    /// Open a channel to `psm` on the peer — used for the AVRCP cover-art fetch, where
    /// *we* are the one connecting out.
    ///
    /// # Errors
    /// [`L2capError::OutOfCids`] if no identifier is free.
    pub fn connect(&mut self, psm: Psm) -> Result<Vec<L2capEvent>, L2capError> {
        let local_cid = self.alloc_cid()?;
        let id = self.alloc_id();
        self.channels.insert(
            local_cid.raw(),
            Channel {
                local_cid,
                remote_cid: Cid::NULL,
                psm,
                state: ChannelState::WaitConnectRsp,
                local_mtu: self.local_mtu,
                remote_mtu: DEFAULT_MTU,
            },
        );
        self.pending.insert(id, local_cid.raw());
        Ok(vec![Self::signal(&Signal::ConnectionRequest {
            id,
            psm,
            source_cid: local_cid,
        })?])
    }

    /// Queue an SDU for an open channel.
    ///
    /// # Errors
    /// [`L2capError::UnknownChannel`] if the CID isn't ours, [`L2capError::WrongState`]
    /// if the channel isn't open, or [`L2capError::TooLong`] if the SDU exceeds the
    /// peer's advertised MTU.
    pub fn send(&self, cid: Cid, payload: Bytes) -> Result<Vec<L2capEvent>, L2capError> {
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
            // Refused here rather than fragmented: basic mode has no SDU segmentation,
            // so oversizing is a protocol error the peer would drop silently.
            return Err(L2capError::TooLong {
                len: payload.len(),
                max: usize::from(ch.remote_mtu),
            });
        }
        Ok(vec![L2capEvent::Send(L2capPdu::new(
            ch.remote_cid,
            payload,
        ))])
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
        Ok(vec![Self::signal(&Signal::DisconnectionRequest {
            id,
            dest_cid: dest,
            source_cid: src,
        })?])
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
        closed
    }

    /// Feed one reassembled PDU.
    ///
    /// # Errors
    /// Parse failures, or [`L2capError::UnknownChannel`] for data on a channel we don't
    /// have open.
    pub fn handle_pdu(&mut self, pdu: &L2capPdu) -> Result<Vec<L2capEvent>, L2capError> {
        if pdu.cid == Cid::SIGNALING {
            let mut out = Vec::new();
            for sig in Signal::decode_all(&pdu.payload)? {
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
        Ok(vec![L2capEvent::Data {
            cid: ch.local_cid,
            psm: ch.psm,
            payload: pdu.payload.clone(),
        }])
    }

    fn handle_signal(&mut self, sig: Signal) -> Result<Vec<L2capEvent>, L2capError> {
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
                options,
                ..
            } => self.on_config_request(id, dest_cid, &options),
            Signal::ConfigurationResponse {
                source_cid, result, ..
            } => self.on_config_response(source_cid, result),
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
                // Extended features: we implement basic mode only, so the mask is zero.
                // Answering "not supported" instead makes some stacks retry forever.
                let (result, data) = match info_type {
                    0x0002 => (0x0000, Bytes::from_static(&[0, 0, 0, 0])),
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
            Signal::EchoResponse { .. }
            | Signal::InformationResponse { .. }
            | Signal::CommandReject { .. } => Ok(Vec::new()),
        }
    }

    fn on_connection_request(
        &mut self,
        id: u8,
        psm: Psm,
        source_cid: Cid,
    ) -> Result<Vec<L2capEvent>, L2capError> {
        if !self.listening.contains(&psm) {
            return Ok(vec![Self::signal(&Signal::ConnectionResponse {
                id,
                dest_cid: Cid::NULL,
                source_cid,
                result: ConnectionResult::PsmNotSupported,
                status: 0,
            })?]);
        }
        let local_cid = self.alloc_cid()?;
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
                local_mtu: self.local_mtu,
                remote_mtu: DEFAULT_MTU,
            },
        );
        let config_id = self.alloc_id();
        Ok(vec![
            Self::signal(&Signal::ConnectionResponse {
                id,
                dest_cid: local_cid,
                source_cid,
                result: ConnectionResult::Success,
                status: 0,
            })?,
            // Configure immediately rather than waiting to be asked: the peer is
            // entitled to wait for us, and two stacks each waiting is a hung channel.
            Self::signal(&Signal::ConfigurationRequest {
                id: config_id,
                dest_cid: source_cid,
                flags: 0,
                options: vec![ConfigOption::Mtu(self.local_mtu)],
            })?,
        ])
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
        let config_id = self.alloc_id();
        Ok(vec![Self::signal(&Signal::ConfigurationRequest {
            id: config_id,
            dest_cid,
            flags: 0,
            options: vec![ConfigOption::Mtu(self.local_mtu)],
        })?])
    }

    fn on_config_request(
        &mut self,
        id: u8,
        dest_cid: Cid,
        options: &[ConfigOption],
    ) -> Result<Vec<L2capEvent>, L2capError> {
        let Some(ch) = self.channels.get_mut(&dest_cid.raw()) else {
            return Ok(vec![Self::signal(&Signal::ConfigurationResponse {
                id,
                source_cid: dest_cid,
                flags: 0,
                result: ConfigResult::Rejected,
                options: Vec::new(),
            })?]);
        };

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
                source_cid: ch.remote_cid,
                flags: 0,
                result: ConfigResult::UnknownOptions,
                options: unsupported,
            })?]);
        }

        // The MTU in *their* request is what they can receive, so it bounds what we send.
        for opt in options {
            if let ConfigOption::Mtu(mtu) = opt {
                ch.remote_mtu = *mtu;
            }
        }
        let remote_cid = ch.remote_cid;
        let mut out = vec![Self::signal(&Signal::ConfigurationResponse {
            id,
            source_cid: remote_cid,
            flags: 0,
            result: ConfigResult::Success,
            options: vec![ConfigOption::Mtu(ch.remote_mtu)],
        })?];
        if let ChannelState::WaitConfig { incoming_done, .. } = &mut ch.state {
            *incoming_done = true;
        }
        out.extend(self.promote_if_configured(dest_cid.raw()));
        Ok(out)
    }

    fn on_config_response(
        &mut self,
        source_cid: Cid,
        result: ConfigResult,
    ) -> Result<Vec<L2capEvent>, L2capError> {
        let Some(ch) = self.channels.get_mut(&source_cid.raw()) else {
            return Ok(Vec::new());
        };
        if result != ConfigResult::Success {
            let (cid, psm) = (ch.local_cid, ch.psm);
            self.channels.remove(&source_cid.raw());
            return Ok(vec![L2capEvent::ChannelClosed { cid, psm }]);
        }
        if let ChannelState::WaitConfig { outgoing_done, .. } = &mut ch.state {
            *outgoing_done = true;
        }
        Ok(self.promote_if_configured(source_cid.raw()))
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
        vec![L2capEvent::ChannelOpen {
            cid: ch.local_cid,
            psm: ch.psm,
            peer_mtu: ch.remote_mtu,
        }]
    }
}
