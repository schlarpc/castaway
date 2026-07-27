//! A scripted phone drives our sink through a full A2DP session.
//!
//! This is the tier-1 harness for the profile layer (ground rule 6): the real state
//! machine, the real wire encoding, and no radio. Every assertion is something that
//! would otherwise need a handset in the room to exercise.

#![allow(clippy::unwrap_used)]

use bytes::Bytes;
use castaway_core::AudioCodec;
use proto_bluetooth_audio::avdtp::{error_code, Message, MessageType, Seid, Signal};
/// The table a build with no LDAC decoder offers — the common case, and the one that
/// proves a sender falls back cleanly instead of picking an endpoint we cannot decode.
const NO_LDAC: &[castaway_core::AudioCodec] = &[
    castaway_core::AudioCodec::Sbc,
    castaway_core::AudioCodec::Aac,
    castaway_core::AudioCodec::AptX,
    castaway_core::AudioCodec::AptXHd,
];

use proto_bluetooth_audio::codec::{advertised, ChannelModes, CodecCapability, SampleRates};
use proto_bluetooth_audio::media::Depacketizer;
use proto_bluetooth_audio::sink::{reject_code, SinkEvent, SinkSession, StreamState};

/// Stands in for the phone: sends commands, reads replies.
struct Phone {
    session: SinkSession,
    transaction: u8,
}

impl Phone {
    fn new(caps: Vec<CodecCapability>) -> Self {
        Self {
            session: SinkSession::new(caps),
            transaction: 0,
        }
    }

    /// Send a command and return everything the sink produced.
    fn send(&mut self, signal: Signal, payload: &[u8]) -> Vec<SinkEvent> {
        self.transaction = (self.transaction + 1) & 0x0F;
        let cmd = Message::command(self.transaction, signal, Bytes::copy_from_slice(payload));
        // Round-trip through the wire encoding so the tests exercise the codec too,
        // not just the state machine's in-memory types.
        let encoded = cmd.encode();
        let decoded = Message::decode(&encoded).unwrap();
        assert_eq!(decoded, cmd, "message must survive its own encoding");
        self.session.handle(&decoded)
    }

    /// The reply message, which every command must produce exactly one of.
    fn reply(events: &[SinkEvent]) -> &Message {
        let mut replies = events.iter().filter_map(|e| match e {
            SinkEvent::Reply(m) => Some(m),
            _ => None,
        });
        let first = replies.next().expect("every command needs a reply");
        assert!(replies.next().is_none(), "exactly one reply per command");
        first
    }

    fn accepted(events: &[SinkEvent]) -> &Message {
        let reply = Self::reply(events);
        assert_eq!(
            reply.message_type,
            MessageType::ResponseAccept,
            "expected accept, got {:?} code {:?}",
            reply.message_type,
            reject_code(reply)
        );
        reply
    }

    fn rejected(events: &[SinkEvent]) -> u8 {
        let reply = Self::reply(events);
        assert_eq!(reply.message_type, MessageType::ResponseReject);
        reject_code(reply).expect("a reject must carry a code")
    }
}

/// Configure `codec` on whichever endpoint advertises it, returning the SEID used.
fn configure(phone: &mut Phone, chosen: &CodecCapability) -> Seid {
    let discover = phone.send(Signal::Discover, &[]);
    let seps = Phone::accepted(&discover).payload.clone();

    // Find the endpoint whose capabilities match the codec we want.
    let seid = seps
        .chunks(2)
        .filter_map(|c| Seid::from_shifted(c[0]).ok())
        .find(|seid| {
            let caps = phone.send(Signal::GetAllCapabilities, &[seid.shifted()]);
            let payload = Phone::accepted(&caps).payload.clone();
            proto_bluetooth_audio::avdtp::find_codec_capability(&payload)
                .map(|c| c.audio_codec() == chosen.audio_codec())
                .unwrap_or(false)
        })
        .expect("an endpoint for the chosen codec");

    let mut set = vec![seid.shifted(), 0x04];
    set.push(0x01); // media transport category
    set.push(0x00);
    let codec = chosen.encode();
    set.push(0x07); // media codec category
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);

    let events = phone.send(Signal::SetConfiguration, &set);
    Phone::accepted(&events);
    seid
}

fn aptx_config() -> CodecCapability {
    CodecCapability::AptX {
        rates: SampleRates::HZ_44100,
        channels: ChannelModes::JOINT_STEREO,
    }
}

#[test]
fn a_phone_walks_the_whole_session_from_discovery_to_streaming() {
    let mut phone = Phone::new(advertised(proto_bluetooth_audio::codec::ALL));
    assert_eq!(phone.session.state(), StreamState::Idle);

    let seid = configure(&mut phone, &aptx_config());
    assert_eq!(phone.session.state(), StreamState::Configured);

    Phone::accepted(&phone.send(Signal::Open, &[seid.shifted()]));
    assert_eq!(phone.session.state(), StreamState::Open);

    let started = phone.send(Signal::Start, &[seid.shifted()]);
    Phone::accepted(&started);
    assert_eq!(phone.session.state(), StreamState::Streaming);
    assert!(started.contains(&SinkEvent::Started));

    Phone::accepted(&phone.send(Signal::Suspend, &[seid.shifted()]));
    assert_eq!(phone.session.state(), StreamState::Open);

    let closed = phone.send(Signal::Close, &[seid.shifted()]);
    Phone::accepted(&closed);
    assert_eq!(phone.session.state(), StreamState::Idle);
    assert!(closed.contains(&SinkEvent::Closed));
}

#[test]
fn configuration_reports_the_codec_and_rate_the_decoder_needs() {
    let mut phone = Phone::new(advertised(proto_bluetooth_audio::codec::ALL));
    let events = {
        configure(&mut phone, &aptx_config());
        phone.send(Signal::GetConfiguration, &[])
    };
    Phone::accepted(&events);

    // The Configured event is what the adapter turns into a Depacketizer + decoder.
    let mut phone = Phone::new(advertised(proto_bluetooth_audio::codec::ALL));
    let discover = phone.send(Signal::Discover, &[]);
    let seps = Phone::accepted(&discover).payload.clone();
    let seid = Seid::from_shifted(seps[0]).unwrap();
    let chosen = CodecCapability::Ldac {
        rate_bits: 1 << 4, // 48 kHz
        channel_bits: 1,
    };
    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);

    let events = phone.send(Signal::SetConfiguration, &set);
    Phone::accepted(&events);
    let configured = events
        .iter()
        .find_map(|e| match e {
            SinkEvent::Configured { codec, format, .. } => Some((*codec, *format)),
            _ => None,
        })
        .expect("configuration must report what to decode with");
    // Both halves matter: Q25 was the rate reaching the log and not the decoder, and a
    // channel count that came from a default rather than the negotiation is the same bug.
    assert_eq!(configured.0, AudioCodec::Ldac);
    assert_eq!(configured.1.sample_rate(), 48_000);
    assert_eq!(configured.1.channels(), 2);
}

#[test]
fn every_advertised_codec_can_actually_be_configured() {
    // Guards against an endpoint that is advertised but not acceptable — which presents
    // to a user as "it works from my phone but not my friend's" and nothing else.
    for cap in advertised(proto_bluetooth_audio::codec::ALL) {
        let configuration = match &cap {
            CodecCapability::Sbc {
                min_bitpool,
                max_bitpool,
                ..
            } => CodecCapability::Sbc {
                rates: SampleRates::HZ_44100,
                channels: ChannelModes::JOINT_STEREO,
                block_lengths: 0b0001,
                subbands: 0b01,
                allocations: 0b01,
                min_bitpool: *min_bitpool,
                max_bitpool: *max_bitpool,
            },
            CodecCapability::Aac { bitrate, .. } => CodecCapability::Aac {
                object_types: 1 << 6,
                rate_bits: 1 << 3,
                channel_bits: 0b10,
                vbr: true,
                bitrate: *bitrate,
            },
            CodecCapability::AptX { .. } => aptx_config(),
            CodecCapability::AptXHd { .. } => CodecCapability::AptXHd {
                rates: SampleRates::HZ_48000,
                channels: ChannelModes::JOINT_STEREO,
            },
            CodecCapability::Ldac { .. } => CodecCapability::Ldac {
                rate_bits: 1 << 5,
                channel_bits: 1,
            },
            // CodecCapability is #[non_exhaustive]; a codec added to the advertised
            // table without a configuration here should fail loudly rather than be
            // skipped, since the point of this test is that *every* endpoint works.
            other => panic!("no test configuration for {}", other.name()),
        };
        let mut phone = Phone::new(advertised(proto_bluetooth_audio::codec::ALL));
        let seid = configure(&mut phone, &configuration);
        assert_eq!(
            phone.session.state(),
            StreamState::Configured,
            "{} should be configurable",
            cap.name()
        );
        Phone::accepted(&phone.send(Signal::Open, &[seid.shifted()]));
        Phone::accepted(&phone.send(Signal::Start, &[seid.shifted()]));
        assert_eq!(phone.session.state(), StreamState::Streaming);
    }
}

#[test]
fn starting_before_opening_is_rejected_with_bad_state() {
    // A sender that skips OPEN must be told so. Accepting it would leave us streaming
    // over a media channel that was never established.
    let mut phone = Phone::new(advertised(NO_LDAC));
    let seid = configure(&mut phone, &aptx_config());
    let code = Phone::rejected(&phone.send(Signal::Start, &[seid.shifted()]));
    assert_eq!(code, error_code::BAD_STATE);
    assert_eq!(phone.session.state(), StreamState::Configured);
}

#[test]
fn a_configuration_that_still_names_a_set_is_rejected() {
    // Several rates left selected means the decoder cannot know the stream's rate, and
    // guessing plays it at the wrong pitch rather than failing. Catch it at negotiation.
    let mut phone = Phone::new(advertised(NO_LDAC));
    let ambiguous = CodecCapability::AptX {
        rates: SampleRates::COMMON, // two rates — an offer, not a configuration
        channels: ChannelModes::JOINT_STEREO,
    };
    let discover = phone.send(Signal::Discover, &[]);
    let seps = Phone::accepted(&discover).payload.clone();
    let seid = seps
        .chunks(2)
        .filter_map(|c| Seid::from_shifted(c[0]).ok())
        .find(|seid| {
            let caps = phone.send(Signal::GetAllCapabilities, &[seid.shifted()]);
            let payload = Phone::accepted(&caps).payload.clone();
            proto_bluetooth_audio::avdtp::find_codec_capability(&payload)
                .map(|c| c.audio_codec() == AudioCodec::AptX)
                .unwrap_or(false)
        })
        .unwrap();

    let codec = ambiguous.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    let code = Phone::rejected(&phone.send(Signal::SetConfiguration, &set));
    assert_eq!(code, error_code::INVALID_CODEC_PARAMETER);
    assert_eq!(phone.session.state(), StreamState::Idle);
}

#[test]
fn configuring_one_codec_onto_another_codecs_endpoint_is_refused() {
    // Otherwise we would accept the configuration and then hand the stream to the wrong
    // decoder, which produces noise rather than an error.
    let mut phone = Phone::new(advertised(proto_bluetooth_audio::codec::ALL));
    let discover = phone.send(Signal::Discover, &[]);
    let seps = Phone::accepted(&discover).payload.clone();

    // Endpoint 1 is LDAC (first in preference order); configure SBC onto it.
    let ldac_seid = Seid::from_shifted(seps[0]).unwrap();
    let sbc = CodecCapability::Sbc {
        rates: SampleRates::HZ_44100,
        channels: ChannelModes::JOINT_STEREO,
        block_lengths: 0b0001,
        subbands: 0b01,
        allocations: 0b01,
        min_bitpool: 2,
        max_bitpool: 53,
    };
    let codec = sbc.encode();
    let mut set = vec![ldac_seid.shifted(), 0x04, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    let code = Phone::rejected(&phone.send(Signal::SetConfiguration, &set));
    assert_eq!(code, error_code::UNSUPPORTED_CONFIGURATION);
}

#[test]
fn an_unknown_seid_is_rejected_rather_than_defaulting_to_the_first_endpoint() {
    let mut phone = Phone::new(advertised(NO_LDAC));
    let code = Phone::rejected(&phone.send(Signal::GetAllCapabilities, &[0x3E << 2]));
    assert_eq!(code, error_code::BAD_ACP_SEID);
}

#[test]
fn abort_is_never_rejected_even_from_idle() {
    // ABORT exists for the case where the two ends disagree about state. Refusing it
    // would strand that disagreement permanently.
    let mut phone = Phone::new(advertised(NO_LDAC));
    Phone::accepted(&phone.send(Signal::Abort, &[]));
    assert_eq!(phone.session.state(), StreamState::Idle);

    let seid = configure(&mut phone, &aptx_config());
    Phone::accepted(&phone.send(Signal::Open, &[seid.shifted()]));
    let events = phone.send(Signal::Abort, &[seid.shifted()]);
    Phone::accepted(&events);
    assert!(events.contains(&SinkEvent::Closed));
    assert_eq!(phone.session.state(), StreamState::Idle);
}

#[test]
fn a_dropped_link_closes_a_live_stream() {
    // The phone walks out of the room mid-song: no CLOSE, just a dead link.
    let mut phone = Phone::new(advertised(NO_LDAC));
    let seid = configure(&mut phone, &aptx_config());
    Phone::accepted(&phone.send(Signal::Open, &[seid.shifted()]));
    Phone::accepted(&phone.send(Signal::Start, &[seid.shifted()]));

    let events = phone.session.link_down().unwrap();
    assert_eq!(events, vec![SinkEvent::Closed]);
    assert_eq!(phone.session.state(), StreamState::Idle);
    // …and the endpoint is free for the next phone.
    assert!(phone.session.endpoints().iter().all(|s| !s.in_use));
}

#[test]
fn a_closed_stream_frees_its_endpoint_for_the_next_sender() {
    let mut phone = Phone::new(advertised(NO_LDAC));
    let seid = configure(&mut phone, &aptx_config());
    assert!(phone
        .session
        .endpoints()
        .iter()
        .any(|s| s.seid == seid && s.in_use));

    Phone::accepted(&phone.send(Signal::Close, &[seid.shifted()]));
    assert!(phone.session.endpoints().iter().all(|s| !s.in_use));

    // Last-writer-wins (Q23): the next phone configures the same endpoint immediately.
    let again = configure(&mut phone, &aptx_config());
    assert_eq!(again, seid);
}

#[test]
fn the_negotiated_configuration_drives_the_depacketizer() {
    // The join between negotiation and media: aptX gets raw framing, everything else
    // RTP. Deriving it from the configuration rather than guessing per packet is what
    // stops 12 bytes of audio being eaten as a phantom header.
    let mut phone = Phone::new(advertised(NO_LDAC));
    configure(&mut phone, &aptx_config());
    let config = phone.session.configuration().unwrap();
    let depacketizer = Depacketizer::new(
        config.audio_codec(),
        config.sample_rate().expect("a configuration has one rate"),
    );
    assert_eq!(depacketizer.codec(), AudioCodec::AptX);
    assert!(
        !depacketizer.expects_rtp(),
        "classic aptX carries no RTP header"
    );
}

/// The RECONFIGURE payload shape: acceptor SEID, then the media codec capability.
fn reconfigure_payload(seid: Seid, capability: &CodecCapability) -> Vec<u8> {
    let codec = capability.encode();
    let mut out = vec![seid.shifted(), 0x07];
    out.push(u8::try_from(codec.len()).unwrap());
    out.extend_from_slice(&codec);
    out
}

#[test]
fn reconfigure_changes_the_negotiated_format_instead_of_being_waved_through() {
    // The bug: RECONFIGURE was lumped in with SecurityControl and DelayReport and
    // answered with a bare ACCEPT, on the reasoning that a sink has no reconfigurable
    // parameters. The codec block is exactly what it carries. AOSP sends one when the
    // rate changes from Developer Options — the sender switched its encoder, we kept
    // decoding at the old rate, and the room got the wrong pitch with nothing logged.
    let mut phone = Phone::new(advertised(NO_LDAC));
    let seid = configure(&mut phone, &aptx_config());
    Phone::accepted(&phone.send(Signal::Open, &[seid.shifted()]));

    let at_48k = CodecCapability::AptX {
        rates: SampleRates::HZ_48000,
        channels: ChannelModes::JOINT_STEREO,
    };
    let events = phone.send(Signal::Reconfigure, &reconfigure_payload(seid, &at_48k));
    Phone::accepted(&events);

    let configured = events
        .iter()
        .find_map(|e| match e {
            SinkEvent::Configured { format, .. } => Some(*format),
            _ => None,
        })
        .expect("a reconfiguration must tell the caller to rebuild its decoder");
    assert_eq!(
        configured.sample_rate(),
        48_000,
        "the decoder must follow the sender's new rate"
    );

    // …and the sink reports the new configuration, not the one it was first given.
    let got = phone.send(Signal::GetConfiguration, &[seid.shifted()]);
    let payload = Phone::accepted(&got).payload.clone();
    let echoed = proto_bluetooth_audio::avdtp::find_codec_capability(&payload).unwrap();
    assert_eq!(echoed.format().unwrap().sample_rate(), 48_000);
}

#[test]
fn reconfigure_cannot_switch_codec_or_arrive_at_the_wrong_time() {
    let mut phone = Phone::new(advertised(NO_LDAC));
    let seid = configure(&mut phone, &aptx_config());

    // Only legal in OPEN. In CONFIGURED there is no stream to reconfigure yet, and in
    // STREAMING accepting one would swap the decoder out from under audio still arriving.
    let at_48k = CodecCapability::AptX {
        rates: SampleRates::HZ_48000,
        channels: ChannelModes::JOINT_STEREO,
    };
    let too_early = phone.send(Signal::Reconfigure, &reconfigure_payload(seid, &at_48k));
    assert_eq!(Phone::rejected(&too_early), error_code::BAD_STATE);

    Phone::accepted(&phone.send(Signal::Open, &[seid.shifted()]));
    Phone::accepted(&phone.send(Signal::Start, &[seid.shifted()]));
    let while_streaming = phone.send(Signal::Reconfigure, &reconfigure_payload(seid, &at_48k));
    assert_eq!(Phone::rejected(&while_streaming), error_code::BAD_STATE);

    Phone::accepted(&phone.send(Signal::Suspend, &[seid.shifted()]));

    // The codec itself may not change — that needs a CLOSE and a fresh SET_CONFIGURATION.
    // Accepting it would leave the endpoint describing one thing and the decoder another.
    let different_codec = CodecCapability::Sbc {
        rates: SampleRates::HZ_44100,
        channels: ChannelModes::JOINT_STEREO,
        block_lengths: 0b1000,
        subbands: 0b01,
        allocations: 0b01,
        min_bitpool: 2,
        max_bitpool: 53,
    };
    let swapped = phone.send(
        Signal::Reconfigure,
        &reconfigure_payload(seid, &different_codec),
    );
    assert_eq!(
        Phone::rejected(&swapped),
        error_code::UNSUPPORTED_CONFIGURATION
    );

    // A capability that still names a *set* is ambiguous — the same rule
    // SET_CONFIGURATION enforces, and for the same reason: guessing a rate plays the
    // stream at the wrong pitch rather than failing.
    let ambiguous = CodecCapability::AptX {
        rates: SampleRates::COMMON,
        channels: ChannelModes::JOINT_STEREO,
    };
    let vague = phone.send(Signal::Reconfigure, &reconfigure_payload(seid, &ambiguous));
    assert_eq!(Phone::rejected(&vague), error_code::INVALID_CODEC_PARAMETER);

    // After all that, the stream is still usable at its original configuration.
    assert_eq!(phone.session.state(), StreamState::Open);
}
