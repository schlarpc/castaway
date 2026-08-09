# castaway

castaway is a universal cast receiver: one Rust program that receives many casting
protocols on one display. A phone, laptop, or tablet on the same network can send
media to the display, and no app install is necessary.

The program drives a large commercial touch panel in kiosk mode. Development occurs
natively on Linux. The deploy target is a Windows box.

## How it works

castaway advertises its services on the local network. It uses one mDNS responder and
one SSDP responder for all protocols. Each protocol has its own adapter with its own
state machine. Every adapter sends events to one shared session manager. One media
pipeline decodes the media and shows it on the display through the GPU. One audio
mixer owns the audio device, and every source writes into that mixer.

## Protocols

| Protocol | What you can do |
|---|---|
| **AirPlay** | Play audio from an Apple device. Mirror the screen of an iPhone or iPad. |
| **Google Cast** | Cast media from Chrome or Android. Mirror a Chrome tab or a desktop. The receiver can also host a Cast app's own page in the embedded browser. |
| **DLNA / UPnP** | Send media from any DLNA control point. The receiver is a standard MediaRenderer. |
| **YouTube (DIAL + Lounge)** | Press the cast button in the YouTube app. The phone is the remote, with the full queue. |
| **Spotify Connect** | Select the receiver in the Spotify app. It plays through your account, and no account data stays on disk. |
| **Miracast** | Mirror a Windows or Android screen over Wi-Fi Direct, or over the regular network ([MS-MICE]). |
| **Matter Casting** | Cast from an app that supports Matter. The receiver commissions the phone onto its own fabric. |
| **Bluetooth audio** | Pair a phone and play audio. The receiver has its own Bluetooth stack and supports SBC, AAC, aptX, and aptX HD. The configuration can also enable LDAC. |
| **GameStream / Moonlight** | The receiver is also a Moonlight client. Pair it with a PC that runs Sunshine, and play games on the panel. |

All protocols are on by default. The `[enable]` table in the configuration file
disables a protocol at runtime.

## On the panel

The idle screen is a launcher. It shows one tile for each enabled service, with
instructions and the exact name to look for. A now-playing card shows cover art,
track data, and touch controls: previous, play-pause, next, shuffle, repeat, and a
scrubber. The card only shows the controls that the current source supports.

An embedded Chromium browser (Electron) shows the real YouTube TV app. Filter lists
from EasyList and uBlock Origin block advertisement requests and inject scriptlets.
The receiver downloads new lists daily and applies them when the browser starts. If
SponsorBlock is enabled in the configuration, the receiver skips sponsor segments
and shows a toast with attribution.

## Watch and control from a browser

The receiver serves its own display over HTTP:

- `/screenshot.png` — a one-shot capture of the display.
- `/stream/` — a live HLS copy of the display, with sound. The encoder runs only
  while a client watches, and it stops 10 seconds after the last request.
- `/remote/` — a live view over WebRTC that also accepts touch input. A phone
  browser can drive the panel from across the room.

CAUTION: Do not expose the HTTP port outside a trusted network. The port has no
authentication, so each person on the network can watch and control the panel. Set
`remote.input = false` to keep the view and refuse the input.

## Configuration

castaway reads `castaway.toml` from the working directory, or from the platform
configuration directory. Every
feature is on by default. The configuration file disables features, sets ports, and
sets the log levels. Example:

```toml
[enable]
miracast = false          # disable one protocol

[sponsorblock]
enabled = true
categories = ["sponsor", "selfpromo", "music_offtopic"]

[log]
level = "info"
to_file = true
```

## Quick start

1. Install Nix, with flakes enabled.
2. Run `nix run .` to start the full receiver: renderer, browser, audio, and
   Bluetooth.

NOTE: `nix run .#castaway-portable` starts the protocol stack alone, with no
renderer and no browser.

For build, test, and environment instructions, see [DEVELOPMENT.md](DEVELOPMENT.md).

## Project status

castaway is a private, single-box project. Some paths are complete in code but not
yet proven against real hardware. `docs/STATUS.md` records, for each crate, what is
tested and what is not.

The source tree is MIT. The default build links the GPL streaming core from
Moonlight, so the default binary is GPL-bound.

## Documentation

- [DEVELOPMENT.md](DEVELOPMENT.md) — build, test, and environment instructions.
- `docs/hackerspace-receiver-build.md` — the goals and the protocol surface.
- `docs/architecture-substrate.md` — the workspace layout and the pipeline design.
- `docs/STATUS.md` — the per-crate record of what is real.

The other files in `docs/` are protocol records. Read the related record before you
change a protocol crate.
