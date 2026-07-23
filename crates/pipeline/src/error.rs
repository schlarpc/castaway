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

    /// A frame's dimensions or format weren't usable.
    #[error("invalid frame: {0}")]
    InvalidFrame(&'static str),
}
