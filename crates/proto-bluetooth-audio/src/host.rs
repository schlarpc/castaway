//! Controller bring-up and pairing policy, as a pure state machine.
//!
//! Everything #68 decided lives here and is therefore testable: Just Works pairing with
//! no prompt on either side, link keys persisted so a repeat guest reconnects silently,
//! discoverable only while no session is active, and legacy PIN pairing refused outright.
//!
//! `fn(state, event) -> (state, actions)` per ground rule 3. The actor above writes the
//! [`HostAction::Send`]s to the transport and hands events back; nothing here opens a
//! socket, so the whole bring-up sequence and every pairing path is exercised in unit
//! tests with no radio.

use std::collections::HashMap;
use std::time::Duration;

use tracing::{debug, warn};

use bytes::Bytes;
use substrate_hci::{
    AcceptRole, AuthRequirements, BdAddr, ClassOfDevice, Command, CommandCredits, ConnectionHandle,
    Eir, Event, IoCapability, LinkKey, LinkType, ScanEnable, Status,
};

/// How far controller bring-up has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostState {
    /// Nothing sent yet.
    Down,
    /// Working through the initialisation sequence.
    Initializing,
    /// Ready to accept connections.
    Ready,
}

/// What the host wants done.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostAction {
    /// Send this command to the controller.
    Send(Command),
    /// Bring-up finished; the controller is discoverable.
    Ready {
        /// Our own address, useful for logs and for the EIR.
        address: BdAddr,
        /// How many ACL fragments may be outstanding.
        acl_credits: u16,
        /// Largest ACL fragment the controller accepts.
        acl_mtu: u16,
    },
    /// A link came up.
    LinkUp {
        /// The controller's handle for it.
        handle: ConnectionHandle,
        /// Who is on the other end.
        peer: BdAddr,
    },
    /// A link went away.
    LinkDown {
        /// The handle that is now dead.
        handle: ConnectionHandle,
        /// Who it was.
        peer: BdAddr,
        /// Why, as the controller reported it.
        reason: Status,
    },
    /// The peer told us what it calls itself.
    PeerName {
        /// Which peer.
        peer: BdAddr,
        /// Its friendly name.
        name: String,
    },
    /// Pairing produced a link key the caller should persist (#68).
    Paired {
        /// The peer it belongs to.
        peer: BdAddr,
        /// The key.
        key: LinkKey,
    },
    /// A stored link key turned out to be stale and has been forgotten; the caller should
    /// drop it from disk too.
    ///
    /// Without this the key survives a restart and the phone that cannot authenticate with
    /// it cannot authenticate after a reboot either — the loop just becomes durable.
    Unpaired {
        /// The peer whose key is no longer valid.
        peer: BdAddr,
    },
    /// The controller freed ACL buffers; this many fragments may be sent again.
    Credits {
        /// Which link.
        handle: ConnectionHandle,
        /// How many fragments completed.
        count: u16,
    },
}

/// Static configuration for the controller.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// The name senders see in their Bluetooth menu.
    pub name: String,
    /// What kind of device we claim to be.
    pub class_of_device: ClassOfDevice,
    /// Whether to be discoverable at all.
    ///
    /// Not "when idle": a receiver anyone in the room should be able to use has to stay
    /// findable while it is in use, or the second person to want it cannot pair (#68, as
    /// amended). Turning this off is for a box that should only ever serve devices that
    /// already know it.
    pub discoverable: bool,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            name: "castaway".to_owned(),
            class_of_device: ClassOfDevice::LOUDSPEAKER,
            discoverable: true,
        }
    }
}

/// The service classes we advertise in the inquiry response.
///
/// These mirror the SDP records `adapter` registers, and the mirroring is the point: a
/// peer is entitled to act on the EIR without querying SDP, so a class listed here that
/// no record backs invites a connection we will then refuse.
///
/// Both AVRCP roles appear because we publish both records: Controller to drive the
/// phone's player, Target so its volume rocker reaches us (Q24).
const SERVICE_CLASSES: [u16; 5] = [
    0x110B, // Audio Sink — the A2DP half.
    0x110C, // A/V Remote Control Target — the volume-rocker half.
    0x110D, // Advanced Audio Distribution — a profile UUID, published as a class on
    // purpose so KDE stops calling us "Other device"; see `a2dp_sink` and D48.
    0x110E, // A/V Remote Control — the generic class every AVRCP role carries.
    0x110F, // A/V Remote Control Controller — the role we play toward the phone's player.
];

/// How long a command may go unanswered before the host gives up on it.
///
/// Five seconds, which is what the firmware loaders already allow their own exchanges
/// (`hci-transport/src/init/intel.rs`, `.../realtek.rs`) — the same wait for the same
/// reason, and the loaders having it was exactly why bring-up's own silence went
/// unnoticed. Long enough that a controller merely busy with the radio is not written
/// off; short enough that a receiver which came up half-configured says so while
/// somebody is still in the room to hear it.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Inquiry scan interval: 1024 slots of 0.625 ms = 640 ms.
const INQUIRY_SCAN_INTERVAL: u16 = 0x0400;
/// Inquiry scan window: 96 slots = 60 ms, so ~9% of the radio goes to being findable.
const INQUIRY_SCAN_WINDOW: u16 = 0x0060;

/// Drives a controller from reset to discoverable, and answers pairing.
#[derive(Debug)]
pub struct HostController {
    config: HostConfig,
    state: HostState,
    /// Commands still to send, each fired when the previous one completes.
    pending: Vec<Command>,
    address: BdAddr,
    acl_credits: u16,
    acl_mtu: u16,
    /// Link keys for peers we have paired with before.
    link_keys: HashMap<BdAddr, LinkKey>,
    /// Live connections, so a disconnection can name its peer.
    connections: HashMap<u16, BdAddr>,
    /// The controller's command window. Every command this host emits passes through it,
    /// so nothing is written into a slot the controller does not have (#90).
    commands: CommandCredits,
    /// How long the oldest unanswered command has been outstanding.
    ///
    /// Advanced by [`HostController::tick`] rather than read from a clock: this is a pure
    /// state machine and the actor above owns the time (ground rule 3).
    unanswered_for: Duration,
}

impl HostController {
    /// Build a host controller.
    #[must_use]
    pub fn new(config: HostConfig) -> Self {
        Self {
            config,
            state: HostState::Down,
            pending: Vec::new(),
            address: BdAddr::ZERO,
            acl_credits: 1,
            acl_mtu: 339,
            link_keys: HashMap::new(),
            connections: HashMap::new(),
            commands: CommandCredits::new(),
            unanswered_for: Duration::ZERO,
        }
    }

    /// Seed the controller with link keys loaded from disk, so a repeat guest
    /// reconnects without pairing again (#68).
    pub fn load_link_keys(&mut self, keys: impl IntoIterator<Item = (BdAddr, LinkKey)>) {
        self.link_keys.extend(keys);
    }

    /// Bring-up state.
    #[must_use]
    pub const fn state(&self) -> HostState {
        self.state
    }

    /// The controller's own address, once known.
    #[must_use]
    pub const fn address(&self) -> BdAddr {
        self.address
    }

    /// Largest ACL fragment the controller accepts. Fragmentation must respect this.
    #[must_use]
    pub const fn acl_mtu(&self) -> u16 {
        self.acl_mtu
    }

    /// Whether we have a stored key for `peer`.
    #[must_use]
    pub fn knows(&self, peer: BdAddr) -> bool {
        self.link_keys.contains_key(&peer)
    }

    /// Begin bring-up. Returns the first command; the rest follow as each completes.
    ///
    /// The order is not arbitrary. `Reset` must come first because a controller that a
    /// previous run left configured will otherwise answer with stale state, and
    /// `WriteSimplePairingMode` must precede `WriteScanEnable` — a peer that pages us
    /// before SSP is enabled gets legacy PIN pairing, which we refuse, so it would fail
    /// to connect for reasons no log explains.
    pub fn start(&mut self) -> Vec<HostAction> {
        self.state = HostState::Initializing;
        self.pending = vec![
            Command::ReadLocalVersion,
            Command::ReadBufferSize,
            Command::ReadBdAddr,
            // Every event we actually handle, plus the SSP ones — which are masked off
            // by default on many controllers, so omitting this makes pairing hang with
            // no event ever arriving.
            Command::SetEventMask(0xFFFF_FFFF_FFFF_FFFF),
            Command::WriteClassOfDevice(self.config.class_of_device),
            Command::WriteLocalName(self.config.name.clone()),
            // The name again, in the inquiry response itself. `WriteLocalName` only
            // answers a *separate* RemoteNameRequest, which BlueZ sends and Android does
            // not — so without this the panel is discoverable and invisible to precisely
            // the devices most likely to walk up to it (Q24).
            Command::WriteExtendedInquiryResponse {
                fec_required: false,
                data: self.eir(),
            },
            Command::WriteSimplePairingMode(true),
            // Scan hard enough to be found *while streaming*. The controller defaults to
            // an 11.25 ms inquiry window every 1.28 s — under 1% of the radio — and an
            // active A2DP link starves even that, which is why a receiver in use vanishes
            // from every scan list in the room. For a mains-powered box that anyone
            // should be able to walk up to, that is the wrong trade: spend ~9% of the
            // radio on being findable (#68).
            Command::WriteInquiryScanActivity {
                interval: INQUIRY_SCAN_INTERVAL,
                window: INQUIRY_SCAN_WINDOW,
            },
            // Interlaced covers the frequency train in half the time, so discovery is
            // about twice as fast. Optional in the spec — a controller without it answers
            // "unsupported feature", and bring-up carries on regardless.
            Command::WriteInquiryScanType(true),
            Command::WritePageScanType(true),
            Command::WriteScanEnable(self.idle_scan()),
        ];
        self.pace(vec![HostAction::Send(Command::Reset)])
    }

    /// Set discoverability directly — used to go quiet while a session is active (#68).
    #[must_use]
    pub fn set_discoverable(&mut self, discoverable: bool) -> Vec<HostAction> {
        let scan = if discoverable {
            self.idle_scan()
        } else {
            // Still connectable: a phone that is already paired must be able to
            // reconnect even while someone else is streaming, or takeover never works.
            ScanEnable::ConnectableOnly
        };
        self.pace(vec![HostAction::Send(Command::WriteScanEnable(scan))])
    }

    /// The inquiry response payload: what we serve, and what to call us.
    ///
    /// The UUID list must stay in step with the records `adapter` publishes over SDP —
    /// [`SERVICE_CLASSES`] says why, and `eir_matches_the_published_sdp_records` fails if
    /// they drift apart.
    fn eir(&self) -> Bytes {
        Eir::new()
            // The list is a three-element constant, so the length check cannot fire; an
            // empty EIR is still the right answer if it somehow did, since a panic here
            // would take down bring-up over a cosmetic field.
            .with_uuids16(&SERVICE_CLASSES)
            .unwrap_or_default()
            .with_name(&self.config.name)
            .finish()
    }

    const fn idle_scan(&self) -> ScanEnable {
        if self.config.discoverable {
            ScanEnable::DiscoverableAndConnectable
        } else {
            ScanEnable::ConnectableOnly
        }
    }

    /// Feed one controller event.
    ///
    /// Commands the controller has no room for are held back here rather than written and
    /// discarded; they go out on their own as the window reopens, which is why one event
    /// can produce commands that have nothing to do with it.
    #[must_use]
    pub fn on_event(&mut self, event: &Event) -> Vec<HostAction> {
        // The window opens *before* the event is handled, because what the event produces
        // is usually the very next command — during bring-up it always is.
        let mut actions = match event {
            Event::CommandComplete {
                opcode,
                allowed_packets,
                ..
            }
            | Event::CommandStatus {
                opcode,
                allowed_packets,
                ..
            } => {
                self.unanswered_for = Duration::ZERO;
                self.commands
                    .answered(*opcode, *allowed_packets)
                    .into_iter()
                    .map(HostAction::Send)
                    .collect()
            }
            _ => Vec::new(),
        };
        let fresh = self.handle(event);
        actions.extend(self.pace(fresh));
        actions
    }

    /// How long until the oldest in-flight command is declared lost, if one is in flight.
    ///
    /// `None` means there is nothing to wait for, and the actor should not arm a timer.
    #[must_use]
    pub fn next_timeout(&self) -> Option<Duration> {
        (self.commands.in_flight() > 0).then(|| COMMAND_TIMEOUT.saturating_sub(self.unanswered_for))
    }

    /// Advance the command watchdog by `elapsed`.
    ///
    /// **This is the whole of the fix for a controller that stops answering.** Bring-up
    /// is a queue advanced only by a completion, so one lost completion — the documented
    /// idle stall on this project's dongle — left the queue stopped with no `Ready`, no
    /// `WriteScanEnable` and nothing in the log: the panel came up, the UI looked
    /// healthy, and the receiver was simply never discoverable over Bluetooth.
    #[must_use]
    pub fn tick(&mut self, elapsed: Duration) -> Vec<HostAction> {
        if self.commands.in_flight() == 0 {
            self.unanswered_for = Duration::ZERO;
            return Vec::new();
        }
        self.unanswered_for = self.unanswered_for.saturating_add(elapsed);
        if self.unanswered_for < COMMAND_TIMEOUT {
            return Vec::new();
        }
        self.unanswered_for = Duration::ZERO;
        let (lost, released) = self.commands.abandon_oldest();
        warn!(
            ?lost,
            state = ?self.state,
            "bluetooth: the controller did not answer a command; giving up on it and moving on"
        );
        let mut actions: Vec<HostAction> = released.into_iter().map(HostAction::Send).collect();
        // The abandoned command was almost certainly bring-up's own — that queue is one
        // deep and only a completion moves it — so the sequence has to be pushed along by
        // hand, or a half-configured controller stays half-configured forever.
        if self.state == HostState::Initializing {
            let next = self.advance_bring_up();
            actions.extend(self.pace(next));
        }
        actions
    }

    /// Take a command slot for every [`HostAction::Send`], holding back what will not fit.
    ///
    /// Every command this host emits goes through here. Most controllers advertise
    /// `Num_HCI_Command_Packets = 1`, and sending on every event put two in flight during
    /// a two-phone connect storm — the second silently discarded, which presents as one
    /// phone stuck in "Connecting…" (#90).
    fn pace(&mut self, actions: Vec<HostAction>) -> Vec<HostAction> {
        actions
            .into_iter()
            .filter_map(|action| match action {
                HostAction::Send(command) => self.commands.submit(command).map(HostAction::Send),
                other => Some(other),
            })
            .collect()
    }

    /// Decide what one event means, before any pacing.
    fn handle(&mut self, event: &Event) -> Vec<HostAction> {
        match event {
            Event::CommandComplete { opcode, params, .. } => {
                self.on_command_complete(*opcode, params)
            }
            Event::CommandStatus { status, opcode, .. } => {
                // Bring-up is a queue with one command in flight, advanced by each
                // completion. A controller that answers one of them with a *status*
                // instead — which is what "unknown HCI command" looks like on some, and
                // what btvirt does for `WriteInquiryScanType` — otherwise stalls that
                // queue forever: no `Ready`, no `WriteScanEnable`, and a receiver nobody
                // can find, with no error anywhere to say why.
                //
                // Every command in the bring-up sequence is a Command Complete command,
                // so a status arriving during initialisation means that one is not going
                // to complete and the next should go out. The optional ones are optional
                // precisely because this can happen.
                if self.state == HostState::Initializing {
                    if !status.is_success() {
                        debug!(%opcode, %status, "controller refused a bring-up command");
                    }
                    return self.advance_bring_up();
                }
                Vec::new()
            }

            // --- connection lifecycle ---
            Event::ConnectionRequest {
                addr, link_type, ..
            } => {
                if *link_type == LinkType::Acl {
                    // Stay peripheral: the phone paged us, and forcing a role switch
                    // mid-pairing is handled badly by more controllers than not.
                    vec![HostAction::Send(Command::AcceptConnectionRequest {
                        addr: *addr,
                        role: AcceptRole::RemainPeripheral,
                    })]
                } else {
                    // SCO is HFP's business. Refusing is correct and keeps the
                    // controller from allocating a synchronous link we never service.
                    vec![HostAction::Send(Command::RejectConnectionRequest {
                        addr: *addr,
                        reason: Status::REJECTED_LIMITED_RESOURCES,
                    })]
                }
            }
            Event::ConnectionComplete {
                status,
                handle,
                addr,
                ..
            } => {
                if status.is_success() {
                    self.connections.insert(handle.raw(), *addr);
                    vec![
                        HostAction::LinkUp {
                            handle: *handle,
                            peer: *addr,
                        },
                        // Ask what the phone calls itself, so the screen can say
                        // "Pixel 8" rather than a MAC. Fire-and-forget: plenty of
                        // senders never answer, and the session must not wait on it.
                        HostAction::Send(Command::RemoteNameRequest(*addr)),
                    ]
                } else {
                    Vec::new()
                }
            }
            Event::DisconnectionComplete { handle, reason, .. } => {
                let peer = self.connections.remove(&handle.raw()).unwrap_or_default();
                vec![HostAction::LinkDown {
                    handle: *handle,
                    peer,
                    reason: *reason,
                }]
            }

            // --- pairing (#68: Just Works, bonded, no prompt) ---
            Event::IoCapabilityRequest(addr) => {
                vec![HostAction::Send(Command::IoCapabilityRequestReply {
                    addr: *addr,
                    // Claiming no input and no output is what *selects* Just Works. Any
                    // other claim makes the controller run numeric comparison and wait
                    // for a confirmation the kiosk has no way to collect.
                    io: IoCapability::NoInputNoOutput,
                    auth: AuthRequirements::GeneralBondingNoMitm,
                })]
            }
            Event::UserConfirmationRequest { addr, .. } => {
                // Auto-accept. With NoInputNoOutput on our side the numeric value is not
                // meant to be shown to anyone, and waiting for a human here is how a
                // kiosk becomes unpairable.
                vec![HostAction::Send(Command::UserConfirmationRequestReply(
                    *addr,
                ))]
            }
            Event::LinkKeyRequest(addr) => self.link_keys.get(addr).map_or_else(
                || {
                    vec![HostAction::Send(Command::LinkKeyRequestNegativeReply(
                        *addr,
                    ))]
                },
                |key| {
                    vec![HostAction::Send(Command::LinkKeyRequestReply {
                        addr: *addr,
                        key: *key,
                    })]
                },
            ),
            Event::LinkKeyNotification { addr, key, .. } => {
                self.link_keys.insert(*addr, *key);
                vec![HostAction::Paired {
                    peer: *addr,
                    key: *key,
                }]
            }
            Event::PinCodeRequest(addr) => {
                // Legacy PIN pairing is refused: it would mean prompting for a number on
                // a device with no keypad, and SSP has been mandatory since 2.1.
                vec![HostAction::Send(Command::PinCodeRequestNegativeReply(
                    *addr,
                ))]
            }

            Event::RemoteNameRequestComplete {
                status, addr, name, ..
            } => {
                // A failed name request is ordinary, not an error: the address is still
                // shown, which is what actually identifies the phone in a room.
                if status.is_success() && !name.trim().is_empty() {
                    vec![HostAction::PeerName {
                        peer: *addr,
                        name: name.trim().to_owned(),
                    }]
                } else {
                    Vec::new()
                }
            }

            Event::AuthenticationComplete { status, handle } => {
                let peer = self.connections.get(&handle.raw()).copied();
                if status.is_success() {
                    debug!(?peer, "bluetooth: authenticated");
                    return Vec::new();
                }
                // A stale link key is the overwhelmingly likely cause: the phone was
                // factory reset, or forgot us, so the key we replied to `LinkKeyRequest`
                // with no longer matches. Keeping it produces a silent connect/authenticate
                // /disconnect loop — the phone tries, fails, tries again — with nothing in
                // the log beyond `link down`, because both events were falling into the
                // wildcard below.
                //
                // Dropping it costs one re-pairing, which is four seconds of someone's
                // attention. Keeping it costs a phone that can never connect again.
                match peer.filter(|addr| self.link_keys.contains_key(addr)) {
                    Some(addr) => {
                        warn!(
                            %addr, ?status,
                            "bluetooth: authentication failed with a stored link key; \
                             forgetting it so the next attempt pairs afresh"
                        );
                        self.link_keys.remove(&addr);
                        vec![HostAction::Unpaired { peer: addr }]
                    }
                    _ => {
                        warn!(?peer, ?status, "bluetooth: authentication failed");
                        Vec::new()
                    }
                }
            }

            Event::EncryptionChange {
                status,
                handle,
                enabled,
            } => {
                let peer = self.connections.get(&handle.raw()).copied();
                if status.is_success() {
                    debug!(?peer, enabled, "bluetooth: encryption changed");
                } else {
                    // Not fatal by itself — the link survives — but it is the difference
                    // between a session that is protected and one that only looks it, and
                    // it was invisible.
                    warn!(?peer, ?status, "bluetooth: encryption change failed");
                }
                Vec::new()
            }

            Event::NumberOfCompletedPackets(pairs) => pairs
                .iter()
                .map(|(handle, count)| HostAction::Credits {
                    handle: *handle,
                    count: *count,
                })
                .collect(),

            _ => Vec::new(),
        }
    }

    fn on_command_complete(
        &mut self,
        opcode: substrate_hci::OpCode,
        params: &[u8],
    ) -> Vec<HostAction> {
        use substrate_hci::OpCode;

        // Record what the informational commands told us before moving on.
        if opcode == OpCode::READ_BUFFER_SIZE {
            if let Ok(bs) = substrate_hci::BufferSize::parse(params) {
                self.acl_mtu = bs.acl_max_len.max(1);
                self.acl_credits = bs.total_packets.max(1);
            }
        } else if opcode == OpCode::READ_BD_ADDR {
            if let Ok(addr) = substrate_hci::event::parse_bd_addr(params) {
                self.address = addr;
            }
        }

        if self.state != HostState::Initializing {
            return Vec::new();
        }
        self.advance_bring_up()
    }

    /// Send the next bring-up command, or declare the controller ready.
    ///
    /// Reached from both a completion and a refusal, because the queue has to move either
    /// way: the sequence is a best effort, and the commands in it that a given controller
    /// does not implement are the ones documented as optional.
    fn advance_bring_up(&mut self) -> Vec<HostAction> {
        if self.pending.is_empty() {
            self.state = HostState::Ready;
            return vec![HostAction::Ready {
                address: self.address,
                acl_credits: self.acl_credits,
                acl_mtu: self.acl_mtu,
            }];
        }
        // One command in flight at a time. Controllers advertise how many they will
        // accept, but the bring-up sequence is order-dependent anyway, so pipelining it
        // buys nothing and risks a WriteScanEnable landing before SSP is on.
        let next = self.pending.remove(0);
        vec![HostAction::Send(next)]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use bytes::Bytes;
    use substrate_hci::{event::code, OpCode};

    use super::*;

    #[test]
    fn a_link_key_the_phone_no_longer_accepts_is_forgotten() {
        // The failure this ends: a phone that was factory reset, or that forgot us, no
        // longer has the key we reply to `LinkKeyRequest` with. Authentication fails, the
        // link drops, the phone tries again — forever — and both events were falling into
        // the wildcard, so the log said nothing beyond `link down`.
        //
        // Forgetting costs one re-pairing, which is four seconds of someone's attention.
        // Keeping it costs a phone that can never connect again.
        let mut host = HostController::new(HostConfig::default());
        let peer = BdAddr::new([1, 2, 3, 4, 5, 6]);
        let handle = ConnectionHandle::new(0x0002).unwrap();
        host.load_link_keys([(peer, LinkKey::new([0xAB; 16]))]);
        let _ = host.on_event(&Event::ConnectionComplete {
            status: Status::SUCCESS,
            handle,
            addr: peer,
            link_type: LinkType::Acl,
            encryption_enabled: false,
        });
        assert!(host.knows(peer), "the key is there to begin with");

        let actions = host.on_event(&Event::AuthenticationComplete {
            status: Status(0x05), // authentication failure
            handle,
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, HostAction::Unpaired { peer: p } if *p == peer)),
            "the caller must be told, or the key survives on disk and the loop \
             outlives a reboot: {actions:?}"
        );
        assert!(!host.knows(peer), "and it is gone from memory too");
    }

    #[test]
    fn a_successful_authentication_keeps_the_key() {
        // The obvious other half: normal reconnection of a bonded phone authenticates
        // successfully every time, and forgetting the key then would make bonding useless.
        let mut host = HostController::new(HostConfig::default());
        let peer = BdAddr::new([1, 2, 3, 4, 5, 6]);
        let handle = ConnectionHandle::new(0x0002).unwrap();
        host.load_link_keys([(peer, LinkKey::new([0xAB; 16]))]);
        let _ = host.on_event(&Event::ConnectionComplete {
            status: Status::SUCCESS,
            handle,
            addr: peer,
            link_type: LinkType::Acl,
            encryption_enabled: false,
        });
        let actions = host.on_event(&Event::AuthenticationComplete {
            status: Status::SUCCESS,
            handle,
        });
        assert!(actions.is_empty(), "nothing to do: {actions:?}");
        assert!(host.knows(peer));
    }

    /// A command-complete for `opcode` with the given return parameters.
    fn complete(opcode: OpCode, params: &[u8]) -> Event {
        complete_granting(opcode, 1, params)
    }

    /// The same, from a controller that will accept `allowed_packets` more commands.
    fn complete_granting(opcode: OpCode, allowed_packets: u8, params: &[u8]) -> Event {
        Event::parse(code::COMMAND_COMPLETE, &{
            let mut v = vec![allowed_packets];
            v.extend_from_slice(&opcode.raw().to_le_bytes());
            v.extend_from_slice(params);
            v
        })
        .unwrap()
    }

    /// The controller answering `command` and granting one more slot.
    ///
    /// Real exchanges are never two commands in a row: the controller answers each, and
    /// the answer is what returns the credit the next one needs (#90). Tests that skipped
    /// it were testing a controller that does not exist.
    fn answer(host: &mut HostController, command: &Command) -> Vec<Command> {
        sent(&host.on_event(&complete(command.opcode(), &[0x00])))
    }

    fn sent(actions: &[HostAction]) -> Vec<Command> {
        actions
            .iter()
            .filter_map(|a| match a {
                HostAction::Send(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    /// Run bring-up to completion, returning every command in order.
    fn bring_up(host: &mut HostController) -> Vec<Command> {
        let mut all = sent(&host.start());
        let mut last = all.last().cloned().unwrap();
        for _ in 0..32 {
            let params: &[u8] = match last.opcode() {
                OpCode::READ_BUFFER_SIZE => &[0x00, 0x54, 0x01, 0xff, 0x08, 0x00, 0x08, 0x00],
                OpCode::READ_BD_ADDR => &[0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa],
                _ => &[0x00],
            };
            let actions = host.on_event(&complete(last.opcode(), params));
            let next = sent(&actions);
            if next.is_empty() {
                break;
            }
            all.extend(next.clone());
            last = next.last().cloned().unwrap();
        }
        all
    }

    #[test]
    fn a_controller_that_refuses_an_optional_command_still_becomes_discoverable() {
        // Found on the virtual bench in five seconds: btvirt answers
        // `WriteInquiryScanType` with a *command status* of "unknown HCI command" rather
        // than a completion, and bring-up stopped dead there — no Ready, no
        // WriteScanEnable, and a receiver nobody can find, with nothing in the log to say
        // why. Interlaced scan is documented as optional precisely because a controller
        // may not have it; optional has to mean the queue keeps moving.
        let mut host = HostController::new(HostConfig::default());
        let mut all = sent(&host.start());
        let mut last = all.last().cloned().unwrap();
        for _ in 0..32 {
            let refuse = last.opcode() == OpCode::WRITE_INQUIRY_SCAN_TYPE;
            let actions = if refuse {
                host.on_event(&Event::CommandStatus {
                    status: Status(0x01), // unknown HCI command
                    allowed_packets: 1,
                    opcode: last.opcode(),
                })
            } else {
                let params: &[u8] = match last.opcode() {
                    OpCode::READ_BUFFER_SIZE => &[0x00, 0x54, 0x01, 0xff, 0x08, 0x00, 0x08, 0x00],
                    OpCode::READ_BD_ADDR => &[0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa],
                    _ => &[0x00],
                };
                host.on_event(&complete(last.opcode(), params))
            };
            let next = sent(&actions);
            if next.is_empty() {
                break;
            }
            all.extend(next.clone());
            last = next.last().cloned().unwrap();
        }

        let opcodes: Vec<OpCode> = all.iter().map(Command::opcode).collect();
        assert!(
            opcodes.contains(&OpCode::WRITE_SCAN_ENABLE),
            "a refusal must not swallow the rest of the sequence: {opcodes:?}"
        );
        assert_eq!(host.state(), HostState::Ready);
    }

    #[test]
    fn bring_up_resets_first_and_enables_ssp_before_becoming_discoverable() {
        // Order is load-bearing twice over. Reset first, or a controller left configured
        // by a previous run answers with stale state. SSP before scan enable, or a phone
        // that pages us in the gap gets legacy PIN pairing — which we refuse, so it
        // fails to connect for reasons no log explains.
        let mut host = HostController::new(HostConfig::default());
        let commands = bring_up(&mut host);
        let opcodes: Vec<OpCode> = commands.iter().map(Command::opcode).collect();

        assert_eq!(opcodes.first(), Some(&OpCode::RESET));
        let ssp = opcodes
            .iter()
            .position(|o| *o == OpCode::WRITE_SIMPLE_PAIRING_MODE)
            .expect("ssp must be enabled");
        let scan = opcodes
            .iter()
            .position(|o| *o == OpCode::WRITE_SCAN_ENABLE)
            .expect("must become discoverable");
        assert!(ssp < scan, "SSP must be on before we answer inquiries");
        assert_eq!(host.state(), HostState::Ready);
    }

    #[test]
    fn bring_up_learns_the_controllers_address_and_buffer_geometry() {
        // The ACL numbers *are* transmit flow control: send more than the controller
        // will hold and it drops fragments silently, which presents as audio that
        // stutters under load with nothing in any log.
        let mut host = HostController::new(HostConfig::default());
        bring_up(&mut host);
        assert_eq!(host.address().to_string(), "AA:BB:CC:DD:EE:FF");
        assert_eq!(host.acl_mtu(), 340);
    }

    #[test]
    fn an_incoming_acl_connection_is_accepted_as_peripheral() {
        // Forcing a role switch mid-pairing is handled badly by more controllers than
        // not, and the phone paged us, so peripheral is the natural role.
        let mut host = HostController::new(HostConfig::default());
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let actions = host.on_event(&Event::ConnectionRequest {
            addr,
            class_of_device: 0x5A_020C,
            link_type: LinkType::Acl,
        });
        assert_eq!(
            sent(&actions),
            vec![Command::AcceptConnectionRequest {
                addr,
                role: AcceptRole::RemainPeripheral,
            }]
        );
    }

    #[test]
    fn a_sco_connection_request_is_refused() {
        // SCO is HFP's business. Accepting allocates a synchronous link we never
        // service, which some controllers then refuse to tear down.
        let mut host = HostController::new(HostConfig::default());
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let actions = host.on_event(&Event::ConnectionRequest {
            addr,
            class_of_device: 0,
            link_type: LinkType::Sco,
        });
        assert!(matches!(
            sent(&actions).first(),
            Some(Command::RejectConnectionRequest { .. })
        ));
    }

    #[test]
    fn pairing_is_just_works_with_no_prompt_on_either_side() {
        // #68's decision, made testable. Claiming NoInputNoOutput is what *selects*
        // Just Works; any other claim makes the controller run numeric comparison and
        // wait for a confirmation a kiosk has no way to collect.
        let mut host = HostController::new(HostConfig::default());
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();

        let io = sent(&host.on_event(&Event::IoCapabilityRequest(addr)));
        assert_eq!(
            io,
            vec![Command::IoCapabilityRequestReply {
                addr,
                io: IoCapability::NoInputNoOutput,
                auth: AuthRequirements::GeneralBondingNoMitm,
            }]
        );
        assert!(answer(&mut host, &io[0]).is_empty());

        let confirm = sent(&host.on_event(&Event::UserConfirmationRequest {
            addr,
            numeric_value: 123_456,
        }));
        assert_eq!(
            confirm,
            vec![Command::UserConfirmationRequestReply(addr)],
            "auto-accept, or the kiosk is unpairable"
        );
    }

    #[test]
    fn legacy_pin_pairing_is_refused() {
        // It would mean prompting for a number on a device with no keypad.
        let mut host = HostController::new(HostConfig::default());
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        assert_eq!(
            sent(&host.on_event(&Event::PinCodeRequest(addr))),
            vec![Command::PinCodeRequestNegativeReply(addr)]
        );
    }

    #[test]
    fn a_returning_guest_reconnects_with_the_stored_key_and_a_new_one_pairs() {
        // The bonding half of #68: keys persist, so a repeat visitor never sees a
        // pairing prompt again.
        let mut host = HostController::new(HostConfig::default());
        let known: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let stranger: BdAddr = "11:22:33:44:55:66".parse().unwrap();
        let key = LinkKey::new([0xAB; 16]);
        host.load_link_keys([(known, key)]);

        assert!(host.knows(known));
        let reply = sent(&host.on_event(&Event::LinkKeyRequest(known)));
        assert_eq!(
            reply,
            vec![Command::LinkKeyRequestReply { addr: known, key }]
        );
        assert!(answer(&mut host, &reply[0]).is_empty());
        assert_eq!(
            sent(&host.on_event(&Event::LinkKeyRequest(stranger))),
            vec![Command::LinkKeyRequestNegativeReply(stranger)],
            "an unknown peer must be told to pair fresh"
        );
    }

    #[test]
    fn a_new_link_key_is_surfaced_for_persistence_and_used_immediately() {
        let mut host = HostController::new(HostConfig::default());
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let key = LinkKey::new([0x11; 16]);
        let actions = host.on_event(&Event::LinkKeyNotification {
            addr,
            key,
            key_type: 0x04,
        });
        assert_eq!(actions, vec![HostAction::Paired { peer: addr, key }]);
        // …and it is live in this session, not only after a restart.
        assert!(host.knows(addr));
    }

    #[test]
    fn a_new_link_triggers_a_name_request_so_the_screen_can_say_who() {
        // A MAC identifies a phone but nobody reads one. The request is fire-and-forget
        // because plenty of senders never answer and the session must not wait.
        let mut host = HostController::new(HostConfig::default());
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let actions = host.on_event(&Event::ConnectionComplete {
            status: Status::SUCCESS,
            handle: ConnectionHandle::new(0x0b).unwrap(),
            addr,
            link_type: LinkType::Acl,
            encryption_enabled: false,
        });
        assert!(actions
            .iter()
            .any(|a| matches!(a, HostAction::LinkUp { .. })));
        assert_eq!(sent(&actions), vec![Command::RemoteNameRequest(addr)]);
    }

    #[test]
    fn a_name_that_arrives_is_surfaced_and_one_that_fails_is_not_an_error() {
        let mut host = HostController::new(HostConfig::default());
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();

        let named = host.on_event(&Event::RemoteNameRequestComplete {
            status: Status::SUCCESS,
            addr,
            name: "Pixel 8".to_owned(),
        });
        assert_eq!(
            named,
            vec![HostAction::PeerName {
                peer: addr,
                name: "Pixel 8".to_owned()
            }]
        );

        // Failure is ordinary: the address still identifies the phone.
        let failed = host.on_event(&Event::RemoteNameRequestComplete {
            status: Status::PAGE_TIMEOUT,
            addr,
            name: String::new(),
        });
        assert!(failed.is_empty());

        // …as is a sender that answers with nothing.
        let blank = host.on_event(&Event::RemoteNameRequestComplete {
            status: Status::SUCCESS,
            addr,
            name: "   ".to_owned(),
        });
        assert!(blank.is_empty());
    }

    #[test]
    fn a_link_that_drops_names_its_peer_and_its_reason() {
        let mut host = HostController::new(HostConfig::default());
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let handle = ConnectionHandle::new(0x0b).unwrap();
        let _ = host.on_event(&Event::ConnectionComplete {
            status: Status::SUCCESS,
            handle,
            addr,
            link_type: LinkType::Acl,
            encryption_enabled: false,
        });
        let actions = host.on_event(&Event::DisconnectionComplete {
            status: Status::SUCCESS,
            handle,
            reason: Status::REMOTE_USER_TERMINATED,
        });
        assert_eq!(
            actions,
            vec![HostAction::LinkDown {
                handle,
                peer: addr,
                reason: Status::REMOTE_USER_TERMINATED,
            }]
        );
    }

    #[test]
    fn a_failed_connection_does_not_report_a_link() {
        let mut host = HostController::new(HostConfig::default());
        let actions = host.on_event(&Event::ConnectionComplete {
            status: Status::PAGE_TIMEOUT,
            handle: ConnectionHandle::new(0).unwrap(),
            addr: "AA:BB:CC:DD:EE:FF".parse().unwrap(),
            link_type: LinkType::Acl,
            encryption_enabled: false,
        });
        assert!(actions.is_empty());
    }

    #[test]
    fn going_quiet_for_a_session_stays_connectable() {
        // Undiscoverable, but a phone that already paired must still be able to
        // reconnect while someone else streams — otherwise takeover never works (#68).
        let mut host = HostController::new(HostConfig::default());
        let quiet = sent(&host.set_discoverable(false));
        assert_eq!(
            quiet,
            vec![Command::WriteScanEnable(ScanEnable::ConnectableOnly)]
        );
        assert!(answer(&mut host, &quiet[0]).is_empty());
        assert_eq!(
            sent(&host.set_discoverable(true)),
            vec![Command::WriteScanEnable(
                ScanEnable::DiscoverableAndConnectable
            )]
        );
    }

    #[test]
    fn bring_up_scans_hard_enough_to_be_found_while_streaming() {
        // #68 as amended: the receiver stays findable while it is in use, which the
        // controller defaults will not do — an 11.25 ms window every 1.28 s loses to an
        // active A2DP link, and the box silently disappears from every scan list.
        let mut host = HostController::new(HostConfig::default());
        let sent = bring_up(&mut host);
        assert!(
            sent.iter().any(|c| matches!(
                c,
                Command::WriteInquiryScanActivity { interval, window }
                    if *window > 0x0012 && window <= interval
            )),
            "inquiry scan must be widened past the default: {sent:?}"
        );
        assert!(
            sent.contains(&Command::WriteInquiryScanType(true)),
            "interlaced scan halves discovery latency"
        );
        // …and it must be configured before scanning is switched on, or the first
        // inquiries are answered with the defaults we are trying to leave behind.
        let activity = sent
            .iter()
            .position(|c| matches!(c, Command::WriteInquiryScanActivity { .. }));
        let enable = sent
            .iter()
            .position(|c| matches!(c, Command::WriteScanEnable(_)));
        assert!(activity < enable, "activity must precede scan enable");
    }

    #[test]
    fn bring_up_publishes_a_name_in_the_inquiry_response() {
        // The failure this ends: `WriteLocalName` only furnishes an answer to a separate
        // RemoteNameRequest. BlueZ sends one, so a Linux laptop saw the panel and every
        // test passed; Android builds its picker from the inquiry response alone and saw
        // an unnamed entry it filtered out. Discoverable, findable by radio, invisible in
        // the UI — and nothing in any log to say so.
        let mut host = HostController::new(HostConfig::default());
        let sent = bring_up(&mut host);
        let eir = sent
            .iter()
            .find_map(|c| match c {
                Command::WriteExtendedInquiryResponse { data, .. } => Some(data.clone()),
                _ => None,
            })
            .expect("bring-up must write an EIR");
        // The name is in there as a complete-local-name structure, not merely somewhere.
        let name = HostConfig::default().name;
        let expected = [
            &[u8::try_from(name.len() + 1).unwrap(), 0x09][..],
            name.as_bytes(),
        ]
        .concat();
        assert!(
            eir.windows(expected.len()).any(|w| w == expected),
            "EIR must carry the complete local name: {eir:?}"
        );
    }

    #[test]
    fn the_inquiry_response_matches_the_published_sdp_records() {
        // The EIR is a claim a peer may act on without ever querying SDP, so a class
        // advertised here that no record backs invites a connection we then refuse.
        // These are the records `BluetoothAdapter::new` registers.
        use substrate_sdp::{
            record::{a2dp_sink, avrcp_controller, avrcp_target},
            Uuid,
        };
        let records = [
            a2dp_sink(1, "x"),
            avrcp_controller(2, "x"),
            avrcp_target(3, "x"),
        ];
        for class in SERVICE_CLASSES {
            assert!(
                records.iter().any(|r| r.has_class(Uuid::short(class))),
                "EIR advertises {class:#06x}, which no published record backs"
            );
        }
    }

    #[test]
    fn a_controller_that_stops_answering_does_not_stop_bring_up_forever() {
        // #90, and the reason it was invisible: bring-up is a queue advanced only by a
        // completion, so the documented idle stall on this dongle left it stopped with no
        // `Ready`, no `WriteScanEnable` and nothing in the log. The panel came up, the UI
        // looked healthy, and the receiver was simply never discoverable.
        let mut host = HostController::new(HostConfig::default());
        let first = sent(&host.start());
        assert_eq!(first, vec![Command::Reset]);

        // Nothing answers. Short of the deadline, nothing happens — a controller busy
        // with the radio is not written off.
        assert!(host.tick(Duration::from_secs(4)).is_empty());
        assert_eq!(host.state(), HostState::Initializing);
        assert!(
            host.next_timeout().is_some(),
            "a command is in flight, so there is a deadline to arm a timer on"
        );

        let moved_on = sent(&host.tick(Duration::from_secs(2)));
        assert_eq!(
            moved_on,
            vec![Command::ReadLocalVersion],
            "the queue moves to the next command rather than waiting forever"
        );
        // And nothing is re-sent: a merely late completion would otherwise run twice.
        assert!(!moved_on.contains(&Command::Reset));
    }

    #[test]
    fn bring_up_reaches_ready_across_a_lost_completion() {
        // The observable half: the box ends up discoverable even though the controller
        // swallowed an answer along the way.
        let mut host = HostController::new(HostConfig::default());
        let mut last = sent(&host.start()).pop().unwrap();
        let mut ready = false;
        let mut swallowed_one = false;
        for _ in 0..64 {
            // Swallow exactly one completion, part way through the sequence.
            let actions = if last.opcode() == OpCode::WRITE_LOCAL_NAME && !swallowed_one {
                swallowed_one = true;
                host.tick(COMMAND_TIMEOUT)
            } else {
                let params: &[u8] = match last.opcode() {
                    OpCode::READ_BUFFER_SIZE => &[0x00, 0x54, 0x01, 0xff, 0x08, 0x00, 0x08, 0x00],
                    OpCode::READ_BD_ADDR => &[0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa],
                    _ => &[0x00],
                };
                host.on_event(&complete(last.opcode(), params))
            };
            ready |= actions
                .iter()
                .any(|a| matches!(a, HostAction::Ready { .. }));
            if ready {
                break;
            }
            let Some(next) = sent(&actions).pop() else {
                panic!("bring-up produced nothing after {last:?}")
            };
            last = next;
        }
        assert!(swallowed_one, "the test must actually lose a completion");
        assert!(ready, "bring-up must still reach Ready");
        assert_eq!(host.state(), HostState::Ready);
    }

    #[test]
    fn there_is_no_deadline_when_nothing_is_in_flight() {
        // The actor arms its timer off this, and a deadline with nothing behind it would
        // wake the loop every five seconds for the life of the process.
        let mut started = HostController::new(HostConfig::default());
        assert_eq!(started.next_timeout(), None, "nothing has been sent yet");
        let _ = started.start();
        assert!(
            started.next_timeout().is_some(),
            "the Reset is in flight and may be lost"
        );

        let mut host = HostController::new(HostConfig::default());
        assert!(!bring_up(&mut host).is_empty());
        assert_eq!(host.state(), HostState::Ready);
        assert_eq!(
            host.next_timeout(),
            None,
            "every command was answered, so there is nothing left to time out"
        );
    }

    #[test]
    fn two_events_in_a_row_do_not_put_two_commands_in_flight() {
        // SUSPECTED at runtime as a phone hanging in "Connecting…" during a two-phone
        // connect storm: most controllers advertise one command packet, and the second
        // command was written into a slot that did not exist and discarded (#90).
        let mut host = HostController::new(HostConfig::default());
        let a: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let b: BdAddr = "11:22:33:44:55:66".parse().unwrap();

        let first = sent(&host.on_event(&Event::IoCapabilityRequest(a)));
        assert_eq!(first.len(), 1);
        let second = sent(&host.on_event(&Event::IoCapabilityRequest(b)));
        assert!(
            second.is_empty(),
            "the second reply must wait for a slot, not be written into none: {second:?}"
        );

        // The controller answers the first, which is what makes room for the second.
        let released = sent(&host.on_event(&complete(first[0].opcode(), &[0x00])));
        assert_eq!(
            released,
            vec![Command::IoCapabilityRequestReply {
                addr: b,
                io: IoCapability::NoInputNoOutput,
                auth: AuthRequirements::GeneralBondingNoMitm,
            }],
            "held back, not dropped: it is a reply the phone is waiting for"
        );
    }

    #[test]
    fn a_wider_window_is_believed() {
        // A controller entitled to more than one in flight should get it; the pacing is
        // there to respect the limit, not to impose one of our own.
        let mut host = HostController::new(HostConfig::default());
        let a: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let b: BdAddr = "11:22:33:44:55:66".parse().unwrap();
        // An unsolicited Command Complete granting two, which is how a controller says so.
        let _ = host.on_event(&complete_granting(OpCode::new(0x0000), 2, &[]));
        let _ = host.on_event(&Event::LinkKeyRequest(a));
        assert_eq!(
            sent(&host.on_event(&Event::LinkKeyRequest(b))).len(),
            1,
            "with two credits, both replies go straight out"
        );
    }

    #[test]
    fn completed_packets_are_reported_per_link_as_credits() {
        let mut host = HostController::new(HostConfig::default());
        let a = ConnectionHandle::new(0x0a).unwrap();
        let b = ConnectionHandle::new(0x0b).unwrap();
        let actions = host.on_event(&Event::NumberOfCompletedPackets(vec![(a, 5), (b, 3)]));
        assert_eq!(
            actions,
            vec![
                HostAction::Credits {
                    handle: a,
                    count: 5
                },
                HostAction::Credits {
                    handle: b,
                    count: 3
                },
            ]
        );
    }

    #[test]
    fn an_unmodelled_event_produces_no_action() {
        let mut host = HostController::new(HostConfig::default());
        let noise = Event::Unhandled {
            code: 0x1b,
            params: Bytes::from_static(&[0x0b, 0x00, 0x05]),
        };
        assert!(host.on_event(&noise).is_empty());
    }
}
