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
//! sudo -E cargo run -p proto-bluetooth-audio --example phone_bench
//! # …or name a different controller and output directory:
//! sudo -E cargo run -p proto-bluetooth-audio --example phone_bench \
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

    // Link keys, kept beside the capture. Without this every run is a fresh pairing —
    // and worse than that, the *phone* still holds the old bond, so it offers a key we
    // answer `LinkKeyRequestNegativeReply` to and iOS quietly refuses to connect until
    // somebody taps "Forget This Device". A bench meant to be run repeatedly against the
    // same phone cannot afford that.
    let keys_path = out_dir.join("link-keys.txt");
    let link_keys = load_link_keys(&keys_path);
    if !link_keys.is_empty() {
        println!("remembered {} paired peer(s)", link_keys.len());
    }
    let writer = keys_path.clone();
    let config = BluetoothConfig {
        // `fetch_best_cover_art` is the default now, so the bench inherits it rather than
        // turning it on — which also means what it captures is what a guest would get.
        link_keys,
        on_paired: Some(Arc::new(move |addr, key| save_link_key(&writer, addr, key))),
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
    println!("  2. play a track, and let one track change happen");
    println!("  3. try more than one app — the settings listing is per *player*, and");
    println!("     what the image server holds may be too (#75). A local file with big");
    println!("     embedded artwork is the sharpest test of that.");
    println!("  4. Ctrl-C here");
    println!();
    println!("every distinct cover image is written beside the capture, so a larger form");
    println!("and the thumbnail can be compared rather than argued about.");
    println!();

    // Shared rather than returned from the task: Ctrl-C aborts the observer, and an
    // aborted `JoinHandle` yields a `JoinError`, not the tally — which would have made
    // every checklist read as though nothing happened.
    let seen = Arc::new(std::sync::Mutex::new(Seen::default()));
    let observer = tokio::spawn(observe(rx, Arc::clone(&seen), out_dir.clone()));
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
    /// Encoded frames actually consumed. Zero beside `audio: true` means the session was
    /// torn down as soon as it opened, which is what dropping the receiver looks like.
    frames: u64,
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
        line(
            self.frames > 0,
            &format!("…and was consumed ({} frames)", self.frames),
        );
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
        if self.audio && self.frames == 0 {
            println!();
            println!("audio opened and delivered nothing, so the session was torn down at");
            println!("its first packet and every later card update was discarded. The three");
            println!("lines above are then meaningless — read the btsnoop, not this list.");
        }
    }
}

async fn observe(
    mut rx: mpsc::Receiver<SourceMessage>,
    seen: Arc<std::sync::Mutex<Seen>>,
    out_dir: PathBuf,
) {
    /// Update the tally without holding the lock a moment longer than the closure.
    fn note(seen: &std::sync::Mutex<Seen>, f: impl FnOnce(&mut Seen)) {
        let mut guard = match seen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard);
    }

    while let Some(msg) = rx.recv().await {
        match msg.event {
            SessionEvent::SourceInfo(description) => {
                note(&seen, |s| s.connected = true);
                println!("● connected: {description:?}");
            }
            // The frame receiver has to be *taken and kept*, not matched past. Dropping it
            // is not inert: the adapter sees `TrySendError::Closed` on the next media
            // packet, tears the session down and clears `session_open` — after which every
            // NowPlaying and every fetched cover is discarded before it is emitted. The
            // first run of this bench did exactly that, and reported three questions
            // unanswered that the capture shows the phone had answered perfectly.
            SessionEvent::Audio { source, format, .. } => {
                note(&seen, |s| s.audio = true);
                println!("● audio: {format:?}");
                if let castaway_core::FrameSource::Encoded(mut frames) = source {
                    let tally = Arc::clone(&seen);
                    tokio::spawn(async move {
                        // Drained rather than merely held. A receiver nobody reads fills
                        // up, and a full queue is a different failure that would read as a
                        // radio problem.
                        let mut count = 0u64;
                        while frames.recv().await.is_some() {
                            count += 1;
                            // Tallied as they arrive rather than once the stream ends,
                            // because Ctrl-C is how this bench is *documented* to finish
                            // — the stream is still open at that point, so a count
                            // written only on close is always zero. `report()` then read
                            // that zero as "the session was torn down at its first
                            // packet" and told the operator to disbelieve three lines
                            // that were correct. A real run of LDAC at 96 kHz, 10423 ACL
                            // packets on the wire, reported exactly that.
                            note(&tally, |s| s.frames = count);
                        }
                        println!("● audio stream ended after {count} frames");
                    });
                }
            }
            SessionEvent::NowPlaying(now) => {
                // Every distinct image, written out. This is what settles the open half
                // of #75: when the peer offers a form larger than the linked thumbnail,
                // both arrive here as separate snapshots for the same track, and the
                // question "is the bigger one a genuine render or an upscale of the
                // smaller" is answered by comparing the two files, not by reading a spec.
                if let Some(art) = &now.artwork {
                    note(&seen, |s| s.artwork = true);
                    write_artwork(&out_dir, art);
                }
                note(&seen, |s| {
                    s.metadata |= now.title.is_some();
                    s.shuffle_or_repeat |= now.shuffle.is_some() || now.repeat.is_some();
                });
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
            other => println!("● {other:?}"),
        }
    }
}

/// Read remembered link keys, tolerating a missing or half-written file.
fn load_link_keys(path: &Path) -> Vec<(substrate_hci::BdAddr, substrate_hci::LinkKey)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let (addr, key) = line.split_once(' ')?;
            let addr = addr.trim().parse().ok()?;
            let raw = (0..16)
                .map(|i| u8::from_str_radix(key.get(i * 2..i * 2 + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            Some((addr, substrate_hci::LinkKey::new(raw.try_into().ok()?)))
        })
        .collect()
}

/// Remember a newly paired peer, or forget one whose key was refused.
fn save_link_key(path: &Path, addr: substrate_hci::BdAddr, key: Option<substrate_hci::LinkKey>) {
    let mut keys = load_link_keys(path);
    keys.retain(|(a, _)| *a != addr);
    match key {
        Some(key) => {
            println!("● paired with {addr}; remembering the key");
            keys.push((addr, key));
        }
        // `None` means the peer refused the key we offered — keeping it would make every
        // future run fail the same way.
        None => println!("● {addr} refused its stored key; forgetting it"),
    }
    let body: String = keys
        .iter()
        .map(|(a, k)| {
            let hex: String = k.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
            format!("{a} {hex}\n")
        })
        .collect();
    if let Err(e) = std::fs::write(path, body) {
        println!("  could not write {}: {e}", path.display());
    }
}

/// Write one cover image out, named by its content so duplicates collapse.
///
/// Content-addressed rather than counted: the same track's art is re-fetched on every
/// reconnect and after every track change back to it, and a directory of forty identical
/// JPEGs would bury the one comparison worth making.
fn write_artwork(dir: &Path, art: &castaway_core::Artwork) {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in art.data.iter() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    let name = format!("art-{:016x}-{}b.{:?}", hash, art.len(), art.format).to_lowercase();
    let path = dir.join(name);
    if path.exists() {
        return;
    }
    match std::fs::write(&path, &art.data) {
        Ok(()) => println!("  wrote {}", path.display()),
        Err(e) => println!("  could not write {}: {e}", path.display()),
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
