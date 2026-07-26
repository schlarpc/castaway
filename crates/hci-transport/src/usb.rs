//! The USB backend: HCI over the standard Bluetooth USB class interface.
//!
//! One implementation for both platforms. On Windows it is the *only* option, because
//! Winsock exposes no L2CAP and the inbox stack hands us nothing; on Linux it is the only
//! way to exercise [`crate::ControllerInit`], since `HCI_CHANNEL_USER` gives back a
//! controller the kernel already initialised.
//!
//! No `unsafe`: `nusb` is a safe API, which is also what keeps both firmware loaders in
//! safe Rust.
//!
//! ## The endpoint layout is the framing
//!
//! HCI-over-USB does not use the packet-type indicator byte that UART transports carry.
//! The *endpoint* says what a packet is: commands go out on the control pipe, events
//! arrive on interrupt IN, ACL moves on the bulk pipes. That is why
//! [`substrate_hci::HciPacket::decode_body`] exists — the type comes from context here,
//! and from a leading byte everywhere else.

use nusb::transfer::{ControlOut, ControlType, Queue, Recipient, RequestBuffer, TransferError};
use nusb::{Device, DeviceInfo, Interface};
use substrate_hci::{HciError, HciPacket, HciTransport, PacketType};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::TransportError;
use crate::firmware::FirmwareSet;
use crate::init::{self, UsbId};

/// USB class triple that identifies a Bluetooth HCI interface.
const CLASS_WIRELESS: u8 = 0xE0;
const SUBCLASS_RF: u8 = 0x01;
const PROTOCOL_BLUETOOTH: u8 = 0x01;

/// Endpoint addresses fixed by the Bluetooth USB transport spec.
const EP_EVENT_IN: u8 = 0x81;
const EP_ACL_IN: u8 = 0x82;
const EP_ACL_OUT: u8 = 0x02;

/// Largest event a controller will send. Interrupt IN transfers are short.
const EVENT_BUF: usize = 260;
/// ACL reads are sized for the largest fragment any controller offers.
const ACL_BUF: usize = 1024;

/// A controller reached over USB.
pub struct UsbTransport {
    interface: Interface,
    id: UsbId,
    /// Serialises reads. Two tasks polling the same endpoint would interleave packets.
    reader: Mutex<Reader>,
    _device: Device,
}

/// Both IN endpoints, kept armed at once.
///
/// Events and ACL arrive on separate endpoints with no ordering between them and no way
/// to know which will speak next. **Reading one at a time blocks forever on whichever is
/// idle** — which is most of the time, because a controller sitting there with no
/// connection sends events and no ACL at all. Keeping a transfer in flight on each and
/// taking whichever completes is the only arrangement that works; an earlier version
/// alternated, and hung on the first real controller it met.
struct Reader {
    events: Queue<RequestBuffer>,
    acl: Queue<RequestBuffer>,
    /// Packets already read but not yet returned, so one transfer can yield several.
    queued: std::collections::VecDeque<HciPacket>,
}

/// What a completed IN transfer told us to do next.
enum Read {
    /// Bytes arrived.
    Data(Vec<u8>),
    /// The endpoint stalled and was recovered; nothing to decode this time round.
    Recovered,
}

/// Handle one IN completion, clearing a stall rather than dying of it.
///
/// A STALL is the device signalling an error condition on that pipe, not the end of the
/// world: for bulk and interrupt endpoints it is cleared with `CLEAR_FEATURE(HALT)` and
/// the pipe carries on. Treating it as fatal cost a live session — the reader exited
/// silently, and a phone spent five minutes pairing with a controller nothing was
/// listening to.
fn handle_completion(
    queue: &mut Queue<RequestBuffer>,
    completion: nusb::transfer::Completion<Vec<u8>>,
    what: &'static str,
    buf_len: usize,
) -> Result<Read, HciError> {
    match completion.into_result() {
        Ok(data) => {
            queue.submit(RequestBuffer::new(buf_len));
            Ok(Read::Data(data))
        }
        Err(TransferError::Stall) => {
            warn!(endpoint = what, "endpoint stalled; clearing and re-arming");
            queue
                .clear_halt()
                .map_err(|e| HciError::Transport(format!("{what}: clearing stall: {e}")))?;
            queue.submit(RequestBuffer::new(buf_len));
            Ok(Read::Recovered)
        }
        // Everything else really is fatal: the device is gone, or the transfer was
        // cancelled because we are shutting down.
        Err(e) => Err(HciError::Transport(format!("{what}: {e}"))),
    }
}

impl Reader {
    /// Arm both endpoints.
    fn new(interface: &Interface) -> Self {
        let mut events = interface.interrupt_in_queue(EP_EVENT_IN);
        let mut acl = interface.bulk_in_queue(EP_ACL_IN);
        // Clear any halt inherited from a previous owner before arming. A process that
        // died on a stall leaves the pipe halted, and the next claim then times out
        // during vendor initialisation — which reads as a dead dongle needing a replug
        // rather than as a pipe needing one control transfer.
        for (queue, what) in [(&mut events, "interrupt in"), (&mut acl, "bulk in")] {
            if let Err(e) = queue.clear_halt() {
                debug!(endpoint = what, error = %e, "no stall to clear at open");
            }
        }
        events.submit(RequestBuffer::new(EVENT_BUF));
        acl.submit(RequestBuffer::new(ACL_BUF));
        Self {
            events,
            acl,
            queued: std::collections::VecDeque::new(),
        }
    }
}

impl std::fmt::Debug for UsbTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbTransport")
            .field("id", &self.id)
            .finish()
    }
}

/// Every Bluetooth controller currently attached.
///
/// # Errors
/// [`TransportError::Io`] if the USB device list cannot be read.
pub fn list() -> Result<Vec<(UsbId, DeviceInfo)>, TransportError> {
    let devices = nusb::list_devices().map_err(|e| TransportError::Io(e.to_string()))?;
    Ok(devices
        .filter(is_bluetooth)
        .map(|info| (UsbId::new(info.vendor_id(), info.product_id()), info))
        .collect())
}

/// Whether a device advertises the Bluetooth HCI class triple.
fn is_bluetooth(info: &DeviceInfo) -> bool {
    // Some controllers report the triple on the device descriptor, others only on an
    // interface. Checking both is what makes this work across vendors.
    if info.class() == CLASS_WIRELESS
        && info.subclass() == SUBCLASS_RF
        && info.protocol() == PROTOCOL_BLUETOOTH
    {
        return true;
    }
    info.interfaces().any(|i| {
        i.class() == CLASS_WIRELESS
            && i.subclass() == SUBCLASS_RF
            && i.protocol() == PROTOCOL_BLUETOOTH
    })
}

impl UsbTransport {
    /// Open the first Bluetooth controller found.
    ///
    /// # Errors
    /// [`TransportError::NoDevice`] if none is attached, or [`TransportError::Claim`] if
    /// the OS will not hand it over.
    pub fn open_first() -> Result<Self, TransportError> {
        let (id, info) = list()?
            .into_iter()
            .next()
            .ok_or(TransportError::NoDevice(None))?;
        Self::open_info(id, &info)
    }

    /// Open a specific controller by USB id.
    ///
    /// # Errors
    /// [`TransportError::NoDevice`] if no such device is attached.
    pub fn open(id: UsbId) -> Result<Self, TransportError> {
        let (id, info) = list()?
            .into_iter()
            .find(|(found, _)| *found == id)
            .ok_or(TransportError::NoDevice(Some(id)))?;
        Self::open_info(id, &info)
    }

    fn open_info(id: UsbId, info: &DeviceInfo) -> Result<Self, TransportError> {
        let device = info.open().map_err(|e| TransportError::Claim {
            id,
            detail: claim_hint(&e.to_string()),
        })?;
        // Interface 0 carries commands, events and ACL. Interface 1 is the isochronous
        // SCO pipe, which an A2DP sink never touches.
        let interface = device
            .claim_interface(0)
            .map_err(|e| TransportError::Claim {
                id,
                detail: claim_hint(&e.to_string()),
            })?;
        info!(%id, "opened bluetooth controller over USB");
        let reader = Reader::new(&interface);
        Ok(Self {
            interface,
            id,
            reader: Mutex::new(reader),
            _device: device,
        })
    }

    /// The controller's USB id.
    #[must_use]
    pub const fn id(&self) -> UsbId {
        self.id
    }

    /// Open a controller and run its firmware loader.
    ///
    /// # Errors
    /// Whatever opening or initialising returns.
    pub async fn open_and_init(
        id: Option<UsbId>,
        firmware: &FirmwareSet,
    ) -> Result<Self, TransportError> {
        let transport = match id {
            Some(id) => Self::open(id)?,
            None => Self::open_first()?,
        };
        let loader = init::select(init::registry(), transport.id)?;
        debug!(loader = loader.name(), id = %transport.id, "initialising controller");
        loader.init(&transport, firmware).await?;
        Ok(transport)
    }
}

/// Turn a claim failure into something actionable.
///
/// This is the error everyone hits first, and "Access denied" alone sends people looking
/// in the wrong place.
fn claim_hint(detail: &str) -> String {
    let hint = if cfg!(target_os = "linux") {
        "the kernel's btusb driver is probably still bound — unbind it \
         (/sys/bus/usb/drivers/btusb/unbind) and check udev permissions"
    } else {
        "the device must be bound to WinUSB rather than the Microsoft Bluetooth driver"
    };
    format!("{detail} ({hint})")
}

#[async_trait::async_trait]
impl HciTransport for UsbTransport {
    async fn send(&self, packet: HciPacket) -> Result<(), HciError> {
        match packet {
            HciPacket::Command { opcode, params } => {
                // Commands go out on the *control* pipe as a class request, not on a
                // bulk endpoint — and with no packet-type indicator, since the pipe
                // already says what this is.
                let mut body = Vec::with_capacity(3 + params.len());
                body.extend_from_slice(&opcode.raw().to_le_bytes());
                body.push(u8::try_from(params.len()).unwrap_or(u8::MAX));
                body.extend_from_slice(&params);
                self.interface
                    .control_out(ControlOut {
                        control_type: ControlType::Class,
                        recipient: Recipient::Interface,
                        request: 0x00,
                        value: 0x00,
                        index: 0x00,
                        data: &body,
                    })
                    .await
                    .into_result()
                    .map_err(|e| HciError::Transport(format!("control out: {e}")))?;
                Ok(())
            }
            HciPacket::Acl(acl) => {
                let framed = HciPacket::Acl(acl).encode()?;
                // Skip the indicator byte the encoder writes: the endpoint carries that
                // information on USB.
                self.interface
                    .bulk_out(EP_ACL_OUT, framed[1..].to_vec())
                    .await
                    .into_result()
                    .map_err(|e| HciError::Transport(format!("bulk out: {e}")))?;
                Ok(())
            }
            other => Err(HciError::Transport(format!(
                "cannot send {:?} over USB",
                other.packet_type()
            ))),
        }
    }

    async fn recv(&self) -> Result<HciPacket, HciError> {
        let mut reader = self.reader.lock().await;
        loop {
            if let Some(packet) = reader.queued.pop_front() {
                return Ok(packet);
            }

            // Await *both* endpoints. Taking them in turn blocks on whichever is idle,
            // and an idle controller sends events and no ACL whatsoever — which is
            // exactly how this hung the first time it met real hardware.
            let reader = &mut *reader;
            let (kind, read) = tokio::select! {
                completion = reader.events.next_complete() => {
                    let read = handle_completion(
                        &mut reader.events, completion, "interrupt in", EVENT_BUF,
                    )?;
                    (PacketType::Event, read)
                }
                completion = reader.acl.next_complete() => {
                    let read = handle_completion(
                        &mut reader.acl, completion, "bulk in", ACL_BUF,
                    )?;
                    (PacketType::AclData, read)
                }
            };

            let Read::Data(data) = read else {
                continue;
            };
            if data.is_empty() {
                continue;
            }
            match HciPacket::decode_body(kind, &data) {
                Ok(packet) => reader.queued.push_back(packet),
                Err(e) => warn!(error = %e, ?kind, "dropping a malformed USB packet"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_claim_hint_points_at_the_thing_that_is_actually_wrong() {
        // "Access denied" alone sends people to udev when the real problem is btusb
        // still holding the device — the first thing everyone hits.
        let hint = claim_hint("Access denied");
        assert!(hint.contains("Access denied"));
        if cfg!(target_os = "linux") {
            assert!(hint.contains("btusb"), "got: {hint}");
        } else {
            assert!(hint.contains("WinUSB"), "got: {hint}");
        }
    }

    #[test]
    fn endpoint_addresses_match_the_bluetooth_usb_transport_spec() {
        // These are fixed by the spec, not discovered. Getting one wrong reads ACL data
        // off the event pipe, which decodes as garbage events rather than failing.
        assert_eq!(EP_EVENT_IN, 0x81, "interrupt IN");
        assert_eq!(EP_ACL_IN, 0x82, "bulk IN");
        assert_eq!(EP_ACL_OUT, 0x02, "bulk OUT");
    }

    #[test]
    fn listing_devices_does_not_error_on_a_box_with_none() {
        // CI has no Bluetooth hardware; enumeration must still succeed and return an
        // empty list rather than failing the whole startup path.
        match list() {
            Ok(found) => {
                for (id, _) in &found {
                    assert_ne!(id.vendor, 0, "a listed device must have a vendor id");
                }
            }
            Err(e) => panic!("enumeration should succeed even with no devices: {e}"),
        }
    }
}
