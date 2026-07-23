//! Display-control abstraction. The session manager fires power/input commands on
//! session start; backends (RS-232, DDC/CI) live in the `control-display` crate.

use crate::error::CoreError;

/// A physical input on the display. The Dell C6522QT selects these via RS-232 or
/// DDC/CI VCP `0x60`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DisplayInput {
    /// HDMI 1.
    Hdmi1,
    /// HDMI 2.
    Hdmi2,
    /// DisplayPort.
    DisplayPort,
    /// USB-C (DP-alt) — the single-cable path on the C6522QT.
    UsbC,
}

/// Controls the physical panel: power and input-source selection. Modeled as a trait
/// so RS-232 and DDC backends are swappable, and so a headless dev box can use a null
/// backend that just logs (ground rule 5).
#[async_trait::async_trait]
pub trait DisplayControl: Send + Sync {
    /// Power the panel on (session start).
    ///
    /// # Errors
    /// Returns [`CoreError::Display`] if the control channel fails.
    async fn power_on(&self) -> Result<(), CoreError>;

    /// Power the panel off (idle timeout).
    ///
    /// # Errors
    /// Returns [`CoreError::Display`] if the control channel fails.
    async fn power_off(&self) -> Result<(), CoreError>;

    /// Select the input the receiver's HDMI/USB-C output is wired to.
    ///
    /// # Errors
    /// Returns [`CoreError::Display`] if the control channel fails.
    async fn select_input(&self, input: DisplayInput) -> Result<(), CoreError>;
}
