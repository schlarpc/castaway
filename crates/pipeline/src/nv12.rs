//! RGBA → NV12 on the GPU: the inverse of what the compositor already does on the way in.
//!
//! Video encoders want NV12 — a full-resolution luma plane and a half-resolution
//! interleaved chroma plane — and the compositor renders RGBA. Converting that on the CPU
//! through swscale would cost more than the encode does and would waste the whole point of
//! reading back at all, so it happens here, in two render passes, straight off the
//! composited scene texture.
//!
//! Three things this buys beyond "not swscale":
//!
//! - **Half the readback.** NV12 is 1.5 bytes a pixel where RGBA is 4. At 1080p that is
//!   3.1 MB a frame instead of 8.3.
//! - **Free downscaling.** The planes are rendered at whatever size the stream wants, and
//!   the sampler was going to filter regardless — so a 4K panel streaming at 1080p costs
//!   less than a 1080p one streaming at 1080p, not more.
//! - **The right colour.** [`crate::color`] derives the YUV→RGB matrix for decoded video;
//!   this is the same derivation run backwards, in the same place, for the same reason —
//!   a stream that is quietly BT.601 looks merely cheap and nobody files a bug for it.
//!
//! The scene is sampled through a **non-sRGB view** of an sRGB texture, so what the shader
//! reads is the display-referred byte the panel is showing rather than its linearisation.
//! BT.709's matrix is defined on gamma-encoded R'G'B'; handing it linear light would
//! produce a picture that is washed out in exactly the way that survives review.

use crate::error::PipelineError;

/// The bytes a `copy_texture_to_buffer` row must start on.
const ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

const SHADER: &str = r#"
// The half-tap offsets, in source texture coordinates. Zero when the stream is the
// panel's own size, in which case the four taps coincide and this is one sample.
struct Taps { offset: vec2<f32>, _pad: vec2<f32> };

@group(0) @binding(0) var scene: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<uniform> taps: Taps;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

// One oversized triangle rather than two triangles: no shared edge to crack, and the
// rasterizer clips the excess for free.
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0),
    );
    let xy = corners[vi];
    var out: VsOut;
    out.pos = vec4<f32>(xy, 0.0, 1.0);
    // Clip space is Y-up, texture coordinates are Y-down.
    out.uv = vec2<f32>((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return out;
}

// Box-filter the source under this destination pixel.
//
// A bilinear tap covers a 2x2 source neighbourhood, so four of them at the destination
// pixel's quarter offsets cover 4x4 — which is what a 4:1 downscale needs and what a
// single tap would alias badly on. Text on a shell screen is the case that shows it.
fn sample_box(uv: vec2<f32>, offset: vec2<f32>) -> vec3<f32> {
    var acc = vec3<f32>(0.0);
    acc += textureSample(scene, smp, uv + vec2<f32>(-offset.x, -offset.y)).rgb;
    acc += textureSample(scene, smp, uv + vec2<f32>( offset.x, -offset.y)).rgb;
    acc += textureSample(scene, smp, uv + vec2<f32>(-offset.x,  offset.y)).rgb;
    acc += textureSample(scene, smp, uv + vec2<f32>( offset.x,  offset.y)).rgb;
    return acc * 0.25;
}

// BT.709 luma coefficients. Spelled out rather than derived at runtime because there is
// exactly one answer here: the stream is opened as BT.709 and the `colr` box in the init
// segment says so, so this constant and that box are the same decision written twice.
const KR: f32 = 0.2126;
const KB: f32 = 0.0722;

// R'G'B' (gamma-encoded, 0..1) to Y'CbCr with Cb/Cr centred on zero.
fn to_yuv(c: vec3<f32>) -> vec3<f32> {
    let kg = 1.0 - KR - KB;
    let y = KR * c.r + kg * c.g + KB * c.b;
    return vec3<f32>(y, (c.b - y) / (2.0 * (1.0 - KB)), (c.r - y) / (2.0 * (1.0 - KR)));
}

// Studio range. Video that omits the flag is limited-range video, players assume it, and
// writing full-range samples into a stream that declares limited crushes the blacks and
// clips the whites — visible, and attributable to anything.
@fragment
fn fs_luma(in: VsOut) -> @location(0) f32 {
    let yuv = to_yuv(sample_box(in.uv, taps.offset));
    return (16.0 + 219.0 * clamp(yuv.x, 0.0, 1.0)) / 255.0;
}

@fragment
fn fs_chroma(in: VsOut) -> @location(0) vec2<f32> {
    let yuv = to_yuv(sample_box(in.uv, taps.offset));
    return vec2<f32>(
        (128.0 + 224.0 * clamp(yuv.y, -0.5, 0.5)) / 255.0,
        (128.0 + 224.0 * clamp(yuv.z, -0.5, 0.5)) / 255.0,
    );
}
"#;

/// One converted frame, as it comes back from the GPU.
///
/// The strides are the GPU's, not the picture's: `copy_texture_to_buffer` rounds every row
/// up to [`ROW_ALIGN`], and unpacking that here would be a second full-frame copy for
/// nothing — libavcodec takes a `linesize` per plane and is happy to be told.
#[derive(Debug, Clone)]
pub struct Nv12Planes {
    /// Coded width in pixels. Always even.
    pub width: u32,
    /// Coded height in pixels. Always even.
    pub height: u32,
    /// Both planes, luma first.
    pub data: Vec<u8>,
    /// Bytes between luma rows.
    pub y_stride: u32,
    /// Where the interleaved chroma plane starts.
    pub uv_offset: usize,
    /// Bytes between chroma rows.
    pub uv_stride: u32,
}

impl Nv12Planes {
    /// The luma plane.
    #[must_use]
    pub fn luma(&self) -> &[u8] {
        &self.data[..self.uv_offset.min(self.data.len())]
    }

    /// The interleaved chroma plane.
    #[must_use]
    pub fn chroma(&self) -> &[u8] {
        &self.data[self.uv_offset.min(self.data.len())..]
    }
}

/// The two render passes and the buffers they read back through.
///
/// Built once and resized on demand: the target size only changes when the panel does,
/// and rebuilding pipelines per frame would be most of the cost of the conversion.
pub struct Nv12Converter {
    luma: wgpu::RenderPipeline,
    chroma: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    sized: Option<Sized>,
}

impl std::fmt::Debug for Nv12Converter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nv12Converter")
            .field("size", &self.sized.as_ref().map(|s| (s.width, s.height)))
            .finish_non_exhaustive()
    }
}

/// Everything that depends on the output size.
struct Sized {
    width: u32,
    height: u32,
    /// The source size the tap offsets were computed for. A panel resize with the stream
    /// size unchanged still changes the filter.
    source: (u32, u32),
    y: wgpu::Texture,
    uv: wgpu::Texture,
    y_taps: wgpu::Buffer,
    uv_taps: wgpu::Buffer,
    readback: wgpu::Buffer,
    y_stride: u32,
    uv_stride: u32,
    uv_offset: usize,
}

impl Nv12Converter {
    /// Build the conversion pipelines on a device.
    #[must_use]
    pub fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let plane = |entry: &str, format: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("nv12-plane"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_main",
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: entry,
                    targets: &[Some(format.into())],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        Self {
            luma: plane("fs_luma", wgpu::TextureFormat::R8Unorm),
            chroma: plane("fs_chroma", wgpu::TextureFormat::Rg8Unorm),
            layout,
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("nv12-sampler"),
                // Clamped, so the tap offsets at the frame's edge fold back onto the edge
                // texel rather than wrapping to the opposite side of the picture.
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            sized: None,
        }
    }

    /// Convert one composited scene into NV12 planes at `width`×`height`.
    ///
    /// `scene` must be a view of the composited target in a **non-sRGB** format — see the
    /// module docs for why that is not an implementation detail.
    ///
    /// # Errors
    /// [`PipelineError::Stream`] if the requested size is not a usable 4:2:0 size, or the
    /// readback map fails.
    pub fn convert(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene: &wgpu::TextureView,
        source: (u32, u32),
        width: u32,
        height: u32,
    ) -> Result<Nv12Planes, PipelineError> {
        if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(PipelineError::Stream(format!(
                "{width}x{height} is not a 4:2:0 size"
            )));
        }
        self.resize(device, queue, source, width, height);
        let Some(sized) = self.sized.as_ref() else {
            return Err(PipelineError::Stream("no conversion targets".into()));
        };

        let bind = |taps: &wgpu::Buffer| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("nv12-bind"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(scene),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: taps.as_entire_binding(),
                    },
                ],
            })
        };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("nv12"),
        });
        for (texture, pipeline, taps) in [
            (&sized.y, &self.luma, &sized.y_taps),
            (&sized.uv, &self.chroma, &sized.uv_taps),
        ] {
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let group = bind(taps);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nv12-plane"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The triangle covers every pixel, so the previous contents are
                        // never read and a discard would do — except wgpu has no
                        // `LoadOp::Discard` (only `StoreOp` has one). Clear is the
                        // cheapest thing the API can express here.
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }

        // Both planes into one buffer, so the frame costs one map and one wait rather than
        // two of each.
        for (texture, offset, stride, rows) in [
            (&sized.y, 0u64, sized.y_stride, height),
            (
                &sized.uv,
                sized.uv_offset as u64,
                sized.uv_stride,
                height / 2,
            ),
        ] {
            encoder.copy_texture_to_buffer(
                wgpu::ImageCopyTexture {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::ImageCopyBuffer {
                    buffer: &sized.readback,
                    layout: wgpu::ImageDataLayout {
                        offset,
                        bytes_per_row: Some(stride),
                        rows_per_image: Some(rows),
                    },
                },
                wgpu::Extent3d {
                    width: texture.width(),
                    height: texture.height(),
                    depth_or_array_layers: 1,
                },
            );
        }
        queue.submit(Some(encoder.finish()));

        let slice = sized.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|_| PipelineError::Stream("nv12 map channel closed".into()))?
            .map_err(|e| PipelineError::Stream(format!("nv12 readback: {e:?}")))?;
        let data = slice.get_mapped_range().to_vec();
        sized.readback.unmap();

        Ok(Nv12Planes {
            width,
            height,
            data,
            y_stride: sized.y_stride,
            uv_offset: sized.uv_offset,
            uv_stride: sized.uv_stride,
        })
    }

    /// Rebuild the size-dependent resources if the shape of the job changed.
    fn resize(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: (u32, u32),
        width: u32,
        height: u32,
    ) {
        if self
            .sized
            .as_ref()
            .is_some_and(|s| (s.width, s.height, s.source) == (width, height, source))
        {
            return;
        }
        let plane = |label, w: u32, h: u32, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let y_stride = width.div_ceil(ROW_ALIGN) * ROW_ALIGN;
        // The chroma plane is half as wide in texels and twice as wide per texel, so its
        // unpadded row is the same `width` bytes as luma's.
        let uv_stride = y_stride;
        let uv_offset = (y_stride * height) as usize;

        let taps = |label, dest: (u32, u32)| {
            // Half a destination pixel, in source texture coordinates — but only when
            // there is something to filter. At 1:1 the four taps would fold a quarter of
            // a pixel of blur into text for no benefit.
            let offset = |dst: u32, src: u32| {
                if dst >= src {
                    0.0f32
                } else {
                    0.25 / dst as f32
                }
            };
            device
                .create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size: 16,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
                .tap_write(queue, [offset(dest.0, source.0), offset(dest.1, source.1)])
        };

        self.sized = Some(Sized {
            width,
            height,
            source,
            y: plane("nv12-y", width, height, wgpu::TextureFormat::R8Unorm),
            uv: plane(
                "nv12-uv",
                width / 2,
                height / 2,
                wgpu::TextureFormat::Rg8Unorm,
            ),
            y_taps: taps("nv12-y-taps", (width, height)),
            uv_taps: taps("nv12-uv-taps", (width / 2, height / 2)),
            readback: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("nv12-readback"),
                size: u64::from(y_stride) * u64::from(height)
                    + u64::from(uv_stride) * u64::from(height / 2),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
            y_stride,
            uv_stride,
            uv_offset,
        });
    }
}

/// Fill a freshly created tap uniform and hand it back, so the construction above reads as
/// one expression rather than a create-then-write pair per plane.
trait TapWrite {
    fn tap_write(self, queue: &wgpu::Queue, offset: [f32; 2]) -> Self;
}

impl TapWrite for wgpu::Buffer {
    fn tap_write(self, queue: &wgpu::Queue, offset: [f32; 2]) -> Self {
        queue.write_buffer(
            &self,
            0,
            bytemuck::bytes_of(&[offset[0], offset[1], 0.0, 0.0]),
        );
        self
    }
}

/// How large a readback buffer a given output needs. Exposed so a caller can decide
/// whether a stream size is affordable before it opens an encoder for it.
#[must_use]
pub fn readback_bytes(width: u32, height: u32) -> u64 {
    let stride = u64::from(width.div_ceil(ROW_ALIGN) * ROW_ALIGN);
    stride * u64::from(height) + stride * u64::from(height / 2)
}

#[cfg(test)]
mod gpu {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::compositor::{Compositor as _, Layer, LayerId, Transform};
    use crate::tap::{CapturedFrame, FrameWant};
    use crate::wgpu_compositor::{TexelFormat, WgpuCompositor};

    /// Compose a solid colour and convert it, or `None` where there is no GPU.
    fn solid(
        colour: [u8; 4],
        format: TexelFormat,
        panel: (u32, u32),
        stream: (u32, u32),
    ) -> Option<Nv12Planes> {
        let mut compositor = WgpuCompositor::new_offscreen(panel.0, panel.1).ok()?;
        let pixels: Vec<u8> = std::iter::repeat_n(colour, (panel.0 * panel.1) as usize)
            .flatten()
            .collect();
        compositor
            .upload_texture(LayerId::Attract, panel.0, panel.1, format, &pixels)
            .unwrap();
        compositor.upsert_layer(Layer {
            id: LayerId::Attract,
            opacity: 1.0,
            transform: Transform::default(),
        });
        let captured = compositor.present_and_capture(&[FrameWant::Nv12 {
            width: stream.0,
            height: stream.1,
        }]);
        match captured.into_iter().next() {
            Some(Some(CapturedFrame::Nv12(planes))) => Some(planes),
            other => panic!("expected NV12 planes, got {other:?}"),
        }
    }

    /// The luma and chroma bytes at the middle of the picture.
    fn centre(planes: &Nv12Planes) -> (u8, u8, u8) {
        let y_row = (planes.height / 2) as usize * planes.y_stride as usize;
        let uv_row = (planes.height / 4) as usize * planes.uv_stride as usize;
        let x = (planes.width / 2) as usize;
        // Chroma is interleaved and half-width, so pixel `x`'s Cb byte is at `x & !1`
        // within its row and Cr is the byte after it.
        let cb = uv_row + (x & !1);
        (
            planes.luma()[y_row + x],
            planes.chroma()[cb],
            planes.chroma()[cb + 1],
        )
    }

    fn near(got: u8, want: u8, what: &str) {
        assert!(
            got.abs_diff(want) <= 2,
            "{what}: got {got}, expected about {want}"
        );
    }

    #[test]
    fn a_mid_grey_converts_from_the_byte_the_panel_shows_not_its_linearisation() {
        // The discriminating case for the whole module. Sampling the scene through its
        // sRGB view hands the shader linear light, and BT.709's matrix is defined on
        // gamma-encoded R'G'B' — so a 50% grey would come back as Y=63 instead of 126,
        // and the stream would look like a different, darker panel.
        //
        // Black and white are identical under both readings, which is exactly why the
        // test colour is neither.
        let Some(planes) = solid(
            [128, 128, 128, 255],
            TexelFormat::Rgba8Srgb,
            (64, 32),
            (64, 32),
        ) else {
            eprintln!("no GPU adapter here; skipping");
            return;
        };
        let (y, u, v) = centre(&planes);
        // 16 + 219 * (128/255), studio range.
        near(y, 126, "luma");
        near(u, 128, "Cb is neutral on grey");
        near(v, 128, "Cr is neutral on grey");
    }

    #[test]
    fn a_saturated_colour_lands_on_its_bt709_coordinates() {
        // Magenta, because a channel swap anywhere in the chain moves Cb and Cr in
        // opposite directions and is unmissable, where on grey it would be invisible.
        let Some(planes) = solid([255, 0, 255, 255], TexelFormat::Rgba8, (64, 32), (64, 32)) else {
            eprintln!("no GPU adapter here; skipping");
            return;
        };
        let (y, u, v) = centre(&planes);
        near(y, 78, "luma");
        near(u, 214, "Cb");
        near(v, 230, "Cr");
    }

    #[test]
    fn black_stays_at_the_studio_floor() {
        // Full-range maths would put this at 0, which a player told the stream is limited
        // range then stretches below black. It reads as crushed shadows and nobody
        // attributes it to the encoder.
        let Some(planes) = solid([0, 0, 0, 255], TexelFormat::Rgba8, (32, 16), (32, 16)) else {
            eprintln!("no GPU adapter here; skipping");
            return;
        };
        let (y, u, v) = centre(&planes);
        near(y, 16, "luma floor");
        near(u, 128, "Cb");
        near(v, 128, "Cr");
    }

    #[test]
    fn the_planes_come_back_at_the_stream_size_not_the_panel_size() {
        // The point of converting on the GPU rather than after the readback: a 4K panel
        // streaming at 1080p reads back a quarter of the pixels, and the sampler was
        // going to filter anyway.
        let Some(planes) = solid([255, 0, 255, 255], TexelFormat::Rgba8, (128, 64), (32, 16))
        else {
            eprintln!("no GPU adapter here; skipping");
            return;
        };
        assert_eq!((planes.width, planes.height), (32, 16));
        assert!(planes.y_stride >= 32);
        assert_eq!(planes.chroma().len(), planes.uv_stride as usize * 8);
        let (y, u, v) = centre(&planes);
        near(y, 78, "luma survives the downscale");
        near(u, 214, "Cb");
        near(v, 230, "Cr");
    }

    #[test]
    fn an_odd_size_is_refused_rather_than_encoded_wrong() {
        let Ok(mut compositor) = WgpuCompositor::new_offscreen(32, 16) else {
            eprintln!("no GPU adapter here; skipping");
            return;
        };
        let captured = compositor.present_and_capture(&[FrameWant::Nv12 {
            width: 31,
            height: 16,
        }]);
        assert!(
            matches!(captured.as_slice(), [None]),
            "4:2:0 has no way to express an odd dimension"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_is_a_third_of_the_bytes_rgba_is() {
        // The whole reason the conversion is on the GPU rather than after the readback:
        // what crosses the bus is 1.5 bytes a pixel, not 4.
        let rgba = u64::from(1920u32 * 1080 * 4);
        assert!(readback_bytes(1920, 1080) * 2 < rgba + 1024);
    }

    #[test]
    fn rows_are_aligned_the_way_a_texture_copy_demands() {
        // An unaligned `bytes_per_row` is a validation error, not a slow path, and 1366
        // is a real panel width.
        for width in [1280u32, 1366, 1920, 3840] {
            let stride = width.div_ceil(ROW_ALIGN) * ROW_ALIGN;
            assert_eq!(stride % ROW_ALIGN, 0);
            assert!(stride >= width);
        }
    }
}
