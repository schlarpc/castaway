//! Recording what the panel played: a [`crate::mixer::MixTap`] that writes the mix to a
//! WAV file.
//!
//! The reason this exists is evidence. Everything downstream of a decoder was asserted by
//! counters — frames accepted, blocks written, RMS over a window — and a counter cannot
//! tell a stream that arrived from a stream that arrived *right*. A channel swap, a
//! half-rate decode, a session that dropped every other block and a phase inversion all
//! satisfy "the speakers were given 2.1 million sample frames". `bluetooth-vm` needed the
//! other kind of answer: the samples themselves, on disk, where a test can correlate them
//! against the waveform a real A2DP source was told to send (#186).
//!
//! Three decisions worth stating, because each one is the reason this is usable rather
//! than nearly-usable:
//!
//! - **A tap, not an output device.** The mix is recorded *and* played. Swapping the
//!   device out would mean the panel goes quiet whenever anyone wants a recording, and the
//!   thing under test — what the speakers got — would no longer be what was measured. A
//!   tap is fed between the sum and the device write, so this is post-gain, post-mix, and
//!   identical to what left the box.
//! - **Off the mixer thread.** A tap must not block ([`crate::mixer::MixTap`]), and a
//!   write to a full disk can block for a long time. Blocks are handed to a writer thread
//!   through a bounded channel; if that thread falls behind, the *recording* loses audio
//!   and the panel does not. Losses are counted and logged rather than papered over,
//!   because a gap in a file a test is correlating against has to be visible.
//! - **The header is patched as it grows.** A receiver on a wall is stopped by SIGKILL, a
//!   power cut, or a test's `systemctl stop` — never by a clean shutdown that gets to
//!   finalise a file. So the two RIFF length fields are rewritten after every batch, and
//!   the file on disk is a playable WAV at all times, not only after a graceful stop.
//!
//! 16-bit PCM rather than the `f32` the mixer carries, for one reason: every tool reads
//! it. `aplay`, Audacity, ffmpeg, and Python's own `wave` module all open this file with
//! no arguments, which is what makes the analysis in a VM test three lines long. 16 bits
//! is also about 90 dB of headroom over anything a lossy codec is going to preserve.

use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

use crate::error::PipelineError;
use crate::mixer::{MixTap, CHANNELS, RATE};

/// Blocks the writer may fall behind by before the recording starts losing audio.
///
/// The mixer hands over one pass at a time — a few milliseconds each — so this is well
/// under a second of slack. Deep enough that a disk hiccup costs nothing, shallow enough
/// that a writer which has genuinely stopped is reported within a second rather than
/// growing an unbounded queue behind it.
const QUEUE_DEPTH: usize = 64;

/// Bytes of RIFF header before the first sample. The canonical 44-byte PCM header.
///
/// `u32` because that is the width of the two length fields it is added to.
const HEADER_LEN: u32 = 44;

/// A recording of everything the mixer plays.
///
/// Created once per run and handed to the mixer as a tap; dropping it ends the writer
/// thread, which finalises the file on its way out.
#[derive(Debug)]
pub struct MixRecorder {
    /// Blocks on their way to the writer thread. `None` once the writer has gone.
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    /// Where it is being written, for logs and for [`Self::path`].
    path: PathBuf,
    /// Sample frames the writer has actually put on disk. Not what the tap accepted —
    /// see [`Self::dropped_frames`] for the difference, which is the whole point of
    /// counting both.
    written: Arc<AtomicU64>,
    /// Sample frames the tap could not hand over because the writer was behind.
    dropped: AtomicU64,
}

impl MixRecorder {
    /// Start recording to `path`, replacing anything already there.
    ///
    /// The file is created and its header written *here*, synchronously, so a path that
    /// cannot be written is an error at startup rather than a silence discovered when
    /// somebody goes looking for a recording that was never made.
    ///
    /// # Errors
    /// [`PipelineError::Audio`] if the file cannot be created or its header written.
    pub fn create(path: &Path) -> Result<Arc<Self>, PipelineError> {
        let file = std::fs::File::create(path).map_err(|e| {
            PipelineError::Audio(format!("creating the recording {}: {e}", path.display()))
        })?;
        let mut writer = Writer::start(file, path.to_path_buf())?;

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(QUEUE_DEPTH);
        let written = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&written);
        // A thread rather than a `spawn_blocking` task: this outlives every session, and a
        // runtime task holding a blocking slot for the life of the process is exactly what
        // ground rule 4 is about.
        std::thread::Builder::new()
            .name("mix-recorder".to_owned())
            .spawn(move || {
                while let Ok(block) = rx.recv() {
                    match writer.append(&block) {
                        Ok(frames) => {
                            counter.fetch_add(frames, Ordering::Relaxed);
                        }
                        // One warning and out: a disk that refused this write will refuse
                        // the next, and a warning per mixer pass would bury the reason.
                        Err(e) => {
                            warn!(error = %e, "audio recording stopped");
                            return;
                        }
                    }
                }
                info!(path = %writer.path.display(), frames = counter.load(Ordering::Relaxed),
                      "audio recording finished");
            })
            .map_err(|e| PipelineError::Audio(format!("spawning the recording writer: {e}")))?;

        info!(
            path = %path.display(),
            rate = RATE,
            channels = CHANNELS,
            "recording the mix (16-bit WAV)"
        );
        Ok(Arc::new(Self {
            tx,
            path: path.to_path_buf(),
            written,
            dropped: AtomicU64::new(0),
        }))
    }

    /// Where the recording is being written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sample frames on disk.
    #[must_use]
    pub fn frames_written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Sample frames the recording lost because the writer was behind.
    ///
    /// Zero in every healthy run. Read by tests, and worth reading before trusting a
    /// correlation: a gap here is a gap in the file, and it belongs to the recorder
    /// rather than to anything under test.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl MixTap for MixRecorder {
    fn mixed(&self, _at: Instant, stereo: &[f32]) {
        // `try_send`, never `send`: this runs on the mixer thread between the sum and the
        // device write, so parking here would starve the device that is about to be
        // written to.
        if self.tx.try_send(stereo.to_vec()).is_err() {
            let frames = (stereo.len() / usize::from(CHANNELS.max(1))) as u64;
            let before = self.dropped.fetch_add(frames, Ordering::Relaxed);
            // Once per run, on the first loss. The counter carries the rest, and this is a
            // path that fires per mixer pass when it fires at all.
            if before == 0 {
                warn!(
                    path = %self.path.display(),
                    "the audio recording is behind and is losing audio; the panel is not"
                );
            }
        }
    }
}

/// The writer thread's half: the file, and how much of it is samples.
struct Writer {
    file: std::fs::File,
    path: PathBuf,
    /// Bytes of sample data written, which is what both RIFF length fields are derived
    /// from.
    data_bytes: u64,
}

impl Writer {
    /// Write the header of an empty recording.
    fn start(file: std::fs::File, path: PathBuf) -> Result<Self, PipelineError> {
        let mut writer = Self {
            file,
            path,
            data_bytes: 0,
        };
        writer
            .write_header()
            .map_err(|e| writer.fail("writing the header", e))?;
        Ok(writer)
    }

    /// Append one block of interleaved [`RATE`]/[`CHANNELS`] float samples, returning the
    /// sample frames it added.
    fn append(&mut self, stereo: &[f32]) -> Result<u64, PipelineError> {
        let mut bytes = Vec::with_capacity(stereo.len() * 2);
        for sample in stereo {
            // Clamp rather than wrap. A sample above full scale is what a device would
            // clip, so clipping is the honest representation of what came out; wrapping
            // would turn a loud passage into noise that looks like a decoder fault.
            //
            // `round` rather than the truncation `as` would do on its own: truncating
            // biases every sample toward zero by up to one LSB, and a bias is exactly the
            // kind of error a correlation is built to notice. A NaN survives both and
            // saturates to zero, which is one sample of silence rather than noise.
            #[allow(clippy::cast_possible_truncation)]
            let scaled = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
            bytes.extend_from_slice(&scaled.to_le_bytes());
        }
        self.file
            .write_all(&bytes)
            .map_err(|e| self.fail("appending samples", e))?;
        self.data_bytes += bytes.len() as u64;
        // Both length fields, every batch. See the module note: the file has to be
        // playable when the process is killed rather than stopped.
        self.write_lengths()
            .map_err(|e| self.fail("updating the RIFF lengths", e))?;
        Ok(stereo.len() as u64 / u64::from(CHANNELS.max(1)))
    }

    /// The canonical 44-byte PCM header, with both lengths at their current values.
    fn write_header(&mut self) -> std::io::Result<()> {
        let channels = CHANNELS.max(1);
        let bits = 16u16;
        let block_align = channels * bits / 8;
        let byte_rate = RATE * u32::from(block_align);
        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&self.riff_len().to_le_bytes());
        header.extend_from_slice(b"WAVEfmt ");
        header.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk length
        header.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
        header.extend_from_slice(&channels.to_le_bytes());
        header.extend_from_slice(&RATE.to_le_bytes());
        header.extend_from_slice(&byte_rate.to_le_bytes());
        header.extend_from_slice(&block_align.to_le_bytes());
        header.extend_from_slice(&bits.to_le_bytes());
        header.extend_from_slice(b"data");
        header.extend_from_slice(&self.data_len().to_le_bytes());
        debug_assert_eq!(header.len(), HEADER_LEN as usize);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Rewrite the two length fields in place, leaving the write cursor at the end.
    fn write_lengths(&mut self) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&self.riff_len().to_le_bytes())?;
        self.file.seek(SeekFrom::Start(40))?;
        self.file.write_all(&self.data_len().to_le_bytes())?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// The `RIFF` chunk length: everything after the first eight bytes.
    fn riff_len(&self) -> u32 {
        self.data_len().saturating_add(HEADER_LEN - 8)
    }

    /// The `data` chunk length.
    fn data_len(&self) -> u32 {
        u32::try_from(self.data_bytes).unwrap_or(u32::MAX)
    }

    /// An I/O failure, named with what was being attempted and to which file.
    fn fail(&self, what: &str, e: std::io::Error) -> PipelineError {
        PipelineError::Audio(format!("{what} in {}: {e}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The recording, once `frames` sample frames have reached the disk.
    ///
    /// A poll rather than a sleep: the writer is a thread, so "has it caught up" is the
    /// only question with an answer, and the timeout makes a stuck writer a failure
    /// rather than a hang.
    fn wait_for(rec: &MixRecorder, frames: u64) -> Vec<u8> {
        for _ in 0..500 {
            if rec.frames_written() >= frames {
                return std::fs::read(rec.path()).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!(
            "the writer stopped at {} of {frames} frames",
            rec.frames_written()
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("castaway-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{tag}.wav"))
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn i16_at(bytes: &[u8], at: usize) -> i16 {
        i16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
    }

    #[test]
    fn the_header_says_what_the_mixer_actually_produces() {
        // A header that disagrees with the samples is worse than no recording: every tool
        // that opens it plays the right bytes at the wrong speed, and a correlation
        // against it fails for a reason that has nothing to do with the audio path. So
        // the format comes from the mixer's own constants and is asserted against them.
        let path = scratch("header");
        let rec = MixRecorder::create(&path).unwrap();
        let frames = 480;
        rec.mixed(Instant::now(), &vec![0.0; frames * usize::from(CHANNELS)]);
        let wav = wait_for(&rec, frames as u64);

        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32_at(&wav, 16), 16, "fmt chunk length");
        assert_eq!(i16_at(&wav, 20), 1, "WAVE_FORMAT_PCM");
        assert_eq!(
            i16_at(&wav, 22),
            i16::try_from(CHANNELS).unwrap(),
            "channels"
        );
        assert_eq!(u32_at(&wav, 24), RATE, "sample rate");
        assert_eq!(
            u32_at(&wav, 28),
            RATE * u32::from(CHANNELS) * 2,
            "byte rate"
        );
        assert_eq!(
            i16_at(&wav, 32),
            i16::try_from(CHANNELS).unwrap() * 2,
            "block align"
        );
        assert_eq!(i16_at(&wav, 34), 16, "bits per sample");
        assert_eq!(&wav[36..40], b"data");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_recording_is_playable_before_it_is_finished() {
        // The property the header-patching exists for: nothing here stops or drops the
        // recorder, and the lengths on disk still describe the samples on disk. A
        // recorder that only finalised on close would leave a file every tool reads as
        // empty — which is what a `systemctl stop` or a power cut produces on a panel.
        let path = scratch("live");
        let rec = MixRecorder::create(&path).unwrap();
        let frames = 1024;
        rec.mixed(Instant::now(), &vec![0.5; frames * usize::from(CHANNELS)]);
        let wav = wait_for(&rec, frames as u64);

        let data_bytes = u32::try_from(frames * usize::from(CHANNELS) * 2).unwrap();
        assert_eq!(u32_at(&wav, 40), data_bytes, "data length");
        assert_eq!(u32_at(&wav, 4), data_bytes + HEADER_LEN - 8, "RIFF length");
        assert_eq!(wav.len(), (HEADER_LEN + data_bytes) as usize);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn samples_arrive_interleaved_in_order_and_clipped_rather_than_wrapped() {
        // Order matters as much as value: a recording with the channels transposed would
        // make a correct decode look like a channel swap, and the test reading it would
        // blame the receiver. Full scale and beyond are pinned in the same pass because
        // wrapping is the failure that turns a loud passage into what looks like a
        // decoder fault.
        let path = scratch("samples");
        let rec = MixRecorder::create(&path).unwrap();
        rec.mixed(
            Instant::now(),
            &[0.0, 1.0, -1.0, 0.5, 4.0, -9.0, f32::NAN, 0.25],
        );
        let wav = wait_for(&rec, 4);

        let at = |frame: usize, channel: usize| {
            i16_at(&wav, HEADER_LEN as usize + (frame * 2 + channel) * 2)
        };
        assert_eq!(at(0, 0), 0);
        assert_eq!(at(0, 1), i16::MAX, "full scale positive");
        assert_eq!(at(1, 0), -i16::MAX, "full scale negative");
        assert_eq!(at(1, 1), i16::MAX / 2 + 1, "half scale, rounded");
        assert_eq!(at(2, 0), i16::MAX, "clipped, not wrapped");
        assert_eq!(at(2, 1), -i16::MAX, "clipped, not wrapped");
        // `clamp` on a NaN returns the NaN, and `as i16` saturates it to zero — silence
        // for one sample, which is the least wrong thing a broken sample can become.
        assert_eq!(at(3, 0), 0, "a NaN is silence, not noise");
        assert_eq!(at(3, 1), i16::MAX / 4 + 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_path_that_cannot_be_written_fails_at_startup() {
        // Not later, and not silently: a misconfigured path has to be an error while
        // somebody is still watching the log for it.
        let err = MixRecorder::create(Path::new("/nonexistent/castaway/mix.wav")).unwrap_err();
        assert!(
            format!("{err}").contains("mix.wav"),
            "the error should name the file: {err}"
        );
    }

    #[test]
    fn losing_audio_is_counted_rather_than_waited_on() {
        // The mixer thread must never park here. Filling the queue is the only way to
        // observe that: a `send` would block forever against a writer that cannot keep up,
        // and this returns and counts instead. Both counters are asserted because either
        // one alone would be misread — frames on disk without the losses beside them looks
        // like a complete recording.
        let path = scratch("behind");
        let rec = MixRecorder::create(&path).unwrap();
        let block = vec![0.1f32; usize::from(CHANNELS)];
        // Far more blocks than the queue holds, as fast as the thread can offer them.
        for _ in 0..QUEUE_DEPTH * 200 {
            rec.mixed(Instant::now(), &block);
        }
        // The writer is fast enough that it may well have kept up; what is asserted is
        // that nothing hung and that whatever was lost was accounted for.
        let total = rec.frames_written() + rec.dropped_frames();
        assert!(
            total > 0 && total <= (QUEUE_DEPTH * 200) as u64,
            "wrote {} and dropped {}",
            rec.frames_written(),
            rec.dropped_frames()
        );
        std::fs::remove_file(&path).ok();
    }
}
