//! Realtek controllers: the RTL8761BU download flow.
//!
//! Reference is the kernel's `btrtl.c`. Much simpler than Intel's — no secure boot, no
//! signature — but with its own trap: the download index byte carries a *wrap-around
//! counter* in its low seven bits and an end-of-transfer flag in the top one, and a
//! controller that never sees the flag waits forever for more data.

use substrate_hci::{Command, HciPacket, HciTransport, OpCode};
use tracing::{debug, info};

use crate::error::TransportError;
use crate::firmware::FirmwareSet;
use crate::init::{ControllerInit, UsbId};

/// Read the ROM version, which selects which patch in the image applies.
const READ_ROM_VERSION: OpCode = OpCode::new(0xFC6D);
/// Download one firmware fragment.
const DOWNLOAD: OpCode = OpCode::new(0xFC20);

/// Largest payload per download command: the parameter length field is one byte and the
/// index consumes one of them.
const MAX_FRAGMENT: usize = 252;

/// The index counter wraps at seven bits; the eighth is the end flag.
const INDEX_MASK: u8 = 0x7F;
const END_FLAG: u8 = 0x80;

/// Signature at the head of a Realtek patch image.
const EPATCH_SIGNATURE: &[u8; 8] = b"Realtech";
/// Signature of the newer container format used by RTL8761BU and later.
const RTL_EPATCH_SIGNATURE_V2: &[u8; 8] = b"RTBTCore";

/// Realtek firmware loader.
#[derive(Debug, Clone)]
pub struct RealtekInit {
    firmware_name: &'static str,
    config_name: &'static str,
}

impl Default for RealtekInit {
    fn default() -> Self {
        Self {
            firmware_name: "rtl_bt/rtl8761bu_fw.bin",
            config_name: "rtl_bt/rtl8761bu_config.bin",
        }
    }
}

impl RealtekInit {
    /// Realtek's USB vendor id.
    pub const VENDOR: u16 = 0x0BDA;

    /// Products this loader handles. `0x8771` is the RTL8761BU in every cheap dongle;
    /// `0x8761` is the older RTL8761B.
    pub const PRODUCTS: &'static [u16] = &[0x8771, 0x8761, 0xa725, 0xb00a];
}

#[async_trait::async_trait]
impl ControllerInit for RealtekInit {
    fn name(&self) -> &'static str {
        "realtek"
    }

    fn matches(&self, id: UsbId) -> bool {
        id.vendor == Self::VENDOR && Self::PRODUCTS.contains(&id.product)
    }

    fn required_images(&self) -> &'static [&'static str] {
        &["rtl_bt/rtl8761bu_fw.bin", "rtl_bt/rtl8761bu_config.bin"]
    }

    async fn init(
        &self,
        hci: &dyn HciTransport,
        firmware: &FirmwareSet,
    ) -> Result<(), TransportError> {
        let rom_version = read_rom_version(hci).await?;
        debug!(rom_version, "realtek rom version");

        let fw = firmware.get(self.firmware_name)?;
        check_signature(&fw, self.firmware_name)?;

        // The config is appended to the firmware, not sent separately. It is optional —
        // a controller without one uses its defaults — so a missing file is a debug line
        // rather than a refusal to start.
        let mut payload = fw.to_vec();
        match firmware.get(self.config_name) {
            Ok(config) => payload.extend_from_slice(&config),
            Err(e) => debug!(error = %e, "realtek: no config blob; using controller defaults"),
        }

        download(hci, &payload).await?;
        info!(bytes = payload.len(), "realtek firmware loaded");
        Ok(())
    }
}

/// Read the ROM version byte.
async fn read_rom_version(hci: &dyn HciTransport) -> Result<u8, TransportError> {
    let params = send(
        hci,
        Command::Vendor {
            opcode: READ_ROM_VERSION,
            params: bytes::Bytes::new(),
        },
    )
    .await?;
    Ok(params.first().copied().unwrap_or(0))
}

/// Reject an image that is not a Realtek patch container.
///
/// Downloading an arbitrary file succeeds command-by-command and leaves the controller
/// wedged, so checking the magic up front turns a mystery into a message.
fn check_signature(fw: &[u8], name: &str) -> Result<(), TransportError> {
    let head = fw.get(..8).ok_or_else(|| TransportError::Firmware {
        name: name.to_owned(),
        detail: "shorter than its 8-byte signature".to_owned(),
    })?;
    if head == EPATCH_SIGNATURE || head == RTL_EPATCH_SIGNATURE_V2 {
        return Ok(());
    }
    Err(TransportError::Firmware {
        name: name.to_owned(),
        detail: format!(
            "not a Realtek patch image (signature {head:02x?}; expected \"Realtech\" or \"RTBTCore\")"
        ),
    })
}

/// The index byte for fragment `n`, with the end flag set on the last one.
///
/// The counter is seven bits and wraps — a 700 kB image is nearly 3000 fragments, so
/// wrapping is the normal case rather than an edge one. Masking wrong makes the
/// controller reassemble in the wrong order, which produces a part that accepts the
/// upload and then does not work.
#[must_use]
pub const fn index_byte(n: usize, last: bool) -> u8 {
    #[allow(clippy::cast_possible_truncation)]
    let counter = (n as u8) & INDEX_MASK;
    if last {
        counter | END_FLAG
    } else {
        counter
    }
}

/// Upload the image.
async fn download(hci: &dyn HciTransport, payload: &[u8]) -> Result<(), TransportError> {
    let chunks: Vec<&[u8]> = payload.chunks(MAX_FRAGMENT).collect();
    let last = chunks.len().saturating_sub(1);
    for (n, chunk) in chunks.iter().enumerate() {
        let mut params = Vec::with_capacity(1 + chunk.len());
        params.push(index_byte(n, n == last));
        params.extend_from_slice(chunk);
        send(
            hci,
            Command::Vendor {
                opcode: DOWNLOAD,
                params: bytes::Bytes::from(params),
            },
        )
        .await?;
    }
    Ok(())
}

/// How long to wait for a controller to answer. See the note in `intel.rs`: bounding
/// iterations without bounding time leaves a wedged part hanging on the first pass.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Send a command and wait for its completion.
async fn send(hci: &dyn HciTransport, command: Command) -> Result<Vec<u8>, TransportError> {
    let opcode = command.opcode();
    hci.send(command.encode()?).await?;
    for _ in 0..64 {
        let packet = tokio::time::timeout(COMMAND_TIMEOUT, hci.recv())
            .await
            .map_err(|_| TransportError::Timeout("realtek command completion"))??;
        let HciPacket::Event { code, params } = packet else {
            continue;
        };
        let event = substrate_hci::Event::parse(code, &params)?;
        if let substrate_hci::Event::CommandComplete {
            opcode: got,
            params,
            ..
        } = event
        {
            if got != opcode {
                continue;
            }
            let mut rest = params.to_vec();
            if rest.is_empty() {
                return Ok(rest);
            }
            let status = rest.remove(0);
            if status != 0 {
                return Err(TransportError::Controller {
                    what: "realtek command",
                    detail: format!("{opcode} returned status {status:#04x}"),
                });
            }
            return Ok(rest);
        }
    }
    Err(TransportError::Timeout("realtek command completion"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use substrate_hci::{event::code, ScriptedTransport};

    use super::*;
    use crate::firmware::Firmware;

    fn controller() -> ScriptedTransport {
        ScriptedTransport::new().with_responder(|sent| {
            let HciPacket::Command { opcode, .. } = sent else {
                return Vec::new();
            };
            let mut params = vec![0x01];
            params.extend_from_slice(&opcode.raw().to_le_bytes());
            params.push(0x00); // status
            if *opcode == READ_ROM_VERSION {
                params.push(0x0A);
            }
            vec![HciPacket::Event {
                code: code::COMMAND_COMPLETE,
                params: bytes::Bytes::from(params),
            }]
        })
    }

    fn image(len: usize) -> Vec<u8> {
        let mut fw = RTL_EPATCH_SIGNATURE_V2.to_vec();
        fw.resize(len.max(8), 0x5A);
        fw
    }

    fn firmware_with(fw: Vec<u8>) -> FirmwareSet {
        FirmwareSet::new().with(
            "rtl_bt/rtl8761bu_fw.bin",
            Firmware::Embedded(Box::leak(fw.into_boxed_slice())),
        )
    }

    /// The index bytes of every download command sent.
    fn indices(transport: &ScriptedTransport) -> Vec<u8> {
        transport
            .sent()
            .iter()
            .filter_map(|p| match p {
                HciPacket::Command { opcode, params } if *opcode == DOWNLOAD => {
                    params.first().copied()
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_index_counter_wraps_at_seven_bits_and_flags_the_last_fragment() {
        // A 700 kB image is ~2800 fragments, so wrapping is the normal case. Getting the
        // mask wrong makes the controller reassemble out of order — it accepts the whole
        // upload and then simply does not work.
        assert_eq!(index_byte(0, false), 0x00);
        assert_eq!(index_byte(127, false), 0x7F);
        assert_eq!(index_byte(128, false), 0x00, "counter wraps");
        assert_eq!(index_byte(129, false), 0x01);
        assert_eq!(index_byte(3, true), 0x83, "end flag on the last fragment");
        assert_eq!(index_byte(128, true), 0x80);
    }

    #[tokio::test]
    async fn a_download_ends_with_exactly_one_flagged_fragment() {
        // A controller that never sees the end flag waits forever for more data; two
        // flags would end the transfer early with a truncated image.
        let transport = controller();
        RealtekInit::default()
            .init(&transport, &firmware_with(image(MAX_FRAGMENT * 3 + 7)))
            .await
            .unwrap();

        let indices = indices(&transport);
        assert_eq!(indices.len(), 4, "three full fragments and a tail");
        assert_eq!(
            indices.iter().filter(|i| *i & END_FLAG != 0).count(),
            1,
            "exactly one end flag"
        );
        assert!(indices.last().unwrap() & END_FLAG != 0, "and it is last");
    }

    #[tokio::test]
    async fn a_single_fragment_image_is_still_flagged() {
        // The off-by-one that leaves a small image hanging: with one chunk, index 0 is
        // also the last one.
        let transport = controller();
        RealtekInit::default()
            .init(&transport, &firmware_with(image(64)))
            .await
            .unwrap();
        assert_eq!(indices(&transport), vec![0x80]);
    }

    #[tokio::test]
    async fn indices_wrap_correctly_across_a_long_image() {
        let transport = controller();
        // 130 fragments, so the counter goes past 127 and starts again.
        RealtekInit::default()
            .init(&transport, &firmware_with(image(MAX_FRAGMENT * 130)))
            .await
            .unwrap();
        let indices = indices(&transport);
        assert_eq!(indices.len(), 130);
        assert_eq!(indices[127], 0x7F);
        assert_eq!(indices[128], 0x00, "wrapped");
        assert_eq!(indices[129], 0x01 | END_FLAG);
    }

    #[tokio::test]
    async fn fragments_fit_the_single_byte_parameter_length() {
        let transport = controller();
        RealtekInit::default()
            .init(&transport, &firmware_with(image(MAX_FRAGMENT * 2)))
            .await
            .unwrap();
        for packet in transport.sent() {
            if let HciPacket::Command { opcode, params } = packet {
                if opcode == DOWNLOAD {
                    assert!(params.len() <= MAX_FRAGMENT + 1);
                }
            }
        }
    }

    #[test]
    fn an_image_that_is_not_a_realtek_patch_is_refused_up_front() {
        // Downloading an arbitrary file succeeds command-by-command and leaves the
        // controller wedged, so the magic is checked before anything is sent.
        let err = check_signature(b"NOT-A-PATCH-FILE", "rtl_bt/x.bin").unwrap_err();
        assert!(format!("{err}").contains("Realtech"), "got: {err}");
        assert!(check_signature(EPATCH_SIGNATURE, "x").is_ok());
        assert!(check_signature(RTL_EPATCH_SIGNATURE_V2, "x").is_ok());
    }

    #[test]
    fn a_short_image_is_refused_before_the_signature_check_can_index_past_it() {
        assert!(check_signature(&[0u8; 4], "x").is_err());
    }

    #[tokio::test]
    async fn a_missing_image_fails_before_anything_is_downloaded() {
        let transport = controller();
        let err = RealtekInit::default()
            .init(&transport, &FirmwareSet::new())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("rtl8761bu_fw.bin"), "got: {err}");
        assert!(indices(&transport).is_empty());
    }

    #[tokio::test]
    async fn the_rom_version_is_read_before_the_download() {
        // It selects which patch applies; reading it after would be pointless.
        let transport = controller();
        RealtekInit::default()
            .init(&transport, &firmware_with(image(64)))
            .await
            .unwrap();
        let opcodes = transport.sent_commands();
        let version = opcodes.iter().position(|o| *o == READ_ROM_VERSION).unwrap();
        let first_download = opcodes.iter().position(|o| *o == DOWNLOAD).unwrap();
        assert!(version < first_download);
    }

    #[test]
    fn the_loader_claims_only_the_realtek_parts_it_knows() {
        let rtl = RealtekInit::default();
        assert!(rtl.matches(UsbId::new(0x0bda, 0x8771)), "RTL8761BU");
        assert!(!rtl.matches(UsbId::new(0x0bda, 0x0001)));
        assert!(!rtl.matches(UsbId::new(0x8087, 0x0029)));
    }
}
