//! Find, claim, and initialise a Bluetooth controller — and nothing else.
//!
//! The first thing to run against real hardware. It fails one step at a time with a
//! message naming the step, which is worth a great deal more than launching the whole
//! receiver and watching it not work.
//!
//! ```text
//! cargo run -p hci-transport --example probe            # list what is attached
//! cargo run -p hci-transport --example probe -- 8087:0029   # claim and initialise
//! ```
//!
//! On Linux the kernel's `btusb` driver holds the device until told otherwise:
//!
//! ```text
//! ls /sys/bus/usb/drivers/btusb/            # find the interface, e.g. 3-10:1.0
//! echo 3-10:1.0 | sudo tee /sys/bus/usb/drivers/btusb/unbind
//! ```
//!
//! That step is *required* to test firmware loading at all: `HCI_CHANNEL_USER` hands
//! over a controller the kernel has already initialised, so it can never exercise the
//! loader (architecture §11.3a).
//!
//! It is also not *sufficient*. Unbinding the driver does not make the controller forget
//! its firmware, and neither does a USB port reset — the operational image survives both,
//! and the loader takes the "already operational" branch every time. To reach the upload
//! path the part has to be sent back to its bootloader:
//!
//! ```text
//! modprobe -r btusb                                      # nothing may re-bind it
//! cargo run -p hci-transport --example probe -- 8087:0032 --to-bootloader
//! cargo run -p hci-transport --example probe -- 8087:0032    # now it loads firmware
//! ```
//!
//! On Linux, `udev` re-loads `btusb` the moment the part re-enumerates, which reloads the
//! firmware behind you; `echo 'install btusb /bin/true' > /run/modprobe.d/no-btusb.conf`
//! holds it off, and deleting that file plus `modprobe btusb` gives the machine its
//! Bluetooth back.

use hci_transport::init::{self, UsbId};
use hci_transport::{usb, FirmwareSet};
use substrate_hci::{Command, HciPacket, HciTransport as _};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "debug".into()),
        )
        .init();

    let firmware = FirmwareSet::embedded();
    println!("firmware embedded in this build:");
    if firmware.is_empty() {
        println!("  (none — set CASTAWAY_FIRMWARE_DIR at build time)");
    }
    for name in firmware.names() {
        println!("  {name}");
    }
    println!();

    let controllers = usb::list()?;
    if controllers.is_empty() {
        println!("no bluetooth controllers attached");
        return Ok(());
    }

    println!("controllers attached:");
    for (id, _) in &controllers {
        let loader = init::select(init::registry(), *id)
            .map_or_else(|_| "none".to_owned(), |l| l.name().to_owned());
        // Say up front whether the images this controller needs are present, rather
        // than discovering it half-way through an upload.
        let missing: Vec<String> = init::select(init::registry(), *id)
            .map(|l| {
                l.required_images(*id)
                    .iter()
                    .filter(|image| !firmware.has(image.name))
                    // Which ones stop the part booting and which only leave it untuned:
                    // the same distinction `driveability` ranks on (#307).
                    .map(|image| match image.necessity {
                        init::Necessity::Essential => image.name.to_owned(),
                        init::Necessity::Optional => format!("{} (optional)", image.name),
                    })
                    .collect()
            })
            .unwrap_or_default();
        print!(
            "  {id}  loader={loader} {:?}",
            init::driveability(*id, &firmware)
        );
        if missing.is_empty() {
            println!();
        } else {
            println!("  MISSING: {}", missing.join(", "));
        }
    }
    println!();

    let Some(requested) = std::env::args().nth(1) else {
        println!("pass a vendor:product to claim and initialise one");
        return Ok(());
    };
    let (vendor, product) = requested
        .split_once(':')
        .ok_or("expected vendor:product, e.g. 8087:0029")?;
    let id = UsbId::new(
        u16::from_str_radix(vendor, 16)?,
        u16::from_str_radix(product, 16)?,
    );

    println!("claiming {id}…");
    let transport = usb::UsbTransport::open(id)?;

    // Read-only mode: claim and ask the controller what it is, and stop. Nothing is
    // written, so a chip in an unknown state cannot be made worse — which is the mode
    // to reach for first, and was not available the day it would have helped.
    if std::env::args().any(|a| a == "--identify") {
        println!("reading local version (standard command, no vendor traffic)…");
        transport.send(Command::ReadLocalVersion.encode()?).await?;
        let params = expect_complete(&transport, substrate_hci::OpCode::READ_LOCAL_VERSION).await?;
        // `expect_complete` returns the return parameters *including* the leading status
        // byte. Forgetting that shifts every field by one, which reads as a plausible
        // wrong answer rather than an obvious failure — `manufacturer` came back as
        // 0x5d0a instead of 0x005d.
        let params = if params.is_empty() {
            params
        } else {
            params[1..].to_vec()
        };
        if params.len() >= 8 {
            let hci_rev = u16::from_le_bytes([params[1], params[2]]);
            let manufacturer = u16::from_le_bytes([params[4], params[5]]);
            let lmp_subver = u16::from_le_bytes([params[6], params[7]]);
            println!("  hci_ver:      {:#04x}", params[0]);
            println!("  hci_rev:      {hci_rev:#06x}");
            println!("  manufacturer: {manufacturer:#06x}");
            println!("  lmp_subver:   {lmp_subver:#06x}");
        } else {
            println!("  short response: {params:02x?}");
        }
        println!("  raw:          {params:02x?}");
        println!("\nidentify only; nothing was written");
        return Ok(());
    }

    // Put the part *back* into the bootloader, which is the only way to exercise the
    // loader at all. Unbinding `btusb` is not enough and neither is a USB port reset —
    // the operational image survives both, and the kernel says "Firmware already loaded".
    // This is `btintel_reset_to_bootloader()`: a hard reset that re-enumerates the
    // controller. It answers nothing, because it is gone by then.
    if std::env::args().any(|a| a == "--to-bootloader") {
        println!("resetting into the bootloader (the device will re-enumerate)…");
        transport
            .send(
                Command::Vendor {
                    opcode: substrate_hci::OpCode::new(0xFC01),
                    // hard reset, patch enable, ddc reload, current image, no address
                    params: bytes::Bytes::from_static(&[0x01, 0x01, 0x01, 0x00, 0, 0, 0, 0]),
                }
                .encode()?,
            )
            .await?;
        println!("sent; give it a second and run the probe again");
        return Ok(());
    }

    println!("initialising…");
    let loader = init::select(init::registry(), id)?;
    println!("  loader: {}", loader.name());
    loader.init(id, &transport, &firmware).await?;
    println!("  firmware ok");

    // The proof that the controller is alive and talking: reset it, then ask its
    // address. Anything less could pass with a part that enumerated and did nothing.
    println!("resetting…");
    transport.send(Command::Reset.encode()?).await?;
    expect_complete(&transport, substrate_hci::OpCode::RESET).await?;

    println!("reading address…");
    transport.send(Command::ReadBdAddr.encode()?).await?;
    let params = expect_complete(&transport, substrate_hci::OpCode::READ_BD_ADDR).await?;
    let addr = substrate_hci::event::parse_bd_addr(&params)?;
    println!("\ncontroller {id} is up at {addr}");

    Ok(())
}

/// Wait for a command to complete, returning its parameters.
async fn expect_complete(
    transport: &usb::UsbTransport,
    opcode: substrate_hci::OpCode,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    for _ in 0..64 {
        let packet = tokio::time::timeout(std::time::Duration::from_secs(5), transport.recv())
            .await
            .map_err(|_| format!("timed out waiting for {opcode}"))??;
        let HciPacket::Event { code, params } = packet else {
            continue;
        };
        if let substrate_hci::Event::CommandComplete {
            opcode: got,
            params,
            ..
        } = substrate_hci::Event::parse(code, &params)?
        {
            if got == opcode {
                return Ok(params.to_vec());
            }
        }
    }
    Err(format!("no completion for {opcode}").into())
}
