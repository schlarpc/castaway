//! `matter-peer --player <ip>` — a scripted Casting Client, the peer this receiver has
//! never had.
//!
//! `tests/udc_over_the_wire.rs` proves the UDC half against a socket. It stops exactly
//! where the interesting part starts: everything past "the user typed the passcode" —
//! the `_matterc._udp` browse, PASE, `ArmFailSafe` → `AddNOC`, CASE,
//! `CommissioningComplete`, and then the client opening CASE *back* and invoking a
//! cluster — needs a peer that runs the Matter core. This is that peer (issue #171).
//!
//! It is `rs-matter` on both sides for the *core*, which is agreement with ourselves and
//! worth saying plainly. What it is not agreement about is everything this project owns,
//! and all of it is on the path this binary walks:
//!
//! - the UDC exchange, encoder and decoder, over a real socket;
//! - [`proto_matter::discovery::await_commissionable`] — our `_matterc._udp` browse on
//!   our own responder, against a real advertiser. `rs-matter`'s own commissioning test
//!   *skips mDNS entirely* (its comment: device and controller share the host's `:5353`
//!   and multicast loopback is unreliable), so two hosts on a test LAN is the first time
//!   this code path meets a peer at all;
//! - our certificate authority, our NOC generator wiring, and the fabric we install;
//! - our ACL seeding, which is what decides whether the invoke below is answered or
//!   refused;
//! - our endpoint tree, and what a `LaunchURL` means once it arrives.
//!
//! ## The passcode, and why it arrives in a file
//!
//! The panel generates the passcode and puts it on the glass; a person reads it and types
//! it into the phone. There is no wire path for it — that gap *is* the security property.
//! So the harness plays the person: it reads the number out of the receiver's log and
//! writes it into `--passcode-file`, which this binary polls. Splitting the run in two
//! processes instead would drop the UDC socket between the two declarations, and the
//! second one has to come from the port the first one named.
//!
//! Prints a terminal sentinel on success so the VM test asserts on a line rather than on
//! the absence of a crash.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use proto_matter::fabric::CastingCa;
use proto_matter::udc::{
    CdError, CommissionerDeclaration, IdentificationDeclaration, InstanceName, TargetApp,
};
use proto_matter::MATTER_PORT;

use rs_matter::crypto::Crypto as _;
use rs_matter::dm::clusters::basic_info::BasicInfoConfig;
use rs_matter::dm::clusters::decl::content_launcher::ContentLauncherClient as _;
use rs_matter::dm::clusters::net_comm::DummyNetworks;
use rs_matter::dm::devices::test::TEST_DEV_ATT;
use rs_matter::dm::endpoints::EthSysHandlerBuilder;
use rs_matter::dm::Node;
use rs_matter::im::{InteractionModel, InteractionModelState};
use rs_matter::persist::DummyKvBlobStore;
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::{
    Spake2pVerifierPassword, Spake2pVerifierPasswordRef, MAX_COMM_WINDOW_TIMEOUT_SECS,
};
use rs_matter::transport::exchange::{Exchange, MatterBuffers};
use rs_matter::transport::network::NoNetwork;
use rs_matter::{root_endpoint, BasicCommData, Matter};

use substrate_mdns::{MdnsResponder, MdnsService};

use tokio::net::UdpSocket;

/// How long to wait for the panel's answer to a declaration.
const CD_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for a person — here, the harness — to supply the passcode.
const PASSCODE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait to be commissioned once the passcode is in and we are advertising.
const COMMISSION_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to wait for the panel to say *why* it gave up.
///
/// Longer than every other wait here, because it is the sum of the panel's own:
/// `COMMISSIONABLE_TIMEOUT` is 60 s on its side, and a wrong passcode fails PASE only
/// after the exchange's own retransmits.
const REFUSAL_TIMEOUT: Duration = Duration::from_secs(90);

/// How long to keep retrying the `LaunchURL` invoke.
///
/// Not a network timeout: `AddNOC` lands the fabric *before* CASE and
/// `CommissioningComplete`, so `is_commissioned()` goes true while the panel is still
/// mid-flow. Retrying rather than sleeping a fixed amount keeps the race visible in the
/// log instead of papered over.
const INVOKE_WINDOW: Duration = Duration::from_secs(30);

struct Args {
    player: IpAddr,
    bind: IpAddr,
    passcode_file: PathBuf,
    instance: InstanceName,
    device_name: String,
    url: String,
    display_string: String,
    endpoint: u16,
    app_vendor_id: u16,
    app_product_id: u16,
    discriminator: u16,
    /// The port this node listens on, and the one it puts in its own SRV record.
    ///
    /// Overridable so this can be run on the same host as the panel, which has already
    /// taken 5540. That is not a hack around the test: the panel dials whatever port the
    /// `_matterc._udp` record names, and a client that is not on the well-known port is
    /// the case worth being able to reproduce by hand.
    matter_port: u16,
    /// Declare, report what came back, and exit — without ever typing a passcode.
    ///
    /// The phone that walks away. Everything after phase 1 is skipped, so the panel is
    /// left holding a passcode nothing will ever redeem, which is the case
    /// `PASSCODE_LIFETIME` exists for and the one the panel used to leave on the glass
    /// indefinitely (#197).
    declare_only: bool,
    /// Type a number that is not the one on the screen.
    ///
    /// The panel's PASE then fails against a verifier built from the wrong secret, which
    /// is the single most likely way commissioning goes wrong in a room — somebody
    /// misreads a digit across it.
    wrong_passcode: bool,
    /// Advertise under a label the panel was never told to look for.
    ///
    /// Everything else is correct, so this isolates the browse: the panel waits out its
    /// full `COMMISSIONABLE_TIMEOUT` on a node that is sitting right there under a
    /// different name.
    wrong_instance: bool,
}

impl Args {
    /// Whether this run expects to be refused rather than commissioned.
    const fn expects_refusal(&self) -> bool {
        self.wrong_passcode || self.wrong_instance
    }

    /// The label to advertise under, which is the declared one unless we are deliberately
    /// hiding.
    fn advertised_instance(&self) -> String {
        if self.wrong_instance {
            "FFFFFFFFFFFFFFFF".to_string()
        } else {
            self.instance.as_str().to_string()
        }
    }
}

fn usage() -> String {
    "usage: matter-peer --player <ip> --passcode-file <path> [--bind <ip>] \
     [--instance <hex16>] [--name <str>] [--url <str>] [--display-string <str>] \
     [--endpoint <n>] [--app-vendor <n>] [--app-product <n>] [--discriminator <n>] \
     [--matter-port <n>] [--declare-only] [--wrong-passcode] [--wrong-instance]"
        .into()
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut player = None;
    let mut bind = None;
    let mut passcode_file = None;
    // The default is `udc_over_the_wire.rs`'s, so a failure here and a failure there name
    // the same client.
    let mut instance = "BC5C01A61C48892F".to_string();
    let mut device_name = "matter-peer".to_string();
    let mut url = "https://example.invalid/matter-peer.mp4".to_string();
    let mut display_string = "matter-peer launch".to_string();
    let mut endpoint = 1_u16;
    // The panel's own default catalogue entry: vendor 0xFFF1, product 0x8001. A client
    // picks an endpoint by matching this, so it is the address of the cast.
    let mut app_vendor_id = 0xFFF1_u16;
    let mut app_product_id = 0x8001_u16;
    let mut discriminator = 3840_u16;
    let mut matter_port = MATTER_PORT;
    let mut declare_only = false;
    let mut wrong_passcode = false;
    let mut wrong_instance = false;

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--player" => player = Some(value()?.parse::<IpAddr>()?),
            "--bind" => bind = Some(value()?.parse::<IpAddr>()?),
            "--passcode-file" => passcode_file = Some(PathBuf::from(value()?)),
            "--instance" => instance = value()?,
            "--name" => device_name = value()?,
            "--url" => url = value()?,
            "--display-string" => display_string = value()?,
            "--endpoint" => endpoint = value()?.parse()?,
            "--app-vendor" => app_vendor_id = value()?.parse()?,
            "--app-product" => app_product_id = value()?.parse()?,
            "--discriminator" => discriminator = value()?.parse()?,
            "--matter-port" => matter_port = value()?.parse()?,
            "--declare-only" => declare_only = true,
            "--wrong-passcode" => wrong_passcode = true,
            "--wrong-instance" => wrong_instance = true,
            other => return Err(format!("unknown argument {other:?}\n{}", usage()).into()),
        }
    }

    Ok(Args {
        player: player.ok_or_else(usage)?,
        bind: bind.unwrap_or(IpAddr::from([0, 0, 0, 0])),
        passcode_file: passcode_file.ok_or_else(usage)?,
        instance: InstanceName::new(&instance)?,
        device_name,
        url,
        display_string,
        endpoint,
        app_vendor_id,
        app_product_id,
        discriminator,
        matter_port,
        declare_only,
        wrong_passcode,
        wrong_instance,
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,rs_matter=debug,proto_matter=debug".into()),
        )
        .init();

    let args = parse_args()?;

    // ---- Phase 1: the UDC exchange, from the client's side -------------------------

    let udc = UdcSocket::bind(
        args.bind,
        SocketAddr::new(args.player, proto_matter::UDC_PORT),
    )
    .await?;
    let declaration = declaration(&args, &udc);

    tracing::info!(
        instance = %args.instance,
        cd_port = udc.port(),
        "matter-peer: declaring, and asking the panel to pick the passcode"
    );
    udc.send(&declaration).await?;

    let cd = udc.reply().await?;
    if cd.error_code != CdError::None {
        return Err(format!("the panel refused: {:?}", cd.error_code).into());
    }
    if !cd.passcode_dialog_displayed || !cd.commissioner_passcode {
        return Err(format!(
            "the panel did not take the passcode on itself: dialog={} commissioner={}",
            cd.passcode_dialog_displayed, cd.commissioner_passcode
        )
        .into());
    }
    println!(
        "matter-peer: passcode dialog is up, {} digits",
        cd.passcode_length
    );

    if args.declare_only {
        // The sentinel for the walk-away case. Nothing here redeems the passcode, so the
        // panel is now the only party with a reason to act, and what it has to do is take
        // the number off its screen on its own.
        println!("matter-peer: declared and leaving");
        return Ok(());
    }

    // ---- Phase 2: play the person ---------------------------------------------------

    let passcode = await_passcode(&args.passcode_file).await?;
    let passcode = if args.wrong_passcode {
        // One digit out, which is what a misread across a room produces. `- 1` rather
        // than `+ 1` so the result stays inside the spec's range at the top end, and
        // guarded at the bottom because passcode 0 is forbidden.
        let wrong = if passcode <= 1 { 2 } else { passcode - 1 };
        println!("matter-peer: typed {wrong} instead of {passcode}, on purpose");
        wrong
    } else {
        println!("matter-peer: typed the passcode");
        passcode
    };

    // ---- Phase 3: become a commissionable node --------------------------------------

    // The DAC key. Unlike the panel's, this one *is* checked: Matter Casting inverts
    // attestation too, and it is the client that presents a Device Attestation
    // Certificate during commissioning. `TEST_DEV_ATT` is the test chain, which is why
    // the panel is run with `allow_test_attestation`.
    let mut dac_key = rs_matter::crypto::CanonPkcSecretKey::new();
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, dac_key.access_mut());
    let crypto = rs_matter::crypto::default_crypto(rand_core::OsRng, dac_key.reference());
    let rand = crypto.rand()?;

    let dev_det = BasicInfoConfig {
        vid: args.app_vendor_id,
        pid: args.app_product_id,
        hw_ver: 1,
        sw_ver: 1,
        sw_ver_str: "1",
        serial_no: "matter-peer",
        device_name: &args.device_name,
        vendor_name: "castaway",
        product_name: "matter-peer",
        hw_ver_str: "1",
        ..Default::default()
    };

    // The passcode the panel chose, as this node's PASE secret. This is the whole point
    // of the file poll above: the number cannot be known until the panel has picked it.
    let dev_comm = BasicCommData {
        password: Spake2pVerifierPassword::new_from_ref(Spake2pVerifierPasswordRef::new(
            &passcode.to_le_bytes(),
        )),
        discriminator: args.discriminator,
    };

    let matter = Matter::new(&dev_det, dev_comm, &TEST_DEV_ATT, args.matter_port);

    const NODE: Node<'static> = Node {
        endpoints: &[root_endpoint!(eth)],
    };
    let dm = (NODE, EthSysHandlerBuilder::new().build(rand));

    let buffers = MatterBuffers::<4>::new();
    let im_state = InteractionModelState::<DummyNetworks, 3, 0>::new(DummyNetworks);
    let kv = matter.kv(DummyKvBlobStore);
    let im = InteractionModel::new(&matter, &crypto, &buffers, dm, &kv, &im_state);
    let responder = DefaultResponder::new(&im);

    matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, &crypto, &())?;

    let (send, recv) =
        proto_matter::net::bind(SocketAddr::new(args.bind, args.matter_port)).await?;

    // The `_matterc._udp` record the panel goes looking for. The instance label is what
    // `await_commissionable` matches against the name in the UDC declaration — get it
    // wrong and the panel waits out its sixty seconds on a node that is right there.
    let mut mdns = MdnsResponder::new()?;
    if !args.bind.is_unspecified() {
        mdns.restrict_to(args.bind)?;
    }
    mdns.advertise(&commissionable_service(&args))?;

    tracing::info!(
        instance = %args.instance,
        discriminator = args.discriminator,
        port = args.matter_port,
        "matter-peer: commissionable, and advertising it"
    );

    let script = async {
        // Only now: the panel starts browsing when this arrives, and a record that is not
        // up yet is a browse that resolves nothing.
        let mut ready = declaration;
        ready.commissioner_passcode_ready = true;
        udc.send(&ready).await?;
        println!("matter-peer: told the panel the passcode is in");

        if args.expects_refusal() {
            // The other half of #198: a commissioning attempt that fails used to tell the
            // client nothing at all, so a phone whose user mistyped got silence and its UI
            // had to guess — and what it usually guesses is a timeout.
            //
            // The transport keeps running underneath this select: a wrong *passcode* is
            // only refused after the panel has actually tried PASE against us, which needs
            // us answering.
            let cd = udc.reply_within(REFUSAL_TIMEOUT).await?;
            println!(
                "matter-peer: the panel refused with {:?} ({})",
                cd.error_code, cd.error_code as u16
            );
            return Ok(());
        }

        run_script(&matter, &crypto, &args).await
    };

    let transport = matter.run(&crypto, send, recv, NoNetwork);

    let outcome = tokio::select! {
        r = transport => r.map_err(Into::into).and(Err("the transport stopped".into())),
        r = im.run() => r.map_err(Into::into).and(Err("the data model stopped".into())),
        r = responder.run::<4, 4>() => {
            r.map_err(Into::into).and(Err("the responder stopped".into()))
        }
        r = script => r,
    };

    let outcome: Result<(), Box<dyn std::error::Error>> = outcome;
    outcome?;

    // The sentinel. Asserted on by nix/matter-vm-test.nix.
    println!("matter-peer completed");
    Ok(())
}

/// Everything after the second declaration: get commissioned, then cast.
async fn run_script<C: rs_matter::crypto::Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error>> {
    let fab_idx = await_commissioned(matter).await?;
    println!("matter-peer: commissioned onto the panel's fabric, index {fab_idx}");

    // The panel's node id on the fabric it administers. A commissionee would ordinarily
    // learn this as `AddNOC`'s `caseAdminSubject`; `rs-matter` does not expose the ACL
    // subjects it seeds from that field, so the constant is read from the crate that
    // wrote it. That makes this an assertion about our commissioning either way: if the
    // panel put a different id in `AddNOC`, the ACL it seeded will not match this
    // subject and the invoke below comes back `UnsupportedAccess`.
    let player_node_id = CastingCa::panel_node_id();

    let deadline = tokio::time::Instant::now() + INVOKE_WINDOW;
    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        match launch_url(matter, crypto, fab_idx, player_node_id, args).await {
            Ok(()) => {
                println!("matter-peer: LaunchURL accepted on attempt {attempt}");
                return Ok(());
            }
            Err(e) if tokio::time::Instant::now() < deadline => {
                // Expected for the first attempt or two: `AddNOC` commits the fabric
                // before `CommissioningComplete`, so this can arrive while the panel is
                // still finishing. Logged rather than swallowed so a *persistent* refusal
                // reads as many identical failures instead of one late one.
                tracing::info!(attempt, error = %e, "matter-peer: LaunchURL not yet, retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => {
                return Err(format!("LaunchURL failed after {attempt} attempts: {e}").into());
            }
        }
    }
}

/// Open CASE back to the panel and invoke `ContentLauncher::LaunchURL`.
///
/// The direction that makes this Matter *Casting*: the panel commissioned us, and now we
/// are the one driving it. [`Exchange::initiate`] reuses the CASE session established
/// during `complete_via_case` — which matters, because the panel does not advertise an
/// operational `_matter._tcp` record for us to resolve it by.
async fn launch_url<C: rs_matter::crypto::Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    fab_idx: core::num::NonZeroU8,
    player_node_id: u64,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error>> {
    let exchange = Exchange::initiate(matter, crypto, fab_idx, player_node_id).await?;

    let handle = exchange
        .content_launcher()
        .launch_url(args.endpoint, |builder| {
            builder
                .content_url(&args.url)?
                .display_string(Some(args.display_string.as_str()))?
                // Absent, not empty. `BrandingInformation` is a rendering hint for the
                // app's own splash; a client casting a bare URL has none to give.
                .branding_information()?
                .none()
                .end()
        })
        .await?;

    {
        let response = handle.response()?;
        let status = response.status()?;
        let data = response.data()?.map(ToString::to_string);
        println!("matter-peer: LauncherResponse status={status:?} data={data:?}");
        if !matches!(
            status,
            rs_matter::dm::clusters::decl::content_launcher::StatusEnum::Success
        ) {
            return Err(format!("the panel refused the launch: {status:?}").into());
        }
    }
    handle.complete().await?;

    Ok(())
}

/// Poll until the panel has put us on its fabric.
async fn await_commissioned(
    matter: &Matter<'_>,
) -> Result<core::num::NonZeroU8, Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + COMMISSION_TIMEOUT;
    loop {
        if let Some(fab_idx) = matter.with_state(|state| {
            state
                .fabrics
                .iter()
                .next()
                .map(rs_matter::fabric::Fabric::fab_idx)
        }) {
            return Ok(fab_idx);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("the panel never commissioned us".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The `_matterc._udp` record, with the commissionable-node TXT keys of Core §4.3.1.
fn commissionable_service(args: &Args) -> MdnsService {
    let host = hostname();
    MdnsService::new(
        proto_matter::discovery::COMMISSIONABLE_SERVICE,
        args.advertised_instance(),
        &host,
        args.matter_port,
    )
    .with_txt("D", args.discriminator.to_string())
    // 1 — the window was opened by a user action rather than being always open.
    .with_txt("CM", "1")
    .with_txt("VP", format!("{}+{}", args.app_vendor_id, args.app_product_id))
    .with_txt("DN", args.device_name.clone())
    .with_txt("SII", "500")
    .with_txt("SAI", "300")
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "matter-peer".into())
}

/// Wait for the harness to write the number that is on the panel's screen.
async fn await_passcode(path: &PathBuf) -> Result<u32, Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + PASSCODE_TIMEOUT;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            // The panel formats it for a person to read (`1234-5678`); accept either.
            let digits: String = text.chars().filter(char::is_ascii_digit).collect();
            if digits.len() == 8 {
                return Ok(digits.parse()?);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("nobody wrote a passcode to {}", path.display()).into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn declaration(args: &Args, udc: &UdcSocket) -> IdentificationDeclaration {
    IdentificationDeclaration {
        instance_name: args.instance.clone(),
        vendor_id: Some(args.app_vendor_id),
        product_id: Some(args.app_product_id),
        device_name: Some(args.device_name.clone()),
        // The port the panel must answer on, which is not the well-known one: a client
        // listens where it chooses, and this is the field that says so.
        cd_port: Some(udc.port()),
        pairing_hint: None,
        pairing_instruction: None,
        rotating_id: None,
        target_apps: vec![TargetApp {
            vendor_id: args.app_vendor_id,
            product_id: args.app_product_id,
        }],
        no_passcode: false,
        cd_upon_passcode_dialog: true,
        // Ask the *panel* to pick the passcode and show it. The other flow — the client
        // displays a number for someone to type into the TV — is not what a phone with a
        // TV across the room does.
        commissioner_passcode: true,
        commissioner_passcode_ready: false,
        cancel_passcode: false,
        passcode_length: Some(8),
    }
}

/// The client's UDC socket: one port, used for both declarations and the reply.
struct UdcSocket {
    socket: UdpSocket,
    player: SocketAddr,
}

impl UdcSocket {
    #[expect(
        clippy::disallowed_methods,
        reason = "not the panel's socket — this is the scripted client's ephemeral \
                  `cdPort`, which is the receiver's listener table's business only \
                  insofar as it must not be assumed to be 5550"
    )]
    async fn bind(bind: IpAddr, player: SocketAddr) -> Result<Self, Box<dyn std::error::Error>> {
        let socket = UdpSocket::bind(SocketAddr::new(bind, 0)).await?;
        Ok(Self { socket, player })
    }

    fn port(&self) -> u16 {
        self.socket.local_addr().map_or(0, |a| a.port())
    }

    async fn send(&self, id: &IdentificationDeclaration) -> Result<(), Box<dyn std::error::Error>> {
        self.socket.send_to(&id.encode()?, self.player).await?;
        Ok(())
    }

    async fn reply(&self) -> Result<CommissionerDeclaration, Box<dyn std::error::Error>> {
        self.reply_within(CD_TIMEOUT).await
    }

    async fn reply_within(
        &self,
        within: Duration,
    ) -> Result<CommissionerDeclaration, Box<dyn std::error::Error>> {
        let mut buf = [0u8; 1024];
        let (len, _) = tokio::time::timeout(within, self.socket.recv_from(&mut buf))
            .await
            .map_err(|_| "the panel never answered the declaration")??;
        Ok(CommissionerDeclaration::decode(&buf[..len])?)
    }
}
