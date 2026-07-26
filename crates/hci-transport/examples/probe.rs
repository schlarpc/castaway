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
        let missing: Vec<&str> = init::select(init::registry(), *id)
            .map(|l| {
                l.required_images()
                    .iter()
                    .filter(|n| !firmware.has(n))
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        print!("  {id}  loader={loader}");
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

    println!("initialising…");
    let loader = init::select(init::registry(), id)?;
    println!("  loader: {}", loader.name());
    loader.init(&transport, &firmware).await?;
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
