//! Wiring the Bluetooth A2DP sink: open a controller, load its firmware, run the adapter.
//!
//! Two things live here rather than in the protocol crates, because both are the app's
//! job: choosing which controller to open, and persisting link keys to disk. `hci-transport`
//! must not know where the state directory is, and `proto-bluetooth-audio` must not open
//! files at all (ground rules 2 and 3).

use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Arc;

use anyhow::Context as _;
use castaway_core::{ProtocolKind, SessionSink, SourceAdapter, SourceId, SourceMessage};
use hci_transport::{usb::UsbTransport, FirmwareSet, UsbId};
use proto_bluetooth_audio::host::HostConfig;
use proto_bluetooth_audio::{BluetoothAdapter, BluetoothConfig};
use substrate_hci::{BdAddr, HciTransport, LinkKey};
use tokio::sync::{mpsc, Notify};
use tracing::{info, warn};

use crate::config::Config;

/// Where paired phones' link keys live, relative to the state directory — see
/// [`Config::state_dir`] for why not the config directory.
const LINK_KEYS_FILE: &str = "bluetooth-link-keys";

/// Start the Bluetooth sink, returning the adapter task.
///
/// # Errors
/// If no controller can be opened, or its firmware will not load. Both are fatal *for
/// this adapter* — the caller logs and carries on with the LAN protocols, because a
/// missing dongle should not stop a receiver that can still do AirPlay and Cast.
pub async fn spawn(
    config: &Config,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    // The first attempt is part of startup, so a box with no dongle says so once, at
    // boot, with a real error — rather than disappearing into a retry loop nobody reads.
    let first = build(config).await?;

    let sink = SessionSink::new(SourceId::new(ProtocolKind::Bluetooth, "listener"), event_tx);
    let config = config.clone();
    Ok(tokio::spawn(async move {
        supervise(config, first, sink, shutdown).await;
    }))
}

/// How long to wait before re-opening a controller that died, and the ceiling.
///
/// A dongle that was yanked out of the socket will fail every attempt until someone puts
/// it back, and that could be a week — so the interval backs off rather than spinning a
/// USB enumeration every second for a device that is not there.
const RETRY_MIN: std::time::Duration = std::time::Duration::from_secs(2);
const RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// A run that lasted at least this long counts as healthy, so the next failure starts
/// over at [`RETRY_MIN`]. Without this, a controller that works fine for hours and then
/// hiccups once inherits the backoff from whatever happened at boot.
const HEALTHY_RUN: std::time::Duration = std::time::Duration::from_secs(60);

/// Keep a Bluetooth adapter running for as long as the receiver is up.
///
/// The thing this exists to prevent: `run` used to return on the first transport error
/// and nothing restarted it, so an unplug, a USB reset, or one oversized ACL packet left
/// Bluetooth dead for the lifetime of the process — with no error anywhere, because the
/// adapter reported success. A receiver on a wall does not get to be restarted by hand.
///
/// Re-opening is the *whole* controller: enumerate, claim, reload firmware, bring up.
/// Anything less would inherit whatever state wedged the last one, and a replugged dongle
/// is a different USB device anyway. Link keys survive because they live on disk, so a
/// phone that paired before the restart does not pair again.
async fn supervise(
    config: Config,
    first: (Arc<BluetoothAdapter>, String),
    sink: SessionSink,
    shutdown: Arc<Notify>,
) {
    let mut next = Some(first);
    let mut backoff = RETRY_MIN;
    loop {
        let (adapter, id) = match next.take() {
            Some(ready) => ready,
            None => {
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    () = shutdown.notified() => return,
                }
                match build(&config).await {
                    Ok(ready) => ready,
                    Err(e) => {
                        warn!(
                            error = %format!("{e:#}"),
                            retry_in = ?backoff,
                            "bluetooth: could not re-open a controller"
                        );
                        backoff = (backoff * 2).min(RETRY_MAX);
                        continue;
                    }
                }
            }
        };

        let started = std::time::Instant::now();
        tokio::select! {
            res = Arc::clone(&adapter).run(sink.clone()) => {
                match res {
                    Ok(()) => info!(controller = %id, "bluetooth adapter stopped"),
                    Err(e) => warn!(error = %e, controller = %id, "bluetooth adapter exited"),
                }
            }
            () = shutdown.notified() => {
                info!("Bluetooth sink stopping");
                return;
            }
        }

        if started.elapsed() >= HEALTHY_RUN {
            backoff = RETRY_MIN;
        }
        info!(retry_in = ?backoff, "bluetooth: re-opening the controller");
    }
}

/// Open a controller and build an adapter around it.
///
/// Everything here is re-done on every restart on purpose: the firmware set, the codec
/// table, and the stored link keys are all read fresh, so a replugged dongle of a
/// different make gets its own firmware and a phone that paired since boot is still known.
async fn build(config: &Config) -> anyhow::Result<(Arc<BluetoothAdapter>, String)> {
    let (transport, id) = open_transport(config).await?;

    let keys_path = link_keys_path(&config.state_dir());
    let link_keys = {
        let path = keys_path.clone();
        // A file read on the runtime, at bring-up and on every controller restart. Cheap
        // enough that it was missed the first time, but it is the same rule as the write
        // below and the same two lines away (#94).
        tokio::task::spawn_blocking(move || load_link_keys(&path))
            .await
            .context("reading stored link keys")?
    };
    if !link_keys.is_empty() {
        info!(count = link_keys.len(), "loaded stored link keys");
    }

    // The codec table follows what this build can actually decode. Advertising a codec
    // we cannot decode means the phone picks it and the session is silence rather than a
    // clean fallback to one we can (#14) — and the phone picks the *best* one it shares
    // with us, so the ones most likely to be missing are exactly the ones it reaches for
    // first. This used to check LDAC alone, which left aptX HD, aptX and AAC unguarded.
    let decodable = decodable();
    let codecs = match &config.bluetooth.codecs {
        // Named explicitly: honour the list verbatim. This is the off-switch for any
        // codec the default table carries — `codecs = ["sbc"]` narrows to the mandatory
        // fallback, and it is the only way to exercise a specific codec on real hardware.
        Some(names) => Some(
            names
                .iter()
                .map(|n| parse_codec(n))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        // Nothing named: everything this build can decode, LDAC included. LDAC spent its
        // first month as a config opt-in because it sorts *first* and had never met a
        // phone; the 2026-08-08 bench session met the exit criterion #14 set (a real
        // Android phone negotiated it first-choice at 96 kHz and the capture decodes,
        // `a_real_android_phones_ldac_decodes_through_the_depacketiser`), so the default
        // is now the decodable set and the config list above is the off-switch (#253).
        None => None,
    };
    if let Some(codecs) = &codecs {
        info!(?codecs, "bluetooth: advertising a restricted codec table");
    }

    // Persist each new key as it is issued, so a repeat guest never re-pairs (#68). A
    // write failure costs one re-pairing next time, which is not worth ending a live
    // session over.
    //
    // The callback itself must not touch the disk. It runs *inline* on the adapter's
    // single `select!` loop, which is also the only thing draining HCI events, ACL
    // fragments and A2DP media packets — so for as long as it blocks, the controller's
    // buffers back up, at the most timing-sensitive moment a connection has (#94). It
    // does one non-blocking send and returns; the writer below owns the file.
    let (writer, _drain) = spawn_link_key_writer(keys_path.clone());
    let on_paired: proto_bluetooth_audio::adapter::OnPaired = Arc::new(move |addr, key| {
        if writer.send((addr, key)).is_err() {
            warn!(%addr, "link-key writer is gone; this peer will re-pair next time");
        }
    });

    let adapter = Arc::new(BluetoothAdapter::new(
        transport,
        BluetoothConfig {
            host: HostConfig {
                name: config.advertised_name(castaway_core::ProtocolKind::Bluetooth),
                discoverable: true,
                ..HostConfig::default()
            },
            decodable: decodable.clone(),
            codecs,
            link_keys,
            on_paired: Some(on_paired),
            // What we tell a phone we hold, so it can delay its video to match. The
            // default is the output queue's own depth; there is no measurement to
            // improve on it with yet (#89).
            sink_delay: proto_bluetooth_audio::sink::DEFAULT_SINK_DELAY,
            // Measured and worth it (#75): a phone that offers a form larger than the
            // fixed 200x200 thumbnail generally has real detail in it, and the extra GET
            // rides a channel that is already open. Bounded by the airtime ceiling in
            // `proto-bluetooth-audio`, not by what the panel could draw.
            fetch_best_cover_art: true,
        },
    ));

    info!(controller = %id, ?decodable, "enabled: Bluetooth A2DP sink");

    Ok((adapter, id))
}

/// Resolve a configured codec name.
///
/// # Errors
/// If the name is not one of the codecs this crate knows, because a typo that silently
/// advertised the full table would be a confusing way to lose a test.
fn parse_codec(name: &str) -> anyhow::Result<castaway_core::AudioCodec> {
    use castaway_core::AudioCodec;
    match name.to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
        "sbc" => Ok(AudioCodec::Sbc),
        "aac" => Ok(AudioCodec::Aac),
        "aptx" => Ok(AudioCodec::AptX),
        "aptx-hd" => Ok(AudioCodec::AptXHd),
        "ldac" => Ok(AudioCodec::Ldac),
        other => anyhow::bail!(
            "unknown bluetooth codec {other:?}; expected sbc, aac, aptx, aptx-hd or ldac"
        ),
    }
}

/// Open whichever transport the config names.
///
/// Returns the transport and a label for logs. The two are genuinely different animals:
/// USB claims a device and loads its firmware, the socket attaches to a controller the
/// kernel has already brought up — which is the only way to reach a *virtual* controller,
/// and therefore the only way to run the whole stack with no hardware.
async fn open_transport(config: &Config) -> anyhow::Result<(Arc<dyn HciTransport>, String)> {
    let spec = config.bluetooth.transport.trim();

    if let Some(index) = spec.strip_prefix("socket:") {
        #[cfg(all(feature = "bluetooth-socket", target_os = "linux"))]
        {
            let index: u16 = index
                .trim()
                .parse()
                .with_context(|| format!("controller index in transport {spec:?}"))?;
            let transport = hci_transport::socket::SocketTransport::open(index)
                .with_context(|| format!("attaching to hci{index}"))?;
            return Ok((Arc::new(transport), format!("hci{index}")));
        }
        #[cfg(not(all(feature = "bluetooth-socket", target_os = "linux")))]
        {
            let _ = index;
            anyhow::bail!("transport {spec:?} needs the `bluetooth-socket` feature on Linux");
        }
    }

    if let Some(addr) = spec.strip_prefix("tcp:") {
        let addr = addr.trim();
        // The address is not validated beyond "has a port": rootcanal's HCI port is
        // wherever the harness put it, and connect() is the only authority on whether
        // something answers there.
        anyhow::ensure!(
            addr.rsplit_once(':')
                .is_some_and(|(_, port)| port.parse::<u16>().is_ok()),
            "transport {spec:?} should look like tcp:127.0.0.1:6402"
        );
        let transport = hci_transport::tcp::TcpTransport::connect(addr)
            .await
            .with_context(|| format!("connecting to the virtual controller at {addr}"))?;
        return Ok((Arc::new(transport), format!("tcp:{addr}")));
    }

    if spec != "usb" {
        anyhow::bail!(
            "unknown bluetooth transport {spec:?}; expected \"usb\", \"socket:N\" or \
             \"tcp:host:port\""
        );
    }

    let requested = match &config.bluetooth.controller {
        Some(spec) => Some(parse_usb_id(spec)?),
        None => None,
    };
    // Prefer blobs from an explicit directory (so a newer one can be tried without a
    // rebuild), then whatever `build.rs` embedded.
    let firmware = match &config.bluetooth.firmware_dir {
        Some(dir) => FirmwareSet::from_dir(Path::new(dir))
            .await
            .with_context(|| format!("loading firmware from {dir}"))?,
        None => FirmwareSet::embedded(),
    };
    if firmware.is_empty() {
        warn!(
            "no bluetooth firmware in this build; only ROM-based controllers will \
             initialise (see architecture §11.3b)"
        );
    }
    let policy = unknown_controller_policy(config.bluetooth.unknown_controller);
    let transport = UsbTransport::open_and_init(requested, &firmware, policy)
        .await
        .context("opening the bluetooth controller")?;
    let id = transport.id().to_string();
    Ok((Arc::new(transport), id))
}

/// Which codecs the linked pipeline can decode.
///
/// A statement of fact, not of policy — the distinction #14 was about: `can_decode` once
/// answered a feature flag instead of "is there a decoder", and a build advertised LDAC
/// with nothing behind it. Policy — which decodable codecs are actually offered — is the
/// config's `codecs` list in `build()` above. There used to be a third thing here, an
/// `OPT_IN` list holding LDAC out of the default table until a real sender had streamed
/// to it; that condition was met on 2026-08-08 and the list retired with it (#253). A
/// future codec that should ship dark goes back through the same shape: a policy list
/// here, deliberately separate from this function's statement of fact.
fn decodable() -> Vec<castaway_core::AudioCodec> {
    #[cfg(feature = "audio")]
    {
        pipeline::audio_session::decodable_codecs()
    }
    #[cfg(not(feature = "audio"))]
    {
        // Without the audio feature nothing decodes, but the stack is still worth
        // running against the null pipeline to prove discovery and pairing.
        Vec::new()
    }
}

/// The library's policy for the config's words (#91).
///
/// The boundary conversion: the config enum is serde's, the transport enum is the
/// library's, and matching here is what keeps `hci-transport` serde-free.
fn unknown_controller_policy(
    choice: crate::config::UnknownController,
) -> hci_transport::init::UnknownControllerPolicy {
    match choice {
        crate::config::UnknownController::AssumeRom => {
            hci_transport::init::UnknownControllerPolicy::AssumeRom
        }
        crate::config::UnknownController::Refuse => {
            hci_transport::init::UnknownControllerPolicy::Refuse
        }
    }
}

/// Parse a `vendor:product` controller selector, as `lsusb` prints it.
fn parse_usb_id(spec: &str) -> anyhow::Result<UsbId> {
    let (vendor, product) = spec
        .split_once(':')
        .with_context(|| format!("controller {spec:?} should look like 8087:0029"))?;
    Ok(UsbId::new(
        u16::from_str_radix(vendor.trim(), 16).with_context(|| format!("vendor id in {spec:?}"))?,
        u16::from_str_radix(product.trim(), 16)
            .with_context(|| format!("product id in {spec:?}"))?,
    ))
}

/// Read stored link keys, so a repeat guest reconnects without pairing again (#68).
///
/// A corrupt or unreadable file is a warning, not a failure: the worst case is that
/// everyone re-pairs once, which is a far better outcome than refusing to start.
fn load_link_keys(path: &Path) -> Vec<(BdAddr, LinkKey)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_link_key(line) {
            Some(entry) => out.push(entry),
            None => warn!(line = n + 1, "skipping a malformed link-key entry"),
        }
    }
    out
}

/// One `AA:BB:CC:DD:EE:FF <32 hex chars>` line.
fn parse_link_key(line: &str) -> Option<(BdAddr, LinkKey)> {
    let (addr, key) = line.split_once(char::is_whitespace)?;
    let addr = BdAddr::from_str(addr.trim()).ok()?;
    let key = key.trim();
    if key.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(key.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some((addr, LinkKey::new(bytes)))
}

/// Spawn the task that owns the link-key file, and hand back its inbox.
///
/// Serialising through one task is not just about keeping the disk off the adapter's
/// thread: [`store_link_key`] is read-modify-write on a whole file, so two pairings close
/// together would otherwise race two `File::create`s at the same path and one key would
/// vanish. In arrival order, through one owner, they cannot.
///
/// Unbounded because the producer is the adapter loop and it must never wait — the queue
/// is one small entry per pairing, which is a human pressing a button on a phone.
///
/// The task ends when the last sender drops, which is when the adapter it belongs to is
/// dropped: a controller restart builds a new adapter and a new writer. The handle comes
/// back so a test can drop the sender and await the drain rather than poll for it;
/// nothing in production joins it.
fn spawn_link_key_writer(
    path: PathBuf,
) -> (
    mpsc::UnboundedSender<(BdAddr, Option<LinkKey>)>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<(BdAddr, Option<LinkKey>)>();
    let task = tokio::spawn(async move {
        while let Some((addr, key)) = rx.recv().await {
            let path = path.clone();
            let written =
                tokio::task::spawn_blocking(move || store_link_key(&path, addr, key.as_ref()))
                    .await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!(error = %format!("{e:#}"), %addr, "could not update the stored link key");
                }
                Err(e) => warn!(error = %e, %addr, "the link-key write panicked"),
            }
        }
    });
    (tx, task)
}

/// Record a newly paired peer's key.
///
/// # Errors
/// If the state directory cannot be created or written.
/// Write `key` for `addr`, or remove the entry entirely when it is `None`.
///
/// Removal matters as much as storage: a key the peer has stopped accepting must not
/// survive a restart, or the connect/authenticate/fail loop it causes becomes permanent.
fn store_link_key(path: &Path, addr: BdAddr, key: Option<&LinkKey>) -> anyhow::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Replace any existing entry for this peer rather than appending a second one: a
    // phone that re-pairs gets a new key, and the old one would be tried first.
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut kept: Vec<String> = existing
        .lines()
        .filter(|line| parse_link_key(line).is_none_or(|(stored, _)| stored != addr))
        .map(str::to_owned)
        .collect();
    if let Some(key) = key {
        kept.push(format!(
            "{addr} {}",
            key.as_bytes().iter().fold(String::new(), |mut acc, b| {
                use std::fmt::Write as _;
                let _ = write!(acc, "{b:02x}");
                acc
            })
        ));
    }

    let mut file =
        std::fs::File::create(path).with_context(|| format!("writing {}", path.display()))?;
    for line in kept {
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// Where link keys are stored.
fn link_keys_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LINK_KEYS_FILE)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[cfg(feature = "audio")]
    #[test]
    fn we_never_advertise_an_endpoint_this_build_cannot_decode() {
        // The invariant #14 actually needs, and the one nothing was checking. The test in
        // `pipeline` asserts `can_decode` over `decodable_codecs()`, which is true by
        // construction; it cannot see the advertised table at all, because that lives in
        // `proto-bluetooth-audio`. This crate is the only one that depends on both.
        //
        // It matters because the table is ordered best-first and a sender takes the first
        // endpoint it shares with us — so an endpoint we cannot decode is not a missed
        // opportunity, it is the one the phone reaches for. The session then connects,
        // the card fills in, and nothing comes out of the speakers.
        let decodable = decodable();
        for cap in proto_bluetooth_audio::codec::advertised(&decodable) {
            let codec = cap.audio_codec();
            assert!(
                codec == castaway_core::AudioCodec::Sbc || decodable.contains(&codec),
                "advertising {codec:?}, which this build cannot decode"
            );
        }
    }

    #[cfg(all(feature = "audio", feature = "ldac"))]
    #[test]
    fn ldac_is_advertised_by_default_and_the_config_is_the_off_switch() {
        use proto_bluetooth_audio::codec::CodecCapability;

        // The flip #253 made, both directions. A default build (`codecs` unset in the
        // config, so `build()` hands the adapter `None`) advertises everything decodable
        // — and LDAC sorts first, so it is what every capable phone now negotiates. That
        // is on purpose: the 2026-08-08 bench session proved a real phone's stream
        // decodes (`a_real_android_phones_ldac_decodes_through_the_depacketiser`).
        assert!(
            decodable().contains(&castaway_core::AudioCodec::Ldac),
            "a build with the ldac feature can decode LDAC"
        );
        let table = proto_bluetooth_audio::codec::advertised(&decodable());
        assert_eq!(
            table.first().map(CodecCapability::name),
            Some("ldac"),
            "the default table offers LDAC, first"
        );

        // And the runtime off-switch (rule 5: a runtime switch, not a feature gate): a
        // config that names codecs without LDAC excludes it, through the same narrowing
        // the adapter applies.
        let named = [
            castaway_core::AudioCodec::Sbc,
            castaway_core::AudioCodec::Aac,
        ];
        let mut narrowed = proto_bluetooth_audio::codec::advertised(&decodable());
        narrowed.retain(|c| named.contains(&c.audio_codec()));
        assert!(
            narrowed
                .iter()
                .all(|c| c.audio_codec() != castaway_core::AudioCodec::Ldac),
            "naming codecs without ldac turns the endpoint off"
        );
        assert_eq!(narrowed.last().map(CodecCapability::name), Some("sbc"));
    }

    #[cfg(feature = "audio")]
    #[test]
    fn sbc_survives_even_when_nothing_else_does() {
        // A sink with no SBC endpoint is not an A2DP sink: SBC is mandatory, so a phone
        // that finds nothing in common simply refuses to connect. That is a worse failure
        // than silence, and it is what a naive "filter everything" would have produced on
        // a build with no ffmpeg at all.
        let table = proto_bluetooth_audio::codec::advertised(&[]);
        assert_eq!(table.len(), 1);
        assert_eq!(
            table[0].audio_codec(),
            castaway_core::AudioCodec::Sbc,
            "SBC is the guaranteed floor"
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("castaway-bt-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_config_policy_reaches_the_registry_it_names() {
        // The whole point of `unknown_controller = "refuse"` (#91): the strict registry
        // must actually be the one selected from, so an unclaimed part is a startup
        // error naming its id rather than a warning and an inert radio.
        use hci_transport::init::{registry_for, select, UnknownControllerPolicy};
        let csr8510 = UsbId::new(0x0a12, 0x0001);

        let policy = unknown_controller_policy(crate::config::UnknownController::Refuse);
        assert_eq!(policy, UnknownControllerPolicy::Refuse);
        assert!(select(registry_for(policy), csr8510).is_err());

        let policy = unknown_controller_policy(crate::config::UnknownController::AssumeRom);
        assert_eq!(policy, UnknownControllerPolicy::AssumeRom);
        assert!(select(registry_for(policy), csr8510).is_ok());
    }

    #[test]
    fn a_controller_selector_parses_the_way_lsusb_prints_it() {
        assert_eq!(
            parse_usb_id("8087:0029").unwrap(),
            UsbId::new(0x8087, 0x0029)
        );
        assert_eq!(
            parse_usb_id("0bda:8771").unwrap(),
            UsbId::new(0x0bda, 0x8771)
        );
        assert!(parse_usb_id("8087").is_err());
        assert!(parse_usb_id("zzzz:0029").is_err());
    }

    #[test]
    fn link_keys_round_trip_through_the_store() {
        let dir = temp_dir("roundtrip");
        let path = link_keys_path(&dir);
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let key = LinkKey::new([0xAB; 16]);

        store_link_key(&path, addr, Some(&key)).unwrap();
        let loaded = load_link_keys(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, addr);
        assert_eq!(loaded[0].1.as_bytes(), key.as_bytes());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn re_pairing_replaces_a_key_rather_than_appending_a_second() {
        // A phone that re-pairs gets a new key. Two entries would mean the stale one is
        // tried first, and the controller would report a key mismatch on every connect.
        let dir = temp_dir("replace");
        let path = link_keys_path(&dir);
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();

        store_link_key(&path, addr, Some(&LinkKey::new([0x11; 16]))).unwrap();
        store_link_key(&path, addr, Some(&LinkKey::new([0x22; 16]))).unwrap();

        let loaded = load_link_keys(&path);
        assert_eq!(loaded.len(), 1, "one entry per peer");
        assert_eq!(loaded[0].1.as_bytes(), &[0x22; 16], "the newest key wins");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn several_peers_coexist() {
        let dir = temp_dir("many");
        let path = link_keys_path(&dir);
        let a: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let b: BdAddr = "11:22:33:44:55:66".parse().unwrap();
        store_link_key(&path, a, Some(&LinkKey::new([0x11; 16]))).unwrap();
        store_link_key(&path, b, Some(&LinkKey::new([0x22; 16]))).unwrap();

        let loaded = load_link_keys(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|(addr, _)| *addr == a));
        assert!(loaded.iter().any(|(addr, _)| *addr == b));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_entry_costs_one_re_pairing_not_a_failed_start() {
        let dir = temp_dir("corrupt");
        let path = link_keys_path(&dir);
        std::fs::write(
            &path,
            "# a comment\n\
             AA:BB:CC:DD:EE:FF aabbccddeeff00112233445566778899\n\
             this line is nonsense\n\
             11:22:33:44:55:66 tooshort\n",
        )
        .unwrap();

        let loaded = load_link_keys(&path);
        assert_eq!(loaded.len(), 1, "the good entry survives");
        assert_eq!(loaded[0].0.to_string(), "AA:BB:CC:DD:EE:FF");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_store_is_simply_no_keys() {
        assert!(load_link_keys(Path::new("/nonexistent/castaway/keys")).is_empty());
    }

    /// The property #94 is actually about: `on_paired` returns without having touched the
    /// disk.
    ///
    /// A single-threaded runtime is what makes this an assertion rather than a hope. The
    /// send happens with nothing else able to run, so if the write were still inline the
    /// file would exist the instant the callback returned. It must not — the key may only
    /// appear after the writer task has been given a chance to run.
    ///
    /// This is the closest thing to a test for a timing property: not "the write was
    /// fast", which is unassertable, but "the write had not happened yet", which is exact.
    #[tokio::test(flavor = "current_thread")]
    async fn pairing_does_not_write_to_disk_on_the_callers_thread() {
        let dir = temp_dir("deferred");
        let path = link_keys_path(&dir);
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();

        let (writer, drain) = spawn_link_key_writer(path.clone());
        writer.send((addr, Some(LinkKey::new([0x33; 16])))).unwrap();

        assert!(
            !path.exists(),
            "the link key was written inline; the adapter loop was parked for a disk \
             round trip (#94)"
        );

        // Closing the channel ends the task once it has drained, so awaiting it is an
        // exact wait rather than a poll.
        drop(writer);
        drain.await.unwrap();

        let loaded = load_link_keys(&path);
        assert_eq!(loaded.len(), 1, "the key still lands, just not inline");
        assert_eq!(loaded[0].1.as_bytes(), &[0x33; 16]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two pairings in quick succession must not race two `File::create`s at one path.
    ///
    /// The queue is ordered and single-owner, so the second key wins deterministically
    /// rather than by whichever `create` truncated last.
    #[tokio::test(flavor = "current_thread")]
    async fn back_to_back_pairings_serialise_through_one_owner() {
        let dir = temp_dir("serialise");
        let path = link_keys_path(&dir);
        let a: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let b: BdAddr = "11:22:33:44:55:66".parse().unwrap();

        let (writer, drain) = spawn_link_key_writer(path.clone());
        writer.send((a, Some(LinkKey::new([0x11; 16])))).unwrap();
        writer.send((b, Some(LinkKey::new([0x22; 16])))).unwrap();
        // Same peer again: last write wins, which only holds if they are ordered.
        writer.send((a, Some(LinkKey::new([0x44; 16])))).unwrap();
        writer.send((b, None)).unwrap();
        drop(writer);
        drain.await.unwrap();

        let loaded = load_link_keys(&path);
        assert_eq!(loaded.len(), 1, "b was forgotten, a survives: {loaded:?}");
        assert_eq!(loaded[0].0, a);
        assert_eq!(loaded[0].1.as_bytes(), &[0x44; 16], "the newest key for a");
        std::fs::remove_dir_all(&dir).ok();
    }
}
