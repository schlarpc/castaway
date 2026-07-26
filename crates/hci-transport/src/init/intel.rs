//! Intel controllers: the AX200/AX201 secure-boot firmware flow.
//!
//! Reference is the kernel's `btintel.c`, which is the specification here — and also the
//! oracle: `btmon` capturing the kernel bring-up the same radio gives a transcript this
//! sequence can be diffed against (architecture §11.3a).
//!
//! Note the loader can only be exercised against a controller the kernel has *not*
//! already initialised. `HCI_CHANNEL_USER` hands over a part that is already operational,
//! so testing this means unbinding `btusb` and claiming through USB — which is what
//! Windows does anyway.

use substrate_hci::{Command, HciPacket, HciTransport, OpCode};
use tracing::{debug, info};

use crate::error::TransportError;
use crate::firmware::FirmwareSet;
use crate::init::{ControllerInit, UsbId};

/// Read version, in TLV form on AX2xx.
const READ_VERSION: OpCode = OpCode::new(0xFC05);
/// Send a firmware fragment.
const SECURE_SEND: OpCode = OpCode::new(0xFC09);
/// Reset into the freshly loaded operational image.
const INTEL_RESET: OpCode = OpCode::new(0xFC01);
/// Push a DDC configuration entry.
const LOAD_DDC: OpCode = OpCode::new(0xFC8B);

/// The parameter that asks for a TLV-encoded version response rather than the legacy
/// fixed struct. AX2xx only answers the TLV form.
const READ_VERSION_TLV: u8 = 0xFF;

/// TLV type carrying which image the controller is currently running.
const TLV_IMAGE_TYPE: u8 = 0x1C;

/// Image type values in a *TLV* response.
mod tlv_image {
    /// Running the bootloader: firmware is needed.
    pub const BOOTLOADER: u8 = 0x01;
    /// Running operational firmware already: nothing to do.
    pub const OPERATIONAL: u8 = 0x03;
}

/// `fw_variant` values in a *legacy* response, which mean the same thing with different
/// numbers. Two encodings for one fact is exactly the sort of thing that gets assumed
/// away.
mod legacy_variant {
    /// Bootloader.
    pub const BOOTLOADER: u8 = 0x06;
    /// Operational firmware.
    pub const OPERATIONAL: u8 = 0x23;
}

/// Length of the legacy fixed-struct version response, once the status byte is stripped.
const LEGACY_VERSION_LEN: usize = 9;
/// Offset of `fw_variant` within it.
const LEGACY_FW_VARIANT: usize = 3;

/// Which image a controller is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunningImage {
    /// In the bootloader, waiting for firmware.
    Bootloader,
    /// Already running operational firmware; there is nothing to upload.
    Operational,
    /// The response was a shape we do not recognise.
    Unknown,
}

/// Secure-send fragment types, in the order `btintel` sends them.
mod fragment {
    /// The 128-byte CSS header that opens the transaction.
    pub const INIT: u8 = 0x00;
    /// Firmware command/data payload.
    pub const DATA: u8 = 0x01;
    /// The 256-byte signature.
    pub const SIGNATURE: u8 = 0x02;
    /// The 256-byte public key.
    pub const PUBLIC_KEY: u8 = 0x03;
}

/// Layout of the `.sfi` header, before the command/data blocks begin.
const CSS_HEADER_LEN: usize = 128;
const PUBLIC_KEY_LEN: usize = 256;
const SIGNATURE_LEN: usize = 256;
const SFI_HEADER_LEN: usize = CSS_HEADER_LEN + PUBLIC_KEY_LEN + SIGNATURE_LEN;

/// Largest payload one `Secure_Send` carries. The opcode's parameter field is a single
/// byte, and the fragment type consumes one of them.
const MAX_FRAGMENT: usize = 252;

/// Intel firmware loader.
#[derive(Debug, Clone)]
pub struct IntelInit {
    /// Which firmware image to use. AX200 and AX201 both take `ibt-20-1-3`.
    image_stem: &'static str,
}

impl Default for IntelInit {
    fn default() -> Self {
        Self {
            image_stem: "intel/ibt-20-1-3",
        }
    }
}

impl IntelInit {
    /// Intel's USB vendor id.
    pub const VENDOR: u16 = 0x8087;

    /// Products this loader handles.
    ///
    /// AX200 is `0x0029`. The others are the same generation and take the same image;
    /// anything Intel outside this list gets a clear "no loader" rather than a wrong one.
    pub const PRODUCTS: &'static [u16] = &[
        0x0029, // AX200
        0x0026, // AX201
        0x0032, // AX210
        0x0033, // AX211
    ];
}

#[async_trait::async_trait]
impl ControllerInit for IntelInit {
    fn name(&self) -> &'static str {
        "intel"
    }

    fn matches(&self, id: UsbId) -> bool {
        id.vendor == Self::VENDOR && Self::PRODUCTS.contains(&id.product)
    }

    fn required_images(&self) -> &'static [&'static str] {
        &["intel/ibt-20-1-3.sfi", "intel/ibt-20-1-3.ddc"]
    }

    async fn init(
        &self,
        hci: &dyn HciTransport,
        firmware: &FirmwareSet,
    ) -> Result<(), TransportError> {
        let version = read_version(hci).await?;
        debug!(tlv = ?hex(&version), "intel version response");

        match running_image(&version) {
            RunningImage::Operational => {
                // A warm reboot — or a kernel that already initialised the part before we
                // took it — leaves operational firmware in place. Uploading again is not
                // merely unnecessary: the controller refuses every Secure_Send with
                // "command disallowed", which is how this was discovered.
                info!("intel controller already running operational firmware");
                return Ok(());
            }
            RunningImage::Bootloader => {}
            RunningImage::Unknown => {
                return Err(TransportError::Controller {
                    what: "intel read_version",
                    detail: format!("unrecognised version response: {}", hex(&version)),
                })
            }
        }

        let sfi_name = format!("{}.sfi", self.image_stem);
        let sfi = firmware.get(&sfi_name)?;
        download_firmware(hci, &sfi, &sfi_name).await?;

        // Reset into the image just uploaded. The parameters are `btintel`'s verbatim:
        // reset type, patch enable, ddc reload, boot option, boot address.
        send(
            hci,
            Command::Vendor {
                opcode: INTEL_RESET,
                params: bytes::Bytes::from_static(&[
                    0x00, 0x01, 0x00, 0x01, 0x00, 0x08, 0x04, 0x00,
                ]),
            },
        )
        .await?;

        // DDC is the per-board tuning table. Missing it is not fatal — the radio works,
        // just not to spec for this antenna layout — so a build without the file logs
        // and continues rather than refusing to start.
        let ddc_name = format!("{}.ddc", self.image_stem);
        match firmware.get(&ddc_name) {
            Ok(ddc) => load_ddc(hci, &ddc).await?,
            Err(e) => debug!(error = %e, "intel: no DDC config; using controller defaults"),
        }

        info!("intel firmware loaded");
        Ok(())
    }
}

/// Send a command and wait for its completion, returning the return parameters.
async fn send(hci: &dyn HciTransport, command: Command) -> Result<Vec<u8>, TransportError> {
    let opcode = command.opcode();
    hci.send(command.encode()?).await?;
    wait_for_complete(hci, opcode).await
}

/// How long to wait for a controller to answer one command.
///
/// A bound on *iterations* is worthless without one on time: `recv` blocks until
/// something arrives, so a wedged controller that answers nothing would hang the loop on
/// its first pass rather than spinning through it. Firmware upload has no other failure
/// mode, and hanging is the worst one — it looks like the loader is working.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Wait for the command-complete matching `opcode`.
///
/// Vendor events and unrelated completions are skipped rather than treated as the
/// answer: a controller mid-boot emits plenty of both.
async fn wait_for_complete(
    hci: &dyn HciTransport,
    opcode: OpCode,
) -> Result<Vec<u8>, TransportError> {
    for _ in 0..64 {
        let packet = tokio::time::timeout(COMMAND_TIMEOUT, hci.recv())
            .await
            .map_err(|_| TransportError::Timeout("intel command completion"))??;
        let HciPacket::Event { code, params } = packet else {
            continue;
        };
        let event = substrate_hci::Event::parse(code, &params)?;
        match event {
            substrate_hci::Event::CommandComplete {
                opcode: got,
                params,
                ..
            } if got == opcode => {
                let mut rest = params.to_vec();
                if rest.is_empty() {
                    return Ok(rest);
                }
                let status = rest.remove(0);
                if status != 0 {
                    return Err(TransportError::Controller {
                        what: "intel command",
                        detail: format!("{opcode} returned status {status:#04x}"),
                    });
                }
                return Ok(rest);
            }
            substrate_hci::Event::CommandStatus {
                opcode: got,
                status,
                ..
            } if got == opcode && !status.is_success() => {
                return Err(TransportError::Controller {
                    what: "intel command",
                    detail: format!("{opcode} returned {status}"),
                })
            }
            _ => continue,
        }
    }
    Err(TransportError::Timeout("intel command completion"))
}

/// Read the TLV version block.
async fn read_version(hci: &dyn HciTransport) -> Result<Vec<u8>, TransportError> {
    send(
        hci,
        Command::Vendor {
            opcode: READ_VERSION,
            params: bytes::Bytes::from_static(&[READ_VERSION_TLV]),
        },
    )
    .await
}

/// Hex for logging a raw response.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Work out which image a controller is running, from either response shape.
///
/// **Both shapes exist and this was found the hard way.** `Read_Version` with the `0xFF`
/// parameter is supposed to return a TLV list, but an AX200 already running operational
/// firmware answers the *legacy* fixed struct and ignores the parameter — nine bytes
/// beginning `37 14`. Parsing that as TLVs finds no image-type entry, concludes
/// "bootloader", and cheerfully tries to upload firmware to a part that then refuses
/// every `Secure_Send` with "command disallowed".
///
/// The two encodings disagree on the numbers as well as the layout: a TLV says `0x03`
/// for operational, the legacy struct says `0x23`.
#[must_use]
pub fn running_image(response: &[u8]) -> RunningImage {
    // The legacy struct is a fixed nine bytes and starts with the Intel hardware
    // platform id, which is always 0x37. A TLV list starts with a type byte, and no
    // type we care about is 0x37 — so the two are told apart without guessing.
    if response.len() == LEGACY_VERSION_LEN && response.first() == Some(&0x37) {
        return match response.get(LEGACY_FW_VARIANT) {
            Some(&legacy_variant::BOOTLOADER) => RunningImage::Bootloader,
            Some(&legacy_variant::OPERATIONAL) => RunningImage::Operational,
            _ => RunningImage::Unknown,
        };
    }

    let mut rest = response;
    while rest.len() >= 2 {
        let kind = rest[0];
        let len = usize::from(rest[1]);
        let Some(value) = rest.get(2..2 + len) else {
            break;
        };
        if kind == TLV_IMAGE_TYPE {
            return match value.first() {
                Some(&tlv_image::BOOTLOADER) => RunningImage::Bootloader,
                Some(&tlv_image::OPERATIONAL) => RunningImage::Operational,
                _ => RunningImage::Unknown,
            };
        }
        rest = &rest[2 + len..];
    }
    RunningImage::Unknown
}

/// The four transfers a `.sfi` is split into, in the order secure boot requires them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SfiParts<'a> {
    /// The 128-byte CSS header that opens the transaction.
    pub css: &'a [u8],
    /// The 256-byte public key.
    pub public_key: &'a [u8],
    /// The 256-byte signature.
    pub signature: &'a [u8],
    /// The command/data payload that follows.
    pub blocks: &'a [u8],
}

/// Split a `.sfi` into the four transfers `btintel` performs.
///
/// # Errors
/// [`TransportError::Firmware`] if the image is shorter than its fixed header.
pub fn split_sfi<'a>(sfi: &'a [u8], name: &str) -> Result<SfiParts<'a>, TransportError> {
    if sfi.len() <= SFI_HEADER_LEN {
        return Err(TransportError::Firmware {
            name: name.to_owned(),
            detail: format!(
                "image is {} bytes; the CSS header, key and signature alone need {}",
                sfi.len(),
                SFI_HEADER_LEN
            ),
        });
    }
    let (css, rest) = sfi.split_at(CSS_HEADER_LEN);
    let (public_key, rest) = rest.split_at(PUBLIC_KEY_LEN);
    let (signature, blocks) = rest.split_at(SIGNATURE_LEN);
    Ok(SfiParts {
        css,
        public_key,
        signature,
        blocks,
    })
}

/// Upload a firmware image.
async fn download_firmware(
    hci: &dyn HciTransport,
    sfi: &[u8],
    name: &str,
) -> Result<(), TransportError> {
    let parts = split_sfi(sfi, name)?;

    // Order is fixed by the secure-boot protocol: the CSS header opens the transaction,
    // then the key, then the signature, and only then the payload. The controller
    // rejects anything out of order, which is the good case — the bad case is a part
    // that accepts a partial upload and boots into an image that half-works.
    secure_send(hci, fragment::INIT, parts.css).await?;
    secure_send(hci, fragment::PUBLIC_KEY, parts.public_key).await?;
    secure_send(hci, fragment::SIGNATURE, parts.signature).await?;

    for block in split_command_blocks(parts.blocks) {
        secure_send(hci, fragment::DATA, block).await?;
    }
    Ok(())
}

/// Split the payload into HCI-command-shaped blocks.
///
/// The tail of a `.sfi` is a sequence of HCI commands — 3-byte header (opcode, then a
/// one-byte parameter length) followed by that many parameter bytes. Fragmenting on any
/// other boundary hands the controller a half command, which it rejects.
#[must_use]
pub fn split_command_blocks(mut data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while data.len() >= 3 {
        let params = usize::from(data[2]);
        let total = 3 + params;
        if data.len() < total {
            break;
        }
        out.push(&data[..total]);
        data = &data[total..];
    }
    out
}

/// Send one fragment, split to the command parameter limit.
async fn secure_send(
    hci: &dyn HciTransport,
    kind: u8,
    payload: &[u8],
) -> Result<(), TransportError> {
    for chunk in payload.chunks(MAX_FRAGMENT) {
        let mut params = Vec::with_capacity(1 + chunk.len());
        params.push(kind);
        params.extend_from_slice(chunk);
        send(
            hci,
            Command::Vendor {
                opcode: SECURE_SEND,
                params: bytes::Bytes::from(params),
            },
        )
        .await?;
    }
    Ok(())
}

/// Push the DDC configuration, one length-prefixed entry at a time.
async fn load_ddc(hci: &dyn HciTransport, ddc: &[u8]) -> Result<(), TransportError> {
    let mut rest = ddc;
    while !rest.is_empty() {
        // Each entry is a length byte that does *not* count itself, followed by that
        // many bytes. Sending the whole file in one command is rejected.
        let len = usize::from(rest[0]);
        let Some(entry) = rest.get(..=len) else { break };
        send(
            hci,
            Command::Vendor {
                opcode: LOAD_DDC,
                params: bytes::Bytes::copy_from_slice(entry),
            },
        )
        .await?;
        rest = &rest[len + 1..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use substrate_hci::{event::code, ScriptedTransport};

    use super::*;
    use crate::firmware::Firmware;

    /// A controller that completes every command it is sent.
    fn controller(version_tlv: Vec<u8>) -> ScriptedTransport {
        ScriptedTransport::new().with_responder(move |sent| {
            let HciPacket::Command { opcode, .. } = sent else {
                return Vec::new();
            };
            let mut params = vec![0x01];
            params.extend_from_slice(&opcode.raw().to_le_bytes());
            params.push(0x00); // status: success
            if *opcode == READ_VERSION {
                params.extend_from_slice(&version_tlv);
            }
            vec![HciPacket::Event {
                code: code::COMMAND_COMPLETE,
                params: bytes::Bytes::from(params),
            }]
        })
    }

    /// The **real** `Read_Version` response from the AX200 in this dev box, captured
    /// 2026-07-25 while it was running operational firmware. Nine bytes, legacy layout,
    /// despite the request carrying the 0xFF parameter that asks for TLVs (ground rule 6:
    /// land the finding as a fixture rather than a memory).
    const AX200_OPERATIONAL: [u8; 9] = [0x37, 0x14, 0x00, 0x23, 0x00, 0xfa, 0x11, 0x14, 0x00];

    /// A TLV block reporting `image`.
    fn version_tlv(image: u8) -> Vec<u8> {
        vec![
            0x01,
            0x02,
            0xAA,
            0xBB, // some other TLV first, so offsets are not fixed
            TLV_IMAGE_TYPE,
            0x01,
            image,
        ]
    }

    /// A `.sfi` with `blocks` worth of command payload after the fixed header.
    fn sfi(blocks: &[u8]) -> Vec<u8> {
        let mut image = vec![0xAA; CSS_HEADER_LEN];
        image.extend(std::iter::repeat_n(0xBB, PUBLIC_KEY_LEN));
        image.extend(std::iter::repeat_n(0xCC, SIGNATURE_LEN));
        image.extend_from_slice(blocks);
        image
    }

    /// One HCI-command-shaped block with `n` parameter bytes.
    fn command_block(n: u8) -> Vec<u8> {
        let mut b = vec![0x09, 0xFC, n];
        b.extend(std::iter::repeat_n(0xEE, usize::from(n)));
        b
    }

    fn firmware_with(sfi_bytes: Vec<u8>) -> FirmwareSet {
        FirmwareSet::new().with(
            "intel/ibt-20-1-3.sfi",
            Firmware::File(write_temp("ibt.sfi", &sfi_bytes)),
        )
    }

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("castaway-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_real_ax200_answers_the_legacy_layout_not_tlvs() {
        // Found on hardware, not by reading: an AX200 already running operational
        // firmware ignores the 0xFF parameter and returns the nine-byte legacy struct.
        // Parsing that as TLVs finds no image-type entry, concludes "bootloader", and
        // uploads firmware to a part that refuses every Secure_Send with "command
        // disallowed" — which is precisely what happened.
        assert_eq!(running_image(&AX200_OPERATIONAL), RunningImage::Operational);
    }

    #[test]
    fn the_legacy_and_tlv_encodings_disagree_on_the_numbers_too() {
        // Operational is 0x23 in the legacy struct and 0x03 in a TLV. Sharing one
        // constant between them would make one of the two silently wrong.
        let mut legacy_bootloader = AX200_OPERATIONAL;
        legacy_bootloader[LEGACY_FW_VARIANT] = legacy_variant::BOOTLOADER;
        assert_eq!(running_image(&legacy_bootloader), RunningImage::Bootloader);

        assert_eq!(
            running_image(&version_tlv(tlv_image::OPERATIONAL)),
            RunningImage::Operational
        );
        assert_eq!(
            running_image(&version_tlv(tlv_image::BOOTLOADER)),
            RunningImage::Bootloader
        );
    }

    #[test]
    fn the_image_type_tlv_is_found_by_walking_not_by_offset() {
        // TLV contents vary by part, so a fixed offset reads a different field on the
        // next generation — which would silently mean "already operational" and skip
        // the upload entirely.
        assert_eq!(
            running_image(&version_tlv(tlv_image::BOOTLOADER)),
            RunningImage::Bootloader
        );
        assert_eq!(running_image(&[]), RunningImage::Unknown);
        assert_eq!(
            running_image(&[0x01, 0x02, 0xAA, 0xBB]),
            RunningImage::Unknown
        );
    }

    #[test]
    fn an_unrecognised_response_is_refused_rather_than_assumed_to_be_a_bootloader() {
        // The original bug in one line: treating "I could not tell" as "needs firmware"
        // is what turned a readable state into a wedged upload.
        assert_eq!(
            running_image(&[0xDE, 0xAD, 0xBE, 0xEF]),
            RunningImage::Unknown
        );
    }

    #[test]
    fn a_truncated_tlv_does_not_panic() {
        assert_eq!(
            running_image(&[TLV_IMAGE_TYPE, 0x04, 0x01]),
            RunningImage::Unknown
        );
    }

    #[test]
    fn an_sfi_splits_into_header_key_signature_and_payload() {
        let image = sfi(&command_block(4));
        let parts = split_sfi(&image, "test").unwrap();
        assert_eq!(parts.css.len(), 128);
        assert_eq!(parts.public_key.len(), 256);
        assert_eq!(parts.signature.len(), 256);
        assert_eq!(parts.blocks.len(), 7);
        assert!(parts.css.iter().all(|b| *b == 0xAA));
        assert!(parts.public_key.iter().all(|b| *b == 0xBB));
        assert!(parts.signature.iter().all(|b| *b == 0xCC));
    }

    #[test]
    fn a_short_image_is_refused_with_the_size_it_needed() {
        let err = split_sfi(&[0u8; 100], "intel/ibt-20-1-3.sfi").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("640"), "should name the header size: {msg}");
    }

    #[test]
    fn the_payload_is_split_on_hci_command_boundaries() {
        // Fragmenting anywhere else hands the controller half a command. Chunking by a
        // fixed size would do exactly that.
        let mut payload = command_block(4);
        payload.extend(command_block(0));
        payload.extend(command_block(10));
        let blocks = split_command_blocks(&payload);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].len(), 7);
        assert_eq!(blocks[1].len(), 3);
        assert_eq!(blocks[2].len(), 13);
    }

    #[test]
    fn a_trailing_partial_command_is_dropped_rather_than_sent_short() {
        let mut payload = command_block(2);
        payload.extend_from_slice(&[0x09, 0xFC, 0x40, 0x00]); // claims 64 params, has 1
        let blocks = split_command_blocks(&payload);
        assert_eq!(blocks.len(), 1, "the partial command must not be sent");
    }

    #[tokio::test]
    async fn a_bootloader_controller_gets_the_full_secure_boot_sequence_in_order() {
        // Order is fixed by the protocol. Out of order the controller rejects — the good
        // case; the bad case is a part that accepts a partial upload and boots an image
        // that half-works.
        let transport = controller(version_tlv(tlv_image::BOOTLOADER));
        IntelInit::default()
            .init(&transport, &firmware_with(sfi(&command_block(8))))
            .await
            .unwrap();

        let fragments: Vec<u8> = transport
            .sent()
            .iter()
            .filter_map(|p| match p {
                HciPacket::Command { opcode, params } if *opcode == SECURE_SEND => {
                    params.first().copied()
                }
                _ => None,
            })
            .collect();

        assert_eq!(fragments.first(), Some(&fragment::INIT));
        let first_key = fragments.iter().position(|f| *f == fragment::PUBLIC_KEY);
        let first_sig = fragments.iter().position(|f| *f == fragment::SIGNATURE);
        let first_data = fragments.iter().position(|f| *f == fragment::DATA);
        assert!(first_key < first_sig, "key before signature");
        assert!(first_sig < first_data, "signature before payload");
    }

    #[tokio::test]
    async fn fragments_respect_the_single_byte_parameter_length() {
        // A 256-byte key cannot go in one command: the parameter length field is one
        // byte and the fragment type eats one of them.
        let transport = controller(version_tlv(tlv_image::BOOTLOADER));
        IntelInit::default()
            .init(&transport, &firmware_with(sfi(&command_block(0))))
            .await
            .unwrap();

        for packet in transport.sent() {
            if let HciPacket::Command { opcode, params } = packet {
                if opcode == SECURE_SEND {
                    assert!(
                        params.len() <= MAX_FRAGMENT + 1,
                        "fragment of {} bytes exceeds the parameter field",
                        params.len()
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn an_already_operational_controller_is_left_alone() {
        // A warm reboot leaves the part running its firmware. Re-uploading is neither
        // possible nor needed, and erroring here would make every second start fail.
        let transport = controller(version_tlv(tlv_image::OPERATIONAL));
        IntelInit::default()
            .init(&transport, &FirmwareSet::new())
            .await
            .unwrap();

        assert!(
            !transport.sent_commands().contains(&SECURE_SEND),
            "no firmware should have been sent"
        );
    }

    #[tokio::test]
    async fn a_missing_image_fails_before_the_upload_starts() {
        let transport = controller(version_tlv(tlv_image::BOOTLOADER));
        let err = IntelInit::default()
            .init(&transport, &FirmwareSet::new())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("ibt-20-1-3.sfi"), "got: {err}");
        assert!(!transport.sent_commands().contains(&SECURE_SEND));
    }

    #[tokio::test]
    async fn the_reset_comes_after_the_firmware_and_not_before() {
        let transport = controller(version_tlv(tlv_image::BOOTLOADER));
        IntelInit::default()
            .init(&transport, &firmware_with(sfi(&command_block(2))))
            .await
            .unwrap();

        let opcodes = transport.sent_commands();
        let last_send = opcodes.iter().rposition(|o| *o == SECURE_SEND).unwrap();
        let reset = opcodes.iter().position(|o| *o == INTEL_RESET).unwrap();
        assert!(reset > last_send, "reset must follow the whole upload");
    }

    #[test]
    fn the_loader_claims_only_the_intel_parts_it_knows() {
        let intel = IntelInit::default();
        assert!(intel.matches(UsbId::new(0x8087, 0x0029)), "AX200");
        assert!(intel.matches(UsbId::new(0x8087, 0x0032)), "AX210");
        // An Intel part with no loader must fall through, not get the wrong image.
        assert!(!intel.matches(UsbId::new(0x8087, 0x07dc)));
        assert!(!intel.matches(UsbId::new(0x0bda, 0x8771)));
    }
}
