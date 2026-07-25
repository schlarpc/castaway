# Cast RTP packet fixtures

These `.bin` files are Cast Streaming RTP packets copied verbatim from the Chromium
[openscreen](https://chromium.googlesource.com/openscreen) project, where they live as
the seed corpus for `cast/streaming/impl/rtp_packet_parser_fuzzer.cc`
(`cast/streaming/rtp_packet_parser_fuzzer_seeds/`).

They are the golden fixtures for `proto_cast::rtp` (ground rule 6): openscreen is the
only authoritative description of Cast's RTP framing, so testing our parser against
bytes *their* parser accepts is what makes the reimplementation trustworthy. Ground
rule 9 keeps openscreen out of the build — we take the findings, not the dependency.

All seeds share SSRC `0x01020304`. What each one exercises:

| File | Exercises |
| --- | --- |
| `rtp_packet_for_key_frame.bin` | Key frame, no reference frame id, no extensions |
| `rtp_packet_for_key_frame_with_bad_packet_id.bin` | `packet_id > max_packet_id` — must be rejected |
| `rtp_packet_for_key_frame_with_latency_ext.bin` | The adaptive-latency extension |
| `rtp_packet_for_key_frame_with_multiple_ext.bin` | An unknown extension skipped by length, then adaptive-latency |
| `rtp_packet_for_non_key_frame_without_rfid.bin` | Implicit reference to the previous frame |
| `rtp_packet_for_non_key_frame_with_rfid.bin` | Explicit reference frame id |
| `rtp_packet_trunc_to_*.bin` | Truncation at each boundary — must error, never panic |

## License

openscreen is BSD-3-Clause. Its notice, retained per the license:

```
// Copyright 2018 The Chromium Authors
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//    * Redistributions of source code must retain the above copyright
// notice, this list of conditions and the following disclaimer.
//    * Redistributions in binary form must reproduce the above
// copyright notice, this list of conditions and the following disclaimer
// in the documentation and/or other materials provided with the
// distribution.
//    * Neither the name of Google LLC nor the names of its
// contributors may be used to endorse or promote products derived from
// this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
// OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
// THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```
