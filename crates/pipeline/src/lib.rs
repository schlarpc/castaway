//! # pipeline
//!
//! The media/render pipeline. Today it ships the [`NullPipeline`] (log + drain) that the
//! whole protocol stack runs against on the Linux dev box with no GPU/codec present, and
//! the backend-agnostic [`compositor`] and [`browser`] trait surfaces the real backends
//! slot behind feature flags:
//!
//! - `ffmpeg` — libav decode (d3d11va/vaapi) → frames.
//! - `wgpu` — the [`compositor::Compositor`] impl (DX12/Vulkan), layers + PiP.
//! - `electron` — the browser subprocess (offscreen YouTube TV surface / PiP), D36.
//!
//! This crate is where `unsafe` FFI *would* live, so unlike the pure crates it does not
//! `forbid(unsafe_code)`; the null backend uses none, and any real backend's `unsafe`
//! must carry a `// SAFETY:` note (ground rule 8).

#[cfg(feature = "audio")]
pub mod audio_decode;
#[cfg(feature = "audio")]
pub mod audio_out;
#[cfg(all(feature = "audio-pipewire", target_os = "linux"))]
pub mod audio_pw;
// Not gated behind `audio`, for the same reason `theme` is not behind `render`: the
// output-device selection is config-file surface, and config parses in every build.
pub mod audio_select;
#[cfg(feature = "audio")]
pub mod audio_session;
pub mod browser;
// The browser subprocess protocol (D36). Pure and always compiled, so the wire types
// are fixture-tested in every build rather than only where a browser exists.
pub mod browser_proto;
pub mod color;
pub mod compositor;
pub mod error;
pub mod hwaccel;
pub mod null;

#[cfg(feature = "electron")]
pub mod adblock_engine;
#[cfg(feature = "render")]
pub mod attract;
pub mod clock;
#[cfg(feature = "electron")]
pub mod electron_browser;
#[cfg(feature = "ffmpeg")]
pub mod ffmpeg_decode;
#[cfg(feature = "electron")]
pub mod filterlists;
#[cfg(feature = "kiosk")]
pub mod kiosk;
#[cfg(feature = "render")]
pub mod nowplaying_card;
#[cfg(feature = "render")]
pub mod osd;
#[cfg(feature = "render")]
pub mod overlay;
#[cfg(feature = "render")]
pub mod picker;
#[cfg(feature = "render")]
pub mod render_pipeline;
pub mod seek;
#[cfg(feature = "render")]
pub mod service;
pub mod shape;
#[cfg(feature = "render")]
pub mod shell;
#[cfg(feature = "render")]
pub mod tap;
#[cfg(feature = "render")]
pub mod text;
// Not gated behind `render`, unlike every other drawing module: `ThemeChoice` is a field
// in the app's config file, which has to parse on a build with no renderer in it at all.
// Nothing here draws — it is colour constants and the calendar.
pub mod theme;
pub mod transport;
#[cfg(feature = "electron")]
pub mod ubo_scriptlets;
#[cfg(feature = "render")]
pub mod wgpu_compositor;
#[cfg(feature = "electron")]
pub use browser::{BrowserCommand, BrowserRole, BrowserSurface, BrowserView, NullBrowser};
pub use browser_proto::Surface as BrowserWindowSurface;
pub use color::YuvMatrix;
pub use compositor::{Compositor, Layer, LayerId, NullCompositor, Transform};
pub use error::PipelineError;
pub use hwaccel::{DecodePath, FallbackPolicy, HwBackendKind, HwGiveUp, HwPreference};
pub use null::NullPipeline;

#[cfg(feature = "render")]
pub use attract::{AttractScene, InsetRect, ServiceDetail, Tile, TileGlyph, WidgetSlot};
#[cfg(feature = "electron")]
pub use electron_browser::{Electron, ElectronHost, TV_USER_AGENT};
#[cfg(feature = "render")]
pub use osd::{Banner, OsdController, OsdUpdate};
#[cfg(feature = "render")]
pub use render_pipeline::{PlaybackHandle, ScreenshotHandle};
#[cfg(feature = "render")]
pub use render_pipeline::{RenderCommand, RenderLoop, RenderPipeline};
#[cfg(feature = "render")]
pub use wgpu_compositor::WgpuCompositor;
