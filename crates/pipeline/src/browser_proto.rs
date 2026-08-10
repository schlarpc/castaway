//! The wire protocol between castaway and its browser subprocess (D36).
//!
//! Pure, per ground rule 3: types and a line framer, no sockets and no process handling.
//! The actor in [`crate::electron_browser`] owns the socket and feeds bytes to this.
//!
//! ## Why this exists at all
//!
//! Under CEF the browser boundary was a version-locked C++ ABI: correctness meant three
//! pins agreeing (the `cef` crate, nixpkgs `cef-binary`, and a hand-forged
//! `archive.json`), and the whole path was `doCheck = false` because exercising it needed
//! a GPU and a display. Moving the browser out of process replaces that with a boundary
//! we define, which means it can be fixture-tested like every other wire protocol here.
//!
//! ## Shape
//!
//! Newline-delimited JSON, because the payloads are small and structural — the *pixels*
//! never travel through it. A paint message carries a handle number and a geometry; the
//! frame itself is a GPU buffer the consumer pulls over with
//! [`crate::hwaccel::remote_handle`]. That split is the whole design: the control plane is
//! chatty and cheap, the data plane never leaves the GPU.
//!
//! ## The one ordering rule
//!
//! A painted frame is borrowed, not given. The browser may not recycle the buffer until
//! [`ToBrowser::Release`] names it, and the consumer may not send that until the GPU has
//! finished sampling. Releasing early is a tear that only shows under load; never
//! releasing is a stalled browser. [`FromBrowser::Paint`] and `Release` are therefore
//! matched by `id`, and the actor is what keeps them paired.
//!
//! ## Two windows, one pipe
//!
//! The host owns one offscreen window per [`Surface`] — the idle widget and the cast
//! page — so the two have separate navigation state: opening a cast never flashes the
//! clock through the page, and ending one never reloads the clock. The pipe stays
//! single; every window-scoped message names its surface instead. Paint `id`s remain
//! globally unique across both windows, so `Release` needs no tag.

use serde::{Deserialize, Serialize};

/// Frames one window has painted and the consumer has not yet released.
///
/// Small on purpose. The browser drops rather than queues when this many are outstanding
/// (ground rule 4: for live output, latency beats freshness), so a slow consumer costs
/// dropped frames rather than growing lag.
///
/// Sized to the consumer's steady state, which legitimately holds three at 60 fps — the
/// frame being retired by the GPU, the frame on the layer, and the newest paint waiting
/// in the pending slot — plus one so a paint arriving in that state is not dropped.
/// **Per window**, not per process: each surface has its own pending slot on the
/// consumer side, so a busy page must not be able to starve the clock of its budget.
/// Mirrored by `MAX_INFLIGHT` in `browser-host/main.js`, which is the side that
/// enforces it.
pub const MAX_INFLIGHT_FRAMES: usize = 4;

/// Which of the browser's two windows a message is about.
///
/// The browser host owns one offscreen `BrowserWindow` per variant, each with its own
/// `webContents` and therefore its own navigation state: opening a cast page must not
/// flash the clock through it, and closing the page must not force the clock to reload.
/// Every message that acts on — or reports from — a window names its surface, so "which
/// window" is never inferred from what happens to be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// The idle widget (the clock): created at startup when an attract URL is
    /// configured, and never navigated anywhere else.
    Widget,
    /// The cast page (YouTube leanback, DIAL launches): created on first navigate,
    /// blanked when the cast ends.
    Page,
}

/// One plane of a painted frame, as the producer's platform describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaneInfo {
    /// The buffer handle **in the browser's own numbering** — an fd on Linux, an NT
    /// handle on Windows. Meaningless in this process until pulled across.
    pub fd: i64,
    /// Bytes per row.
    pub stride: u64,
    /// Byte offset of the plane within its buffer.
    pub offset: u64,
}

/// How a paint's plane buffers travel from the browser to us (#271).
///
/// On the wire so the consumer never has to guess: the two mechanisms fail differently
/// (a pidfd pull that races the browser's death versus a delivery that never arrives),
/// and inferring the transport from what happens to be in the fd table would make the
/// choice a race instead of a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FdTransport {
    /// `planes[].fd` numbers descriptors **in the browser's own process**; the consumer
    /// reaches in (`pidfd_getfd` on Linux, `DuplicateHandle` on Windows). The default,
    /// and what an older host app that says nothing means.
    #[default]
    Process,
    /// The descriptors were passed with `SCM_RIGHTS` on the fd-plane socket, keyed by
    /// this paint's `id`; `planes[].fd` is correlation only. Linux-only — Windows'
    /// `DuplicateHandle` route has no policy that can withdraw it, so it never needs
    /// this.
    Scm,
}

/// Pixel channel order, as the browser reported it.
///
/// An enum rather than a string because getting this wrong is survivable-looking: the
/// picture renders with red and blue exchanged, which reads as a colour-management bug
/// rather than a decoding one (ground rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PixelOrder {
    /// `B,G,R,A` — what Chromium reports on both platforms in practice.
    Bgra,
    /// `R,G,B,A`.
    Rgba,
}

/// What the browser sends us.
/// `rename_all` on the enum renames *variants*; `rename_all_fields` is what makes
/// multi-word fields match the JavaScript that produces them. Without it `sampleRate`
/// simply never deserializes into `sample_rate` — and the failure is not loud: a message
/// with a defaulted field silently carries a zero (which is how the A/V clock read 0),
/// and one without a default is dropped as a protocol error, which for audio meant
/// perfect silence with the tap working correctly on the far side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum FromBrowser {
    /// The host app is up. Carries the pid so handles can be pulled from it.
    Ready {
        /// The browser's process id.
        pid: u32,
    },
    /// A frame is ready and borrowed until released.
    Paint {
        /// Which window painted it — and therefore which compositor layer it is for.
        surface: Surface,
        /// Matches the [`ToBrowser::Release`] that returns it.
        id: u64,
        /// Channel order.
        format: PixelOrder,
        /// Coded width in pixels.
        width: u32,
        /// Coded height in pixels.
        height: u32,
        /// Chromium's paint timestamp in seconds — the compositor's frame clock, on an
        /// origin of Chromium's choosing. **Not** the media clock
        /// [`FromBrowser::Audio::media_time`] is on: the two share a rate, not an
        /// origin, which is why `av_skew_ms` pairs them through a gauge that removes the
        /// origin difference rather than subtracting them (#278). `0.0` when the
        /// compositor did not supply one, which is the case for pages with no media
        /// element.
        #[serde(default)]
        media_time: f64,
        /// DRM format modifier as a decimal string (Linux). `u64` does not survive JSON
        /// numbers, and `0` is linear. Absent on Windows, where the handle carries it.
        #[serde(default)]
        modifier: Option<String>,
        /// How the plane buffers travel (#271). Defaulted, so a host app predating the
        /// fd plane decodes as [`FdTransport::Process`] — the reach-in path.
        #[serde(default)]
        fd_transport: FdTransport,
        /// One entry for BGRA. More than one means a layout this build does not handle.
        planes: Vec<PlaneInfo>,
    },
    /// A frame was produced and thrown away because the consumer was behind.
    Dropped {
        /// How many have been dropped in total this session.
        total: u64,
    },
    /// The browser could not give a GPU handle and fell back to software readback.
    ///
    /// Its own variant rather than an error because it is *the* recorded worst case for
    /// D36: the port is only worthwhile if frames stay on the GPU.
    NoTexture {
        /// Whatever the browser could say about why.
        detail: String,
    },
    /// What should be injected into this page? Answered with
    /// [`ToBrowser::ScriptletSource`].
    ///
    /// Per-URL rather than once at startup, because uBO's `##+js(...)` rules are
    /// **domain-scoped**: the script for youtube.com is not the script for anything else,
    /// and a single blob would either inject the wrong page's patches everywhere or
    /// nothing anywhere. The engine computes it per navigation, exactly as the CEF render
    /// process used to.
    ScriptletQuery {
        /// Correlates the answer.
        id: u64,
        /// The document about to load.
        url: String,
    },
    /// May this resource load? Answered with [`ToBrowser::AdblockVerdict`].
    AdblockQuery {
        /// Correlates the answer.
        id: u64,
        /// Absolute request URL.
        url: String,
        /// The document the request came from, for domain-scoped rules.
        source: String,
        /// Chromium's resource type string (`script`, `image`, `xhr`, …).
        kind: String,
    },
    /// A page finished loading.
    LoadEnd {
        /// The window it loaded in.
        surface: Surface,
        /// The URL that finished.
        url: String,
        /// HTTP status, where there was one.
        status: Option<u32>,
    },
    /// A page failed to load.
    LoadError {
        /// The window it failed in — recovery reloads that window, not both.
        surface: Surface,
        /// The URL that failed.
        url: String,
        /// Chromium's description.
        error: String,
    },
    /// A renderer process died. The host decides whether to recover.
    RenderGone {
        /// The window whose renderer died. The other window's renderer is a separate
        /// process and is still fine — a crashing cast page must not take the clock
        /// down with it.
        surface: Surface,
        /// Chromium's reason string (`crashed`, `killed`, `oom`, …).
        reason: String,
    },
    /// A block of the page's audio, taken before it reached any sound card.
    ///
    /// Carries `media_time` because that is what makes A/V sync *measurable*: paired
    /// against the paint timestamps the composited frames carry, drift between the
    /// page's sound and its pictures becomes a number. The two are **not** on one clock
    /// — the paint side is the compositor's own origin — so the pairing subtracts the
    /// session-start offset rather than the raw values (#278). Without the pair there is
    /// a picture and a sound with no stated relationship, which is precisely the bug
    /// nobody can diagnose from the room.
    Audio {
        /// The window whose page produced it. Only [`Surface::Page`] audio counts
        /// toward the lip-sync measurement — the clock has no media clock to pair with.
        surface: Surface,
        /// Base64 interleaved `f32` samples. CDP bindings carry strings only.
        pcm: String,
        /// Channel count in the interleave.
        channels: u16,
        /// Samples per second.
        sample_rate: u32,
        /// The media element's `currentTime` when this block was captured, in seconds.
        media_time: f64,
        /// Whether the element is paused (a tail block after a pause is not a gap).
        paused: bool,
    },
    /// The page's answer to [`ToBrowser::Probe`], for tests that need to see inside it.
    ProbeResult {
        /// The query being answered.
        id: u64,
        /// The expression's value, JSON-encoded, or an error string.
        value: String,
    },
    /// Something the browser wants logged, without inventing a message type for it.
    Log {
        /// Severity as a bare word.
        level: String,
        /// Message body.
        message: String,
    },
}

/// What we send the browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ToBrowser {
    /// Return a painted frame's buffer to the browser's pool.
    Release {
        /// The `id` from the [`FromBrowser::Paint`] being returned.
        id: u64,
    },
    /// Show `url` in a window at the given viewport size. The window is created on its
    /// first navigate; a failure to create it falls back to sharing the other window,
    /// logged, rather than crashing (single-window behaviour as the degraded mode).
    Navigate {
        /// The window to navigate.
        surface: Surface,
        /// Where to go.
        url: String,
        /// Viewport width in pixels.
        width: u32,
        /// Viewport height in pixels.
        height: u32,
    },
    /// Stop painting and drop one window's page. The other window is untouched — this
    /// is what lets a cast end without the clock so much as blinking.
    Blank {
        /// The window to blank.
        surface: Surface,
    },
    /// A window's viewport changed size — or came back onto the glass at the size it
    /// already had. Either way the host app answers with `setContentSize` *and* a forced
    /// repaint, because the consumer drops stale-sized frames and a page with no reason
    /// to damage itself (a paused video, a clock between ticks) would otherwise leave
    /// its layer empty until it next chose to animate.
    Resize {
        /// The window that changed.
        surface: Surface,
        /// New width in pixels.
        width: u32,
        /// New height in pixels.
        height: u32,
    },
    /// A touch point, in browser view pixels.
    Touch {
        /// The window that owns the contact.
        surface: Surface,
        /// Which contact.
        id: u32,
        /// What the contact did.
        phase: TouchPhase,
        /// X in view pixels.
        x: f32,
        /// Y in view pixels.
        y: f32,
    },
    /// A mouse-shaped event, in browser view pixels.
    Pointer {
        /// The window under the pointer.
        surface: Surface,
        /// What happened.
        kind: PointerKind,
        /// X in view pixels.
        x: f32,
        /// Y in view pixels.
        y: f32,
    },
    /// A scroll, at a position and with a delta.
    ///
    /// Its own variant rather than a `PointerKind`, because a wheel is the one input that
    /// needs *both* a position and a displacement: Chromium scrolls whatever is under the
    /// cursor, so sending the delta alone would scroll the wrong element on any page with
    /// more than one scrollable region.
    Wheel {
        /// The window under the cursor.
        surface: Surface,
        /// X in view pixels.
        x: f32,
        /// Y in view pixels.
        y: f32,
        /// Horizontal delta in pixels.
        dx: f32,
        /// Vertical delta in pixels.
        dy: f32,
    },
    /// A special key, tapped, into a window's page (#260).
    ///
    /// The host app synthesizes the CDP down/up pair (`Input.dispatchKeyEvent`); one
    /// message is one press-and-release, matching [`input_touch::InputSink::key`]'s tap
    /// semantics. Keys are the editing and navigation strokes composed text cannot say —
    /// everything typeable travels as [`ToBrowser::InsertText`] instead.
    Key {
        /// The window whose page is typed at.
        surface: Surface,
        /// Which key.
        key: Key,
    },
    /// Composed text, inserted at a window's page focus (#260).
    ///
    /// `Input.insertText` on the far side: by the time a phone's IME has composed —
    /// autocorrect, swipe, CJK, paste — there is no key sequence left to replay, only
    /// the string, and synthesizing per-character key events would fabricate keycodes
    /// the composition never had.
    InsertText {
        /// The window whose page is typed at.
        surface: Surface,
        /// What to insert.
        text: String,
    },
    /// The answer to an [`FromBrowser::AdblockQuery`].
    AdblockVerdict {
        /// The query being answered.
        id: u64,
        /// Whether to cancel the request.
        block: bool,
    },
    /// Install, or withdraw, the Cast receiver platform shim (#16).
    ///
    /// Sent before a hosted Cast application is navigated to and withdrawn when it
    /// stops. Withdrawal matters as much as installation: a page that is not a Cast
    /// application must not find a platform sitting there, or a stale tab could drive
    /// the last application's session.
    CastPlatform {
        /// `None` withdraws the shim.
        ///
        /// `Some(port)` is the loopback port the receiver platform listens on, which the
        /// page's SDK is handed through `queryPlatformValue("port-for-web-server")`. It
        /// travels rather than being defaulted so the page and the server cannot
        /// disagree — the SDK's own fallback is a hardcoded 8008, and a page that dials
        /// the wrong port fails silently.
        port: Option<u16>,
    },
    /// The scriptlets for a page, answering [`FromBrowser::ScriptletQuery`].
    ScriptletSource {
        /// The query being answered.
        id: u64,
        /// JavaScript to evaluate in the main world before any page script. Empty when
        /// no rule matches, which is the common case.
        source: String,
    },
    /// Evaluate an expression in the page and report the result.
    ///
    /// Exists so an integration test can ask the *page* whether it received an input,
    /// rather than asserting that we sent one. "Touch was dispatched" and "the page saw a
    /// touch" are different claims, and only the second is worth anything.
    Probe {
        /// The window whose page is asked.
        surface: Surface,
        /// Correlates the answer.
        id: u64,
        /// A JavaScript expression.
        expression: String,
    },
    /// Shut down cleanly.
    Quit,
}

/// A touch contact's state, mapped from [`input_touch::TouchPhase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TouchPhase {
    /// Contact began.
    Start,
    /// Contact moved.
    Move,
    /// Contact lifted.
    End,
    /// Contact cancelled by the system.
    Cancel,
}

/// A special key, in the spelling the host app's key table reads (#260).
///
/// Mapped from [`input_touch::Key`] like [`TouchPhase`] is from its phase — the wire
/// spelling is this crate's contract with `browser-host/main.js`, and the router's
/// vocabulary must not leak into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Key {
    /// Submit / activate.
    Enter,
    /// Delete backwards.
    Backspace,
    /// Delete forwards.
    Delete,
    /// Move focus.
    Tab,
    /// Navigate up.
    Up,
    /// Navigate down.
    Down,
    /// Navigate left.
    Left,
    /// Navigate right.
    Right,
}

impl From<input_touch::Key> for Key {
    fn from(key: input_touch::Key) -> Self {
        match key {
            input_touch::Key::Enter => Self::Enter,
            input_touch::Key::Backspace => Self::Backspace,
            input_touch::Key::Delete => Self::Delete,
            input_touch::Key::Tab => Self::Tab,
            input_touch::Key::ArrowUp => Self::Up,
            input_touch::Key::ArrowDown => Self::Down,
            input_touch::Key::ArrowLeft => Self::Left,
            input_touch::Key::ArrowRight => Self::Right,
        }
    }
}

/// The mouse-shaped events the panel can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PointerKind {
    /// Cursor moved.
    Move,
    /// Primary button pressed.
    Down,
    /// Primary button released.
    Up,
}

/// Accumulates bytes from the browser's control socket and yields whole messages.
///
/// The *socket*, not stdout. The channel moved off the child's stdio because Electron's
/// main-process stdin is unusable on Windows, and the child's stdout and stderr are
/// inherited Chromium diagnostics — CRLF-punctuated log spew that this framer would read
/// as a stream of decode errors. Not desynchronizing framing is the point of the move.
///
/// A framer rather than a `BufRead::lines()` loop because the actor reads from an async
/// pipe in arbitrary chunks: a message can arrive split across two reads, and two
/// messages can arrive in one. Getting that wrong shows up as rare, load-dependent
/// corruption, which is the worst kind to debug — so it is a type with tests.
#[derive(Debug, Default)]
pub struct LineFramer {
    buffer: Vec<u8>,
}

/// How much unframed input to tolerate before concluding the peer is not speaking this
/// protocol. Generous next to any real message; small next to memory exhaustion.
const MAX_LINE: usize = 1 << 20;

impl LineFramer {
    /// Feed a chunk; returns each complete message it completed.
    ///
    /// Undecodable lines are reported as errors and *skipped* rather than ending the
    /// stream: one malformed message from a browser we did not write should cost that
    /// message, not the session.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Result<FromBrowser, ProtocolError>> {
        let mut out = Vec::new();
        self.buffer.extend_from_slice(chunk);
        while let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = &line[..line.len() - 1];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            out.push(serde_json::from_slice::<FromBrowser>(line).map_err(|e| {
                ProtocolError::Decode {
                    detail: e.to_string(),
                    line: String::from_utf8_lossy(&line[..line.len().min(200)]).into_owned(),
                }
            }));
        }
        if self.buffer.len() > MAX_LINE {
            let overflowed = self.buffer.len();
            self.buffer.clear();
            out.push(Err(ProtocolError::LineTooLong { bytes: overflowed }));
        }
        out
    }
}

/// Encode a message for the browser's control socket, newline-terminated.
///
/// # Errors
/// [`ProtocolError::Encode`] if the message cannot be serialized, which for these types
/// means a string containing something JSON cannot hold.
pub fn encode(msg: &ToBrowser) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = serde_json::to_vec(msg).map_err(|e| ProtocolError::Encode {
        detail: e.to_string(),
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// What can go wrong translating the browser's side of the conversation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// A line was not a message this build understands.
    #[error("undecodable browser message ({detail}): {line}")]
    Decode {
        /// The decoder's complaint.
        detail: String,
        /// The offending line, truncated.
        line: String,
    },
    /// The peer sent a great deal without a newline — not this protocol.
    #[error("browser sent {bytes} bytes with no message boundary")]
    LineTooLong {
        /// How much was buffered before giving up.
        bytes: usize,
    },
    /// A message could not be encoded.
    #[error("could not encode a browser command: {detail}")]
    Encode {
        /// The encoder's complaint.
        detail: String,
    },
}

impl PixelOrder {
    /// The `wgpu` format this order corresponds to.
    ///
    /// The `Srgb` variants, and deliberately: a browser paints sRGB-encoded pixels, so
    /// the sampler has to decode them and the sRGB swapchain re-encode on the way out.
    /// Importing the same bytes as plain `Unorm` skips the decode and keeps the encode,
    /// which is the double-encode `TexelFormat::Rgba8`'s docs warn about — on the panel
    /// it read as the whole page washed out.
    #[cfg(feature = "render")]
    #[must_use]
    pub const fn texture_format(self) -> wgpu::TextureFormat {
        match self {
            Self::Bgra => wgpu::TextureFormat::Bgra8UnormSrgb,
            Self::Rgba => wgpu::TextureFormat::Rgba8UnormSrgb,
        }
    }
}

impl From<input_touch::TouchPhase> for TouchPhase {
    fn from(phase: input_touch::TouchPhase) -> Self {
        match phase {
            input_touch::TouchPhase::Down => Self::Start,
            input_touch::TouchPhase::Move => Self::Move,
            input_touch::TouchPhase::Up => Self::End,
            input_touch::TouchPhase::Cancel => Self::Cancel,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn framed(chunks: &[&[u8]]) -> Vec<Result<FromBrowser, ProtocolError>> {
        let mut framer = LineFramer::default();
        chunks.iter().flat_map(|c| framer.push(c)).collect()
    }

    #[test]
    fn a_message_split_across_reads_is_reassembled() {
        // The failure this prevents is rare and load-dependent: pipes split writes
        // wherever they like, and a framer that assumed one read per message would work
        // in every test and corrupt under a busy page.
        let got = framed(&[br#"{"type":"ready","p"#, br#"id":42}"#, b"\n"]);
        assert_eq!(got.len(), 1);
        assert_eq!(*got[0].as_ref().unwrap(), FromBrowser::Ready { pid: 42 });
    }

    #[test]
    fn several_messages_in_one_read_all_arrive() {
        let got = framed(&[b"{\"type\":\"ready\",\"pid\":1}\n{\"type\":\"blank-unknown\"}\n{\"type\":\"dropped\",\"total\":7}\n"]);
        assert_eq!(got.len(), 3);
        assert_eq!(*got[0].as_ref().unwrap(), FromBrowser::Ready { pid: 1 });
        // The unknown one is an error…
        assert!(got[1].is_err());
        // …and crucially does not eat the message after it.
        assert_eq!(*got[2].as_ref().unwrap(), FromBrowser::Dropped { total: 7 });
    }

    #[test]
    fn crlf_and_blank_lines_are_tolerated() {
        // Windows pipes, and a host app that prints an extra newline, should not be a
        // protocol error.
        let got = framed(&[b"\r\n{\"type\":\"ready\",\"pid\":3}\r\n\n"]);
        assert_eq!(got.len(), 1);
        assert_eq!(*got[0].as_ref().unwrap(), FromBrowser::Ready { pid: 3 });
    }

    #[test]
    fn a_peer_that_never_sends_a_newline_is_cut_off_rather_than_buffered_forever() {
        let mut framer = LineFramer::default();
        let mut errors = 0;
        for _ in 0..40 {
            for r in framer.push(&vec![b'x'; 64 * 1024]) {
                if matches!(r, Err(ProtocolError::LineTooLong { .. })) {
                    errors += 1;
                }
            }
        }
        assert!(errors > 0, "should have given up on an unframed peer");
    }

    #[test]
    fn a_paint_message_round_trips_with_its_modifier_as_a_string() {
        // The modifier is u64 and JSON numbers are f64: 0xffff_ffff_ffff_ff00 would come
        // back changed if this were ever "simplified" to a number.
        // One physical line: the framer splits on newlines, so a fixture wrapped for
        // readability would be testing the framer's error path instead of this.
        let line = br#"{"type":"paint","surface":"page","id":9,"format":"bgra","width":3840,"height":2160,"modifier":"18446744073709551360","planes":[{"fd":108,"stride":15360,"offset":0}]}"#;
        let mut framer = LineFramer::default();
        let mut got = framer.push(line);
        got.extend(framer.push(b"\n"));
        let FromBrowser::Paint {
            surface,
            id,
            format,
            width,
            modifier,
            planes,
            ..
        } = got.pop().unwrap().unwrap()
        else {
            panic!("expected a paint")
        };
        assert_eq!(surface, Surface::Page);
        assert_eq!(id, 9);
        assert_eq!(format, PixelOrder::Bgra);
        assert_eq!(width, 3840);
        assert_eq!(
            modifier.unwrap().parse::<u64>().unwrap(),
            0xffff_ffff_ffff_ff00
        );
        assert_eq!(planes[0].fd, 108);
        assert_eq!(planes[0].stride, 15360);
    }

    #[test]
    fn the_fd_transport_defaults_to_reach_in_and_decodes_the_scm_marker() {
        // A host app predating the fd plane (#271) says nothing and must mean the
        // pidfd/DuplicateHandle path; one that passed descriptors says so explicitly,
        // in the spelling main.js writes.
        let mut framer = LineFramer::default();
        let lines: &[&[u8]] = &[
            br#"{"type":"paint","surface":"page","id":1,"format":"bgra","width":8,"height":4,"modifier":"0","planes":[{"fd":9,"stride":32,"offset":0}]}"#,
            br#"{"type":"paint","surface":"page","id":2,"format":"bgra","width":8,"height":4,"modifier":"0","fdTransport":"scm","planes":[{"fd":9,"stride":32,"offset":0}]}"#,
        ];
        let mut got = Vec::new();
        for line in lines {
            got.extend(framer.push(line));
            got.extend(framer.push(b"\n"));
        }
        let transports: Vec<FdTransport> = got
            .into_iter()
            .map(|r| match r.unwrap() {
                FromBrowser::Paint { fd_transport, .. } => fd_transport,
                other => panic!("expected a paint, got {other:?}"),
            })
            .collect();
        assert_eq!(transports, vec![FdTransport::Process, FdTransport::Scm]);
    }

    #[test]
    fn messages_decode_from_the_field_names_the_browser_actually_sends() {
        // Byte-for-byte what browser-host/main.js writes. The previous round-trip test
        // only proved Rust agreed with itself, which is why a `sampleRate`/`sample_rate`
        // mismatch survived it: audio was dropped as undecodable and the A/V clock read
        // zero, both silently.
        let lines: &[&[u8]] = &[
            br#"{"type":"audio","surface":"page","pcm":"AAAAAA==","channels":2,"sampleRate":48000,"mediaTime":12.5,"paused":false}"#,
            br#"{"type":"paint","surface":"widget","id":1,"format":"bgra","width":8,"height":4,"mediaTime":3.25,"modifier":"0","planes":[{"fd":9,"stride":32,"offset":0}]}"#,
            br#"{"type":"probe-result","id":2,"value":"3"}"#,
            br#"{"type":"adblock-query","id":3,"url":"https://x/","source":"https://y/","kind":"script"}"#,
            br#"{"type":"render-gone","surface":"page","reason":"crashed"}"#,
            br#"{"type":"load-error","surface":"widget","url":"https://clock/","error":"boom (-2)"}"#,
        ];
        let mut framer = LineFramer::default();
        let mut got = Vec::new();
        for line in lines {
            got.extend(framer.push(line));
            got.extend(framer.push(b"\n"));
        }
        assert_eq!(got.len(), 6, "every line must decode");
        for (i, r) in got.iter().enumerate() {
            assert!(r.is_ok(), "line {i} failed: {:?}", r.as_ref().err());
        }
        let FromBrowser::Audio {
            surface,
            sample_rate,
            media_time,
            channels,
            ..
        } = got[0].as_ref().unwrap().clone()
        else {
            panic!("expected audio")
        };
        assert_eq!(surface, Surface::Page);
        assert_eq!(sample_rate, 48_000);
        assert_eq!(channels, 2);
        assert!((media_time - 12.5).abs() < f64::EPSILON);
        let FromBrowser::Paint {
            surface,
            media_time,
            ..
        } = got[1].as_ref().unwrap().clone()
        else {
            panic!("expected paint")
        };
        assert_eq!(
            surface,
            Surface::Widget,
            "a widget paint must not be routable to the page's layer"
        );
        assert!(
            (media_time - 3.25).abs() < f64::EPSILON,
            "the paint's media clock must survive: it is half of av_skew_ms"
        );
        // The two faults carry the window they happened in — recovery is per-window.
        assert_eq!(
            *got[4].as_ref().unwrap(),
            FromBrowser::RenderGone {
                surface: Surface::Page,
                reason: "crashed".into()
            }
        );
        assert_eq!(
            *got[5].as_ref().unwrap(),
            FromBrowser::LoadError {
                surface: Surface::Widget,
                url: "https://clock/".into(),
                error: "boom (-2)".into()
            }
        );
    }

    #[test]
    fn commands_encode_as_one_line_each() {
        // The host app reads by newline, so an encoder that emitted pretty-printed JSON
        // would deadlock it — silently, since the first fragment parses as nothing.
        for msg in [
            ToBrowser::Release { id: 1 },
            ToBrowser::Blank {
                surface: Surface::Page,
            },
            ToBrowser::Quit,
            ToBrowser::ScriptletSource {
                id: 1,
                source: "//\nmultiline\n".into(),
            },
        ] {
            let bytes = encode(&msg).unwrap();
            assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
            assert!(bytes.ends_with(b"\n"));
        }
    }

    #[test]
    fn window_commands_name_their_surface_in_the_words_the_host_app_reads() {
        // Byte-level, like the decode fixture above and for the same reason: the Rust
        // side agreeing with itself proves nothing about the JavaScript that switches on
        // `msg.surface`. A tag that serialized as "Widget" would route every command to
        // the fallback path — silently, since main.js logs-and-continues on the unknown.
        let cases: [(ToBrowser, &str); 4] = [
            (
                ToBrowser::Navigate {
                    surface: Surface::Widget,
                    url: "https://clock/".into(),
                    width: 640,
                    height: 360,
                },
                r#"{"type":"navigate","surface":"widget","url":"https://clock/","width":640,"height":360}"#,
            ),
            (
                ToBrowser::Blank {
                    surface: Surface::Page,
                },
                r#"{"type":"blank","surface":"page"}"#,
            ),
            (
                ToBrowser::Resize {
                    surface: Surface::Page,
                    width: 1920,
                    height: 1080,
                },
                r#"{"type":"resize","surface":"page","width":1920,"height":1080}"#,
            ),
            (
                ToBrowser::Touch {
                    surface: Surface::Page,
                    id: 2,
                    phase: TouchPhase::Start,
                    x: 12.0,
                    y: 34.0,
                },
                r#"{"type":"touch","surface":"page","id":2,"phase":"start","x":12.0,"y":34.0}"#,
            ),
        ];
        for (msg, want) in cases {
            let bytes = encode(&msg).unwrap();
            assert_eq!(
                std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap(),
                want,
                "encoding drifted from what browser-host/main.js reads"
            );
        }
    }

    #[test]
    fn key_and_text_encode_in_the_words_the_host_apps_key_table_reads() {
        // Byte-level for the same reason the surface test above is: main.js switches on
        // `msg.type` and indexes KEYS on `msg.key`, and a spelling that drifted would
        // route to the unknown-command log line — silently, from the panel's side (#260).
        let cases: [(ToBrowser, &str); 3] = [
            (
                ToBrowser::Key {
                    surface: Surface::Page,
                    key: Key::Enter,
                },
                r#"{"type":"key","surface":"page","key":"enter"}"#,
            ),
            (
                ToBrowser::Key {
                    surface: Surface::Page,
                    key: Key::Left,
                },
                r#"{"type":"key","surface":"page","key":"left"}"#,
            ),
            (
                ToBrowser::InsertText {
                    surface: Surface::Page,
                    text: "héllo ✓".into(),
                },
                r#"{"type":"insert-text","surface":"page","text":"héllo ✓"}"#,
            ),
        ];
        for (msg, want) in cases {
            let bytes = encode(&msg).unwrap();
            assert_eq!(
                std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap(),
                want,
                "encoding drifted from what browser-host/main.js reads"
            );
        }
        // Every key has a row in main.js's KEYS table; the spellings are the contract.
        for (key, want) in [
            (Key::Enter, "enter"),
            (Key::Backspace, "backspace"),
            (Key::Delete, "delete"),
            (Key::Tab, "tab"),
            (Key::Up, "up"),
            (Key::Down, "down"),
            (Key::Left, "left"),
            (Key::Right, "right"),
        ] {
            assert_eq!(serde_json::to_value(key).unwrap(), want);
        }
    }

    #[test]
    fn keys_map_from_the_input_crate() {
        // The router's arrows are `ArrowUp`-style; the wire's are bare directions. The
        // pairing is what this pins.
        for (from, to) in [
            (input_touch::Key::Enter, Key::Enter),
            (input_touch::Key::Backspace, Key::Backspace),
            (input_touch::Key::Delete, Key::Delete),
            (input_touch::Key::Tab, Key::Tab),
            (input_touch::Key::ArrowUp, Key::Up),
            (input_touch::Key::ArrowDown, Key::Down),
            (input_touch::Key::ArrowLeft, Key::Left),
            (input_touch::Key::ArrowRight, Key::Right),
        ] {
            assert_eq!(Key::from(from), to);
        }
    }

    #[test]
    fn a_scriptlet_blob_survives_the_round_trip_through_json() {
        // uBO scriptlets are full of quotes, backslashes and newlines; if any of that
        // broke framing the symptom would be "injection silently stopped working".
        let source = "const s = \"a\\nb\";\n// ünïcode ✓\nfunction f(){return `x${1}`}\n";
        let bytes = encode(&ToBrowser::ScriptletSource {
            id: 4,
            source: source.into(),
        })
        .unwrap();
        assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
        let back: ToBrowser = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(
            back,
            ToBrowser::ScriptletSource {
                id: 4,
                source: source.into()
            }
        );
    }

    #[test]
    fn touch_phases_map_from_the_input_crate() {
        assert_eq!(
            TouchPhase::from(input_touch::TouchPhase::Down),
            TouchPhase::Start
        );
        assert_eq!(
            TouchPhase::from(input_touch::TouchPhase::Up),
            TouchPhase::End
        );
        assert_eq!(
            TouchPhase::from(input_touch::TouchPhase::Cancel),
            TouchPhase::Cancel
        );
    }
}
