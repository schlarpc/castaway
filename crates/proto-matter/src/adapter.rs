//! The actor: sockets, the Matter stack, the commissioner, and the bridge to the panel.
//!
//! Six things run concurrently for the life of the adapter, and each of them fails
//! differently:
//!
//! - `rs-matter`'s transport, over [`crate::net`]'s tokio socket.
//! - Its interaction model, and the responder that answers reads and invokes.
//! - [`crate::server`]'s UDC socket, which is what a phone talks to first.
//! - The **commissioning worker**, which is where the inversion actually happens: it takes
//!   a phone that has typed the passcode, finds it on mDNS, and runs PASE → `AddNOC` →
//!   CASE against it.
//! - The command pump, turning a [`CastCommand`] into a [`SessionEvent`].
//! - The prompt pump, putting the passcode on the glass.
//!
//! They are joined rather than spawned: `rs-matter` is built around borrowed state
//! (`Matter<'a>` borrows the device details, the commissioner borrows the fabric) and
//! spawning would demand `'static` for all of it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU8;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use castaway_core::{
    Advertisement, ControlTxn, CoreError, MediaUri, OsdMessage, OsdSink, ProtocolKind,
    SessionEvent, SessionSink, SourceAdapter,
};

use rs_matter::acl::{AclEntry, AuthMode};
use rs_matter::crypto::{Crypto, SecretKey, SigningSecretKey};
use rs_matter::dm::clusters::basic_info::BasicInfoConfig;
use rs_matter::dm::clusters::net_comm::DummyNetworks;
use rs_matter::dm::devices::test::TEST_DEV_ATT;
use rs_matter::dm::Privilege;
use rs_matter::im::{InteractionModel, InteractionModelState};
use rs_matter::persist::DummyKvBlobStore;
use rs_matter::respond::DefaultResponder;
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::transport::network::{Address, NoNetwork};
use rs_matter::{BasicCommData, Matter, MATTER_PORT};

use substrate_mdns::MdnsResponder;
use tokio::sync::{mpsc, Mutex};

use crate::error::MatterError;
use crate::fabric::{CastingCa, CommissionedClient, ADMIN_VENDOR_ID};
use crate::node::{handlers, CastingContext, NodeTree};
use crate::player::{
    CastCommand, Catalogue, PlaybackState, PlayerSnapshot, PlayerState, Surface, Transport,
};
use crate::server::{CommissionRequest, Prompt, UdcServer};
use crate::udc::{CommissionStage, CommissionerDeclaration};

/// How long the panel waits for a phone to appear as a commissionable node after it says
/// the passcode is typed. Generous: this is a wait on a person walking back to the sofa,
/// and the phone has to bring up its own advertisement first.
const COMMISSIONABLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A launch the panel's *browser* has to serve, because the app it was aimed at is a web
/// surface rather than a media URL.
///
/// Its own event stream rather than a [`SessionEvent`], for the same reason DIAL's is
/// (D24): the session manager drives the media pipeline, and "open this page" is not
/// something it can express. The app consumes these and drives the browser directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserLaunch {
    /// The page to open.
    pub url: String,
    /// What to call it on the now-playing surface.
    pub title: Option<String>,
    /// Which content-app endpoint asked, so a later `MediaPlayback` command can be
    /// attributed to the same app.
    pub app: crate::player::EndpointId,
}

/// How the adapter is set up.
#[derive(Debug, Clone)]
pub struct MatterConfig {
    /// The name shown in a phone's list of TVs.
    pub friendly_name: String,
    /// mDNS host label (`<host>.local.`).
    pub host: String,
    /// Where the fabric and the list of commissioned phones live.
    pub state_dir: PathBuf,
    /// The panel's CSA vendor id. `0xFFF1` is the test range, which is what this is.
    pub vendor_id: u16,
    /// The panel's product id.
    pub product_id: u16,
    /// Which apps this panel hosts, and what launching into them does.
    pub catalogue: Catalogue,
    /// The address to bind both UDP sockets on.
    pub bind: IpAddr,
}

impl Default for MatterConfig {
    fn default() -> Self {
        Self {
            friendly_name: "castaway".into(),
            host: "castaway".into(),
            state_dir: PathBuf::from("."),
            vendor_id: ADMIN_VENDOR_ID,
            product_id: 0x8001,
            catalogue: Catalogue::default(),
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        }
    }
}

/// The Matter Casting receiver.
pub struct MatterAdapter {
    config: MatterConfig,
    state: Arc<PlayerState>,
    osd: Option<OsdSink>,
    browser: Option<mpsc::UnboundedSender<BrowserLaunch>>,
    /// Taken by `run`; a second call finds it gone, which is what makes running twice an
    /// error rather than two half-initialised Matter stacks fighting over port 5540.
    once: Mutex<Option<()>>,
}

impl MatterAdapter {
    /// Build the adapter.
    #[must_use]
    pub fn new(config: MatterConfig) -> Self {
        Self {
            config,
            state: Arc::new(PlayerState::new()),
            osd: None,
            browser: None,
            once: Mutex::new(Some(())),
        }
    }

    /// Give the adapter an overlay to put the passcode on. Without one it still
    /// commissions — the passcode simply goes to the log, which is enough for a headless
    /// test and useless to a person.
    #[must_use]
    pub fn with_osd(mut self, osd: OsdSink) -> Self {
        self.osd = Some(osd);
        self
    }

    /// Give the adapter somewhere to send browser launches.
    ///
    /// Without one, a content app whose [`crate::LaunchTarget`] is a browser has nowhere
    /// to go — so a build with no browser should not declare browser apps in the first
    /// place, and the catalogue is where that is decided.
    #[must_use]
    pub fn with_browser(mut self, launches: mpsc::UnboundedSender<BrowserLaunch>) -> Self {
        self.browser = Some(launches);
        self
    }

    /// What the panel is playing, shared so the session manager can keep it current.
    #[must_use]
    pub fn player_state(&self) -> Arc<PlayerState> {
        Arc::clone(&self.state)
    }
}

#[async_trait::async_trait]
impl SourceAdapter for MatterAdapter {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::MatterCast
    }

    fn advertisements(&self) -> Vec<Advertisement> {
        // `_matterd._udp` — the commissioner service. Not `_matter._tcp`: the panel's
        // operational node exists only on a fabric it created, so there is nobody to
        // advertise it to until a phone has been commissioned, and that phone learns the
        // address during commissioning rather than by browsing.
        let service = crate::discovery::commissioner_service(
            &self.config.friendly_name,
            &self.config.host,
            crate::udc::UDC_PORT,
            self.config.vendor_id,
            self.config.product_id,
        );

        vec![Advertisement::MdnsService {
            ty: service.service_type,
            instance: service.instance.into_string(),
            port: service.port,
            txt: service.txt,
            // Matter discovery is by its own commissioning attributes in the TXT
            // record, not by DNS-SD sub-types.
            subtypes: Vec::new(),
        }]
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
        self.once
            .lock()
            .await
            .take()
            .ok_or_else(|| CoreError::Adapter("the Matter adapter was already run".into()))?;

        // `rs-matter` is a `no_std`-shaped stack: its mutexes are `NoopRawMutex`, its
        // random source is a `!Sync` handle, and its whole design assumes one task on one
        // core. None of that is a defect — it is what lets the same crate run on a
        // microcontroller — but it does mean the future is `!Send` and cannot live on the
        // shared runtime, which `SourceAdapter::run` requires.
        //
        // So it gets a thread and a current-thread runtime of its own, in the same spirit
        // as the decode threads (architecture §6). The bridge back is one channel carrying
        // one result; everything else the stack needs to say, it says through `sink`.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        std::thread::Builder::new()
            .name("matter".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(source) => {
                        let _ = done_tx.send(Err(MatterError::Io {
                            context: "starting the matter runtime",
                            source,
                        }));
                        return;
                    }
                };

                let _ = done_tx.send(runtime.block_on(self.serve(sink)));
            })
            .map_err(|e| CoreError::Adapter(format!("starting the matter thread: {e}")))?;

        done_rx
            .await
            .map_err(|_| CoreError::Adapter("the matter thread died without saying why".into()))?
            .map_err(|e| CoreError::Adapter(e.to_string()))
    }
}

impl MatterAdapter {
    async fn serve(&self, sink: SessionSink) -> Result<(), MatterError> {
        // The DAC key. The panel presents no device attestation of its own — it is the
        // *commissioner* here, and the certificate that gets checked is the phone's — so
        // this is a key that signs nothing anybody validates. It is still generated
        // rather than fixed, so two panels are not identical.
        let mut dac_key = rs_matter::crypto::CanonPkcSecretKey::new();
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, dac_key.access_mut());
        let crypto = rs_matter::crypto::default_crypto(rand_core::OsRng, dac_key.reference());
        let mut rand = crypto.rand().map_err(core_err)?;

        let mut ca = CastingCa::open(&self.config.state_dir, &crypto)?;

        let dev_det = BasicInfoConfig {
            vid: self.config.vendor_id,
            pid: self.config.product_id,
            hw_ver: 1,
            sw_ver: 1,
            sw_ver_str: env!("CARGO_PKG_VERSION"),
            serial_no: "castaway",
            device_name: &self.config.friendly_name,
            vendor_name: "castaway",
            product_name: "castaway",
            hw_ver_str: "1",
            ..Default::default()
        };

        // Commissioning data for a window this node never opens: the panel is not
        // commissionable, it is the commissioner. `rs-matter` still wants the field.
        let dev_comm = BasicCommData {
            // Never used: the panel does not open a commissioning window on itself, so
            // there is no passcode for anyone to type *into* it.
            password: rs_matter::sc::pase::Spake2pVerifierPassword::new(),
            discriminator: 0,
        };

        let matter = Matter::new(&dev_det, dev_comm, &TEST_DEV_ATT, MATTER_PORT);

        let fab_idx = install_fabric(&matter, &crypto, &ca)?;
        seed_acls(&matter, fab_idx, ca.clients())?;

        let tree = NodeTree::new(&self.config.catalogue);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (outcome_tx, outcome_rx) = mpsc::unbounded_channel();

        let ctx = Arc::new(CastingContext {
            catalogue: self.config.catalogue.clone(),
            state: Arc::clone(&self.state),
            commands: command_tx,
        });

        // Four buffers: the responder is run with four concurrent exchanges below, and a
        // pool smaller than that deadlocks under a client that pipelines.
        let buffers = MatterBuffers::<4>::new();
        let im_state = InteractionModelState::<DummyNetworks, 3, 0>::new(DummyNetworks);
        let kv = matter.kv(DummyKvBlobStore);

        let dm = InteractionModel::new(
            &matter,
            &crypto,
            &buffers,
            (tree.node(), handlers(&ctx, rand)),
            &kv,
            &im_state,
        );
        let responder = DefaultResponder::new(&dm);

        let (send, recv) = crate::net::bind(SocketAddr::new(self.config.bind, MATTER_PORT)).await?;

        let udc = UdcServer::bind(
            SocketAddr::new(self.config.bind, crate::udc::UDC_PORT),
            self.config.catalogue.clone(),
            prompt_tx,
            request_tx,
            outcome_rx,
        )
        .await?;

        // The browse handle. Its own daemon, like GameStream's: the app's shared
        // responder is built for advertising, and this is the one place in the crate that
        // needs to *look* for something.
        let responder_mdns = MdnsResponder::new()?;

        tracing::info!(
            udc = crate::udc::UDC_PORT,
            matter = MATTER_PORT,
            clients = ca.clients().len(),
            "matter: casting receiver up"
        );

        let transport = matter.run(&crypto, send, recv, NoNetwork);
        let im = dm.run();
        let answers = responder.run::<4, 4>();
        let udc = udc.run(&mut rand);
        let cx = Commissioning {
            matter: &matter,
            crypto: &crypto,
            fab_idx,
            mdns: &responder_mdns,
            outcomes: &outcome_tx,
        };
        let commissioning = self.commission_loop(&cx, &mut ca, request_rx);
        let commands = self.pump_commands(command_rx, &sink);
        let prompts = self.pump_prompts(prompt_rx);

        // Any one of them ending ends the adapter: they are halves of one service, and a
        // transport that stopped while the UDC socket kept answering would put passcodes
        // on the screen for a stack that cannot commission anybody.
        tokio::select! {
            r = transport => r.map_err(core_err),
            r = im => r.map_err(core_err),
            r = answers => r.map_err(core_err),
            r = udc => r,
            r = commissioning => r,
            r = commands => r,
            () = prompts => Ok(()),
        }
    }

    /// Take one phone at a time from "the passcode is typed" to "on the fabric".
    async fn commission_loop<C: rs_matter::crypto::Crypto>(
        &self,
        cx: &Commissioning<'_, '_, C>,
        ca: &mut CastingCa,
        mut requests: mpsc::UnboundedReceiver<CommissionRequest>,
    ) -> Result<(), MatterError> {
        while let Some(request) = requests.recv().await {
            tracing::info!(
                instance = %request.instance,
                device = %request.device_name,
                source = %request.source,
                "matter: commissioning a casting client"
            );

            match self.commission_one(cx, ca, &request).await {
                Ok(node_id) => {
                    ca.remember(CommissionedClient {
                        node_id,
                        instance: request.instance.to_string(),
                        name: request.device_name.clone(),
                    })?;
                    seed_acls(cx.matter, cx.fab_idx, ca.clients())?;
                    self.show(OsdMessage::banner(
                        format!("{} can now cast", request.device_name),
                        Duration::from_secs(5),
                    ));
                    tracing::info!(node_id, "matter: casting client commissioned");
                }
                Err(e) => {
                    // One phone failing is not the adapter failing: the next request is
                    // still worth serving, and the person can simply try again.
                    tracing::warn!(
                        instance = %request.instance,
                        error = %e,
                        "matter: commissioning failed"
                    );
                    self.show(OsdMessage::banner(
                        format!("could not pair {}", request.device_name),
                        Duration::from_secs(5),
                    ));

                    // …and tell the phone, which is otherwise left with silence and a UI
                    // that has to guess — and what it usually guesses is a timeout. The
                    // code is derived from the typed stage rather than from the message,
                    // so a new step in the flow is a compile error rather than a
                    // plausible wrong code (#198).
                    if let (Some(to), Some(error_code)) =
                        (request.reply_to, e.commissioner_declaration_error())
                    {
                        tracing::info!(
                            %to, ?error_code,
                            "matter: telling the client why commissioning stopped"
                        );
                        let _ = cx.outcomes.send(crate::server::Outcome {
                            to,
                            declaration: CommissionerDeclaration {
                                error_code,
                                ..CommissionerDeclaration::default()
                            },
                        });
                    }
                }
            }

            self.clear_osd();
        }

        Ok(())
    }

    async fn commission_one<C: rs_matter::crypto::Crypto>(
        &self,
        cx: &Commissioning<'_, '_, C>,
        ca: &CastingCa,
        request: &CommissionRequest,
    ) -> Result<u64, MatterError> {
        let found = crate::discovery::await_commissionable(
            cx.mdns,
            &request.instance,
            COMMISSIONABLE_TIMEOUT,
        )
        .await?;

        // A returning phone keeps the node id it already has, so its access-control entry
        // and any binding it cached stay valid.
        let node_id = ca
            .client_for(request.instance.as_str())
            .map_or_else(|| ca.next_node_id(), |c| c.node_id);

        let mut noc_buf = vec![0u8; rs_matter::cert::MAX_CERT_TLV_AND_ASN1_LEN];
        let root_key = root_key_ref(ca)?;
        let mut generator = crate::fabric::noc_generator(ca, root_key.reference(), &mut noc_buf)?;

        let mut scratch = crate::fabric::commissioner_scratch();
        let mut commissioner = rs_matter::onboard::Commissioner::new(
            cx.matter,
            cx.crypto,
            cx.fab_idx,
            &mut generator,
            &mut scratch,
        );

        let opts = rs_matter::onboard::CommissionOptions {
            // The client's device attestation is accepted rather than verified. Verifying
            // it means fetching the CSA's distributed compliance ledger and validating a
            // chain against the production attestation authorities — which `rs-matter`
            // does not implement yet, and which would in any case only tell this panel
            // that the phone is a certified Matter device, not that its owner is in the
            // room. What says that is the passcode on the screen.
            allow_test_attestation: true,
            ..rs_matter::onboard::CommissionOptions::default()
        };

        let result = commissioner
            .commission(
                Address::Udp(found.addr),
                request.passcode,
                &opts,
                node_id,
                CastingCa::validity(),
            )
            .await
            .map_err(|e| MatterError::CommissioningFailed {
                instance: request.instance.to_string(),
                addr: found.addr,
                stage: CommissionStage::Pase,
                reason: e.to_string(),
            })?;

        commissioner
            .complete_via_case(Address::Udp(found.addr), &result)
            .await
            .map_err(|e| MatterError::CommissioningFailed {
                instance: request.instance.to_string(),
                addr: found.addr,
                stage: CommissionStage::Case,
                reason: format!("completing over CASE: {e}"),
            })?;

        Ok(node_id)
    }

    /// [`CastCommand`] → [`SessionEvent`]. The one place the two vocabularies meet.
    async fn pump_commands(
        &self,
        mut commands: mpsc::UnboundedReceiver<CastCommand>,
        sink: &SessionSink,
    ) -> Result<(), MatterError> {
        while let Some(command) = commands.recv().await {
            match command {
                CastCommand::Launch {
                    app,
                    url,
                    title,
                    autoplay,
                    surface,
                } => {
                    tracing::info!(app, %url, ?surface, "matter: launching");

                    self.state.set(PlayerSnapshot {
                        state: if autoplay {
                            PlaybackState::Buffering
                        } else {
                            PlaybackState::Paused
                        },
                        position: Duration::ZERO,
                        duration: None,
                        app: Some(app),
                    });

                    match surface {
                        Surface::Player => {
                            sink.emit(SessionEvent::Play {
                                source: MediaUri::parse(&url)?,
                                start: None,
                            })
                            .await?;

                            if let Some(title) = title {
                                sink.emit(SessionEvent::NowPlaying(
                                    castaway_core::NowPlaying::default().with_title(title),
                                ))
                                .await?;
                            }

                            if !autoplay {
                                sink.emit(SessionEvent::Control(ControlTxn::Pause)).await?;
                            }
                        }

                        Surface::Browser => match &self.browser {
                            Some(launches) => {
                                let _ = launches.send(BrowserLaunch { url, title, app });
                            }
                            // The cluster already answered `Success`, which is now a small
                            // lie — but the catalogue is what decides an app is a browser
                            // app, so a build with no browser declaring one is a wiring
                            // mistake worth a loud line rather than a refusal a phone
                            // cannot act on.
                            None => tracing::error!(
                                app,
                                %url,
                                "matter: a browser app is configured on a build with no browser"
                            ),
                        },
                    }
                }

                CastCommand::Transport(transport) => {
                    if let Some(txn) = self.transport_to_control(transport) {
                        sink.emit(SessionEvent::Control(txn)).await?;
                    }
                }

                CastCommand::SelectTarget(endpoint) => {
                    // Selecting a target is not playing anything — it says which app a
                    // subsequent launch belongs to. The projection moved in the handler,
                    // synchronously with the invoke the client was waiting on (#196), so
                    // what is left here is the record: this is the one cast command that
                    // changes nothing about playback, and without a line it would happen
                    // entirely invisibly.
                    tracing::info!(app = endpoint, "matter: a client selected a content app");
                }

                CastCommand::End => {
                    self.state.set(PlayerSnapshot::default());
                    sink.emit(SessionEvent::End).await?;
                }
            }
        }

        Ok(())
    }

    /// The two relative verbs resolve against the position only the panel knows, which is
    /// why they could not be translated when they were parsed.
    fn transport_to_control(&self, transport: Transport) -> Option<ControlTxn> {
        let position = self.state.get().position;

        Some(match transport {
            Transport::Play => ControlTxn::Play,
            Transport::Pause => ControlTxn::Pause,
            Transport::Stop => ControlTxn::Stop,
            Transport::StartOver => ControlTxn::Seek(Duration::ZERO),
            Transport::Previous => ControlTxn::Previous,
            Transport::Next => ControlTxn::Next,
            Transport::Seek(to) => ControlTxn::Seek(to),
            Transport::SkipForward(by) => ControlTxn::Seek(position.saturating_add(by)),
            Transport::SkipBackward(by) => ControlTxn::Seek(position.saturating_sub(by)),
        })
    }

    /// Put the passcode where a person can read it.
    async fn pump_prompts(&self, mut prompts: mpsc::UnboundedReceiver<Prompt>) {
        while let Some(prompt) = prompts.recv().await {
            match prompt {
                Prompt::Passcode { device, passcode } => {
                    tracing::info!(%device, %passcode, "matter: passcode on screen");
                    self.show(OsdMessage::sticky(format!(
                        "{device} wants to cast — enter {passcode}"
                    )));
                }
                Prompt::Clear => self.clear_osd(),
            }
        }
    }

    fn show(&self, message: OsdMessage) {
        if let Some(osd) = &self.osd {
            osd.show(message);
        }
    }

    fn clear_osd(&self) {
        if let Some(osd) = &self.osd {
            osd.clear();
        }
    }
}

/// The apparatus a commissioning attempt runs against.
///
/// Grouped because all five are fixed for the adapter's whole life and travelled together
/// through both functions below — the only things that vary per attempt are the client and
/// the CA the result is written into, which stay as arguments because one of them is
/// `&mut`.
struct Commissioning<'a, 'm, C> {
    matter: &'a Matter<'m>,
    crypto: &'a C,
    fab_idx: NonZeroU8,
    /// Its own browse handle: the app's shared responder is built for advertising, and
    /// this is the one place in the crate that needs to *look* for something.
    mdns: &'a MdnsResponder,
    /// Where to say how it went, so the client is not left guessing (#198).
    outcomes: &'a crate::server::OutcomeSender,
}

/// Install the panel's own identity on its fabric.
///
/// Rebuilt every boot from the stored root key (see [`crate::fabric`]): a fresh
/// operational certificate signed by the same root, for the same node id, is
/// indistinguishable to a client from yesterday's.
fn install_fabric<C: rs_matter::crypto::Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    ca: &CastingCa,
) -> Result<NonZeroU8, MatterError> {
    let key = crypto.generate_secret_key().map_err(core_err)?;
    let mut csr_buf = [0u8; 256];
    let csr = key.csr(&mut csr_buf).map_err(core_err)?;

    let mut canon = rs_matter::crypto::CanonPkcSecretKey::new();
    key.write_canon(&mut canon).map_err(core_err)?;

    let mut noc_buf = vec![0u8; rs_matter::cert::MAX_CERT_TLV_AND_ASN1_LEN];
    let root_key = root_key_ref(ca)?;
    let mut generator = crate::fabric::noc_generator(ca, root_key.reference(), &mut noc_buf)?;

    let noc = generator
        .generate(
            crypto,
            csr,
            CastingCa::panel_node_id(),
            &[],
            CastingCa::validity(),
        )
        .map_err(core_err)?;

    let ipk = ipk_ref(ca)?;

    matter
        .with_state(|state| {
            state
                .fabrics
                .add(
                    crypto,
                    canon.reference(),
                    ca.root_cert(),
                    noc,
                    &[],
                    Some(ipk.reference()),
                    ADMIN_VENDOR_ID,
                    CastingCa::panel_node_id(),
                )
                .map(|fabric| fabric.fab_idx())
        })
        .map_err(core_err)
}

/// Let every phone we have ever commissioned drive the panel.
///
/// In ordinary Matter the commissioner writes this list *into* the device. Here the
/// commissioner and the device are the same box, so it is written locally — and rebuilt
/// from the persisted client list at every boot, because a phone paired yesterday that
/// cannot speak today has effectively been un-paired without being told.
fn seed_acls(
    matter: &Matter<'_>,
    fab_idx: NonZeroU8,
    clients: &[CommissionedClient],
) -> Result<(), MatterError> {
    matter
        .with_state(|state| {
            let fabric = state.fabrics.fabric_mut(fab_idx)?;

            for client in clients {
                if fabric.acl_iter().any(|e| {
                    e.subjects()
                        .as_opt_ref()
                        .is_some_and(|s| s.contains(&client.node_id))
                }) {
                    continue;
                }

                // `Operate`, not `Administer`. A casting client needs to invoke the media
                // clusters; it does not need to add fabrics, write access control, or
                // remove the panel's own credentials. Nothing in Matter Casting asks for
                // more than this, and handing out Administer because it is simpler would
                // let any commissioned phone evict every other one.
                let mut entry = AclEntry::new(None, Privilege::OPERATE, AuthMode::Case);
                entry.add_subject(client.node_id)?;
                fabric.acl_add(entry)?;
            }

            Ok(())
        })
        .map_err(core_err)
}

/// The stored root key, back in the fixed-size form the crypto layer wants.
///
/// `CryptoSensitiveRef::new_from_slice` panics on a length mismatch, so the length is
/// checked here: these bytes come off disk and a truncated file is a thing that happens.
fn root_key_ref(ca: &CastingCa) -> Result<rs_matter::crypto::CanonPkcSecretKey, MatterError> {
    let bytes: &[u8; rs_matter::crypto::PKC_CANON_SECRET_KEY_LEN] = ca
        .root_key()
        .try_into()
        .map_err(|_| MatterError::Core("the stored root key is the wrong length".into()))?;
    Ok(rs_matter::crypto::CanonPkcSecretKey::new_from_ref(
        rs_matter::crypto::CanonPkcSecretKeyRef::new(bytes),
    ))
}

/// Likewise the identity protection key.
fn ipk_ref(ca: &CastingCa) -> Result<rs_matter::crypto::CanonAeadKey, MatterError> {
    let bytes: &[u8; rs_matter::crypto::AEAD_CANON_KEY_LEN] =
        ca.ipk().try_into().map_err(|_| {
            MatterError::Core("the stored identity protection key is the wrong length".into())
        })?;
    Ok(rs_matter::crypto::CanonAeadKey::new_from_ref(
        rs_matter::crypto::CanonAeadKeyRef::new(bytes),
    ))
}

fn core_err(e: rs_matter::error::Error) -> MatterError {
    MatterError::Core(e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn adapter() -> MatterAdapter {
        MatterAdapter::new(MatterConfig {
            friendly_name: "hackerspace tv".into(),
            ..MatterConfig::default()
        })
    }

    #[test]
    fn it_advertises_the_commissioner_service() {
        let ads = adapter().advertisements();
        assert_eq!(ads.len(), 1);
        let Advertisement::MdnsService {
            ty,
            instance,
            port,
            txt,
            ..
        } = &ads[0]
        else {
            panic!("expected an mDNS service");
        };
        assert_eq!(ty, "_matterd._udp");
        assert_eq!(instance, "hackerspace tv");
        assert_eq!(*port, crate::udc::UDC_PORT);
        assert!(txt.iter().any(|(k, v)| k == "DT" && v == "35"));
    }

    /// Skip is relative to a position only the panel knows, which is why it could not be
    /// resolved at parse time.
    #[test]
    fn skips_resolve_against_the_current_position() {
        let adapter = adapter();
        adapter
            .state
            .update(|s| s.position = Duration::from_secs(30));

        assert_eq!(
            adapter.transport_to_control(Transport::SkipForward(Duration::from_secs(15))),
            Some(ControlTxn::Seek(Duration::from_secs(45)))
        );
        assert_eq!(
            adapter.transport_to_control(Transport::SkipBackward(Duration::from_secs(15))),
            Some(ControlTxn::Seek(Duration::from_secs(15)))
        );
    }

    /// A skip past the start is a seek to the start, not an underflow.
    #[test]
    fn a_skip_backward_past_the_start_lands_at_zero() {
        let adapter = adapter();
        adapter
            .state
            .update(|s| s.position = Duration::from_secs(5));
        assert_eq!(
            adapter.transport_to_control(Transport::SkipBackward(Duration::from_secs(30))),
            Some(ControlTxn::Seek(Duration::ZERO))
        );
    }

    #[test]
    fn start_over_is_a_seek_to_zero() {
        let adapter = adapter();
        adapter
            .state
            .update(|s| s.position = Duration::from_secs(90));
        assert_eq!(
            adapter.transport_to_control(Transport::StartOver),
            Some(ControlTxn::Seek(Duration::ZERO))
        );
    }
}
