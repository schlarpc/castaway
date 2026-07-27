//! Wiring the Bluetooth A2DP sink: open a controller, load its firmware, run the adapter.
//!
//! Two things live here rather than in the protocol crates, because both are the app's
//! job: choosing which controller to open, and persisting link keys to disk. `hci-transport`
//! must not know where the config directory is, and `proto-bluetooth-audio` must not open
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

/// Where paired phones' link keys live, relative to the config directory.
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
    let link_keys = load_link_keys(&keys_path);
    if !link_keys.is_empty() {
        info!(count = link_keys.len(), "loaded stored link keys");
    }

    // The codec table follows what this build can actually decode. Advertising a codec
    // we cannot decode means the phone picks it and the session is silence rather than a
    // clean fallback to one we can (Q22) — and the phone picks the *best* one it shares
    // with us, so the ones most likely to be missing are exactly the ones it reaches for
    // first. This used to check LDAC alone, which left aptX HD, aptX and AAC unguarded.
    let decodable = decodable();
    let codecs = match &config.bluetooth.codecs {
        Some(names) => Some(
            names
                .iter()
                .map(|n| parse_codec(n))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        None => None,
    };
    if let Some(codecs) = &codecs {
        info!(?codecs, "bluetooth: advertising a restricted codec table");
    }

    // Persist each new key as it is issued, so a repeat guest never re-pairs (Q23). A
    // write failure costs one re-pairing next time, which is not worth ending a live
    // session over.
    let store_path = keys_path.clone();
    let on_paired: proto_bluetooth_audio::adapter::OnPaired = Arc::new(move |addr, key| {
        if let Err(e) = store_link_key(&store_path, addr, key.as_ref()) {
            warn!(error = %format!("{e:#}"), %addr, "could not update the stored link key");
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

    if spec != "usb" {
        anyhow::bail!("unknown bluetooth transport {spec:?}; expected \"usb\" or \"socket:N\"");
    }

    let requested = match &config.bluetooth.controller {
        Some(spec) => Some(parse_usb_id(spec)?),
        None => None,
    };
    // Prefer blobs from an explicit directory (so a newer one can be tried without a
    // rebuild), then whatever `build.rs` embedded.
    let firmware = match &config.bluetooth.firmware_dir {
        Some(dir) => FirmwareSet::from_dir(Path::new(dir))
            .with_context(|| format!("loading firmware from {dir}"))?,
        None => FirmwareSet::embedded(),
    };
    if firmware.is_empty() {
        warn!(
            "no bluetooth firmware in this build; only ROM-based controllers will \
             initialise (see architecture §11.3b)"
        );
    }
    let transport = UsbTransport::open_and_init(requested, &firmware)
        .await
        .context("opening the bluetooth controller")?;
    let id = transport.id().to_string();
    Ok((Arc::new(transport), id))
}

/// Which codecs the linked pipeline can decode.
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

/// Read stored link keys, so a repeat guest reconnects without pairing again (Q23).
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
        // The invariant Q22 actually needs, and the one nothing was checking. The test in
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
}
