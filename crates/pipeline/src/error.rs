//! Pipeline errors.

use thiserror::Error;

/// Failures in the render/decode pipeline.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PipelineError {
    /// No suitable GPU adapter/device could be acquired.
    #[error("gpu init failed: {0}")]
    GpuInit(String),

    /// A surface/swapchain operation failed.
    #[error("surface error: {0}")]
    Surface(String),

    /// The decoder could not open or decode the input.
    #[error("decode error: {0}")]
    Decode(String),

    /// Audio decode or output failed.
    #[error("audio: {0}")]
    Audio(String),

    /// A frame's dimensions or format weren't usable.
    #[error("invalid frame: {0}")]
    InvalidFrame(&'static str),

    /// A hardware decoder could not be set up, or lost its device mid-session. Always
    /// recoverable: the decode loop answers this by falling back to software.
    #[error("hardware decode unavailable: {0}")]
    HwDecode(String),

    /// A GPU surface could not be imported into the compositor's device.
    #[error("gpu surface import failed: {0}")]
    GpuImport(String),
}
