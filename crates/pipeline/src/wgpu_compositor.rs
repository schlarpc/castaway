//! A real [`Compositor`] backed by `wgpu` (Vulkan on Linux, DX12 on Windows). It owns
//! the GPU device/queue and draws each [`Layer`] as a textured quad placed by its
//! [`Transform`], back-to-front by `z`, with per-layer opacity and alpha blending —
//! "PiP is just a layer with a scale+translate transform" (architecture §4).
//!
//! Two targets: an **offscreen** texture (headless — used by the tests to read pixels
//! back and prove the GPU actually drew) and a **surface** (the winit kiosk window).
//! Frame pixels arrive via [`WgpuCompositor::upload_texture`]; the decoder/pipeline
//! calls it on the render thread.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt as _;

use crate::compositor::{Compositor, Layer, LayerId, Transform};
use crate::error::PipelineError;

const SHADER: &str = r#"
// {vec4, f32} lays out as 32 bytes (struct align 16), matching the Rust `Uniform`
// which pads to 32. Do NOT add a vec3 pad here — that would round the size up to 48.
struct Uniform { transform: vec4<f32>, opacity: f32 };
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<uniform> u: Uniform;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var quad = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let uv = quad[vi];
    let sx = u.transform.x; let sy = u.transform.y;
    let ox = u.transform.z; let oy = u.transform.w;
    // Map the layer's [0,1] surface-space rect into clip space (flip Y).
    let x = (ox + uv.x * sx) * 2.0 - 1.0;
    let y = 1.0 - (oy + uv.y * sy) * 2.0;
    var out: VsOut;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(tex, smp, in.uv);
    return vec4<f32>(c.rgb, c.a * u.opacity);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniform {
    transform: [f32; 4],
    opacity: f32,
    _pad: [f32; 3],
}

impl Uniform {
    fn from_layer(t: Transform, opacity: f32) -> Self {
        Self {
            transform: [t.scale_x, t.scale_y, t.offset_x, t.offset_y],
            opacity,
            _pad: [0.0; 3],
        }
    }
}

/// Pixel layout accepted by [`WgpuCompositor::upload_texture`]. BGRA sources (CEF
/// `on_paint`, some decoders) upload as native `Bgra8Unorm` — no CPU swizzle pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexelFormat {
    /// Packed RGBA8.
    Rgba8,
    /// Packed BGRA8.
    Bgra8,
}

impl TexelFormat {
    fn to_wgpu(self) -> wgpu::TextureFormat {
        match self {
            Self::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
            Self::Bgra8 => wgpu::TextureFormat::Bgra8Unorm,
        }
    }
}

struct LayerGpu {
    texture: wgpu::Texture,
    format: TexelFormat,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

struct LayerState {
    meta: Layer,
    gpu: Option<LayerGpu>,
}

enum Target {
    Offscreen {
        texture: wgpu::Texture,
        size: (u32, u32),
    },
    Surface {
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
    },
}

/// The wgpu compositor.
pub struct WgpuCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    target: Target,
    layers: HashMap<LayerId, LayerState>,
}

impl WgpuCompositor {
    /// Create an offscreen compositor rendering into a `width`×`height` texture. Used by
    /// the headless tests (and any capture path); no window/surface required.
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if no adapter/device is available.
    pub fn new_offscreen(width: u32, height: u32) -> Result<Self, PipelineError> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| PipelineError::GpuInit("no GPU adapter".into()))?;
        let (device, queue) = request_device(&adapter)?;

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let (pipeline, bind_group_layout, sampler) = build_pipeline(&device, format);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            sampler,
            target: Target::Offscreen {
                texture,
                size: (width, height),
            },
            layers: HashMap::new(),
        })
    }

    /// Create a compositor rendering into a window surface (the kiosk output). The
    /// caller owns `instance` and the `surface` created from the window.
    ///
    /// # Errors
    /// [`PipelineError`] if no compatible adapter/device is found or config fails.
    pub fn new_for_surface(
        instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Result<Self, PipelineError> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| PipelineError::GpuInit("no GPU adapter for surface".into()))?;
        let (device, queue) = request_device(&adapter)?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(caps.formats[0]);
        // Prefer Mailbox (latest-frame, no tearing, and immune to the Wayland frame-
        // callback stalls that make Fifo's acquire time out on some compositors);
        // Fifo is the spec-guaranteed fallback.
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let (pipeline, bind_group_layout, sampler) = build_pipeline(&device, format);
        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            sampler,
            target: Target::Surface { surface, config },
            layers: HashMap::new(),
        })
    }

    /// Resize the surface target (no-op for offscreen).
    pub fn resize(&mut self, width: u32, height: u32) {
        if let Target::Surface { surface, config } = &mut self.target {
            config.width = width.max(1);
            config.height = height.max(1);
            surface.configure(&self.device, config);
        }
    }

    /// Upload packed pixels for a layer's texture (a decoded frame or OSD/browser
    /// paint). Creates the layer's GPU resources on first upload; a same-size,
    /// same-format re-upload writes into the existing texture, so the per-frame
    /// video/browser paths never rebuild texture + bind group.
    ///
    /// # Errors
    /// [`PipelineError::InvalidFrame`] if the buffer is smaller than `width*height*4`.
    pub fn upload_texture(
        &mut self,
        id: LayerId,
        width: u32,
        height: u32,
        format: TexelFormat,
        pixels: &[u8],
    ) -> Result<(), PipelineError> {
        let need = (width as usize) * (height as usize) * 4;
        if width == 0 || height == 0 || pixels.len() < need {
            return Err(PipelineError::InvalidFrame("pixel buffer too small"));
        }

        if let Some(gpu) = self.layers.get(&id).and_then(|s| s.gpu.as_ref()) {
            if gpu.format == format
                && gpu.texture.width() == width
                && gpu.texture.height() == height
            {
                write_pixels(&self.queue, &gpu.texture, width, height, &pixels[..need]);
                return Ok(());
            }
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layer-texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: format.to_wgpu(),
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_pixels(&self.queue, &texture, width, height, &pixels[..need]);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let meta = self.layers.get(&id).map_or(
            Layer {
                id,
                z: default_z(id),
                opacity: 1.0,
                transform: Transform::default(),
            },
            |s| s.meta.clone(),
        );
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("layer-uniform"),
                contents: bytemuck::bytes_of(&Uniform::from_layer(meta.transform, meta.opacity)),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("layer-bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });

        self.layers.insert(
            id,
            LayerState {
                meta,
                gpu: Some(LayerGpu {
                    texture,
                    format,
                    uniform,
                    bind_group,
                }),
            },
        );
        Ok(())
    }

    /// Read back the offscreen target as RGBA8 (`width*height*4` bytes). Offscreen only.
    ///
    /// # Errors
    /// [`PipelineError`] if called on a surface target or the map fails.
    pub fn read_rgba(&self) -> Result<Vec<u8>, PipelineError> {
        let Target::Offscreen { texture, size } = &self.target else {
            return Err(PipelineError::Surface(
                "read_rgba only valid offscreen".into(),
            ));
        };
        let (width, height) = *size;
        let unpadded = (width * 4) as usize;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let padded = unpadded.div_ceil(align) * align;
        let padded_u32 = u32::try_from(padded)
            .map_err(|_| PipelineError::Surface("row stride too large".into()))?;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * height as usize) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_u32),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|_| PipelineError::Surface("map channel closed".into()))?
            .map_err(|e| PipelineError::Surface(format!("{e:?}")))?;

        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity(unpadded * height as usize);
        for row in 0..height as usize {
            let start = row * padded;
            out.extend_from_slice(&data[start..start + unpadded]);
        }
        drop(data);
        buffer.unmap();
        Ok(out)
    }

    fn render_into(&self, view: &wgpu::TextureView) {
        let mut ordered: Vec<&LayerState> = self.layers.values().collect();
        ordered.sort_by_key(|s| s.meta.z);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            for state in ordered {
                if let Some(gpu) = &state.gpu {
                    pass.set_bind_group(0, &gpu.bind_group, &[]);
                    pass.draw(0..6, 0..1);
                }
            }
        }
        self.queue.submit(Some(encoder.finish()));
    }
}

impl Compositor for WgpuCompositor {
    fn upsert_layer(&mut self, layer: Layer) {
        let entry = self.layers.entry(layer.id).or_insert_with(|| LayerState {
            meta: layer.clone(),
            gpu: None,
        });
        entry.meta = layer.clone();
        // If the layer already has a texture, push the new transform/opacity to its uniform.
        if let Some(gpu) = &entry.gpu {
            self.queue.write_buffer(
                &gpu.uniform,
                0,
                bytemuck::bytes_of(&Uniform::from_layer(layer.transform, layer.opacity)),
            );
        }
    }

    fn remove_layer(&mut self, id: LayerId) {
        self.layers.remove(&id);
    }

    fn present(&mut self) {
        match &self.target {
            Target::Offscreen { texture, .. } => {
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.render_into(&view);
            }
            Target::Surface { surface, config } => {
                let frame = match surface.get_current_texture() {
                    Ok(frame) => Some(frame),
                    Err(e) => {
                        // Outdated/Lost after compositor-side changes (scale, mode) and
                        // Timeout under Wayland frame-callback stalls all recover the
                        // same way: reconfigure the swapchain and try once more.
                        tracing::warn!(error = ?e, "surface frame acquire failed; reconfiguring");
                        surface.configure(&self.device, config);
                        surface.get_current_texture().ok()
                    }
                };
                if let Some(frame) = frame {
                    let view = frame
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    self.render_into(&view);
                    frame.present();
                }
            }
        }
    }
}

/// Copy one tightly-packed frame into `texture` via the queue's staging path.
fn write_pixels(queue: &wgpu::Queue, texture: &wgpu::Texture, width: u32, height: u32, px: &[u8]) {
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        px,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(4 * width),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

fn request_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), PipelineError> {
    pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("castaway-compositor"),
            required_features: wgpu::Features::empty(),
            // Downlevel baseline, but raised to the adapter's real max texture size:
            // the baseline caps 2D textures at 2048, which can't even configure a 4K
            // surface (the Dell panel is 3840×2160).
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::Performance,
        },
        None,
    ))
    .map_err(|e| PipelineError::GpuInit(e.to_string()))
}

fn build_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout, wgpu::Sampler) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("composite-shader"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("layer-bgl"),
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
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("composite-layout"),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("composite-pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("layer-sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    (pipeline, bind_group_layout, sampler)
}

fn default_z(id: LayerId) -> i32 {
    match id {
        LayerId::Attract => -10,
        LayerId::Video => 0,
        LayerId::Browser => 5,
        LayerId::Osd => 10,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Skip GPU tests gracefully if no adapter is available in this environment.
    macro_rules! compositor_or_skip {
        ($w:expr, $h:expr) => {
            match WgpuCompositor::new_offscreen($w, $h) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("skipping GPU test: {e}");
                    return;
                }
            }
        };
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        rgba.iter()
            .copied()
            .cycle()
            .take((width * height * 4) as usize)
            .collect()
    }

    #[test]
    fn full_screen_red_layer_fills_target() {
        let mut c = compositor_or_skip!(32, 32);
        c.upload_texture(
            LayerId::Video,
            4,
            4,
            TexelFormat::Rgba8,
            &solid(4, 4, [255, 0, 0, 255]),
        )
        .unwrap();
        c.upsert_layer(Layer {
            id: LayerId::Video,
            z: 0,
            opacity: 1.0,
            transform: Transform::default(),
        });
        c.present();
        let px = c.read_rgba().unwrap();
        // Center pixel should be red.
        let idx = ((16 * 32) + 16) * 4;
        assert_eq!(&px[idx..idx + 4], &[255, 0, 0, 255], "center should be red");
    }

    #[test]
    fn pip_layer_covers_only_its_corner() {
        let mut c = compositor_or_skip!(64, 64);
        // Full-screen blue background.
        c.upload_texture(
            LayerId::Video,
            2,
            2,
            TexelFormat::Rgba8,
            &solid(2, 2, [0, 0, 255, 255]),
        )
        .unwrap();
        c.upsert_layer(Layer {
            id: LayerId::Video,
            z: 0,
            opacity: 1.0,
            transform: Transform::default(),
        });
        // Green PiP in the bottom-right corner (corner 3) — BGRA path: green survives
        // the swizzle-free upload because G is channel-order invariant.
        c.upload_texture(
            LayerId::Browser,
            2,
            2,
            TexelFormat::Bgra8,
            &solid(2, 2, [0, 255, 0, 255]),
        )
        .unwrap();
        c.upsert_layer(Layer {
            id: LayerId::Browser,
            z: 5,
            opacity: 1.0,
            transform: Transform::pip(3),
        });
        c.present();
        let px = c.read_rgba().unwrap();
        let at = |x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            [px[i], px[i + 1], px[i + 2]]
        };
        // Top-left is background blue; bottom-right holds the green PiP.
        assert_eq!(at(4, 4), [0, 0, 255], "corner away from PiP stays blue");
        assert_eq!(at(58, 58), [0, 255, 0], "PiP corner is green");
    }
}
