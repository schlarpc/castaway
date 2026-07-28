//! The D36 gate, executable — OPEN-QUESTIONS Q40.
//!
//! Spawns Electron on `browser-host/`, receives shared-texture paint events over the
//! line protocol, pulls each frame's dmabuf plane fd out of the child with
//! `pidfd_getfd(2)`, imports it into a wgpu device via
//! [`pipeline::hwaccel::vulkan_import::VulkanImporter::import_single_plane`], samples it
//! in a render pass, and reads pixels back to prove the picture is *correct*, not merely
//! present. Prints a pacing summary and exits non-zero if the gate fails.
//!
//! Run from the repo root (needs the `hwaccel` feature and an Electron on PATH or in
//! `$ELECTRON`):
//!
//! ```sh
//! cargo run -p pipeline --features hwaccel --example electron_osr
//! ```
//!
//! What "fail" means here, spelled out because each one is a distinct verdict on D36:
//! - `no-texture` from the host: the platform gave software OSR, the recorded worst case.
//! - import errors: the handle shape does not survive the trip into Vulkan.
//! - wrong pixels: it *looks* imported and is scrambled — the modifier/layout trap the
//!   NV12 path's docs warn about.
//! - frame rate far below the requested rate: the pacing story does not hold.

// Spike harness: process wrangling and readback assertions, not library code. The
// unsafe here is two raw syscalls; each carries its SAFETY comment.
#![allow(unsafe_code, clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use castaway_core::{ColorInfo, GpuSurface};
use pipeline::hwaccel::dmabuf::PlaneLayout;
use pipeline::hwaccel::vulkan_import::VulkanImporter;

/// How long to consume frames before declaring a verdict.
const RUN_FOR: Duration = Duration::from_secs(15);
/// Readback cadence: every Nth frame gets its center pixel checked.
const CHECK_EVERY: u64 = 30;
/// The fixed channels the test page paints — g=64, b=192 (see browser-host/main.js).
const EXPECT_G: u8 = 64;
const EXPECT_B: u8 = 192;
const TOLERANCE: i16 = 3;

/// Keeps the frame's duplicated fd alive until wgpu retires the texture.
///
/// The `release` ack back to the browser is what actually recycles the buffer; this
/// owner just makes sure the fd we imported outlives every submission that samples it.
#[derive(Debug)]
struct FrameOwner(#[allow(dead_code)] LocalFd);

impl GpuSurface for FrameOwner {
    fn color(&self) -> ColorInfo {
        ColorInfo::default()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("SPIKE FAIL: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<(), String> {
    let electron = std::env::var("ELECTRON").unwrap_or_else(|_| "electron".into());
    let host = concat!(env!("CARGO_MANIFEST_DIR"), "/../../browser-host");

    // -- wgpu device with the interop extensions ------------------------------------
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .ok_or("no Vulkan adapter")?;
    eprintln!("spike: adapter = {}", adapter.get_info().name);
    let (device, queue, mut importer) =
        VulkanImporter::open_device(&adapter, wgpu::Limits::default())
            .map_err(|e| format!("open interop device: {e}"))?;

    let pass = SamplePass::new(&device);

    // -- the browser ----------------------------------------------------------------
    let mut child = Command::new(&electron)
        .arg(host)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawning {electron}: {e}"))?;
    let mut to_browser = child.stdin.take().unwrap();
    let from_browser = BufReader::new(child.stdout.take().unwrap());

    let result = consume(
        &device,
        &queue,
        &mut importer,
        &pass,
        from_browser,
        &mut to_browser,
    );

    // Whatever the verdict, do not leave an Electron behind.
    let _ = writeln!(to_browser, r#"{{"type":"quit"}}"#);
    let _ = to_browser.flush();
    reap(&mut child);
    result
}

/// One paint message, as the wire carries it.
#[derive(serde::Deserialize)]
struct Paint {
    id: u64,
    #[serde(rename = "pixelFormat")]
    pixel_format: String,
    width: u32,
    height: u32,
    modifier: String,
    planes: Vec<Plane>,
}

#[derive(serde::Deserialize)]
struct Plane {
    fd: RawFd,
    stride: u64,
    offset: u64,
}

struct Stats {
    frames: u64,
    checks: u64,
    reds: Vec<u8>,
    first_paint: Option<Instant>,
}

fn consume(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    importer: &mut VulkanImporter,
    pass: &SamplePass,
    from_browser: BufReader<impl std::io::Read>,
    to_browser: &mut impl std::io::Write,
) -> Result<(), String> {
    let mut pidfd: Option<OwnedFd> = None;
    let mut stats = Stats {
        frames: 0,
        checks: 0,
        reds: Vec::new(),
        first_paint: None,
    };
    let started = Instant::now();

    for line in from_browser.lines() {
        let line = line.map_err(|e| format!("browser pipe: {e}"))?;
        let msg: serde_json::Value =
            serde_json::from_str(&line).map_err(|e| format!("bad message {line:?}: {e}"))?;
        match msg["type"].as_str() {
            Some("ready") => {
                let pid = msg["pid"].as_i64().ok_or("ready without pid")? as libc::pid_t;
                pidfd = Some(pidfd_open(pid)?);
                eprintln!("spike: browser up, pid {pid}");
            }
            Some("no-texture") => {
                return Err("platform delivered software OSR — the D36 worst case".into());
            }
            Some("drop") => { /* the host counted it; pacing shows up in fps */ }
            Some("paint") => {
                let paint: Paint = serde_json::from_value(msg).map_err(|e| e.to_string())?;
                let pidfd = pidfd.as_ref().ok_or("paint before ready")?;
                stats.first_paint.get_or_insert_with(Instant::now);
                match import_and_sample(device, queue, importer, pass, pidfd, &paint) {
                    Ok(pixel) => {
                        stats.frames += 1;
                        if stats.frames % CHECK_EVERY == 1 {
                            let [r, g, b, a] = pixel;
                            check_fixed_channels(g, b, a)?;
                            stats.reds.push(r);
                            stats.checks += 1;
                            eprintln!(
                                "spike: frame {} ({}x{} {}) center rgba({r},{g},{b},{a})",
                                paint.id, paint.width, paint.height, paint.pixel_format,
                            );
                        }
                    }
                    Err(e) => return Err(format!("frame {}: {e}", paint.id)),
                }
                // Sampling is complete (readback path waits on the GPU), so the buffer
                // can go back to Chromium's pool.
                writeln!(to_browser, r#"{{"type":"release","id":{}}}"#, paint.id)
                    .and_then(|()| to_browser.flush())
                    .map_err(|e| format!("release ack: {e}"))?;
            }
            other => eprintln!("spike: ignoring message type {other:?}"),
        }
        if started.elapsed() > RUN_FOR {
            break;
        }
    }

    summarize(&stats)
}

/// The gate's arithmetic: enough frames, at pace, with live and correct pixels.
fn summarize(stats: &Stats) -> Result<(), String> {
    let Some(first) = stats.first_paint else {
        return Err("no frames arrived at all".into());
    };
    let secs = first.elapsed().as_secs_f64();
    let fps = stats.frames as f64 / secs;
    println!(
        "spike: {} frames in {secs:.1}s = {fps:.1} fps",
        stats.frames
    );
    println!(
        "spike: {} pixel checks, red channel over time: {:?}",
        stats.checks, stats.reds
    );
    if stats.frames < 60 {
        return Err(format!("only {} frames in {secs:.1}s", stats.frames));
    }
    // The page repaints every frame, so a healthy pipeline holds a large fraction of the
    // requested 60. Half is deliberately forgiving — the gate is "does the architecture
    // pace", not "is the dev box fast".
    if fps < 30.0 {
        return Err(format!("{fps:.1} fps is below the 30 fps gate"));
    }
    // The animation walks the red channel; a frozen or one-frame-forever picture fails.
    let all_same = stats.reds.windows(2).all(|w| w[0] == w[1]);
    if stats.checks >= 3 && all_same {
        return Err("center pixel never changed — the picture is frozen".into());
    }
    println!("SPIKE PASS");
    Ok(())
}

/// Judge the two channels the page holds constant, and name the likely cause when they
/// are wrong — the two failure modes need different fixes.
///
/// R and B are what a BGRA/RGBA mixup exchanges; G is invariant under it. So "G right,
/// B wrong" is channel order, while "G wrong too" means the bytes themselves are not
/// where the layout said — the modifier/pitch trap the NV12 path's docs warn about,
/// which imports successfully and renders plausible garbage.
fn check_fixed_channels(g: u8, b: u8, a: u8) -> Result<(), String> {
    let close = |got: u8, want: u8| (i16::from(got) - i16::from(want)).abs() <= TOLERANCE;
    if close(g, EXPECT_G) && !close(b, EXPECT_B) {
        return Err(format!(
            "green is right but blue is not (g={g} b={b}, expected b={EXPECT_B}) — R/B \
             exchanged, i.e. a BGRA/RGBA mismatch in the import format, not a layout fault"
        ));
    }
    if !close(g, EXPECT_G) || !close(b, EXPECT_B) || a != 255 {
        return Err(format!(
            "center pixel g={g} b={b} a={a}, expected g={EXPECT_G} b={EXPECT_B} a=255 — \
             scrambled import (modifier/pitch?)"
        ));
    }
    Ok(())
}

/// Import one frame, sample it into the offscreen target, read the center pixel back.
fn import_and_sample(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    importer: &mut VulkanImporter,
    pass: &SamplePass,
    pidfd: &OwnedFd,
    paint: &Paint,
) -> Result<[u8; 4], String> {
    let [plane] = paint.planes.as_slice() else {
        return Err(format!(
            "expected 1 plane for {}, got {}",
            paint.pixel_format,
            paint.planes.len()
        ));
    };
    let format = match paint.pixel_format.as_str() {
        "bgra" => wgpu::TextureFormat::Bgra8Unorm,
        "rgba" => wgpu::TextureFormat::Rgba8Unorm,
        other => return Err(format!("unhandled pixelFormat {other:?}")),
    };
    let modifier: u64 = paint
        .modifier
        .parse()
        .map_err(|e| format!("modifier {:?}: {e}", paint.modifier))?;

    // The fd number in the message is the *browser's*; make it ours.
    let local = pidfd_getfd(pidfd, plane.fd)?;
    let raw = local.as_raw();
    let texture = importer
        .import_single_plane(
            device,
            paint.width,
            paint.height,
            modifier,
            PlaneLayout {
                fd: raw,
                offset: plane.offset,
                pitch: plane.stride,
            },
            format,
            Arc::new(FrameOwner(local)),
        )
        .map_err(|e| format!("import: {e}"))?;

    pass.sample_and_read_center(device, queue, &texture)
}

/// The sampling half: a fullscreen triangle through a real shader into a small
/// offscreen target, because "importable" and "sampleable with correct pixels" are
/// different claims and the second one is the gate.
struct SamplePass {
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    layout: wgpu::BindGroupLayout,
    target: wgpu::Texture,
    readback: wgpu::Buffer,
}

const TARGET: u32 = 64;
const SHADER: &str = r#"
@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> }
@vertex fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -3.0), vec2(3.0, 1.0), vec2(-1.0, 1.0));
    var o: VOut;
    o.pos = vec4(p[i], 0.0, 1.0);
    o.uv = vec2((p[i].x + 1.0) * 0.5, 1.0 - (p[i].y + 1.0) * 0.5);
    return o;
}
@fragment fn fs(v: VOut) -> @location(0) vec4<f32> {
    return textureSample(t, s, v.uv);
}
"#;

impl SamplePass {
    fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("spike-sample"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
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
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("spike-sample"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: "vs",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: "fs",
                targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spike-target"),
            size: wgpu::Extent3d {
                width: TARGET,
                height: TARGET,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spike-readback"),
            size: u64::from(TARGET) * u64::from(TARGET) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            sampler,
            layout,
            target,
            readback,
        }
    }

    fn sample_and_read_center(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
    ) -> Result<[u8; 4], String> {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let target_view = self.target.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spike-sample"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::RED),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &bind, &[]);
            rp.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            self.target.as_image_copy(),
            wgpu::ImageCopyBuffer {
                buffer: &self.readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(TARGET * 4),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: TARGET,
                height: TARGET,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|_| "map callback dropped".to_string())?
            .map_err(|e| format!("map readback: {e}"))?;
        let pixel = {
            let data = slice.get_mapped_range();
            let center = ((TARGET / 2) * TARGET + TARGET / 2) as usize * 4;
            [
                data[center],
                data[center + 1],
                data[center + 2],
                data[center + 3],
            ]
        };
        self.readback.unmap();
        Ok(pixel)
    }
}

/// An fd pulled out of the browser process, closed on drop.
#[derive(Debug)]
struct LocalFd(OwnedFd);

impl LocalFd {
    fn as_raw(&self) -> RawFd {
        use std::os::fd::AsRawFd as _;
        self.0.as_raw_fd()
    }
}

fn pidfd_open(pid: libc::pid_t) -> Result<OwnedFd, String> {
    // SAFETY: `pidfd_open` takes a pid and flags and returns a new fd or -1; no memory
    // is passed. The child is ours, so the pid cannot have been recycled yet.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if fd < 0 {
        return Err(format!(
            "pidfd_open({pid}): {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: the syscall returned a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn pidfd_getfd(pidfd: &OwnedFd, remote: RawFd) -> Result<LocalFd, String> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: `pidfd_getfd` duplicates `remote` from the process behind `pidfd` into
    // this one; both descriptors are live (`pidfd` is owned, `remote` is held open by
    // the browser until we ack release). Yama permits it because the browser is our
    // direct child.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), remote, 0u32) };
    if fd < 0 {
        return Err(format!(
            "pidfd_getfd({remote}): {} (production uses SCM_RIGHTS — see Q40)",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: the syscall returned a fresh descriptor nothing else owns.
    Ok(LocalFd(unsafe { OwnedFd::from_raw_fd(fd as RawFd) }))
}

fn reap(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}
