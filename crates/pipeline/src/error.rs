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

    /// uBlock Origin's scriptlet modules could not be evaluated into resources.
    ///
    /// Distinct from "no scriptlets": an empty set looks identical to a working one from
    /// the outside, so a graph that fails to run says so rather than quietly injecting
    /// nothing.
    #[error("scriptlet conversion failed: {0}")]
    Scriptlets(String),

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

    /// The Widevine CDM could not be pointed out to the browser. Never fatal — the panel
    /// runs, it just cannot play EME-gated video — but reported rather than swallowed,
    /// because that failure is otherwise indistinguishable from a network problem.
    #[error("widevine: {0}")]
    Widevine(String),
}
