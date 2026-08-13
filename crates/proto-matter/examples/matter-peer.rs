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
use std::path::{Path, PathBuf};
use std::time::Duration;

use proto_matter::fabric::CastingCa;
use proto_matter::player::PLAYER_ENDPOINT;
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
use rs_matter::persist::{DirKvBlobStore, DummyKvBlobStore, KvBlobStore};
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::{
    Spake2pVerifierPassword, Spake2pVerifierPasswordRef, MAX_COMM_WINDOW_TIMEOUT_SECS,
};
use rs_matter::transport::exchange::{Exchange, MatterBuffers};
use rs_matter::transport::network::mdns::{DottedName, MdnsRemoteService};
use rs_matter::transport::network::NoNetwork;
use rs_matter::{root_endpoint, BasicCommData, Matter};

use substrate_mdns::{BrowseEvent, MdnsResponder, MdnsService};

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

/// How long a `--cast-again` run keeps retrying `LaunchURL`.
///
/// Wider than [`INVOKE_WINDOW`] because every failed attempt here already contains
/// `rs-matter`'s own 5 s resolve timeout, and the panel this run aims at may still be
/// bringing its sockets up after the restart that killed the CASE session.
const CAST_AGAIN_WINDOW: Duration = Duration::from_secs(60);

/// How long one browse serves one resolve request before giving up on it.
///
/// Longer than the requester's own 5 s timeout on purpose: an answer that arrives just
/// after the caller gave up still refreshes the daemon's cache for the retry.
const RESOLVE_BROWSE_WINDOW: Duration = Duration::from_secs(10);

struct Args {
    player: IpAddr,
    bind: IpAddr,
    /// Absent only in `--cast-again` mode, where no passcode exists to be typed.
    passcode_file: Option<PathBuf>,
    /// Where the phone's own Matter state (its fabric, above all) persists. Without it
    /// the run is the one-shot phone the harness always had; with it, a later
    /// `--cast-again` run can be the same phone coming back.
    state_dir: Option<PathBuf>,
    /// Skip commissioning entirely: load the fabric persisted by an earlier run and cast
    /// over a CASE session established from the panel's operational record (#173).
    cast_again: bool,
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
    /// Send a cancel for this instance and exit: the phone whose user backed out.
    ///
    /// Used by the overlap scenario (#209): a phone whose pairing was refused cancels on
    /// its way out, and the panel must not take the *other* phone's passcode down.
    cancel_only: bool,
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
    /// What to do to the panel once commissioned, past the `LaunchURL` (#196).
    ///
    /// Every one of these is a cluster handler that no test had ever executed: they need
    /// a `ReadContext` or an `InvokeContext`, which in `rs-matter` means a real
    /// transaction, which means a peer. This is the peer, so it may as well drive them.
    probes: Probes,
}

/// The post-commissioning probes, all off by default.
///
/// Off so the runs that exist keep proving what they proved — the commissioning path is
/// what those assert, and a probe that failed would fail them for an unrelated reason.
#[derive(Debug, Default)]
struct Probes {
    /// Read the Descriptor on the root, the player and the app endpoint — including the
    /// Descriptor cluster's own attribute and command lists, which no client had ever
    /// read (#283).
    descriptor: bool,
    /// Drive the seek surface on the player endpoint (#283): read `Duration` and the
    /// seek range, then `SkipForward`, `SkipBackward`, a `Seek`, and the two variable-
    /// speed verbs the panel refuses. Fixed operands, so the VM asserts exact numbers.
    playback: bool,
    /// Read all seven `ApplicationBasic` attributes on the app endpoint.
    app_basic: bool,
    /// Invoke these `MediaPlayback` commands, in order, on the player endpoint.
    transport: Vec<String>,
    /// Send these CEC keys through `KeypadInput`, in order, *before* the transport
    /// sequence — while the launch above still has the panel playing (#274).
    keys: Vec<String>,
    /// Send these CEC keys *after* the transport sequence — the `stop` in it is what
    /// makes `InvalidKeyInCurrentState` reachable (#274).
    keys_idle: Vec<String>,
    /// Drive `ApplicationLauncher` (#274): read `CatalogList`, launch the second app by
    /// its catalog entry, read `CurrentApp` back, aim at an app the panel does not host,
    /// then stop the app and read `CurrentApp` again. Fixed operands, like the playback
    /// battery, so the VM asserts exact answers.
    app_launcher: bool,
    /// Read `TargetList` and `CurrentTarget`, then `NavigateTarget` to this identifier.
    navigate: Option<u8>,
    /// `LaunchContent` this query at the app endpoint.
    launch_content: Option<String>,
    /// Read the Access Control cluster's `ACL` attribute, which needs Administer.
    ///
    /// The panel seeds a commissioned client with `Operate` and says in a comment why not
    /// `Administer` — "handing out Administer because it is simpler would let any
    /// commissioned phone evict every other one". Nothing tested it, so a regression to
    /// `ADMINISTER` passed every check in the repo. This is the read that has to fail.
    read_acl: bool,
    /// Which endpoint the app-level probes address. The panel's first content app.
    app_endpoint: u16,
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
     [--matter-port <n>] [--state-dir <dir>] [--declare-only] [--cancel-only] \
     [--wrong-passcode] \
     [--wrong-instance] [--read-descriptor] [--playback-probes] [--app-basic] \
     [--read-acl] [--app-endpoint <n>] [--transport <verb>[,<verb>…]] \
     [--send-key <key>[,<key>…]] [--send-key-idle <key>[,<key>…]] [--app-launcher] \
     [--navigate <n>] [--launch-content <query>]\n\
     or:    matter-peer --player <ip> --cast-again --state-dir <dir> [--bind <ip>] \
     [--url <str>] [--display-string <str>] [--endpoint <n>] [--matter-port <n>]"
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
    let mut state_dir = None;
    let mut cast_again = false;
    let mut declare_only = false;
    let mut cancel_only = false;
    let mut wrong_passcode = false;
    let mut wrong_instance = false;
    // `FIRST_CONTENT_APP_ENDPOINT`: the panel packs content apps densely from 6, and the
    // default catalogue has exactly one.
    let mut probes = Probes {
        app_endpoint: 6,
        ..Probes::default()
    };

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
            "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
            "--cast-again" => cast_again = true,
            "--declare-only" => declare_only = true,
            "--cancel-only" => cancel_only = true,
            "--wrong-passcode" => wrong_passcode = true,
            "--wrong-instance" => wrong_instance = true,
            "--read-descriptor" => probes.descriptor = true,
            "--playback-probes" => probes.playback = true,
            "--app-basic" => probes.app_basic = true,
            "--read-acl" => probes.read_acl = true,
            "--app-endpoint" => probes.app_endpoint = value()?.parse()?,
            // Comma-separated so one run can drive the transport through a sequence, which
            // is the only way `NotActive` and `Success` are both reachable in one session.
            "--transport" => probes
                .transport
                .extend(value()?.split(',').map(str::to_owned)),
            "--send-key" => probes.keys.extend(value()?.split(',').map(str::to_owned)),
            "--send-key-idle" => probes
                .keys_idle
                .extend(value()?.split(',').map(str::to_owned)),
            "--app-launcher" => probes.app_launcher = true,
            "--navigate" => probes.navigate = Some(value()?.parse()?),
            "--launch-content" => probes.launch_content = Some(value()?),
            other => return Err(format!("unknown argument {other:?}\n{}", usage()).into()),
        }
    }

    if !cast_again && !cancel_only && passcode_file.is_none() {
        // Commissioning is a person typing a number; the file is how the harness plays
        // the person. The come-back run has no number to type, and the cancelling run
        // never gets far enough to want one.
        return Err(usage().into());
    }
    if cast_again && state_dir.is_none() {
        return Err(format!("--cast-again needs --state-dir\n{}", usage()).into());
    }

    Ok(Args {
        player: player.ok_or_else(usage)?,
        bind: bind.unwrap_or(IpAddr::from([0, 0, 0, 0])),
        passcode_file,
        state_dir,
        cast_again,
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
        cancel_only,
        wrong_passcode,
        wrong_instance,
        probes,
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

    if args.cast_again {
        // The phone that comes back. No UDC, no passcode, no commissioning — the whole
        // point is that none of that should be needed twice.
        return cast_again(&args).await;
    }

    // ---- Phase 1: the UDC exchange, from the client's side -------------------------

    let udc = UdcSocket::bind(
        args.bind,
        SocketAddr::new(args.player, proto_matter::UDC_PORT),
    )
    .await?;
    let declaration = declaration(&args, &udc);

    if args.cancel_only {
        // The phone whose user backed out (#209). One cancel, one acknowledgement, and
        // out — whether the panel takes anything off its screen for it is the panel's
        // decision, and the VM test reads that off the panel's own journal.
        let mut cancel = declaration;
        cancel.cancel_passcode = true;
        udc.send(&cancel).await?;
        let cd = udc.reply().await?;
        if !cd.cancel_passcode {
            return Err(format!("the panel did not acknowledge the cancel: {cd:?}").into());
        }
        println!("matter-peer: cancel acknowledged");
        return Ok(());
    }

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

    let passcode_file = args
        .passcode_file
        .as_ref()
        .ok_or("a commissioning run needs --passcode-file")?;
    let passcode = await_passcode(passcode_file).await?;
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

    let dev_det = dev_details(&args);

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
    // With `--state-dir`, everything commissioning grants this node — its fabric, its
    // operational certificate, its keys — survives the process, and a later
    // `--cast-again` run is the same phone coming back rather than a stranger.
    let kv = matter.kv(PeerStore::open(args.state_dir.as_deref())?);
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
            let cd = udc.await_refusal(REFUSAL_TIMEOUT).await?;
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
                return run_probes(matter, crypto, fab_idx, player_node_id, args).await;
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

/// Drive whichever cluster handlers this run was asked to drive (#196).
///
/// Every probe is one IM transaction and therefore one exchange, because a cluster's
/// client view consumes the exchange it was entered from. Each prints a line the VM test
/// asserts on rather than returning a structure: what is under test is what the *panel*
/// answered, and the shortest honest way to carry that out of a `no_std`-shaped API is to
/// print it.
async fn run_probes<C: rs_matter::crypto::Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    fab_idx: core::num::NonZeroU8,
    player: u64,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error>> {
    use rs_matter::dm::clusters::decl::access_control::AccessControlClient as _;
    use rs_matter::dm::clusters::decl::application_basic::ApplicationBasicClient as _;
    use rs_matter::dm::clusters::decl::application_launcher::ApplicationLauncherClient as _;
    use rs_matter::dm::clusters::decl::content_launcher::{self, ContentLauncherClient as _};
    use rs_matter::dm::clusters::decl::descriptor::DescriptorClient as _;
    use rs_matter::dm::clusters::decl::media_playback::MediaPlaybackClient as _;
    use rs_matter::dm::clusters::decl::target_navigator::TargetNavigatorClient as _;

    let probes = &args.probes;
    let app = probes.app_endpoint;

    if probes.descriptor {
        // The root, the player and one content app: the whole of what a commissioned
        // phone discovers about this panel, read by a client rather than asserted against
        // our own constructor. `NodeTree` had no coverage of any kind until `4cd7373`,
        // and none of it went through the interaction model.
        for endpoint in [0, PLAYER_ENDPOINT, app] {
            let parts = Exchange::initiate(matter, crypto, fab_idx, player)
                .await?
                .descriptor()
                .parts_list_read_with(endpoint, |v| {
                    v.map(|list| {
                        list.iter()
                            .filter_map(Result::ok)
                            .map(|p| p.to_string())
                            .collect::<Vec<_>>()
                    })
                })
                .await??;
            let servers = Exchange::initiate(matter, crypto, fab_idx, player)
                .await?
                .descriptor()
                .server_list_read_with(endpoint, |v| {
                    v.map(|list| {
                        list.iter()
                            .filter_map(Result::ok)
                            .map(|c| format!("{c:#06x}"))
                            .collect::<Vec<_>>()
                    })
                })
                .await??;
            let types = Exchange::initiate(matter, crypto, fab_idx, player)
                .await?
                .descriptor()
                .device_type_list_read_with(endpoint, |v| {
                    v.map(|list| {
                        list.iter()
                            .filter_map(Result::ok)
                            .filter_map(|d| d.device_type().ok())
                            .map(|t| format!("{t:#010x}"))
                            .collect::<Vec<_>>()
                    })
                })
                .await??;
            println!(
                "matter-peer: descriptor endpoint={endpoint} \
                 device_types={types:?} servers={servers:?} parts={parts:?}"
            );
        }

        // The Descriptor cluster's *own* attribute and command lists, still unread after
        // everything above (#283): the global list attributes are served by `rs-matter`
        // from the cluster metadata, and what the metadata claims is exactly what a
        // strict client walks before touching anything else.
        let attributes = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .descriptor()
            .attribute_list_read_with(PLAYER_ENDPOINT, |v| {
                v.map(|list| {
                    list.iter()
                        .filter_map(Result::ok)
                        .map(|a| format!("{a:#06x}"))
                        .collect::<Vec<_>>()
                })
            })
            .await??;
        let accepted = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .descriptor()
            .accepted_command_list_read_with(PLAYER_ENDPOINT, |v| {
                v.map(|list| {
                    list.iter()
                        .filter_map(Result::ok)
                        .map(|c| format!("{c:#06x}"))
                        .collect::<Vec<_>>()
                })
            })
            .await??;
        let generated = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .descriptor()
            .generated_command_list_read_with(PLAYER_ENDPOINT, |v| {
                v.map(|list| {
                    list.iter()
                        .filter_map(Result::ok)
                        .map(|c| format!("{c:#06x}"))
                        .collect::<Vec<_>>()
                })
            })
            .await??;
        println!(
            "matter-peer: descriptor lists endpoint={PLAYER_ENDPOINT} \
             attributes={attributes:?} accepted={accepted:?} generated={generated:?}"
        );
    }

    if probes.app_basic {
        // All seven, on the endpoint the app actually occupies. Each one comes out of the
        // catalogue entry rather than out of a constant, so reading them is what says the
        // endpoint a client picked is the app it thought it picked.
        let read = Exchange::initiate(matter, crypto, fab_idx, player).await?;
        let vendor_name = read
            .application_basic()
            .vendor_name_read_with(app, |v| v.map(ToString::to_string))
            .await??;
        let vendor_id = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .application_basic()
            .vendor_id_read(app)
            .await?;
        let product_id = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .application_basic()
            .product_id_read(app)
            .await?;
        let name = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .application_basic()
            .application_name_read_with(app, |v| v.map(ToString::to_string))
            .await??;
        let version = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .application_basic()
            .application_version_read_with(app, |v| v.map(ToString::to_string))
            .await??;
        let application = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .application_basic()
            .application_read_with(app, |v| {
                v.map(|s| {
                    format!(
                        "catalog={} app_id={}",
                        s.catalog_vendor_id().unwrap_or_default(),
                        s.application_id()
                            .map(ToString::to_string)
                            .unwrap_or_default()
                    )
                })
            })
            .await??;
        let status = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .application_basic()
            .status_read(app)
            .await?;
        // `AllowedVendorList` is the one attribute here a commissioned client is *not*
        // entitled to read: the spec gives it Administer, because it is the list a content
        // app would refuse a casting client by. So a refusal is the expected answer for a
        // client holding `Operate`, and it is better evidence for that grant than
        // `--read-acl` — this one is a media cluster a phone actually touches. Reported
        // rather than propagated: the whole point is that it does not succeed.
        let allowed = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .application_basic()
            .allowed_vendor_list_read_with(app, |v| match v {
                Ok(list) => format!("{} entries", list.iter().count()),
                Err(e) => format!("refused: {e}"),
            })
            .await
            .unwrap_or_else(|e| format!("refused: {e}"));
        println!(
            "matter-peer: application_basic endpoint={app} vendor_name={vendor_name:?} \
             vendor_id={vendor_id:#06x} product_id={product_id:#06x} name={name:?} \
             version={version:?} application=({application}) status={status:?} \
             allowed_vendors={allowed}"
        );
    }

    if probes.playback {
        // The seek surface (#283), while the LaunchURL above is still the item in
        // flight. Distinct line shapes from the `--transport` probes on purpose: the VM
        // matches `media_playback <verb> status=` for those, and these must not satisfy
        // or pollute that pattern.
        let duration = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback()
            .duration_read(PLAYER_ENDPOINT)
            .await?;
        let range_start = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback()
            .seek_range_start_read(PLAYER_ENDPOINT)
            .await?;
        let range_end = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback()
            .seek_range_end_read(PLAYER_ENDPOINT)
            .await?;
        println!(
            "matter-peer: playback duration={:?} seek_range_start={:?} seek_range_end={:?}",
            duration.into_option(),
            range_start.into_option(),
            range_end.into_option(),
        );

        // The other half of the seek surface, and what AdvancedSeek makes mandatory
        // (#310): where playback is, and when that was true.
        println!(
            "matter-peer: playback sampled_position {}",
            read_sampled_position(matter, crypto, fab_idx, player).await?
        );

        // Two skips whose targets compose (15 s forward, then 5 s back → 10 s): what the
        // panel forwards to its pipeline is the resolved absolute seek, and the VM reads
        // both numbers out of the panel's own journal.
        let handle = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback()
            .skip_forward(PLAYER_ENDPOINT, |builder| {
                builder.delta_position_milliseconds(15_000)?.end()
            })
            .await?;
        {
            let response = handle.response()?;
            println!(
                "matter-peer: playback skip-forward(15000) status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;

        let handle = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback()
            .skip_backward(PLAYER_ENDPOINT, |builder| {
                builder.delta_position_milliseconds(5_000)?.end()
            })
            .await?;
        {
            let response = handle.response()?;
            println!(
                "matter-peer: playback skip-backward(5000) status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;

        // An absolute seek. Against the null pipeline no duration is ever known, so the
        // honest answer is `SeekOutOfRange` — the refusal this probe exists to reach.
        let handle = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback()
            .seek(PLAYER_ENDPOINT, |builder| builder.position(60_000)?.end())
            .await?;
        {
            let response = handle.response()?;
            println!(
                "matter-peer: playback seek(60000) status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;

        // The two variable-speed verbs, refused with `SpeedOutOfRange`: the panel does
        // not advertise variable speed and says so rather than seeking and calling it
        // rewind.
        let handle = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback()
            .rewind(PLAYER_ENDPOINT, |builder| {
                builder.audio_advance_unmuted(None)?.end()
            })
            .await?;
        {
            let response = handle.response()?;
            println!(
                "matter-peer: playback rewind status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;

        let handle = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback()
            .fast_forward(PLAYER_ENDPOINT, |builder| {
                builder.audio_advance_unmuted(None)?.end()
            })
            .await?;
        {
            let response = handle.response()?;
            println!(
                "matter-peer: playback fast-forward status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;
    }

    // Keys while the panel is playing: the transport keys work, and a key the panel does
    // not have is refused with `UnsupportedKey` (#274).
    send_keys(matter, crypto, fab_idx, player, &probes.keys).await?;

    for verb in &probes.transport {
        // One exchange per verb, and the *response status* is the whole point: `NotActive`
        // when nothing is loaded and `Success` when something is, which is the guard
        // `MediaPlaybackHandler::drive` exists for and which nothing had run.
        let view = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback();
        let handle = match verb.as_str() {
            "play" => view.play(PLAYER_ENDPOINT).await?,
            "pause" => view.pause(PLAYER_ENDPOINT).await?,
            "stop" => view.stop(PLAYER_ENDPOINT).await?,
            "start-over" => view.start_over(PLAYER_ENDPOINT).await?,
            "previous" => view.previous(PLAYER_ENDPOINT).await?,
            "next" => view.next(PLAYER_ENDPOINT).await?,
            other => return Err(format!("unknown transport verb {other:?}").into()),
        };
        {
            let response = handle.response()?;
            println!(
                "matter-peer: media_playback {verb} status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;

        let state = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .media_playback()
            .current_state_read(PLAYER_ENDPOINT)
            .await?;
        println!("matter-peer: media_playback current_state={state:?}");

        // And what the position pair says at the same moment. The `stop` in this sequence
        // is what makes the `Null` case reachable from a client: nothing loaded is nothing
        // to sample, and an attribute that kept reporting a stale position through a stop
        // would have a phone drawing a scrubber for media that is gone (#310).
        println!(
            "matter-peer: media_playback sampled_position {}",
            read_sampled_position(matter, crypto, fab_idx, player).await?
        );
    }

    // Keys after the transport sequence — its `stop` is what makes a transport key with
    // nothing to act on reachable, and the answer has to be `InvalidKeyInCurrentState`,
    // not `UnsupportedKey`: the button exists, the moment is wrong (#274).
    send_keys(matter, crypto, fab_idx, player, &probes.keys_idle).await?;

    if let Some(target) = probes.navigate {
        let targets = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .target_navigator()
            .target_list_read_with(PLAYER_ENDPOINT, |v| {
                v.map(|list| {
                    list.iter()
                        .filter_map(Result::ok)
                        .map(|t| {
                            format!(
                                "{}:{}",
                                t.identifier().unwrap_or_default(),
                                t.name().map(ToString::to_string).unwrap_or_default()
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .await??;
        let current = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .target_navigator()
            .current_target_read(PLAYER_ENDPOINT)
            .await?;
        println!("matter-peer: target_navigator targets={targets:?} current={current}");

        let handle = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .target_navigator()
            .navigate_target(PLAYER_ENDPOINT, |builder| {
                builder.target(target)?.data(None)?.end()
            })
            .await?;
        {
            let response = handle.response()?;
            println!(
                "matter-peer: navigate_target target={target} status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;

        // And read it back. `SelectTarget` logs nothing on the panel and changes nothing
        // a session can see — the only place it is observable is the next read of this
        // attribute, which is also the only place a phone would look.
        let after = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .target_navigator()
            .current_target_read(PLAYER_ENDPOINT)
            .await?;
        println!("matter-peer: target_navigator current after navigate={after}");
    }

    if let Some(query) = &probes.launch_content {
        // One parameter, which the panel joins into a query string. `LaunchContent`'s
        // `{query}` template resolution is T0-tested; the *handler* that gets to it — the
        // `parameterList` walk and the refusal a non-searchable app deserves — is not.
        let handle = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .content_launcher()
            .launch_content(app, |builder| {
                builder
                    .search()?
                    .parameter_list()?
                    .push()?
                    .r#type(content_launcher::ParameterEnum::Any)?
                    .value(query.as_str())?
                    .external_id_list()?
                    .none()
                    .end()?
                    .end()?
                    .end()?
                    .auto_play(true)?
                    .data(None)?
                    // Absent, not defaulted: a client casting a search has no opinion
                    // about audio tracks, and none about reusing a context it never had.
                    .playback_preferences()?
                    .none()
                    .use_current_context(None)?
                    .end()
            })
            .await?;
        {
            let response = handle.response()?;
            println!(
                "matter-peer: launch_content query={query:?} endpoint={app} status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;
    }

    if probes.app_launcher {
        // The catalogue as a launchable platform (#274). CatalogList first: the catalogs
        // the panel's apps come from, which for the VM's two test apps is the one
        // CSA-reserved "vendor's own catalog", 0.
        let catalogs = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .application_launcher()
            .catalog_list_read_with(PLAYER_ENDPOINT, |v| {
                v.map(|list| {
                    list.iter()
                        .filter_map(Result::ok)
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                })
            })
            .await??;
        println!("matter-peer: application_launcher catalogs={catalogs:?}");

        // Launch the *second* app by its catalog entry — the same off-by-one honesty the
        // TargetNavigator probe buys: with one app every wrong lookup still lands right.
        launch_app(matter, crypto, fab_idx, player, "castaway.two").await?;
        read_current_app(matter, crypto, fab_idx, player).await?;

        // An app the panel does not host: refused in the cluster's own words, and the
        // selection above must survive the refusal.
        launch_app(matter, crypto, fab_idx, player, "com.example.absent").await?;

        // Stop it, and read the selection back — Null, the launcher's reserved "no app",
        // the same shape as TargetNavigator's CurrentTarget 0.
        let handle = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .application_launcher()
            .stop_app(PLAYER_ENDPOINT, |builder| {
                builder
                    .application()?
                    .some()?
                    .catalog_vendor_id(0)?
                    .application_id("castaway.two")?
                    .end()?
                    .end()
            })
            .await?;
        {
            let response = handle.response()?;
            println!(
                "matter-peer: application_launcher stop-app(castaway.two) status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;
        read_current_app(matter, crypto, fab_idx, player).await?;
    }

    if probes.read_acl {
        // The one probe whose *failure* is the pass. Reading the Access Control cluster's
        // ACL needs Administer, and the panel seeded this client with Operate — so a
        // success here means any commissioned phone can rewrite the access list and evict
        // every other one. The status is printed either way; the VM decides.
        let outcome = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .access_control()
            .acl_read_with(0, |v| match v {
                Ok(list) => format!("readable, {} entries", list.iter().count()),
                Err(e) => format!("refused: {e}"),
            })
            .await;
        match outcome {
            Ok(report) => println!("matter-peer: access_control acl {report}"),
            Err(e) => println!("matter-peer: access_control acl refused: {e}"),
        }
    }

    Ok(())
}

/// A CEC key by the name the command line spells it. Only what the probes send.
fn key_code(
    name: &str,
) -> Result<rs_matter::dm::clusters::decl::keypad_input::CECKeyCodeEnum, Box<dyn std::error::Error>>
{
    use rs_matter::dm::clusters::decl::keypad_input::CECKeyCodeEnum as Key;
    Ok(match name {
        "play" => Key::Play,
        "pause" => Key::Pause,
        "pause-play" => Key::PausePlayFunction,
        "stop" => Key::Stop,
        "forward" => Key::Forward,
        "backward" => Key::Backward,
        // Two keys the panel deliberately does not have: a menu key and a number key,
        // sent to prove the `UnsupportedKey` answer.
        "select" => Key::Select,
        "numbers5" => Key::Numbers5,
        other => return Err(format!("unknown key {other:?}").into()),
    })
}

/// Send a list of CEC keys through `KeypadInput`, one exchange each, printing the status
/// the panel answered (#274).
async fn send_keys<C: rs_matter::crypto::Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    fab_idx: core::num::NonZeroU8,
    player: u64,
    keys: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use rs_matter::dm::clusters::decl::keypad_input::KeypadInputClient as _;

    for name in keys {
        let key = key_code(name)?;
        let handle = Exchange::initiate(matter, crypto, fab_idx, player)
            .await?
            .keypad_input()
            .send_key(PLAYER_ENDPOINT, |builder| builder.key_code(key)?.end())
            .await?;
        {
            let response = handle.response()?;
            println!(
                "matter-peer: keypad_input key={name} status={:?}",
                response.status()?
            );
        }
        handle.complete().await?;
    }
    Ok(())
}

/// Read `MediaPlayback::SampledPosition` and print the pair a client would extrapolate
/// from (#310).
///
/// Read through the interaction model like everything else here, because the question the
/// attribute answers is a client's: `Null` when the panel has nothing to report, and
/// otherwise a position *and* a `UpdatedAt` in Matter epoch-µs that has to be a real
/// clock reading — a fabricated zero would have a client draw a position twenty-six years
/// extrapolated.
async fn read_sampled_position<C: rs_matter::crypto::Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    fab_idx: core::num::NonZeroU8,
    player: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    use rs_matter::dm::clusters::decl::media_playback::MediaPlaybackClient as _;

    let line = Exchange::initiate(matter, crypto, fab_idx, player)
        .await?
        .media_playback()
        .sampled_position_read_with(PLAYER_ENDPOINT, |sample| {
            let sample = sample?;
            Ok::<_, rs_matter::error::Error>(match sample.into_option() {
                None => "None".to_owned(),
                Some(position) => format!(
                    "position_ms={:?} updated_at_us={}",
                    position.position()?.into_option(),
                    position.updated_at()?
                ),
            })
        })
        .await??;
    Ok(line)
}

/// Invoke `ApplicationLauncher::LaunchApp` for an app in catalog 0 and print the status.
async fn launch_app<C: rs_matter::crypto::Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    fab_idx: core::num::NonZeroU8,
    player: u64,
    application_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use rs_matter::dm::clusters::decl::application_launcher::ApplicationLauncherClient as _;

    let handle = Exchange::initiate(matter, crypto, fab_idx, player)
        .await?
        .application_launcher()
        .launch_app(PLAYER_ENDPOINT, |builder| {
            builder
                .application()?
                .some()?
                .catalog_vendor_id(0)?
                .application_id(application_id)?
                .end()?
                .data(None)?
                .end()
        })
        .await?;
    {
        let response = handle.response()?;
        println!(
            "matter-peer: application_launcher launch-app({application_id}) status={:?}",
            response.status()?
        );
    }
    handle.complete().await?;
    Ok(())
}

/// Read `ApplicationLauncher::CurrentApp` and print it — the catalog entry and the
/// endpoint when an app is current, `null` when none is.
async fn read_current_app<C: rs_matter::crypto::Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    fab_idx: core::num::NonZeroU8,
    player: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    use rs_matter::dm::clusters::decl::application_launcher::ApplicationLauncherClient as _;

    let current = Exchange::initiate(matter, crypto, fab_idx, player)
        .await?
        .application_launcher()
        .current_app_read_with(PLAYER_ENDPOINT, |v| match v {
            Ok(nullable) => match nullable.into_option() {
                Some(app) => {
                    let catalog = app
                        .application()
                        .and_then(|a| a.catalog_vendor_id())
                        .unwrap_or_default();
                    let id = app
                        .application()
                        .and_then(|a| a.application_id().map(ToString::to_string))
                        .unwrap_or_default();
                    let endpoint = app.endpoint().ok().flatten().unwrap_or_default();
                    format!("(catalog={catalog} app_id={id} endpoint={endpoint})")
                }
                None => "null".to_string(),
            },
            Err(e) => format!("error: {e}"),
        })
        .await?;
    println!("matter-peer: application_launcher current_app={current}");
    Ok(())
}

/// Open CASE back to the panel and invoke `ContentLauncher::LaunchURL`.
///
/// The direction that makes this Matter *Casting*: the panel commissioned us, and now we
/// are the one driving it. [`Exchange::initiate`] reuses the CASE session established
/// during `complete_via_case` when one is live; with none — the `--cast-again` run — it
/// resolves the panel's operational `_matter._tcp` record and establishes a fresh one
/// (#173), which is why that run drives [`serve_resolves`] alongside this.
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

/// The device details both runs present. One function so the phone that comes back
/// (#173) is byte-for-byte the device the panel commissioned.
fn dev_details(args: &Args) -> BasicInfoConfig<'_> {
    BasicInfoConfig {
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
    }
}

/// `--cast-again`: the phone that comes back (#173).
///
/// The normal run's CASE session died with the process (or, in the VM test, with the
/// panel's restart). A real phone in that position resolves
/// `<compressed-fabric-id>-<node-id>._matter._tcp` and establishes a fresh CASE session
/// — `rs_matter::transport::Transport::initiate`'s second branch — which only works if
/// the panel actually publishes that record. Before it did, this function failed at the
/// resolve, which is the assertion the matter-vm scenario wrote first.
async fn cast_again(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = args
        .state_dir
        .as_deref()
        .ok_or("--cast-again needs --state-dir")?;

    // A DAC key is only presented during commissioning, which this run never does; the
    // crypto backend simply wants one to exist.
    let mut dac_key = rs_matter::crypto::CanonPkcSecretKey::new();
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, dac_key.access_mut());
    let crypto = rs_matter::crypto::default_crypto(rand_core::OsRng, dac_key.reference());
    let rand = crypto.rand()?;

    let dev_det = dev_details(args);

    // No commissioning window ever opens on this run, so the PASE verifier is the empty
    // one — the same stance the panel itself takes.
    let dev_comm = BasicCommData {
        password: Spake2pVerifierPassword::new(),
        discriminator: args.discriminator,
    };

    let matter = Matter::new(&dev_det, dev_comm, &TEST_DEV_ATT, args.matter_port);

    const NODE: Node<'static> = Node {
        endpoints: &[root_endpoint!(eth)],
    };
    let dm = (NODE, EthSysHandlerBuilder::new().build(rand));

    let buffers = MatterBuffers::<4>::new();
    let im_state = InteractionModelState::<DummyNetworks, 3, 0>::new(DummyNetworks);
    let kv = matter.kv(PeerStore::open(Some(state_dir))?);

    // The whole point: yesterday's fabric, not a fresh one. `CommissioningComplete` on
    // the earlier run is what persisted it.
    matter.load_persist(&kv).await?;
    let fab_idx = matter
        .with_state(|state| {
            state
                .fabrics
                .iter()
                .next()
                .map(rs_matter::fabric::Fabric::fab_idx)
        })
        .ok_or("no persisted fabric — run a commissioning pass with --state-dir first")?;
    println!(
        "matter-peer: loaded the fabric a previous run was commissioned onto, index {fab_idx}"
    );

    let im = InteractionModel::new(&matter, &crypto, &buffers, dm, &kv, &im_state);
    let responder = DefaultResponder::new(&im);

    let (send, recv) =
        proto_matter::net::bind(SocketAddr::new(args.bind, args.matter_port)).await?;

    // Browse-only this time — there is nothing to advertise, the panel is not
    // commissioning anybody — but `rs-matter`'s resolve still needs someone to actually
    // ask the LAN, and that someone is `serve_resolves` below.
    let mut mdns = MdnsResponder::new()?;
    if !args.bind.is_unspecified() {
        mdns.restrict_to(args.bind)?;
    }

    let script = async {
        let player_node_id = CastingCa::panel_node_id();
        let deadline = tokio::time::Instant::now() + CAST_AGAIN_WINDOW;
        let mut attempt = 0_u32;
        loop {
            attempt += 1;
            match launch_url(&matter, &crypto, fab_idx, player_node_id, args).await {
                Ok(()) => {
                    println!(
                        "matter-peer: cast again on attempt {attempt} — CASE re-established \
                         off the operational record"
                    );
                    return Ok(());
                }
                Err(e) if tokio::time::Instant::now() < deadline => {
                    // Each failure here is most likely the resolve timing out, which is
                    // exactly what an unfixed panel produces — logged per attempt so the
                    // terminal error reads as many identical failures, not one late one.
                    tracing::info!(attempt, error = %e, "matter-peer: cast again not yet, retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(e) => {
                    return Err(format!("cast again failed after {attempt} attempts: {e}").into());
                }
            }
        }
    };

    let transport = matter.run(&crypto, send, recv, NoNetwork);

    let outcome = tokio::select! {
        r = transport => r.map_err(Into::into).and(Err("the transport stopped".into())),
        r = im.run() => r.map_err(Into::into).and(Err("the data model stopped".into())),
        r = responder.run::<4, 4>() => {
            r.map_err(Into::into).and(Err("the responder stopped".into()))
        }
        r = serve_resolves(&matter, &mdns) => r.and(Err("the resolver stopped".into())),
        r = script => r,
    };

    let outcome: Result<(), Box<dyn std::error::Error>> = outcome;
    outcome?;

    // The same terminal sentinel as a commissioning run, so the VM test's "it finished"
    // check is one string in both scenarios.
    println!("matter-peer completed");
    Ok(())
}

/// Service `rs-matter`'s mDNS resolve requests from this project's own responder.
///
/// `Exchange::initiate` with no live CASE session parks a resolve request on the
/// transport and waits for "a running mDNS responder" (its words) to answer it. This
/// binary's responder is `substrate-mdns`, so this is the bridge: take the request,
/// browse its service type, and deposit every resolution —
/// `try_deposit_mdns_resolve` does the instance-name matching itself, so an answer for
/// some other node is simply ignored.
async fn serve_resolves(
    matter: &Matter<'_>,
    mdns: &MdnsResponder,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let service = matter.transport().wait_mdns_resolve_request().await;
        tracing::info!(?service, "matter-peer: resolving over mDNS");

        let mut browser = mdns.browse(service.service_type())?;
        let deadline = tokio::time::Instant::now() + RESOLVE_BROWSE_WINDOW;

        // Poll with a deadline rather than blocking on the browse: the requester gives
        // up on its own 5 s timer, and a browse that outlives its request would deposit
        // answers into nothing.
        while matter.transport().mdns_resolve_in_flight() {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("matter-peer: nothing on the LAN answered the resolve");
                break;
            }
            match tokio::time::timeout(Duration::from_millis(250), browser.next()).await {
                Ok(Some(BrowseEvent::Resolved(found))) => {
                    tracing::info!(
                        instance = %found.instance,
                        port = found.port,
                        addresses = ?found.addresses,
                        "matter-peer: resolve candidate"
                    );
                    let answer = MdnsRemoteService {
                        instance_name: DottedName(&found.fullname),
                        port: Some(found.port),
                        addrs: found.addresses.iter().copied(),
                        txt: found.txt.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                        scope_id: 0,
                    };
                    matter.transport().try_deposit_mdns_resolve(&answer);
                }
                // A removal is not an answer, and an expired 250 ms tick is just the
                // in-flight check coming round again.
                Ok(Some(BrowseEvent::Removed { .. })) | Err(_) => {}
                Ok(None) => return Err("the mDNS daemon shut down mid-browse".into()),
            }
        }
    }
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

/// The phone's persistence: nothing for the one-shot runs the harness started with, a
/// directory when the run is meant to be come back from (#173).
///
/// One enum rather than two code paths through `main`, because the type of the KV store
/// threads through `Matter::kv` and `InteractionModel::new` — a branch there would
/// duplicate the whole stack construction.
enum PeerStore {
    Ephemeral(DummyKvBlobStore),
    Durable(DirKvBlobStore),
}

impl PeerStore {
    fn open(dir: Option<&Path>) -> Result<Self, Box<dyn std::error::Error>> {
        match dir {
            None => Ok(Self::Ephemeral(DummyKvBlobStore)),
            Some(dir) => {
                // `DirKvBlobStore` writes `<dir>/k_<key>` and assumes the directory.
                std::fs::create_dir_all(dir)?;
                Ok(Self::Durable(DirKvBlobStore::new(dir.to_path_buf())))
            }
        }
    }
}

impl KvBlobStore for PeerStore {
    fn load<'a>(
        &mut self,
        key: u16,
        buf: &'a mut [u8],
    ) -> Result<Option<&'a [u8]>, rs_matter::error::Error> {
        match self {
            Self::Ephemeral(store) => store.load(key, buf),
            Self::Durable(store) => KvBlobStore::load(store, key, buf),
        }
    }

    fn store(
        &mut self,
        key: u16,
        data: &[u8],
        buf: &mut [u8],
    ) -> Result<(), rs_matter::error::Error> {
        match self {
            Self::Ephemeral(store) => store.store(key, data, buf),
            Self::Durable(store) => KvBlobStore::store(store, key, data, buf),
        }
    }

    fn remove(&mut self, key: u16, buf: &mut [u8]) -> Result<(), rs_matter::error::Error> {
        match self {
            Self::Ephemeral(store) => store.remove(key, buf),
            Self::Durable(store) => KvBlobStore::remove(store, key, buf),
        }
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

    /// Read declarations until one carries an error code, or `within` elapses.
    ///
    /// Not `reply_within`: the panel answers the `commissionerPasscodeReady` declaration
    /// immediately with "understood, going now" — `CdError::None` — and the outcome
    /// arrives a minute later on the same socket. Taking the first datagram as the answer
    /// reads the acknowledgement as the verdict, which is what a real client would do
    /// wrong too.
    async fn await_refusal(
        &self,
        within: Duration,
    ) -> Result<CommissionerDeclaration, Box<dyn std::error::Error>> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Err("the panel never said why commissioning stopped".into());
            }
            let cd = self.reply_within(left).await?;
            if cd.error_code != CdError::None {
                return Ok(cd);
            }
            tracing::debug!(
                ?cd,
                "matter-peer: an all-clear; still waiting for the outcome"
            );
        }
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
