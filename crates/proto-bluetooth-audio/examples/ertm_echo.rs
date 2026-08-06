//! Listen on a PSM in Enhanced Retransmission Mode and echo whatever arrives.
//!
//! The differential test for the ERTM engine. Everything else that exercises it — the
//! unit tests, the two-multiplexer handshake, the adapter's end-to-end run — judges our
//! frames against *our own* decoder, which cannot catch a shared misreading of the spec.
//! Here the peer is the Linux kernel's L2CAP, driven by BlueZ's `l2test`, so the control
//! field, the sequence numbers and the frame check sequence are all marked by the
//! reference implementation. It is the pattern #54 settled on, applied to a protocol
//! rather than a codec: pin the reference, run the real bytes through it.
//!
//! ```text
//! sudo btvirt -l2 &                       # two linked virtual controllers, no radio
//! sudo hciconfig hci2 down                # HCI_CHANNEL_USER is exclusive
//! sudo cargo run -p proto-bluetooth-audio --example ertm_echo -- 2 4101
//!
//! # …and from the other one, the kernel's own ERTM:
//! l2test -y -P 4101 -X ertm -N 4 -b 800 00:AA:01:01:00:02
//! ```
//!
//! `-b 800` matters: it is larger than one frame, so the kernel segments and our
//! reassembly is on the hook. What the run proves is that every echoed byte came back
//! through the kernel's own acknowledgement and reassembly logic — a wrong checksum, a
//! wrong sequence number or a wrong SAR bit stalls it rather than being tolerated.
//!
//! Since #210 this is not a bench: `checks.bluetooth-vm` builds it and runs all four
//! scenarios below on every CI run. The by-hand invocation above still works and is still
//! the fastest way to look at one of them under `btmon`.
//!
//! # Losing frames on purpose
//!
//! A lossless link only ever exercises the happy path, and the emulated air between two
//! `btvirt` controllers is lossless. So the three recovery paths that matter are induced
//! from here rather than waited for:
//!
//! ```text
//! --drop-inbound-i-frame 3   # the 3rd I-frame never happened: our REJ, their retransmit
//! --corrupt-inbound-fcs 3    # …or it arrived with a checksum that does not match
//! --drop-inbound-acks        # nothing we send is ever acknowledged: max_transmit, then death
//! ```
//!
//! The injection sits between the socket and the multiplexer deliberately. `substrate-l2cap`
//! has no switch for this and must not grow one — a library that can be told to drop a
//! frame is a library that can drop a frame in production. Here the frame is lost after
//! the kernel has sent it and before our mux has any record of it, which is the only
//! place a loss is indistinguishable from one the air caused: the kernel still counts the
//! frame as sent, still expects it acknowledged, and still retransmits when we say we
//! never had it.

#[cfg(not(target_os = "linux"))]
fn main() {
    // Compiled everywhere (D55) — `clippy --all-targets` at the default feature set is
    // what keeps this file honest — but there is nothing to point it at off Linux, since
    // `HCI_CHANNEL_USER` is how it takes a controller away from the kernel.
    eprintln!("ertm_echo drives a controller through Linux's raw HCI socket; this is not Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    echo::run()
}

#[cfg(target_os = "linux")]
mod echo {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bytes::{Bytes, BytesMut};
    use proto_bluetooth_audio::host::{HostAction, HostConfig, HostController};
    use proto_bluetooth_audio::AclWriter;
    use substrate_hci::{ConnectionHandle, Event, HciPacket, Reassembler};
    use substrate_l2cap::{
        ChannelMode, Cid, FcsType, Frame, L2capEvent, L2capPdu, Multiplexer, Psm,
    };

    /// What to do to the bytes coming off the link before the engine is told they exist.
    ///
    /// Every counter is of *inbound* frames on the data channel only. Signalling is never
    /// touched: losing a configuration response tests the mux's request timers, which have
    /// their own tests, and would only make a scenario here ambiguous about what failed.
    #[derive(Debug, Default)]
    struct Faults {
        /// Drop the nth inbound I-frame, once. One-based, because the first frame of the
        /// first SDU is the one nobody means.
        drop_i_frame: Option<u32>,
        /// Flip a bit of the nth inbound I-frame's checksum, once.
        corrupt_fcs: Option<u32>,
        /// Drop every inbound S-frame, so nothing we send is ever acknowledged.
        drop_acks: bool,
        i_frames: u32,
        dropped_acks: u32,
    }

    impl Faults {
        /// Whether anything at all is being injected — used only to say so at startup, so
        /// a run that was meant to be lossy and was not is visible in its first line.
        const fn any(&self) -> bool {
            self.drop_i_frame.is_some() || self.corrupt_fcs.is_some() || self.drop_acks
        }

        /// The PDU the multiplexer should see, or `None` if this one is to be lost.
        fn apply(&mut self, pdu: L2capPdu, fcs: FcsType) -> Option<L2capPdu> {
            let Ok(frame) = Frame::decode(&pdu.payload, pdu.cid, fcs) else {
                // A frame we cannot read is one the engine deserves the chance to refuse
                // on its own terms — spoiling it further would only mask that.
                return Some(pdu);
            };
            match frame {
                Frame::Supervisory { req_seq, .. } => {
                    if !self.drop_acks {
                        return Some(pdu);
                    }
                    self.dropped_acks = self.dropped_acks.saturating_add(1);
                    println!(
                        "injected: dropped inbound s-frame #{} (req_seq {req_seq})",
                        self.dropped_acks
                    );
                    None
                }
                Frame::Information { tx_seq, .. } => {
                    self.i_frames = self.i_frames.saturating_add(1);
                    let nth = self.i_frames;
                    if self.drop_i_frame == Some(nth) {
                        println!("injected: dropped inbound i-frame #{nth} (tx_seq {tx_seq})");
                        return None;
                    }
                    if self.corrupt_fcs == Some(nth) {
                        let mut spoiled = BytesMut::from(&pdu.payload[..]);
                        // The last two bytes *are* the frame check sequence, so this is a
                        // checksum that does not match its frame rather than a frame that
                        // does not match its checksum. Both present identically on the
                        // wire; only this one is what a bad link actually produces.
                        if let Some(last) = spoiled.last_mut() {
                            *last ^= 0x01;
                        }
                        println!("injected: flipped a checksum bit on inbound i-frame #{nth} (tx_seq {tx_seq})");
                        return Some(L2capPdu::new(pdu.cid, spoiled.freeze()));
                    }
                    Some(pdu)
                }
                // `Frame` is `#[non_exhaustive]`, and a kind of frame this file has never
                // heard of is not one it can meaningfully lose. Pass it through: the
                // engine is what gets to have an opinion about it.
                _ => Some(pdu),
            }
        }
    }

    struct Args {
        index: u16,
        psm: Psm,
        faults: Faults,
    }

    fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
        const USAGE: &str = "usage: ertm_echo <hci index> [psm] \
             [--drop-inbound-i-frame <n>] [--corrupt-inbound-fcs <n>] [--drop-inbound-acks]";
        let mut positional: Vec<String> = Vec::new();
        let mut faults = Faults::default();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut number = || -> Result<u32, Box<dyn std::error::Error>> {
                Ok(args.next().ok_or(USAGE)?.parse::<u32>()?)
            };
            match arg.as_str() {
                "--drop-inbound-i-frame" => faults.drop_i_frame = Some(number()?),
                "--corrupt-inbound-fcs" => faults.corrupt_fcs = Some(number()?),
                "--drop-inbound-acks" => faults.drop_acks = true,
                other if other.starts_with("--") => return Err(format!("{USAGE}\n{other}?").into()),
                other => positional.push(other.to_owned()),
            }
        }
        let index: u16 = positional.first().ok_or(USAGE)?.parse()?;
        let psm = Psm::new(match positional.get(1) {
            Some(raw) => raw.parse::<u16>().map_err(|e| format!("bad psm: {e}"))?,
            None => 0x1005,
        })?;
        Ok(Args { index, psm, faults })
    }

    #[tokio::main]
    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info,substrate_l2cap=debug".into()),
            )
            .init();

        let Args {
            index,
            psm,
            mut faults,
        } = parse_args()?;

        let transport: Arc<dyn substrate_hci::HciTransport> =
            Arc::new(hci_transport::socket::SocketTransport::open(index)?);
        println!("attached to hci{index}; echoing on {psm} in enhanced retransmission mode");
        if faults.any() {
            println!("injecting faults: {faults:?}");
        }

        let mut host = HostController::new(HostConfig {
            name: "castaway ertm echo".to_owned(),
            ..HostConfig::default()
        });
        let acl = AclWriter::spawn(Arc::clone(&transport));
        for action in host.start() {
            apply(&transport, &action).await?;
        }

        // One link is enough for a bench: `l2test` connects, sends, and goes away.
        let mut link: Option<(ConnectionHandle, Reassembler, Multiplexer)> = None;
        // Which channel the faults apply to, and what it agreed about checksums. `None`
        // until the channel is open, so the connection and configuration exchanges reach
        // the mux untouched whatever was asked for.
        let mut data: Option<(Cid, FcsType)> = None;
        let mut last_tick = Instant::now();

        loop {
            let due = link
                .as_ref()
                .and_then(|(_, _, mux)| mux.next_timeout())
                .unwrap_or(Duration::from_secs(3600));
            let received = tokio::select! {
                packet = transport.recv() => Some(packet?),
                () = tokio::time::sleep(due) => None,
            };

            // Retransmission timers move on wall clock, whatever woke us.
            let elapsed = last_tick.elapsed();
            last_tick = Instant::now();
            if let Some((handle, _, mux)) = link.as_mut() {
                for event in mux.tick(elapsed) {
                    match event {
                        L2capEvent::Send(pdu) => acl.send(*handle, pdu),
                        // The one event a timer can produce on its own, and the whole of
                        // the `--drop-inbound-acks` scenario: `max_transmit` transmissions
                        // went unanswered, so the engine gave up and the mux told the peer.
                        L2capEvent::ChannelClosed { cid, .. } => {
                            println!(
                                "channel {cid} closed after its retransmission allowance ran out"
                            );
                            data = None;
                        }
                        other => println!("{other:?}"),
                    }
                }
            }

            let Some(packet) = received else { continue };
            match packet {
                HciPacket::Event { code, params } => {
                    let Ok(event) = Event::parse(code, &params) else {
                        continue;
                    };
                    for action in host.on_event(&event) {
                        match &action {
                            HostAction::Ready {
                                address,
                                acl_credits,
                                acl_mtu,
                            } => {
                                acl.configure(*acl_credits, *acl_mtu).await;
                                println!("discoverable at {address} (acl mtu {acl_mtu})");
                            }
                            HostAction::Credits { handle, count } => {
                                acl.completed(*handle, *count).await;
                            }
                            HostAction::LinkUp { handle, peer } => {
                                println!("link up from {peer}");
                                let mut mux = Multiplexer::new(672);
                                mux.listen_with(psm, ChannelMode::EnhancedRetransmission);
                                link = Some((*handle, Reassembler::new(), mux));
                                data = None;
                            }
                            HostAction::LinkDown { handle, peer, .. } => {
                                println!("link down from {peer}");
                                acl.link_down(*handle).await;
                                link = None;
                                data = None;
                            }
                            _ => {}
                        }
                        apply(&transport, &action).await?;
                    }
                }
                HciPacket::Acl(packet) => {
                    let Some((handle, reassembler, mux)) = link.as_mut() else {
                        continue;
                    };
                    let Ok(Some(bytes)) = reassembler.push(&packet) else {
                        continue;
                    };
                    let Ok(pdu) = L2capPdu::decode(&bytes) else {
                        continue;
                    };
                    // Faults apply to the data channel and only once it is open, so
                    // nothing here can lose a connection or configuration signal.
                    let pdu = match data {
                        Some((cid, fcs)) if cid == pdu.cid => match faults.apply(pdu, fcs) {
                            Some(pdu) => pdu,
                            None => continue,
                        },
                        _ => pdu,
                    };
                    let events = match mux.handle_pdu(&pdu) {
                        Ok(events) => events,
                        Err(e) => {
                            eprintln!("l2cap refused a pdu: {e}");
                            continue;
                        }
                    };
                    let mut echo = Vec::new();
                    for event in events {
                        match event {
                            L2capEvent::Send(pdu) => acl.send(*handle, pdu),
                            L2capEvent::ChannelOpen { cid, .. } => {
                                let Some(channel) = mux.channel(cid) else {
                                    continue;
                                };
                                // What the *kernel* agreed to, which is the content of this
                                // differential: every number below came out of its
                                // configuration response rather than out of our defaults.
                                let p = &channel.parameters;
                                println!(
                                    "channel {cid} open in {:?} (mps {}, tx window {}, \
                                     max transmit {}, fcs {:?}, retransmission {:?}, monitor {:?})",
                                    channel.mode,
                                    p.send_mps,
                                    p.send_window,
                                    p.max_transmit,
                                    p.fcs,
                                    p.retransmission_timeout,
                                    p.monitor_timeout,
                                );
                                if channel.mode != ChannelMode::EnhancedRetransmission {
                                    eprintln!("  …which is not the mode this test is for");
                                }
                                if faults.corrupt_fcs.is_some() && !p.fcs.is_present() {
                                    // Fail here rather than flip a bit of *data* and call
                                    // it a checksum: the two are indistinguishable to a
                                    // grep of the log and only one of them is the test.
                                    eprintln!(
                                        "--corrupt-inbound-fcs needs a channel with a checksum, \
                                         and this one negotiated {:?}",
                                        p.fcs
                                    );
                                    std::process::exit(2);
                                }
                                data = Some((cid, p.fcs));
                            }
                            L2capEvent::Data { cid, payload, .. } => {
                                println!("{} bytes in on {cid}; echoing", payload.len());
                                echo.push((cid, payload));
                            }
                            L2capEvent::ChannelClosed { cid, .. } => {
                                println!("channel {cid} closed");
                                data = None;
                            }
                            other => println!("{other:?}"),
                        }
                    }
                    for (cid, payload) in echo {
                        send_echo(&acl, *handle, mux, cid, payload);
                    }
                }
                _ => {}
            }
        }
    }

    /// Put an SDU back where it came from, and say so if the channel died doing it.
    fn send_echo(
        acl: &AclWriter,
        handle: ConnectionHandle,
        mux: &mut Multiplexer,
        cid: Cid,
        payload: Bytes,
    ) {
        match mux.send(cid, payload) {
            Ok(events) => {
                for event in events {
                    match event {
                        L2capEvent::Send(pdu) => acl.send(handle, pdu),
                        L2capEvent::ChannelClosed { cid, .. } => {
                            println!(
                                "channel {cid} closed after its retransmission allowance ran out"
                            );
                        }
                        other => println!("{other:?}"),
                    }
                }
            }
            Err(e) => eprintln!("could not echo: {e}"),
        }
    }

    async fn apply(
        transport: &Arc<dyn substrate_hci::HciTransport>,
        action: &HostAction,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let HostAction::Send(command) = action {
            transport.send(command.encode()?).await?;
        }
        Ok(())
    }
}
