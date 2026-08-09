//! The UDC socket, and the passcode that appears on the glass.
//!
//! The actor half of [`crate::udc`] (ground rule 3): it owns UDP 5550, feeds bytes to the
//! pure core, and turns what comes back into two things — something on the screen, and a
//! request for the commissioning worker.
//!
//! ## The passcode is generated here, not by the phone
//!
//! Matter Casting has two ways to agree a passcode. In the first the *client* shows one
//! and the user types it into the TV, which needs a keyboard the panel does not have. In
//! the second — `commissionerPasscode`, the one Amazon's senders use — the **player**
//! generates it, puts it on its own screen, and the user types it into the phone. That is
//! the flow this panel implements, and it is the only one that fits a device whose input
//! is a person looking at it.
//!
//! The sequence is therefore two round trips with a person in the middle:
//!
//! 1. Phone sends an `IdentificationDeclaration` with `commissionerPasscode` set.
//! 2. Panel generates a passcode, puts it on screen, and answers with a
//!    `CommissionerDeclaration` saying the dialog is up.
//! 3. The user types it into the phone. The phone starts advertising itself as a
//!    commissionable node with a verifier derived from that passcode, and sends a second
//!    declaration with `commissionerPasscodeReady`.
//! 4. Only then does the panel go looking for it and run PASE.

use std::net::SocketAddr;
use std::time::Duration;

use rand_core::RngCore;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::error::{MatterError, UdcError};
use crate::player::Catalogue;
use crate::udc::{
    CdError, CommissionerDeclaration, IdentificationDeclaration, InstanceName, UdcRequest, UDC_PORT,
};

/// How long a displayed passcode is good for.
///
/// The spec's own commissioning window is fifteen minutes. This is shorter because the
/// passcode is *on a screen in a shared room*: the risk it protects against is somebody
/// who can see the panel but was not invited, and every extra minute is another minute
/// that number is readable from the sofa.
pub const PASSCODE_LIFETIME: Duration = Duration::from_secs(180);

/// Matter setup passcodes are eight digits, minus a list the spec forbids.
const PASSCODE_MIN: u32 = 1;
const PASSCODE_MAX: u32 = 99_999_998;

/// Passcodes the Matter spec explicitly disallows: the trivially guessable ones, plus the
/// two reserved values. Core spec §5.1.7.1.
const FORBIDDEN_PASSCODES: [u32; 12] = [
    0, 11_111_111, 22_222_222, 33_333_333, 44_444_444, 55_555_555, 66_666_666, 77_777_777,
    88_888_888, 99_999_999, 12_345_678, 87_654_321,
];

/// What the panel should be showing, and what it should stop showing.
///
/// The screen is not this module's to draw, so it says what it wants and the adapter puts
/// it somewhere a person can see.
///
/// Every variant names the phone it is about. The screen is one slot, and a `Clear` that
/// cannot say *whose* prompt it means is a `Clear` that takes down whatever is showing —
/// which, when two phones asked at once, was the other phone's passcode (#209).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// A phone is asking to cast. Show this passcode until told otherwise.
    Passcode {
        /// The phone this passcode belongs to — the key a later [`Prompt::Clear`] must
        /// match before it may take the number down.
        instance: InstanceName,
        /// Who is asking, as they named themselves.
        device: String,
        /// The number to display, already formatted with the spec's grouping.
        passcode: String,
    },
    /// Take this phone's prompt down: the user dismissed it on the phone, it expired, or
    /// commissioning finished. A `Clear` for a phone whose prompt is not on the glass is
    /// a no-op, never a takedown of somebody else's number.
    Clear {
        /// Whose prompt to take down.
        instance: InstanceName,
    },
}

/// A phone that has typed the passcode and is now waiting to be commissioned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommissionRequest {
    /// The commissionable instance name to look for on mDNS.
    pub instance: InstanceName,
    /// The passcode the panel displayed, which is now the PASE secret.
    pub passcode: u32,
    /// What the phone called itself.
    pub device_name: String,
    /// Where its UDC message came from, for the log.
    pub source: SocketAddr,
    /// Where to send the *outcome*, once there is one — the client's `cdPort` at the
    /// address the declaration came from, or `None` if it named no port.
    ///
    /// Carried on the request because the attempt outlives the datagram that started it
    /// by up to a minute, and by then the socket half has long since answered and moved
    /// on (#198).
    pub reply_to: Option<SocketAddr>,
}

/// A `CommissionerDeclaration` to send after the fact.
///
/// The outcome of a commissioning attempt is decided long after the datagram that asked
/// for it was answered, and the socket belongs to [`UdcServer`]. So the worker says what
/// to send and to whom, and the socket loop sends it — rather than opening a second socket
/// and replying to the client from a source port it never spoke to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The client's `cdPort`, at the address its declaration came from.
    pub to: SocketAddr,
    /// What to tell it.
    pub declaration: CommissionerDeclaration,
}

/// The worker's end-of-attempt report: free the pairing slot, and optionally tell the
/// client how it went.
///
/// Every attempt ends in one of these, success or failure — the slot the attempt held
/// ([`UdcState::finished`]) has to come free either way, or the panel refuses phones
/// forever on the strength of an attempt that is long over (#209).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptEnd {
    /// Whose attempt ended.
    pub instance: InstanceName,
    /// What to send, when the attempt failed and the client left a return address
    /// (#198). A success sends nothing: the panel appearing on the client's fabric is
    /// the answer.
    pub outcome: Option<Outcome>,
}

/// The one pairing the panel runs at a time (#209).
///
/// A single slot rather than a map, on purpose: the OSD is one line of glass and the
/// commissioning worker is one loop, so two concurrent flows were never actually
/// *served* — they were interleaved onto one screen and one queue, and each phone's
/// cancel or expiry tore down whatever the other had up. Making the slot a type is what
/// makes "a second phone while the first is pairing" a state the compiler insists gets
/// an explicit answer.
#[derive(Debug, Default)]
enum Flow {
    /// Nobody is pairing.
    #[default]
    Idle,
    /// A passcode is on the glass, waiting for a person to type it.
    Displayed {
        instance: InstanceName,
        passcode: u32,
        device_name: String,
        issued: tokio::time::Instant,
    },
    /// The worker is running discovery, PASE and CASE against the phone. The slot stays
    /// occupied until [`UdcState::finished`]: the prompt is still that phone's (a user
    /// who mistyped re-reads it), and inviting a second phone onto the screen while the
    /// first is mid-handshake behind it is the interleaving this type exists to prevent.
    Commissioning { instance: InstanceName },
}

/// The pure state machine behind the socket: declarations in, decisions out.
///
/// Separated from the socket so the whole passcode lifecycle — issue, refuse-busy,
/// expire, cancel, redeem, finish — is testable without a network (ground rule 3).
#[derive(Debug, Default)]
pub struct UdcState {
    flow: Flow,
}

/// What the server should do about a declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The reply to send, if the client asked for one.
    pub reply: CommissionerDeclaration,
    /// A phone whose displayed passcode lapsed on the way past this datagram, noticed
    /// here because the datagram beat the expiry timer to it. Its prompt comes down
    /// *before* `prompt` goes up — they can belong to different phones.
    pub expired: Option<InstanceName>,
    /// What to put on (or take off) the screen.
    pub prompt: Option<Prompt>,
    /// A phone to commission, once the reply is away.
    pub commission: Option<CommissionRequest>,
}

impl UdcState {
    /// A server with nothing pending.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide what a declaration means.
    ///
    /// `now` is passed in rather than read, so expiry is testable without sleeping.
    pub fn handle(
        &mut self,
        id: &IdentificationDeclaration,
        source: SocketAddr,
        catalogue: &Catalogue,
        rand: &mut impl RngCore,
        now: tokio::time::Instant,
    ) -> Decision {
        // Expiry first, and *reported*: a datagram can beat the expiry timer to a lapsed
        // passcode, and an entry dropped silently here is a prompt the timer will then
        // never find to take down. It also frees the slot, so a second phone arriving
        // just after the first's window closed is served rather than refused.
        let expired = self.expire(now);

        match id.request() {
            UdcRequest::Cancel => {
                let prompt = match &self.flow {
                    Flow::Displayed { instance, .. } if *instance == id.instance_name => {
                        self.flow = Flow::Idle;
                        Some(Prompt::Clear {
                            instance: id.instance_name.clone(),
                        })
                    }
                    // Mid-commissioning: the user backed out on the phone. The number
                    // comes off the glass, but the slot stays occupied until the worker
                    // reports — the attempt is already running and nothing here can
                    // un-run it.
                    Flow::Commissioning { instance } if *instance == id.instance_name => {
                        Some(Prompt::Clear {
                            instance: id.instance_name.clone(),
                        })
                    }
                    // Somebody else's prompt is not this phone's to take down (#209).
                    // The cancel is still acknowledged — cancelling nothing is
                    // idempotent, and the spec's own reply for it carries no error.
                    Flow::Idle | Flow::Displayed { .. } | Flow::Commissioning { .. } => None,
                };

                Decision {
                    reply: CommissionerDeclaration {
                        cancel_passcode: true,
                        ..CommissionerDeclaration::default()
                    },
                    expired,
                    prompt,
                    commission: None,
                }
            }

            UdcRequest::PasscodeReady => match std::mem::take(&mut self.flow) {
                Flow::Displayed {
                    instance,
                    passcode,
                    device_name,
                    issued: _,
                } if instance == id.instance_name => {
                    self.flow = Flow::Commissioning {
                        instance: instance.clone(),
                    };
                    Decision {
                        reply: CommissionerDeclaration::default(),
                        expired,
                        // The prompt stays up: the passcode is still the shared secret
                        // until PASE completes, and taking it down the instant the phone
                        // claims to have it would leave a user who mistyped with nothing
                        // to re-read.
                        prompt: None,
                        commission: Some(CommissionRequest {
                            instance,
                            passcode,
                            device_name,
                            source,
                            reply_to: id
                                .reply_port()
                                .map(|port| SocketAddr::new(source.ip(), port)),
                        }),
                    }
                }
                // A retransmit: the client sends five copies of the ready declaration
                // too, and the first one already started the worker. Acknowledged, not
                // answered with an error a phone would show its user mid-pairing.
                Flow::Commissioning { instance } if instance == id.instance_name => {
                    self.flow = Flow::Commissioning { instance };
                    Decision {
                        reply: CommissionerDeclaration::default(),
                        expired,
                        prompt: None,
                        commission: None,
                    }
                }
                // A phone claiming a passcode we never issued — a retransmit after the
                // window closed, most likely. The spec has a code for exactly this.
                other => {
                    self.flow = other;
                    Decision {
                        reply: CommissionerDeclaration {
                            error_code: CdError::UnexpectedCommissionerPasscodeReady,
                            ..CommissionerDeclaration::default()
                        },
                        expired,
                        prompt: None,
                        commission: None,
                    }
                }
            },

            UdcRequest::Commission => {
                if !id.commissioner_passcode {
                    // The client wants to show its own passcode for someone to type into
                    // the panel. There is no keyboard here, so this is declined in the
                    // spec's own words rather than by silence (D32).
                    return Decision {
                        reply: CommissionerDeclaration {
                            error_code: CdError::CommissionerPasscodeNotSupported,
                            needs_passcode: true,
                            ..CommissionerDeclaration::default()
                        },
                        expired,
                        prompt: None,
                        commission: None,
                    };
                }

                if !catalogue.hosts_any(&id.target_apps) {
                    // Say so *before* a passcode goes on the screen: a user who types one
                    // and then learns the app is missing has been made to do work for
                    // nothing.
                    return Decision {
                        reply: CommissionerDeclaration {
                            no_apps_found: true,
                            ..CommissionerDeclaration::default()
                        },
                        expired,
                        prompt: None,
                        commission: None,
                    };
                }

                let (passcode, device_name) = match &self.flow {
                    // A declaration that is already displayed gets the number it already
                    // got. The client sends five copies of every message and there is
                    // nothing in them to tell a retransmit from a second attempt, so
                    // generating a fresh passcode per datagram would change the number on
                    // the screen four times while somebody was reading it — and
                    // invalidate the one they had already started typing. The issue time
                    // is not refreshed either: the window starts when the passcode was
                    // first shown, not at the last retransmit.
                    Flow::Displayed {
                        instance,
                        passcode,
                        device_name,
                        ..
                    } if *instance == id.instance_name => (*passcode, device_name.clone()),

                    // The slot is somebody else's — or this phone's own attempt is still
                    // running, in which case a *fresh* request means it gave up and will
                    // be back once the worker's verdict frees the slot. Refused, not
                    // queued, and not silently (#209): the spec's `CdError` list has no
                    // "busy" code, which reads as one passcode dialog at a time being
                    // the expected shape. Of the codes that exist, 17 — "supported but
                    // switched off" — is the honest one: temporary unavailability,
                    // rather than 11's permanent `NotSupported`, which could push a
                    // client into the client-generated flow this panel also declines.
                    Flow::Displayed { .. } | Flow::Commissioning { .. } => {
                        return Decision {
                            reply: CommissionerDeclaration {
                                error_code: CdError::CommissionerPasscodeDisabled,
                                ..CommissionerDeclaration::default()
                            },
                            expired,
                            prompt: None,
                            commission: None,
                        };
                    }

                    Flow::Idle => {
                        let passcode = generate_passcode(rand);
                        let device_name = id
                            .device_name
                            .clone()
                            .unwrap_or_else(|| "a device".to_string());

                        self.flow = Flow::Displayed {
                            instance: id.instance_name.clone(),
                            passcode,
                            device_name: device_name.clone(),
                            issued: now,
                        };

                        (passcode, device_name)
                    }
                };

                Decision {
                    reply: CommissionerDeclaration {
                        passcode_dialog_displayed: true,
                        commissioner_passcode: true,
                        // Eight digits, said out loud so a client can size its input field.
                        passcode_length: 8,
                        ..CommissionerDeclaration::default()
                    },
                    expired,
                    prompt: Some(Prompt::Passcode {
                        instance: id.instance_name.clone(),
                        device: device_name,
                        passcode: format_passcode(passcode),
                    }),
                    commission: None,
                }
            }
        }
    }

    /// Drop a displayed passcode past its lifetime, freeing the slot. Returns whose it
    /// was, which is the panel's cue to take that prompt — and only that prompt — down.
    pub fn expire(&mut self, now: tokio::time::Instant) -> Option<InstanceName> {
        if let Flow::Displayed {
            instance, issued, ..
        } = &self.flow
        {
            if now.duration_since(*issued) >= PASSCODE_LIFETIME {
                let instance = instance.clone();
                self.flow = Flow::Idle;
                return Some(instance);
            }
        }
        None
    }

    /// The worker finished the attempt for `instance`, however it went: free the slot.
    ///
    /// Keyed for the same reason [`Prompt::Clear`] is — a report can only free the slot
    /// its own attempt occupied. With the single-slot policy there is only ever one
    /// attempt in flight, so the key is a cross-check rather than a router.
    pub fn finished(&mut self, instance: &InstanceName) {
        if matches!(&self.flow, Flow::Commissioning { instance: current } if current == instance) {
            self.flow = Flow::Idle;
        }
    }

    /// When the displayed passcode falls out of its window, or `None` if none is up.
    ///
    /// This is what makes expiry an *event* rather than something noticed the next time a
    /// datagram happens to arrive. A phone that sends one declaration and walks away sends
    /// no more datagrams by definition, which is exactly the case where the number is left
    /// on a wall-mounted panel with nobody watching.
    #[must_use]
    pub fn next_expiry(&self) -> Option<tokio::time::Instant> {
        match &self.flow {
            Flow::Displayed { issued, .. } => Some(*issued + PASSCODE_LIFETIME),
            // A commissioning attempt has no display deadline: its bound is the worker's
            // own discovery timeout, and its end is reported, not timed out here.
            Flow::Idle | Flow::Commissioning { .. } => None,
        }
    }

    /// The phone whose passcode is on the glass, if any.
    #[must_use]
    pub fn displayed(&self) -> Option<&InstanceName> {
        match &self.flow {
            Flow::Displayed { instance, .. } => Some(instance),
            Flow::Idle | Flow::Commissioning { .. } => None,
        }
    }
}

/// A Matter setup passcode: eight digits, avoiding the values the spec forbids.
fn generate_passcode(rand: &mut impl RngCore) -> u32 {
    loop {
        let candidate = PASSCODE_MIN + rand.next_u32() % (PASSCODE_MAX - PASSCODE_MIN + 1);
        if !FORBIDDEN_PASSCODES.contains(&candidate) {
            return candidate;
        }
    }
}

/// `12345678` → `1234-5678`, which is how the spec's own manual pairing code is grouped
/// and how a person reads a number off a screen across a room.
fn format_passcode(passcode: u32) -> String {
    let digits = format!("{passcode:08}");
    format!("{}-{}", &digits[..4], &digits[4..])
}

/// The socket half: bind 5550, parse, decide, reply.
pub struct UdcServer {
    socket: UdpSocket,
    state: UdcState,
    catalogue: Catalogue,
    prompts: mpsc::UnboundedSender<Prompt>,
    requests: mpsc::UnboundedSender<CommissionRequest>,
    outcomes: mpsc::UnboundedReceiver<AttemptEnd>,
}

impl UdcServer {
    /// Bind the UDC port.
    ///
    /// # Errors
    /// [`MatterError::Io`] if 5550 is taken — which on this port usually means a second
    /// Matter commissioner is already running on the box.
    #[expect(
        clippy::disallowed_methods,
        reason = "registered: the user-directed-commissioning socket (5550/udp), in the \
                  listener table of crates/app/src/surface.rs"
    )]
    pub async fn bind(
        addr: SocketAddr,
        catalogue: Catalogue,
        prompts: mpsc::UnboundedSender<Prompt>,
        requests: mpsc::UnboundedSender<CommissionRequest>,
        outcomes: mpsc::UnboundedReceiver<AttemptEnd>,
    ) -> Result<Self, MatterError> {
        let socket = UdpSocket::bind(addr)
            .await
            .map_err(|source| MatterError::Io {
                context: "binding the user-directed-commissioning socket",
                source,
            })?;

        Ok(Self {
            socket,
            state: UdcState::new(),
            catalogue,
            prompts,
            requests,
            outcomes,
        })
    }

    /// The address actually bound. Ephemeral in tests; 5550 in the panel.
    ///
    /// # Errors
    /// [`MatterError::Io`] if the socket cannot report it.
    pub fn local_addr(&self) -> Result<SocketAddr, MatterError> {
        self.socket.local_addr().map_err(|source| MatterError::Io {
            context: "reading the user-directed-commissioning socket address",
            source,
        })
    }

    /// Run until the socket dies.
    ///
    /// Two things wake this loop: a datagram, and a passcode reaching the end of
    /// [`PASSCODE_LIFETIME`]. The second is not an optimisation. Expiry used to be checked
    /// only on the way past a *new* datagram, so a phone that sent one declaration and
    /// walked away left an eight-digit commissioning passcode on a wall-mounted panel
    /// indefinitely — while the state machine considered it dead and would refuse it. That
    /// defeats the reasoning [`PASSCODE_LIFETIME`] exists for, which is that every extra
    /// minute is another minute the number is readable from the sofa (#197).
    ///
    /// # Errors
    /// [`MatterError::Io`] if the socket fails; a malformed datagram is logged and
    /// dropped, because anything at all can arrive on an unauthenticated UDP port.
    pub async fn run(mut self, rand: &mut impl RngCore) -> Result<(), MatterError> {
        // The reference client sends five copies of every message 100 ms apart, so the
        // buffer only has to hold one and the duplicates are handled by the state
        // machine being idempotent per instance name.
        let mut buf = [0u8; 1024];

        loop {
            // The deadline of the *earliest* outstanding passcode, so the loop sleeps
            // exactly as long as it has to rather than ticking. With nothing pending there
            // is no deadline and this branch never completes — a timer that fired with
            // nothing to expire would send a `Clear` over a prompt somebody else put up.
            let next_expiry = self.state.next_expiry();

            tokio::select! {
                () = async {
                    match next_expiry {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(instance) = self.state.expire(tokio::time::Instant::now()) {
                        tracing::info!(
                            %instance,
                            "matter: a displayed passcode expired; clearing the prompt"
                        );
                        let _ = self.prompts.send(Prompt::Clear { instance });
                    }
                }

                // A commissioning attempt finished — the slot it held comes free either
                // way, and a failure is reported to the client if it left an address.
                // The declaration goes out from *this* socket rather than a fresh one,
                // so the client sees the answer arrive from the port it has been talking
                // to. The good case sends nothing: it is announced by the panel
                // appearing on the client's fabric.
                Some(end) = self.outcomes.recv() => {
                    self.state.finished(&end.instance);
                    if let Some(outcome) = end.outcome {
                        match outcome.declaration.encode() {
                            Ok(datagram) => {
                                if let Err(e) = self.socket.send_to(&datagram, outcome.to).await {
                                    tracing::warn!(
                                        to = %outcome.to, error = %e,
                                        "matter: could not tell a client how commissioning went"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "matter: could not encode a UDC outcome"
                                );
                            }
                        }
                    }
                }

                received = self.socket.recv_from(&mut buf) => {
                    let (len, source) = received.map_err(|source| MatterError::Io {
                        context: "reading a user-directed-commissioning datagram",
                        source,
                    })?;

                    match IdentificationDeclaration::decode(&buf[..len]) {
                        Ok(id) => self.dispatch(&id, source, rand).await,
                        Err(UdcError::WrongProtocol { .. }) => {
                            // Something else on 5550. Not worth a warning: this port is
                            // unauthenticated and the internet scans it.
                            tracing::debug!(%source, "matter: a non-UDC datagram on the UDC port");
                        }
                        Err(e) => {
                            tracing::warn!(%source, error = %e, "matter: undecodable UDC datagram");
                        }
                    }
                }
            }
        }
    }

    async fn dispatch(
        &mut self,
        id: &IdentificationDeclaration,
        source: SocketAddr,
        rand: &mut impl RngCore,
    ) {
        let decision = self.state.handle(
            id,
            source,
            &self.catalogue,
            rand,
            tokio::time::Instant::now(),
        );

        // A passcode this datagram beat the expiry timer to. Down first: it may belong
        // to a different phone than the prompt this decision puts up.
        if let Some(instance) = decision.expired {
            tracing::info!(
                %instance,
                "matter: a displayed passcode expired; clearing the prompt"
            );
            let _ = self.prompts.send(Prompt::Clear { instance });
        }

        if decision.reply.error_code != CdError::None {
            tracing::info!(
                %source,
                instance = %id.instance_name,
                code = ?decision.reply.error_code,
                "matter: declining a UDC declaration"
            );
        }

        if let Some(prompt) = decision.prompt {
            let _ = self.prompts.send(prompt);
        }

        // The reply goes to the port the *client* named, not to the port the datagram
        // came from: a client behind its own ephemeral source port still listens on
        // `cdPort`.
        if let Some(port) = id.reply_port() {
            match decision.reply.encode() {
                Ok(datagram) => {
                    let to = SocketAddr::new(source.ip(), port);
                    if let Err(e) = self.socket.send_to(&datagram, to).await {
                        tracing::warn!(%to, error = %e, "matter: could not answer a UDC message");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "matter: could not encode a UDC reply"),
            }
        }

        if let Some(request) = decision.commission {
            let _ = self.requests.send(request);
        }
    }
}

/// The port the panel listens on. Re-exported so the adapter does not reach into [`crate::udc`].
pub const PORT: u16 = UDC_PORT;

/// The clock the state machine is tested against, shared so tests and the server agree.
pub type Instant = tokio::time::Instant;

/// A helper for the adapter: the passcode prompt channel, typed.
pub type PromptSender = mpsc::UnboundedSender<Prompt>;

/// Likewise for commissioning requests.
pub type RequestSender = mpsc::UnboundedSender<CommissionRequest>;

/// And for the worker's end-of-attempt reports.
pub type OutcomeSender = mpsc::UnboundedSender<AttemptEnd>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // The scripted phone binds an ephemeral loopback socket; the registry in
    // `crates/app/src/surface.rs` governs the ports the panel actually listens on.
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use crate::player::{ContentApp, LaunchTarget};
    use crate::udc::TargetApp;

    /// A deterministic RNG: passcode *content* is not what these tests are about, and a
    /// real one would make the assertions unrepeatable.
    struct Counter(u32);

    impl RngCore for Counter {
        fn next_u32(&mut self) -> u32 {
            self.0 = self.0.wrapping_add(1);
            self.0
        }
        fn next_u64(&mut self) -> u64 {
            u64::from(self.next_u32())
        }
        #[allow(clippy::cast_possible_truncation)]
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for b in dest.iter_mut() {
                *b = self.next_u32() as u8;
            }
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    fn catalogue() -> Catalogue {
        Catalogue::new([ContentApp {
            endpoint: 0,
            vendor_id: 4996,
            product_id: 1,
            vendor_name: "Amazon".into(),
            name: "Prime Video".into(),
            application_id: "com.amazon.avod".into(),
            catalog_vendor_id: 0,
            launch: LaunchTarget::Browser { search: None },
        }])
    }

    fn declaration() -> IdentificationDeclaration {
        IdentificationDeclaration {
            instance_name: InstanceName::new("BC5C01A61C48892F").unwrap(),
            vendor_id: Some(4996),
            product_id: Some(1),
            device_name: Some("Chaz's phone".into()),
            cd_port: Some(UDC_PORT),
            pairing_hint: None,
            pairing_instruction: None,
            rotating_id: None,
            target_apps: vec![TargetApp {
                vendor_id: 4996,
                product_id: 0,
            }],
            no_passcode: false,
            cd_upon_passcode_dialog: true,
            commissioner_passcode: true,
            commissioner_passcode_ready: false,
            cancel_passcode: false,
            passcode_length: Some(8),
        }
    }

    fn source() -> SocketAddr {
        "192.0.2.7:41234".parse().unwrap()
    }

    fn decide(
        state: &mut UdcState,
        id: &IdentificationDeclaration,
        now: tokio::time::Instant,
    ) -> Decision {
        state.handle(id, source(), &catalogue(), &mut Counter(41), now)
    }

    #[test]
    fn a_request_puts_a_passcode_on_the_screen() {
        let mut state = UdcState::new();
        let decision = decide(&mut state, &declaration(), Instant::now());

        assert!(decision.reply.passcode_dialog_displayed);
        assert!(decision.reply.commissioner_passcode);
        assert_eq!(decision.reply.passcode_length, 8);
        assert!(decision.commission.is_none(), "nothing to commission yet");

        let Some(Prompt::Passcode {
            instance,
            device,
            passcode,
        }) = decision.prompt
        else {
            panic!("expected a passcode prompt, got {:?}", decision.prompt);
        };
        assert_eq!(instance.as_str(), "BC5C01A61C48892F");
        assert_eq!(device, "Chaz's phone");
        assert_eq!(passcode.len(), 9, "eight digits and a separator");
        assert_eq!(&passcode[4..5], "-");
    }

    /// The panel has no keyboard. A client that wants to show its own passcode is
    /// declined in the spec's own words, not by silence.
    #[test]
    fn a_client_generated_passcode_is_declined() {
        let mut state = UdcState::new();
        let mut id = declaration();
        id.commissioner_passcode = false;

        let decision = decide(&mut state, &id, Instant::now());
        assert_eq!(
            decision.reply.error_code,
            CdError::CommissionerPasscodeNotSupported
        );
        assert!(decision.reply.needs_passcode);
        assert!(decision.prompt.is_none(), "nothing goes on the screen");
        assert_eq!(state.displayed(), None);
    }

    /// A user who types a passcode and *then* learns the app is missing has been made to
    /// do work for nothing.
    #[test]
    fn a_missing_app_is_reported_before_the_passcode() {
        let mut state = UdcState::new();
        let mut id = declaration();
        id.target_apps = vec![TargetApp {
            vendor_id: 1234,
            product_id: 0,
        }];

        let decision = decide(&mut state, &id, Instant::now());
        assert!(decision.reply.no_apps_found);
        assert!(!decision.reply.passcode_dialog_displayed);
        assert!(decision.prompt.is_none());
        assert_eq!(state.displayed(), None);
    }

    #[test]
    fn typing_the_passcode_produces_a_commissioning_request() {
        let mut state = UdcState::new();
        let issued = decide(&mut state, &declaration(), Instant::now());
        let Some(Prompt::Passcode { passcode, .. }) = issued.prompt else {
            panic!("no passcode issued");
        };

        let mut ready = declaration();
        ready.commissioner_passcode_ready = true;
        let decision = decide(&mut state, &ready, Instant::now());

        let request = decision.commission.expect("a commissioning request");
        assert_eq!(request.instance.as_str(), "BC5C01A61C48892F");
        assert_eq!(request.device_name, "Chaz's phone");
        assert_eq!(format_passcode(request.passcode), passcode);
        assert_eq!(state.displayed(), None, "the passcode is spent");
    }

    /// A user who mistypes must still be able to read the number off the screen, so the
    /// prompt stays up until commissioning actually finishes.
    #[test]
    fn the_prompt_survives_the_phone_claiming_the_passcode() {
        let mut state = UdcState::new();
        decide(&mut state, &declaration(), Instant::now());

        let mut ready = declaration();
        ready.commissioner_passcode_ready = true;
        assert!(decide(&mut state, &ready, Instant::now()).prompt.is_none());
    }

    #[test]
    fn a_passcode_we_never_issued_is_rejected() {
        let mut state = UdcState::new();
        let mut ready = declaration();
        ready.commissioner_passcode_ready = true;

        let decision = decide(&mut state, &ready, Instant::now());
        assert_eq!(
            decision.reply.error_code,
            CdError::UnexpectedCommissionerPasscodeReady
        );
        assert!(decision.commission.is_none());
    }

    #[test]
    fn cancelling_takes_the_prompt_down() {
        let mut state = UdcState::new();
        decide(&mut state, &declaration(), Instant::now());
        assert!(state.displayed().is_some());

        let mut cancel = declaration();
        cancel.cancel_passcode = true;
        let decision = decide(&mut state, &cancel, Instant::now());

        assert_eq!(
            decision.prompt,
            Some(Prompt::Clear {
                instance: InstanceName::new("BC5C01A61C48892F").unwrap()
            })
        );
        assert!(decision.reply.cancel_passcode);
        assert_eq!(state.displayed(), None);
    }

    /// The heart of #209: a second phone's cancel — or any stranger's — must not take
    /// down the prompt the first phone's user is reading.
    #[test]
    fn a_strangers_cancel_leaves_the_prompt_alone() {
        let mut state = UdcState::new();
        decide(&mut state, &declaration(), Instant::now());

        let mut other_cancel = declaration();
        other_cancel.instance_name = InstanceName::new("0011223344556677").unwrap();
        other_cancel.cancel_passcode = true;
        let decision = decide(&mut state, &other_cancel, Instant::now());

        // Acknowledged — cancelling nothing is idempotent — but nothing comes off the
        // screen, and the first phone's passcode is still redeemable.
        assert!(decision.reply.cancel_passcode);
        assert_eq!(
            decision.prompt, None,
            "the stranger cleared somebody's prompt"
        );
        assert_eq!(
            state.displayed().map(InstanceName::as_str),
            Some("BC5C01A61C48892F")
        );

        let mut ready = declaration();
        ready.commissioner_passcode_ready = true;
        assert!(
            decide(&mut state, &ready, Instant::now())
                .commission
                .is_some(),
            "the first phone's flow must survive the stranger's cancel"
        );
    }

    /// A second phone mid-prompt is refused, not queued and not interleaved (#209).
    ///
    /// `CommissionerPasscodeDisabled` because the spec's `CdError` list has no "busy"
    /// code: 17 is temporary unavailability, where 11 (`NotSupported`) is permanent and
    /// could push a client into the client-generated flow this panel also declines.
    #[test]
    fn a_second_phone_mid_prompt_is_refused_not_queued() {
        let mut state = UdcState::new();
        let issued = decide(&mut state, &declaration(), Instant::now());
        let Some(Prompt::Passcode {
            passcode: first, ..
        }) = issued.prompt
        else {
            panic!("no passcode issued");
        };

        let mut second = declaration();
        second.instance_name = InstanceName::new("0011223344556677").unwrap();
        let decision = decide(&mut state, &second, Instant::now());

        assert_eq!(
            decision.reply.error_code,
            CdError::CommissionerPasscodeDisabled
        );
        assert!(!decision.reply.passcode_dialog_displayed);
        assert_eq!(decision.prompt, None, "the second phone reached the screen");
        assert!(decision.commission.is_none());

        // And the first phone's number did not move: a retransmit still shows it.
        match decide(&mut state, &declaration(), Instant::now()).prompt {
            Some(Prompt::Passcode { passcode, .. }) => assert_eq!(passcode, first),
            other => panic!("the first phone lost its prompt: {other:?}"),
        }
    }

    /// The slot stays occupied through the whole attempt — discovery, PASE, CASE — not
    /// just while the number is on the glass, and comes free when the worker reports
    /// (#209). Without that, a second phone would be invited onto the screen while the
    /// first was mid-handshake behind it, and the worker's end-of-attempt cleanup would
    /// tear the newcomer's prompt down.
    #[test]
    fn the_slot_spans_the_commissioning_attempt() {
        let mut state = UdcState::new();
        let first = InstanceName::new("BC5C01A61C48892F").unwrap();
        decide(&mut state, &declaration(), Instant::now());

        let mut ready = declaration();
        ready.commissioner_passcode_ready = true;
        assert!(decide(&mut state, &ready, Instant::now())
            .commission
            .is_some());

        // The worker is now running. A second phone is still refused…
        let mut second = declaration();
        second.instance_name = InstanceName::new("0011223344556677").unwrap();
        assert_eq!(
            decide(&mut state, &second, Instant::now()).reply.error_code,
            CdError::CommissionerPasscodeDisabled
        );

        // …until the attempt ends, whichever way it went.
        state.finished(&first);
        let decision = decide(&mut state, &second, Instant::now());
        assert_eq!(decision.reply.error_code, CdError::None);
        assert!(decision.reply.passcode_dialog_displayed);
        assert!(matches!(
            decision.prompt,
            Some(Prompt::Passcode { instance, .. }) if instance.as_str() == "0011223344556677"
        ));
    }

    /// The ready declaration is retransmitted five times like everything else. The first
    /// copy starts the worker; the rest are acknowledged, not answered with an error the
    /// phone would show its user mid-pairing.
    #[test]
    fn a_ready_retransmit_is_acknowledged_not_rejected() {
        let mut state = UdcState::new();
        decide(&mut state, &declaration(), Instant::now());

        let mut ready = declaration();
        ready.commissioner_passcode_ready = true;
        assert!(decide(&mut state, &ready, Instant::now())
            .commission
            .is_some());

        let retransmit = decide(&mut state, &ready, Instant::now());
        assert_eq!(retransmit.reply.error_code, CdError::None);
        assert!(
            retransmit.commission.is_none(),
            "a retransmit must not start a second attempt"
        );
    }

    /// Cancelling mid-attempt takes the number off the glass but keeps the slot: the
    /// worker is already running and only its report can free it (#209).
    #[test]
    fn a_cancel_mid_attempt_clears_the_glass_but_keeps_the_slot() {
        let mut state = UdcState::new();
        let first = InstanceName::new("BC5C01A61C48892F").unwrap();
        decide(&mut state, &declaration(), Instant::now());

        let mut ready = declaration();
        ready.commissioner_passcode_ready = true;
        decide(&mut state, &ready, Instant::now());

        let mut cancel = declaration();
        cancel.cancel_passcode = true;
        let decision = decide(&mut state, &cancel, Instant::now());
        assert_eq!(
            decision.prompt,
            Some(Prompt::Clear {
                instance: first.clone()
            })
        );

        let mut second = declaration();
        second.instance_name = InstanceName::new("0011223344556677").unwrap();
        assert_eq!(
            decide(&mut state, &second, Instant::now()).reply.error_code,
            CdError::CommissionerPasscodeDisabled,
            "the worker still owns the slot"
        );

        state.finished(&first);
        assert_eq!(
            decide(&mut state, &second, Instant::now()).reply.error_code,
            CdError::None
        );
    }

    /// A datagram can reach a lapsed passcode before the expiry timer does. The lapse is
    /// then *reported* on the decision — a silently dropped entry is a prompt the timer
    /// will never find to take down — and the slot it held is already free for the
    /// arriving phone.
    #[test]
    fn an_expiry_noticed_by_a_datagram_is_reported_and_frees_the_slot() {
        let mut state = UdcState::new();
        let start = Instant::now();
        decide(&mut state, &declaration(), start);

        let mut second = declaration();
        second.instance_name = InstanceName::new("0011223344556677").unwrap();
        let decision = decide(&mut state, &second, start + PASSCODE_LIFETIME);

        assert_eq!(
            decision.expired.as_ref().map(InstanceName::as_str),
            Some("BC5C01A61C48892F"),
            "the lapsed prompt must be reported for takedown"
        );
        assert!(
            decision.reply.passcode_dialog_displayed,
            "the expired slot should have been free for the second phone"
        );
    }

    /// A passcode readable from the sofa is one anybody in the room can use.
    #[test]
    fn a_passcode_expires() {
        let mut state = UdcState::new();
        let start = Instant::now();
        decide(&mut state, &declaration(), start);

        let mut ready = declaration();
        ready.commissioner_passcode_ready = true;

        let later = start + PASSCODE_LIFETIME + Duration::from_secs(1);
        let decision = decide(&mut state, &ready, later);
        assert!(decision.commission.is_none());
        assert_eq!(
            decision.reply.error_code,
            CdError::UnexpectedCommissionerPasscodeReady
        );
    }

    /// …and the loop that has to *notice* it expired knows when to wake up.
    ///
    /// `a_passcode_expires` proves the state machine refuses a stale passcode, which was
    /// never the missing half — the missing half was that nothing took the number off the
    /// screen, because `expire` ran only on the way past the next inbound datagram and a
    /// phone that walked away sends no more datagrams (#197).
    ///
    /// The deadline is the displayed passcode's and nothing else's: a refused second
    /// phone leaves no deadline behind (#209), and a redeemed passcode takes its
    /// deadline with it — a timer armed then would fire mid-commissioning with nothing
    /// to expire.
    #[test]
    fn the_displayed_passcode_is_the_one_the_loop_waits_for() {
        let mut state = UdcState::new();
        assert_eq!(
            state.next_expiry(),
            None,
            "with nothing pending there is no deadline, so the loop must not arm a timer \
             that would clear a prompt somebody else put up"
        );

        let first = Instant::now();
        decide(&mut state, &declaration(), first);
        assert_eq!(state.next_expiry(), Some(first + PASSCODE_LIFETIME));

        // A refused second phone holds nothing, so it changes no deadline.
        let mut other = declaration();
        other.instance_name = InstanceName::new("0011223344556677").unwrap();
        decide(&mut state, &other, first + Duration::from_secs(30));
        assert_eq!(state.next_expiry(), Some(first + PASSCODE_LIFETIME));

        // Past the deadline: the slot empties and reports whose number came down.
        assert_eq!(
            state
                .expire(first + PASSCODE_LIFETIME + Duration::from_millis(1))
                .map(|i| i.as_str().to_string()),
            Some("BC5C01A61C48892F".to_string())
        );
        assert_eq!(state.displayed(), None);
        assert_eq!(state.next_expiry(), None);

        // And a redeemed passcode has no display deadline either: the attempt's bound is
        // the worker's, and its end is reported rather than timed out.
        decide(
            &mut state,
            &other,
            first + PASSCODE_LIFETIME + Duration::from_secs(2),
        );
        let mut ready = other;
        ready.commissioner_passcode_ready = true;
        decide(
            &mut state,
            &ready,
            first + PASSCODE_LIFETIME + Duration::from_secs(3),
        );
        assert_eq!(state.next_expiry(), None);
    }

    /// The whole thing over a real socket: a phone declares once and then goes away, and
    /// the number leaves the screen on its own.
    ///
    /// Paused time rather than a shortened lifetime, so what is asserted is the constant
    /// the panel actually ships with. The elapsed-virtual-time check is the second half of
    /// the assertion and the more important one: a `Clear` sent immediately would satisfy
    /// "the prompt goes away" and would take the passcode down while the user was still
    /// typing it.
    #[tokio::test(start_paused = true)]
    async fn a_phone_that_walks_away_does_not_leave_its_passcode_on_the_wall() {
        let (prompts_tx, mut prompts_rx) = mpsc::unbounded_channel();
        let (requests_tx, _requests_rx) = mpsc::unbounded_channel();
        let (_outcomes_tx, outcomes_rx) = mpsc::unbounded_channel();
        let server = UdcServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            catalogue(),
            prompts_tx,
            requests_tx,
            outcomes_rx,
        )
        .await
        .unwrap();
        let addr = server.local_addr().unwrap();

        tokio::spawn(async move {
            let _ = server.run(&mut Counter(41)).await;
        });

        // One declaration, and then nothing — the phone is gone.
        let phone = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        phone
            .send_to(&declaration().encode().unwrap(), addr)
            .await
            .unwrap();

        let shown = prompts_rx.recv().await.unwrap();
        let Prompt::Passcode { passcode, .. } = &shown else {
            panic!("expected a passcode prompt, got {shown:?}");
        };
        assert_eq!(passcode.len(), 9);
        let issued_at = Instant::now();

        // Under `start_paused` the runtime advances its clock to the *earliest* pending
        // deadline whenever it has nothing else to run. So if the loop armed an expiry
        // timer, that one is earlier than this bound and fires first; if it did not, the
        // clock jumps straight to the bound and this fails rather than hanging, which is
        // what it did before the fix.
        let cleared = tokio::time::timeout(PASSCODE_LIFETIME * 2, prompts_rx.recv())
            .await
            .expect("the passcode is still on the screen twice its lifetime later")
            .unwrap();
        assert_eq!(
            cleared,
            Prompt::Clear {
                instance: InstanceName::new("BC5C01A61C48892F").unwrap()
            }
        );
        assert!(
            Instant::now().duration_since(issued_at) >= PASSCODE_LIFETIME,
            "the prompt came down after {:?}, inside the window somebody is reading it in",
            Instant::now().duration_since(issued_at)
        );
    }

    /// The client sends five copies of every message and nothing in them distinguishes a
    /// retransmit from a second attempt. A fresh passcode per datagram would change the
    /// number four times while somebody was reading it.
    #[test]
    fn a_retransmit_shows_the_same_number() {
        let mut state = UdcState::new();
        let shown: Vec<_> = (0..5)
            .map(
                |_| match decide(&mut state, &declaration(), Instant::now()).prompt {
                    Some(Prompt::Passcode { passcode, .. }) => passcode,
                    other => panic!("expected a passcode prompt, got {other:?}"),
                },
            )
            .collect();

        assert!(state.displayed().is_some());
        assert_eq!(shown.len(), 5);
        assert!(
            shown.windows(2).all(|w| w[0] == w[1]),
            "the number changed under the user: {shown:?}"
        );
    }

    /// And the window is measured from when it first went up, not from the last
    /// retransmit — otherwise a client that re-declares every minute holds a passcode on
    /// the screen forever.
    #[test]
    fn a_retransmit_does_not_extend_the_window() {
        let mut state = UdcState::new();
        let start = Instant::now();
        decide(&mut state, &declaration(), start);
        decide(
            &mut state,
            &declaration(),
            start + PASSCODE_LIFETIME - Duration::from_secs(1),
        );

        let mut ready = declaration();
        ready.commissioner_passcode_ready = true;
        let decision = decide(
            &mut state,
            &ready,
            start + PASSCODE_LIFETIME + Duration::from_secs(1),
        );
        assert!(
            decision.commission.is_none(),
            "the window should have closed"
        );
    }

    #[test]
    fn passcodes_avoid_the_values_the_spec_forbids() {
        // A generator that would hand out a forbidden value first must not.
        struct Fixed(std::cell::Cell<usize>, Vec<u32>);
        impl RngCore for Fixed {
            fn next_u32(&mut self) -> u32 {
                let i = self.0.get();
                self.0.set(i + 1);
                self.1[i.min(self.1.len() - 1)]
            }
            fn next_u64(&mut self) -> u64 {
                u64::from(self.next_u32())
            }
            fn fill_bytes(&mut self, _dest: &mut [u8]) {}
            fn try_fill_bytes(&mut self, _dest: &mut [u8]) -> Result<(), rand_core::Error> {
                Ok(())
            }
        }

        // 11111110 + PASSCODE_MIN == 11111111, which is forbidden; the next draw is not.
        let mut rand = Fixed(std::cell::Cell::new(0), vec![11_111_110, 42]);
        assert_eq!(generate_passcode(&mut rand), 43);
    }

    #[test]
    fn a_passcode_is_grouped_for_reading_across_a_room() {
        assert_eq!(format_passcode(1_234_567), "0123-4567");
        assert_eq!(format_passcode(98_765_432), "9876-5432");
    }
}
