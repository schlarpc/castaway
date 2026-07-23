//! # control-display
//!
//! Drives the Dell C6522QT panel. The session manager fires `power_on` /
//! `select_input` on session start (architecture §8). Backends sit behind a trait:
//! [`NullDisplay`] (logs; default on a headless dev box), RS-232 (`serial` feature,
//! primary on the panel), and DDC/CI (`ddc` feature). CEC is intentionally absent — a
//! commercial panel doesn't need it.
//!
//! This crate may use `unsafe` for FFI (serialport/i2c), so it does not
//! `forbid(unsafe_code)`; any `unsafe` block must carry a `// SAFETY:` note (rule 8).

pub mod dell;

use async_trait::async_trait;
use castaway_core::{CoreError, DisplayControl, DisplayInput};
use tracing::info;

pub use dell::DellCommand;

/// A display-control backend that logs commands — used when no serial/DDC hardware is
/// wired (the dev box), and as the safe default.
#[derive(Default)]
pub struct NullDisplay;

#[async_trait]
impl DisplayControl for NullDisplay {
    async fn power_on(&self) -> Result<(), CoreError> {
        info!(frame = ?DellCommand::PowerOn.encode(), "display: power on (null)");
        Ok(())
    }
    async fn power_off(&self) -> Result<(), CoreError> {
        info!("display: power off (null)");
        Ok(())
    }
    async fn select_input(&self, input: DisplayInput) -> Result<(), CoreError> {
        info!(?input, "display: select input (null)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn null_display_accepts_commands() {
        let d = NullDisplay;
        d.power_on().await.unwrap();
        d.select_input(DisplayInput::Hdmi1).await.unwrap();
        d.power_off().await.unwrap();
    }
}
