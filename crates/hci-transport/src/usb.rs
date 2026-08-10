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

use std::time::Duration;

use nusb::transfer::{
    Buffer, Bulk, BulkOrInterrupt, Completion, ControlOut, ControlType, In, Interrupt, Out,
    Recipient, TransferError,
};
use nusb::{Device, DeviceInfo, Endpoint, Interface, MaybeFuture};
use substrate_hci::{HciError, HciPacket, HciTransport, OpCode, PacketType};
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
///
/// The four bytes past a round number are the HCI ACL header, and leaving them out is a
/// live bug rather than a tidiness one. A bulk IN transfer *larger than the buffer it was
/// given* is an overflow, not a short read — usbfs fails it `-EOVERFLOW`, WinUSB fails the
/// pipe — and the reader treats every non-STALL error as fatal. So the buffer has to hold
/// the largest packet we ourselves invite, and we invite a big one on purpose: the
/// adapter advertises a 1017-byte L2CAP MTU so a full SDU lands in one ACL packet, which
/// is 1017 + 4 (L2CAP header) + 4 (HCI ACL header) = 1025. At 1024 the first
/// maximum-size PDU from a phone killed the stack.
///
/// The kernel's `btusb` sizes this the same way and for the same reason
/// (`HCI_MAX_FRAME_SIZE = HCI_MAX_ACL_SIZE + 4`); ours is that, rounded up.
const ACL_BUF: usize = 1028;

/// The largest L2CAP MTU any of our profile crates advertises, and the headers under it.
///
/// A build-time assertion rather than a test, because the failure it guards is a dead
/// Bluetooth stack on the first big packet — that should not be able to compile. If
/// `proto-bluetooth-audio` ever raises its MTU past what this buffer holds, the build
/// stops here instead of the panel going quiet on someone's first long track title.
const _: () = {
    const ADVERTISED_L2CAP_MTU: usize = 1017;
    const L2CAP_HEADER: usize = 4;
    const HCI_ACL_HEADER: usize = 4;
    assert!(ACL_BUF >= ADVERTISED_L2CAP_MTU + L2CAP_HEADER + HCI_ACL_HEADER);
};

/// How long a control transfer may take before it is abandoned.
///
/// The control pipe is how every HCI *command* leaves this host, so a hang here is the
/// stack going quiet with no error — the same failure the stall handling above exists to
/// avoid. `nusb` requires the caller to name a bound; the kernel's own `btusb` uses
/// `USB_CTRL_SET_TIMEOUT`, which is five seconds, and there is no reason to disagree with
/// it. A controller that has not accepted a 3-byte command header in five seconds is not
/// going to.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// Say what a refused command transfer actually means, and which command it was.
///
/// Two lies to undo, and they cost a kernel diff to see through once (#229).
///
/// `nusb` implements a timeout by *cancelling* the transfer, so an expired
/// [`CONTROL_TIMEOUT`] surfaces as [`TransferError::Cancelled`] — rendered "transfer was
/// cancelled", which reads as a shutdown race. It is the opposite: nothing here cancels
/// anything, and the transfer is abandoned precisely because the controller never took it.
///
/// And every command left through this one line with no name attached, so a controller
/// that refused `Read_Version` and one that refused the forty-seventh firmware fragment
/// produced the same sentence. Naming the opcode is most of the diagnosis: on Intel parts
/// `FC09` in particular must go down the *bulk* endpoint while the bootloader is running,
/// which is a different bug (#229) that this message is how you find.
fn control_failure(opcode: OpCode, error: &TransferError) -> HciError {
    match error {
        TransferError::Cancelled => HciError::Transport(format!(
            "control out: the controller did not accept {opcode} within {CONTROL_TIMEOUT:?}"
        )),
        other => HciError::Transport(format!("control out: {opcode}: {other}")),
    }
}

/// Round an IN transfer length up to a whole number of packets.
///
/// `nusb` 0.2 rejects an IN transfer whose requested length is not a nonzero multiple of
/// the endpoint's maximum packet size — it fails the transfer with
/// [`TransferError::InvalidArgument`] rather than short-reading. The sizes above are
/// derived from what HCI can put on the wire, not from any endpoint's geometry, so they
/// have to be rounded to fit. Rounding *up* is what keeps [`ACL_BUF`]'s guarantee intact:
/// the buffer only ever grows, so it still holds the largest packet we invite.
const fn whole_packets(len: usize, max_packet_size: usize) -> usize {
    // A controller reporting a zero max packet size is malformed; leaving the length
    // alone lets `submit` report it as the transfer error it is, rather than dividing by
    // zero here.
    if max_packet_size == 0 {
        return len;
    }
    len.div_ceil(max_packet_size) * max_packet_size
}

/// A controller reached over USB.
pub struct UsbTransport {
    interface: Interface,
    id: UsbId,
    /// Serialises reads. Two tasks polling the same endpoint would interleave packets.
    reader: Mutex<Reader>,
    /// Serialises ACL writes. Submitting a transfer needs `&mut` on the endpoint, and
    /// [`HciTransport::send`] only has `&self`.
    acl_out: Mutex<Endpoint<Bulk, Out>>,
    /// Whether the controller is running a bootloader that puts HCI on the bulk pipes.
    ///
    /// See [`HciTransport::set_bootloader_framing`]. Atomic because `send`/`recv` take
    /// `&self` and the loader flips this between them.
    bootloader: std::sync::atomic::AtomicBool,
    _device: Device,
}

/// `Secure_Send` — the one command an Intel bootloader wants on the bulk pipe.
///
/// `btusb_send_frame_intel` singles this opcode out by number and so do we: everything
/// else, `Read_Version` and `Intel_Reset` included, stays on control in both modes, which
/// is what the capture of the kernel's own bring-up shows.
const SECURE_SEND: OpCode = OpCode::new(0xFC09);

/// Both IN endpoints, kept armed at once.
///
/// Events and ACL arrive on separate endpoints with no ordering between them and no way
/// to know which will speak next. **Reading one at a time blocks forever on whichever is
/// idle** — which is most of the time, because a controller sitting there with no
/// connection sends events and no ACL at all. Keeping a transfer in flight on each and
/// taking whichever completes is the only arrangement that works; an earlier version
/// alternated, and hung on the first real controller it met.
struct Reader {
    events: Endpoint<Interrupt, In>,
    acl: Endpoint<Bulk, In>,
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
///
/// The completed transfer's own buffer is re-armed rather than a fresh one allocated: an
/// IN completion leaves `requested_len` untouched, so resubmitting it asks for exactly the
/// same rounded, whole-packet length that [`Reader::new`] worked out from the endpoint.
/// That also keeps a zero-copy buffer zero-copy for the life of the transport.
async fn handle_completion<EpType: BulkOrInterrupt>(
    endpoint: &mut Endpoint<EpType, In>,
    completion: Completion,
    what: &'static str,
) -> Result<Read, HciError> {
    let Completion { buffer, status, .. } = completion;
    match status {
        Ok(()) => {
            let data = buffer.to_vec();
            endpoint.submit(buffer);
            Ok(Read::Data(data))
        }
        Err(TransferError::Stall) => {
            warn!(endpoint = what, "endpoint stalled; clearing and re-arming");
            // Sound to clear here precisely because this endpoint carries one transfer at
            // a time: the completion we are holding was the only one in flight, so
            // nothing is pending while the CLEAR_FEATURE goes out.
            //
            // Awaiting this needs nusb's `tokio` feature, and needs it at *runtime*: a
            // `MaybeFuture` awaited without it panics rather than failing to compile, so
            // this line was a live panic on the one path that exists to keep a session
            // alive. See the dependency's comment in Cargo.toml — do not drop that feature.
            endpoint
                .clear_halt()
                .await
                .map_err(|e| HciError::Transport(format!("{what}: clearing stall: {e}")))?;
            endpoint.submit(buffer);
            Ok(Read::Recovered)
        }
        // Everything else really is fatal: the device is gone, or the transfer was
        // cancelled because we are shutting down.
        Err(e) => Err(HciError::Transport(format!("{what}: {e}"))),
    }
}

/// Clear any halt inherited from a previous owner, and arm the endpoint.
///
/// A process that died on a stall leaves the pipe halted, and the next claim then times
/// out during vendor initialisation — which reads as a dead dongle needing a replug rather
/// than as a pipe needing one control transfer.
fn arm<EpType: BulkOrInterrupt>(
    endpoint: &mut Endpoint<EpType, In>,
    want: usize,
    what: &'static str,
) {
    if let Err(e) = endpoint.clear_halt().wait() {
        debug!(endpoint = what, error = %e, "no stall to clear at open");
    }
    let len = whole_packets(want, endpoint.max_packet_size());
    endpoint.submit(endpoint.allocate(len));
}

impl Reader {
    /// Arm both endpoints.
    fn new(interface: &Interface, id: UsbId) -> Result<Self, TransportError> {
        let mut events =
            open_endpoint::<Interrupt, In>(interface, id, EP_EVENT_IN, "interrupt in")?;
        let mut acl = open_endpoint::<Bulk, In>(interface, id, EP_ACL_IN, "bulk in")?;
        arm(&mut events, EVENT_BUF, "interrupt in");
        arm(&mut acl, ACL_BUF, "bulk in");
        Ok(Self {
            events,
            acl,
            queued: std::collections::VecDeque::new(),
        })
    }
}

/// Take exclusive use of one endpoint, saying which one when the device has not got it.
///
/// The addresses are fixed by the Bluetooth USB transport spec, so a device that filtered
/// through [`is_bluetooth`] and then has no endpoint here is misdescribing itself — worth
/// naming precisely, because it is not the failure the claim hint talks about and no
/// amount of unbinding `btusb` will fix it.
fn open_endpoint<EpType: nusb::transfer::EndpointType, Dir: nusb::transfer::EndpointDirection>(
    interface: &Interface,
    id: UsbId,
    address: u8,
    what: &'static str,
) -> Result<Endpoint<EpType, Dir>, TransportError> {
    interface
        .endpoint::<EpType, Dir>(address)
        .map_err(|e| TransportError::Claim {
            id,
            detail: format!("{what} endpoint {address:#04x}: {e}"),
        })
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
    let devices = nusb::list_devices()
        .wait()
        .map_err(|e| TransportError::Io(e.to_string()))?;
    Ok(devices
        .filter(is_bluetooth)
        .map(|info| (UsbId::new(info.vendor_id(), info.product_id()), info))
        .collect())
}

/// Which of the enumerated controllers to open when the config names none.
///
/// The first one a non-catch-all firmware loader claims — a part we can *drive* — and
/// only then the first enumerated (#91). Enumeration order is bus topology, and bus
/// topology once decided that an unknown dongle beat the AX200 next to it: the unknown
/// part "initialised" through the catch-all, answered `HCI_Reset` from whatever it boots
/// into, and the radio sat inert with the driveable controller unopened.
///
/// Pure over the id list, so the two-radio bench (architecture §11.3a-ii) is a fixture
/// here rather than hardware: `list()` supplies the ids at runtime, tests fake them.
fn preferred_index(ids: &[UsbId]) -> Option<usize> {
    ids.iter()
        .position(|id| init::has_dedicated_loader(*id))
        .or(if ids.is_empty() { None } else { Some(0) })
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
    /// Open the preferred Bluetooth controller: one a firmware loader claims if any is
    /// attached, else the first found.
    ///
    /// Enumeration order used to be the whole policy, which mattered on any box with two
    /// radios (#91): an unknown dongle listed before an AX200 won, "initialised" through
    /// the catch-all, and sat inert. `bluetooth.controller = "vendor:product"` still
    /// overrides all of this by naming a device.
    ///
    /// # Errors
    /// [`TransportError::NoDevice`] if none is attached, or [`TransportError::Claim`] if
    /// the OS will not hand it over.
    pub fn open_first() -> Result<Self, TransportError> {
        let devices = list()?;
        let ids: Vec<UsbId> = devices.iter().map(|(id, _)| *id).collect();
        let index = preferred_index(&ids).ok_or(TransportError::NoDevice(None))?;
        if index != 0 {
            if let (Some(preferred), Some(first)) = (ids.get(index), ids.first()) {
                info!(
                    %preferred,
                    skipped = %first,
                    "preferring the controller a firmware loader claims over the first \
                     enumerated"
                );
            }
        }
        let (id, info) = devices
            .into_iter()
            .nth(index)
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
        let device = info.open().wait().map_err(|e| TransportError::Claim {
            id,
            detail: claim_hint(&e.to_string()),
        })?;
        // Interface 0 carries commands, events and ACL. Interface 1 is the isochronous
        // SCO pipe, which an A2DP sink never touches.
        let interface = device
            .claim_interface(0)
            .wait()
            .map_err(|e| TransportError::Claim {
                id,
                detail: claim_hint(&e.to_string()),
            })?;
        info!(%id, "opened bluetooth controller over USB");
        let reader = Reader::new(&interface, id)?;
        let acl_out = open_endpoint::<Bulk, Out>(&interface, id, EP_ACL_OUT, "bulk out")?;
        Ok(Self {
            interface,
            id,
            reader: Mutex::new(reader),
            acl_out: Mutex::new(acl_out),
            bootloader: std::sync::atomic::AtomicBool::new(false),
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
    /// `policy` decides what an unknown controller is: with
    /// [`init::UnknownControllerPolicy::AssumeRom`] the catch-all takes it (loudly, for
    /// anything not on the ROM allow-list); with
    /// [`init::UnknownControllerPolicy::Refuse`] it is a startup error naming the id.
    ///
    /// # Errors
    /// Whatever opening or initialising returns, plus
    /// [`TransportError::UnsupportedController`] under the refuse policy.
    pub async fn open_and_init(
        id: Option<UsbId>,
        firmware: &FirmwareSet,
        policy: init::UnknownControllerPolicy,
    ) -> Result<Self, TransportError> {
        let transport = match id {
            Some(id) => Self::open(id)?,
            None => Self::open_first()?,
        };
        let loader = init::select(init::registry_for(policy), transport.id)?;
        debug!(loader = loader.name(), id = %transport.id, "initialising controller");
        loader.init(transport.id, &transport, firmware).await?;
        Ok(transport)
    }
}

/// Turn a claim failure into something actionable.
///
/// This is the error everyone hits first, and "Access denied" alone sends people looking
/// in the wrong place.
///
/// Both hints name the *command*, not just the condition. Binding a controller away from
/// the OS stack is a once-per-box administrative act, and deliberately not something the
/// receiver does to its own machine: it runs unprivileged, terminates six untrusted
/// protocols and hosts a browser, so giving it driver-install rights would buy one command
/// a human runs once at the cost of a very large blast radius. The least this can do is
/// finish the sentence.
fn claim_hint(detail: &str) -> String {
    let hint = if cfg!(target_os = "linux") {
        "the kernel's btusb driver is probably still bound — unbind it \
         (/sys/bus/usb/drivers/btusb/unbind) and check udev permissions"
    } else {
        "the device must be bound to WinUSB rather than the Microsoft Bluetooth driver — \
         `nix run .#windows-winusb` does that (and `-- --undo` gives it back)"
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

                // The one exception, and only while the bootloader is running: firmware
                // fragments go out on bulk OUT, in the same unprefixed framing.
                if opcode == SECURE_SEND
                    && self.bootloader.load(std::sync::atomic::Ordering::Relaxed)
                {
                    let mut endpoint = self.acl_out.lock().await;
                    endpoint.submit(Buffer::from(body));
                    endpoint
                        .next_complete()
                        .await
                        .into_result()
                        .map_err(|e| HciError::Transport(format!("bulk out: {opcode}: {e}")))?;
                    return Ok(());
                }

                self.interface
                    .control_out(
                        ControlOut {
                            control_type: ControlType::Class,
                            recipient: Recipient::Interface,
                            request: 0x00,
                            value: 0x00,
                            index: 0x00,
                            data: &body,
                        },
                        CONTROL_TIMEOUT,
                    )
                    .await
                    .map_err(|e| control_failure(opcode, &e))?;
                Ok(())
            }
            HciPacket::Acl(acl) => {
                let framed = HciPacket::Acl(acl).encode()?;
                // Skip the indicator byte the encoder writes: the endpoint carries that
                // information on USB.
                let mut endpoint = self.acl_out.lock().await;
                endpoint.submit(Buffer::from(&framed[1..]));
                endpoint
                    .next_complete()
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
                        &mut reader.events, completion, "interrupt in",
                    ).await?;
                    (PacketType::Event, read)
                }
                completion = reader.acl.next_complete() => {
                    let read = handle_completion(
                        &mut reader.acl, completion, "bulk in",
                    ).await?;
                    // While the bootloader runs there is no ACL — there is no link to
                    // carry it — and this endpoint carries the acknowledgement of every
                    // firmware fragment instead. Reading it as ACL is what made a
                    // successful upload look like a timeout.
                    if self.bootloader.load(std::sync::atomic::Ordering::Relaxed) {
                        (PacketType::Event, read)
                    } else {
                        (PacketType::AclData, read)
                    }
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

    fn set_bootloader_framing(&self, on: bool) {
        self.bootloader
            .store(on, std::sync::atomic::Ordering::Relaxed);
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
    fn a_control_timeout_is_reported_as_one_and_names_the_command() {
        // The deploy box spent a session saying "control out: transfer was cancelled",
        // which is nusb's rendering of a *timeout* and says nothing about which command
        // expired — the whole diagnosis of #229 was working out that it was FC09 and not
        // FC05. Both facts have to be in the sentence.
        let expired = control_failure(OpCode::new(0xFC09), &TransferError::Cancelled);
        let text = expired.to_string();
        assert!(
            text.contains("FC09") || text.contains("fc09"),
            "got: {text}"
        );
        assert!(
            !text.contains("cancelled"),
            "a timeout must not read as a cancellation: {text}"
        );
        assert!(
            text.contains('5'),
            "should name the bound it waited: {text}"
        );

        // Anything else keeps what the transport said, still with the opcode attached.
        let stalled = control_failure(OpCode::new(0xFC05), &TransferError::Stall).to_string();
        assert!(
            stalled.contains("FC05") || stalled.contains("fc05"),
            "got: {stalled}"
        );
        assert!(
            stalled.contains("stall") || stalled.contains("STALL"),
            "got: {stalled}"
        );
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
    fn in_transfer_lengths_are_whole_packets_and_never_shrink() {
        // nusb 0.2 fails an IN transfer whose length is not a nonzero multiple of the
        // endpoint's max packet size, so both buffers have to be rounded. Rounding the
        // wrong way would silently give back a buffer smaller than ACL_BUF, and the
        // build-time assertion above cannot see a runtime division — this is what
        // stands in for it.
        for max_packet in [16, 32, 64, 512, 1024] {
            for want in [EVENT_BUF, ACL_BUF] {
                let got = whole_packets(want, max_packet);
                assert_eq!(
                    got % max_packet,
                    0,
                    "{got} is not whole packets of {max_packet}"
                );
                assert!(got >= want, "{got} shrank below the requested {want}");
                assert!(
                    got < want + max_packet,
                    "{got} rounded further than one packet past {want}"
                );
            }
        }
    }

    #[test]
    fn an_exact_multiple_is_left_alone() {
        // The common case on a high-speed bulk endpoint: ACL_BUF is already a multiple of
        // some packet sizes, and growing it anyway would invite a larger transfer than
        // the comment above says we ever invite.
        assert_eq!(whole_packets(1024, 512), 1024);
        assert_eq!(whole_packets(256, 16), 256);
        // A controller reporting nothing must not divide by zero; submit reports it.
        assert_eq!(whole_packets(260, 0), 260);
    }

    /// The two-radio bench of architecture §11.3a-ii, as fixtures (#91). `preferred_index`
    /// is pure over the id list precisely so these run with no USB stack at all — `list()`
    /// itself cannot be faked (nusb owns enumeration), so the seam is the id list.
    mod preference {
        use super::super::preferred_index;
        use crate::init::UsbId;

        /// The AX200 in the dev box: the Intel loader claims it.
        const AX200: UsbId = UsbId::new(0x8087, 0x0029);
        /// A TP-Link UB500: the Realtek loader claims it.
        const RTL8761BU: UsbId = UsbId::new(0x0bda, 0x8771);
        /// A CSR8510 clone: works, but from ROM — no loader claims it.
        const CSR8510: UsbId = UsbId::new(0x0a12, 0x0001);
        /// An MT7921-era MediaTek dongle: no loader, and not on the ROM allow-list.
        const UNKNOWN: UsbId = UsbId::new(0x0e8d, 0x0616);

        #[test]
        fn a_driveable_controller_beats_enumeration_order() {
            // The defect the issue names: the unknown dongle enumerated first, won, and
            // sat inert while the AX200 next to it went unopened.
            assert_eq!(preferred_index(&[UNKNOWN, AX200]), Some(1));
            assert_eq!(preferred_index(&[UNKNOWN, CSR8510, RTL8761BU]), Some(2));
        }

        #[test]
        fn among_driveable_controllers_enumeration_order_still_decides() {
            // Preference is not a ranking of vendors; the first part we can drive wins,
            // and `bluetooth.controller` is the override for anything finer.
            assert_eq!(preferred_index(&[RTL8761BU, AX200]), Some(0));
            assert_eq!(preferred_index(&[AX200, RTL8761BU]), Some(0));
        }

        #[test]
        fn with_nothing_driveable_the_first_enumerated_is_the_fallback() {
            // Exactly the old behaviour: a ROM-based part or an unknown dongle is still
            // opened rather than refused — refusal is the config's job, not this one's.
            assert_eq!(preferred_index(&[CSR8510]), Some(0));
            assert_eq!(preferred_index(&[UNKNOWN, CSR8510]), Some(0));
        }

        #[test]
        fn no_devices_is_no_answer_not_index_zero() {
            assert_eq!(preferred_index(&[]), None);
        }
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
            // The Nix build sandbox has no /sys/bus/usb at all, so enumeration cannot
            // even start there. That is a property of the sandbox, not of the code —
            // on any real box (and the plain-cargo CI path) sysfs exists and the
            // Ok arm is the one exercised.
            Err(e) if !std::path::Path::new("/sys/bus/usb").exists() => {
                eprintln!("skipped: no /sys/bus/usb in this environment ({e})");
            }
            Err(e) => panic!("enumeration should succeed even with no devices: {e}"),
        }
    }

    #[tokio::test]
    async fn a_nusb_blocking_call_can_be_awaited_from_the_runtime() {
        // Guards a live panic, not a style preference. nusb 0.2 models its blocking
        // syscalls as `MaybeFuture`: `.wait()` runs one synchronously, `.await` hands it
        // to `spawn_blocking` — but only if the crate was built with its `tokio` feature.
        // Without it, awaiting compiles fine and **panics at runtime**:
        //
        //   Awaiting blocking syscall without an async runtime: enable the `smol` or
        //   `tokio` feature of nusb.
        //
        // The one place this transport awaits one is the endpoint stall recovery in
        // `handle_completion`, so the panic sat on the single path that exists to keep a
        // session alive through a stall — and a controller duly stalled mid-session and
        // took the receiver down with it at exactly the moment it should have recovered.
        //
        // `list_devices` is awaited here rather than `clear_halt` because it is the same
        // `MaybeFuture` machinery with no hardware attached, so this runs in CI.
        let _ = nusb::list_devices()
            .await
            .map(std::iter::Iterator::count)
            .unwrap_or(0);
    }
}
