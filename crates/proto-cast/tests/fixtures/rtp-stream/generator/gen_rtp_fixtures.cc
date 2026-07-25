// Generates golden Cast RTP fixtures using openscreen's own packetizer and frame
// crypto, so castaway's receiver can be differential-tested against the reference
// implementation rather than only against itself.
//
// Writes two files:
//   packets.bin  — u16be length-prefixed RTP datagrams, in transmission order
//   frames.bin   — the plaintext frames those datagrams should reassemble into
//
// See crates/proto-cast/tests/fixtures/rtp-stream/README.md for the formats.

#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#include "cast/streaming/impl/frame_crypto.h"
#include "cast/streaming/impl/rtp_packetizer.h"
#include "cast/streaming/public/encoded_frame.h"
#include "cast/streaming/public/frame_id.h"
#include "cast/streaming/rtp_time.h"

using openscreen::Clock;
using openscreen::cast::EncodedFrame;
using openscreen::cast::EncryptedFrame;
using openscreen::cast::FrameCrypto;
using openscreen::cast::FrameId;
using openscreen::cast::RtpPacketizer;
using openscreen::cast::RtpPayloadType;
using openscreen::cast::RtpTimeTicks;

namespace {

constexpr uint32_t kSenderSsrc = 0x01020304;
constexpr int kMaxPacketSize = 1472;

// Fixed so the fixtures are byte-reproducible. A real session randomizes both.
constexpr std::array<uint8_t, 16> kAesKey = {0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
                                            0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
                                            0xcc, 0xdd, 0xee, 0xff};
constexpr std::array<uint8_t, 16> kIvMask = {0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a,
                                             0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4,
                                             0xc3, 0xd2, 0xe1, 0xf0};

void PutU16(std::vector<uint8_t>& out, uint16_t value) {
  out.push_back(static_cast<uint8_t>(value >> 8));
  out.push_back(static_cast<uint8_t>(value & 0xff));
}

void PutU32(std::vector<uint8_t>& out, uint32_t value) {
  out.push_back(static_cast<uint8_t>(value >> 24));
  out.push_back(static_cast<uint8_t>(value >> 16));
  out.push_back(static_cast<uint8_t>(value >> 8));
  out.push_back(static_cast<uint8_t>(value & 0xff));
}

void WriteFile(const std::string& path, const std::vector<uint8_t>& bytes) {
  FILE* f = fopen(path.c_str(), "wb");
  if (!f) {
    fprintf(stderr, "cannot open %s\n", path.c_str());
    exit(1);
  }
  fwrite(bytes.data(), 1, bytes.size(), f);
  fclose(f);
}

// A deterministic, incompressible-ish payload so a mis-ordered reassembly cannot
// accidentally compare equal.
std::vector<uint8_t> MakePayload(int frame_index, size_t length) {
  std::vector<uint8_t> payload(length);
  uint32_t state = 0x9e3779b9u + static_cast<uint32_t>(frame_index) * 2654435761u;
  for (size_t i = 0; i < length; ++i) {
    state = state * 1103515245u + 12345u;
    payload[i] = static_cast<uint8_t>(state >> 24);
  }
  return payload;
}

struct FrameSpec {
  EncodedFrame::Dependency dependency;
  int64_t frame_id;
  int64_t referenced_frame_id;
  uint32_t rtp_timestamp;
  size_t payload_size;
  int new_playout_delay_ms;
};

}  // namespace

int main() {
  const FrameSpec specs[] = {
      // A key frame small enough for a single packet.
      {EncodedFrame::Dependency::kKeyFrame, 0, 0, 0, 100, 0},
      // A dependent frame spanning several packets.
      {EncodedFrame::Dependency::kDependent, 1, 0, 3000, 5000, 0},
      // A frame that also asks for a new playout delay.
      {EncodedFrame::Dependency::kDependent, 2, 1, 6000, 200, 800},
      // An independent (non-key) frame — the kind skip-ahead may land on.
      {EncodedFrame::Dependency::kIndependent, 3, 3, 9000, 1400, 0},
      // Exactly at a packet boundary, to catch off-by-one splitting.
      {EncodedFrame::Dependency::kDependent, 4, 3, 12000, 1440 * 2, 0},
      // Large enough to need many packets.
      {EncodedFrame::Dependency::kDependent, 5, 4, 15000, 20000, 0},
  };

  FrameCrypto crypto(kAesKey, kIvMask);
  RtpPacketizer packetizer(RtpPayloadType::kVideoVp8, kSenderSsrc,
                           kMaxPacketSize);

  std::vector<uint8_t> packets_out;
  std::vector<uint8_t> frames_out;
  int frame_index = 0;
  uint16_t sequence_number = 0;

  for (const FrameSpec& spec : specs) {
    std::vector<uint8_t> payload = MakePayload(frame_index, spec.payload_size);

    EncodedFrame encoded;
    encoded.dependency = spec.dependency;
    encoded.frame_id = FrameId::first() + spec.frame_id;
    encoded.referenced_frame_id = FrameId::first() + spec.referenced_frame_id;
    encoded.rtp_timestamp =
        RtpTimeTicks() + openscreen::cast::RtpTimeDelta::FromTicks(
                             spec.rtp_timestamp);
    encoded.reference_time = Clock::time_point{};
    encoded.new_playout_delay = std::chrono::milliseconds(spec.new_playout_delay_ms);
    encoded.data = openscreen::ByteView(payload.data(), payload.size());

    EncryptedFrame encrypted = crypto.Encrypt(encoded);

    // Record what the receiver must end up with: the plaintext frame.
    frames_out.push_back(static_cast<uint8_t>(spec.dependency));
    PutU32(frames_out, static_cast<uint32_t>(spec.frame_id));
    PutU32(frames_out, static_cast<uint32_t>(spec.referenced_frame_id));
    PutU32(frames_out, spec.rtp_timestamp);
    PutU16(frames_out, static_cast<uint16_t>(spec.new_playout_delay_ms));
    PutU32(frames_out, static_cast<uint32_t>(payload.size()));
    frames_out.insert(frames_out.end(), payload.begin(), payload.end());

    const int num_packets = packetizer.ComputeNumberOfPackets(encrypted);
    if (num_packets <= 0) {
      fprintf(stderr, "frame %d could not be packetized\n", frame_index);
      return 1;
    }
    for (int p = 0; p < num_packets; ++p) {
      std::vector<uint8_t> buffer(kMaxPacketSize);
      openscreen::ByteBuffer span =
          packetizer.GeneratePacket(encrypted, static_cast<uint16_t>(p),
                                    openscreen::ByteBuffer(buffer.data(),
                                                           buffer.size()));
      PutU16(packets_out, static_cast<uint16_t>(span.size()));
      const size_t packet_start = packets_out.size();
      packets_out.insert(packets_out.end(), span.data(),
                         span.data() + span.size());

      // Normalize the RTP sequence number (bytes 2-3). RFC 3550 recommends a random
      // starting value to make stream hijacking harder, and openscreen's packetizer
      // obliges — which would make these fixtures differ on every run. Cast reassembles
      // by frame id and packet id, never by sequence number, so replacing it with a
      // counter costs the receiver test nothing and buys byte-reproducibility.
      packets_out[packet_start + 2] = static_cast<uint8_t>(sequence_number >> 8);
      packets_out[packet_start + 3] = static_cast<uint8_t>(sequence_number & 0xff);
      ++sequence_number;
    }
    ++frame_index;
  }

  WriteFile("packets.bin", packets_out);
  WriteFile("frames.bin", frames_out);
  fprintf(stderr, "wrote %zu packet bytes, %zu frame bytes\n",
          packets_out.size(), frames_out.size());
  return 0;
}
