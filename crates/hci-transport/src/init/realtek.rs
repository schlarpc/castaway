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
/// The standard version read, which `btrtl.c` issues *before* any vendor command.
const READ_LOCAL_VERSION: OpCode = OpCode::new(0x1001);

/// Realtek's Bluetooth SIG company identifier.
///
/// Stable in every state, unlike `lmp_subver`, so this is what actually answers "is this
/// a Realtek part" — and unlike the USB id, it comes from the silicon rather than from
/// whichever vendor rebadged it.
const MANUFACTURER_REALTEK: u16 = 0x005D;

/// `lmp_subver` values that identify an *unpatched* Realtek Bluetooth core.
///
/// **These only appear before firmware is loaded.** Once a patch is applied the
/// controller reports the firmware's own version here instead — a UB500 with the
/// kernel's firmware in it answers `lmp_subver 0xd922`, `hci_rev 0xdfc6`, which
/// concatenate to `0xdfc6d922`: precisely the `fw_version` in the epatch header. That is
/// how an already-patched part is recognised, and it is the difference between skipping
/// a redundant download and wedging a working dongle.
/// What blob a `(lmp_subver, hci_rev)` pair calls for.
///
/// **Both halves, because `lmp_subver` alone does not identify the image.** `btrtl.c`
/// keys on the pair, and it has to: `0x8761` with `hci_rev 0x000b` is a bare RTL8761B and
/// takes `rtl8761b_fw.bin`, while `0x8761` with `hci_rev 0x000c` is the BU variant and
/// takes `rtl8761bu_fw.bin`. The loader used to hold one fixed pair of filenames and send
/// them to every part it claimed, while reading `hci_rev` into `LocalVersion` and only
/// logging it. A bare 8761B or an 8723B therefore got another chip's patch — which
/// downloads command-by-command without complaint and then misbehaves or bricks the
/// radio, the worst case this file's own comments warn about.
///
/// `None` for a chip we can name but have no blob for: better a clear refusal than a
/// plausible-looking wrong image.
const KNOWN_CHIPS: &[(u16, Option<u16>, &str, Option<&str>)] = &[
    (0x8761, Some(0x000b), "RTL8761B", Some("rtl8761b")),
    (0x8761, Some(0x000c), "RTL8761BU", Some("rtl8761bu")),
    // Seen on the TP-Link UB500 in the dev box, which is a BU.
    (0x8761, None, "RTL8761A/B", Some("rtl8761bu")),
    (0x8723, None, "RTL8723B", None),
    (0x8821, None, "RTL8821A", None),
    (0x8822, None, "RTL8822B", None),
    (0x8852, None, "RTL8852A", None),
    (0x8703, None, "RTL8703B", None),
];

/// Identify the chip and the blob stem for a version response.
///
/// Exact `hci_rev` matches win over the wildcard entry, so adding a precise pair narrows
/// the rule rather than being shadowed by the catch-all above it.
fn chip_for(lmp_subver: u16, hci_rev: u16) -> Option<(&'static str, Option<&'static str>)> {
    let exact = KNOWN_CHIPS
        .iter()
        .find(|(subver, rev, _, _)| *subver == lmp_subver && *rev == Some(hci_rev));
    let any = KNOWN_CHIPS
        .iter()
        .find(|(subver, rev, _, _)| *subver == lmp_subver && rev.is_none());
    exact.or(any).map(|(_, _, chip, stem)| (*chip, *stem))
}
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
#[derive(Debug, Clone, Default)]
pub struct RealtekInit;

impl RealtekInit {
    /// Realtek's own USB vendor id.
    pub const VENDOR: u16 = 0x0BDA;

    /// Controllers this loader handles, as `(vendor, product)`.
    ///
    /// **Realtek chips ship under other companies' USB ids, and this bit us.** The
    /// TP-Link UB500 in the dev box is an RTL8761BU that reports `2357:0604` — TP-Link's
    /// vendor id, not Realtek's — so a registry keyed on `0x0BDA` alone falls through to
    /// [`crate::NoInit`], does nothing, and leaves the dongle without firmware. On Linux
    /// that is invisible because the kernel already loaded it; on Windows, where nothing
    /// else will, the device is simply dead.
    ///
    /// The list is therefore inherently incomplete: any vendor may rebadge the same part.
    /// The robust fix is to identify the *chip* over HCI (`Read_Local_Version`'s
    /// `lmp_subver`, as `btrtl.c` does) when the USB id is unknown, rather than trusting
    /// the id at all — see the note in [`ControllerInit::matches`] callers.
    pub const CONTROLLERS: &'static [(u16, u16)] = &[
        (0x0BDA, 0x8771), // Realtek reference RTL8761BU
        (0x0BDA, 0x8761), // RTL8761B
        (0x0BDA, 0xA725),
        (0x0BDA, 0xB00A),
        (0x2357, 0x0604), // TP-Link UB500 — verified on the dev box
        (0x0B05, 0x190E), // ASUS BT500
        (0x7392, 0xC611), // Edimax BT-8500
        (0x2550, 0x8761),
    ];
}

#[async_trait::async_trait]
impl ControllerInit for RealtekInit {
    fn name(&self) -> &'static str {
        "realtek"
    }

    fn matches(&self, id: UsbId) -> bool {
        Self::CONTROLLERS
            .iter()
            .any(|(vendor, product)| *vendor == id.vendor && *product == id.product)
    }

    fn required_images(&self, _id: UsbId) -> Vec<&'static str> {
        // The USB id does not say which blob: the chip does, and that answer needs an HCI
        // round trip we cannot do here. The BU is what every dongle in this list has
        // turned out to be so far, so it is what the probe checks for.
        vec!["rtl_bt/rtl8761bu_fw.bin", "rtl_bt/rtl8761bu_config.bin"]
    }

    async fn init(
        &self,
        _id: UsbId,
        hci: &dyn HciTransport,
        firmware: &FirmwareSet,
    ) -> Result<(), TransportError> {
        // Standard command first, exactly as `btrtl.c` does. Two reasons, and both
        // matter: it identifies the silicon by `lmp_subver` rather than by whatever USB
        // id the rebadging vendor chose, and — because it is an ordinary HCI command —
        // a failure here says the *transport* is wrong rather than the vendor sequence,
        // which is the distinction that cost a wedged dongle to learn.
        let version = read_local_version(hci).await?;
        if version.manufacturer != MANUFACTURER_REALTEK {
            return Err(TransportError::Controller {
                what: "realtek identification",
                detail: format!(
                    "manufacturer {:#06x} is not Realtek ({MANUFACTURER_REALTEK:#06x}); \
                     refusing rather than downloading firmware to an unknown part",
                    version.manufacturer
                ),
            });
        }

        let Some((chip, stem)) = chip_for(version.lmp_subver, version.hci_rev) else {
            // An unrecognised subver on a confirmed Realtek part means firmware is
            // already loaded: patching rewrites this field to the firmware's own
            // version. Re-downloading is at best redundant and at worst wedges a
            // working controller, so this is a no-op exactly as Intel's operational
            // check is.
            info!(
                lmp_subver = version.lmp_subver,
                hci_rev = version.hci_rev,
                "realtek controller already patched; nothing to load"
            );
            return Ok(());
        };
        info!(
            chip,
            lmp_subver = version.lmp_subver,
            hci_rev = version.hci_rev,
            "realtek controller"
        );

        let Some(stem) = stem else {
            return Err(TransportError::Controller {
                what: "realtek firmware selection",
                detail: format!(
                    "{chip} (lmp_subver {:#06x}, hci_rev {:#06x}) is recognised but this \
                     build ships no blob for it; refusing rather than downloading another \
                     chip's patch",
                    version.lmp_subver, version.hci_rev
                ),
            });
        };

        let rom_version = read_rom_version(hci).await?;
        debug!(rom_version, "realtek rom version");

        let firmware_name = format!("rtl_bt/{stem}_fw.bin");
        let config_name = format!("rtl_bt/{stem}_config.bin");
        let fw = firmware.get(&firmware_name)?;

        // The config is appended to the *extracted patch*, not to the container, and not
        // sent separately. It is optional — a controller without one uses its defaults —
        // so a missing file is a debug line rather than a refusal to start.
        let config = match firmware.get(&config_name) {
            Ok(config) => config.to_vec(),
            Err(e) => {
                debug!(error = %e, "realtek: no config blob; using controller defaults");
                Vec::new()
            }
        };

        let payload = build_payload(&fw, &config, rom_version, &firmware_name)?;
        download(hci, &payload).await?;
        info!(bytes = payload.len(), "realtek firmware loaded");
        Ok(())
    }
}

/// What `Read_Local_Version` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalVersion {
    /// HCI specification version.
    pub hci_ver: u8,
    /// Revision — on a patched Realtek part, the high half of the firmware version.
    pub hci_rev: u16,
    /// Bluetooth SIG company identifier. Stable in every state.
    pub manufacturer: u16,
    /// LMP subversion — the core id when cold, the firmware version's low half when
    /// patched.
    pub lmp_subver: u16,
}

/// Read the standard local version.
///
/// # Errors
/// [`TransportError`] if the controller does not answer, which means the transport is
/// broken rather than the vendor sequence.
async fn read_local_version(hci: &dyn HciTransport) -> Result<LocalVersion, TransportError> {
    let params = send(
        hci,
        Command::Vendor {
            opcode: READ_LOCAL_VERSION,
            params: bytes::Bytes::new(),
        },
    )
    .await?;
    // hci_ver(1), hci_rev(2), lmp_ver(1), manufacturer(2), lmp_subver(2) — after the
    // status byte the caller already stripped.
    if params.len() < 8 {
        return Err(TransportError::Controller {
            what: "read_local_version",
            detail: format!("expected 8 bytes, got {}", params.len()),
        });
    }
    Ok(LocalVersion {
        hci_ver: params[0],
        hci_rev: u16::from_le_bytes([params[1], params[2]]),
        manufacturer: u16::from_le_bytes([params[4], params[5]]),
        lmp_subver: u16::from_le_bytes([params[6], params[7]]),
    })
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

/// Header at the top of an epatch v1 container.
///
/// `signature[8]`, `fw_version` (LE32), `num_patches` (LE16) — then three parallel
/// arrays: chip ids, patch lengths, patch offsets.
const EPATCH_HEADER_LEN: usize = 14;
/// Marks the end of the trailing extension section.
const EXTENSION_SIGNATURE: [u8; 4] = [0x51, 0x04, 0xFD, 0x77];

/// Assemble what actually gets downloaded.
///
/// **A Realtek firmware file is a container, not an image.** `rtl8761bu_fw.bin` holds
/// *two* patches for different chip revisions behind an epatch header, and sending the
/// whole file is meaningless to the controller. The kernel's `btrtl.c` does four things
/// we must also do:
///
/// 1. pick the patch whose chip id is `rom_version + 1`;
/// 2. copy just that patch out;
/// 3. overwrite its **last four bytes** with the container's `fw_version` — the patch
///    carries a placeholder there, and leaving it makes the controller reject the image;
/// 4. append the config blob to the extracted patch.
///
/// Step 4 is the one that reads like an afterthought and is not: the config is part of
/// the downloaded payload, not a separate transfer.
///
/// # Errors
/// [`TransportError::Firmware`] if the container is malformed or holds no patch for this
/// chip revision.
pub fn build_payload(
    fw: &[u8],
    config: &[u8],
    rom_version: u8,
    name: &str,
) -> Result<Vec<u8>, TransportError> {
    let bad = |detail: String| TransportError::Firmware {
        name: name.to_owned(),
        detail,
    };

    let head = fw
        .get(..8)
        .ok_or_else(|| bad("shorter than its 8-byte signature".into()))?;
    if head == RTL_EPATCH_SIGNATURE_V2 {
        // Newer parts use a different container. Refusing by name beats parsing it as v1
        // and downloading nonsense.
        return Err(bad(
            "RTBTCore (epatch v2) containers are not supported yet".into()
        ));
    }
    if head != EPATCH_SIGNATURE {
        return Err(bad(format!(
            "not a Realtek patch container (signature {head:02x?})"
        )));
    }
    if fw.len() < EPATCH_HEADER_LEN + 4 || !fw.ends_with(&EXTENSION_SIGNATURE) {
        return Err(bad(
            "missing the trailing extension signature; truncated or not an epatch".into(),
        ));
    }

    let fw_version = &fw[8..12];
    let num_patches = usize::from(u16::from_le_bytes([fw[12], fw[13]]));
    if num_patches == 0 {
        return Err(bad("container declares no patches".into()));
    }

    // Three parallel arrays follow the header: chip ids (u16), lengths (u16), offsets
    // (u32). Reading them as one interleaved struct is the obvious wrong guess.
    let ids_at = EPATCH_HEADER_LEN;
    let lengths_at = ids_at + num_patches * 2;
    let offsets_at = lengths_at + num_patches * 2;
    let table_end = offsets_at + num_patches * 4;
    if fw.len() < table_end {
        return Err(bad(format!(
            "patch table runs past the file ({num_patches} patches need {table_end} bytes)"
        )));
    }

    // The chip id that matches is the ROM version *plus one*, which is not a typo.
    let wanted = u16::from(rom_version) + 1;
    let index = (0..num_patches)
        .find(|i| {
            let at = ids_at + i * 2;
            u16::from_le_bytes([fw[at], fw[at + 1]]) == wanted
        })
        .ok_or_else(|| {
            bad(format!(
                "no patch for chip id {wanted} (rom version {rom_version}) among {num_patches}"
            ))
        })?;

    let length = usize::from(u16::from_le_bytes([
        fw[lengths_at + index * 2],
        fw[lengths_at + index * 2 + 1],
    ]));
    let offset = u32::from_le_bytes([
        fw[offsets_at + index * 4],
        fw[offsets_at + index * 4 + 1],
        fw[offsets_at + index * 4 + 2],
        fw[offsets_at + index * 4 + 3],
    ]) as usize;

    let patch = fw.get(offset..offset + length).ok_or_else(|| {
        bad(format!(
            "patch {index} at {offset}+{length} runs past the file"
        ))
    })?;
    if length < 4 {
        return Err(bad(format!("patch {index} is only {length} bytes")));
    }

    let mut payload = patch.to_vec();
    // The patch's last four bytes are a placeholder for the container's firmware
    // version. Leaving them makes the controller refuse the image.
    let tail = payload.len() - 4;
    payload[tail..].copy_from_slice(fw_version);
    payload.extend_from_slice(config);
    Ok(payload)
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
                // ROM version 0, so the loader looks for chip id 1.
                params.push(0x00);
            } else if *opcode == READ_LOCAL_VERSION {
                // A *cold* RTL8761: manufacturer 0x005d, lmp_subver 0x8761.
                params.extend_from_slice(&[0x0a, 0xc6, 0x0a, 0x0a, 0x5d, 0x00, 0x61, 0x87]);
            }
            vec![HciPacket::Event {
                code: code::COMMAND_COMPLETE,
                params: bytes::Bytes::from(params),
            }]
        })
    }

    /// The TP-Link UB500 in the dev box.
    const UB500: UsbId = UsbId::new(0x2357, 0x0604);

    /// A container whose extracted patch is exactly `len` bytes — which is what the
    /// fragmentation tests care about, since the patch is what gets downloaded.
    fn image(len: usize) -> Vec<u8> {
        let body = vec![0x5Au8; len.max(4)];
        container(&[(1, &body)])
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
        RealtekInit
            .init(
                UB500,
                &transport,
                &firmware_with(image(MAX_FRAGMENT * 3 + 7)),
            )
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
        RealtekInit
            .init(UB500, &transport, &firmware_with(image(64)))
            .await
            .unwrap();
        assert_eq!(indices(&transport), vec![0x80]);
    }

    #[tokio::test]
    async fn indices_wrap_correctly_across_a_long_image() {
        let transport = controller();
        // 130 fragments, so the counter goes past 127 and starts again.
        RealtekInit
            .init(UB500, &transport, &firmware_with(image(MAX_FRAGMENT * 130)))
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
        RealtekInit
            .init(UB500, &transport, &firmware_with(image(MAX_FRAGMENT * 2)))
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

    /// A synthetic epatch container with `n` patches, shaped exactly like a real one.
    fn container(patches: &[(u16, &[u8])]) -> Vec<u8> {
        let n = patches.len();
        let mut fw = EPATCH_SIGNATURE.to_vec();
        fw.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // fw_version
        fw.extend_from_slice(&u16::try_from(n).unwrap().to_le_bytes());

        let table = EPATCH_HEADER_LEN + n * 2 + n * 2 + n * 4;
        let mut offset = table;
        let mut ids = Vec::new();
        let mut lengths = Vec::new();
        let mut offsets = Vec::new();
        for (chip_id, body) in patches {
            ids.extend_from_slice(&chip_id.to_le_bytes());
            lengths.extend_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
            offsets.extend_from_slice(&u32::try_from(offset).unwrap().to_le_bytes());
            offset += body.len();
        }
        fw.extend_from_slice(&ids);
        fw.extend_from_slice(&lengths);
        fw.extend_from_slice(&offsets);
        for (_, body) in patches {
            fw.extend_from_slice(body);
        }
        fw.extend_from_slice(&EXTENSION_SIGNATURE);
        fw
    }

    #[test]
    fn the_right_patch_is_extracted_for_this_chip_revision() {
        // A container holds several patches. Sending the whole file — which is what a
        // naive loader does — hands the controller headers and other revisions' code.
        let first = [0xA1u8, 0xA2, 0xA3, 0x00, 0x00, 0x00, 0x00];
        let second = [0xB1u8, 0xB2, 0xB3, 0x00, 0x00, 0x00, 0x00];
        let fw = container(&[(1, &first), (2, &second)]);

        // Chip id is the ROM version *plus one*, which is not a typo.
        let payload = build_payload(&fw, &[], 0, "x").unwrap();
        assert_eq!(
            &payload[..3],
            &[0xA1, 0xA2, 0xA3],
            "rom 0 selects chip id 1"
        );
        let payload = build_payload(&fw, &[], 1, "x").unwrap();
        assert_eq!(
            &payload[..3],
            &[0xB1, 0xB2, 0xB3],
            "rom 1 selects chip id 2"
        );
    }

    #[test]
    fn the_patch_tail_is_replaced_with_the_containers_firmware_version() {
        // The patch carries a placeholder there; leaving it makes the controller refuse
        // the image, with no clue as to why.
        let body = [0xA1u8, 0xA2, 0xA3, 0xFF, 0xFF, 0xFF, 0xFF];
        let fw = container(&[(1, &body)]);
        let payload = build_payload(&fw, &[], 0, "x").unwrap();
        assert_eq!(
            &payload[payload.len() - 4..],
            &0xDEAD_BEEFu32.to_le_bytes(),
            "the last four bytes must be the container's fw_version"
        );
    }

    #[test]
    fn the_config_is_appended_to_the_patch_not_to_the_container() {
        // The detail that reads like an afterthought and is not: the config blob is part
        // of the downloaded payload, tacked onto the extracted patch.
        let body = [0xA1u8, 0xA2, 0xA3, 0x00, 0x00, 0x00, 0x00];
        let fw = container(&[(1, &body)]);
        let config = [0x55u8, 0xAB, 0x23, 0x87, 0x00, 0x00];
        let payload = build_payload(&fw, &config, 0, "x").unwrap();

        assert_eq!(payload.len(), body.len() + config.len());
        assert_eq!(&payload[body.len()..], &config, "config goes last");
    }

    #[test]
    fn a_container_with_no_patch_for_this_chip_says_so() {
        let body = [0u8; 8];
        let fw = container(&[(1, &body)]);
        let err = build_payload(&fw, &[], 9, "rtl_bt/x.bin").unwrap_err();
        assert!(format!("{err}").contains("chip id 10"), "got: {err}");
    }

    #[test]
    fn a_file_that_is_not_a_container_is_refused_before_anything_is_sent() {
        // Downloading an arbitrary file succeeds command-by-command and leaves the
        // controller wedged — which is exactly what happened on the dev box.
        let err = build_payload(b"NOT-A-PATCH-FILE-AT-ALL", &[], 0, "x").unwrap_err();
        assert!(
            format!("{err}").contains("not a Realtek patch container"),
            "got: {err}"
        );
        assert!(build_payload(&[0u8; 4], &[], 0, "x").is_err(), "too short");

        // v2 is refused by name rather than misparsed as v1.
        let mut v2 = RTL_EPATCH_SIGNATURE_V2.to_vec();
        v2.extend_from_slice(&[0u8; 32]);
        let err = build_payload(&v2, &[], 0, "x").unwrap_err();
        assert!(format!("{err}").contains("epatch v2"), "got: {err}");
    }

    #[test]
    fn a_container_missing_its_extension_signature_is_refused() {
        let body = [0u8; 8];
        let mut fw = container(&[(1, &body)]);
        fw.truncate(fw.len() - 4);
        assert!(build_payload(&fw, &[], 0, "x").is_err());
    }

    /// The **real** `rtl8761bu_fw.bin` from `linux-firmware`, if this build embedded it.
    ///
    /// Synthetic containers prove the parser handles the shape; only the real file proves
    /// it handles the shape that actually ships.
    #[test]
    fn the_real_ub500_firmware_parses() {
        let firmware = FirmwareSet::embedded();
        let Ok(fw) = firmware.get("rtl_bt/rtl8761bu_fw.bin") else {
            eprintln!("no embedded Realtek firmware in this build; skipping");
            return;
        };
        let config = firmware
            .get("rtl_bt/rtl8761bu_config.bin")
            .map(|c| c.to_vec())
            .unwrap_or_default();

        assert_eq!(&fw[..8], EPATCH_SIGNATURE, "the shipping file is epatch v1");
        let num_patches = u16::from_le_bytes([fw[12], fw[13]]);
        assert_eq!(num_patches, 2, "the shipping container holds two patches");

        // Both revisions must extract, and each must be far smaller than the container —
        // which is the whole point: sending the 44 kB file is not sending a patch.
        for rom_version in [0u8, 1] {
            let payload = build_payload(&fw, &config, rom_version, "rtl8761bu").unwrap();
            assert!(!payload.is_empty());
            assert!(
                payload.len() < fw.len(),
                "a patch must be smaller than the container it came from"
            );
            assert_eq!(
                &payload[payload.len() - config.len()..],
                &config[..],
                "the config must be the tail of the payload"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_image_fails_before_anything_is_downloaded() {
        let transport = controller();
        let err = RealtekInit
            .init(UB500, &transport, &FirmwareSet::new())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("rtl8761bu_fw.bin"), "got: {err}");
        assert!(indices(&transport).is_empty());
    }

    #[tokio::test]
    async fn an_unknown_chip_is_refused_rather_than_flashed() {
        // Downloading firmware to a part we cannot identify is how a device gets
        // wedged. lmp_subver comes from the silicon, unlike the USB id.
        let transport = ScriptedTransport::new().with_responder(|sent| {
            let HciPacket::Command { opcode, .. } = sent else {
                return Vec::new();
            };
            let mut params = vec![0x01];
            params.extend_from_slice(&opcode.raw().to_le_bytes());
            params.push(0x00);
            if *opcode == READ_LOCAL_VERSION {
                // manufacturer 0x000f — not Realtek at all.
                params.extend_from_slice(&[0x0a, 0xc6, 0x0a, 0x0a, 0x0f, 0x00, 0x34, 0x12]);
            }
            vec![HciPacket::Event {
                code: code::COMMAND_COMPLETE,
                params: bytes::Bytes::from(params),
            }]
        });
        let err = RealtekInit
            .init(UB500, &transport, &firmware_with(image(64)))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not Realtek"), "got: {err}");
        assert!(!transport.sent_commands().contains(&DOWNLOAD));
    }

    #[tokio::test]
    async fn an_already_patched_controller_is_left_alone() {
        // Captured from the UB500 in the dev box with the kernel's firmware loaded:
        // manufacturer is still Realtek, but lmp_subver/hci_rev have become the
        // firmware version 0xdfc6d922 from the epatch header. Re-downloading to a
        // working controller is how one gets wedged.
        let transport = ScriptedTransport::new().with_responder(|sent| {
            let HciPacket::Command { opcode, .. } = sent else {
                return Vec::new();
            };
            let mut params = vec![0x01];
            params.extend_from_slice(&opcode.raw().to_le_bytes());
            params.push(0x00);
            if *opcode == READ_LOCAL_VERSION {
                params.extend_from_slice(&[0x0a, 0xc6, 0xdf, 0x0a, 0x5d, 0x00, 0x22, 0xd9]);
            }
            vec![HciPacket::Event {
                code: code::COMMAND_COMPLETE,
                params: bytes::Bytes::from(params),
            }]
        });
        RealtekInit
            .init(UB500, &transport, &firmware_with(image(64)))
            .await
            .unwrap();
        assert!(
            !transport.sent_commands().contains(&DOWNLOAD),
            "a patched controller must not be re-flashed"
        );
    }

    #[tokio::test]
    async fn the_standard_version_read_comes_before_any_vendor_command() {
        // btrtl.c's order, and a better diagnostic: a failure on an ordinary HCI
        // command says the transport is broken, not the vendor sequence.
        let transport = controller();
        RealtekInit
            .init(UB500, &transport, &firmware_with(image(64)))
            .await
            .unwrap();
        let opcodes = transport.sent_commands();
        let local = opcodes
            .iter()
            .position(|o| *o == READ_LOCAL_VERSION)
            .unwrap();
        let rom = opcodes.iter().position(|o| *o == READ_ROM_VERSION).unwrap();
        assert!(local < rom, "standard read must precede the vendor read");
    }

    #[tokio::test]
    async fn the_rom_version_is_read_before_the_download() {
        // It selects which patch applies; reading it after would be pointless.
        let transport = controller();
        RealtekInit
            .init(UB500, &transport, &firmware_with(image(64)))
            .await
            .unwrap();
        let opcodes = transport.sent_commands();
        let version = opcodes.iter().position(|o| *o == READ_ROM_VERSION).unwrap();
        let first_download = opcodes.iter().position(|o| *o == DOWNLOAD).unwrap();
        assert!(version < first_download);
    }

    #[test]
    fn the_loader_claims_rebadged_parts_not_just_realtek_branded_ones() {
        // Found on hardware: the TP-Link UB500 is an RTL8761BU that reports TP-Link's
        // vendor id. Matching on 0x0BDA alone left it falling through to NoInit, which
        // does nothing — invisible on Linux, fatal on Windows.
        let rtl = RealtekInit;
        assert!(rtl.matches(UsbId::new(0x2357, 0x0604)), "TP-Link UB500");
        assert!(rtl.matches(UsbId::new(0x0bda, 0x8771)), "Realtek reference");
        assert!(rtl.matches(UsbId::new(0x0b05, 0x190e)), "ASUS BT500");

        assert!(!rtl.matches(UsbId::new(0x0bda, 0x0001)));
        assert!(
            !rtl.matches(UsbId::new(0x8087, 0x0029)),
            "that one is Intel's"
        );
    }
}
