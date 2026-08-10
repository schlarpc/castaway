//! Protocol v4 message bodies: verified FlatBuffers in, rich owned types at the
//! boundary (ground rule 1), and builders for everything the receiver says back.
//!
//! The direction rules, error kinds and relay stripping mirror the reference
//! receiver exactly (see the #248 study notes); the captured transcripts in
//! `tests/fixtures/sdk-0.3.0-v4-*.jsonl` replay through [`parse_flatbuf`] in
//! `tests/real_sender_transcripts.rs`.

use std::time::Duration;

use fcast_flatbuf::{flat, FlatBufferBuilder, WIPOffset};

use crate::error::FCastError;
use crate::wire::{Frame, Opcode};

/// v4's packet ceiling (512 KiB), replacing the v1-v3 32 000-byte one once a
/// session negotiates v4.
pub const MAX_PACKET_V4: usize = fcast_flatbuf::MAX_PACKET_SIZE;

/// Who a peer says it is (`DeviceInfo` on the wire).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Human-readable device name.
    pub display_name: Option<String>,
    /// Application name.
    pub app_name: Option<String>,
    /// Application version.
    pub app_version: Option<String>,
}

impl DeviceInfo {
    fn from_flat(info: flat::DeviceInfo<'_>) -> Self {
        Self {
            display_name: info.display_name().map(str::to_owned),
            app_name: info.app_name().map(str::to_owned),
            app_version: info.app_version().map(str::to_owned),
        }
    }
}

/// One v4 media item, owned. `raw` retains the *whole packet body* it came from
/// so relays can re-serialize with the reference's stripping rules (headers off,
/// typed metadata off, everything else kept) without an owned model of the
/// recursive `extra_metadata` union.
#[derive(Debug, Clone, PartialEq)]
pub struct V4MediaItem {
    /// MIME type of the container at `source_url`.
    pub container: String,
    /// Where to fetch the media.
    pub source_url: String,
    /// Start position.
    pub start_time: Option<Duration>,
    /// Volume 0.0-1.0, when the sender says.
    pub volume: Option<f32>,
    /// Speed factor, when the sender says.
    pub speed: Option<f32>,
    /// HTTP request headers for the fetch. Kept for us; stripped from relays.
    pub headers: Vec<(String, String)>,
    /// Display title.
    pub title: Option<String>,
    /// Cover art URL.
    pub thumbnail_url: Option<String>,
}

impl V4MediaItem {
    fn from_flat(item: flat::MediaItem<'_>) -> Self {
        Self {
            container: item.container().to_owned(),
            source_url: item.source_url().to_owned(),
            start_time: item.start_time().map(|t| Duration::from_micros(t.micros())),
            volume: item.volume(),
            speed: item.speed(),
            headers: item
                .headers()
                .map(|hs| {
                    hs.iter()
                        .map(|h| (h.key().to_owned(), h.value().to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
            title: item.title().map(str::to_owned),
            thumbnail_url: item.thumbnail_url().map(str::to_owned),
        }
    }
}

/// A queue position (`QueuePosition` union).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePosition {
    /// A zero-based index.
    Index(u8),
    /// The front of the queue.
    Front,
    /// The back of the queue.
    Back,
}

/// What a `Load` carries.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadSource {
    /// One item.
    Single(V4MediaItem),
    /// A queue of up to 256 items.
    Queue {
        /// The items, each with an optional on-screen duration.
        items: Vec<(V4MediaItem, Option<Duration>)>,
        /// First item to play.
        start_index: Option<u8>,
        /// Whether finishing an item plays the next.
        autoplay: bool,
    },
}

/// A sender-legal v4 message, parsed and owned. `raw` on the variants that get
/// relayed is the whole packet body, retained for stripped re-serialization.
#[derive(Debug, Clone, PartialEq)]
pub enum V4Inbound {
    /// Load and play content.
    Load {
        /// What to load.
        source: LoadSource,
        /// The packet body, for the stripped relay.
        raw: Vec<u8>,
    },
    /// A seek: `duration` on the wire is ignored by the receiver.
    ProgressChanged {
        /// The target position.
        position: Duration,
    },
    /// Set the volume.
    VolumeChanged(f32),
    /// Pause/resume by absolute state.
    PlaybackStateChanged(flat::PlaybackState),
    /// Set the speed factor.
    SpeedChanged(f32),
    /// Who the sender is.
    SenderIntroduction(DeviceInfo),
    /// Stop and unload.
    StopPlayback,
    /// Begin a mirroring session.
    StartMirroringSession(u16),
    /// The sender's SDP offer.
    MirroringSessionDescription {
        /// Must match the active session.
        session_id: u16,
        /// The offer SDP.
        sdp: String,
    },
    /// Insert one item.
    QueueInsert {
        /// The item.
        item: V4MediaItem,
        /// Its on-screen duration.
        playback_duration: Option<Duration>,
        /// Where.
        position: QueuePosition,
        /// The packet body, for the stripped relay.
        raw: Vec<u8>,
    },
    /// Remove one item.
    QueueRemove(QueuePosition),
    /// Jump to an item.
    QueueItemSelected(QueuePosition),
    /// Select or disable a track. This receiver has no track model: a concrete
    /// id can never name a track (→ `MalformedBody`, the reference's answer),
    /// and disabling an unrendered track is vacuously done.
    ChangeTrack {
        /// `None` disables the track type.
        id: Option<u32>,
    },
    /// Add an external subtitle. Unsupported here (no subtitle rendering).
    AddSubtitleSource,
    /// Change this session's progress cadence, already rounded to 100 ms steps.
    SetProgressUpdateInterval(Duration),
    /// The sender wants to serve resources.
    CompanionHelloRequest,
    /// A resource-info answer, routed by request id.
    CompanionResourceInfoResponse {
        /// The request this answers.
        request_id: u32,
        /// The resource's MIME type.
        content_type: String,
        /// Its size, when known.
        size: Option<u64>,
    },
}

/// What parsing concluded: a message, or the typed error to send back — which is
/// an *answer*, not a session fault.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// A legal message.
    Message(V4Inbound),
    /// Answer `Error { kind, packet_num: <this packet> }` and carry on.
    Reply(flat::ErrorKind),
}

/// Round a progress interval to the nearest 100 ms step, floored at one step —
/// the reference's rule, with the overflow its version has guarded.
#[must_use]
pub fn round_progress_interval(micros: u64) -> Duration {
    const STEP_MICROS: u64 = 100_000;
    let steps = (micros / STEP_MICROS + u64::from(micros % STEP_MICROS >= STEP_MICROS / 2)).max(1);
    Duration::from_micros(steps.saturating_mul(STEP_MICROS))
}

/// Parse one verified `Flatbuf` body from a sender.
///
/// # Errors
/// [`FCastError::MalformedFlatbuf`] when the buffer fails verification or a
/// present union's member is absent — session-fatal, per the reference.
pub fn parse_flatbuf(body: &[u8]) -> Result<Parsed, FCastError> {
    let packet = fcast_flatbuf::root_as_packet(body)
        .map_err(|e| FCastError::MalformedFlatbuf(e.to_string()))?;

    macro_rules! union {
        ($accessor:expr) => {
            $accessor.ok_or_else(|| {
                FCastError::MalformedFlatbuf("union member absent for its tag".into())
            })?
        };
    }

    Ok(match packet.payload_type() {
        flat::Message::Load => {
            let load = union!(packet.payload_as_load());
            let source = match load.source_type() {
                flat::MediaSource::Single => {
                    LoadSource::Single(V4MediaItem::from_flat(union!(load.source_as_single())))
                }
                flat::MediaSource::Queue => {
                    let queue = union!(load.source_as_queue());
                    let items = queue
                        .items()
                        .iter()
                        .map(|qi| {
                            (
                                V4MediaItem::from_flat(qi.media_item()),
                                qi.playback_duration()
                                    .map(|t| Duration::from_micros(t.micros())),
                            )
                        })
                        .collect();
                    LoadSource::Queue {
                        items,
                        start_index: queue.start_index(),
                        autoplay: queue.autoplay(),
                    }
                }
                _ => return Ok(Parsed::Reply(flat::ErrorKind::MalformedBody)),
            };
            Parsed::Message(V4Inbound::Load {
                source,
                raw: body.to_vec(),
            })
        }
        flat::Message::ProgressChanged => {
            let msg = union!(packet.payload_as_progress_changed());
            match msg.position() {
                Some(t) => Parsed::Message(V4Inbound::ProgressChanged {
                    position: Duration::from_micros(t.micros()),
                }),
                None => Parsed::Reply(flat::ErrorKind::MalformedBody),
            }
        }
        flat::Message::VolumeChanged => Parsed::Message(V4Inbound::VolumeChanged(
            union!(packet.payload_as_volume_changed()).volume(),
        )),
        flat::Message::PlaybackStateChanged => Parsed::Message(V4Inbound::PlaybackStateChanged(
            union!(packet.payload_as_playback_state_changed()).state(),
        )),
        flat::Message::SpeedChanged => Parsed::Message(V4Inbound::SpeedChanged(
            union!(packet.payload_as_speed_changed()).speed(),
        )),
        flat::Message::SenderIntroduction => Parsed::Message(V4Inbound::SenderIntroduction(
            DeviceInfo::from_flat(union!(packet.payload_as_sender_introduction()).device_info()),
        )),
        flat::Message::StopPlayback => Parsed::Message(V4Inbound::StopPlayback),
        flat::Message::StartMirroringSession => Parsed::Message(V4Inbound::StartMirroringSession(
            union!(packet.payload_as_start_mirroring_session()).session_id(),
        )),
        flat::Message::MirroringSessionDescription => {
            let msg = union!(packet.payload_as_mirroring_session_description());
            Parsed::Message(V4Inbound::MirroringSessionDescription {
                session_id: msg.session_id(),
                sdp: msg.sdp().to_owned(),
            })
        }
        flat::Message::QueueInsert => {
            let msg = union!(packet.payload_as_queue_insert());
            let Some(position) = queue_position(msg.position_type(), || {
                msg.position_as_index().map(|i| i.index())
            }) else {
                return Ok(Parsed::Reply(flat::ErrorKind::MalformedBody));
            };
            Parsed::Message(V4Inbound::QueueInsert {
                item: V4MediaItem::from_flat(msg.item().media_item()),
                playback_duration: msg
                    .item()
                    .playback_duration()
                    .map(|t| Duration::from_micros(t.micros())),
                position,
                raw: body.to_vec(),
            })
        }
        flat::Message::QueueRemove => {
            let msg = union!(packet.payload_as_queue_remove());
            match queue_position(msg.position_type(), || {
                msg.position_as_index().map(|i| i.index())
            }) {
                Some(position) => Parsed::Message(V4Inbound::QueueRemove(position)),
                None => Parsed::Reply(flat::ErrorKind::MalformedBody),
            }
        }
        flat::Message::QueueItemSelected => {
            let msg = union!(packet.payload_as_queue_item_selected());
            match queue_position(msg.position_type(), || {
                msg.position_as_index().map(|i| i.index())
            }) {
                Some(position) => Parsed::Message(V4Inbound::QueueItemSelected(position)),
                None => Parsed::Reply(flat::ErrorKind::MalformedBody),
            }
        }
        flat::Message::ChangeTrack => {
            let msg = union!(packet.payload_as_change_track());
            Parsed::Message(V4Inbound::ChangeTrack { id: msg.id() })
        }
        flat::Message::AddSubtitleSource => {
            let msg = union!(packet.payload_as_add_subtitle_source());
            if msg.url().is_empty() {
                Parsed::Reply(flat::ErrorKind::MalformedBody)
            } else {
                Parsed::Message(V4Inbound::AddSubtitleSource)
            }
        }
        flat::Message::SetProgressUpdateInterval => {
            let msg = union!(packet.payload_as_set_progress_update_interval());
            match msg.interval() {
                Some(t) => Parsed::Message(V4Inbound::SetProgressUpdateInterval(
                    round_progress_interval(t.micros()),
                )),
                None => Parsed::Reply(flat::ErrorKind::MalformedBody),
            }
        }
        flat::Message::CompanionHelloRequest => Parsed::Message(V4Inbound::CompanionHelloRequest),
        flat::Message::CompanionResourceInfoResponse => {
            let msg = union!(packet.payload_as_companion_resource_info_response());
            let size = match msg.resource_size_type() {
                flat::CompanionResourceSize::Known => {
                    msg.resource_size_as_known().map(|k| k.size())
                }
                _ => None,
            };
            Parsed::Message(V4Inbound::CompanionResourceInfoResponse {
                request_id: msg.request_id(),
                content_type: msg.content_type().to_owned(),
                size,
            })
        }
        // Receiver-direction messages a confused sender might echo, and union
        // tags newer than this schema: a polite typed answer, not a fault.
        _ => Parsed::Reply(flat::ErrorKind::InvalidPayloadType),
    })
}

fn queue_position(
    ty: flat::QueuePosition,
    index: impl FnOnce() -> Option<u8>,
) -> Option<QueuePosition> {
    match ty {
        flat::QueuePosition::Index => index().map(QueuePosition::Index),
        flat::QueuePosition::Front => Some(QueuePosition::Front),
        flat::QueuePosition::Back => Some(QueuePosition::Back),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Outbound builders. Each returns a ready-to-write Frame (opcode 20).
// ---------------------------------------------------------------------------

fn finish(
    mut builder: FlatBufferBuilder<'_>,
    ty: flat::Message,
    payload: WIPOffset<flatbuffers::UnionWIPOffset>,
) -> Frame {
    let packet = flat::Packet::create(
        &mut builder,
        &flat::PacketArgs {
            payload_type: ty,
            payload: Some(payload),
        },
    );
    builder.finish(packet, None);
    Frame::with_body(Opcode::Flatbuf, builder.finished_data().to_vec())
}

use fcast_flatbuf::flatbuffers;

/// `Error { kind, packet_num }`.
#[must_use]
pub fn error_frame(kind: flat::ErrorKind, packet_num: Option<u32>) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let payload =
        flat::Error::create(&mut b, &flat::ErrorArgs { kind, packet_num }).as_union_value();
    finish(b, flat::Message::Error, payload)
}

/// `VolumeChanged`.
#[must_use]
pub fn volume_changed_frame(volume: f32) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let payload =
        flat::VolumeChanged::create(&mut b, &flat::VolumeChangedArgs { volume }).as_union_value();
    finish(b, flat::Message::VolumeChanged, payload)
}

/// `SpeedChanged`.
#[must_use]
pub fn speed_changed_frame(speed: f32) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let payload =
        flat::SpeedChanged::create(&mut b, &flat::SpeedChangedArgs { speed }).as_union_value();
    finish(b, flat::Message::SpeedChanged, payload)
}

/// `PlaybackStateChanged`.
#[must_use]
pub fn playback_state_frame(state: flat::PlaybackState) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let payload =
        flat::PlaybackStateChanged::create(&mut b, &flat::PlaybackStateChangedArgs { state })
            .as_union_value();
    finish(b, flat::Message::PlaybackStateChanged, payload)
}

/// `ProgressChanged { position, duration }`, both in micros.
#[must_use]
pub fn progress_frame(position: Duration, duration: Option<Duration>) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let pos = time(position);
    let dur = duration.map(time);
    let payload = flat::ProgressChanged::create(
        &mut b,
        &flat::ProgressChangedArgs {
            position: Some(&pos),
            duration: dur.as_ref(),
        },
    )
    .as_union_value();
    finish(b, flat::Message::ProgressChanged, payload)
}

/// `StopPlayback`.
#[must_use]
pub fn stop_playback_frame() -> Frame {
    let mut b = FlatBufferBuilder::new();
    let payload = flat::StopPlayback::create(&mut b, &flat::StopPlaybackArgs {}).as_union_value();
    finish(b, flat::Message::StopPlayback, payload)
}

/// `CompanionHelloResponse { provider_id }`.
#[must_use]
pub fn companion_hello_response_frame(provider_id: u16) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let payload = flat::CompanionHelloResponse::create(
        &mut b,
        &flat::CompanionHelloResponseArgs { provider_id },
    )
    .as_union_value();
    finish(b, flat::Message::CompanionHelloResponse, payload)
}

/// `CompanionResourceInfoRequest { request_id, resource_id }` — "what is this, and how
/// big is it?", asked of the sender that owns the provider id (#249).
#[must_use]
pub fn companion_resource_info_request_frame(request_id: u32, resource_id: u32) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let payload = flat::CompanionResourceInfoRequest::create(
        &mut b,
        &flat::CompanionResourceInfoRequestArgs {
            request_id,
            resource_id,
        },
    )
    .as_union_value();
    finish(b, flat::Message::CompanionResourceInfoRequest, payload)
}

/// `CompanionResourceRequest { request_id, resource_id, read_head }` — a byte range,
/// answered with one or more `Resource` packets (#249).
///
/// `stop_inclusive` is inclusive, as the field name says and as HTTP's own `Range` is;
/// getting that off by one reads a byte short of every window.
#[must_use]
pub fn companion_resource_request_frame(
    request_id: u32,
    resource_id: u32,
    start: u64,
    stop_inclusive: u64,
) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let read_head = flat::ResourceReadHead::new(start, stop_inclusive);
    let payload = flat::CompanionResourceRequest::create(
        &mut b,
        &flat::CompanionResourceRequestArgs {
            request_id,
            resource_id,
            read_head: Some(&read_head),
        },
    )
    .as_union_value();
    finish(b, flat::Message::CompanionResourceRequest, payload)
}

/// `MirroringSessionDescription` — the answer SDP.
#[must_use]
pub fn mirroring_answer_frame(session_id: u16, sdp: &str) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let sdp = b.create_string(sdp);
    let payload = flat::MirroringSessionDescription::create(
        &mut b,
        &flat::MirroringSessionDescriptionArgs {
            session_id,
            sdp: Some(sdp),
        },
    )
    .as_union_value();
    finish(b, flat::Message::MirroringSessionDescription, payload)
}

/// `QueueItemSelected` (receiver-initiated advance, or a relay).
#[must_use]
pub fn queue_select_frame(position: QueuePosition) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let (position_type, position) = build_position(&mut b, position);
    let payload = flat::QueueItemSelected::create(
        &mut b,
        &flat::QueueItemSelectedArgs {
            position_type,
            position: Some(position),
        },
    )
    .as_union_value();
    finish(b, flat::Message::QueueItemSelected, payload)
}

/// `QueueRemove` relay.
#[must_use]
pub fn queue_remove_frame(position: QueuePosition) -> Frame {
    let mut b = FlatBufferBuilder::new();
    let (position_type, position) = build_position(&mut b, position);
    let payload = flat::QueueRemove::create(
        &mut b,
        &flat::QueueRemoveArgs {
            position_type,
            position: Some(position),
        },
    )
    .as_union_value();
    finish(b, flat::Message::QueueRemove, payload)
}

fn build_position(
    b: &mut FlatBufferBuilder<'_>,
    position: QueuePosition,
) -> (flat::QueuePosition, WIPOffset<flatbuffers::UnionWIPOffset>) {
    match position {
        QueuePosition::Index(index) => (
            flat::QueuePosition::Index,
            flat::QueueIndex::create(b, &flat::QueueIndexArgs { index }).as_union_value(),
        ),
        QueuePosition::Front => (
            flat::QueuePosition::Front,
            flat::QueueMarkerFront::create(b, &flat::QueueMarkerFrontArgs {}).as_union_value(),
        ),
        QueuePosition::Back => (
            flat::QueuePosition::Back,
            flat::QueueMarkerBack::create(b, &flat::QueueMarkerBackArgs {}).as_union_value(),
        ),
    }
}

fn time(d: Duration) -> flat::Time {
    // Truncating: a Duration whose micros exceed u64 is ~585k years of media.
    #[allow(clippy::cast_possible_truncation)]
    flat::Time::new(d.as_micros() as u64)
}

/// The honest capability tokens this receiver introduces itself with. Static,
/// like the codec table it reflects: the pipeline decodes through ffmpeg, and
/// these are the containers/codecs the media path actually plays. No subtitle
/// rendering, no image display, no HDR signalling, no mirroring until #248's
/// mirroring stage lands — absent capabilities are stated absent (D32) rather
/// than copied from the reference's gstreamer probe.
pub struct Capabilities {
    /// Whether WebRTC mirroring is offered.
    pub mirroring: bool,
}

/// `ReceiverIntroduction { device_info, capabilities }`.
#[must_use]
pub fn receiver_introduction_frame(
    display_name: &str,
    app_name: &str,
    app_version: &str,
    caps: &Capabilities,
) -> Frame {
    let mut b = FlatBufferBuilder::new();

    let display_name = b.create_string(display_name);
    let app_name_off = b.create_string(app_name);
    let app_version_off = b.create_string(app_version);
    let device_info = flat::DeviceInfo::create(
        &mut b,
        &flat::DeviceInfoArgs {
            display_name: Some(display_name),
            app_name: Some(app_name_off),
            app_version: Some(app_version_off),
        },
    );

    fn str_vector<'a>(
        b: &mut FlatBufferBuilder<'a>,
        items: &[&str],
    ) -> WIPOffset<flatbuffers::Vector<'a, flatbuffers::ForwardsUOffset<&'a str>>> {
        let offs: Vec<_> = items.iter().map(|s| b.create_string(s)).collect();
        b.create_vector(&offs)
    }
    let protocols = str_vector(&mut b, &["http", "https", "rtsp"]);
    let containers = str_vector(
        &mut b,
        &[
            "mp4",
            "quicktime",
            "mkv",
            "webm",
            "mpegts",
            "hls",
            "ogg",
            "wav",
        ],
    );
    let video_formats = str_vector(&mut b, &["h264", "h265", "vp8", "vp9", "av1"]);
    let audio_formats = str_vector(
        &mut b,
        &["aac", "mp3", "opus", "vorbis", "flac", "pcm", "ac3"],
    );
    let subtitle_formats = str_vector(&mut b, &[]);
    let hdr_formats = str_vector(&mut b, &[]);
    let image_formats = str_vector(&mut b, &[]);

    let media = flat::MediaCapabilities::create(
        &mut b,
        &flat::MediaCapabilitiesArgs {
            protocols: Some(protocols),
            containers: Some(containers),
            video_formats: Some(video_formats),
            audio_formats: Some(audio_formats),
            subtitle_formats: Some(subtitle_formats),
            hdr_formats: Some(hdr_formats),
            image_formats: Some(image_formats),
            external_subtitles: false,
            mirroring: caps.mirroring,
        },
    );
    let audio = flat::AudioCapabilities::create(
        &mut b,
        &flat::AudioCapabilitiesArgs {
            volume_step_interval: 0.01,
        },
    );
    let display = flat::DisplayCapabilities::create(
        &mut b,
        &flat::DisplayCapabilitiesArgs { resolution: None },
    );
    let capabilities = flat::ReceiverCapabilities::create(
        &mut b,
        &flat::ReceiverCapabilitiesArgs {
            media: Some(media),
            display: Some(display),
            audio: Some(audio),
        },
    );

    let payload = flat::ReceiverIntroduction::create(
        &mut b,
        &flat::ReceiverIntroductionArgs {
            device_info: Some(device_info),
            capabilities: Some(capabilities),
        },
    )
    .as_union_value();
    finish(b, flat::Message::ReceiverIntroduction, payload)
}

/// Re-serialize a captured `Load` or `QueueInsert` body with the reference's
/// relay stripping: `headers` and the typed metadata union dropped, everything
/// else — `extra_metadata` included — carried through by copying the raw tables.
///
/// Returns `None` when the raw body no longer parses (it did once, at inbound
/// time, so this is defensive) — a suppressed relay, exactly the reference's
/// behaviour.
#[must_use]
pub fn stripped_relay_frame(raw: &[u8]) -> Option<Frame> {
    let packet = fcast_flatbuf::root_as_packet(raw).ok()?;
    let mut b = FlatBufferBuilder::new();
    match packet.payload_type() {
        flat::Message::Load => {
            let load = packet.payload_as_load()?;
            let (source_type, source) = match load.source_type() {
                flat::MediaSource::Single => {
                    let item = strip_item(&mut b, load.source_as_single()?);
                    (flat::MediaSource::Single, item.as_union_value())
                }
                flat::MediaSource::Queue => {
                    let queue = load.source_as_queue()?;
                    let items: Vec<_> = queue
                        .items()
                        .iter()
                        .map(|qi| {
                            let media_item = strip_item(&mut b, qi.media_item());
                            let duration = qi.playback_duration().copied();
                            flat::QueueItem::create(
                                &mut b,
                                &flat::QueueItemArgs {
                                    media_item: Some(media_item),
                                    playback_duration: duration.as_ref(),
                                },
                            )
                        })
                        .collect();
                    let items = b.create_vector(&items);
                    let queue = flat::Queue::create(
                        &mut b,
                        &flat::QueueArgs {
                            items: Some(items),
                            start_index: queue.start_index(),
                            autoplay: queue.autoplay(),
                        },
                    );
                    (flat::MediaSource::Queue, queue.as_union_value())
                }
                _ => return None,
            };
            let payload = flat::Load::create(
                &mut b,
                &flat::LoadArgs {
                    source_type,
                    source: Some(source),
                },
            )
            .as_union_value();
            Some(finish(b, flat::Message::Load, payload))
        }
        flat::Message::QueueInsert => {
            let insert = packet.payload_as_queue_insert()?;
            let media_item = strip_item(&mut b, insert.item().media_item());
            let duration = insert.item().playback_duration().copied();
            let item = flat::QueueItem::create(
                &mut b,
                &flat::QueueItemArgs {
                    media_item: Some(media_item),
                    playback_duration: duration.as_ref(),
                },
            );
            let (position_type, position) = copy_position(&mut b, insert.position_type(), || {
                insert.position_as_index().map(|i| i.index())
            })?;
            let payload = flat::QueueInsert::create(
                &mut b,
                &flat::QueueInsertArgs {
                    item: Some(item),
                    position_type,
                    position: Some(position),
                },
            )
            .as_union_value();
            Some(finish(b, flat::Message::QueueInsert, payload))
        }
        _ => None,
    }
}

fn copy_position(
    b: &mut FlatBufferBuilder<'_>,
    ty: flat::QueuePosition,
    index: impl FnOnce() -> Option<u8>,
) -> Option<(flat::QueuePosition, WIPOffset<flatbuffers::UnionWIPOffset>)> {
    let position = match ty {
        flat::QueuePosition::Index => QueuePosition::Index(index()?),
        flat::QueuePosition::Front => QueuePosition::Front,
        flat::QueuePosition::Back => QueuePosition::Back,
        _ => return None,
    };
    Some(build_position(b, position))
}

/// One item with `headers: None` and the typed metadata union dropped;
/// `extra_metadata` copied recursively.
fn strip_item<'a>(
    b: &mut FlatBufferBuilder<'a>,
    item: flat::MediaItem<'_>,
) -> WIPOffset<flat::MediaItem<'a>> {
    let container = b.create_string(item.container());
    let source_url = b.create_string(item.source_url());
    let title = item.title().map(|s| b.create_string(s));
    let thumbnail_url = item.thumbnail_url().map(|s| b.create_string(s));
    let extra_metadata = item.extra_metadata().map(|kvs| {
        let copies: Vec<_> = kvs.iter().map(|kv| copy_kv(b, kv)).collect();
        b.create_vector(&copies)
    });
    let start_time = item.start_time().copied();
    flat::MediaItem::create(
        b,
        &flat::MediaItemArgs {
            container: Some(container),
            source_url: Some(source_url),
            start_time: start_time.as_ref(),
            volume: item.volume(),
            speed: item.speed(),
            headers: None,
            title,
            thumbnail_url,
            metadata_type: flat::Metadata::NONE,
            metadata: None,
            extra_metadata,
        },
    )
}

fn copy_kv<'a>(
    b: &mut FlatBufferBuilder<'a>,
    kv: flat::MetadataKV<'_>,
) -> WIPOffset<flat::MetadataKV<'a>> {
    let key = b.create_string(kv.key());
    let value = kv_value(&kv, kv.value_type()).map(|v| copy_resolved(b, v));
    let (value_type, value) = value.unzip();
    flat::MetadataKV::create(
        b,
        &flat::MetadataKVArgs {
            key: Some(key),
            value_type: value_type.unwrap_or(flat::GenericMetaValue::NONE),
            value,
        },
    )
}

/// A borrowed view of one `GenericMetaValue` union member.
enum MetaValue<'a> {
    Str(Option<&'a str>),
    Float(f64),
    Int(i64),
    List(
        Option<
            flatbuffers::Vector<
                'a,
                flatbuffers::ForwardsUOffset<flat::WrappedGenericMetaValue<'a>>,
            >,
        >,
    ),
    Kv(flat::MetadataKV<'a>),
}

fn kv_value<'a>(kv: &flat::MetadataKV<'a>, ty: flat::GenericMetaValue) -> Option<MetaValue<'a>> {
    match ty {
        flat::GenericMetaValue::String => kv.value_as_string().map(|s| MetaValue::Str(s.value())),
        flat::GenericMetaValue::Float => kv.value_as_float().map(|f| MetaValue::Float(f.value())),
        flat::GenericMetaValue::Int => kv.value_as_int().map(|i| MetaValue::Int(i.value())),
        flat::GenericMetaValue::List => kv.value_as_list().map(|l| MetaValue::List(l.value())),
        flat::GenericMetaValue::KVPair => kv.value_as_kvpair().map(MetaValue::Kv),
        _ => None,
    }
}

/// Copy one already-resolved union member. Non-generic on purpose: the metadata
/// union nests (lists of KV pairs of lists...), and a generic reader parameter
/// here made every recursion level a fresh monomorphization — the compiler's
/// recursion limit, hit at build time.
fn copy_resolved<'a>(
    b: &mut FlatBufferBuilder<'a>,
    value: MetaValue<'_>,
) -> (
    flat::GenericMetaValue,
    WIPOffset<flatbuffers::UnionWIPOffset>,
) {
    match value {
        MetaValue::Str(s) => {
            let s = s.map(|s| b.create_string(s));
            (
                flat::GenericMetaValue::String,
                flat::GenericMetaString::create(b, &flat::GenericMetaStringArgs { value: s })
                    .as_union_value(),
            )
        }
        MetaValue::Float(value) => (
            flat::GenericMetaValue::Float,
            flat::GenericMetaFloat::create(b, &flat::GenericMetaFloatArgs { value })
                .as_union_value(),
        ),
        MetaValue::Int(value) => (
            flat::GenericMetaValue::Int,
            flat::GenericMetaInt::create(b, &flat::GenericMetaIntArgs { value }).as_union_value(),
        ),
        MetaValue::List(items) => {
            let items = items.map(|items| {
                let copies: Vec<_> = items
                    .iter()
                    .map(|wrapped| {
                        let inner = wrapped_value(&wrapped, wrapped.value_type())
                            .map(|v| copy_resolved(b, v));
                        let (value_type, value) = inner.unzip();
                        flat::WrappedGenericMetaValue::create(
                            b,
                            &flat::WrappedGenericMetaValueArgs {
                                value_type: value_type.unwrap_or(flat::GenericMetaValue::NONE),
                                value,
                            },
                        )
                    })
                    .collect();
                b.create_vector(&copies)
            });
            (
                flat::GenericMetaValue::List,
                flat::GenericMetaList::create(b, &flat::GenericMetaListArgs { value: items })
                    .as_union_value(),
            )
        }
        MetaValue::Kv(kv) => (
            flat::GenericMetaValue::KVPair,
            copy_kv(b, kv).as_union_value(),
        ),
    }
}

fn wrapped_value<'a>(
    wrapped: &flat::WrappedGenericMetaValue<'a>,
    ty: flat::GenericMetaValue,
) -> Option<MetaValue<'a>> {
    match ty {
        flat::GenericMetaValue::String => {
            wrapped.value_as_string().map(|s| MetaValue::Str(s.value()))
        }
        flat::GenericMetaValue::Float => wrapped
            .value_as_float()
            .map(|f| MetaValue::Float(f.value())),
        flat::GenericMetaValue::Int => wrapped.value_as_int().map(|i| MetaValue::Int(i.value())),
        flat::GenericMetaValue::List => wrapped.value_as_list().map(|l| MetaValue::List(l.value())),
        flat::GenericMetaValue::KVPair => wrapped.value_as_kvpair().map(MetaValue::Kv),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The interval rounding the reference tests with its own vectors, plus the
    /// overflow its version panics on.
    #[test]
    fn progress_interval_rounds_to_100ms_steps() {
        for (micros, expect_ms) in [
            (0u64, 100u64),
            (49_000, 100),
            (149_000, 100),
            (150_000, 200),
            (249_000, 200),
            (250_000, 300),
            (549_000, 500),
            (550_000, 600),
        ] {
            assert_eq!(
                round_progress_interval(micros),
                Duration::from_millis(expect_ms),
                "{micros}"
            );
        }
        // The reference's `(micros + 50_000)` overflows here; ours must not.
        let _ = round_progress_interval(u64::MAX);
    }

    /// Round-trip: our builders' frames parse back through our own parser.
    #[test]
    fn outbound_frames_reparse() {
        let frame = volume_changed_frame(0.5);
        // VolumeChanged is sender-legal too, so it round-trips through parse.
        match parse_flatbuf(&frame.body).unwrap() {
            Parsed::Message(V4Inbound::VolumeChanged(v)) => assert!((v - 0.5).abs() < 1e-6),
            other => panic!("{other:?}"),
        }
        let frame = error_frame(flat::ErrorKind::InvalidOpcode, Some(7));
        let packet = fcast_flatbuf::root_as_packet(&frame.body).unwrap();
        let error = packet.payload_as_error().unwrap();
        assert_eq!(error.kind(), flat::ErrorKind::InvalidOpcode);
        assert_eq!(error.packet_num(), Some(7));
        // And packet_num really is nullable, not zero-defaulted.
        let frame = error_frame(flat::ErrorKind::Internal, None);
        let packet = fcast_flatbuf::root_as_packet(&frame.body).unwrap();
        assert_eq!(packet.payload_as_error().unwrap().packet_num(), None);
    }

    /// A garbage body is session-fatal; a well-formed packet with a
    /// receiver-direction payload is a polite typed reply. The reference's
    /// conformance driver holds receivers to exactly this split.
    #[test]
    fn malformed_is_fatal_but_wrong_direction_is_answered() {
        assert!(matches!(
            parse_flatbuf(&[0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4]),
            Err(FCastError::MalformedFlatbuf(_))
        ));
        let intro = receiver_introduction_frame("x", "y", "z", &Capabilities { mirroring: false });
        assert_eq!(
            parse_flatbuf(&intro.body).unwrap(),
            Parsed::Reply(flat::ErrorKind::InvalidPayloadType)
        );
    }

    /// The introduction carries the honest capability surface: mirroring off
    /// until it exists, no subtitle/image/HDR claims, and the 0.01 volume step.
    #[test]
    fn the_introduction_is_honest() {
        let frame = receiver_introduction_frame(
            "dma.space/screen",
            "castaway",
            "0.1.0",
            &Capabilities { mirroring: false },
        );
        let packet = fcast_flatbuf::root_as_packet(&frame.body).unwrap();
        let intro = packet.payload_as_receiver_introduction().unwrap();
        assert_eq!(intro.device_info().app_name(), Some("castaway"));
        let caps = intro.capabilities().unwrap();
        let media = caps.media().unwrap();
        assert!(!media.mirroring());
        assert!(!media.external_subtitles());
        assert_eq!(media.subtitle_formats().unwrap().len(), 0);
        assert_eq!(media.image_formats().unwrap().len(), 0);
        let protocols: Vec<_> = media.protocols().unwrap().iter().collect();
        assert!(protocols.contains(&"http") && protocols.contains(&"https"));
        assert!((caps.audio().unwrap().volume_step_interval() - 0.01).abs() < 1e-6);
    }

    /// The relay strip: headers vanish, title/extra_metadata survive — the rule
    /// that keeps one sender's bearer token out of every other phone on the LAN.
    #[test]
    fn the_relay_strips_headers_and_keeps_metadata() {
        // Build a Load(Single) with headers + title + extra_metadata.
        let mut b = FlatBufferBuilder::new();
        let key = b.create_string("Authorization");
        let value = b.create_string("Bearer sekrit");
        let header = flat::RequestHeader::create(
            &mut b,
            &flat::RequestHeaderArgs {
                key: Some(key),
                value: Some(value),
            },
        );
        let headers = b.create_vector(&[header]);
        let container = b.create_string("video/mp4");
        let url = b.create_string("http://h/v.mp4");
        let title = b.create_string("A Film");
        let meta_key = b.create_string("origin");
        let meta_str = b.create_string("grayjay");
        let meta_val = flat::GenericMetaString::create(
            &mut b,
            &flat::GenericMetaStringArgs {
                value: Some(meta_str),
            },
        );
        let kv = flat::MetadataKV::create(
            &mut b,
            &flat::MetadataKVArgs {
                key: Some(meta_key),
                value_type: flat::GenericMetaValue::String,
                value: Some(meta_val.as_union_value()),
            },
        );
        let extra = b.create_vector(&[kv]);
        let item = flat::MediaItem::create(
            &mut b,
            &flat::MediaItemArgs {
                container: Some(container),
                source_url: Some(url),
                start_time: None,
                volume: None,
                speed: None,
                headers: Some(headers),
                title: Some(title),
                thumbnail_url: None,
                metadata_type: flat::Metadata::NONE,
                metadata: None,
                extra_metadata: Some(extra),
            },
        );
        let load = flat::Load::create(
            &mut b,
            &flat::LoadArgs {
                source_type: flat::MediaSource::Single,
                source: Some(item.as_union_value()),
            },
        );
        let packet = flat::Packet::create(
            &mut b,
            &flat::PacketArgs {
                payload_type: flat::Message::Load,
                payload: Some(load.as_union_value()),
            },
        );
        b.finish(packet, None);
        let raw = b.finished_data().to_vec();

        // Inbound parse keeps the headers for our own fetch...
        let Parsed::Message(V4Inbound::Load { source, raw }) = parse_flatbuf(&raw).unwrap() else {
            panic!("expected a Load");
        };
        let LoadSource::Single(item) = source else {
            panic!("expected Single");
        };
        assert_eq!(item.headers[0].0, "Authorization");

        // ...and the relay drops them while keeping the rest.
        let relayed = stripped_relay_frame(&raw).unwrap();
        let packet = fcast_flatbuf::root_as_packet(&relayed.body).unwrap();
        let single = packet
            .payload_as_load()
            .unwrap()
            .source_as_single()
            .unwrap();
        assert!(single.headers().is_none(), "headers must not be relayed");
        assert_eq!(single.title(), Some("A Film"));
        let extra = single.extra_metadata().unwrap();
        assert_eq!(extra.get(0).key(), "origin");
        assert_eq!(
            extra.get(0).value_as_string().unwrap().value(),
            Some("grayjay")
        );
    }
}
