//! Listen on a PSM in Enhanced Retransmission Mode and echo whatever arrives.
//!
//! The differential test for the ERTM engine. Everything else that exercises it — the
//! unit tests, the two-multiplexer handshake, the adapter's end-to-end run — judges our
//! frames against *our own* decoder, which cannot catch a shared misreading of the spec.
//! Here the peer is the Linux kernel's L2CAP, driven by BlueZ's `l2test`, so the control
//! field, the sequence numbers and the frame check sequence are all marked by the
//! reference implementation. It is the pattern Q13 settled on, applied to a protocol
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

use std::sync::Arc;
use std::time::{Duration, Instant};

use proto_bluetooth_audio::host::{HostAction, HostConfig, HostController};
use proto_bluetooth_audio::AclWriter;
use substrate_hci::{ConnectionHandle, Event, HciPacket, Reassembler};
use substrate_l2cap::{ChannelMode, L2capEvent, L2capPdu, Multiplexer, Psm};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,substrate_l2cap=debug".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let index: u16 = args
        .next()
        .ok_or("usage: ertm_echo <hci index> [psm]")?
        .parse()?;
    let psm = Psm::new(
        args.next()
            .map_or(Ok(0x1005), |s| s.parse::<u16>())
            .map_err(|e| format!("bad psm: {e}"))?,
    )?;

    let transport: Arc<dyn substrate_hci::HciTransport> =
        Arc::new(hci_transport::socket::SocketTransport::open(index)?);
    println!("attached to hci{index}; echoing on {psm} in enhanced retransmission mode");

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
                if let L2capEvent::Send(pdu) = event {
                    acl.send(*handle, pdu);
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
                        }
                        HostAction::LinkDown { handle, peer, .. } => {
                            println!("link down from {peer}");
                            acl.link_down(*handle).await;
                            link = None;
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
                            let mode = mux.channel(cid).map(|c| c.mode);
                            println!("channel {cid} open in {mode:?}");
                            if mode != Some(ChannelMode::EnhancedRetransmission) {
                                eprintln!("  …which is not the mode this bench is for");
                            }
                        }
                        L2capEvent::Data { cid, payload, .. } => {
                            println!("{} bytes in on {cid}; echoing", payload.len());
                            echo.push((cid, payload));
                        }
                        L2capEvent::ChannelClosed { cid, .. } => println!("channel {cid} closed"),
                        other => println!("{other:?}"),
                    }
                }
                for (cid, payload) in echo {
                    match mux.send(cid, payload) {
                        Ok(events) => {
                            for event in events {
                                if let L2capEvent::Send(pdu) = event {
                                    acl.send(*handle, pdu);
                                }
                            }
                        }
                        Err(e) => eprintln!("could not echo: {e}"),
                    }
                }
            }
            _ => {}
        }
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
