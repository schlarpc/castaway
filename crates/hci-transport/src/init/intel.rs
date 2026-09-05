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
//! `hci-probe --to-bootloader`.

use substrate_hci::{Command, HciPacket, HciTransport, OpCode};
use tracing::{debug, info, warn};

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
/// TLV types naming the silicon: the CNVi (the Bluetooth IP on the host or card) and
/// the CNVr (the radio). `btintel.h`'s `INTEL_TLV_CNVI_TOP` and `INTEL_TLV_CNVR_TOP`,
/// each a little-endian u32 with the part *type* in the low 12 bits and the stepping in
/// bits 24–27.
const TLV_CNVI_TOP: u8 = 0x10;
const TLV_CNVR_TOP: u8 = 0x11;
/// The CNVi's Bluetooth IP: `hw_variant` in bits 16–21, which is what decides whether the
/// part boots straight into the image or through an intermediate loader first.
const TLV_CNVI_BT: u8 = 0x12;
/// Which signed-header layout the bootloader verifies: `0x00` RSA, `0x01` ECDSA.
const TLV_SBE_TYPE: u8 = 0x2F;

/// The first `hw_variant` that boots through an intermediate loader image
/// (`ibt-*-iml.sfi`) before the operational one. `btintel` gates the whole `-iml` flow on
/// this; below it a part takes one image and one reset.
const HW_VARIANT_INTERMEDIATE_LOADER: u8 = 0x1e;

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

    /// Products this loader claims, with the image stem and signed-header layout each one
    /// is *expected* to take.
    ///
    /// Expected, not used: which image a part needs is not a property of its USB id. The
    /// AX2xx generation answers `Read_Version` with TLVs naming its own silicon, and the
    /// loader builds the image name from those, the way `btintel` does
    /// ([`VersionTlv::image_stem`]). One product id spans more than one CNVi — the
    /// AX211 in the deploy box and the AX210 in the dev box take different images, and
    /// sending the AX211 the AX210's is accepted by the bootloader (the signature is
    /// Intel's either way) and then never boots (#391). So the entry here is what the
    /// probe and [`driveability`](crate::init::driveability) predict *before* the part is
    /// opened, and what a legacy-struct part (the AX200 pair, which answer no TLVs) is
    /// sent.
    ///
    /// The layout follows the same rule: the TLV's `sbe_type` names it when present, and
    /// the entry here stands in when it is not. Getting it wrong sends the other layout's
    /// offsets as the opening fragment, which the part refuses with `0x1F`.
    pub const PRODUCTS: &'static [(u16, &'static str, SecureBoot)] = &[
        (0x0029, "intel/ibt-20-1-3", SecureBoot::Rsa), // AX200
        (0x0026, "intel/ibt-20-1-3", SecureBoot::Rsa), // AX201
        (0x0032, "intel/ibt-0041-0041", SecureBoot::Ecdsa), // AX210: CNVi 0x410, CNVr 0x410
        (0x0033, "intel/ibt-1040-0041", SecureBoot::Ecdsa), // AX211: CNVi 0x401, CNVr 0x410
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

    /// The image stem a product is expected to take, if this loader claims it.
    #[must_use]
    fn expected_image_stem(id: UsbId) -> Option<&'static str> {
        Self::product(id).map(|(stem, _)| stem)
    }
}

#[async_trait::async_trait]
impl ControllerInit for IntelInit {
    fn name(&self) -> &'static str {
        "intel"
    }

    fn matches(&self, id: UsbId) -> bool {
        Self::expected_image_stem(id).is_some()
    }

    fn required_images(&self, id: UsbId) -> Vec<RequiredImage> {
        // Leaking through as `&'static str` so the probe can name the missing file. The
        // stems are compile-time constants, so the only allocation is the pair.
        //
        // The `.sfi` is the firmware; without it `init` cannot boot the part at all. The
        // `.ddc` is the per-board tuning table, and `init` explicitly logs and continues
        // without one — so it must not count against this build's ability to drive the
        // controller (#307).
        //
        // A prediction from the product id, because the part has not been opened yet and
        // only the part knows its silicon (see `PRODUCTS`). `init` asks for what the TLV
        // names, and a build carrying the wrong one finds out there, by file name.
        Self::expected_image_stem(id).map_or_else(Vec::new, |stem| match stem {
            "intel/ibt-0041-0041" => vec![
                RequiredImage::essential("intel/ibt-0041-0041.sfi"),
                RequiredImage::optional("intel/ibt-0041-0041.ddc"),
            ],
            "intel/ibt-1040-0041" => vec![
                RequiredImage::essential("intel/ibt-1040-0041.sfi"),
                RequiredImage::optional("intel/ibt-1040-0041.ddc"),
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
        let expected = Self::product(id).ok_or(TransportError::UnsupportedController(id))?;
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

        let (image_stem, secure_boot) = select_image(&version, expected)?;
        // At info, not debug, and deliberately: this happens once on a cold boot and
        // never again, and without it a deploy log cannot tell a controller that refused
        // `Read_Version` from one that refused a firmware fragment — or, now, one that
        // was sent the image its product id predicted rather than the one it asked for.
        info!(
            image = %image_stem,
            expected = expected.0,
            layout = ?secure_boot,
            version = %hex(&version),
            "intel controller is in the bootloader; loading firmware"
        );

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
    if is_legacy_version(response) {
        return match response.get(LEGACY_FW_VARIANT) {
            Some(&legacy_variant::BOOTLOADER) => RunningImage::Bootloader,
            Some(&legacy_variant::OPERATIONAL) => RunningImage::Operational,
            _ => RunningImage::Unknown,
        };
    }

    tlvs(response)
        .find(|(kind, _)| *kind == TLV_IMAGE_TYPE)
        .map_or(RunningImage::Unknown, |(_, value)| match value.first() {
            Some(&tlv_image::BOOTLOADER) => RunningImage::Bootloader,
            Some(&tlv_image::OPERATIONAL) => RunningImage::Operational,
            _ => RunningImage::Unknown,
        })
}

/// Whether a version response is the AX200-era fixed struct rather than a TLV list.
///
/// The legacy struct is a fixed nine bytes and starts with the Intel hardware platform
/// id, which is always 0x37. A TLV list starts with a type byte, and no type we care
/// about is 0x37 — so the two are told apart without guessing.
fn is_legacy_version(response: &[u8]) -> bool {
    response.len() == LEGACY_VERSION_LEN && response.first() == Some(&0x37)
}

/// Walk a TLV list, yielding `(type, value)` and stopping at the first entry that runs
/// off the end.
fn tlvs(response: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut rest = response;
    std::iter::from_fn(move || {
        let (&kind, after_kind) = rest.split_first()?;
        let (&len, after_len) = after_kind.split_first()?;
        let value = after_len.get(..usize::from(len))?;
        rest = &after_len[usize::from(len)..];
        Some((kind, value))
    })
}

/// The fields of a TLV version response the loader acts on, parsed once at the boundary.
///
/// Every field is optional because every field is: a part answers the TLVs it has, and a
/// missing one is a fact about the part rather than a malformed response. What the
/// loader cannot proceed without, it refuses at the point of use with the field named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VersionTlv {
    cnvi_top: Option<u32>,
    cnvr_top: Option<u32>,
    cnvi_bt: Option<u32>,
    sbe_type: Option<u8>,
}

impl VersionTlv {
    /// Parse a TLV list. Entries that are not one of the fields above are skipped, and a
    /// field of the wrong width is treated as absent rather than misread.
    #[must_use]
    pub fn parse(response: &[u8]) -> Self {
        let mut out = Self::default();
        let u32_le = |v: &[u8]| -> Option<u32> {
            let bytes: [u8; 4] = v.try_into().ok()?;
            Some(u32::from_le_bytes(bytes))
        };
        for (kind, value) in tlvs(response) {
            match kind {
                TLV_CNVI_TOP => out.cnvi_top = u32_le(value),
                TLV_CNVR_TOP => out.cnvr_top = u32_le(value),
                TLV_CNVI_BT => out.cnvi_bt = u32_le(value),
                TLV_SBE_TYPE => out.sbe_type = value.first().copied(),
                _ => {}
            }
        }
        out
    }

    /// `INTEL_HW_VARIANT`: the Bluetooth IP generation, from the CNVi.
    #[must_use]
    pub fn hw_variant(&self) -> Option<u8> {
        self.cnvi_bt
            .and_then(|bt| u8::try_from((bt >> 16) & 0x3f).ok())
    }

    /// The image this part takes: `intel/ibt-<cnvi>-<cnvr>`, exactly as `btintel` names
    /// it.
    ///
    /// Each half is `INTEL_CNVX_TOP_PACK_SWAB` of the top: the 12-bit type shifted up a
    /// nibble, the 4-bit stepping in the low nibble, and the two bytes swapped. The swap
    /// is why the AX210's type `0x410` reads as `0041` and the AX211's `0x401` as `1040`
    /// — two parts one bit apart in the silicon id, one image name apart on disk.
    ///
    /// # Errors
    /// [`TransportError::Controller`] if the response names no silicon — there is then no
    /// image to choose, and choosing one from the product id instead is the guess that
    /// booted nothing on the AX211 — or if the part is a generation that boots through an
    /// intermediate loader, which this loader does not send and must not pretend to.
    pub fn image_stem(&self) -> Result<String, TransportError> {
        let (Some(cnvi), Some(cnvr)) = (self.cnvi_top, self.cnvr_top) else {
            return Err(TransportError::Controller {
                what: "intel read_version",
                detail: "the version response names no CNVi/CNVr silicon, so no image can \
                         be chosen for this part"
                    .to_owned(),
            });
        };
        if let Some(variant) = self.hw_variant() {
            if variant >= HW_VARIANT_INTERMEDIATE_LOADER {
                return Err(TransportError::Controller {
                    what: "intel read_version",
                    detail: format!(
                        "hw_variant {variant:#04x} boots through an intermediate loader \
                         image, which this loader does not implement"
                    ),
                });
            }
        }
        Ok(format!(
            "intel/ibt-{:04x}-{:04x}",
            pack_top(cnvi),
            pack_top(cnvr)
        ))
    }

    /// The signed-header layout the bootloader will verify, if the part said.
    ///
    /// # Errors
    /// [`TransportError::Controller`] for an `sbe_type` that is neither RSA nor ECDSA:
    /// there is no third layout to fall back to, and uploading in either would be a
    /// guess the part answers with `0x1F` at best.
    pub fn secure_boot(&self) -> Result<Option<SecureBoot>, TransportError> {
        match self.sbe_type {
            None => Ok(None),
            Some(0x00) => Ok(Some(SecureBoot::Rsa)),
            Some(0x01) => Ok(Some(SecureBoot::Ecdsa)),
            Some(other) => Err(TransportError::Controller {
                what: "intel read_version",
                detail: format!("sbe_type {other:#04x} is neither RSA (0x00) nor ECDSA (0x01)"),
            }),
        }
    }
}

/// `INTEL_CNVX_TOP_PACK_SWAB(INTEL_CNVX_TOP_TYPE(top), INTEL_CNVX_TOP_STEP(top))`.
fn pack_top(top: u32) -> u16 {
    // Both masks leave fewer than 16 bits, so the narrowing cannot truncate.
    let kind = (top & 0x0000_0fff) as u16;
    let step = ((top & 0x0f00_0000) >> 24) as u16;
    ((kind << 4) | step).swap_bytes()
}

/// Which image to upload, and in which layout, from what the part said about itself.
///
/// A TLV response names its own silicon and that decides; `expected` — the product
/// table's entry — is what a legacy-struct part gets, and the layout a TLV part gets when
/// it carries no `sbe_type`.
///
/// # Errors
/// As [`VersionTlv::image_stem`] and [`VersionTlv::secure_boot`].
fn select_image(
    response: &[u8],
    expected: (&'static str, SecureBoot),
) -> Result<(String, SecureBoot), TransportError> {
    if is_legacy_version(response) {
        return Ok((expected.0.to_owned(), expected.1));
    }
    let tlv = VersionTlv::parse(response);
    let stem = tlv.image_stem()?;
    let secure_boot = tlv.secure_boot()?.unwrap_or(expected.1);
    Ok((stem, secure_boot))
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

    // The upload is the one step that is both long and silent, and "it returned `Ok`" is
    // a weaker statement than it looks: `split_command_blocks` stops at the first
    // fragment it cannot close, so a payload it only partly covers is uploaded *happily*
    // and short. Saying how much was covered turns that into something the log shows
    // rather than something the boot step reports as an unexplained silence.
    let blocks = split_command_blocks(parts.blocks);
    let covered: usize = blocks.iter().map(|b| b.len()).sum();
    debug!(
        fragments = blocks.len(),
        covered,
        payload = parts.blocks.len(),
        "intel: uploading firmware payload"
    );
    if covered != parts.blocks.len() {
        warn!(
            covered,
            payload = parts.blocks.len(),
            "intel: the image does not split into whole fragments; uploading it short"
        );
    }

    for (index, block) in blocks.iter().enumerate() {
        if let Err(e) = secure_send(hci, fragment::DATA, block).await {
            // *Which* fragment stopped separates failures that look identical from
            // outside: a refusal at 0 is a header or layout problem, and one at 2873 is
            // not.
            warn!(index, of = blocks.len(), error = %e, "intel: firmware fragment refused");
            return Err(e);
        }
    }
    debug!(
        fragments = blocks.len(),
        "intel: every firmware fragment was acknowledged"
    );

    // Every fragment being acknowledged is *not* the same as the download being finished,
    // and the difference is the whole of #391. The bootloader acknowledges each
    // `Secure_Send` as it lands and then, separately, emits a vendor notification saying
    // the image as a whole was accepted. `btusb` waits for that before it resets:
    //
    //   "Before switching the device into operational mode and with that booting the
    //    loaded firmware, wait for the bootloader notification that all fragments have
    //    been sent successfully."
    //
    // Resetting without it left the AX211 with no operational image and no bus presence:
    // the notification then arrived 367us *after* `Intel_Reset` had already gone out, was
    // discarded as "not the bootup notification", and the part never came back.
    await_download_result(hci).await
}

/// Intel's "the image was accepted" vendor notification, and how long to allow it.
///
/// The payload is `btintel`'s `intel_secure_send_result`: a result byte, the opcode that
/// produced it, and a status. Only the first matters here — a non-zero result is a
/// refused image, which is worth reporting as such rather than as the boot timeout it
/// would otherwise become five seconds later.
const SECURE_SEND_RESULT: u8 = 0x06;
const DOWNLOAD_RESULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Wait for the bootloader to say the uploaded image is good.
async fn await_download_result(hci: &dyn HciTransport) -> Result<(), TransportError> {
    let deadline = tokio::time::Instant::now() + DOWNLOAD_RESULT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(TransportError::Timeout("intel firmware download result"));
        }
        let packet = tokio::time::timeout(remaining, hci.recv())
            .await
            .map_err(|_| TransportError::Timeout("intel firmware download result"))??;
        let HciPacket::Event { code, params } = packet else {
            continue;
        };
        if code != VENDOR_EVENT || params.first() != Some(&SECURE_SEND_RESULT) {
            debug!(
                event = format!("{code:#04x}"),
                params = %hex(&params),
                "intel: not the download result; still waiting"
            );
            continue;
        }
        match params.get(1) {
            Some(0) => {
                debug!("intel: the bootloader accepted the image");
                return Ok(());
            }
            // A result byte that is present and non-zero is the controller refusing the
            // image it was just given; one that is absent is a notification we do not
            // understand, and booting on either would be booting on a guess.
            other => {
                return Err(TransportError::Controller {
                    what: "intel firmware download",
                    detail: match other {
                        Some(result) => {
                            format!("the bootloader refused the image: result {result:#04x}")
                        }
                        None => format!("truncated download result: {}", hex(&params)),
                    },
                })
            }
        }
    }
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
    debug!(
        boot_addr = format!("{boot_addr:#010x}"),
        "intel: resetting into the operational image"
    );
    hci.send(
        Command::Vendor {
            opcode: INTEL_RESET,
            params: bytes::Bytes::from(params),
        }
        .encode()?,
    )
    .await?;
    // Its own line, because the two halves fail differently and the timestamps separate
    // them: everything before this is the upload, everything after is the part deciding
    // whether to come back.
    debug!("intel: reset sent; waiting for the bootup notification");

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
            // A part that answers *something* here and one that answers nothing are
            // different failures, and discarding the something reported them as the same
            // five-second silence.
            debug!(
                event = format!("{code:#04x}"),
                params = %hex(&params),
                "intel: not the bootup notification; still waiting"
            );
        } else {
            debug!("intel: a non-event packet arrived while waiting for the bootup notification");
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
    /// The common case: a part that accepts the image it is given.
    fn controller(version_tlv: Vec<u8>, payload_len: usize) -> ScriptedTransport {
        controller_announcing(version_tlv, payload_len, 0x00)
    }

    /// As above, but the acceptance carries `result` — and a `payload_len` of
    /// [`usize::MAX`] is a part that never announces at all.
    ///
    /// The download result is emitted **once**, after the fragment that completes the
    /// payload, which is where the AX211 put it: 950us after the last fragment's
    /// command-complete and unprompted by anything the loader sent. Announcing it per
    /// fragment instead would agree with a loader that never waits for it, which is the
    /// failure this fake exists to refuse (#391).
    fn controller_announcing(
        version_tlv: Vec<u8>,
        payload_len: usize,
        result: u8,
    ) -> ScriptedTransport {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let data_seen = AtomicUsize::new(0);
        let signature_seen = AtomicBool::new(false);
        let announced = AtomicBool::new(false);

        ScriptedTransport::new().with_responder(move |sent| {
            let HciPacket::Command { opcode, params } = sent else {
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

            let mut complete = vec![0x01];
            complete.extend_from_slice(&opcode.raw().to_le_bytes());
            complete.push(0x00); // status: success
            if *opcode == READ_VERSION {
                complete.extend_from_slice(&version_tlv);
            }
            let mut out = vec![HciPacket::Event {
                code: code::COMMAND_COMPLETE,
                params: bytes::Bytes::from(complete),
            }];

            if *opcode == SECURE_SEND {
                match params.first().copied() {
                    Some(fragment::SIGNATURE) => signature_seen.store(true, Ordering::Relaxed),
                    // The type byte is a parameter but not payload.
                    Some(fragment::DATA) => {
                        data_seen.fetch_add(params.len() - 1, Ordering::Relaxed);
                    }
                    _ => {}
                }
                if signature_seen.load(Ordering::Relaxed)
                    && data_seen.load(Ordering::Relaxed) >= payload_len
                    && !announced.swap(true, Ordering::Relaxed)
                {
                    out.push(HciPacket::Event {
                        code: VENDOR_EVENT,
                        params: bytes::Bytes::from(vec![
                            SECURE_SEND_RESULT,
                            result,
                            0x00,
                            0x00,
                            0x00,
                        ]),
                    });
                }
            }
            out
        })
    }

    /// How many payload bytes the loader will actually upload, which the fake counts to
    /// know the image is complete.
    ///
    /// Not the same as how many the image *holds*: `split_command_blocks` stops at the
    /// first fragment it cannot close on a 4-byte boundary, so a tail that does not align
    /// is never sent. Expecting the image's length instead leaves the fake waiting for
    /// bytes that are never coming — which is a fake that fails a correct loader.
    fn uploaded_len(image: &[u8], secure_boot: SecureBoot) -> usize {
        let payload = &image[secure_boot.layout().payload..];
        split_command_blocks(payload).iter().map(|b| b.len()).sum()
    }

    /// The **real** `Read_Version` response from the AX200 in this dev box, captured
    /// 2026-07-25 while it was running operational firmware. Nine bytes, legacy layout,
    /// despite the request carrying the 0xFF parameter that asks for TLVs (ground rule 6:
    /// land the finding as a fixture rather than a memory).
    const AX200_OPERATIONAL: [u8; 9] = [0x37, 0x14, 0x00, 0x23, 0x00, 0xfa, 0x11, 0x14, 0x00];

    /// The **real** `Read_Version` response from the AX210 in this dev box, read through
    /// the kernel (`hcitool cmd 0x3f 0x0005 0xff`) on 2026-09-04 while it was running
    /// operational firmware. CNVi and CNVr type `0x410`, `hw_variant` `0x17`, and the
    /// kernel loaded `ibt-0041-0041.sfi` into it that morning.
    const AX210_OPERATIONAL_TLV: [u8; 103] = [
        0x10, 0x04, 0x10, 0x04, 0x40, 0x00, 0x11, 0x04, 0x10, 0x04, 0x40, 0x00, 0x12, 0x04, 0x00,
        0x37, 0x17, 0x00, 0x13, 0x04, 0x20, 0x37, 0x12, 0x00, 0x15, 0x02, 0x13, 0x04, 0x16, 0x02,
        0x00, 0x00, 0x17, 0x02, 0x87, 0x80, 0x18, 0x02, 0x32, 0x00, 0x1C, 0x01, 0x03, 0x1D, 0x02,
        0x05, 0x1A, 0x1E, 0x01, 0x01, 0x1F, 0x04, 0xCA, 0x40, 0x01, 0x00, 0x20, 0x01, 0x06, 0x21,
        0x01, 0x06, 0x22, 0x01, 0xA0, 0x23, 0x01, 0x0D, 0x24, 0x02, 0x02, 0x00, 0x25, 0x02, 0xCA,
        0x30, 0x26, 0x02, 0xCA, 0x30, 0x2A, 0x01, 0x01, 0x2B, 0x01, 0x01, 0x32, 0x04, 0x7D, 0x67,
        0x25, 0x29, 0x33, 0x01, 0x00, 0x34, 0x00, 0x35, 0x04, 0x00, 0x00, 0x00, 0x00,
    ];

    /// The **real** `Read_Version` response from the AX211 in the deploy box, in its
    /// bootloader, as logged by the probe on 2026-09-05 (#391). CNVi type `0x401`, CNVr
    /// type `0x410`, `hw_variant` `0x19`, `sbe_type` `0x01`: the part that was sent
    /// `ibt-0041-0041` on 2026-08-29 and booted nothing, and that booted `ibt-1040-0041`
    /// in 17ms once asked what it was.
    const AX211_BOOTLOADER_TLV: [u8; 90] = [
        0x10, 0x04, 0x01, 0x04, 0x08, 0x00, 0x11, 0x04, 0x10, 0x14, 0x40, 0x00, 0x12, 0x04, 0x00,
        0x37, 0x19, 0x00, 0x15, 0x02, 0x13, 0x06, 0x16, 0x02, 0x00, 0x00, 0x17, 0x02, 0x87, 0x80,
        0x18, 0x02, 0x33, 0x00, 0x1C, 0x01, 0x01, 0x1D, 0x02, 0x28, 0x13, 0x1E, 0x01, 0x01, 0x1F,
        0x04, 0x26, 0x00, 0x00, 0x00, 0x27, 0x01, 0x00, 0x28, 0x01, 0x01, 0x29, 0x01, 0x00, 0x2A,
        0x01, 0x01, 0x2B, 0x01, 0x01, 0x2C, 0x01, 0x00, 0x2D, 0x03, 0x01, 0x0A, 0x0E, 0x2E, 0x01,
        0x00, 0x2F, 0x01, 0x01, 0x30, 0x06, 0xF9, 0x97, 0x95, 0x6D, 0xB2, 0x5C, 0x31, 0x01, 0x00,
    ];

    /// A TLV block reporting `image`, from the part the scripted controller models: the
    /// AX210's silicon ids and ECDSA secure boot, with a stray TLV first so no offset is
    /// fixed.
    fn version_tlv(image: u8) -> Vec<u8> {
        let mut tlv = vec![0x01, 0x02, 0xAA, 0xBB];
        tlv.extend_from_slice(&[TLV_CNVI_TOP, 0x04, 0x10, 0x04, 0x40, 0x00]);
        tlv.extend_from_slice(&[TLV_CNVR_TOP, 0x04, 0x10, 0x04, 0x40, 0x00]);
        tlv.extend_from_slice(&[TLV_CNVI_BT, 0x04, 0x00, 0x37, 0x17, 0x00]);
        tlv.extend_from_slice(&[TLV_IMAGE_TYPE, 0x01, image]);
        tlv.extend_from_slice(&[TLV_SBE_TYPE, 0x01, 0x01]);
        tlv
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

    /// The AX210 layout, which is what the scripted controller's `sbe_type` asks for.
    fn sfi(blocks: &[u8]) -> Vec<u8> {
        sfi_for(SecureBoot::Ecdsa, blocks)
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

    /// A build carrying `sfi_bytes` under `name`.
    fn firmware_named(name: &'static str, sfi_bytes: Vec<u8>) -> FirmwareSet {
        FirmwareSet::new().with(name, Firmware::File(write_temp("ibt.sfi", &sfi_bytes)))
    }

    /// A build carrying the image the scripted controller names.
    fn firmware_with(sfi_bytes: Vec<u8>) -> FirmwareSet {
        firmware_named("intel/ibt-0041-0041.sfi", sfi_bytes)
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
        let image = sfi_for(SecureBoot::Rsa, &aligned_block(1));
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
        let image = sfi(&command_block(8));
        let transport = controller(
            version_tlv(tlv_image::BOOTLOADER),
            uploaded_len(&image, SecureBoot::Ecdsa),
        );
        IntelInit
            .init(AX210, &transport, &firmware_with(image))
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
        let image = sfi(&command_block(0));
        let transport = controller(
            version_tlv(tlv_image::BOOTLOADER),
            uploaded_len(&image, SecureBoot::Ecdsa),
        );
        IntelInit
            .init(AX210, &transport, &firmware_with(image))
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
        let transport = controller(version_tlv(tlv_image::OPERATIONAL), 0);
        IntelInit
            .init(AX210, &transport, &FirmwareSet::new())
            .await
            .unwrap();

        assert!(
            !transport.sent_commands().contains(&SECURE_SEND),
            "no firmware should have been sent"
        );
    }

    #[tokio::test]
    async fn a_missing_image_fails_before_the_upload_starts() {
        let transport = controller(version_tlv(tlv_image::BOOTLOADER), 0);
        let err = IntelInit
            .init(AX210, &transport, &FirmwareSet::new())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("ibt-0041-0041.sfi"), "got: {err}");
        assert!(!transport.sent_commands().contains(&SECURE_SEND));
    }

    #[tokio::test]
    async fn the_image_asked_for_is_the_one_the_part_names_not_the_one_its_product_id_predicts() {
        // #391 in one test. The AX211 and the AX210 share a loader, a layout and a boot
        // address, and their product table entries once shared an image name. The AX211's
        // silicon ids say otherwise, and a build carrying only the AX210's image must fail
        // by *name* before the upload — not upload it, watch the bootloader accept a
        // validly signed image for the wrong silicon, and reset into nothing.
        let image = sfi(&command_block(2));
        let transport = controller(AX211_BOOTLOADER_TLV.to_vec(), usize::MAX);
        let err = IntelInit
            .init(AX211, &transport, &firmware_with(image))
            .await
            .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("ibt-1040-0041.sfi"), "got: {text}");
        assert!(
            !transport.sent_commands().contains(&SECURE_SEND),
            "the AX210's image must not be sent to the AX211"
        );

        // And with the right image present, that is the one that goes.
        let image = sfi(&command_block(2));
        let transport = controller(
            AX211_BOOTLOADER_TLV.to_vec(),
            uploaded_len(&image, SecureBoot::Ecdsa),
        );
        IntelInit
            .init(
                AX211,
                &transport,
                &firmware_named("intel/ibt-1040-0041.sfi", image),
            )
            .await
            .unwrap();
        assert!(transport.sent_commands().contains(&INTEL_RESET));
    }

    #[tokio::test]
    async fn the_reset_comes_after_the_firmware_and_not_before() {
        let image = sfi(&command_block(2));
        let transport = controller(
            version_tlv(tlv_image::BOOTLOADER),
            uploaded_len(&image, SecureBoot::Ecdsa),
        );
        IntelInit
            .init(AX210, &transport, &firmware_with(image))
            .await
            .unwrap();

        let opcodes = transport.sent_commands();
        let last_send = opcodes.iter().rposition(|o| *o == SECURE_SEND).unwrap();
        let reset = opcodes.iter().position(|o| *o == INTEL_RESET).unwrap();
        assert!(reset > last_send, "reset must follow the whole upload");
    }

    #[tokio::test(start_paused = true)]
    async fn a_controller_that_never_accepts_the_image_is_not_reset() {
        // #391. Every fragment being acknowledged is not the image being accepted: the
        // bootloader acknowledges each `Secure_Send` as it lands and announces the image
        // as a whole separately. Resetting on the acknowledgements alone left the AX211
        // with no operational firmware and no USB presence at all, recoverable only by
        // `pnputil /scan-devices`, and the announcement then arrived 367us after the
        // reset had already gone out.
        //
        // So a part that never announces has to stop the loader *before* `Intel_Reset` —
        // which is the assertion below, and the one that fails against the old loader.
        // `usize::MAX` is the fake never reaching the end of its payload.
        let image = sfi(&command_block(2));
        let transport = controller(version_tlv(tlv_image::BOOTLOADER), usize::MAX);
        let err = IntelInit
            .init(AX210, &transport, &firmware_with(image))
            .await
            .unwrap_err();

        // In virtual time, so the shipped five seconds is asserted rather than waited out.
        assert!(
            format!("{err}").contains("download result"),
            "the timeout must name the step that expired: {err}"
        );
        assert!(
            !transport.sent_commands().contains(&INTEL_RESET),
            "an unconfirmed image must not be booted"
        );
    }

    #[tokio::test]
    async fn a_refused_image_is_reported_as_a_refusal_rather_than_a_boot_timeout() {
        // The same event carries the bootloader's verdict, and a non-zero result is the
        // image being rejected outright. Left unread it would surface five seconds later
        // as "no bootup notification", which points at the boot step rather than at the
        // image that was refused before it.
        let image = sfi(&command_block(2));
        let len = uploaded_len(&image, SecureBoot::Ecdsa);
        let transport = controller_announcing(version_tlv(tlv_image::BOOTLOADER), len, 0x0D);
        let err = IntelInit
            .init(AX210, &transport, &firmware_with(image))
            .await
            .unwrap_err();

        let text = format!("{err}");
        assert!(text.contains("refused the image"), "got: {text}");
        assert!(
            text.contains("0x0d"),
            "the result byte is the diagnosis: {text}"
        );
        assert!(
            !transport.sent_commands().contains(&INTEL_RESET),
            "a refused image must not be booted"
        );
    }

    /// The AX200 in the dev box.
    const AX200: UsbId = UsbId::new(0x8087, 0x0029);
    /// The AX210 in the dev box.
    const AX210: UsbId = UsbId::new(0x8087, 0x0032);
    /// The AX211 in the deploy box.
    const AX211: UsbId = UsbId::new(0x8087, 0x0033);

    #[test]
    fn the_image_is_named_from_the_silicon_the_part_reports() {
        // Both TLVs are captures, and the two parts differ by one bit in the CNVi type
        // — 0x410 against 0x401 — which the byte swap turns into `0041` against `1040`.
        // The kernel loaded `ibt-0041-0041.sfi` into the first part the morning its TLV
        // was read; the second was sent that same image on 2026-08-29 and booted nothing.
        assert_eq!(
            VersionTlv::parse(&AX210_OPERATIONAL_TLV)
                .image_stem()
                .unwrap(),
            "intel/ibt-0041-0041"
        );
        assert_eq!(
            VersionTlv::parse(&AX211_BOOTLOADER_TLV)
                .image_stem()
                .unwrap(),
            "intel/ibt-1040-0041"
        );
        assert_eq!(
            VersionTlv::parse(&AX210_OPERATIONAL_TLV).hw_variant(),
            Some(0x17)
        );
        assert_eq!(
            VersionTlv::parse(&AX211_BOOTLOADER_TLV).hw_variant(),
            Some(0x19)
        );
    }

    #[test]
    fn the_stepping_lands_in_the_low_nibble_before_the_swap() {
        // `INTEL_CNVX_TOP_PACK_SWAB`: type 0x504 at stepping 1 is `ibt-*-4150`, which is
        // a real file name and the one case where the stepping is visible.
        assert_eq!(pack_top(0x0100_0504), 0x4150);
        assert_eq!(pack_top(0x0040_0410), 0x0041);
        assert_eq!(pack_top(0x0008_0401), 0x1040);
    }

    #[test]
    fn a_response_naming_no_silicon_gets_no_image_rather_than_the_predicted_one() {
        // Falling back to the product id here would be the exact guess #391 was.
        let err = VersionTlv::parse(&version_tlv(tlv_image::BOOTLOADER)[..4])
            .image_stem()
            .unwrap_err();
        assert!(format!("{err}").contains("no CNVi/CNVr"), "got: {err}");
        let expected = ("intel/ibt-0041-0041", SecureBoot::Ecdsa);
        assert!(select_image(&[0x01, 0x02, 0xAA, 0xBB], expected).is_err());
        // The legacy struct names no silicon either, and *is* what the product id is for.
        assert_eq!(
            select_image(&AX200_OPERATIONAL, ("intel/ibt-20-1-3", SecureBoot::Rsa)).unwrap(),
            ("intel/ibt-20-1-3".to_owned(), SecureBoot::Rsa)
        );
    }

    #[test]
    fn a_part_that_boots_through_an_intermediate_loader_is_refused() {
        // Blazar and later take an `-iml` image and a second handshake before the
        // operational one. Sending them the operational image alone is another wrong
        // image with a valid signature.
        let mut tlv = version_tlv(tlv_image::BOOTLOADER);
        let bt = tlv.iter().position(|b| *b == TLV_CNVI_BT).unwrap();
        tlv[bt + 4] = HW_VARIANT_INTERMEDIATE_LOADER;
        let err = VersionTlv::parse(&tlv).image_stem().unwrap_err();
        assert!(
            format!("{err}").contains("intermediate loader"),
            "got: {err}"
        );
    }

    #[test]
    fn the_layout_follows_sbe_type_when_the_part_gives_one() {
        let expected = ("intel/ibt-0041-0041", SecureBoot::Rsa);
        let ecdsa = version_tlv(tlv_image::BOOTLOADER);
        assert_eq!(select_image(&ecdsa, expected).unwrap().1, SecureBoot::Ecdsa);

        let mut rsa = ecdsa.clone();
        let sbe = rsa.iter().position(|b| *b == TLV_SBE_TYPE).unwrap();
        rsa[sbe + 2] = 0x00;
        assert_eq!(select_image(&rsa, expected).unwrap().1, SecureBoot::Rsa);

        let mut unknown = ecdsa.clone();
        unknown[sbe + 2] = 0x02;
        assert!(select_image(&unknown, expected).is_err());

        // No `sbe_type` at all: the product table's layout stands in.
        let cut = AX211_BOOTLOADER_TLV
            .iter()
            .position(|b| *b == TLV_SBE_TYPE)
            .unwrap();
        let without = &AX211_BOOTLOADER_TLV[..cut];
        assert_eq!(
            select_image(without, ("intel/ibt-1040-0041", SecureBoot::Ecdsa))
                .unwrap()
                .1,
            SecureBoot::Ecdsa
        );
    }

    #[test]
    fn each_generation_gets_its_own_signed_image() {
        // The product table is the prediction the probe makes before the part is opened,
        // and it has to predict the file `init` will actually ask for on the parts we
        // have, or its MISSING check lies about a part that is going to fail.
        let intel = IntelInit;
        assert_eq!(
            IntelInit::expected_image_stem(AX200),
            Some("intel/ibt-20-1-3"),
            "AX200"
        );
        assert_eq!(
            IntelInit::expected_image_stem(AX210),
            Some("intel/ibt-0041-0041"),
            "AX210 is a different generation"
        );
        assert_eq!(
            IntelInit::expected_image_stem(AX211),
            Some("intel/ibt-1040-0041"),
            "the AX211's silicon is not the AX210's"
        );
        assert!(intel
            .required_images(AX210)
            .contains(&RequiredImage::essential("intel/ibt-0041-0041.sfi")));
        assert!(intel
            .required_images(AX211)
            .contains(&RequiredImage::essential("intel/ibt-1040-0041.sfi")));
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
