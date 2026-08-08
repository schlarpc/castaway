# `eureka_info` — what a real device answers

The probe Play Services sends on `urn:x-cast:com.google.cast.setup` after device auth,
and the reply a real Chromecast gives it. Leaving this unanswered is what kept castaway
out of every GMS picker while every other symptom looked correct (#226).

## How these were captured

On the `checks.android-bt` bench extended to the network slice (#225): an Android
emulator running Play Services discovers a receiver, and `castaway`'s own log records the
inbound request verbatim. The *response* half came from asking a real Google Home Mini on
the development LAN the identical question — one `CONNECT` and one `eureka_info` query,
read-only, nothing launched and no state changed (`nix/../ask-eureka.py` on the bench).

## Redaction

`response.json` is the real reply with its **identifiers and room names replaced**: the
device is somebody's home, and `cloud_device_id`, `ssdp_udn`, `name` and the speaker-group
names identify it and the rooms it sits in. Replacements keep the original *shape* — field
widths, hex casing, dashed vs undashed UUID form — because the shape is the whole point of
the fixture and the values are not. `request.json` is verbatim; it contains nothing
device-specific.

What was **not** changed: the key set, their order, `version`, `response_code`,
`response_string`, and the structure of `multizone` (including that a group carries an
`elected_leader` as `host:port` and that `groups` is a list of objects). Those are the
parts a receiver has to match.

## What it shows, and what castaway does with it

- The reply repeats `type: "eureka_info"` rather than using a `responseType` — the same
  unusual shape `GET_DEVICE_INFO` has, and the same trap: a sender matching on
  `responseType` never sees it.
- `request_id` is echoed, and `response_code`/`response_string` carry the 200.
- The requested dotted params are **nested**: `device_info.ssdp_udn` comes back as
  `ssdp_udn` inside a `device_info` object.
- A device answers the params it was *asked* for. castaway does the same, and omits
  `cloud_device_id` outright — it has no Google-cloud registration, and a receiver that
  invented one would be describing a device that does not exist.
