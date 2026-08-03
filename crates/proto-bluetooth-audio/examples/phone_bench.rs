//! Put a real phone in front of the real stack and record everything it says.
//!
//! Three open issues all end with the same sentence — *one phone visit, three fixtures* —
//! and none of them could be answered because the questions are "what does the peer
//! *reply*", and nothing here could ask. #74 wants the cover-art chain proven against a
//! phone rather than against BlueZ. #75 wants one `GetImageProperties` listing, which
//! decides whether 200×200 is an iPhone's ceiling or merely the form we ask for. #76 wants
//! the player-application-setting attributes an iPhone exposes for a given app.
//!
//! This runs the **real adapter** — not a reimplementation of it — with the properties
//! probe turned on, and taps the transport underneath. Everything the controller sees goes
//! to a `btsnoop` file, which Wireshark reads natively and which is a legitimate
//! checked-in fixture (ground rule 9): the answers are in the bytes rather than in this
//! program's opinion of them.
//!
//! ```text
//! # The dongle must not be the kernel's — HCI_CHANNEL_USER is exclusive, and claiming
//! # hci0 would take the machine's own Bluetooth away.
//! ls /sys/bus/usb/drivers/btusb/
//! echo 3-10:1.0 | sudo tee /sys/bus/usb/drivers/btusb/unbind
//!
//! sudo -E cargo run -p proto-bluetooth-audio --features bench --example phone_bench
//! # …or name a different controller and output directory:
//! sudo -E cargo run -p proto-bluetooth-audio --features bench --example phone_bench \
//!     -- 2357:0604 ./capture
//! ```
//!
//! Then, on the phone: pair with **castaway bench**, play a track in the app under test,
//! let it sit for one track change, and press Ctrl-C. The checklist printed at the end
//! says which of the three questions the capture actually answered — a run that ends with
//! "not seen" against a line is a run to do again, not a result.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use castaway_core::{SessionEvent, SourceAdapter, SourceMessage};
use hci_transport::init::{self, UsbId};
use hci_transport::{usb, FirmwareSet};
use proto_bluetooth_audio::{BluetoothAdapter, BluetoothConfig};
use substrate_hci::{HciError, HciPacket, HciTransport};
use tokio::sync::mpsc;

/// The spare UB500 on the dev box. Not the machine's own AX200, which is `hci0` and
/// belongs to whoever is sitting at it.
const DEFAULT_ID: &str = "2357:0604";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // The interpreted view, beside the raw one. `proto_bluetooth_audio` at
                // debug is what prints the SDP record, the OBEX exchange and the settings
                // listings in a form a person can read.
                "info,proto_bluetooth_audio=debug,substrate_l2cap=debug,substrate_sdp=debug,\
                 nusb=warn"
                    .into()
            }),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let id = args.next().unwrap_or_else(|| DEFAULT_ID.to_owned());
    let out_dir = PathBuf::from(args.next().unwrap_or_else(|| "capture".to_owned()));
    let (vendor, product) = id
        .split_once(':')
        .ok_or("expected vendor:product, e.g. 2357:0604")?;
    let id = UsbId::new(
        u16::from_str_radix(vendor, 16)?,
        u16::from_str_radix(product, 16)?,
    );

    std::fs::create_dir_all(&out_dir)?;
    let snoop_path = out_dir.join("phone-bench.btsnoop");

    println!("claiming {id}…");
    let usb = usb::UsbTransport::open(id)?;
    let loader = init::select(init::registry(), id)?;
    println!("  loader: {}", loader.name());
    loader.init(id, &usb, &FirmwareSet::embedded()).await?;
    println!("  firmware ok");

    let transport = Arc::new(Recording::new(usb, &snoop_path)?);

    let config = BluetoothConfig {
        // The whole reason this bench exists. Off everywhere else, because it spends the
        // one risk the cover-art path has.
        probe_image_properties: true,
        ..BluetoothConfig::default()
    };
    let adapter = Arc::new(BluetoothAdapter::new(
        Arc::clone(&transport) as Arc<dyn HciTransport>,
        config,
    ));
    println!("advertised codecs: {:?}", adapter.advertised_codecs());

    let (tx, rx) = mpsc::channel(64);
    let sink = castaway_core::SessionSink::new(
        castaway_core::SourceId::new(castaway_core::ProtocolKind::Bluetooth, "bench"),
        tx,
    );

    println!();
    println!("bench is up. On the phone:");
    println!("  1. pair with the receiver and connect");
    println!("  2. play a track — YouTube Music answers #76's question for that app");
    println!("  3. let one track change happen, so a second metadata read is captured");
    println!("  4. Ctrl-C here");
    println!();

    // Shared rather than returned from the task: Ctrl-C aborts the observer, and an
    // aborted `JoinHandle` yields a `JoinError`, not the tally — which would have made
    // every checklist read as though nothing happened.
    let seen = Arc::new(std::sync::Mutex::new(Seen::default()));
    let observer = tokio::spawn(observe(rx, Arc::clone(&seen)));
    let run = tokio::spawn(async move { adapter.run(sink).await });

    tokio::select! {
        r = tokio::signal::ctrl_c() => r?,
        r = run => {
            // The adapter stopping on its own is a failure — nothing here asks it to.
            match r? {
                Ok(()) => println!("\nadapter exited"),
                Err(e) => println!("\nadapter failed: {e}"),
            }
        }
    }

    observer.abort();
    let packets = transport.finish()?;
    println!();
    println!("wrote {} packets to {}", packets, snoop_path.display());
    println!("  wireshark {}", snoop_path.display());
    println!();
    match seen.lock() {
        Ok(seen) => seen.report(),
        Err(poisoned) => poisoned.into_inner().report(),
    }
    Ok(())
}

/// What the run managed to observe, so the checklist can say what is still missing.
#[derive(Default)]
struct Seen {
    connected: bool,
    audio: bool,
    metadata: bool,
    artwork: bool,
    shuffle_or_repeat: bool,
}

impl Seen {
    fn report(&self) {
        println!("what this capture can answer:");
        let line = |ok: bool, what: &str| {
            println!("  [{}] {what}", if ok { "yes" } else { " - " });
        };
        line(self.connected, "a phone connected");
        line(self.audio, "audio arrived (the sink negotiated a codec)");
        line(self.metadata, "AVRCP metadata reached the card");
        line(
            self.artwork,
            "cover art was fetched — #74's chain, end to end, against a phone",
        );
        line(
            self.shuffle_or_repeat,
            "the player exposed shuffle or repeat — #76",
        );
        println!();
        println!("#75's answer is not a session event: grep the log for");
        println!("  \"peer's image properties\"   — the variants the phone listed");
        println!("and #76's full listing for");
        println!("  \"player application settings\" — including ids we do not implement");
        if !self.artwork {
            println!();
            println!("no artwork: check the log for how far the chain got — the SDP query,");
            println!("the image PSM, the ERTM configure, then OBEX CONNECT.");
        }
    }
}

async fn observe(mut rx: mpsc::Receiver<SourceMessage>, seen: Arc<std::sync::Mutex<Seen>>) {
    while let Some(msg) = rx.recv().await {
        let mut seen = match seen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match msg.event {
            SessionEvent::SourceInfo(ref description) => {
                seen.connected = true;
                println!("● connected: {description:?}");
            }
            SessionEvent::Audio { ref format, .. } => {
                seen.audio = true;
                println!("● audio: {format:?}");
            }
            SessionEvent::NowPlaying(ref now) => {
                seen.metadata |= now.title.is_some();
                seen.artwork |= now.artwork.is_some();
                seen.shuffle_or_repeat |= now.shuffle.is_some() || now.repeat.is_some();
                println!(
                    "● now playing: {:?} — {:?} | art {} | shuffle {:?} repeat {:?}",
                    now.title.as_deref().unwrap_or("?"),
                    now.artist.as_deref().unwrap_or("?"),
                    now.artwork.as_ref().map_or(0, castaway_core::Artwork::len),
                    now.shuffle,
                    now.repeat,
                );
            }
            SessionEvent::End => println!("● session ended"),
            ref other => println!("● {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The tap.
// ---------------------------------------------------------------------------

/// An [`HciTransport`] that passes everything through and writes it to a `btsnoop` file.
///
/// A decorator rather than a hook inside the adapter, for the reason that makes the
/// capture worth having: it sees the bytes as the controller sees them, so it cannot be
/// wrong in the same way the code under test is wrong. A tap that logged our *parsed*
/// view would agree with a misparse.
struct Recording {
    inner: usb::UsbTransport,
    /// Unbuffered on purpose. A `BufWriter` here produced a zero-byte capture the first
    /// time the process was killed rather than asked to stop — the bytes were in the
    /// buffer and the buffer went with the process. One `write_all` per packet is a
    /// syscall per packet, which against a few hundred ACL packets a second costs
    /// nothing measurable and means the file on disk is always a complete capture up to
    /// the instant of death.
    file: std::sync::Mutex<Option<std::fs::File>>,
    packets: std::sync::atomic::AtomicU64,
}

impl Recording {
    fn new(inner: usb::UsbTransport, path: &Path) -> std::io::Result<Self> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(path)?;
        file.write_all(b"btsnoop\0")?;
        file.write_all(&1u32.to_be_bytes())?; // version
                                              // 1002 = HCI UART (H4): every record begins with the packet-type indicator, which
                                              // is exactly what `HciPacket::encode` produces.
        file.write_all(&1002u32.to_be_bytes())?;
        Ok(Self {
            inner,
            file: std::sync::Mutex::new(Some(file)),
            packets: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Record one packet. Never fails the transport: a capture that stops is worth less
    /// than a session that stops, and the session is the thing with a person waiting on it.
    fn record(&self, packet: &HciPacket, received: bool) {
        use std::io::Write as _;
        let Ok(bytes) = packet.encode() else { return };
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(file) = guard.as_mut() else { return };

        // btsnoop's epoch is 0000-01-01, in microseconds; this is the offset to it from
        // the Unix epoch. A wrong value here makes every timestamp in Wireshark absurd
        // while the packets themselves stay perfectly readable.
        const EPOCH_OFFSET_US: u64 = 0x00E0_3AB4_4A67_6000;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let ts = EPOCH_OFFSET_US + u64::try_from(now.as_micros()).unwrap_or(u64::MAX);

        // Bit 0: direction, 1 = controller→host. Bit 1: 1 = command/event rather than data.
        let mut flags = u32::from(received);
        if matches!(packet, HciPacket::Command { .. } | HciPacket::Event { .. }) {
            flags |= 0b10;
        }

        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        let mut record = Vec::with_capacity(24 + bytes.len());
        record.extend_from_slice(&len.to_be_bytes()); // original length
        record.extend_from_slice(&len.to_be_bytes()); // included length
        record.extend_from_slice(&flags.to_be_bytes());
        record.extend_from_slice(&0u32.to_be_bytes()); // cumulative drops
        record.extend_from_slice(&ts.to_be_bytes());
        record.extend_from_slice(&bytes);
        if file.write_all(&record).is_ok() {
            self.packets
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Flush and close, returning how many packets were written.
    fn finish(&self) -> std::io::Result<u64> {
        use std::io::Write as _;
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(mut file) = guard.take() {
            file.flush()?;
        }
        Ok(self.packets.load(std::sync::atomic::Ordering::Relaxed))
    }
}

#[async_trait::async_trait]
impl HciTransport for Recording {
    async fn send(&self, packet: HciPacket) -> Result<(), HciError> {
        self.record(&packet, false);
        self.inner.send(packet).await
    }

    async fn recv(&self) -> Result<HciPacket, HciError> {
        let packet = self.inner.recv().await?;
        self.record(&packet, true);
        Ok(packet)
    }
}
