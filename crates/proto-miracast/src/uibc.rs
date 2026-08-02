//! UIBC — the User Input Back Channel, which is what makes the panel's glass do anything.
//!
//! Without it Miracast is a projector. With it, a touch on the C6522QT moves the pointer
//! on the laptop that is casting, which for a hackerspace panel is most of the point.
//! It is also the one optional part of WFD worth the effort: the sink connects out to a
//! TCP port the *source* opened (the opposite of HDCP, and easy to get backwards — see
//! `docs/miracast-protocol-notes.md` §2.1), and everything after that is small.
//!
//! ## Two length conventions in one frame
//!
//! The outer header's `Length` counts *"the entire TCP payload… including padding"* — the
//! header itself included. The inner generic-message `Length` counts only its own describe
//! payload. They are different numbers with the same name, one nested inside the other,
//! and conflating them is the classic implementation bug. [`UibcFrame`] and
//! [`GenericInput`] are separate types for exactly that reason.
//!
//! ## Coordinates
//!
//! A UIBC coordinate is a pixel index in the *negotiated stream's* space, not the panel's.
//! On a 4K panel showing a letterboxed 1920×1080 stream, a touch at the physical
//! bottom-right must be sent as (1919, 1079) — and the transform has to invert exactly
//! what the renderer applied. [`SourcePixel`] can only be built by
//! [`VideoGeometry::map_from_panel`], so a raw panel coordinate cannot reach the wire; the
//! bug that prevents is the classic "pointer moves at half speed and only reaches the
//! top-left quadrant".

use std::collections::HashMap;
use std::sync::Mutex;

use castaway_core::{SurfaceTouch, TouchPhase, TouchSurface};
use tokio::sync::mpsc;
use tracing::debug;

use crate::video::VideoMode;

/// The header without a timestamp.
const HEADER_LEN: usize = 4;

/// The header with one.
const HEADER_LEN_TIMESTAMPED: usize = 6;

/// The largest magnitude a scroll field can carry: 13 bits.
const MAX_SCROLL_UNITS: u16 = 0x1FFF;

/// Which family of input a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputCategory {
    /// Abstract events — touch, keys, scroll. The interoperable path.
    Generic,
    /// Raw HID reports. Required for Windows, which advertises HIDC only, and the only
    /// way to express a modifier or an arrow key at all.
    Hidc,
}

impl InputCategory {
    const fn wire(self) -> u8 {
        match self {
            Self::Generic => 0,
            Self::Hidc => 1,
        }
    }

    const fn from_wire(raw: u8) -> Option<Self> {
        match raw & 0x0F {
            0 => Some(Self::Generic),
            1 => Some(Self::Hidc),
            _ => None,
        }
    }
}

/// A pixel coordinate in the negotiated stream's space.
///
/// No public constructor. The only way to obtain one is [`VideoGeometry::map_from_panel`],
/// which is also the only place that knows the letterbox offsets — so "we sent the panel's
/// coordinates" is unrepresentable rather than merely tested for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePixel {
    x: u16,
    y: u16,
}

impl SourcePixel {
    /// The horizontal index, `0..width`.
    #[must_use]
    pub const fn x(self) -> u16 {
        self.x
    }

    /// The vertical index, `0..height`.
    #[must_use]
    pub const fn y(self) -> u16 {
        self.y
    }
}

/// How the negotiated picture sits on the panel.
///
/// Holds the letterbox because the inverse transform needs it: a 16:9 stream on a 16:9
/// panel has none, but a 4:3 stream on the C6522QT has bars down both sides, and a touch
/// on a bar is not a point in the stream at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoGeometry {
    mode: VideoMode,
    /// Fraction of the panel width the picture occupies, `0.0..=1.0`.
    scale_x: f32,
    /// Fraction of the panel height.
    scale_y: f32,
}

impl VideoGeometry {
    /// The geometry of `mode` letterboxed to fit a panel of `panel_width`×`panel_height`,
    /// preserving aspect — which is what the compositor does.
    #[must_use]
    pub fn letterboxed(mode: VideoMode, panel_width: u32, panel_height: u32) -> Self {
        let panel_aspect = panel_width.max(1) as f32 / panel_height.max(1) as f32;
        let video_aspect = f32::from(mode.width.max(1)) / f32::from(mode.height.max(1));
        // Wider video than panel: full width, bars top and bottom. Narrower: full height.
        let (scale_x, scale_y) = if video_aspect > panel_aspect {
            (1.0, panel_aspect / video_aspect)
        } else {
            (video_aspect / panel_aspect, 1.0)
        };
        Self {
            mode,
            scale_x,
            scale_y,
        }
    }

    /// The geometry of a picture that fills the panel exactly.
    #[must_use]
    pub const fn fullscreen(mode: VideoMode) -> Self {
        Self {
            mode,
            scale_x: 1.0,
            scale_y: 1.0,
        }
    }

    /// The mode this geometry is for.
    #[must_use]
    pub const fn mode(self) -> VideoMode {
        self.mode
    }

    /// Map a panel-normalized point (`0.0..=1.0`, as `input-touch` reports it) into the
    /// stream's pixel space.
    ///
    /// `None` when the point lands on a letterbox bar: there is no pixel of the source
    /// there, and sending the nearest edge instead would make the bars behave like a
    /// sticky border.
    #[must_use]
    pub fn map_from_panel(self, x: f32, y: f32) -> Option<SourcePixel> {
        // The picture is centred, so the bar on each side is half the leftover.
        let inset_x = (1.0 - self.scale_x) / 2.0;
        let inset_y = (1.0 - self.scale_y) / 2.0;
        let in_x = (x - inset_x) / self.scale_x;
        let in_y = (y - inset_y) / self.scale_y;
        if !(0.0..=1.0).contains(&in_x) || !(0.0..=1.0).contains(&in_y) {
            return None;
        }
        // `width - 1` is the last addressable column; scaling by `width` would produce
        // the width itself at the right edge, one past the picture.
        Some(SourcePixel {
            x: to_pixel(in_x, self.mode.width),
            y: to_pixel(in_y, self.mode.height),
        })
    }
}

/// Scale a `0.0..=1.0` fraction to a pixel index in `0..extent`.
///
/// The clamp is what makes the conversion total: both bounds are already inside `u16`, so
/// nothing can truncate — the lint fires on the shape of the cast, not on a reachable
/// value.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_pixel(fraction: f32, extent: u16) -> u16 {
    (fraction * f32::from(extent.saturating_sub(1)))
        .round()
        .clamp(0.0, f32::from(u16::MAX)) as u16
}

/// One contact of a (possibly multi-touch) event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    /// The contact id, stable for the life of one finger.
    pub id: u8,
    /// Where it is, in the stream's space.
    pub at: SourcePixel,
}

/// What a scroll is measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollUnit {
    /// Pixels, normalised to the *source's* display resolution from M4 — a different
    /// space from the one touch coordinates use, and the reason to prefer notches.
    Pixels,
    /// Mouse notches. Unambiguous, and what this sink sends.
    Notch,
}

/// Which way a scroll goes. The wire has no sign — direction is a flag and the magnitude
/// is unsigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    /// Down (vertical) or right (horizontal). Wire value 0.
    DownOrRight,
    /// Up (vertical) or left (horizontal). Wire value 1.
    UpOrLeft,
}

/// A scroll amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scroll {
    /// What the amount counts.
    pub unit: ScrollUnit,
    /// Which way.
    pub direction: ScrollDirection,
    /// How much. Clamped to 13 bits on encode, because that is all the field has.
    pub amount: u16,
}

impl Scroll {
    fn bits(self) -> u16 {
        let unit = match self.unit {
            ScrollUnit::Pixels => 0u16,
            ScrollUnit::Notch => 1u16,
        };
        let direction = match self.direction {
            ScrollDirection::DownOrRight => 0u16,
            ScrollDirection::UpOrLeft => 1u16,
        };
        (unit << 14) | (direction << 13) | (self.amount.min(MAX_SCROLL_UNITS))
    }

    fn from_bits(bits: u16) -> Self {
        Self {
            unit: if (bits >> 14) & 0b11 == 1 {
                ScrollUnit::Notch
            } else {
                ScrollUnit::Pixels
            },
            direction: if bits & 0x2000 == 0 {
                ScrollDirection::DownOrRight
            } else {
                ScrollDirection::UpOrLeft
            },
            amount: bits & MAX_SCROLL_UNITS,
        }
    }
}

/// One generic input message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GenericInput {
    /// A finger touched down, or the left mouse button went down.
    TouchDown(Vec<Pointer>),
    /// A finger lifted.
    TouchUp(Vec<Pointer>),
    /// A tracked contact moved.
    TouchMove(Vec<Pointer>),
    /// A key went down. Codes are **ASCII**, not HID usages and not Windows VK, and the
    /// high byte goes first — which is why this path cannot express an arrow key, a
    /// modifier, or Escape. Use [`HidcMessage`] for a real keyboard.
    KeyDown {
        /// The first key code.
        key1: u16,
        /// A simultaneous second code, or `0`.
        key2: u16,
    },
    /// A key came up.
    KeyUp {
        /// The first key code.
        key1: u16,
        /// A simultaneous second code, or `0`.
        key2: u16,
    },
    /// A pinch. The fractional part is in units of 1/256 and is always added positively.
    Zoom {
        /// Where the gesture is centred.
        at: SourcePixel,
        /// Whole times to zoom, unsigned.
        integer: u8,
        /// Sixteenths… no: 1/256ths.
        fraction: u8,
    },
    /// A vertical scroll.
    VerticalScroll(Scroll),
    /// A horizontal scroll.
    HorizontalScroll(Scroll),
    /// A rotation in radians. `integer` is signed and negative means clockwise; the
    /// fraction is always added positively, so −0.5 rad is `integer = -1, fraction = 128`.
    Rotate {
        /// The whole radians, signed.
        integer: i8,
        /// 1/256ths, added positively.
        fraction: u8,
    },
}

impl GenericInput {
    /// The type id.
    ///
    /// The Qualcomm UIBC patents publish an earlier draft of this table shifted by one —
    /// Touch Down at 1 rather than 0, and no Rotate at all. This is the shipped
    /// assignment, which MiracleCast's enum agrees with.
    #[must_use]
    pub const fn type_id(&self) -> u8 {
        match self {
            Self::TouchDown(_) => 0,
            Self::TouchUp(_) => 1,
            Self::TouchMove(_) => 2,
            Self::KeyDown { .. } => 3,
            Self::KeyUp { .. } => 4,
            Self::Zoom { .. } => 5,
            Self::VerticalScroll(_) => 6,
            Self::HorizontalScroll(_) => 7,
            Self::Rotate { .. } => 8,
        }
    }

    /// The describe payload — what the *inner* length counts.
    fn describe(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::TouchDown(p) | Self::TouchUp(p) | Self::TouchMove(p) => {
                out.push(u8::try_from(p.len()).unwrap_or(u8::MAX));
                for pointer in p {
                    out.push(pointer.id);
                    out.extend_from_slice(&pointer.at.x().to_be_bytes());
                    out.extend_from_slice(&pointer.at.y().to_be_bytes());
                }
            }
            Self::KeyDown { key1, key2 } | Self::KeyUp { key1, key2 } => {
                out.push(0x00); // reserved
                out.extend_from_slice(&key1.to_be_bytes());
                out.extend_from_slice(&key2.to_be_bytes());
            }
            Self::Zoom {
                at,
                integer,
                fraction,
            } => {
                out.extend_from_slice(&at.x().to_be_bytes());
                out.extend_from_slice(&at.y().to_be_bytes());
                out.push(*integer);
                out.push(*fraction);
            }
            Self::VerticalScroll(s) | Self::HorizontalScroll(s) => {
                out.extend_from_slice(&s.bits().to_be_bytes());
            }
            Self::Rotate { integer, fraction } => {
                out.push((*integer).cast_unsigned());
                out.push(*fraction);
            }
        }
        out
    }

    /// Encode this message, inner length included.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let describe = self.describe();
        let mut out = Vec::with_capacity(describe.len() + 3);
        out.push(self.type_id());
        out.extend_from_slice(
            &u16::try_from(describe.len())
                .unwrap_or(u16::MAX)
                .to_be_bytes(),
        );
        out.extend_from_slice(&describe);
        out
    }

    /// Decode one message, returning it and how many bytes it consumed.
    ///
    /// Present so the encoder can be round-trip tested against itself and so a capture can
    /// be replayed; a sink never receives these.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<(Self, usize)> {
        let header = bytes.get(..3)?;
        let len = usize::from(u16::from_be_bytes([header[1], header[2]]));
        let describe = bytes.get(3..3 + len)?;
        let consumed = 3 + len;
        let pointers = |d: &[u8]| -> Option<Vec<Pointer>> {
            let count = usize::from(*d.first()?);
            (0..count)
                .map(|i| {
                    let p = d.get(1 + i * 5..6 + i * 5)?;
                    Some(Pointer {
                        id: p[0],
                        at: SourcePixel {
                            x: u16::from_be_bytes([p[1], p[2]]),
                            y: u16::from_be_bytes([p[3], p[4]]),
                        },
                    })
                })
                .collect()
        };
        let keys = |d: &[u8]| -> Option<(u16, u16)> {
            let b = d.get(..5)?;
            Some((
                u16::from_be_bytes([b[1], b[2]]),
                u16::from_be_bytes([b[3], b[4]]),
            ))
        };
        let message = match header[0] {
            0 => Self::TouchDown(pointers(describe)?),
            1 => Self::TouchUp(pointers(describe)?),
            2 => Self::TouchMove(pointers(describe)?),
            3 => {
                let (key1, key2) = keys(describe)?;
                Self::KeyDown { key1, key2 }
            }
            4 => {
                let (key1, key2) = keys(describe)?;
                Self::KeyUp { key1, key2 }
            }
            5 => {
                let d = describe.get(..6)?;
                Self::Zoom {
                    at: SourcePixel {
                        x: u16::from_be_bytes([d[0], d[1]]),
                        y: u16::from_be_bytes([d[2], d[3]]),
                    },
                    integer: d[4],
                    fraction: d[5],
                }
            }
            6 | 7 => {
                let d = describe.get(..2)?;
                let scroll = Scroll::from_bits(u16::from_be_bytes([d[0], d[1]]));
                if header[0] == 6 {
                    Self::VerticalScroll(scroll)
                } else {
                    Self::HorizontalScroll(scroll)
                }
            }
            8 => {
                let d = describe.get(..2)?;
                Self::Rotate {
                    integer: d[0].cast_signed(),
                    fraction: d[1],
                }
            }
            _ => return None,
        };
        Some((message, consumed))
    }
}

/// Where a HID report came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HidInputPath {
    /// Infrared.
    Infrared,
    /// USB. What a panel's own touch controller looks like, and what lazycast sends.
    Usb,
    /// Bluetooth. Spelled `BT` in the RTSP `hidc_cap_list`, and `2` here.
    Bluetooth,
    /// Zigbee.
    Zigbee,
    /// Wi-Fi.
    WiFi,
    /// Anything else.
    Other(u8),
}

impl HidInputPath {
    const fn wire(self) -> u8 {
        match self {
            Self::Infrared => 0,
            Self::Usb => 1,
            Self::Bluetooth => 2,
            Self::Zigbee => 3,
            Self::WiFi => 4,
            Self::Other(v) => v,
        }
    }
}

/// What kind of device a HID report is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HidType {
    /// Keyboard.
    Keyboard,
    /// Mouse.
    Mouse,
    /// Single touch.
    SingleTouch,
    /// Multi touch.
    MultiTouch,
    /// Joystick.
    Joystick,
    /// Camera.
    Camera,
    /// Gesture.
    Gesture,
    /// Remote controller.
    RemoteController,
    /// Anything else.
    Other(u8),
}

impl HidType {
    const fn wire(self) -> u8 {
        match self {
            Self::Keyboard => 0,
            Self::Mouse => 1,
            Self::SingleTouch => 2,
            Self::MultiTouch => 3,
            Self::Joystick => 4,
            Self::Camera => 5,
            Self::Gesture => 6,
            Self::RemoteController => 7,
            Self::Other(v) => v,
        }
    }
}

/// Whether a HIDC message carries a report or the descriptor that explains reports.
///
/// A sink *should* register the descriptor for each path/type pair before sending reports
/// on it — unless the reports match USB HID's default boot keyboard and mouse, in which
/// case the spec says registration can be skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidUsage {
    /// A HID input report.
    Report,
    /// A HID report descriptor.
    Descriptor,
}

impl HidUsage {
    const fn wire(self) -> u8 {
        match self {
            Self::Report => 0,
            Self::Descriptor => 1,
        }
    }
}

/// A HIDC message: a raw HID report, or the descriptor for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidcMessage {
    /// Which transport the device is on.
    pub path: HidInputPath,
    /// What kind of device.
    pub hid_type: HidType,
    /// Report or descriptor.
    pub usage: HidUsage,
    /// The bytes.
    pub value: Vec<u8>,
}

impl HidcMessage {
    /// Encode the body — everything after the UIBC header.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.value.len() + 5);
        out.push(self.path.wire());
        out.push(self.hid_type.wire());
        out.push(self.usage.wire());
        out.extend_from_slice(
            &u16::try_from(self.value.len())
                .unwrap_or(u16::MAX)
                .to_be_bytes(),
        );
        out.extend_from_slice(&self.value);
        out
    }
}

/// A complete UIBC frame, ready for the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UibcFrame {
    /// Which body follows.
    pub category: InputCategory,
    /// The low 16 bits of the RTP timestamp of the frame that was on screen when the
    /// input happened.
    ///
    /// Optional on the wire (the `T` bit). Worth sending: it lets the source compensate
    /// input latency, and it is a free end-to-end latency probe for us — the difference
    /// between what we stamp and what the source acts on is measurable.
    pub timestamp: Option<u16>,
    /// The body, already encoded.
    pub body: Vec<u8>,
}

impl UibcFrame {
    /// One or more generic input messages in a frame.
    #[must_use]
    pub fn generic(messages: &[GenericInput], timestamp: Option<u16>) -> Self {
        Self {
            category: InputCategory::Generic,
            timestamp,
            body: messages.iter().flat_map(GenericInput::encode).collect(),
        }
    }

    /// A HIDC frame.
    #[must_use]
    pub fn hidc(message: &HidcMessage, timestamp: Option<u16>) -> Self {
        Self {
            category: InputCategory::Hidc,
            timestamp,
            body: message.encode(),
        }
    }

    /// Serialize.
    ///
    /// The `Length` field counts *everything* — this header, the body, and the pad byte
    /// — which is the rule three independent implementations confirm and the one most
    /// often got wrong.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let header_len = if self.timestamp.is_some() {
            HEADER_LEN_TIMESTAMPED
        } else {
            HEADER_LEN
        };
        // "should be padded up to an integer multiple of 16 bits".
        let total = header_len + self.body.len();
        let padded = total + (total % 2);
        let mut out = Vec::with_capacity(padded);
        // Version 0b000 in bits 7:5; the T bit is bit 4.
        out.push(if self.timestamp.is_some() { 0x10 } else { 0x00 });
        out.push(self.category.wire());
        out.extend_from_slice(&u16::try_from(padded).unwrap_or(u16::MAX).to_be_bytes());
        if let Some(ts) = self.timestamp {
            out.extend_from_slice(&ts.to_be_bytes());
        }
        out.extend_from_slice(&self.body);
        out.resize(padded, 0);
        out
    }

    /// Parse one frame from the front of a byte stream.
    ///
    /// Returns the frame and how many bytes it consumed, or `None` if the buffer does not
    /// hold a whole frame yet — the stream has no delimiter but this length, so the reader
    /// must be a framer tolerant of partial reads and coalesced messages.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<(Self, usize)> {
        let header = bytes.get(..HEADER_LEN)?;
        let timestamped = header[0] & 0x10 != 0;
        let category = InputCategory::from_wire(header[1])?;
        let declared = usize::from(u16::from_be_bytes([header[2], header[3]]));
        if declared < HEADER_LEN || bytes.len() < declared {
            return None;
        }
        let (timestamp, body_start) = if timestamped {
            let ts = bytes.get(HEADER_LEN..HEADER_LEN_TIMESTAMPED)?;
            (
                Some(u16::from_be_bytes([ts[0], ts[1]])),
                HEADER_LEN_TIMESTAMPED,
            )
        } else {
            (None, HEADER_LEN)
        };
        Some((
            Self {
                category,
                timestamp,
                body: bytes.get(body_start..declared)?.to_vec(),
            },
            declared,
        ))
    }

    /// The generic messages in this frame's body, if it is a generic frame.
    #[must_use]
    pub fn generic_messages(&self) -> Option<Vec<GenericInput>> {
        if self.category != InputCategory::Generic {
            return None;
        }
        let mut out = Vec::new();
        let mut rest = self.body.as_slice();
        while rest.len() >= 3 {
            let (message, consumed) = GenericInput::decode(rest)?;
            out.push(message);
            rest = rest.get(consumed..)?;
        }
        Some(out)
    }
}

/// How many encoded frames may wait for the socket before input is dropped.
///
/// Small on purpose. UIBC is a *live* input channel: a touch that has been queued behind
/// thirty-one others is a touch the person has already given up on, and delivering it
/// late is worse than not delivering it — the source acts on it against a screen that has
/// moved on. Dropping the newest also keeps a down/up pair from being split by a queue
/// that filled between them, since one full queue drops the rest of that gesture too.
pub const UIBC_QUEUE: usize = 32;

/// The panel driving a Miracast source over the negotiated back-channel.
///
/// This is the sink end of [`castaway_core::TouchSurface`] for WFD, and the only consumer
/// of this module's encoder. What it owns is the translation nobody else can do: the
/// router speaks panel-normalized coordinates and knows nothing about the stream's
/// resolution, the source speaks source pixels and knows nothing about the panel, and
/// [`VideoGeometry`] is what sits between them — including the part where a touch on a
/// letterbox bar is not a point in the picture at all and must not be sent as the nearest
/// edge.
///
/// Frames go out through a channel rather than a socket: this is called from the thread
/// that owns the glass, and that thread must never wait on a network peer (ground rule
/// 4). The actor owns the socket at the other end.
pub struct UibcSurface {
    mode: VideoMode,
    /// Recomputed whenever the router says the panel changed size — see
    /// [`TouchSurface::panel_resized`]. Starts fullscreen, which is exact whenever the
    /// stream and the panel share an aspect ratio and wrong by the bars when they do not.
    geometry: Mutex<VideoGeometry>,
    frames: mpsc::Sender<Vec<u8>>,
    contacts: Mutex<Contacts>,
}

/// The live contacts, and their UIBC ids.
///
/// UIBC numbers a pointer in one byte while the router's contact id is a `u64` unique
/// across origins, so the mapping is real and has to be kept: a `TouchUp` naming an id
/// the source never saw go down leaves the source's own tracking wrong for the rest of
/// the session.
#[derive(Debug)]
struct Contacts {
    /// Router contact id to the UIBC pointer id and its last in-picture position.
    live: HashMap<u64, (u8, SourcePixel)>,
    /// Which of the 256 ids are taken.
    used: [bool; 256],
}

impl Default for Contacts {
    fn default() -> Self {
        Self {
            live: HashMap::new(),
            used: [false; 256],
        }
    }
}

impl Contacts {
    fn claim(&mut self, contact: u64, at: SourcePixel) -> Option<u8> {
        if let Some((id, last)) = self.live.get_mut(&contact) {
            *last = at;
            return Some(*id);
        }
        let id = u8::try_from(self.used.iter().position(|taken| !taken)?).ok()?;
        self.used[usize::from(id)] = true;
        self.live.insert(contact, (id, at));
        Some(id)
    }

    fn release(&mut self, contact: u64) -> Option<(u8, SourcePixel)> {
        let (id, at) = self.live.remove(&contact)?;
        self.used[usize::from(id)] = false;
        Some((id, at))
    }

    fn drain(&mut self) -> Vec<(u8, SourcePixel)> {
        self.used = [false; 256];
        self.live.drain().map(|(_, entry)| entry).collect()
    }
}

impl UibcSurface {
    /// A surface for `mode`, queueing frames onto `frames`.
    #[must_use]
    pub fn new(mode: VideoMode, frames: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            mode,
            geometry: Mutex::new(VideoGeometry::fullscreen(mode)),
            frames,
            contacts: Mutex::new(Contacts::default()),
        }
    }

    /// The geometry touches are currently mapped through.
    #[must_use]
    pub fn geometry(&self) -> VideoGeometry {
        self.geometry
            .lock()
            .map_or_else(|_| VideoGeometry::fullscreen(self.mode), |g| *g)
    }

    /// Queue one generic input message, dropping it if the socket is behind.
    fn send(&self, message: &GenericInput) {
        // No timestamp: it is the low 16 bits of the RTP stamp of the frame that was on
        // screen, and this side of the boundary has no access to it. The field is
        // optional on the wire precisely so a sink that cannot supply it may omit it —
        // see `UibcFrame::timestamp` for what is lost by doing so.
        let frame = UibcFrame::generic(std::slice::from_ref(message), None).encode();
        if self.frames.try_send(frame).is_err() {
            debug!("miracast: UIBC back-channel is behind; dropping an input frame");
        }
    }
}

impl TouchSurface for UibcSurface {
    fn touch(&self, touch: SurfaceTouch) {
        let Ok(mut contacts) = self.contacts.lock() else {
            return;
        };
        let inside = self.geometry().map_from_panel(touch.x, touch.y);
        match touch.phase {
            TouchPhase::Down => {
                // A press that starts on a bar is not a press on the picture, and there is
                // no id to allocate for it: the matching release will find nothing live
                // and be dropped too, which is the consistent outcome.
                let Some(at) = inside else { return };
                let Some(id) = contacts.claim(touch.contact, at) else {
                    debug!("miracast: more simultaneous contacts than UIBC can number");
                    return;
                };
                self.send(&GenericInput::TouchDown(vec![Pointer { id, at }]));
            }
            TouchPhase::Move => {
                // Only for a contact the source has seen go down, and only while it is
                // over the picture: a finger dragged onto a bar stops reporting rather
                // than sticking to the edge, and picks up again if it comes back.
                let (Some(at), Some(&(id, _))) = (inside, contacts.live.get(&touch.contact)) else {
                    return;
                };
                contacts.claim(touch.contact, at);
                self.send(&GenericInput::TouchMove(vec![Pointer { id, at }]));
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                let Some((id, last)) = contacts.release(touch.contact) else {
                    return;
                };
                // The last position *inside the picture*, not wherever the finger left
                // from: a release off the edge would otherwise report a pixel the drag
                // never reached, and a cancel carries no meaningful position at all.
                self.send(&GenericInput::TouchUp(vec![Pointer {
                    id,
                    at: inside.unwrap_or(last),
                }]));
            }
        }
    }

    fn panel_resized(&self, width: u32, height: u32) {
        let geometry = VideoGeometry::letterboxed(self.mode, width, height);
        if let Ok(mut slot) = self.geometry.lock() {
            *slot = geometry;
        }
        debug!(
            width,
            height,
            mode = %self.mode,
            "miracast: UIBC touch mapping follows the panel"
        );
    }

    fn cancel_all(&self) {
        let Ok(mut contacts) = self.contacts.lock() else {
            return;
        };
        let live = contacts.drain();
        drop(contacts);
        if live.is_empty() {
            return;
        }
        // One frame with every contact in it — the shape UIBC's pointer list exists for,
        // and one message rather than N racing each other onto the socket.
        let pointers: Vec<Pointer> = live
            .into_iter()
            .map(|(id, at)| Pointer { id, at })
            .collect();
        debug!(
            contacts = pointers.len(),
            "miracast: releasing UIBC contacts; the panel is no longer ours"
        );
        self.send(&GenericInput::TouchUp(pointers));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn geometry() -> VideoGeometry {
        VideoGeometry::fullscreen(VideoMode::new(1920, 1080, 60, false))
    }

    #[test]
    fn the_worked_touch_down_matches_the_specs_own_bytes() {
        // The fully worked example from the notes' §5.4: a single touch at (960, 540),
        // no timestamp — describe 6, generic message 9, frame 13, padded to 14.
        let at = geometry()
            .map_from_panel(960.0 / 1919.0, 540.0 / 1079.0)
            .unwrap();
        assert_eq!((at.x(), at.y()), (960, 540));
        let frame = UibcFrame::generic(
            &[GenericInput::TouchDown(vec![Pointer { id: 0, at }])],
            None,
        );
        assert_eq!(hex(&frame.encode()), "0000000e000006010003c0021c00");
    }

    #[test]
    fn the_outer_length_counts_the_header_and_the_padding() {
        // Three independent implementations confirm this, and it is the classic mistake.
        let at = geometry().map_from_panel(0.0, 0.0).unwrap();
        let bytes = UibcFrame::generic(
            &[GenericInput::TouchDown(vec![Pointer { id: 0, at }])],
            None,
        )
        .encode();
        assert_eq!(bytes.len(), 14);
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 14);
        assert_eq!(*bytes.last().unwrap(), 0, "the pad byte is counted");
    }

    #[test]
    fn the_inner_length_counts_only_the_describe_payload() {
        // Two length conventions with the same name, one nested in the other.
        let at = geometry().map_from_panel(0.5, 0.5).unwrap();
        let message = GenericInput::TouchDown(vec![Pointer { id: 0, at }]);
        let encoded = message.encode();
        assert_eq!(u16::from_be_bytes([encoded[1], encoded[2]]), 6);
        assert_eq!(encoded.len(), 9);
    }

    #[test]
    fn a_multi_touch_describe_is_five_n_plus_one() {
        let g = geometry();
        let pointers: Vec<Pointer> = (0..3)
            .map(|i| Pointer {
                id: i,
                at: g.map_from_panel(0.25 * f32::from(i + 1), 0.5).unwrap(),
            })
            .collect();
        let encoded = GenericInput::TouchMove(pointers.clone()).encode();
        assert_eq!(u16::from_be_bytes([encoded[1], encoded[2]]), 16);
        let (back, consumed) = GenericInput::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(back, GenericInput::TouchMove(pointers));
    }

    #[test]
    fn a_touch_on_the_letterbox_bar_is_not_a_point_in_the_stream() {
        // A 4:3 stream on the 16:9 panel has bars down both sides. Clamping to the edge
        // instead of refusing would make the bars behave like a sticky border.
        let g = VideoGeometry::letterboxed(VideoMode::new(1024, 768, 60, false), 3840, 2160);
        assert!(g.map_from_panel(0.01, 0.5).is_none(), "left bar");
        assert!(g.map_from_panel(0.99, 0.5).is_none(), "right bar");
        // The picture is 0.75 of the panel's width, centred, so half way across the panel
        // is half way across the picture.
        let middle = g.map_from_panel(0.5, 0.5).unwrap();
        assert_eq!(middle.x(), 512);
        assert_eq!(middle.y(), 384);
    }

    #[test]
    fn a_4k_panel_showing_1080p_maps_the_far_corner_to_the_last_pixel() {
        // The classic bug this prevents: sending panel coordinates gives a pointer that
        // moves at half speed and only reaches the top-left quadrant.
        let g = VideoGeometry::letterboxed(VideoMode::new(1920, 1080, 60, false), 3840, 2160);
        let corner = g.map_from_panel(1.0, 1.0).unwrap();
        assert_eq!((corner.x(), corner.y()), (1919, 1079));
        let origin = g.map_from_panel(0.0, 0.0).unwrap();
        assert_eq!((origin.x(), origin.y()), (0, 0));
    }

    #[test]
    fn the_timestamp_bit_adds_two_bytes_and_changes_the_length() {
        let at = geometry().map_from_panel(0.0, 0.0).unwrap();
        let frame = UibcFrame::generic(
            &[GenericInput::TouchUp(vec![Pointer { id: 0, at }])],
            Some(0x2AB6),
        );
        let bytes = frame.encode();
        assert_eq!(bytes[0] & 0x10, 0x10, "the T bit");
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 16);
        assert_eq!(&bytes[4..6], &[0x2A, 0xB6]);
        let (back, consumed) = UibcFrame::parse(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(back.timestamp, Some(0x2AB6));
    }

    #[test]
    fn a_key_frame_carries_ascii_high_byte_first() {
        // Not HID usages and not Windows VK — which is exactly why this path cannot send
        // an arrow key, and why HIDC exists.
        let encoded = GenericInput::KeyDown {
            key1: u16::from(b'A'),
            key2: 0,
        }
        .encode();
        assert_eq!(hex(&encoded), "0300050000410000");
        let (back, _) = GenericInput::decode(&encoded).unwrap();
        assert_eq!(back, GenericInput::KeyDown { key1: 65, key2: 0 });
    }

    #[test]
    fn scroll_packs_unit_direction_and_an_unsigned_magnitude() {
        // Not a signed integer and not fixed point — the sign is a direction flag in bit
        // 13 and the magnitude is 13 unsigned bits.
        let scroll = Scroll {
            unit: ScrollUnit::Notch,
            direction: ScrollDirection::UpOrLeft,
            amount: 3,
        };
        assert_eq!(scroll.bits(), 0x4000 | 0x2000 | 3);
        assert_eq!(Scroll::from_bits(scroll.bits()), scroll);
        // The magnitude saturates rather than wrapping into the flags.
        let huge = Scroll {
            amount: u16::MAX,
            ..scroll
        };
        assert_eq!(huge.bits() & MAX_SCROLL_UNITS, MAX_SCROLL_UNITS);
        assert_eq!(huge.bits() >> 14, 1, "the unit bits survive");
    }

    #[test]
    fn a_negative_rotation_adds_its_fraction_positively() {
        // -0.5 rad is integer -1 plus 128/256, not integer 0 with a negative fraction.
        let encoded = GenericInput::Rotate {
            integer: -1,
            fraction: 128,
        }
        .encode();
        assert_eq!(hex(&encoded), "080002ff80");
        let (back, _) = GenericInput::decode(&encoded).unwrap();
        assert_eq!(
            back,
            GenericInput::Rotate {
                integer: -1,
                fraction: 128
            }
        );
    }

    #[test]
    fn the_lazycast_mouse_descriptor_frame_reproduces_its_wire_bytes() {
        // Real bytes a shipping Windows source accepts (notes §5.6): a USB mouse report
        // descriptor, T=1. The arithmetic 6 + 3 + 2 + 51 = 62 is what validates the whole
        // layout — header, HIDC prologue, value length, value.
        let value = vec![0xABu8; 51];
        let frame = UibcFrame::hidc(
            &HidcMessage {
                path: HidInputPath::Usb,
                hid_type: HidType::Mouse,
                usage: HidUsage::Descriptor,
                value,
            },
            Some(0x2AB6),
        );
        let bytes = frame.encode();
        assert_eq!(bytes.len(), 62);
        assert_eq!(&bytes[..4], &[0x10, 0x01, 0x00, 0x3E]);
        assert_eq!(&bytes[4..6], &[0x2A, 0xB6]);
        assert_eq!(&bytes[6..11], &[0x01, 0x01, 0x01, 0x00, 0x33]);
    }

    #[test]
    fn an_odd_length_hidc_frame_is_padded_and_the_pad_is_counted() {
        // lazycast's gesture registration: 6 header + 3 + 2 + 196 = 207, odd, so the
        // frame carries 0x00D0 = 208 — direct wire proof that the pad byte counts.
        let frame = UibcFrame::hidc(
            &HidcMessage {
                path: HidInputPath::Usb,
                hid_type: HidType::Gesture,
                usage: HidUsage::Descriptor,
                value: vec![0u8; 196],
            },
            Some(0x2AB6),
        );
        let bytes = frame.encode();
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 208);
        assert_eq!(bytes.len(), 208);
    }

    #[test]
    fn several_messages_share_one_frame() {
        let g = geometry();
        let messages = vec![
            GenericInput::TouchMove(vec![Pointer {
                id: 0,
                at: g.map_from_panel(0.1, 0.1).unwrap(),
            }]),
            GenericInput::VerticalScroll(Scroll {
                unit: ScrollUnit::Notch,
                direction: ScrollDirection::DownOrRight,
                amount: 1,
            }),
        ];
        let bytes = UibcFrame::generic(&messages, None).encode();
        let (frame, consumed) = UibcFrame::parse(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(frame.generic_messages().unwrap(), messages);
    }

    #[test]
    fn a_partial_frame_yields_nothing_rather_than_a_wrong_one() {
        // The stream has no delimiter but the length, so the reader has to tolerate both
        // partial reads and coalesced messages.
        let at = geometry().map_from_panel(0.5, 0.5).unwrap();
        let bytes = UibcFrame::generic(
            &[GenericInput::TouchDown(vec![Pointer { id: 0, at }])],
            None,
        )
        .encode();
        for cut in 1..bytes.len() {
            assert!(UibcFrame::parse(&bytes[..cut]).is_none(), "cut at {cut}");
        }
        assert!(UibcFrame::parse(&bytes).is_some());
    }

    #[test]
    fn two_coalesced_frames_are_read_one_at_a_time() {
        let at = geometry().map_from_panel(0.5, 0.5).unwrap();
        let one = UibcFrame::generic(
            &[GenericInput::TouchDown(vec![Pointer { id: 0, at }])],
            None,
        )
        .encode();
        let mut stream = one.clone();
        stream.extend_from_slice(
            &UibcFrame::generic(&[GenericInput::TouchUp(vec![Pointer { id: 0, at }])], None)
                .encode(),
        );
        let (first, consumed) = UibcFrame::parse(&stream).unwrap();
        assert_eq!(consumed, one.len());
        assert_eq!(first.generic_messages().unwrap().len(), 1);
        let (second, _) = UibcFrame::parse(&stream[consumed..]).unwrap();
        assert!(matches!(
            second.generic_messages().unwrap().as_slice(),
            [GenericInput::TouchUp(_)]
        ));
    }

    #[test]
    fn a_declared_length_below_the_header_is_refused() {
        assert!(UibcFrame::parse(&[0x00, 0x00, 0x00, 0x02]).is_none());
    }

    /// The surface's own end-to-end: a drag across the glass becomes UIBC frames in the
    /// stream's pixel space, and a touch on a letterbox bar becomes nothing.
    #[tokio::test]
    async fn a_drag_on_the_glass_becomes_uibc_frames_in_source_pixels() {
        let mode = VideoMode::new(1920, 1080, 60, false);
        let (tx, mut rx) = mpsc::channel(16);
        let surface = UibcSurface::new(mode, tx);
        // A 16:9 stream on a 16:9 panel: no bars, so the mapping is the identity scale.
        surface.panel_resized(3840, 2160);

        let at = |phase, x, y| SurfaceTouch {
            contact: 7,
            phase,
            x,
            y,
        };
        surface.touch(at(TouchPhase::Down, 0.5, 0.5));
        surface.touch(at(TouchPhase::Move, 0.25, 0.75));
        surface.touch(at(TouchPhase::Up, 0.25, 0.75));

        let decode = |bytes: &[u8]| {
            let (frame, _) = UibcFrame::parse(bytes).expect("a frame");
            frame.generic_messages().expect("generic input")
        };

        let down = rx.try_recv().expect("a down frame");
        assert_eq!(
            decode(&down),
            vec![GenericInput::TouchDown(vec![Pointer {
                id: 0,
                at: SourcePixel { x: 960, y: 540 },
            }])]
        );
        let moved = rx.try_recv().expect("a move frame");
        assert_eq!(
            decode(&moved),
            vec![GenericInput::TouchMove(vec![Pointer {
                id: 0,
                at: SourcePixel { x: 480, y: 809 },
            }])]
        );
        let up = rx.try_recv().expect("an up frame");
        assert_eq!(
            decode(&up),
            vec![GenericInput::TouchUp(vec![Pointer {
                id: 0,
                at: SourcePixel { x: 480, y: 809 },
            }])]
        );
        assert!(rx.try_recv().is_err(), "and nothing else");
    }

    #[tokio::test]
    async fn a_touch_on_a_letterbox_bar_is_not_a_touch_on_the_picture() {
        // A 4:3 stream on the 16:9 panel has bars down both sides. Sending the nearest
        // edge instead would make them behave like a sticky border.
        let mode = VideoMode::new(640, 480, 60, false);
        let (tx, mut rx) = mpsc::channel(16);
        let surface = UibcSurface::new(mode, tx);
        surface.panel_resized(3840, 2160);

        surface.touch(SurfaceTouch {
            contact: 1,
            phase: TouchPhase::Down,
            x: 0.02,
            y: 0.5,
        });
        assert!(rx.try_recv().is_err(), "the bar is not part of the stream");

        // …and the middle of the panel still is.
        surface.touch(SurfaceTouch {
            contact: 2,
            phase: TouchPhase::Down,
            x: 0.5,
            y: 0.5,
        });
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn losing_the_glass_releases_every_contact_in_one_frame() {
        // A session that never hears the `Up` believes a finger is down for the rest of
        // its life, and the person who lifted it has no way to say otherwise.
        let mode = VideoMode::new(1920, 1080, 60, false);
        let (tx, mut rx) = mpsc::channel(16);
        let surface = UibcSurface::new(mode, tx);
        surface.panel_resized(1920, 1080);

        for contact in 0..3u64 {
            surface.touch(SurfaceTouch {
                contact,
                phase: TouchPhase::Down,
                x: 0.5,
                y: 0.5,
            });
            rx.try_recv().expect("a down frame");
        }
        surface.cancel_all();

        let (frame, _) = UibcFrame::parse(&rx.try_recv().expect("a release")).expect("a frame");
        let messages = frame.generic_messages().expect("generic");
        let [GenericInput::TouchUp(pointers)] = messages.as_slice() else {
            panic!("one TouchUp naming every contact")
        };
        assert_eq!(pointers.len(), 3);
        // Distinct ids, because the source tracks them and a repeat would merge fingers.
        let mut ids: Vec<u8> = pointers.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1, 2]);

        // …and a second cancel says nothing, having nothing left to say.
        surface.cancel_all();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_id_is_reused_only_after_its_finger_lifts() {
        let mode = VideoMode::new(1920, 1080, 60, false);
        let (tx, mut rx) = mpsc::channel(16);
        let surface = UibcSurface::new(mode, tx);
        surface.panel_resized(1920, 1080);
        let down = |contact| SurfaceTouch {
            contact,
            phase: TouchPhase::Down,
            x: 0.5,
            y: 0.5,
        };
        let id_of = |bytes: &[u8]| {
            let (frame, _) = UibcFrame::parse(bytes).expect("a frame");
            match frame.generic_messages().expect("generic").first() {
                Some(GenericInput::TouchDown(p) | GenericInput::TouchUp(p)) => p[0].id,
                other => panic!("unexpected {other:?}"),
            }
        };

        surface.touch(down(10));
        assert_eq!(id_of(&rx.try_recv().unwrap()), 0);
        surface.touch(down(11));
        assert_eq!(id_of(&rx.try_recv().unwrap()), 1, "a second live finger");

        surface.touch(SurfaceTouch {
            contact: 10,
            phase: TouchPhase::Up,
            x: 0.5,
            y: 0.5,
        });
        assert_eq!(id_of(&rx.try_recv().unwrap()), 0);
        surface.touch(down(12));
        assert_eq!(id_of(&rx.try_recv().unwrap()), 0, "0 is free again");
    }

    #[tokio::test]
    async fn a_release_for_a_contact_that_never_went_down_says_nothing() {
        // A press that started on a bar allocated no id. Its release must not invent one:
        // an `Up` for a pointer the source never saw leaves its tracking wrong for good.
        let mode = VideoMode::new(1920, 1080, 60, false);
        let (tx, mut rx) = mpsc::channel(16);
        let surface = UibcSurface::new(mode, tx);
        surface.panel_resized(1920, 1080);
        surface.touch(SurfaceTouch {
            contact: 99,
            phase: TouchPhase::Up,
            x: 0.5,
            y: 0.5,
        });
        assert!(rx.try_recv().is_err());
    }
}
