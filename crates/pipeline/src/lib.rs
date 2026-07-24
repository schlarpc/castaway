//! # pipeline
//!
//! The media/render pipeline. Today it ships the [`NullPipeline`] (log + drain) that the
//! whole protocol stack runs against on the Linux dev box with no GPU/codec present, and
//! the backend-agnostic [`compositor`] and [`browser`] trait surfaces the real backends
//! slot behind feature flags:
//!
//! - `ffmpeg` — libav decode (d3d11va/vaapi) → frames.
//! - `wgpu` — the [`compositor::Compositor`] impl (DX12/Vulkan), layers + PiP.
//! - `cef` — the [`browser::BrowserSurface`] impl (offscreen YouTube TV surface / PiP).
//!
//! This crate is where `unsafe` FFI *would* live, so unlike the pure crates it does not
//! `forbid(unsafe_code)`; the null backend uses none, and any real backend's `unsafe`
//! must carry a `// SAFETY:` note (ground rule 8).

pub mod browser;
pub mod compositor;
pub mod error;
pub mod null;

#[cfg(feature = "render")]
pub mod attract;
#[cfg(feature = "cef")]
pub mod cef_adblock;
#[cfg(feature = "cef")]
pub mod cef_browser;
#[cfg(feature = "cef")]
pub mod easylist;
#[cfg(feature = "ffmpeg")]
pub mod ffmpeg_decode;
#[cfg(feature = "kiosk")]
pub mod kiosk;
#[cfg(feature = "render")]
pub mod osd;
#[cfg(feature = "render")]
pub mod render_pipeline;
#[cfg(feature = "render")]
pub mod text;
#[cfg(feature = "render")]
pub mod wgpu_compositor;

pub use browser::{BrowserSurface, NullBrowser};
pub use compositor::{Compositor, Layer, LayerId, NullCompositor, Transform};
pub use error::PipelineError;
pub use null::NullPipeline;

#[cfg(feature = "render")]
pub use attract::{AttractRow, AttractScene};
#[cfg(feature = "render")]
pub use osd::{OsdController, OsdUpdate};
#[cfg(feature = "render")]
pub use render_pipeline::{RenderCommand, RenderLoop, RenderPipeline};
#[cfg(feature = "render")]
pub use wgpu_compositor::WgpuCompositor;
