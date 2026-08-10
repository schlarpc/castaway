//! Controller initialisation: bringing a cold radio to the point where `HCI_Reset` works.
//!
//! Separate from [`HciTransport`] because moving packets is vendor-neutral and waking a
//! controller is not. Most modern parts ship with no usable ROM image and depend on the
//! OS driver uploading firmware at probe; under WinUSB nothing does, so the chip's
//! firmware protocol is ours, and it differs per vendor (architecture §11.3a).
//!
//! Two implementations from the start, deliberately: a seam with one implementation has
//! never been tested as a seam. [`NoInit`] is the third and is a *correct answer* for
//! ROM-based parts like the CSR8510 rather than a fallback.
//!
//! Every loader is written against [`HciTransport`], so it runs unchanged against a
//! `ScriptedTransport` — which is how the chunking and sequencing below are tested with
//! no radio present.

use std::fmt;

use substrate_hci::HciTransport;

use crate::error::TransportError;
use crate::firmware::FirmwareSet;

pub mod intel;
pub mod realtek;

pub use intel::IntelInit;
pub use realtek::RealtekInit;

/// A USB vendor/product pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UsbId {
    /// Vendor id.
    pub vendor: u16,
    /// Product id.
    pub product: u16,
}

impl UsbId {
    /// Build an id.
    #[must_use]
    pub const fn new(vendor: u16, product: u16) -> Self {
        Self { vendor, product }
    }
}

impl fmt::Display for UsbId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor, self.product)
    }
}

/// Brings a cold controller to the point where `HCI_Reset` will work.
#[async_trait::async_trait]
pub trait ControllerInit: Send + Sync {
    /// A short name for logs.
    fn name(&self) -> &'static str;

    /// Whether this initialiser handles the device at this USB id.
    fn matches(&self, id: UsbId) -> bool;

    /// Which firmware images this controller will need, so a build missing one can say
    /// so before touching the radio rather than half-way through an upload.
    fn required_images(&self, _id: UsbId) -> Vec<&'static str> {
        Vec::new()
    }

    /// Upload firmware and apply any vendor configuration.
    ///
    /// `id` is the USB id the loader matched, because which *image* a part needs is not
    /// something the loader knows from its own type: Intel's AX200 and AX210 share a
    /// loader and take different signed images, and sending the wrong one to a
    /// secure-boot part is the worst outcome available here.
    ///
    /// Must be idempotent in the sense that running against an already-initialised
    /// controller is a no-op, not a failure: a warm reboot leaves the part in operational
    /// mode and re-uploading is neither possible nor needed.
    ///
    /// # Errors
    /// [`TransportError`] if the controller refuses a step or an image is missing.
    async fn init(
        &self,
        id: UsbId,
        hci: &dyn HciTransport,
        firmware: &FirmwareSet,
    ) -> Result<(), TransportError>;
}

/// Parts that genuinely run from ROM and need no upload.
///
/// The distinction this list exists to draw: for a CSR8510, "no firmware" is the right
/// answer and a silent one is correct. For anything else reaching [`NoInit`], it is a
/// guess — and on Windows, where nothing else loads firmware, a wrong guess is a radio
/// that enumerates, answers `HCI_Reset` from its bootloader, and then does nothing at all.
const ROM_BASED: &[(u16, u16)] = &[
    (0x0A12, 0x0001), // CSR8510 and the flood of clones
    (0x0BDA, 0x8771), // handled by the Realtek loader, listed so a fallback is not silent
];

/// For controllers that need nothing — a correct answer for some parts, a guess for the
/// rest, and it now says which.
///
/// The CSR8510 and its clones run entirely from ROM. Treating "no firmware" as an error
/// would refuse a part that works perfectly, which is why this is the last entry in
/// [`registry`] rather than an error. But it used to be *silent* for everything, so
/// plugging in a MediaTek or Broadcom dongle logged "initialising controller loader=rom"
/// and produced an inert radio with nothing pointing at the cause.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoInit;

#[async_trait::async_trait]
impl ControllerInit for NoInit {
    fn name(&self) -> &'static str {
        "rom"
    }

    fn matches(&self, _id: UsbId) -> bool {
        true
    }

    async fn init(
        &self,
        id: UsbId,
        _hci: &dyn HciTransport,
        _firmware: &FirmwareSet,
    ) -> Result<(), TransportError> {
        if !ROM_BASED
            .iter()
            .any(|(vendor, product)| *vendor == id.vendor && *product == id.product)
        {
            // Loud, because the failure downstream is silent. If this part does need
            // firmware, everything from here looks fine — it enumerates, it answers
            // HCI_Reset — and no phone ever connects.
            tracing::warn!(
                %id,
                "no firmware loader for this controller; assuming it runs from ROM. \
                 If Bluetooth does not work, this is the first thing to doubt — \
                 supported: Intel AX200/AX201/AX210/AX211, Realtek RTL8761B/BU"
            );
        }
        Ok(())
    }
}

/// The initialisers, tried in order.
///
/// [`NoInit`] matches everything, so it must stay last — and being last is what makes it
/// the answer for parts nobody wrote a loader for, which is right for ROM-based ones and
/// merely optimistic for the rest. [`registry_strict`] is the version that says no.
#[must_use]
pub fn registry() -> Vec<Box<dyn ControllerInit>> {
    vec![Box::new(IntelInit), Box::new(RealtekInit), Box::new(NoInit)]
}

/// The initialisers, without the catch-all.
///
/// Use this when an unknown controller should be a clear error rather than a part that
/// enumerates, accepts `HCI_Reset`, and then behaves oddly for reasons nobody can see.
#[must_use]
pub fn registry_strict() -> Vec<Box<dyn ControllerInit>> {
    vec![Box::new(IntelInit), Box::new(RealtekInit)]
}

/// What to do with a controller no firmware loader claims (#91).
///
/// This is the [`registry`]/[`registry_strict`] choice as a value, so the app config can
/// carry it to [`registry_for`] without knowing which loaders exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnknownControllerPolicy {
    /// Assume it runs from ROM: [`NoInit`] takes it, silently for the parts on its
    /// allow-list and loudly for everything else. Right for a box that must come up
    /// with whatever dongle it finds.
    #[default]
    AssumeRom,
    /// Refuse it at startup with [`TransportError::UnsupportedController`]. Right for a
    /// box known to have supported hardware, where "the radio came up inert" should be
    /// impossible rather than a warning in a log nobody reads. Note this refuses
    /// ROM-based parts (the CSR8510) too: strict means *a loader claims it*, and no
    /// loader claims a part that needs nothing.
    Refuse,
}

/// The initialisers the policy asks for.
#[must_use]
pub fn registry_for(policy: UnknownControllerPolicy) -> Vec<Box<dyn ControllerInit>> {
    match policy {
        UnknownControllerPolicy::AssumeRom => registry(),
        UnknownControllerPolicy::Refuse => registry_strict(),
    }
}

/// Whether a non-catch-all loader claims this id — that is, whether we can actually
/// *drive* the part rather than merely hope it runs from ROM.
///
/// This is what `open_first` prefers when enumeration order is the only other input
/// (#91): on a bench with an AX200 and an unknown dongle, the AX200 should win no matter
/// which the bus lists first.
#[must_use]
pub fn has_dedicated_loader(id: UsbId) -> bool {
    registry_strict().iter().any(|init| init.matches(id))
}

/// Pick the initialiser for a controller.
///
/// # Errors
/// [`TransportError::UnsupportedController`] if nothing in `registry` matches.
pub fn select(
    registry: Vec<Box<dyn ControllerInit>>,
    id: UsbId,
) -> Result<Box<dyn ControllerInit>, TransportError> {
    registry
        .into_iter()
        .find(|init| init.matches(id))
        .ok_or(TransportError::UnsupportedController(id))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The AX200 in the dev box.
    const AX200: UsbId = UsbId::new(0x8087, 0x0029);
    /// A TP-Link UB500 and the flood of identical dongles.
    const RTL8761BU: UsbId = UsbId::new(0x0bda, 0x8771);
    /// A CSR8510 clone: ROM-based, needs nothing.
    const CSR8510: UsbId = UsbId::new(0x0a12, 0x0001);

    /// The name of whichever loader claims `id`.
    fn picked(id: UsbId) -> &'static str {
        match select(registry(), id) {
            Ok(init) => init.name(),
            Err(e) => panic!("nothing claimed {id}: {e}"),
        }
    }

    #[test]
    fn each_controller_selects_its_own_loader() {
        assert_eq!(picked(AX200), "intel");
        assert_eq!(picked(RTL8761BU), "realtek");
        assert_eq!(picked(CSR8510), "rom");
    }

    #[test]
    fn the_catch_all_is_last_or_it_would_shadow_every_loader() {
        // NoInit matches everything. If the ordering ever changed, an AX200 would
        // "initialise" successfully, accept HCI_Reset, and then behave oddly in ways
        // no log would explain.
        let names: Vec<&str> = registry().iter().map(|i| i.name()).collect();
        assert_eq!(names.last(), Some(&"rom"));
        assert!(names.len() >= 3, "a seam with one impl is not a seam");
    }

    #[test]
    fn strict_mode_refuses_a_controller_nobody_wrote_a_loader_for() {
        let Err(err) = select(registry_strict(), CSR8510) else {
            panic!("strict mode must refuse an unknown controller");
        };
        assert!(format!("{err}").contains("0a12:0001"), "got: {err}");
    }

    #[test]
    fn the_policy_selects_the_registry_it_names() {
        // AssumeRom is today's behaviour: everything initialises, NoInit last.
        let lenient: Vec<&str> = registry_for(UnknownControllerPolicy::AssumeRom)
            .iter()
            .map(|i| i.name())
            .collect();
        assert_eq!(lenient.last(), Some(&"rom"));

        // Refuse is registry_strict: an unknown part is a startup error naming the id,
        // not a radio that enumerates and then does nothing.
        let Err(err) = select(registry_for(UnknownControllerPolicy::Refuse), CSR8510) else {
            panic!("refuse policy must refuse a part no loader claims");
        };
        assert!(
            matches!(err, TransportError::UnsupportedController(id) if id == CSR8510),
            "got: {err}"
        );
        // And it must still accept the hardware the loaders exist for.
        assert!(select(registry_for(UnknownControllerPolicy::Refuse), AX200).is_ok());
    }

    #[test]
    fn a_dedicated_loader_is_one_that_is_not_the_catch_all() {
        assert!(has_dedicated_loader(AX200));
        assert!(has_dedicated_loader(RTL8761BU));
        // The CSR8510 works, but from ROM: nothing *drives* it, so it earns no
        // preference over enumeration order.
        assert!(!has_dedicated_loader(CSR8510));
        // An arbitrary unknown dongle.
        assert!(!has_dedicated_loader(UsbId::new(0x0e8d, 0x0616)));
    }

    #[test]
    fn loaders_declare_the_images_they_need_before_touching_the_radio() {
        // So a build missing a blob fails at startup naming the file, rather than
        // half-way through an upload with the part in bootloader mode.
        let images = |id| match select(registry(), id) {
            Ok(init) => init.required_images(id),
            Err(e) => panic!("nothing claimed {id}: {e}"),
        };
        assert!(
            images(AX200).iter().any(|n| n.ends_with(".sfi")),
            "intel must declare its .sfi"
        );
        assert!(
            images(RTL8761BU).iter().any(|n| n.contains("rtl")),
            "realtek must declare its firmware"
        );
        assert!(images(CSR8510).is_empty());
    }

    #[test]
    fn usb_ids_render_the_way_lsusb_prints_them() {
        assert_eq!(AX200.to_string(), "8087:0029");
    }

    #[tokio::test]
    async fn a_rom_based_part_initialises_by_doing_nothing() {
        let transport = substrate_hci::ScriptedTransport::new();
        NoInit
            .init(CSR8510, &transport, &FirmwareSet::new())
            .await
            .unwrap();
        assert!(
            transport.sent().is_empty(),
            "a ROM part must not be sent vendor commands it will reject"
        );
    }
}
