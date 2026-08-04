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
}

async fn harness(catalogue: Catalogue) -> Harness {
    let (prompt_tx, prompts) = mpsc::unbounded_channel();
    let (request_tx, requests) = mpsc::unbounded_channel();

    let server = UdcServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        catalogue,
        prompt_tx,
        request_tx,
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

    let Some(Prompt::Passcode { device, passcode }) = h.prompts.recv().await else {
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
    assert_eq!(h.prompts.recv().await, Some(Prompt::Clear));
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
