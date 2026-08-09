//! A scripted Casting Client, over a real socket.
//!
//! The unit tests drive [`proto_matter::server::UdcState`] directly, which proves the
//! decisions. This drives the *server* — bind, parse, decide, reply — from a socket that
//! speaks the same bytes a phone does, which is the part a state machine cannot check:
//! that the reply is addressed to the port the client named rather than the one it sent
//! from, that a retransmit is idempotent, and that the encoder and decoder agree across
//! the wire rather than only across a function call.
//!
//! What it is not: a commissioning test. Everything past the passcode needs a peer that
//! runs PASE, and the only such peer is a Matter node — see the tracking issue.

#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::time::Duration;

use proto_matter::player::{Catalogue, ContentApp, LaunchTarget};
use proto_matter::server::{CommissionRequest, Prompt, UdcServer};
use proto_matter::udc::{
    CdError, CommissionerDeclaration, IdentificationDeclaration, InstanceName, TargetApp,
};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// The phone: a socket that sends declarations and reads what comes back.
struct Client {
    socket: UdpSocket,
    server: SocketAddr,
    instance: InstanceName,
}

impl Client {
    async fn new(server: SocketAddr) -> Self {
        Self {
            socket: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            server,
            instance: InstanceName::new("BC5C01A61C48892F").unwrap(),
        }
    }

    fn port(&self) -> u16 {
        self.socket.local_addr().unwrap().port()
    }

    fn declaration(&self) -> IdentificationDeclaration {
        IdentificationDeclaration {
            instance_name: self.instance.clone(),
            vendor_id: Some(4996),
            product_id: Some(1),
            device_name: Some("Chaz's phone".into()),
            // The port the reply must go to. Deliberately *not* the well-known 5550: the
            // whole point of this field is that a client listens where it chooses.
            cd_port: Some(self.port()),
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

    async fn send(&self, id: &IdentificationDeclaration) {
        self.socket
            .send_to(&id.encode().unwrap(), self.server)
            .await
            .unwrap();
    }

    async fn reply(&self) -> CommissionerDeclaration {
        let mut buf = [0u8; 1024];
        let (len, _) =
            tokio::time::timeout(Duration::from_secs(5), self.socket.recv_from(&mut buf))
                .await
                .expect("the panel never answered")
                .unwrap();
        CommissionerDeclaration::decode(&buf[..len]).unwrap()
    }
}

struct Harness {
    client: Client,
    prompts: mpsc::UnboundedReceiver<Prompt>,
    requests: mpsc::UnboundedReceiver<CommissionRequest>,
    /// The commissioning worker's end: what the panel tells a client once an attempt it
    /// already answered has finished. There is no worker in this harness, so tests drive
    /// it directly.
    outcomes: proto_matter::server::OutcomeSender,
}

async fn harness(catalogue: Catalogue) -> Harness {
    let (prompt_tx, prompts) = mpsc::unbounded_channel();
    let (request_tx, requests) = mpsc::unbounded_channel();
    let (outcome_tx, outcome_rx) = mpsc::unbounded_channel();

    let server = UdcServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        catalogue,
        prompt_tx,
        request_tx,
        outcome_rx,
    )
    .await
    .unwrap();
    let addr = server.local_addr().unwrap();

    tokio::spawn(async move {
        let mut rand = rand_core::OsRng;
        let _ = server.run(&mut rand).await;
    });

    Harness {
        client: Client::new(addr).await,
        prompts,
        requests,
        outcomes: outcome_tx,
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

/// The whole handshake, minus the person: declare, get a passcode, say it was typed, and
/// come out the other side as a commissioning request carrying the number that was shown.
#[tokio::test]
async fn a_passcode_round_trip() {
    let mut h = harness(catalogue()).await;
    let id = h.client.declaration();

    h.client.send(&id).await;

    let reply = h.client.reply().await;
    assert!(reply.passcode_dialog_displayed);
    assert!(reply.commissioner_passcode);
    assert_eq!(reply.error_code, CdError::None);
    assert_eq!(reply.passcode_length, 8);

    let Some(Prompt::Passcode {
        device, passcode, ..
    }) = h.prompts.recv().await
    else {
        panic!("nothing went on the screen");
    };
    assert_eq!(device, "Chaz's phone");

    let mut ready = id;
    ready.commissioner_passcode_ready = true;
    h.client.send(&ready).await;

    let request = h.requests.recv().await.expect("a commissioning request");
    assert_eq!(request.instance.as_str(), "BC5C01A61C48892F");
    assert_eq!(request.device_name, "Chaz's phone");
    // The number on the glass is the PASE secret, formatted the way a person reads it.
    assert_eq!(
        passcode,
        format!("{:08}", request.passcode)
            .split_at(4)
            .pipe(|(a, b)| format!("{a}-{b}"))
    );
}

/// A commissioning request carries where to send the answer, and the answer arrives.
///
/// The failure this closes is that `commission_loop`'s `Err` arm logged, put a banner on
/// the panel, and sent the client **nothing** — so all ten of the spec's commissioning
/// error codes had no producer anywhere in the tree. In the room that is: somebody
/// mistypes the passcode, and their phone gets silence rather than "wrong code, try
/// again". The phone's UI then has to guess, and what it usually guesses is a timeout
/// (#198).
///
/// There is no commissioning worker in this harness — that needs a Matter node, and lives
/// in `matter-vm` — so the outcome is pushed onto the same channel the worker uses. What
/// this proves is the half a worker cannot: that the request carries a usable return
/// address, and that a declaration sent minutes after the exchange still reaches the
/// client's `cdPort` from the socket it has been talking to.
#[tokio::test]
async fn a_client_is_told_how_commissioning_went() {
    let mut h = harness(catalogue()).await;
    let id = h.client.declaration();

    h.client.send(&id).await;
    let _ = h.client.reply().await;
    let _ = h.prompts.recv().await;

    let mut ready = id;
    ready.commissioner_passcode_ready = true;
    h.client.send(&ready).await;
    let request = h.requests.recv().await.expect("a commissioning request");
    // The immediate answer to `commissionerPasscodeReady`: "understood, going now". Drained
    // so the assertion below is on the *outcome* and not on this.
    assert_eq!(h.client.reply().await.error_code, CdError::None);

    // The `cdPort` the client named, at the address it sent from — not the source port,
    // which is the same distinction `the_reply_goes_to_the_declared_port` makes for the
    // immediate reply and which is easier to get wrong here, minutes later.
    let to = request.reply_to.expect("nowhere to send the outcome");
    assert_eq!(to.port(), h.client.port());

    // What the worker sends when `await_commissionable` times out: the client advertised
    // nothing we could find. The report also frees the pairing slot (#209).
    h.outcomes
        .send(proto_matter::server::AttemptEnd {
            instance: request.instance,
            outcome: Some(proto_matter::server::Outcome {
                to,
                declaration: CommissionerDeclaration {
                    error_code: CdError::CommissionableDiscoveryFailed,
                    ..CommissionerDeclaration::default()
                },
            }),
        })
        .unwrap();

    let told = h.client.reply().await;
    assert_eq!(told.error_code, CdError::CommissionableDiscoveryFailed);
}

/// A client that names no `cdPort` is not one we can tell anything, and asking is how the
/// caller finds that out rather than by a send to port 0.
#[tokio::test]
async fn a_client_with_no_reply_port_has_no_return_address() {
    let mut h = harness(catalogue()).await;
    let mut id = h.client.declaration();
    id.cd_port = None;
    // `cdUponPasscodeDialog` asks for a reply there is no port for. The panel still shows
    // the passcode — a client can be commissioned without ever reading a CD — so the
    // request is real and only the return address is missing.
    id.cd_upon_passcode_dialog = false;

    h.client.send(&id).await;
    let _ = h.prompts.recv().await;

    let mut ready = id;
    ready.commissioner_passcode_ready = true;
    h.client.send(&ready).await;

    let request = h.requests.recv().await.expect("a commissioning request");
    assert_eq!(request.reply_to, None);
}

/// The reference client sends five copies of every message, 100 ms apart, because there
/// is no acknowledgement. Five passcodes would be four numbers that are not the one being
/// typed.
#[tokio::test]
async fn five_copies_produce_one_passcode() {
    let mut h = harness(catalogue()).await;
    let id = h.client.declaration();

    for _ in 0..5 {
        h.client.send(&id).await;
    }

    let mut shown = Vec::new();
    for _ in 0..5 {
        let _ = h.client.reply().await;
        let Some(Prompt::Passcode { passcode, .. }) = h.prompts.recv().await else {
            panic!("expected a passcode prompt");
        };
        shown.push(passcode);
    }

    assert_eq!(shown.len(), 5, "each copy is answered");
    assert!(
        shown.windows(2).all(|w| w[0] == w[1]),
        "and they are all the same number: {shown:?}"
    );
}

/// The reply goes where the client said to send it, which is not where it sent from.
#[tokio::test]
async fn the_reply_goes_to_the_declared_port() {
    let mut h = harness(catalogue()).await;

    // A second socket, sending on behalf of the first — exactly the shape of a client
    // whose UDC sender and listener are different sockets.
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let id = h.client.declaration();
    sender
        .send_to(&id.encode().unwrap(), h.client.server)
        .await
        .unwrap();

    // The answer arrives on the declared port…
    let reply = h.client.reply().await;
    assert!(reply.passcode_dialog_displayed);

    // …and not on the one it was sent from.
    let mut buf = [0u8; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(200), sender.recv_from(&mut buf))
            .await
            .is_err(),
        "the sending socket must not receive the reply"
    );

    assert!(h.prompts.recv().await.is_some());
}

/// A user who types a passcode and then learns the app is missing has been made to do
/// work for nothing, so the refusal comes first and nothing goes on the screen.
#[tokio::test]
async fn an_unhosted_app_is_refused_before_any_passcode() {
    let mut h = harness(Catalogue::default()).await;

    h.client.send(&h.client.declaration()).await;

    let reply = h.client.reply().await;
    assert!(reply.no_apps_found);
    assert!(!reply.passcode_dialog_displayed);

    assert!(
        tokio::time::timeout(Duration::from_millis(200), h.prompts.recv())
            .await
            .is_err(),
        "nothing should have gone on the screen"
    );
}

/// Dismissing the prompt on the phone takes it off the panel.
#[tokio::test]
async fn cancelling_from_the_phone_clears_the_screen() {
    let mut h = harness(catalogue()).await;
    let id = h.client.declaration();

    h.client.send(&id).await;
    let _ = h.client.reply().await;
    assert!(matches!(
        h.prompts.recv().await,
        Some(Prompt::Passcode { .. })
    ));

    let mut cancel = id;
    cancel.cancel_passcode = true;
    h.client.send(&cancel).await;

    let reply = h.client.reply().await;
    assert!(reply.cancel_passcode);
    assert_eq!(
        h.prompts.recv().await,
        Some(Prompt::Clear {
            instance: h.client.instance.clone()
        })
    );
}

/// Two phones on the wire at once (#209): the second is refused with the one `CdError`
/// that reads as "busy" — and its cancel on the way out does not reach the screen.
#[tokio::test]
async fn a_second_phone_is_refused_and_its_cancel_touches_nothing() {
    let mut h = harness(catalogue()).await;

    h.client.send(&h.client.declaration()).await;
    assert!(h.client.reply().await.passcode_dialog_displayed);
    assert!(matches!(
        h.prompts.recv().await,
        Some(Prompt::Passcode { .. })
    ));

    // The second phone: its own socket, its own instance, the same panel.
    let mut second = Client::new(h.client.server).await;
    second.instance = InstanceName::new("0011223344556677").unwrap();
    second.send(&second.declaration()).await;

    let refused = second.reply().await;
    assert_eq!(refused.error_code, CdError::CommissionerPasscodeDisabled);
    assert!(!refused.passcode_dialog_displayed);

    // The refused user backs out on their phone. Acknowledged — and the first phone's
    // prompt stays where it is.
    let mut cancel = second.declaration();
    cancel.cancel_passcode = true;
    second.send(&cancel).await;
    assert!(second.reply().await.cancel_passcode);

    assert!(
        tokio::time::timeout(Duration::from_millis(200), h.prompts.recv())
            .await
            .is_err(),
        "the second phone's refusal or cancel reached the screen"
    );

    // And the first phone's flow is intact: typing the passcode still commissions it.
    let mut ready = h.client.declaration();
    ready.commissioner_passcode_ready = true;
    h.client.send(&ready).await;
    let request = h.requests.recv().await.expect("a commissioning request");
    assert_eq!(request.instance, h.client.instance);
}

/// The refusal is temporary: once the worker reports the attempt over, the next phone is
/// served (#209).
#[tokio::test]
async fn the_slot_frees_when_the_worker_reports() {
    let mut h = harness(catalogue()).await;

    h.client.send(&h.client.declaration()).await;
    let _ = h.client.reply().await;
    let _ = h.prompts.recv().await;
    let mut ready = h.client.declaration();
    ready.commissioner_passcode_ready = true;
    h.client.send(&ready).await;
    let request = h.requests.recv().await.expect("a commissioning request");
    assert_eq!(h.client.reply().await.error_code, CdError::None);

    // Mid-attempt: a second phone is refused.
    let mut second = Client::new(h.client.server).await;
    second.instance = InstanceName::new("0011223344556677").unwrap();
    second.send(&second.declaration()).await;
    assert_eq!(
        second.reply().await.error_code,
        CdError::CommissionerPasscodeDisabled
    );

    // The worker reports the attempt over — a success, so nothing is sent to anyone.
    h.outcomes
        .send(proto_matter::server::AttemptEnd {
            instance: request.instance,
            outcome: None,
        })
        .unwrap();

    // The same phone, declaring again, now gets the glass. Polled with a deadline: the
    // report travels a channel, and the datagram can beat it into the select loop.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        second.send(&second.declaration()).await;
        let reply = second.reply().await;
        if reply.passcode_dialog_displayed {
            break;
        }
        assert_eq!(reply.error_code, CdError::CommissionerPasscodeDisabled);
        assert!(
            tokio::time::Instant::now() < deadline,
            "the slot never came free after the worker's report"
        );
    }
}

/// Anything at all arrives on an unauthenticated UDP port. None of it should stop the
/// server answering the next real message.
#[tokio::test]
async fn junk_on_the_port_does_not_stop_the_server() {
    let mut h = harness(catalogue()).await;

    for junk in [
        &b""[..],
        &b"\x00"[..],
        &[0u8; 13][..],
        // A well-formed Matter header for a different protocol.
        &[0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0][..],
        // The right protocol, an unknown opcode.
        &[0, 0, 0, 0, 0, 0, 0, 0, 1, 0x7f, 0, 0, 9, 0][..],
        &[0xff; 300][..],
    ] {
        h.client
            .socket
            .send_to(junk, h.client.server)
            .await
            .unwrap();
    }

    h.client.send(&h.client.declaration()).await;
    assert!(h.client.reply().await.passcode_dialog_displayed);
    assert!(h.prompts.recv().await.is_some());
}

/// A tiny helper so the passcode formatting assertion reads in one line.
trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}

impl<T> Pipe for T {}
