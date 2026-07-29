//! The native PipeWire output backend (`audio-pipewire`, Linux only).
//!
//! Exists because the ALSA compatibility shim cannot do the two things the settings
//! screen needs: name the real sinks (through the shim there is exactly one device,
//! called "pipewire"), and route one stream to a chosen sink (`target.object` is a
//! PipeWire concept). Playback itself mirrors [`crate::audio_out::CpalAudioOut`]'s
//! shape exactly — a bounded channel into a callback that never blocks, the stream
//! owned by a thread of its own — because that shape is what keeps a dropout from
//! being anyone else's problem.
//!
//! A named sink that is not there falls back to whatever the session manager links
//! (the default), same policy as the cpal backend: wrong speakers that say so beat a
//! panel gone silent over an unplugged DAC.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use tracing::{info, warn};

use crate::audio_decode::PcmBlock;
use crate::audio_out::AudioOut;
use crate::audio_select::{OutputDeviceInfo, OutputSelection};
use crate::error::PipelineError;

/// Same depth as the cpal backend, for the same reason: ~a third of a second of ride,
/// short enough that a phone's pause is not audible a beat later.
const QUEUE_BLOCKS: usize = 96;

/// The PipeWire sinks on this machine, for the settings screen.
///
/// # Errors
/// [`PipelineError::Audio`] when the daemon is not reachable — which on a PipeWire
/// desktop means something is genuinely wrong, and is worth words rather than an empty
/// list that reads as "no speakers exist".
pub fn list_sinks() -> Result<Vec<OutputDeviceInfo>, PipelineError> {
    use std::cell::RefCell;
    use std::rc::Rc;

    pipewire::init();
    let mainloop = pipewire::main_loop::MainLoop::new(None)
        .map_err(|e| PipelineError::Audio(format!("pipewire main loop: {e}")))?;
    let context = pipewire::context::Context::new(&mainloop)
        .map_err(|e| PipelineError::Audio(format!("pipewire context: {e}")))?;
    let core = context
        .connect(None)
        .map_err(|e| PipelineError::Audio(format!("PipeWire daemon not reachable: {e}")))?;
    let registry = core
        .get_registry()
        .map_err(|e| PipelineError::Audio(format!("pipewire registry: {e}")))?;

    let sinks: Rc<RefCell<Vec<OutputDeviceInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let sinks = Rc::clone(&sinks);
            move |global| {
                let Some(props) = global.props else { return };
                if props.get("media.class") != Some("Audio/Sink") {
                    return;
                }
                let Some(id) = props.get("node.name") else {
                    return;
                };
                let label = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .unwrap_or(id);
                sinks.borrow_mut().push(OutputDeviceInfo {
                    id: id.to_owned(),
                    label: label.to_owned(),
                });
            }
        })
        .register();

    // One round trip: when the core answers the sync, the registry has told us
    // everything it had, and the loop can stop.
    let pending = core
        .sync(0)
        .map_err(|e| PipelineError::Audio(format!("pipewire sync: {e}")))?;
    let _core_listener = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            move |id, seq| {
                if id == pipewire::core::PW_ID_CORE && seq == pending {
                    mainloop.quit();
                }
            }
        })
        .register();
    mainloop.run();

    let mut sinks = sinks.borrow_mut();
    Ok(std::mem::take(&mut *sinks))
}

/// A real output through PipeWire. See the module note for why this exists beside the
/// cpal backend and why its internals mirror it.
pub struct PipeWireAudioOut {
    selection: OutputSelection,
    samples: Option<SyncSender<Vec<f32>>>,
    quit: Option<pipewire::channel::Sender<()>>,
    underruns: Arc<AtomicU64>,
}

impl std::fmt::Debug for PipeWireAudioOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireAudioOut")
            .field("open", &self.samples.is_some())
            .field("underruns", &self.underruns.load(Ordering::Relaxed))
            .finish()
    }
}

impl PipeWireAudioOut {
    /// An output that will play to whatever `selection` names.
    #[must_use]
    pub fn with_selection(selection: OutputSelection) -> Self {
        Self {
            selection,
            samples: None,
            quit: None,
            underruns: Arc::new(AtomicU64::new(0)),
        }
    }

    /// How many times the process callback ran dry. Same accounting as the cpal
    /// backend, for the same reason: the symptom is otherwise just "sounds bad".
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }
}

impl AudioOut for PipeWireAudioOut {
    fn start(&mut self, sample_rate: u32, channels: u16) -> Result<(), PipelineError> {
        self.stop();
        let (samples_tx, samples_rx) = sync_channel::<Vec<f32>>(QUEUE_BLOCKS);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<()>();
        let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
        let underruns = Arc::clone(&self.underruns);
        let selection = self.selection.clone();

        std::thread::spawn(move || {
            run_stream(
                &selection,
                sample_rate,
                channels,
                samples_rx,
                quit_rx,
                &ready_tx,
                &underruns,
            );
        });

        match ready_rx.recv() {
            Ok(Ok(())) => {
                info!(sample_rate, channels, "pipewire output started");
                self.samples = Some(samples_tx);
                self.quit = Some(quit_tx);
                Ok(())
            }
            Ok(Err(e)) => Err(PipelineError::Audio(e)),
            Err(_) => Err(PipelineError::Audio(
                "pipewire thread died starting up".into(),
            )),
        }
    }

    fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
        let Some(tx) = self.samples.as_ref() else {
            return Err(PipelineError::Audio("audio output not started".into()));
        };
        match tx.try_send(block.samples.clone()) {
            Ok(()) => Ok(()),
            // Same policy as cpal: drop the newest and say so rather than back the
            // decode thread up into the adapter.
            Err(TrySendError::Full(_)) => {
                warn!("pipewire output queue full; dropping a block");
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(PipelineError::Audio("audio device went away".into()))
            }
        }
    }

    fn stop(&mut self) {
        self.samples = None;
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(());
        }
    }
}

/// Own the main loop and the stream for one session, on this thread, until told to
/// quit. The other half of [`PipeWireAudioOut::start`].
fn run_stream(
    selection: &OutputSelection,
    sample_rate: u32,
    channels: u16,
    samples: Receiver<Vec<f32>>,
    quit: pipewire::channel::Receiver<()>,
    ready: &SyncSender<Result<(), String>>,
    underruns: &Arc<AtomicU64>,
) {
    match open_stream(selection, sample_rate, channels, samples, underruns) {
        Ok((mainloop, _stream, _listener)) => {
            // `stop()` reaches this thread through the channel; quitting the loop ends
            // this scope, and everything the stream owns unwinds with it. Attached
            // here rather than inside `open_stream` because the attachment borrows the
            // loop it is attached to.
            let _quit_attachment = quit.attach(mainloop.loop_(), {
                let mainloop = mainloop.clone();
                move |()| mainloop.quit()
            });
            let _ = ready.send(Ok(()));
            // Everything above lives exactly as long as this call: callbacks fire in
            // here, and the quit handler is what returns from it.
            mainloop.run();
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

/// What the audio callback owns.
struct StreamData {
    rx: Receiver<Vec<f32>>,
    pending: VecDeque<f32>,
    stride: usize,
    underruns: Arc<AtomicU64>,
}

/// Build and connect the stream. Runs on (and its results must stay on) the audio
/// thread — none of these types are `Send`, which is exactly why the thread owns them.
#[allow(clippy::type_complexity)]
fn open_stream(
    selection: &OutputSelection,
    sample_rate: u32,
    channels: u16,
    samples: Receiver<Vec<f32>>,
    underruns: &Arc<AtomicU64>,
) -> Result<
    (
        pipewire::main_loop::MainLoop,
        pipewire::stream::Stream,
        pipewire::stream::StreamListener<StreamData>,
    ),
    String,
> {
    pipewire::init();
    let mainloop =
        pipewire::main_loop::MainLoop::new(None).map_err(|e| format!("pipewire main loop: {e}"))?;
    let context =
        pipewire::context::Context::new(&mainloop).map_err(|e| format!("pipewire context: {e}"))?;
    let core = context
        .connect(None)
        .map_err(|e| format!("PipeWire daemon not reachable: {e}"))?;

    let mut props = pipewire::properties::properties! {
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Playback",
        *pipewire::keys::MEDIA_ROLE => "Music",
        *pipewire::keys::APP_NAME => "castaway",
        *pipewire::keys::NODE_NAME => "castaway",
    };
    if let OutputSelection::Device(node) = selection {
        // The chosen sink, by node.name. If it has left the building, the session
        // manager links us to the default instead — the fallback the cpal backend
        // implements by hand. The raw key: pipewire-rs 0.8 does not export a
        // constant for `target.object` (only the deprecated `node.target`).
        props.insert("target.object", node.as_str());
    }

    let stream = pipewire::stream::Stream::new(&core, "castaway-out", props)
        .map_err(|e| format!("pipewire stream: {e}"))?;

    let stride = std::mem::size_of::<f32>() * usize::from(channels);
    let data = StreamData {
        rx: samples,
        pending: VecDeque::new(),
        stride,
        underruns: Arc::clone(underruns),
    };
    let listener = stream
        .add_local_listener_with_user_data(data)
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(slot) = datas.first_mut() else {
                return;
            };
            let n_frames = if let Some(bytes) = slot.data() {
                let n_frames = bytes.len() / data.stride;
                let wanted = n_frames * (data.stride / std::mem::size_of::<f32>());
                // Top up without ever blocking: this is the audio thread, and waiting
                // here is a dropout by another name.
                while data.pending.len() < wanted {
                    match data.rx.try_recv() {
                        Ok(block) => data.pending.extend(block),
                        Err(_) => break,
                    }
                }
                let mut ran_dry = false;
                for chunk in bytes
                    .chunks_exact_mut(std::mem::size_of::<f32>())
                    .take(wanted)
                {
                    let sample = match data.pending.pop_front() {
                        Some(s) => s,
                        None => {
                            // Silence beats stale samples, same as the cpal callback.
                            ran_dry = true;
                            0.0
                        }
                    };
                    chunk.copy_from_slice(&sample.to_le_bytes());
                }
                if ran_dry {
                    data.underruns.fetch_add(1, Ordering::Relaxed);
                }
                n_frames
            } else {
                0
            };
            let chunk = slot.chunk_mut();
            *chunk.offset_mut() = 0;
            // The stride is bytes-per-frame (8 for stereo f32); neither cast can
            // actually lose anything a real buffer produces.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            {
                *chunk.stride_mut() = data.stride as i32;
                *chunk.size_mut() = (data.stride * n_frames) as u32;
            }
        })
        .register()
        .map_err(|e| format!("pipewire stream listener: {e}"))?;

    // Tell the graph exactly what the session decoded to — PipeWire owns any
    // resampling from here, which is its job and not the decode thread's.
    let mut audio_info = pipewire::spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(pipewire::spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(sample_rate);
    audio_info.set_channels(u32::from(channels));
    let values: Vec<u8> = pipewire::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pipewire::spa::pod::Value::Object(pipewire::spa::pod::Object {
            type_: pipewire::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: pipewire::spa::param::ParamType::EnumFormat.as_raw(),
            properties: audio_info.into(),
        }),
    )
    .map_err(|e| format!("pipewire format pod: {e:?}"))?
    .0
    .into_inner();
    let mut params = [pipewire::spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| "pipewire format pod did not round-trip".to_owned())?];

    stream
        .connect(
            pipewire::spa::utils::Direction::Output,
            None,
            pipewire::stream::StreamFlags::AUTOCONNECT
                | pipewire::stream::StreamFlags::MAP_BUFFERS
                | pipewire::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("pipewire stream connect: {e}"))?;

    Ok((mainloop, stream, listener))
}
