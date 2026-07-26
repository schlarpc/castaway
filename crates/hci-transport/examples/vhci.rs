//! Attach to a controller over the raw HCI socket and prove it answers.
//!
//! Built for virtual controllers: `btvirt -l2` makes a linked pair that need no
//! firmware, so this is the shortest path to exercising the stack above HCI with no
//! radio at all (architecture §11.7).
//!
//! ```text
//! sudo btvirt -l2 &          # creates two linked virtual controllers
//! sudo cargo run -p hci-transport --features socket --example vhci -- 1
//! ```

use hci_transport::socket::SocketTransport;
use substrate_hci::{Command, HciPacket, HciTransport as _};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let index: u16 = std::env::args()
        .nth(1)
        .ok_or("usage: vhci <hci index>")?
        .parse()?;

    println!("attaching to hci{index}…");
    let transport = SocketTransport::open(index)?;

    println!("resetting…");
    transport.send(Command::Reset.encode()?).await?;
    expect(&transport, substrate_hci::OpCode::RESET).await?;

    println!("reading address…");
    transport.send(Command::ReadBdAddr.encode()?).await?;
    let params = expect(&transport, substrate_hci::OpCode::READ_BD_ADDR).await?;
    let addr = substrate_hci::event::parse_bd_addr(&params)?;

    println!("reading buffer size…");
    transport.send(Command::ReadBufferSize.encode()?).await?;
    let params = expect(&transport, substrate_hci::OpCode::READ_BUFFER_SIZE).await?;
    let buffers = substrate_hci::BufferSize::parse(&params)?;

    println!("\nhci{index} is up at {addr}");
    println!("  acl mtu:     {}", buffers.acl_max_len);
    println!("  acl buffers: {}", buffers.total_packets);
    Ok(())
}

async fn expect(
    transport: &SocketTransport,
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
