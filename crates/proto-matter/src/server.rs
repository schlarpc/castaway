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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// A phone is asking to cast. Show this passcode until told otherwise.
    Passcode {
        /// Who is asking, as they named themselves.
        device: String,
        /// The number to display, already formatted with the spec's grouping.
        passcode: String,
    },
    /// Take it down: the user dismissed it on the phone, it expired, or commissioning
    /// finished.
    Clear,
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
}

/// One phone's in-flight passcode.
#[derive(Debug, Clone)]
struct Pending {
    passcode: u32,
    device_name: String,
    issued: tokio::time::Instant,
}

/// The pure state machine behind the socket: declarations in, decisions out.
///
/// Separated from the socket so the whole passcode lifecycle — issue, expire, cancel,
/// redeem — is testable without a network (ground rule 3).
#[derive(Debug, Default)]
pub struct UdcState {
    pending: std::collections::HashMap<InstanceName, Pending>,
}

/// What the server should do about a declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The reply to send, if the client asked for one.
    pub reply: CommissionerDeclaration,
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
        self.expire(now);

        match id.request() {
            UdcRequest::Cancel => {
                self.pending.remove(&id.instance_name);
                Decision {
                    reply: CommissionerDeclaration {
                        cancel_passcode: true,
                        ..CommissionerDeclaration::default()
                    },
                    prompt: Some(Prompt::Clear),
                    commission: None,
                }
            }

            UdcRequest::PasscodeReady => match self.pending.remove(&id.instance_name) {
                Some(pending) => Decision {
                    reply: CommissionerDeclaration::default(),
                    // The prompt stays up: the passcode is still the shared secret until
                    // PASE completes, and taking it down the instant the phone claims to
                    // have it would leave a user who mistyped with nothing to re-read.
                    prompt: None,
                    commission: Some(CommissionRequest {
                        instance: id.instance_name.clone(),
                        passcode: pending.passcode,
                        device_name: pending.device_name,
                        source,
                    }),
                },
                // A phone claiming a passcode we never issued — a retransmit after the
                // window closed, most likely. The spec has a code for exactly this.
                None => Decision {
                    reply: CommissionerDeclaration {
                        error_code: CdError::UnexpectedCommissionerPasscodeReady,
                        ..CommissionerDeclaration::default()
                    },
                    prompt: None,
                    commission: None,
                },
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
                        prompt: None,
                        commission: None,
                    };
                }

                // A declaration that is already pending gets the number it already got.
                // The client sends five copies of every message and there is nothing in
                // them to tell a retransmit from a second attempt, so generating a fresh
                // passcode per datagram would change the number on the screen four times
                // while somebody was reading it — and invalidate the one they had already
                // started typing. The issue time is not refreshed either: the window
                // starts when the passcode was first shown, not at the last retransmit.
                let (passcode, device_name) = match self.pending.get(&id.instance_name) {
                    Some(pending) => (pending.passcode, pending.device_name.clone()),
                    None => {
                        let passcode = generate_passcode(rand);
                        let device_name = id
                            .device_name
                            .clone()
                            .unwrap_or_else(|| "a device".to_string());

                        self.pending.insert(
                            id.instance_name.clone(),
                            Pending {
                                passcode,
                                device_name: device_name.clone(),
                                issued: now,
                            },
                        );

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
                    prompt: Some(Prompt::Passcode {
                        device: device_name,
                        passcode: format_passcode(passcode),
                    }),
                    commission: None,
                }
            }
        }
    }

    /// Drop passcodes past their lifetime. Returns whether anything was dropped, which is
    /// the panel's cue to take the prompt down.
    pub fn expire(&mut self, now: tokio::time::Instant) -> bool {
        let before = self.pending.len();
        self.pending
            .retain(|_, p| now.duration_since(p.issued) < PASSCODE_LIFETIME);
        self.pending.len() != before
    }

    /// When the next passcode falls out of its window, or `None` if none is outstanding.
    ///
    /// This is what makes expiry an *event* rather than something noticed the next time a
    /// datagram happens to arrive. A phone that sends one declaration and walks away sends
    /// no more datagrams by definition, which is exactly the case where the number is left
    /// on a wall-mounted panel with nobody watching.
    #[must_use]
    pub fn next_expiry(&self) -> Option<tokio::time::Instant> {
        self.pending
            .values()
            .map(|p| p.issued + PASSCODE_LIFETIME)
            .min()
    }

    /// How many passcodes are outstanding.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
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
                    if self.state.expire(tokio::time::Instant::now()) {
                        tracing::info!("matter: a displayed passcode expired; clearing the prompt");
                        let _ = self.prompts.send(Prompt::Clear);
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

        let Some(Prompt::Passcode { device, passcode }) = decision.prompt else {
            panic!("expected a passcode prompt, got {:?}", decision.prompt);
        };
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
        assert_eq!(state.pending(), 0);
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
        assert_eq!(state.pending(), 0);
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
        assert_eq!(state.pending(), 0, "the passcode is spent");
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
        assert_eq!(state.pending(), 1);

        let mut cancel = declaration();
        cancel.cancel_passcode = true;
        let decision = decide(&mut state, &cancel, Instant::now());

        assert_eq!(decision.prompt, Some(Prompt::Clear));
        assert!(decision.reply.cancel_passcode);
        assert_eq!(state.pending(), 0);
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
    #[test]
    fn the_earliest_passcode_is_the_one_the_loop_waits_for() {
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

        // A second phone, later. The deadline stays the *earlier* one, so the loop wakes
        // for whichever number comes off the screen first.
        let second = first + Duration::from_secs(30);
        let mut other = declaration();
        other.instance_name = InstanceName::new("0011223344556677").unwrap();
        decide(&mut state, &other, second);
        assert_eq!(state.pending(), 2);
        assert_eq!(state.next_expiry(), Some(first + PASSCODE_LIFETIME));

        // Past the first deadline: it goes, the second remains, and the deadline moves to
        // it rather than to `None`.
        assert!(state.expire(first + PASSCODE_LIFETIME + Duration::from_millis(1)));
        assert_eq!(state.pending(), 1);
        assert_eq!(state.next_expiry(), Some(second + PASSCODE_LIFETIME));

        assert!(state.expire(second + PASSCODE_LIFETIME + Duration::from_millis(1)));
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
        let server = UdcServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            catalogue(),
            prompts_tx,
            requests_tx,
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
        assert_eq!(cleared, Prompt::Clear);
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

        assert_eq!(state.pending(), 1);
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
