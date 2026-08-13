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

    /// A remote-control peer connection could not be set up or kept (#18).
    #[error("remote: {0}")]
    Remote(String),

    /// The decoder could not open or decode the input.
    #[error("decode error: {0}")]
    Decode(String),

    /// The media could not be *got*: the connection was refused, the server answered
    /// 404 or 403, the file is not there (#341).
    ///
    /// Split out of [`Self::Decode`] because it is the difference between "your link is
    /// dead" and "this box cannot play that", and every sender protocol has separate
    /// words for the two — a receiver that answers "internal error" to a 404 sends
    /// somebody to debug the panel.
    #[error("fetch error: {0}")]
    Fetch(String),

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

    /// The output duplicate could not be produced or described (#101). Never fatal to the
    /// panel — the glass keeps presenting whatever the stream is doing — so this is
    /// carried back to whoever asked for the stream rather than escalated.
    #[error("stream: {0}")]
    Stream(String),

    /// An encoder could not be opened, or refused a frame.
    #[error("encode: {0}")]
    Encode(String),

    /// The Widevine CDM could not be pointed out to the browser. Never fatal — the panel
    /// runs, it just cannot play EME-gated video — but reported rather than swallowed,
    /// because that failure is otherwise indistinguishable from a network problem.
    #[error("widevine: {0}")]
    Widevine(String),
}
