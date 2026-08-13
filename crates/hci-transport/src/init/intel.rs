//! Intel controllers: the AX200/AX201 and AX210/AX211 secure-boot firmware flow.
//!
//! Reference is the kernel's `btintel.c`, which is the specification here — and also the
//! oracle: a usbmon capture of the kernel bringing up the same radio gives a transcript
//! this sequence can be diffed against (architecture §11.3a). That diff is what this file
//! is now built from: on 2026-08-08 an AX210 was pushed back into its bootloader and the
//! kernel's own 2877-fragment upload captured, and what follows reproduces it fragment for
//! fragment (#229).
//!
//! Note the loader can only be exercised against a controller the kernel has *not*
//! already initialised, and unbinding `btusb` is not enough to arrange that: the
//! operational image survives both a driver unbind and a USB port reset. The part has to
//! be sent back to the bootloader with `Intel_Reset` first — see
//! `examples/probe --to-bootloader`.

use substrate_hci::{Command, HciPacket, HciTransport, OpCode};
use tracing::{debug, info};

use crate::error::TransportError;
use crate::firmware::FirmwareSet;
use crate::init::{ControllerInit, RequiredImage, UsbId};

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

/// The CSS header is 128 bytes in both layouts; everything else about them differs.
const CSS_HEADER_LEN: usize = 128;

/// Which signed-header layout a `.sfi` carries.
///
/// Two generations, two layouts, and the numbers are not guessable from one another —
/// getting this wrong sends the *other* layout's bytes as the opening fragment and the
/// controller answers `0x1F`. Confirmed on an AX210 on 2026-08-08 (#229): the same
/// `Secure_Send`, on the same pipe, differing only in which offset the CSS header was
/// read from, is rejected with `0x1F` at 0 and accepted with `0x00` at 644.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecureBoot {
    /// AX200/AX201. A 128-byte CSS header, a 256-byte RSA modulus followed by a 4-byte
    /// exponent, then a 256-byte signature — so the signature starts at 388, not 384,
    /// and the payload at 644, not 640. Both of those fours have been wrong here.
    Rsa,
    /// AX210/AX211. The 644-byte RSA header above is present but unused; a 320-byte
    /// ECDSA header follows it, with a 128-byte CSS header and a **96**-byte key and
    /// signature. The payload starts at 964.
    Ecdsa,
}

/// Where each block of a signed header sits, as `(offset, length)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    css: (usize, usize),
    public_key: (usize, usize),
    signature: (usize, usize),
    /// First byte of the command/data payload.
    payload: usize,
}

impl SecureBoot {
    /// The offsets this layout puts each block at.
    ///
    /// These are `btintel.c`'s `RSA_HEADER_LEN`, `ECDSA_OFFSET` and `ECDSA_HEADER_LEN`
    /// spelled out. Every one of them was checked byte-for-byte against what the kernel
    /// uploaded to this part.
    const fn layout(self) -> Layout {
        match self {
            Self::Rsa => Layout {
                css: (0, CSS_HEADER_LEN),
                public_key: (128, 256),
                signature: (388, 256),
                payload: 644,
            },
            Self::Ecdsa => Layout {
                css: (644, CSS_HEADER_LEN),
                public_key: (772, 96),
                signature: (868, 96),
                payload: 964,
            },
        }
    }
}

/// Largest payload one `Secure_Send` carries. The opcode's parameter field is a single
/// byte, and the fragment type consumes one of them.
const MAX_FRAGMENT: usize = 252;

/// Intel firmware loader.
#[derive(Debug, Clone, Default)]
pub struct IntelInit;

impl IntelInit {
    /// Intel's USB vendor id.
    pub const VENDOR: u16 = 0x8087;

    /// Products this loader handles, and the image stem each one takes.
    ///
    /// The stem is per-product, not per-loader, and that is the whole point. It used to be
    /// a fixed field on the loader — `ibt-20-1-3`, which is the AX200/AX201 image — while
    /// the product list also claimed the AX210 and AX211. Those are a different generation
    /// and need `ibt-0041-0041`, so a bootloader-mode AX210 was being sent another part's
    /// *signed* image: secure boot rejects it, or worse accepts a partial upload. The
    /// right blob was already in the binary, unused, because `flake.nix` embeds it.
    ///
    /// The signed-header layout travels with the stem, because it is a property of the
    /// same generation: the AX200 pair are RSA, the AX210 pair are ECDSA, and sending one
    /// layout's offsets to the other part gets the opening fragment refused with `0x1F`.
    ///
    /// `btintel` derives both from the TLV version response (the CNVi/CNVR ids, and
    /// `sbe_type`) rather than from USB. Keying on the product id is a narrower rule that
    /// happens to agree for every part we claim; if that stops being true, the answer is
    /// to read the TLV, not to add another entry here.
    pub const PRODUCTS: &'static [(u16, &'static str, SecureBoot)] = &[
        (0x0029, "intel/ibt-20-1-3", SecureBoot::Rsa), // AX200
        (0x0026, "intel/ibt-20-1-3", SecureBoot::Rsa), // AX201
        (0x0032, "intel/ibt-0041-0041", SecureBoot::Ecdsa), // AX210
        (0x0033, "intel/ibt-0041-0041", SecureBoot::Ecdsa), // AX211
    ];

    /// The image stem and header layout for a product, if this loader claims it.
    #[must_use]
    fn product(id: UsbId) -> Option<(&'static str, SecureBoot)> {
        if id.vendor != Self::VENDOR {
            return None;
        }
        Self::PRODUCTS
            .iter()
            .find(|(product, _, _)| *product == id.product)
            .map(|(_, stem, layout)| (*stem, *layout))
    }

    /// The image stem for a product, if this loader claims it.
    #[must_use]
    fn image_stem(id: UsbId) -> Option<&'static str> {
        Self::product(id).map(|(stem, _)| stem)
    }
}

#[async_trait::async_trait]
impl ControllerInit for IntelInit {
    fn name(&self) -> &'static str {
        "intel"
    }

    fn matches(&self, id: UsbId) -> bool {
        Self::image_stem(id).is_some()
    }

    fn required_images(&self, id: UsbId) -> Vec<RequiredImage> {
        // Leaking through as `&'static str` so the probe can name the missing file. The
        // stems are compile-time constants, so the only allocation is the pair.
        //
        // The `.sfi` is the firmware; without it `init` cannot boot the part at all. The
        // `.ddc` is the per-board tuning table, and `init` explicitly logs and continues
        // without one — so it must not count against this build's ability to drive the
        // controller (#307).
        Self::image_stem(id).map_or_else(Vec::new, |stem| match stem {
            "intel/ibt-0041-0041" => vec![
                RequiredImage::essential("intel/ibt-0041-0041.sfi"),
                RequiredImage::optional("intel/ibt-0041-0041.ddc"),
            ],
            _ => vec![
                RequiredImage::essential("intel/ibt-20-1-3.sfi"),
                RequiredImage::optional("intel/ibt-20-1-3.ddc"),
            ],
        })
    }

    async fn init(
        &self,
        id: UsbId,
        hci: &dyn HciTransport,
        firmware: &FirmwareSet,
    ) -> Result<(), TransportError> {
        let (image_stem, secure_boot) =
            Self::product(id).ok_or(TransportError::UnsupportedController(id))?;
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
            RunningImage::Bootloader => {
                // At info, not debug, and deliberately: this happens once on a cold boot
                // and never again, it is the branch that has never run against real
                // hardware (#229), and without it a deploy log cannot tell a controller
                // that refused `Read_Version` from one that refused a firmware fragment.
                info!(
                    image = %image_stem,
                    version = %hex(&version),
                    "intel controller is in the bootloader; loading firmware"
                );
            }
            RunningImage::Unknown => {
                return Err(TransportError::Controller {
                    what: "intel read_version",
                    detail: format!("unrecognised version response: {}", hex(&version)),
                })
            }
        }

        let sfi_name = format!("{image_stem}.sfi");
        let sfi = firmware.get(&sfi_name).await?;
        let parts = split_sfi(&sfi, &sfi_name, secure_boot)?;

        // The boot address is read out of the image rather than assumed. It used to be
        // the AX200-era constant 0x00040800; this AX210's image says 0x00100800, and
        // booting a part at the wrong address is not something it reports politely.
        let boot_addr = boot_address(parts.blocks).ok_or_else(|| TransportError::Firmware {
            name: sfi_name.clone(),
            detail: "no CMD_WRITE_BOOT_PARAMS in the image, so no boot address".to_owned(),
        })?;

        // From here until the new image is running, HCI lives on the bulk pipes.
        hci.set_bootloader_framing(true);
        let loaded = download_firmware(hci, &parts).await;
        let booted = match loaded {
            Ok(()) => boot(hci, boot_addr).await,
            Err(e) => Err(e),
        };
        // Whatever happened, stop reading bulk IN as events: on the way out of this
        // function the caller either has an operational controller or an error, and in
        // both cases ACL is what that endpoint carries next.
        hci.set_bootloader_framing(false);
        booted?;

        // DDC is the per-board tuning table. Missing it is not fatal — the radio works,
        // just not to spec for this antenna layout — so a build without the file logs
        // and continues rather than refusing to start.
        let ddc_name = format!("{image_stem}.ddc");
        match firmware.get(&ddc_name).await {
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

/// Split a `.sfi` into the four transfers `btintel` performs, for one header layout.
///
/// # Errors
/// [`TransportError::Firmware`] if the image is shorter than the header that layout
/// describes, which is the shape a mismatched image arrives in.
pub fn split_sfi<'a>(
    sfi: &'a [u8],
    name: &str,
    secure_boot: SecureBoot,
) -> Result<SfiParts<'a>, TransportError> {
    let layout = secure_boot.layout();
    if sfi.len() <= layout.payload {
        return Err(TransportError::Firmware {
            name: name.to_owned(),
            detail: format!(
                "image is {} bytes; the {:?} header alone needs {}",
                sfi.len(),
                secure_boot,
                layout.payload
            ),
        });
    }
    let block = |(offset, len): (usize, usize)| &sfi[offset..offset + len];
    Ok(SfiParts {
        css: block(layout.css),
        public_key: block(layout.public_key),
        signature: block(layout.signature),
        blocks: &sfi[layout.payload..],
    })
}

/// Intel's `CMD_WRITE_BOOT_PARAMS`, whose first parameter is the address to boot from.
const WRITE_BOOT_PARAMS: u16 = 0xFC0E;

/// Find the address the image wants to be booted at.
///
/// `btintel_firmware_version` scans the payload for this command "instead of using
/// static value per SKU", and the SKUs disagree: the AX200 image says `0x00040800`, this
/// AX210's says `0x00100800`.
#[must_use]
pub fn boot_address(payload: &[u8]) -> Option<u32> {
    let mut rest = payload;
    while rest.len() >= 3 {
        let opcode = u16::from_le_bytes([rest[0], rest[1]]);
        let plen = usize::from(rest[2]);
        if opcode == WRITE_BOOT_PARAMS {
            let addr = rest.get(3..7)?;
            return Some(u32::from_le_bytes([addr[0], addr[1], addr[2], addr[3]]));
        }
        rest = rest.get(3 + plen..)?;
    }
    None
}

/// Upload a firmware image.
async fn download_firmware(
    hci: &dyn HciTransport,
    parts: &SfiParts<'_>,
) -> Result<(), TransportError> {
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

/// Split the payload into fragments the controller will accept.
///
/// The tail of a `.sfi` is a sequence of HCI commands — 3-byte header (opcode, then a
/// one-byte parameter length) followed by that many parameter bytes. Fragmenting on any
/// other boundary hands the controller a half command.
///
/// Command boundaries are necessary but **not sufficient**: a `Secure_Send` payload has
/// to be a multiple of four bytes, so `btintel_download_firmware_payload` accumulates
/// whole commands until the running length is 4-aligned and sends *that* as one
/// fragment. The image contains Intel NOPs placed to make it work out. Emitting one
/// fragment per command instead leaves 5 of the 2877 in `ibt-0041-0041.sfi` misaligned,
/// and the controller rejects those — this rule reproduces the kernel's own 2874
/// fragments exactly, length for length.
#[must_use]
pub fn split_command_blocks(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut frag = 0;
    while start + frag + 3 <= data.len() {
        let plen = usize::from(data[start + frag + 2]);
        let end = frag + 3 + plen;
        if start + end > data.len() {
            break;
        }
        frag = end;
        if frag % 4 == 0 {
            out.push(&data[start..start + frag]);
            start += frag;
            frag = 0;
        }
    }
    out
}

/// Reset into the freshly uploaded image and wait for it to come up.
///
/// `Intel_Reset` is not an ordinary command: it "will actually not send a command
/// complete event" (`btusb.c`), which `btusb` papers over by injecting a fake one. Waiting
/// for a completion here simply times out. The real signal is a vendor bootup
/// notification from the operational firmware — captured from this part as
/// `ff 07 02 00 02 01 02 ff 01`, and the kernel allows it five seconds.
async fn boot(hci: &dyn HciTransport, boot_addr: u32) -> Result<(), TransportError> {
    // reset type, patch enable, ddc reload, boot option, then the boot address.
    let mut params = vec![0x00, 0x01, 0x00, 0x01];
    params.extend_from_slice(&boot_addr.to_le_bytes());
    hci.send(
        Command::Vendor {
            opcode: INTEL_RESET,
            params: bytes::Bytes::from(params),
        }
        .encode()?,
    )
    .await?;

    let deadline = tokio::time::Instant::now() + BOOT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(TransportError::Timeout("intel bootup notification"));
        }
        let packet = tokio::time::timeout(remaining, hci.recv())
            .await
            .map_err(|_| TransportError::Timeout("intel bootup notification"))??;
        if let HciPacket::Event { code, params } = packet {
            if code == VENDOR_EVENT && params.first() == Some(&BOOTUP_NOTIFICATION) {
                debug!(
                    boot_addr = format!("{boot_addr:#010x}"),
                    "intel image booted"
                );
                return Ok(());
            }
        }
    }
}

/// Intel's vendor event code, which carries the bootup notification among other things.
const VENDOR_EVENT: u8 = 0xFF;
/// First parameter byte of that event when it means "the image is running".
const BOOTUP_NOTIFICATION: u8 = 0x02;
/// How long the operational image gets to come up. `btintel_boot` allows the same.
const BOOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

    /// A controller that completes every command it is sent — except the one that
    /// doesn't.
    ///
    /// `Intel_Reset` answers with a bootup notification and no command-complete, because
    /// that is what the real part does (`btusb` injects the fake completion that hides
    /// this). A fake that completed it would agree with a loader that waits for the wrong
    /// thing, which is the failure mode ground rule 6 names by hand.
    fn controller(version_tlv: Vec<u8>) -> ScriptedTransport {
        ScriptedTransport::new().with_responder(move |sent| {
            let HciPacket::Command { opcode, .. } = sent else {
                return Vec::new();
            };
            if *opcode == INTEL_RESET {
                return vec![HciPacket::Event {
                    code: VENDOR_EVENT,
                    params: bytes::Bytes::from_static(&[
                        BOOTUP_NOTIFICATION,
                        0x00,
                        0x02,
                        0x01,
                        0x02,
                        0xFF,
                        0x01,
                    ]),
                }];
            }
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

    /// A `.sfi` in `secure_boot`'s layout, with `blocks` as the payload.
    ///
    /// Built from the layout table rather than from repeated constants, so a test image
    /// cannot drift away from the offsets the loader reads.
    fn sfi_for(secure_boot: SecureBoot, blocks: &[u8]) -> Vec<u8> {
        let layout = secure_boot.layout();
        let mut image = vec![0x00; layout.payload];
        let fill = |image: &mut Vec<u8>, (offset, len): (usize, usize), byte: u8| {
            image[offset..offset + len].fill(byte);
        };
        fill(&mut image, layout.css, 0xAA);
        fill(&mut image, layout.public_key, 0xBB);
        fill(&mut image, layout.signature, 0xCC);
        // Every image has to name a boot address or the loader refuses it.
        image.extend_from_slice(&write_boot_params(0x0010_0800));
        image.extend_from_slice(blocks);
        image
    }

    /// The AX200-era layout, which is what the `ibt-20-1-3` fixtures below use.
    fn sfi(blocks: &[u8]) -> Vec<u8> {
        sfi_for(SecureBoot::Rsa, blocks)
    }

    /// A `CMD_WRITE_BOOT_PARAMS` command carrying `addr`, padded to 4-byte alignment.
    fn write_boot_params(addr: u32) -> Vec<u8> {
        let mut b = vec![0x0E, 0xFC, 0x05];
        b.extend_from_slice(&addr.to_le_bytes());
        b.push(0x00); // one spare parameter byte, so the command is 8 bytes
        b
    }

    /// One HCI-command-shaped block with `n` parameter bytes.
    fn command_block(n: u8) -> Vec<u8> {
        let mut b = vec![0x09, 0xFC, n];
        b.extend(std::iter::repeat_n(0xEE, usize::from(n)));
        b
    }

    /// A block whose total length is a multiple of four, so it is a fragment on its own.
    fn aligned_block(words: u8) -> Vec<u8> {
        command_block(words * 4 + 1)
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
        let image = sfi(&aligned_block(1));
        let parts = split_sfi(&image, "test", SecureBoot::Rsa).unwrap();
        assert_eq!(parts.css.len(), 128);
        assert_eq!(parts.public_key.len(), 256);
        assert_eq!(parts.signature.len(), 256);
        assert!(parts.css.iter().all(|b| *b == 0xAA));
        assert!(parts.public_key.iter().all(|b| *b == 0xBB));
        assert!(parts.signature.iter().all(|b| *b == 0xCC));
    }

    #[test]
    fn the_rsa_signature_starts_after_the_exponent_not_after_the_modulus() {
        // The modulus is 256 bytes and is followed by a 4-byte exponent, so the
        // signature is at 388 and the payload at 644. Reading them at 384 and 640 —
        // which is what this shipped for months — takes the last four bytes of the
        // exponent as the first four of the signature, and starts the payload inside
        // the signature.
        let layout = SecureBoot::Rsa.layout();
        assert_eq!(layout.signature.0, 388, "signature offset");
        assert_eq!(layout.payload, 644, "payload offset");
        assert_eq!(
            layout.public_key.0 + layout.public_key.1 + 4,
            layout.signature.0,
            "the 4-byte exponent sits between the modulus and the signature"
        );
    }

    #[test]
    fn the_ax210_layout_is_ecdsa_at_the_offsets_the_part_confirmed() {
        // Every one of these was checked against the bytes the kernel uploaded to a
        // real AX210 on 2026-08-08 (#229). The A/B that proves it matters: the same
        // Secure_Send with the CSS taken from 0 is answered 0x1F, and from 644 is
        // answered 0x00.
        let layout = SecureBoot::Ecdsa.layout();
        assert_eq!(layout.css, (644, 128));
        assert_eq!(layout.public_key, (772, 96));
        assert_eq!(layout.signature, (868, 96));
        assert_eq!(layout.payload, 964);
        // `ibt-0041-0041.sfi` is 713448 bytes and the part was sent 712484 of them.
        assert_eq!(713_448 - layout.payload, 712_484);
    }

    #[test]
    fn a_short_image_is_refused_with_the_size_it_needed() {
        let err = split_sfi(&[0u8; 100], "intel/ibt-20-1-3.sfi", SecureBoot::Rsa).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("644"), "should name the header size: {msg}");
        // An AX210 image is longer, and the message has to say so rather than repeating
        // the other generation's number.
        let err = split_sfi(&[0u8; 700], "intel/ibt-0041-0041.sfi", SecureBoot::Ecdsa).unwrap_err();
        assert!(format!("{err}").contains("964"), "got: {err}");
    }

    #[test]
    fn the_payload_is_split_on_hci_command_boundaries() {
        // Fragmenting anywhere else hands the controller half a command. Chunking by a
        // fixed size would do exactly that.
        let mut payload = command_block(5); // 8 bytes, aligned on its own
        payload.extend(command_block(9)); // 12 bytes, aligned on its own
        let blocks = split_command_blocks(&payload);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].len(), 8);
        assert_eq!(blocks[1].len(), 12);
    }

    #[test]
    fn commands_are_accumulated_until_the_fragment_is_four_byte_aligned() {
        // The rule that matters, and the one this did not have: a Secure_Send payload
        // must be a multiple of four. Three 7-byte commands are individually misaligned
        // and are rejected one at a time; run together they are 21, 14 and finally 28
        // bytes — so the fourth is where the fragment closes.
        let payload: Vec<u8> = std::iter::repeat_n(command_block(4), 4).flatten().collect();
        assert_eq!(payload.len(), 28);
        let blocks = split_command_blocks(&payload);
        assert_eq!(blocks.len(), 1, "one fragment, not four");
        assert_eq!(blocks[0].len(), 28);
        assert!(
            blocks.iter().all(|b| b.len() % 4 == 0),
            "every fragment must be 4-byte aligned"
        );
    }

    #[test]
    fn a_tail_that_never_reaches_alignment_is_dropped_rather_than_sent_misaligned() {
        // Better to send nothing than to send a fragment the controller will refuse.
        let payload = command_block(4);
        assert!(split_command_blocks(&payload).is_empty());
    }

    #[test]
    fn the_boot_address_comes_out_of_the_image_and_not_out_of_a_constant() {
        // 0x00040800 is the AX200's; this AX210's image says 0x00100800, and the two
        // were the same hardcoded number until the part was asked.
        let mut payload = command_block(5);
        payload.extend(write_boot_params(0x0010_0800));
        assert_eq!(boot_address(&payload), Some(0x0010_0800));
        assert_eq!(boot_address(&command_block(5)), None);
    }

    #[test]
    fn a_trailing_partial_command_is_dropped_rather_than_sent_short() {
        let mut payload = command_block(5); // 8 bytes: a fragment closes here
        payload.extend_from_slice(&[0x09, 0xFC, 0x40, 0x00]); // claims 64 params, has 1
        let blocks = split_command_blocks(&payload);
        assert_eq!(blocks.len(), 1, "the partial command must not be sent");
        assert_eq!(blocks[0].len(), 8);
    }

    #[tokio::test]
    async fn a_bootloader_controller_gets_the_full_secure_boot_sequence_in_order() {
        // Order is fixed by the protocol. Out of order the controller rejects — the good
        // case; the bad case is a part that accepts a partial upload and boots an image
        // that half-works.
        let transport = controller(version_tlv(tlv_image::BOOTLOADER));
        IntelInit
            .init(AX200, &transport, &firmware_with(sfi(&command_block(8))))
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
        IntelInit
            .init(AX200, &transport, &firmware_with(sfi(&command_block(0))))
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
        IntelInit
            .init(AX200, &transport, &FirmwareSet::new())
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
        let err = IntelInit
            .init(AX200, &transport, &FirmwareSet::new())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("ibt-20-1-3.sfi"), "got: {err}");
        assert!(!transport.sent_commands().contains(&SECURE_SEND));
    }

    #[tokio::test]
    async fn the_reset_comes_after_the_firmware_and_not_before() {
        let transport = controller(version_tlv(tlv_image::BOOTLOADER));
        IntelInit
            .init(AX200, &transport, &firmware_with(sfi(&command_block(2))))
            .await
            .unwrap();

        let opcodes = transport.sent_commands();
        let last_send = opcodes.iter().rposition(|o| *o == SECURE_SEND).unwrap();
        let reset = opcodes.iter().position(|o| *o == INTEL_RESET).unwrap();
        assert!(reset > last_send, "reset must follow the whole upload");
    }

    /// The AX200 in the dev box.
    const AX200: UsbId = UsbId::new(0x8087, 0x0029);

    #[test]
    fn each_generation_gets_its_own_signed_image() {
        // A secure-boot part sent another part's signed image rejects it, or worse
        // accepts a partial upload. The AX210 blob was already embedded by `flake.nix`
        // and simply never selected, because the stem was a fixed field on the loader.
        let intel = IntelInit;
        assert_eq!(
            IntelInit::image_stem(AX200),
            Some("intel/ibt-20-1-3"),
            "AX200"
        );
        assert_eq!(
            IntelInit::image_stem(UsbId::new(0x8087, 0x0032)),
            Some("intel/ibt-0041-0041"),
            "AX210 is a different generation"
        );
        assert_ne!(
            IntelInit::image_stem(AX200),
            IntelInit::image_stem(UsbId::new(0x8087, 0x0033)),
            "AX211 must not be sent the AX200 image"
        );
        // And the probe must name the file it will actually ask for, or its MISSING
        // check lies about a part that is going to fail.
        assert!(intel
            .required_images(UsbId::new(0x8087, 0x0032))
            .contains(&RequiredImage::essential("intel/ibt-0041-0041.sfi")));
        assert!(intel
            .required_images(AX200)
            .contains(&RequiredImage::essential("intel/ibt-20-1-3.sfi")));
        // And the .ddc is *optional*, because `init` logs and continues without one: a
        // build carrying only the .sfi can still drive this part (#307).
        assert!(intel
            .required_images(AX200)
            .contains(&RequiredImage::optional("intel/ibt-20-1-3.ddc")));
    }

    #[test]
    fn the_loader_claims_only_the_intel_parts_it_knows() {
        let intel = IntelInit;
        assert!(intel.matches(UsbId::new(0x8087, 0x0029)), "AX200");
        assert!(intel.matches(UsbId::new(0x8087, 0x0032)), "AX210");
        // An Intel part with no loader must fall through, not get the wrong image.
        assert!(!intel.matches(UsbId::new(0x8087, 0x07dc)));
        assert!(!intel.matches(UsbId::new(0x0bda, 0x8771)));
    }
}
