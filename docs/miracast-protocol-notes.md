# Miracast / Wi-Fi Display (WFD) protocol notes

Reference notes for implementing a **Miracast sink (receiver)** in `proto-miracast`. This is a
research document, not a conformance record — where it states something as fact it cites a primary
source (a spec, or a line of a real implementation); where it is inference it says so.

Scope note per ground rule 9: everything named here — MiracleCast, AOSP `libstagefright/wifi-display`,
gnome-network-displays, lazycast, wpa_supplicant — is a **wire-behaviour source and a fixture
mine**, not a runtime dependency. We reimplement.

**Terminology.** "Source" = the sending device (phone, laptop). "Sink" = us. The Wi-Fi Alliance
spells it *Wi-Fi Display / WFD*; *Miracast* is the certification brand.

**On sources.** The Wi-Fi Alliance **Miracast® Specification v2.3 is publicly downloadable**
(`https://www.wi-fi.org/system/files/Miracast_Specification_v2.3.pdf`) and is the normative
reference used throughout — table numbers like "Table 29" refer to it. Where a claim rests on an
older revision (v1.1 / v2.1, both mirrored, see §8.1) or on Microsoft's `[MS-WFDPE]` /
`[MS-MICE]` extensions, that is stated inline. This matters because a great deal of secondhand
Miracast documentation on the web is wrong in at least one load-bearing detail; several such
errors were caught while writing this by going back to the spec text, and they are flagged
where they occur.

## 0. The shape of the thing, in one page

A Miracast session is four loosely-coupled protocols stacked on a link the sink has to build
itself:

1. **A Wi-Fi Direct (P2P) group**, or an existing infrastructure LAN if the source speaks
   Miracast-over-Infrastructure. The sink advertises a **WFD Information Element** in its
   beacons/probe responses saying "I am a primary sink, my RTSP port is 7236". This is the layer
   that will cost us the most, and none of it is protocol work — it is Wi-Fi driver work. (§1)
2. **An RTSP 1.0 control session on TCP 7236.** Two surprises here: **the sink dials out to the
   source** (the source is the TCP server, despite the sink being the one that advertises a
   "control port"), and the client/server roles then *invert* halfway through the handshake — the
   source drives OPTIONS/GET_PARAMETER/SET_PARAMETER, the sink drives SETUP/PLAY/PAUSE/TEARDOWN.
   Both endpoints are simultaneously an RTSP client and an RTSP server on one socket. (§2)
3. **A capability negotiation** carried in `text/parameters` bodies — a tiny ASCII key-value
   language with fixed-width hex fields (`wfd_video_formats`, `wfd_audio_codecs`, …). (§3)
4. **The media itself**: H.264 (+ AAC/LPCM/AC-3) muxed into MPEG-2 TS, 7 TS packets per RTP
   packet, RTP payload type 33, UDP unicast to a port the sink chose. (§4)

Plus two optional side-channels: **UIBC** (touch/keyboard back-channel, its own TCP socket, §5)
and **HDCP 2.x** (its own TCP socket, §6). Both are skippable; a sink that answers
`wfd_content_protection: none` still gets a picture from Windows and Android.

**The honest summary for planning:** the protocol above the link layer is small, fully
sans-I/O-able, and testable against checked-in fixtures. The link layer is the project. See §7.6
for the driver reality and §7.7 for whether a third-party app can even be a Miracast sink on the
Windows deploy target.

### Reading order for implementation

* Build §2 + §3 first as a pure `fn(state, bytes) -> (state, outputs)` core. It is ~1500 lines of
  Rust and needs no radio at all — you can drive it end-to-end from a checked-in transcript.
* §4 next: an RTP depacketiser and TS demuxer feeding `FrameSource::Encoded`. Also pure, also
  fixture-testable.
* §1 last, behind a `MiracastBackend` trait (per architecture-substrate.md), because it is the part
  that differs between Linux (`nl80211`/wpa_supplicant P2P) and Windows, and the part most likely
  to be impossible on one of them. Within §1, do **Miracast-over-Infrastructure (§1.10) before
  Wi-Fi Direct** — it needs only mDNS, a TCP listener and an RTSP client, Windows 10 v1703+
  actively prefers it, and the RTSP session downstream is byte-identical either way. That gets a
  working Windows demo with the least hardware risk.
## 1. Discovery and connection setup

This is the layer that will cost us the most, and almost none of it is protocol work — it is Wi-Fi
driver work. Read §7.6 before estimating.

### 1.1 Two vendor IEs, two endiannesses

Wi-Fi Direct and Wi-Fi Display each add a vendor-specific IE. They look almost identical and
differ in exactly the way that produces silent, hard-to-see bugs:

| | P2P IE | **WFD IE** |
|---|---|---|
| Element ID | 221 (`0xDD`) | 221 (`0xDD`) |
| OUI | `50-6F-9A` | `50-6F-9A` |
| OUI Type | `0x09` | **`0x0A`** |
| Vendor type dword | `0x506F9A09` | `0x506F9A0A` |
| Attribute/subelement length field | 2 bytes **little**-endian | 2 bytes **big**-endian |

From hostap `src/common/ieee802_11_defs.h:1514-1518`:

```c
#define OUI_WFA 0x506f9a
#define P2P_IE_VENDOR_TYPE 0x506f9a09
#define WFD_IE_VENDOR_TYPE 0x506f9a0a
#define WFD_OUI_TYPE 10
```

P2P attributes use `wpabuf_put_le16()` (`src/p2p/p2p_build.c:88`); WFD subelements use
`WPA_GET_BE16()` (`wpa_supplicant/wifi_display.c:318`) and Wireshark's dissector renders them
`ENC_BIG_ENDIAN` (`epan/dissectors/packet-wifi-display.c:370`). **Make these two distinct newtypes
in Rust** — a raw `u16` here is a bug waiting to happen.

### 1.2 WFD IE framing

Miracast v2.3 Table 25:

| Field | Octets | Value |
|-------|--------|-------|
| Element ID | 1 | `0xDD` |
| Length | 1 | *"4 plus the total length of WFD subelements"* |
| OUI | 3 | `50-6F-9A` |
| OUI Type | 1 | `0x0A` |
| WFD subelements | var | one or more |

Each subelement: **Subelement ID (1) + Length (2, big-endian) + body**.

**Fragmentation.** An IE body maxes out at 255 bytes, so a long subelement set is split across
*several* WFD IEs in the same frame:

> *"If multiple WFD IEs are present, the complete WFD subelement data consists of the
> **concatenation** of the WFD subelement fields… The WFD subelements field of each WFD IE may be
> any length up to the maximum (**251 octets**)… If a WFD subelement is not contained entirely
> within a single WFD IE, the WFD subelement ID field and Length field for that subelement occur
> **only once at the start**."*

hostap implements exactly this (`src/p2p/p2p_group.c:283-308`, chunking at 251). **A parser must
concatenate every WFD IE payload in the frame *before* parsing subelements** — parsing each IE
independently will corrupt any subelement that straddles a boundary.

Forward compatibility: *"A WFD Device that encounters an unknown or reserved subelement ID value…
shall ignore that WFD subelement and parse any remaining fields."*

### 1.3 Subelement IDs — note what v2.3 deleted

Miracast v2.3 Table 27:

| ID | Name | Status |
|----|------|--------|
| 0 | **WFD Device Information** | mandatory in every frame that carries the IE |
| 1 | Associated BSSID | |
| **2–5** | **Reserved** | ← were Audio Formats / Video Formats / 3D Video Formats / Content Protection in v1.0; **deleted** |
| 6 | Coupled Sink Information | |
| 7 | WFD Extended Capability | |
| 8 | Local IP Address | TDLS only |
| 9 | WFD Session Information | GO only |
| 10 | Alternative MAC Address | |
| 11 | WFD R2 Device Information | R2 |
| 12–255 | Reserved | |

> The A/V capability subelements (2–5) were **removed** from the IE — v2.3 §5.1.5: *"The WFD Video
> Formats subelement is no longer defined"*, and likewise §5.1.6/§5.1.7 for 3D and audio. Their
> content now lives exclusively in the RTSP `wfd_*` parameters (§3). hostap and Wireshark still
> name them for legacy R1 peers, so **keep them parseable-but-ignored**; never emit them.

### 1.4 WFD Device Information subelement (ID 0)

The one subelement you cannot omit. Body is always 6 octets:

```
+------+---------+---------------------+---------------------+------------------------+
| 0x00 | 00 06   | Device Info (BE16)  | Ctrl Port (BE16)    | Max Throughput (BE16)  |
+------+---------+---------------------+---------------------+------------------------+
```

* **Session Management Control Port** — default **7236**. *"(If a WFD Sink that is transmitting
  this subelement does not support the RTSP server function, this field is set to all zeros.)"*
  The spec recommends 49152–65535 for alternatives. **Ignore that advice and use 7236.** Android
  hard-codes the port with exactly one quirk override (`WifiDisplayController.java:1039-1046`):

  ```java
  if (device.deviceName.startsWith("DIRECT-") && device.deviceName.endsWith("Broadcom")) {
      return 8554;   // These dongles ignore the port we broadcast in our WFD IE.
  }
  return DEFAULT_CONTROL_PORT;   // 7236
  ```
* **WFD Device Maximum Throughput** — *"in multiples of 1 Mbps"*.

**Device Info bitmap** (Miracast v2.3 Table 29; bit 0 = LSB of the big-endian u16). Confirmed three
independently: the spec, Wireshark's masks (`packet-wifi-display.c:426-472`), and Android's
`WifiP2pWfdInfo.java:112-195`.

| Bits | Mask | Name | Values |
|------|------|------|--------|
| 1:0 | `0x0003` | **WFD Device Type** | `0b00` Source · `0b01` Primary Sink · `0b10` Secondary Sink · `0b11` dual-role |
| 2 | `0x0004` | Coupled Sink Support at Source | only meaningful for type `0b00`/`0b11` |
| 3 | `0x0008` | Coupled Sink Support at Sink | only meaningful for type `0b01`/`0b10`/`0b11` |
| 5:4 | `0x0030` | **WFD Session Availability** | `0b00` Not available · `0b01` Available · `0b10`,`0b11` **Reserved** |
| 6 | `0x0040` | WFD Service Discovery Support | see §1.7 — set to 0 |
| 7 | `0x0080` | **Preferred Connectivity (PC)** | `0b0` Wi-Fi Direct · `0b1` TDLS |
| 8 | `0x0100` | Content Protection (HDCP 2.x) support | see §6 — set to 0 |
| 9 | `0x0200` | Time Synchronization (802.1AS) | |
| 10 | `0x0400` | Audio unsupported at Primary Sink | |
| 11 | `0x0800` | Audio-only support at WFD Source | |
| 12 | `0x1000` | TDLS Persistent Group | |
| 13 | `0x2000` | TDLS Persistent Group Re-invoke | |
| 15:14 | `0xC000` | Reserved | zeros |

**The availability bits are a hard gate**, v2.3 §4.5: *"If a discovered WFD Device sets WFD Session
Availability bits (B5B4) … to 0b00 (i.e., not available), other WFD devices **shall not attempt**
WFD Connection establishment with that WFD Device until that WFD Device indicates its
availability."* So a sink that is busy with one source advertises `0b00` and disappears from
everyone's list — that is the intended mechanism, not a hack.

**Field-proven sink values** — both accepted by shipping Windows and Android senders, ideal golden
fixtures:

```
000600111c4400c8      MiracleCast (src/ctl/wifictl.c:148) and lazycast
 │   │    │    └── 0x00c8 = 200 Mbps
 │   │    └─────── 0x1c44 = port 7236
 │   └──────────── 0x0011 = Primary Sink (0b01) + Session Available (0b01)
 └──────────────── ID 0, length 6

000601511c440036      sigma-dut (Wi-Fi Alliance certification harness), sink
000601101c440036      sigma-dut, source
```

### 1.5 The other subelements

| ID | Name | Body |
|----|------|------|
| 1 | Associated BSSID | `00 06` + 6-byte BSSID of the AP/GO you are associated with |
| 6 | Coupled Sink Information | `00 07` + status bitmap (1 B: bits 1:0 = `0b00` not coupled · `0b01` coupled · `0b10` teardown · `0b11` reserved) + 6-byte coupled sink MAC (zeros if not coupled) |
| 7 | WFD Extended Capability | `00 02` + BE16 bitmap (below) |
| 8 | Local IP Address | `00 05` + version (`0x01` = IPv4 follows) + 4-byte IPv4. **TDLS only.** |
| 9 | WFD Session Information | list of 24-octet descriptors, one per WFD-capable associated client. **GO only**, and *"shall not include GO itself"*. |
| 10 | Alternative MAC Address | `00 06` + 6-byte MAC, when the connection will use a different interface than discovery did |
| 11 | WFD R2 Device Information | `00 02` + BE16: bits 1:0 = `0b00` R2 Source · `0b01` R2 Primary Sink · `0b10` **Reserved** · `0b11` dual-role; bits 15:2 reserved |

**WFD Extended Capability bitmap** (Table 47; masks identical in Wireshark
`packet-wifi-display.c:579-605`):

| Bit | Mask | Name |
|-----|------|------|
| 0 | `0x0001` | **UIBC Support** |
| 1 | `0x0002` | I2C Read/Write Support |
| 2 | `0x0004` | Preferred Display Mode Support |
| 3 | `0x0008` | Standby and Resume Control Support |
| 4 | `0x0010` | TDLS Persistent Support |
| 5 | `0x0020` | TDLS Persistent BSSID Support |
| 15:6 | | Reserved |

Each Session Information descriptor is: length (1, = **23**) + device address (6) + associated
BSSID (6) + WFD Device Information (2, same bitmap as Table 29) + max throughput (2) + coupled sink
info (7).

**R2 is additive, never a replacement:** *"All WFD R2 devices shall include the WFD R2 Device
Information subelement in the WFD IE… All WFD R2 devices shall **continue including WFD Device
Information subelement**."* Note there is no R2 Secondary Sink; Android's
`WifiP2pWfdInfo.setR2DeviceType()` rejects it.

### 1.6 Which subelements go in which frame

Miracast v2.3 §5.2, Tables 54–66. `M` = shall, `C` = conditional.

| Frame | 0 DevInfo | 1 AssocBSSID | 6 CoupledSink | 7 ExtCapab | 9 SessionInfo | 10 AltMAC | 11 R2 |
|---|---|---|---|---|---|---|---|
| **Beacon** (GO only) | M | C¹ | C² | – | – | – | – |
| **Probe Request** | M | C¹ | C³ | – | – | – | M (if R2) |
| **Probe Response** | M | C¹ | C³ | C⁴ | C⁵ | C⁶ | C⁷ |
| **(Re)Assoc Request** | M | C¹ | C³ | – | – | – | M (to R2 peer) |
| **(Re)Assoc Response** | M | C¹ | C³ | – | C⁵ | – | C⁷ |
| **GO Neg Req/Resp/Confirm** | M | C¹ | C³ | – | – | – | C⁷ |
| **P2P Invitation Req/Resp** | M | C¹ | C³ | – | C⁵ | – | C⁷ |
| **Provision Discovery Req** | M | C¹ | C³ | – | – | – | C⁷ |
| **Provision Discovery Resp** | M | C¹ | C³ | – | C⁵ | – | C⁷ |

¹ if associated with an AP/GO **and** PC bit = 1 · ² if sink supports Coupled Sink and is the GO ·
³ if the sink supports Coupled Sink Operation · ⁴ if advertising TDLS persistent capability ·
⁵ if a WFD-capable GO has ≥1 WFD-capable associated client · ⁶ if a different interface will carry
the connection · ⁷ if the peer is R2.

hostap's implementation (`wpa_supplicant/wifi_display.c:105-222`) is a faithful and simpler
summary, and emits R2 DevInfo immediately after DevInfo in every set.

### 1.7 P2P Service Discovery for WFD is dead — do not implement it

Miracast v2.3 §4.3.1: *"Because the Service Discovery using WFD IE as defined in the Miracast
Specification v1.0 has been **deprecated, this section is removed**."* §5.2.7.1: *"This feature is
not applicable for a WFD R2 Device."*

The WSD bit (Device Info bit 6) gates whether a peer will send you a WFD-typed SD query
(`src/p2p/p2p_sd.c:18-46`). Android never issues one; Windows never mentions it; MiracleCast and
lazycast ship with the bit clear. **Set it to 0.** All capability negotiation happens over RTSP M3.

Likewise **Wi-Fi Direct Services / ASP buys nothing.** Grepping the whole Miracast v2.3 text for
"WFDS", "ASP", "Application Service Platform" returns zero hits. The service names do exist
(`org.wi-fi.wfds.display.tx` / `.rx`, from hostap `tests/hwsim/test_p2ps.py:653-666`), but Miracast
R2 does not use them — it uses plain P2P probe exchanges with the WFD IE, or mDNS over
infrastructure.

### 1.8 Group formation, and the GO-role trap

**P2P Public Action frame header** (`src/p2p/p2p_build.c:30-40`): Category `0x04`, Action `0x09`,
OUI+Type `50 6F 9A 09`, OUI Subtype (1), Dialog Token (1), then the IEs. Subtypes: 0 GO Neg Req,
1 GO Neg Resp, 2 GO Neg Confirm, 3 P2P Invitation Req, 4 P2P Invitation Resp, 5 Device
Discoverability Req, 6 Device Discoverability Resp, 7 Provision Discovery Req, 8 Provision
Discovery Resp.

**Discovery**: social channels are **1, 6, 11** (2412/2437/2462 MHz, plus 60480 for 60 GHz —
`p2p_supplicant.c:435`). Find phase alternates Search (probe requests on the social channels) with
Listen (dwell 100–300 TU ≈ 102–307 ms by hostap default). Android re-runs `discoverPeers()` every
10 s. The wildcard SSID is `"DIRECT-"`.

**GO Intent** is one byte: `(intent << 1) | tie_breaker`. Winner logic
(`src/p2p/p2p_go_neg.c:21-33`): higher intent wins; equal intent uses the tie-breaker bit
(a coin flip); **both at 15 is a hard failure** (`P2P_SC_FAIL_BOTH_GO_INTENT_15 = 9`).

> ### ⚠ Windows and Android want opposite GO roles
>
> * **Android** sets `groupOwnerIntent = GROUP_OWNER_INTENT_MIN = 0`
>   (`WifiDisplayController.java:701`) — it wants **the sink to be GO**.
> * **Windows** *"will prefer to act as the Wi-Fi Direct GO (**Intent 14**)"*
>   ([Microsoft receiver requirements](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/wireless-projection-receiver-manufacturers)).
>
> | Sink strategy | vs. Windows (14) | vs. Android (0) | Verdict |
> |---|---|---|---|
> | **Autonomous GO** | Windows joins as client | Android joins as client | **Best** |
> | GO intent 1–13 | Windows becomes GO | sink becomes GO | Works, deterministic |
> | GO intent 15 | **both-15 → failure risk** | sink becomes GO | Risky |
> | GO intent 0 (MiracleCast's default, `src/wifi/wifid.c:48`) | Windows GO | **coin flip** | Avoid |
>
> **Recommendation: be an autonomous GO.** Bring the group up unilaterally
> (`P2P_GROUP_ADD [persistent]`), beacon it, and let peers associate — no GO negotiation state
> machine at all. This is what Microsoft's own Surface Hub does: *"Surface Hub takes advantage of
> Wi-Fi Direct 'Autonomous mode,' which skips the GO negotiation phase… And Surface Hub is always
> the group owner."* Android handles it (`action = dev.isGroupOwner() ? JOIN_GROUP : FORM_GROUP`)
> and Windows is documented as client-capable.

**Group SSID** is `"DIRECT-"` + 2 random chars + a postfix, usually the device name → e.g.
`DIRECT-Ab-HackerspacePanel`. Cipher is CCMP/WPA2-PSK.

**Provisioning** is **WPS PBC** in practice, on both platforms. Android's selection
(`WifiDisplayController.java:686-697`): PBC if the sink advertises it, else the mirror-image PIN
method. Windows uses PBC on first pairing. Device Password ID for PBC is `0x0004`; config method
bit is `0x0080` (`PUSHBUTTON`). **No DPP / Wi-Fi Easy Connect is involved.**

**Persistent groups** are worth supporting: *"When persistent P2P groups are supported on a
receiver, Windows will perform a reconnection rather than a pairing and therefore connect more
quickly."* WFD R2 devices *shall* support them. Microsoft adds an important error-path detail:

> *"If the Miracast receiver is acting as a GO, a peer may attempt to reconnect to you… If you no
> longer have a profile for that peer, fail the association with an association response frame
> which includes a P2P IE with status (**8 — Fail; Unknown P2P Group**); Windows 10 will retry the
> connection with a new pairing attempt."*

### 1.9 WPS device type — set it, but it is not the gate

8 bytes big-endian: `Category(BE16) | OUI(4) | SubCategory(BE16)`. String form `%u-%08X-%u`.
`7-0050F204-1` = `00 07 00 50 F2 04 00 01` = **Displays / Television**. Sub-categories: 1
Television, 2 Electronic Picture Frame, 3 Projector, 4 Monitor.

**But nothing filters on it.** Android's `isWifiDisplay()` (`WifiDisplayController.java:1048-1057`)
checks only that the WFD IE is present, enabled, and the device type is Primary Sink or dual-role —
`primaryDeviceType` is not consulted. MiracleCast advertises `1-0050F204-1` (Computer/PC) and
Windows connects anyway. Real dongles disagree with each other (PTV3000 `7-0050F204-1`, Q-WH-D1
`8-0050F204-5`).

Use `7-0050F204-1` anyway — it is what lazycast (a known-working Win+K target) uses and it gives
the right icon in the picker.

**Microsoft *does* require the WPS identity attributes** for telemetry: `WPS:Manufacturer`,
`WPS:Model` (must be unique per model), and `WPS:Model-Number` (**used as the firmware version**).
Listed as *Required* in the receiver checklist.

### 1.10 Miracast over Infrastructure — [MS-MICE]

Windows 10 v1703+ prefers to run Miracast over the existing WLAN instead of forming a Wi-Fi Direct
group. This is **the cheapest path to a working Windows demo** because it needs no P2P data path —
just mDNS, a TCP listener, and an RTSP client. Spec:
<https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-mice/>
([PDF](https://winprotocoldocs-bhdugrdyduf5h2e4.b02.azurefd.net/MS-MICE/%5bMS-MICE%5d.pdf), 48 pp).

**Important: MICE does not replace discovery, only the data path.** The sink must *still* be a
Wi-Fi Direct peer beaconing a WSC Vendor Extension attribute, because that is how Windows learns
MICE is available. Only the group-formation work is avoided.

**Control channel: TCP port 7250.** (Not UDP. And despite [MS-MICE] §1.9 citing `[IANAPORT]`, 7250
is *not* IANA-registered — 7236 is.)

**Message header** — all multi-byte fields big-endian:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 2 | **Size** — *"the size of the entire message, in bytes"*, **including this 4-byte header** |
| 2 | 1 | Version — `0x01` |
| 3 | 1 | Command |
| 4 | var | TLV array |

| Command | Value | Sender | TLVs |
|---------|-------|--------|------|
| `SOURCE_READY` | `0x01` | Source | Friendly Name (omitted if a Session Request preceded), RTSP Port, Source ID |
| `STOP_PROJECTION` | `0x02` | either | Friendly Name, Source ID |
| `SECURITY_HANDSHAKE` | `0x03` | either | Security Token |
| `SESSION_REQUEST` | `0x04` | Source | Friendly Name, Source ID, Security Options — *"MUST be the first message sent by the Source"* |
| `PIN_CHALLENGE` | `0x05` | Source | Source ID, PIN Challenge |
| `PIN_RESPONSE` | `0x06` | Sink | Source ID, PIN Response Reason |

**TLV**: Type (1) + Length (2, ≥ 1) + Value.

| Type | Name | Length | Value |
|------|------|--------|-------|
| `0x00` | FRIENDLY_NAME | ≤ 520 | **UTF-16LE**, no NUL |
| `0x02` | RTSP_PORT | 2 | source's RTSP port, default 7236 |
| `0x03` | SOURCE_ID | 16 | opaque source identifier |
| `0x04` | SECURITY_TOKEN | var | DTLS handshake payload (RFC 6347) |
| `0x05` | SECURITY_OPTIONS | ≥ 1 | bit0 `UseDtlsStreamEncryption`, bit1 `SinkDisplaysPin` (**bit0 MUST be set if bit1 is**) |
| `0x06` | PIN_CHALLENGE | var | salted SHA-256 of the PIN |
| `0x07` | PIN_RESPONSE_REASON | 1 | `0x00` accepted · `0x01` wrong PIN · `0x02` invalid message |

Note `0x01` is unassigned. Note also the mixed endianness: every length/size field is big-endian
but the friendly name is **UTF-16LE**. Model that at the boundary.

**Sink advertisement — the WSC Vendor Extension attribute**, carried in the 802.11 WSC IE of
Beacons and Probe Responses:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 2 | `0x1049` (WSC Vendor Extension) |
| 2 | 2 | length of what follows |
| 4 | 3 | OUI `00-01-37` (WPS ID) |
| 7 | var | one or more attributes, each ID(2) + Length(2) + body |

| ID | Attribute | Len | Body |
|----|-----------|-----|------|
| `0x2001` | **Capability** (mandatory) | 1 | bitfield below |
| `0x2002` | **Host Name** (exactly once) | var | ASCII, **unqualified** — *"A Sink having a host name that contains the period ('.') character MUST NOT be used"* |
| `0x2003` | BSSID | 6 | of the associated AP |
| `0x2004` | Connection Preference | 4 | packed 4-bit transport IDs, 8 slots, descending preference: `0x1` MICE, `0x2` Wi-Fi Direct |
| `0x2005` | IP Address | var | ASCII dotted-quad or IPv6; *"SHOULD be the same set as the Sink's mDNS responder would provide"* |

Capability byte, LSB-first: bit 0 `MiracastOverInfrastructureSupport`, bit 1
`StreamEncryptionSupported`, bits 4:2 `Version` (= 1), bit 5 `PinSupported` (requires bit 1),
bits 7:6 reserved. The spec's example `0x05` = MICE on, encryption off, version 1, no PIN.

If bit 0 is clear, Windows *"MUST fall back to using standard Miracast."*

**Sink initialization**, [MS-MICE] §3.1.3, verbatim — this is the whole to-do list:

> *"Upon initialization, the Miracast Sink MUST register the following service instance name with
> the Sink's local mDNS implementation: `<instance name>._display._tcp.local`. The `<instance
> name>` is the friendly name of the Sink, which will be associated with both **port 7250** and
> the following TXT key-value pair: **Key:** `container_id`, **Value:** A GUID that identifies the
> Sink."*
>
> *"After registering the service instance name, the Sink MUST start listening on **TCP port
> 7250**… Finally, the Sink MUST begin being discoverable by Beacons and/or Probe Requests as in
> standard Miracast, except that every Beacon and Probe Response the Sink sends MUST include a
> Vendor Extension attribute."*

This converges nicely with WFA R2, which independently defines `_display._tcp` for the sink and
`_displaysrc._tcp` for the source (Miracast v2.3 §4.4.1) — **one mDNS responder can serve both**,
which is exactly what our single shared mDNS responder is for.

**Projection flow.** Three entry points, chosen by the source from the sink's advertised
capabilities:

1. **No security** — source listens on RTSP 7236, sends `SOURCE_READY` on 7250, **the sink then
   connects to the source's RTSP port**, and RTSP proceeds identically to Wi-Fi Direct Miracast
   (§2).
2. **Security handshake first** — DTLS in `SECURITY_HANDSHAKE`/`SECURITY_TOKEN` until complete,
   then as above. RTP and UIBC are then DTLS-encrypted.
3. **Session Request first (PIN)** — sink displays a random **8-digit** PIN, DTLS handshake,
   `PIN_CHALLENGE`/`PIN_RESPONSE` with encrypted TLV arrays, then as above.

Teardown is `STOP_PROJECTION` from either side, then close 7250.

**PIN computation** (§3.1.5.6.1): ASCII PIN (no NUL) ‖ **binary** sender IP, then SHA-256; each
side hashes with *its own* IP. Two ready-made test vectors:

```
PIN "12345678", IP 192.0.2.100
  input:  31 32 33 34 35 36 37 38  c0 00 02 64
  SHA256: 605409f832308ad0b893a7f91be42b26 4c7372b36e9077506e1b4cc183de79da

PIN "98765432", IP 2001:db8:1f::4242
  input:  39 38 37 36 35 34 33 32  20 01 0d b8 00 1f 00 00 00 00 00 00 00 00 42 42
  SHA256: b3452b2c46c83d28d8d464b6697a81d1 af3f356107e1d0731ea9bb183803f9c7
```

**Golden message fixtures** from [MS-MICE] §4:

```
Vendor Extension attribute:
10 49                 // WSC Vendor Extension
00 1B                 // length 27
00 01 37              // OUI (WPS ID)
20 01 00 01 05        // Capability: MICE=1, version=1
20 02 00 0F 44 75 6D 6D 79 31 2D 4B 61 62 79 6C 61 6B 65   // "Dummy1-Kabylake"

SOURCE_READY (61 = 0x3D bytes):
00 3D 01 01
00 00 1E <30 bytes UTF-16LE "Dummy1-Kabylake">
02 00 02 1C 44                                       // RTSP port 7236
03 00 10 91 F4 AB E9 EF F5 46 4A AE E2 69 72 2A ED 11 B5   // Source ID
```
Size check: 4 + (3+30) + (3+2) + (3+16) = 61 ✓ — confirming Size includes the header.

**Timers and error handling** (§3.1.5.8):

* **Session Establishment Timer** — started on TCP accept, cancelled when RTSP is established.
  **2 minutes** with PIN entry, **30 seconds** without.
* **Security Handshake Message Timer** — **1 second** per DTLS response.
* Timer expiry → tear down 7250.
* A second connection while one is established → *"SHOULD reject"*.
* **Any unexpected or unknown message for the current state → tear down 7250.** This is a
  textbook typestate: a `PIN_CHALLENGE` should not typecheck outside `WaitingForPin`.

**Windows behaviours worth knowing** (Appendix A): discovery timer 1.5 s; control-channel
connection timer 5 s; the source picks the **first** IP in the advertised set; DNS and mDNS are
attempted in parallel with first-responder-wins; `PinSupported` is configurable from Win10 v1809;
and **Windows will not attempt MICE over a wireless network lacking WPA2 link-layer security.**

### 1.11 TDLS (R1 only)

Miracast v2.3 §4.5.3: *"This section is only applicable to WFD R1 devices."* Both peers must be
associated to a common AP/GO and the link must be WPA2 (WEP/WPA → refuse with TDLS Setup Response
status 5). The Local IP Address subelement (ID 8) is mandatory in TDLS Setup Request/Response, and
the Device Type must not be `0b11` (the role must be fixed) — wrong role gives status 38.

Connectivity resolution (Table 11): TDLS is chosen **only** when both peers set PC = 1 *and* are on
the same BSSID; every other combination resolves to Wi-Fi Direct. Persistent TDLS groups were
deprecated and removed (§4.13.2).

**We should set PC = 0 and not implement TDLS.**

### 1.12 Recommended sink profile

```
device_name          = <friendly name>          # shown in Win+K / Cast screen
device_type          = 7-0050F204-1             # Displays / Television
config_methods       = pbc [display]
WPS Manufacturer / Model / Model-Number         # required by Microsoft
P2P role             = autonomous persistent GO # skips GO negotiation entirely
WFD subelem 0        = 000600111c4400c8         # Primary Sink, Available, port 7236, 200 Mbps
WFD subelem 7        = 0002 0001                # Extended Capability: UIBC supported
WFD subelem 11       = 0002 0001                # R2 Primary Sink, if advertising R2
  DevInfo bit 6 (WSD) = 0                       # P2P SD is deprecated
  DevInfo bit 7 (PC)  = 0                       # Wi-Fi Direct, not TDLS
  DevInfo bit 8 (CP)  = 0                       # no HDCP (§6)
mDNS                 = <name>._display._tcp.local, port 7250, TXT container_id=<GUID>
TCP listen           = 7250 (MS-MICE control), <hdcp port> (only if CP were enabled)
TCP connect-out      = <source IP>:7236 (RTSP), <source IP>:<uibc port> (UIBC)
```
## 2. The RTSP control session

### 2.1 Transport and framing

The WFD control channel is **RTSP 1.0 ([RFC 2326](https://www.rfc-editor.org/rfc/rfc2326)) over a
single TCP connection**.

> ### ⚠ The sink is the TCP *client*
>
> Miracast v2.3 §4.5.4, verbatim: *"The TCP connection shall be initiated by the WFD Sink. **The
> WFD Source plays the TCP server role and the WFD Sink plays the TCP client role.** A Control
> Port (default is 7236)…"*
>
> This is counter-intuitive — the sink advertises a Session Management Control Port in its WFD IE,
> which reads like a listen port, and much secondhand documentation says the sink listens. It does
> not. Confirmed four ways:
> * Miracast v2.3 §4.5.4 (normative text above).
> * AOSP, the *source*, accepts connections (`kWhatClientConnected` in `WifiDisplaySource.cpp`).
> * MiracleCast's sink calls `connect()` to `peer:7236` (`src/ctl/ctl-sink.c:578-603`).
> * lazycast's sink calls `sock.connect(server_address)` (`d2.py:58`).
>
> The default port is **7236** (`kWifiDisplayDefaultPort` in AOSP
> [`WifiDisplaySource.h:40`](https://android.googlesource.com/platform/frameworks/av/+/android-7.1.2_r36/media/libstagefright/wifi-display/source/WifiDisplaySource.h),
> and IANA-registered as `display/7236/tcp`, "Wi-Fi Alliance Wi-Fi Display Protocol").
>
> Note the two optional side-channels go the *other* ways, and mixing them up produces a silent
> hang rather than an error:
>
> | Channel | Listener | Connector | Port advertised by |
> |---------|----------|-----------|--------------------|
> | **RTSP** | Source | **Sink** | sink (WFD IE), default 7236 |
> | **UIBC** | Source | **Sink** | source (`wfd_uibc_capability port=`) |
> | **HDCP** | **Sink** | Source | sink (`wfd_content_protection port=`) |

Deviations from stock RTSP that matter:

* **The roles are inverted relative to normal RTSP.** The *sink* is the RTSP client for `SETUP`,
  `PLAY`, `PAUSE`, `TEARDOWN` (M6–M9); the *source* is the RTSP client for `OPTIONS`,
  `GET_PARAMETER`, `SET_PARAMETER` (M1, M3, M4, M5, M16). Both endpoints must therefore be a
  full RTSP client *and* a full RTSP server on the same socket, with two independent CSeq
  counters (one per direction). This is the single biggest structural difference from AirPlay's
  RTSP dialect and it is where naive implementations break.
* Requests and responses interleave freely on the one connection. A sink must not assume that the
  next thing it reads after sending a request is that request's response.
* There is **no SDP**. Capability negotiation is entirely in `text/parameters` bodies (§3), not
  `application/sdp`. `wfd_presentation_URL` replaces the SDP-derived media URL.
* Message bodies are `Content-Type: text/parameters`, `Content-Length`-delimited.

**Framing hazard (real, observed):** sources routinely put more than one RTSP message in a single
TCP segment, and split messages across segments. The reader must be a proper incremental parser
over a byte stream, not a `read()`-per-message loop. lazycast (a working Python sink) does
`sock.recv(2048)` per message and is known to mis-handle coalesced M4+M5; MiracleCast's
`rtsp_message` decoder is incremental and does this correctly
(`src/rtsp/rtsp.c`).

### 2.2 The M-message table

The Wi-Fi Display Technical Specification numbers each RTSP exchange `Mn`, defined in spec section
`6.4.n` (this section-numbering correspondence is confirmed by [MS-WFDPE] which cites
"[WF-DTS2.1] section 6.4.3" for M3, "6.4.4" for M4, "6.4.8" for M8, and "6.4.13" for M13 —
<https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-wfdpe/>).

| Msg | Method | Direction (request) | Purpose / body |
|-----|--------|---------------------|----------------|
| **M1** | `OPTIONS *` | Source → Sink | Source probes sink's method set. Carries `Require: org.wfa.wfd1.0`. No body. |
| **M2** | `OPTIONS *` | **Sink → Source** | Sink probes source's method set. Carries `Require: org.wfa.wfd1.0`. Sent immediately after answering M1. |
| **M3** | `GET_PARAMETER` | Source → Sink | Body = newline-separated list of parameter **names only** (no colons/values). Sink answers with `name: value` lines. This is the capability query. |
| **M4** | `SET_PARAMETER` | Source → Sink | Source states the chosen configuration (a subset of M3's names, now with values). Sink `200 OK`s, empty body. |
| **M5** | `SET_PARAMETER` | Source → Sink | Body is exactly `wfd_trigger_method: SETUP` (or `PLAY`/`PAUSE`/`TEARDOWN`). Tells the sink to issue the corresponding M6/M7/M9/M8. |
| **M6** | `SETUP` | **Sink → Source** | On the `wfd_presentation_URL` from M4. Carries `Transport:`. Response carries `Session:` (and `Transport:` with `server_port`). |
| **M7** | `PLAY` | **Sink → Source** | Carries `Session:`. After the 200 the source starts RTP. |
| **M8** | `TEARDOWN` | Either | Ends the session. |
| **M9** | `PAUSE` | **Sink → Source** | Pauses the stream; session survives. |
| **M10** | `SET_PARAMETER` | Source → Sink | `wfd_route` — change the audio rendering sink (primary sink vs. secondary). |
| **M11** | `SET_PARAMETER` | Source → Sink | `wfd_connector_type` — change the active display connector. |
| **M12** | `SET_PARAMETER` | Source → Sink | `wfd_standby` — source is entering/requesting WFD standby. |
| **M13** | `SET_PARAMETER` | **Sink → Source** | `wfd_idr_request` — sink asks for an IDR picture. |
| **M14** | `SET_PARAMETER` | Source → Sink | `wfd_uibc_capability` — establish the UIBC (open the back-channel port). |
| **M15** | `SET_PARAMETER` | Source → Sink | `wfd_uibc_setting: enable` / `disable` — turn UIBC on/off within a session. |
| **M16** | `GET_PARAMETER` | Source → Sink | **Keep-alive.** Empty body, carries `Session:`. Sink replies `200 OK`. |

Directions for M1–M9 are confirmed against the message table reproduced in
[EP3104551A1](https://patents.google.com/patent/EP3104551A1/en); M10/M11/M12 semantics are
confirmed by [WO2016048065A1](https://patents.google.com/patent/WO2016048065A1/en) ("RTSP M10 is a
request message to request the WFD source to change the audio rendering device, RTSP M11 … change
the active connector type, and RTSP M12 … WFD source enters WFD standby mode"). M13's identity is
confirmed by [MS-WFDPE] §2.6.1.1, which defines `wfd_idr_request_capability` as "whether a Wi-Fi
Display Sink supports sending an RTSP M13 message". M14/M15 assignment to UIBC
capability/setting is **inferred** from the parameter names and from the ordering of the remaining
slots; treat it as a labelling convention, not a wire fact — nothing on the wire carries the
message number.

### 2.3 Keep-alive and session timeout

* The **source** sends M16 (`GET_PARAMETER` with `Session:` and an empty body) as the keep-alive.
  Note the inversion: the party that owns the session ID is the sink's peer.
* AOSP sets `Session: <id>;timeout=30` and schedules M16 at `timeout - 5s`, i.e. every 25 s
  (`kPlaybackSessionTimeoutSecs = 30`, `scheduleKeepAlive()` posts at
  `kPlaybackSessionTimeoutUs - 5000000`).
  Source: [`WifiDisplaySource.h:113`](https://android.googlesource.com/platform/frameworks/av/+/android-7.1.2_r36/media/libstagefright/wifi-display/source/WifiDisplaySource.h),
  `WifiDisplaySource.cpp:1035`.
* The spec default when `timeout=` is absent is 60 s (RFC 2326 §12.37). **A sink should honour
  whatever the source put in `Session:` and should not itself time the session out aggressively** —
  several sources send M16 late.
* A sink that never receives M16 should *not* tear down on that basis alone; Windows in some
  configurations relies on RTP flow rather than M16.

### 2.4 Header requirements

**`Require: org.wfa.wfd1.0`** — present on M1 and M2 requests. Sinks must accept it (i.e. must not
answer `551 Option Not Supported`) and should send it on M2. Real senders will abort if the sink
551s.

**`Public:`** — the M1 *response* (sink → source) enumerates what the sink implements. Two real
values seen in the wild:

```
Public: org.wfa.wfd1.0, SET_PARAMETER, GET_PARAMETER
```
(lazycast `d2.py:203`; MiracleCast `src/ctl/ctl-sink.c:42-44` uses
`org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER`)

and the source-side value from AOSP / Windows:

```
Public: org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER
```
(AOSP `WifiDisplaySource.cpp:1145`; also the [MS-WFDPE] §3 example M2 response.)

Note that a **sink** legitimately advertises only `SET_PARAMETER, GET_PARAMETER` in `Public:`,
because a sink never *receives* SETUP/PLAY/PAUSE/TEARDOWN — it sends them. Advertising the full
set is harmless and is what most sinks do; advertising only the two is what the two working
open-source sinks do, and Windows accepts it.

**`CSeq:`** — mandatory on every request and mirrored on every response. **Two independent
counters**, one per direction. Both AOSP and lazycast start at 1 in each direction (lazycast
hard-codes `CSeq: 1` for its M1 response and `CSeq: 1` for its own M2 request — different
counters, same number, and Windows accepts it).

**`Session:`** — appears from the M6 response onward. The sink must strip everything from the first
`;` when echoing it back; MiracleCast does exactly this (`ctl-sink.c:186-188`).

> **The session ID is an opaque string. Do not `atoi()` it.** AOSP happens to emit a decimal
> (`Session: 1804289383;timeout=30`), which tempts you into an integer type — but a real Samsung
> sink emits `Session: VaMkltjy;timeout=60`. Model it as an opaque token newtype.
>
> **And read the `timeout=` rather than hard-coding it.** AOSP says 30 (keepalive at 25 s); the WFD
> spec default and intel/wds say 60 (keepalive at 55 s); that Samsung sink says 60. Drive the
> keepalive watchdog from whatever arrives in the SETUP response. **Real-world caveat, quoted from
AOSP:** `// XXX the older dongles do not always include a "Session:" header.` — AOSP falls back to
the single known session when the header is missing (`WifiDisplaySource.cpp:1627`). A sink should
be equally tolerant.

**`Transport:`** — on M6 request and response. Sink sends e.g.

```
Transport: RTP/AVP/UDP;unicast;client_port=1028
```

The source's M6 response echoes and adds `server_port`:

```
Transport: RTP/AVP/UDP;unicast;client_port=1028-1029;server_port=<n>-<n+1>
```

AOSP emits the `-`-range form only when the sink gave two ports, otherwise a single port
(`WifiDisplaySource.cpp:1324-1336`). Per [MS-WFDPE] §2.8.1.1, the source **MUST** send exactly one
`server_port` number unless the sink advertised `microsoft_rtcp_capability: supported`, in which
case it must send two.

**`User-Agent:` / `Server:`** — AOSP puts its user-agent in a `Server:` header on responses
(`AppendCommonResponse`, `WifiDisplaySource.cpp:1590`), built from the Android build fingerprint.
Observed real values:

* `stagefright/1.1 (Linux;Android 4.1)` (+ vendor suffix, e.g. `:rockchip`)
* `MSMiracastSource/10.00.10011.0000 guid/be113d06-9e40-43e4-98e6-540a325e9ced` — Windows.
  [MS-WFDPE] §2.5.1.1 specifies this exactly:

  ```
  source-product-id   = "MSMiracastSource"
  connection-id       = 8HEXDIG "-" 4HEXDIG "-" 4HEXDIG "-" 4HEXDIG "-" 12HEXDIG
  connection-id-token = "guid/" connection-id
  server-header-data  = source-product-id "/" product-version [ SP connection-id-token ] *( SP product )
  Server              = "Server:" SP server-header-data
  ```

  **This is the reliable way to detect a Windows source** and switch on Windows-specific
  behaviour. It is only present on RTSP *responses generated by the source*, i.e. on the M2
  response, so a sink learns it early.

**`Date:`** — AOSP sends it on every response (`%a, %d %b %Y %H:%M:%S %z`). Not required.

### 2.5 A complete, real M1→M7 transcript (sink's point of view)

Reconstructed from lazycast `d2.py` (a Python sink that is known to work against Windows 10/11 and
Android) plus the AOSP source implementation. `S→K` = source to sink.

```
S→K  OPTIONS * RTSP/1.0                                              (M1)
     CSeq: 1
     Require: org.wfa.wfd1.0

K→S  RTSP/1.0 200 OK
     CSeq: 1
     Public: org.wfa.wfd1.0, SET_PARAMETER, GET_PARAMETER

K→S  OPTIONS * RTSP/1.0                                              (M2)
     CSeq: 1
     Require: org.wfa.wfd1.0

S→K  RTSP/1.0 200 OK
     CSeq: 1
     Date: ...
     Public: org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER
     Server: MSMiracastSource/10.00.10011.0000 guid/be113d06-...

S→K  GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0                  (M3)
     CSeq: 2
     Content-Type: text/parameters
     Content-Length: NN

     wfd_content_protection
     wfd_video_formats
     wfd_audio_codecs
     wfd_client_rtp_ports
     wfd_uibc_capability
     wfd_display_edid
     wfd_connector_type
     wfd_idr_request_capability
     microsoft_latency_management_capability
     microsoft_cursor

K→S  RTSP/1.0 200 OK
     CSeq: 2
     Content-Type: text/parameters
     Content-Length: NN

     wfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play
     wfd_audio_codecs: AAC 00000001 00
     wfd_video_formats: 00 00 02 10 0001FFFF 3FFFFFFF 00000FFF 00 0000 0000 00 none none
     wfd_3d_video_formats: none
     wfd_coupled_sink: none
     wfd_connector_type: 05
     wfd_uibc_capability: input_category_list=GENERIC, HIDC;generic_cap_list=Keyboard, Mouse;hidc_cap_list=Keyboard/USB, Mouse/USB;port=none
     wfd_standby_resume_capability: none
     wfd_content_protection: none
     wfd_idr_request_capability: 1

S→K  SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0                  (M4)
     CSeq: 3
     Content-Type: text/parameters
     Content-Length: NN

     wfd_video_formats: 00 00 02 10 00000100 00000000 00000000 00 0000 0000 00 none none
     wfd_audio_codecs: AAC 00000001 00
     wfd_presentation_URL: rtsp://192.168.173.1/wfd1.0/streamid=0 none
     wfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play

K→S  RTSP/1.0 200 OK
     CSeq: 3

S→K  SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0                  (M5)
     CSeq: 4
     Content-Type: text/parameters
     Content-Length: 27

     wfd_trigger_method: SETUP

K→S  RTSP/1.0 200 OK
     CSeq: 4

K→S  SETUP rtsp://192.168.173.1/wfd1.0/streamid=0 RTSP/1.0           (M6)
     CSeq: 2
     Transport: RTP/AVP/UDP;unicast;client_port=1028

S→K  RTSP/1.0 200 OK
     CSeq: 2
     Session: 1804289383;timeout=30
     Transport: RTP/AVP/UDP;unicast;client_port=1028;server_port=19000

K→S  PLAY rtsp://192.168.173.1/wfd1.0/streamid=0 RTSP/1.0            (M7)
     CSeq: 3
     Session: 1804289383

S→K  RTSP/1.0 200 OK
     CSeq: 3
     Session: 1804289383

     ... RTP/MPEG2-TS begins flowing to UDP 1028 ...

S→K  GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0                  (M16, every ~25 s)
     CSeq: 5
     Session: 1804289383

K→S  RTSP/1.0 200 OK
     CSeq: 5
```

Note the **presentation URL** in M4 is the *source's* IP with the sink's session path
(`rtsp://<source-ip>/wfd1.0/streamid=0`), while M1/M3/M4/M5/M16 request-URIs use the literal
string `rtsp://localhost/wfd1.0`. Both AOSP and Windows do this. The sink must use the M4
`wfd_presentation_URL` verbatim in M6/M7, **not** reconstruct it.

### 2.6 The IDR request (M13)

The sink sends:

```
SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0
CSeq: <n>
Session: <id>
Content-Type: text/parameters
Content-Length: 16

wfd_idr_request
```

AOSP detects it with a literal substring match: `strstr(data->getContent(), "wfd_idr_request\r\n")`
(`WifiDisplaySource.cpp:1564`) — so the trailing CRLF is required and the parameter must be a bare
name with no colon. This is the sink's only recovery mechanism for a lost/corrupt reference frame;
it should be wired to the H.264 decoder's error path and rate-limited (Windows will honour it, but
spamming it collapses the bitrate).

A sink should advertise `wfd_idr_request_capability: 1` in M3 if and only if it will actually send
M13 — [MS-WFDPE] §2.6.1.1 states a source that knows the sink will never send M13 "can insert
IDR pictures more frequently … to compensate", so answering `0` buys you more frequent IDRs at the
cost of bitrate.
## 3. The `wfd-kv` parameter language

### 3.1 Lexical structure

Bodies are `Content-Type: text/parameters`. The grammar is deliberately trivial:

```
body        = *( line CRLF )
line        = param-name [ ":" SP param-value ]     ; name-only form is used in M3 requests
param-name  = 1*( ALPHA / DIGIT / "_" )
param-value = *VCHAR                                ; parameter-specific, see below
```

Rules that bite:

* **M3 *requests* carry names only** (no colon). M3 *responses*, M4, M5, M10–M15 carry
  `name: value`.
* Separator after the colon is **one space**. AOSP's parser trims, so it is tolerant; write one
  space.
* **Line terminator is CRLF.** AOSP's `Parameters::parse` scans for `\r\n` explicitly
  ([`Parameters.cpp:57`](https://android.googlesource.com/platform/frameworks/av/+/android-7.1.2_r36/media/libstagefright/wifi-display/Parameters.cpp)).
  A body ending without a final CRLF is malformed for some parsers; always terminate the last line.
* **Names are case-insensitive in practice.** AOSP lowercases both the parsed name and the lookup
  key. Emit them in the canonical lowercase spelling anyway (note `wfd_I2C` is the one parameter
  with an uppercase segment in its canonical form).
* Hex fields are **fixed-width, zero-padded, and conventionally uppercase or lowercase
  inconsistently** across senders — parse case-insensitively, emit either. Widths are load-bearing:
  AOSP's `wfd_video_formats` parser advances by a **hard-coded 60 bytes per codec tuple**
  (`VideoFormats::parseFormatSpec`, `offset += 60`), so a sink that emits a differently-padded
  tuple will be misparsed by Android.
* Multi-valued parameters use `", "` (comma + space) as the list separator.
* The token `none` is the universal "not supported / not present" value and is valid for almost
  every parameter.

**Parse, don't validate:** every parameter below has a small closed value space. In Rust these
should each be a distinct type with a `FromStr`/`Display` pair and a `thiserror` error enum, not a
`HashMap<String, String>`. The M3-response builder should be a struct whose `Display` emits the
body, so an un-answerable parameter is a compile-time missing field rather than a runtime lookup
miss.

---

### 3.2 `wfd_video_formats` — the important one

ABNF (from [MS-WFDPE] §2.7.1.1, which restates the [WF-DTS2.1] §6.1.3 grammar it is extending;
field widths cross-checked against AOSP `VideoFormats::getFormatSpec`):

```
wfd-video-formats = "wfd_video_formats:" SP sink-video-list CRLF
sink-video-list   = "none" / ( native SP preferred-display-mode-supported SP H264-codec )
native            = 2*2HEXDIG
preferred-display-mode-supported = 2*2HEXDIG      ; 0 = not supported, 1 = supported
H264-codec        = profile SP level SP misc-params SP max-hres SP max-vres
                    *( "," SP H264-codec )
profile           = 2*2HEXDIG
level             = 2*2HEXDIG
misc-params       = CEA-Support SP VESA-Support SP HH-Support SP latency SP
                    min-slice-size SP slice-enc-params SP frame-rate-control-support
CEA-Support       = 8*8HEXDIG
VESA-Support      = 8*8HEXDIG
HH-Support        = 8*8HEXDIG
latency           = 2*2HEXDIG
min-slice-size    = 4*4HEXDIG
slice-enc-params  = 4*4HEXDIG
frame-rate-control-support = 2*2HEXDIG
max-hres          = "none" / 4*4HEXDIG
max-vres          = "none" / 4*4HEXDIG
```

AOSP documents the same layout as a byte list in
[`VideoFormats.cpp:421-435`](https://android.googlesource.com/platform/frameworks/av/+/android-7.1.2_r36/media/libstagefright/wifi-display/VideoFormats.cpp):

```
// 1 byte "native"
// 1 byte "preferred-display-mode-supported" 0 or 1
// one or more avc codec structures
//   1 byte profile
//   1 byte level
//   4 byte CEA mask
//   4 byte VESA mask
//   4 byte HH mask
//   1 byte latency
//   2 byte min-slice-slice
//   2 byte slice-enc-params
//   1 byte framerate-control-support
//   max-hres (none or 2 byte)
//   max-vres (none or 2 byte)
```

#### `native`

One byte, but **two packed fields**:

```
bits 7:3  = resolution index within the table
bits 2:0  = table selector   0 = CEA, 1 = VESA, 2 = HH
```

AOSP encodes `(index << 3) | type` and decodes `index = native >> 3`, `type = native & 7`
(`VideoFormats.cpp:399-400`, `:439`). Worked example: the Windows sink capture
`wfd_video_formats: 40 00 01 10 …` has `native = 0x40` → index `8`, type `0` → **CEA index 8 =
1920×1080 p60**.

In an **M4** message the source sets `native` to `00` (AOSP passes `forM4Message=true` which
forces the field to zero) — the *chosen* mode is conveyed by the single set bit in the CEA/VESA/HH
mask, not by `native`.

#### `profile` (bitmap)

| Bit | Meaning |
|-----|---------|
| 0 | H.264 Constrained Baseline Profile (CBP), `profile_idc` 66, `constraint_set` 0xC0 |
| 1 | H.264 Constrained High Profile (CHP), `profile_idc` 100, `constraint_set` 0x0C |
| 2 | *(R2 / `wfdx_`)* H.265 v1 Main Profile (8-bit 4:2:0) |
| 3 | *(R2 / `wfdx_`)* H.265 v1 Main 10 Profile (10-bit 4:2:0) |
| 15:4 | reserved |

Bits 0/1 with their `profile_idc`/`constraint_set` values are from AOSP
`VideoFormats::GetProfileLevel` (`kProfileIDC = {66,100}`, `kConstraintSet = {0xc0,0x0c}`).
Bits 2/3 are from [MS-WFDPE] §2.7.1.1.1 (only meaningful in the 4-hex-digit `wfdx_video_formats`
form).

> **The "no CABAC" rule is R1-only, and Windows breaks it. Do not build a CAVLC-only decode path.**
>
> [MS-WFDPE] §2.7.1.1.1 and Miracast Table 6 both say entries "MUST NOT use the B slice tool and
> MUST NOT use the CABAC entropy coding tool", which reads like a licence to simplify the decoder.
> It is not. Miracast R2 Table 77 adds **RHP2** — *"formerly known as CHP, **with CABAC
> enabled**"* — plus MP/HiP in non-transcoding mode, and Microsoft's receiver requirements list
> *"Support for CABAC … for AVC (H264) Baseline Profile and High Profile"* as a receiver feature.
>
> **B-slices really are absent** in every profile, so DTS ≡ PTS and no reorder buffer is needed
> (§4.4). But use a general H.264 decoder and let the SPS decide the entropy coder.

#### `level` (bitmap)

| Bit | Level |
|-----|-------|
| 0 | 3.1 |
| 1 | 3.2 |
| 2 | 4.0 |
| 3 | 4.1 |
| 4 | 4.2 |
| 5 | *(ext)* 5.0 |
| 6 | *(ext)* 5.1 |
| 7 | *(ext)* 5.2 |

Bits 0–4 from AOSP `kLevelIDC = {31,32,40,41,42}`; bits 5–7 from [MS-WFDPE] §2.7.1.1.1 "Levels
Bitmap with Extension".

#### `latency`

2 hex digits. **Units are 5 ms.** gnome-network-displays parses it as
`latency_ms = strtoull(tok, 16) * 5`
([`src/wfd/wfd-audio-codec.c:114`](https://gitlab.gnome.org/GNOME/gnome-network-displays/-/blob/main/src/wfd/wfd-audio-codec.c) —
the same 5 ms unit applies to the audio latency field). `00` = unspecified. This is the *decoder*
latency the sink is declaring, not a request.

#### `min-slice-size`, `slice-enc-params`

`min-slice-size` = minimum number of macroblocks per slice the sink can decode; `0000` means the
sink can only handle **one slice per picture**. Per [MS-WFDPE] §2.7.1.1: "A Wi-Fi Display Source
MUST set the min-slice-size value to 0 in the RTSP M4 request message and MUST NOT transmit an
encoded picture constructed by multiple slices to a Wi-Fi Display Sink that does not support
decoding a picture constructed by multiple slices (the Wi-Fi Display Sink sets the min-slice-size
value to 0 in the RTSP M3 response message)."

`slice-enc-params` (4 hex digits) — **Miracast v2.3 Table 40**:

| Bits | Name | Meaning |
|------|------|---------|
| 9:0 | Max Slice Num | Maximum number of slices per picture, **minus 1** |
| 12:10 | Max Slice Size Ratio | Ratio of maximum slice size used to the `min-slice-size` field |
| 15:13 | Reserved | zero |

Spec rule: *"If this bitmap is used in an RTSP message and the minimum-slice-size field in the
wfd-video-formats parameter … is all zeros, all bits in this bitmap shall be set to zero. The
subfields [B9:B0] and [B12:B10] shall be set to a non-zero value in other cases."* So
`min-slice-size = 0000` ⟹ `slice-enc-params = 0000`, and they are only meaningful together. A
sink can safely emit both as `0000` and receive one slice per picture.

#### `frame-rate-control-support` (bitmap)

**Miracast v2.3 Table 41.** Note the bit assignment is *not* what most secondhand descriptions say
— frame-rate *change* is bit 4, not bit 0:

| Bits | Name | Meaning |
|------|------|---------|
| 0 | Video Frame Skipping Support | `0b0` not supported, `0b1` supported |
| 3:1 | Max Skip Interval | Reserved if B0 = 0. Max time between two successive frames after skipping = *(value) × 0.5 s*. `0b000` = no limitation. |
| 4 | **Video Frame Rate Change Support** | `0b0` dynamic refresh-rate change without user intervention not supported; `0b1` supported |
| 7:5 | Reserved | zero |

[MS-WFDPE] §2.7.1.1 constrains the handshake: the sink MUST set the Video Frame Rate Change
Support bit in M3 if it supports it, and if the sink does not set it, the source MUST NOT set it
in M4.

Real emitted values: `00` (AOSP, lazycast — no skipping, no rate change), `10` (MiracleCast — bit
4 set, i.e. frame-rate change supported), `11` ([MS-WFDPE] example and the observed Windows sink —
frame skipping *and* frame-rate change).

#### `max-hres` / `max-vres`

`none` or 4 hex digits of pixels. Windows sink capture: `0780 0438` = 1920 × 1080. Both AOSP and
lazycast emit `none none`.

#### The resolution tables

**These are the authoritative index → mode mappings** and the highest-value thing in this document.
Transcribed from AOSP
[`VideoFormats.cpp:28-134`](https://android.googlesource.com/platform/frameworks/av/+/android-7.1.2_r36/media/libstagefright/wifi-display/VideoFormats.cpp)
(`{width, height, fps, interlaced}`), cross-checked against
gnome-network-displays `src/wfd/wfd-video-codec.c`.

**CEA bitmap** (bit *n* ⇔ index *n*):

| Idx | Mode | Idx | Mode |
|-----|------|-----|------|
| 0 | 640×480 p60 | 9 | 1920×1080 **i**60 |
| 1 | 720×480 p60 | 10 | 1280×720 p25 |
| 2 | 720×480 **i**60 | 11 | 1280×720 p50 |
| 3 | 720×576 p50 | 12 | 1920×1080 p25 |
| 4 | 720×576 **i**50 | 13 | 1920×1080 p50 |
| 5 | 1280×720 p30 | 14 | 1920×1080 **i**50 |
| 6 | 1280×720 p60 | 15 | 1280×720 p24 |
| 7 | 1920×1080 p30 | 16 | 1920×1080 p24 |
| 8 | 1920×1080 p60 | 17–31 | reserved in v1; see extension below |

CEA **extension** indices (the 4K modes) exist in two mutually incompatible numberings. Only valid
in the extended parameters, whose CEA field is 10 hex digits = 40 bits.

| Idx | **WFA R2** (`wfd2_video_formats`) | **Microsoft** (`wfdx_video_formats`) |
|-----|-----------------------------------|--------------------------------------|
| 17 | 3840×2160 p24 | 3840×2160 p30 |
| 18 | 3840×2160 p25 | 3840×2160 p60 |
| 19 | 3840×2160 p30 | 4096×2160 p30 |
| 20 | 3840×2160 p50 | 4096×2160 p60 |
| 21 | 3840×2160 p60 | 3840×2160 p25 |
| 22 | 4096×2160 p24 | 3840×2160 p50 |
| 23 | 4096×2160 p25 | 4096×2160 p25 |
| 24 | 4096×2160 p30 | 4096×2160 p50 |
| 25 | 4096×2160 p50 | 3840×2160 p24 |
| 26 | 4096×2160 p60 | 4096×2160 p24 |
| 39:27 | reserved | reserved |

> **Same ten modes, completely different ordering.** WFA R2 groups by resolution then ascending
> refresh rate; Microsoft's ordering is its own. Only index 23 happens to agree. **Key strictly off
> the parameter name the peer used** — `wfd2_video_formats` ⟹ WFA numbering,
> `wfdx_video_formats` ⟹ Microsoft numbering — and never share a decode table between them. In
> Rust these should be two distinct enums, not one enum with a flag.
>
> The same divergence affects VESA; see the note under that table.

**VESA bitmap** (all progressive):

| Idx | Mode | Idx | Mode |
|-----|------|-----|------|
| 0 | 800×600 p30 | 15 | 1280×1024 p60 |
| 1 | 800×600 p60 | 16 | 1400×1050 p30 |
| 2 | 1024×768 p30 | 17 | 1400×1050 p60 |
| 3 | 1024×768 p60 | 18 | 1440×900 p30 |
| 4 | 1152×864 p30 | 19 | 1440×900 p60 |
| 5 | 1152×864 p60 | 20 | 1600×900 p30 |
| 6 | 1280×768 p30 | 21 | 1600×900 p60 |
| 7 | 1280×768 p60 | 22 | 1600×1200 p30 |
| 8 | 1280×800 p30 | 23 | 1600×1200 p60 |
| 9 | 1280×800 p60 | 24 | 1680×1024 p30 |
| 10 | 1360×768 p30 | 25 | 1680×1024 p60 |
| 11 | 1360×768 p60 | 26 | 1680×1050 p30 |
| 12 | 1366×768 p30 | 27 | 1680×1050 p60 |
| 13 | 1366×768 p60 | 28 | 1920×1200 p30 |
| 14 | 1280×1024 p30 | 31:29 | **Reserved in R1** |

> **Resolved — and there is a real spec conflict here worth knowing about.**
>
> Miracast v2.3 **Table 35** (the R1 `wfd-video-formats` VESA bitmap) ends at **index 28 =
> 1920×1200 p30**; bits 31:29 are *Reserved*. That is authoritative for `wfd_video_formats`.
>
> The **R2** extended VESA table (used by `wfd2_video_formats`) continues:
> **29 = 1920×1200 p60**, **30 = 2560×1440 p30**, **31 = 2560×1440 p60**,
> **32 = 2560×1600 p30**, **33 = 2560×1600 p60**.
>
> [MS-WFDPE] §2.7.1.1.1 disagrees with the WFA R2 table: it assigns **29 = 2560×1440 p30** and
> **30 = 2560×1440 p60** — i.e. Microsoft's `wfdx_video_formats` extension is offset by one
> against WFA's R2 numbering, having skipped 1920×1200 p60. **Do not assume the two extension
> tables agree.** Key off which parameter name the peer used: `wfd2_video_formats` ⟹ WFA
> numbering, `wfdx_video_formats` ⟹ Microsoft numbering.
>
> Implementations in the wild: MiracleCast advertises `0x1fffffff` (bits 0–28) — **spec-correct**.
> AOSP's table has a 30th entry (1920×1200 p60 at index 29) and lazycast advertises `0x3FFFFFFF`
> (bits 0–29) — both are reaching into R1's reserved space with the R2 meaning. Harmless in
> practice, but **our sink should advertise at most bits 0–28 in `wfd_video_formats`** and express
> anything above 1920×1200 p30 through the R2/Microsoft parameters.

Spec constraint on what you may advertise (Miracast v2.3 §5.1.5.2, and the same rule for CEA/HH):

> *"the WFD Sink shall indicate support for a resolution with higher refresh rate(s) **if and only
> if it also indicates support for a corresponding lower refresh rate**. For instance, support for
> 720x480p60 can be indicated only if support for 720x480p30 is also indicated."*

This is a genuine invariant, not advice — encode it in the type that builds the masks so an
inconsistent bitmap cannot be constructed.

**HH ("handheld") bitmap** (all progressive):

| Idx | Mode | Idx | Mode |
|-----|------|-----|------|
| 0 | 800×480 p30 | 6 | 640×360 p30 |
| 1 | 800×480 p60 | 7 | 640×360 p60 |
| 2 | 854×480 p30 | 8 | 960×540 p30 |
| 3 | 854×480 p60 | 9 | 960×540 p60 |
| 4 | 864×480 p30 | 10 | 848×480 p30 |
| 5 | 864×480 p60 | 11 | 848×480 p60 |
| | | 12–31 | reserved |

Note MiracleCast advertises HH `0x00001fff` = bits 0–12, i.e. **one bit past the end of the
defined table**. lazycast advertises `0x00000FFF` = bits 0–11, which is correct. Harmless in
practice (sources intersect against their own table) but don't copy MiracleCast's value.

#### Known-good real values

| Source | String |
|--------|--------|
| lazycast sink (works vs. Win10/11 + Android) | `00 00 02 10 0001FFFF 3FFFFFFF 00000FFF 00 0000 0000 00 none none` |
| MiracleCast sink | `00 00 03 10 0001ffff 1fffffff 00001fff 00 0000 0000 10 none none` |
| gnome-network-displays default | `01 01 00000081 00000000 00000000 00 0000 0000 00 none none` (native `7<<3`) |
| AOSP source, M3 response shape | `%02x 00 %02x %02x %08x %08x %08x 00 0000 0000 00 none none` |
| Windows-as-sink, observed | `40 00 01 10 0001bdeb 051557ff 00000fff 10 0000 001f 11 0780 0438` |
| [MS-WFDPE] §3 example | `00 00 01 01 00000001 00000000 00000000 00 0000 0000 00 none none` |
| [MS-WFDPE] §3 `wfdx_` example | `0040 00 0001 0001 0000500001 0010000000 00000000 00 0000 0000 11 none none` |

Note `02 10` in lazycast = profile bit 1 (**CHP only**) + level bit 4 (4.2), while MiracleCast's
`03 10` = CBP|CHP + level 4.2, and AOSP's default is `01 01` = CBP + level 3.1. Advertising CHP
without CBP (lazycast) is a real, working configuration — but advertising **both** is strictly
safer for a sink, since a source that only does CBP will otherwise find no common profile.

#### Selection algorithm (what the source will do)

AOSP `VideoFormats::PickBestFormat` intersects the sink's and source's enabled bits across all
three tables and picks the highest `width * height * fps * (interlaced ? 1 : 2)`; profile and
level are then `min(source, sink)` by enum ordinal. Notably the **native/preferred resolution is
ignored** — AOSP `#if 0`s that code with the comment:

> `// Support for the native format is a great idea, the spec includes these features, but nobody supports it and the tests don't validate it.`

**Practical consequence for us:** a sink cannot steer the negotiation with `native` or
`wfd_preferred_display_mode`. The only reliable lever is *which bits you set*. To force
1920×1080 p60 out of an Android source, advertise CEA bit 8 and nothing higher-scoring.

---

### 3.3 `wfd_audio_codecs`

```
wfd-audio-codecs = "wfd_audio_codecs:" SP sink-audio-list CRLF
sink-audio-list  = "none" / ( audio-format SP modes SP latency *( ", " audio-format SP modes SP latency ) )
audio-format     = "LPCM" / "AAC" / "AC3"
modes            = 8*8HEXDIG
latency          = 2*2HEXDIG          ; units of 5 ms
```

AOSP documents the same shape in a comment above `GetAudioModes`:
`// sink_audio_list := ("LPCM"|"AAC"|"AC3" HEXDIGIT*8 HEXDIGIT*2) (", " sink_audio_list)*`
(`WifiDisplaySource.cpp:769`). The latency unit (×5 ms) is from
gnome-network-displays `wfd-audio-codec.c:114`.

**Mode bitmaps** — Miracast v2.3 Tables 43 (LPCM), 44 (AAC), 45 (AC3). All modes are 16-bit
nominal decoder output width.

| Codec | Bit | Sample rate | Channels | Codec option |
|-------|-----|-------------|----------|--------------|
| LPCM | 0 | 44.1 kHz | 2 | |
| LPCM | 1 | **48 kHz** | **2** | **mandatory — see below** |
| LPCM | 31:2 | *Reserved* | | |
| AAC | 0 | 48 kHz | 2 | AAC-LC |
| AAC | 1 | 48 kHz | 4 | AAC-LC |
| AAC | 2 | 48 kHz | 6 | AAC-LC |
| AAC | 3 | 48 kHz | 8 | AAC-LC |
| AAC | 31:4 | *Reserved* | | |
| AC3 | 0 | 48 kHz | 2 | Dolby Digital |
| AC3 | 1 | 48 kHz | 4 | Dolby Digital |
| AC3 | 2 | 48 kHz | 6 | Dolby Digital |
| AC3 | 31:3 | *Reserved* | | |

> **LPCM bit 1 is mandatory.** Miracast v2.3 §5.1.7.1: *"B1 of the LPCM Modes bitmap shall be set
> to one for all WFD devices to indicate support of 2-channel LPCM audio at 16 bits/channel at
> 48000 samples/second **as a mandatory mode of operation** (except for a Primary Sink that does
> not have audio rendering capability, e.g., typical office projector). Other LPCM audio formats
> are optional at all WFD devices."*
>
> So a sink with speakers **must** advertise at least `LPCM 00000002 00`. This is not merely
> defensive interop advice; it is a conformance requirement, and it is why sources are entitled to
> assume LPCM always works.

Cross-check from implementation: AOSP tests `supportsAAC = (modes & 1)` with the comment
`// AAC 2ch 48kHz` and `supportsPCM = (modes & 2)` with `// LPCM 2ch 48kHz`
(`WifiDisplaySource.cpp:908-912`), and emits `LPCM 00000002 00` / `AAC 00000001 00` in M4
(`:646-647`) — exactly matching the spec tables.

Spec note on AAC bit 3: *"In ISO/IEC standard, down-mix method is not defined for 8-ch (7.1ch),
and it is recommended that the WFD Sink that does not support 8-ch (7.1ch) natively should not set
this bit to one."*

Real values seen:

* `AAC 00000001 00` — AOSP M4, lazycast.
* `LPCM 00000002 00` — AOSP M4 when `media.wfd.use-pcm-audio` is set.
* `AAC 00000007 00` — MiracleCast sink (claims 2/4/6-channel AAC).
* `LPCM 00000003 00, AAC 0000000f 00, AC3 00000007 00` — Windows as sink.
* `LPCM 00000003 00, AAC 00000001 00, AC3 00000000 00` — [MS-WFDPE] §3 example. Note the
  `AC3 00000000` entry: **listing a codec with an all-zero mode mask means "I know this codec but
  support no mode of it"**, which is not the same as omitting it. Parse it, don't crash on it.

**Sink guidance.** Advertise `LPCM 00000002 00, AAC 00000001 00` at minimum. AOSP's selection
order is: PCM if the `media.wfd.use-pcm-audio` system property is set and supported, else AAC if
supported, else PCM, else fail. So AAC is the default path on Android and LPCM is the default on
some Windows configurations. **A sink that advertises only AAC will silently get no audio from a
source configured for LPCM** — support both.

---

### 3.4 `wfd_client_rtp_ports`

```
wfd-client-rtp-ports = "wfd_client_rtp_ports:" SP rtp-port0 SP rtp-port1 SP mode CRLF
                       ; where the profile token precedes port0
```
concretely, the value is one of:

```
RTP/AVP/UDP;unicast <port0> <port1> mode=play
RTP/AVP/TCP;unicast <port0> <port1> mode=play
RTP/AVP/TCP;interleaved mode=play
```

Exactly these three forms are what AOSP accepts (`WifiDisplaySource.cpp:823-842`). Constraints
AOSP *enforces*:

* `port0` must be non-zero and ≤ 65535.
* **`port1` must be 0.** AOSP rejects the message with "Sink chose its wfd_client_rtp_ports poorly"
  if `port1 != 0`.

That last one is the trap: `port1` is nominally the RTCP port, but the WFD profile as implemented
requires it to be `0`. gnome-network-displays (acting as a *source*) has explicit compensating
logic — if `secondary_rtp_port == 0` it warns and auto-corrects to `primary + 1`
(`src/wfd/wfd-params.c:229-240`), i.e. it treats the spec-mandated `0` as a bug in the sink. That
divergence is worth knowing about but **a sink must emit `0`** or Android drops the session.

Working sink values: `RTP/AVP/UDP;unicast 1028 0 mode=play` (lazycast), and MiracleCast builds the
same string from its chosen port. Windows-observed: `RTP/AVP/UDP;unicast 19000 0 mode=play`.

The port here is the **UDP port the sink will receive RTP on**, and it is echoed in M6's
`Transport: …;client_port=`. Keep them consistent; some sources cross-check.

---

### 3.5 `wfd_presentation_URL`

```
wfd-presentation-URL = "wfd_presentation_URL:" SP presentation-url0 SP presentation-url1 CRLF
presentation-url0    = "rtsp://" host [ ":" port ] "/wfd1.0/streamid=0" / "none"
presentation-url1    = <same shape, for the secondary sink in a coupled pair> / "none"
```

AOSP emits `wfd_presentation_URL: rtsp://%s/wfd1.0/streamid=0 none` where `%s` is the source's
local IP (`WifiDisplaySource.cpp:652`). Sent by the source in **M4**. The sink must use `url0`
verbatim as the request-URI for M6/M7/M8/M9.

---

### 3.6 `wfd_trigger_method`

```
wfd-trigger-method = "wfd_trigger_method:" SP ( "SETUP" / "PAUSE" / "TEARDOWN" / "PLAY" ) CRLF
```
Source → sink only (M5). AOSP's `sendTrigger` emits exactly these four tokens. On receipt the sink
must issue the matching RTSP request (M6/M9/M8/M7) — the 200 OK to the M5 is **not** the action.

---

### 3.7 `wfd_content_protection`

```
wfd-content-protection = "wfd_content_protection:" SP ( "none" / hdcp-spec SP "port=" port ) CRLF
hdcp-spec              = "HDCP2.0" / "HDCP2.1"
```

AOSP accepts only `none`, `HDCP2.0 port=<n>`, `HDCP2.1 port=<n>`; anything else is
`ERROR_MALFORMED` (`WifiDisplaySource.cpp:940-961`).

Three corrections to the folklore:

* **There is no default port, and it is not 4444.** Miracast v2.3 §6.1.5 constrains it only to
  1–65535, and the **sink chooses and advertises it**. (The spec text contains no occurrence of
  4444.) Observed real values: `1189`, `53002`.
* **The space after the version token is load-bearing.** AOSP parses the port with
  `ParsedMessage::GetInt32Attribute(value.c_str() + 8, "port", ...)` — a hard-coded skip of
  exactly `"HDCP2.x "`. Emit `HDCP2.1 port=1189`, never `HDCP2.1port=1189`.
* **The grammar tops out at `HDCP2.1` in every WFD version through 2.1.** HDCP 2.2/2.3 exist but
  have no wire representation here; a 2.2-capable sink still says `HDCP2.1` and the real revision
  is settled inside the HDCP handshake. Miracast v2.3 §6.1.5: *"If the WFD Sink supports HDCP 2.0,
  2.1 and higher version, the parameter is set to 'HDCP 2.1'."* `HDCP2.0`-only is now prohibited
  ("transition period has been expired").

For this parameter the **sink is the TCP server** — the opposite of RTSP and UIBC. See §6.

**Both open-source sinks answer `none`** (MiracleCast `ctl-sink.c:90`, lazycast `d2.py:240`), as
does Windows' own sink, and both Windows and Android proceed with an unencrypted stream. See §6
for why this is the right call for us.

---

### 3.8 `wfd_uibc_capability` / `wfd_uibc_setting`

```
wfd-uibc-capability = "wfd_uibc_capability:" SP ( "none" /
                        "input_category_list=" input-cat-list ";"
                        "generic_cap_list="    generic-cap-list ";"
                        "hidc_cap_list="       hidc-cap-list ";"
                        "port=" ( "none" / port ) ) CRLF

input-cat-list   = "none" / ( ( "GENERIC" / "HIDC" ) *( ", " ( "GENERIC" / "HIDC" ) ) )
generic-cap-list = "none" / ( generic-cap *( ", " generic-cap ) )
generic-cap      = "Keyboard" / "Mouse" / "SingleTouch" / "MultiTouch" /
                   "Joystick" / "Camera" / "Gesture" / "RemoteControl"
hidc-cap-list    = "none" / ( hidc-cap *( ", " hidc-cap ) )
hidc-cap         = generic-cap "/" input-path
input-path       = "Infrared" / "USB" / "BT" / "Zigbee" / "Wi-Fi" / "No-SP"

wfd-uibc-setting = "wfd_uibc_setting:" SP ( "enable" / "disable" ) CRLF
```

Real working values:

```
wfd_uibc_capability: input_category_list=GENERIC, HIDC;generic_cap_list=Keyboard, Mouse;hidc_cap_list=Keyboard/USB, Mouse/USB;port=none
```
(lazycast `d2.py:238` — the sink sends `port=none` in M3 and the **source** fills in the real port
in its M4/M14; lazycast then reads `port=` out of the source's message and connects to it.)

```
wfd_uibc_capability: input_category_list=GENERIC;generic_cap_list=Mouse,SingleTouch;hidc_cap_list=none;port=none
```
(MiracleCast `ctl-sink.c:143-146` — note it omits the space after the comma in
`generic_cap_list`; both forms are accepted in practice, so **parse leniently on whitespace**.)

See §5 for the wire format on the UIBC socket itself.

---

### 3.9 `wfd_display_edid`

```
wfd-display-edid = "wfd_display_edid:" SP ( "none" / edid-block-count SP edid-payload ) CRLF
edid-block-count = 4*4HEXDIG      ; number of 128-byte EDID blocks
edid-payload     = 256*HEXDIG     ; edid-block-count * 128 bytes, lowercase hex, no separators
```

lazycast emits `'{:04X}'.format(int(edidlen/256 + 1))` followed by `edidbytes.hex()`
(`d2.py:248`). gnome-network-displays validates `128 * 2 * length == strlen(hex)`
(`wfd-params.c:346`).

> **Interop hazard:** gnome-network-displays parses the block count with
> `g_ascii_strtoll(value, NULL, 10)` — **base 10, on a hex field**. Counts ≥ 10 blocks parse
> wrong and counts of `000A`+ become 0. Keep the count small (1–2 blocks is normal) and you will
> not trip it.

Sending real EDID is how you tell the source about the panel's true native timing. It is optional
and most sinks answer `none`; the Dell C6522QT's EDID is worth passing through once we have it,
since it is the only channel that communicates the panel's actual capabilities.

---

### 3.10 `wfd_connector_type`

```
wfd-connector-type = "wfd_connector_type:" SP connector-type CRLF
connector-type     = 2*2HEXDIG
```

Miracast v2.3 **Table 91**, complete:

| Value | Connector | Value | Connector |
|-------|-----------|-------|-----------|
| 0 | VGA | 7 | **Miracast** |
| 1 | S-Video | 8 | Japanese D (EIAJ RC-5237) |
| 2 | Composite video | 9 | SDI |
| 3 | Component video | 10 | DisplayPort |
| 4 | DVI | 11 | Reserved |
| 5 | **HDMI** | 12 | UDI |
| 6 | Reserved | 13–254 | Reserved |
| | | 255 | a physical connector not listed above |

`05` = HDMI is what lazycast emits (`d2.py:237`) and is the right answer for an HDMI-attached
panel like the C6522QT.

> **Avoid 255.** Spec note 1: *"If the value 255 as connector-type is reported from the WFD Sink,
> WFD Sources may not be able to unambiguously identify the connector type that is in use. Due to
> this reason, **some WFD Sources may not be able to recognize the WFD Sink at all** or may
> interoperate in a sub-optimal manner."* Pick the closest real connector instead.

M11 lets a source change the active connector at runtime; a single-output sink should 200-OK it
and ignore it. Per the spec the sink may also *volunteer* a `SET_PARAMETER` with the new
connector-type after a change — but only if the source indicated M11 support.

---

### 3.11 The rest of the parameter set

| Parameter | Direction | Grammar / value | Notes |
|-----------|-----------|-----------------|-------|
| `wfd_3d_video_formats` | sink caps | `none` / `<native> <pref-mode> <profile> <level> <3d-cap-bitmap(16 hex)> <latency> <min-slice> <slice-enc> <frame-rate-ctl> <max-hres> <max-vres>` | Stereoscopic modes. Every real sink answers `none`. |
| `wfd_coupled_sink` | both | `none` / `<coupled-sink-status(2 hex)> <sink-MAC or "none">` | Splitting audio to one sink and video to another. Answer `none`. |
| `wfd_I2C` | sink caps | `none` / `<i2c-port(4 hex)>` | DDC/CI pass-through port. Answer `none`. |
| `wfd_standby_resume_capability` | both | `none` / `supported` | Whether M12 standby is understood. Answer `none` unless implemented. |
| `wfd_standby` | S→K (M12) | *(no value — bare parameter name)* | Source is entering standby. Sink should blank/idle but keep the RTSP session. |
| `wfd_idr_request_capability` | sink caps | `"0"` / `"1"` | [MS-WFDPE] §2.6.1.1. Answer `1` if you will send M13. |
| `wfd_idr_request` | K→S (M13) | *(no value — bare parameter name)* | Must be followed by CRLF; AOSP substring-matches `"wfd_idr_request\r\n"`. |
| `wfd_route` | S→K (M10) | `primary` / `secondary` | Which sink renders audio. |
| `wfd_preferred_display_mode` | S→K | `none` / `<p-clock> <H> <HB> <HSPOL-HSOFF> <HSW> <V> <VB> <VSPOL-VSOFF> <VSW> <VBS3D> <R> <F> <flags> <mode-type> <profile> <level>` (all hex) | Full explicit CVT/CEA timing. Requires `preferred-display-mode-supported=01` in `wfd_video_formats`. **Effectively dead** — AOSP ignores it (see the `#if 0` above). |
| `wfd_av_format_change_timing` | S→K | `<PTS(10 hex)> SP <DTS(10 hex)>` | Sent *before* the source switches format mid-stream; gives the 90 kHz PTS/DTS of the first sample in the new format. A sink that supports mid-stream format change must latch this and re-init the decoder at that PTS. |
| `wfd2_*` (R2) | both | `wfd2_audio_codecs`, `wfd2_video_codecs`, `wfd2_aux_stream_formats`, `wfd2_buffer_length`, `wfd2_audio_playback_status`, `wfd2_video_playback_status`, `wfd2_cta_datablock_collection` | Miracast R2 (WFD 2.x). List from `benzea/miracast` `rtsp.py` `m3_optional`. Not required; a source that asks and gets no answer falls back to R1. |

### 3.12 Vendor extension parameters (you will be asked for these)

**Microsoft** — all specified in [MS-WFDPE] (publicly downloadable PDF:
<https://winprotocoldocs-bhdugrdyduf5h2e4.b02.azurefd.net/MS-WFDPE/%5bMS-WFDPE%5d.pdf>):

| Parameter | Value grammar | Meaning |
|-----------|---------------|---------|
| `microsoft_diagnostics_capability` | `supported` / `none` | Sink will include `microsoft_teardown_reason` in its M8. |
| `microsoft_teardown_reason` | `<8 HEXDIG HRESULT> SP <free text>` | e.g. `C00D4278 No RTP data was provided for 2 minutes`. Predefined: `C00D36F0` MF_E_CANNOT_PARSE_BYTESTREAM, `C00D3E8C` MF_E_INVALID_FORMAT. |
| `microsoft_format_change_capability` | `supported` / `none` | Sink tracks SPS/PPS changes and re-inits the decoder without flicker; source guarantees an IDR + new SPS/PPS at the switch. **Answer `supported` only if the render path can genuinely resize without a black frame.** |
| `microsoft_latency_management_capability` | caps: `supported`/`none`; setting: `low`/`normal`/`high` | Source can later `SET_PARAMETER` a mode. `low` = sink SHOULD keep latency under 50 ms. |
| `microsoft_rtcp_capability` | `supported` / `none` | If `supported`, the source's SETUP response carries **two** `server_port` values and the sink sends RTCP RRs to the second. This is the only sanctioned way to get RTCP into a WFD session. |
| `microsoft_color_space_conversion` | `supported` / `none` | 4:4:4 recovery across 4 repeat frames. Frame index is signalled by the **number of PES header stuffing bytes minus 1** (1–4 stuffing bytes ⇒ counter 0–3; 0 or >4 ⇒ no counter). Clever and genuinely useful for text legibility. |
| `microsoft_max_bitrate` | `1*10DIGIT` (bits/s) | Source MUST encode at or below. |
| `microsoft_multiscreen_projection` | `supported`/`none`, or `primary` / `secondary <hres> <vres> <bitrate>` | Sink can demote a source to a window. |
| `microsoft_audio_mute` | `supported`/`none`, or `1`/`0` | Sink can tell the source to stop sending audio entirely. |
| `microsoft_video_formats` | `12HEXDIG` bitmap | Surface-shaped 3:2 modes: 0–2 = 1920×1280 p30/60/24, 3–5 = 2160×1440, 6–8 = 2256×1504, 9–11 = 2736×1824, 12–14 = 3000×2000, 15–17 = 3240×2160, 18–20 = 4500×3000. |
| `wfdx_video_formats` | see §3.2 | 4-hex `native`, 4-hex `profile`, 4-hex `level`, 10-hex CEA, 10-hex VESA. **If a sink sends both `wfd_video_formats` and `wfdx_video_formats` in M3, the source MUST ignore `wfd_video_formats`.** M4 must contain only one of them. |
| `microsoft_cursor` | `none` / `<none\|full> <width 4hex> <height 4hex> <port 4hex>` | **Not in [MS-WFDPE].** Reverse-engineered by gnome-network-displays (`wfd-params.c:363-390`): a separate TCP port on which the source pushes hardware-cursor bitmaps so the sink can composite a low-latency pointer. `full` means XOR-blend support is claimed. |

**Intel** — also documented in [MS-WFDPE] §2.1 (they predate it and Windows queries them):
`intel_friendly_name`, `intel_sink_manufacturer_name`, `intel_sink_model_name`,
`intel_sink_device_URL`, `intel_sink_version` (value shape:
`product_ID=G4716-2000 hw_version=1.1.5.1345 sw_version=1.2.4.2451`),
`intel_sink_manufacturer_logo`. lazycast answers all of these
(`d2.py:255-264`) and it is cheap insurance — answer them with our device identity.

**Rule for unknown parameters:** if an M3 request names a parameter you do not implement,
**omit it from the response** rather than answering `none` blindly, *except* where `none` is the
documented "not supported" value. Neither AOSP nor Windows errors on a missing parameter; both do
error on a malformed value. MiracleCast implements this as `check_and_response_option()` — only
emit a parameter if the source asked for it.
## 4. Media transport

Everything in this section was read out of AOSP's actual muxer,
[`TSPacketizer.cpp`](https://android.googlesource.com/platform/frameworks/av/+/android-7.1.2_r36/media/libstagefright/wifi-display/source/TSPacketizer.cpp)
and [`rtp/RTPSender.cpp`](https://android.googlesource.com/platform/frameworks/av/+/android-7.1.2_r36/media/libstagefright/wifi-display/rtp/RTPSender.cpp),
so it describes what an Android source really emits — which is the thing our depacketiser has to
survive.

### 4.1 The stack

```
UDP (unicast, to the sink's wfd_client_rtp_ports port0)
 └─ RTP  (RFC 3550, payload type 33 = MP2T, 90 kHz clock)
     └─ 7 × MPEG-2 TS packets (188 B each) = 1316 B RTP payload
         └─ PES
             └─ H.264 Annex-B  /  ADTS AAC  /  LPCM  /  AC-3
```

### 4.2 RTP

* **Payload type 33** (`MP2T`), from the RFC 3551 static table. Clock rate **90 kHz**.
* 12-byte fixed header, no extension, no CSRCs, in the AOSP sender. Do not assume that of every
  source — parse the CC field and the X bit properly.
* **The marker bit means different things to different senders. Never gate discontinuity handling
  on it.**
  * RFC 2250 and the Miracast spec define `M = 1` as *"timestamp is discontinuous"*.
  * AOSP never sets it.
  * **Windows repurposes it as an end-of-frame flag.** Microsoft's receiver guidance: *"Microsoft
    Miracast source repurposes the M-Bit of the RTP packet header to denote the end of the frame
    in the RTP packet. A Miracast receiver looking for the M-bit can save time by starting to
    decode the frame … instead of waiting for the next RTP packet."*

  Model it as an advisory `MarkerBit::EndOfFrameHint` — useful for shaving one datagram of latency
  off the decode start, never authoritative. This also explains why short RTP datagrams are
  routine: a muxer that flushes at frame boundaries emits a runt at every frame end.
* **7 TS packets per RTP packet.** This is not a spec constant, it falls out of the MTU:
  AOSP defines `kMaxUDPPacketSize = 1472` ("Really UDP _payload_ size", `rtp/RTPBase.h`) and
  `kMaxNumTSPacketsPerRTPPacket = (kMaxUDPPacketSize - 12) / 188` = **7**
  (`rtp/RTPSender.h:70`). Resulting datagram: 12 + 7×188 = **1328 bytes**, RTP payload 1316.
  A sink **must not hard-code 7** — the final RTP packet of a burst carries the remainder
  (`numTSPackets = (size - srcOffset)/188`, clamped), so 1..7 packets are all legal. Validate
  `payload_len % 188 == 0` and iterate.
* AOSP asserts `tsPackets->size() % 188 == 0` before sending (`RTPSender.cpp:250`) — the TS stream
  is always packet-aligned within an RTP payload, never split mid-TS-packet. That is what makes a
  stateless depacketiser possible.
* **RTCP**: not used in baseline WFD. `wfd_client_rtp_ports` requires `port1 = 0` (§3.4) and the
  SETUP response carries a single `server_port`. RTCP only appears if the sink advertised
  `microsoft_rtcp_capability: supported`, in which case the source sends two `server_port` values
  and the sink sends RRs to the second ([MS-WFDPE] §2.8.1.1). **Worth advertising** — it is the
  only sanctioned back-pressure signal, and Windows will drop its bitrate in response.
* Sequence numbers are conventional. There is **no retransmission and no FEC**. Loss is handled by
  (a) tolerating it in the TS/PES layer and (b) sending M13 `wfd_idr_request`.
* **AOSP hard-codes `SSRC = 0xdeadbeef`** (`kSourceID` in `RTPSender.cpp`). A reliable "this source
  is AOSP-derived" fingerprint, and a reminder not to treat SSRC as identifying anything.

> ### ⚠ The AOSP RTP timestamp is wall-clock, not media time
>
> ```cpp
> rtpTime = (nowUs * 9) / 100;   // 90 kHz, but from ALooper::GetNowUs()
> ```
>
> It is a 90 kHz *wall-clock* value sampled at send time, **not derived from the media PTS**. It
> will therefore drift against the PCR and against the PES PTS values in the very same packets.
>
> **Do not use RTP timestamps for A/V sync or presentation timing.** Use the PES PTS, with PCR only
> as a coarse drift check (§4.3). RTP sequence numbers are still good for ordering and loss
> detection. This is the kind of thing that produces slow, mysterious lip-sync drift over minutes.
* Per ground rule 4: for live mirroring, **drop late frames**. A jitter buffer deeper than ~1
  frame is the wrong trade here.

### 4.3 MPEG-2 Transport Stream

**PIDs actually used by Android** (`TSPacketizer.h:69-70`, `TSPacketizer.cpp:391-393`):

| PID | Contents |
|-----|----------|
| `0x0000` | PAT |
| `0x0100` | PMT (`kPID_PMT`) |
| `0x1000` | PCR-only packets (`kPID_PCR`) — adaptation field only, **no payload** |
| `0x1011` | first video ES (increments per additional video track) |
| `0x1100` | first audio ES |

**Android and Windows both use exactly these values.** lazycast hard-codes `pid == 0x1011` /
`pid == 0x1100` with no PAT/PMT parsing at all (`h264/h264.c:512,552`) and works in production
against Windows 8.1/10 — which settles the question of what Windows emits without needing a
capture.

**But parse PAT → PMT → ES PIDs properly anyway.** A GStreamer-based source does *not* use the
canonical values: `mpegtsmux` defaults to `TSMUX_START_PMT_PID 0x0020` and
`TSMUX_START_ES_PID 0x0040` (`gst/mpegtsmux/tsmux/tsmux.h:79-80`), and gnome-network-displays does
not override them. Use the canonical PIDs as a fast path and sanity check, never as the only path.
(Corollary: gnome-network-displays is a poor fixture generator for the TS layer — its PIDs will not
match any real device. Use it for RTP/PES *shape* only.)

**Stream types** — Miracast v2.3 **Table 105**, the normative MPEG2-TS parameter table:

| AV format | PES `stream_id` | `stream_type` | PMT descriptor tag |
|-----------|-----------------|---------------|--------------------|
| **H.264 video** | `0xE0`–`0xEF` | **`0x1B`** | `0x28` AVC descriptor |
| H.265 / HEVC video (R2) | `0xE0`–`0xEF` | `0x24`, `0x25` | `0x38` HEVC video descriptor |
| **LPCM** | `0xBD` | **`0x83`** | `0x83` LPCM audio stream descriptor |
| **AAC** | `0xC0`–`0xDF` | **`0x0F`** | `0x2B` MPEG-2 AAC audio descriptor |
| Dolby AC-3 | `0xBD` | `0x81` | `0x81` AC-3 audio descriptor |
| E-AC-3 | `0xBD` | `0x87` | `0xCC` |
| MPEG-4 AAC / AAC-ELDv2 | `0xC0`–`0xDF` | `0x11` | `0x1C` |
| MPEG-H 3D Audio | `0xC0`–`0xDF` | `0x2D` | `0x1C` |
| AC-4 | `0xBD` | `0x06` (private, identify by descriptor) | `0x15` |
| DTS-HD | `0xBD` | `0x06` (private, identify by descriptor) | `0x7B` |
| JPEG (R2 still image) | `0xBD` | `0x92` | `0x92` |
| PNG (R2 still image) | `0xBD` | `0x93` | `0x93` |

The four bolded rows are all AOSP implements (`TSPacketizer.cpp:402-416`) and all a baseline R1
sink needs. Note `0x06` is used for both AC-4 and DTS-HD — for those you **must** dispatch on the
PMT descriptor tag, not the stream type.

**Descriptors.** AOSP emits, in the PMT ES loop (`TSPacketizer.cpp:257-337`):

* For H.264: **AVC video descriptor**, `descriptor_tag = 40`, length 4 — carries
  `profile_idc`, the constraint_set flags, `level_idc`.
* For H.264: **AVC timing and HRD descriptor**, `descriptor_tag = 42`, length 2.
* For LPCM: **LPCM audio stream descriptor**, `descriptor_tag = 0x83`, length 2 — carries the
  sampling frequency / bits-per-sample / channel configuration.

And in the PMT **program_info** loop, when content protection is on
(`TSPacketizer.cpp:364-375`), the **HDCP descriptor**:

```
05 05 'H' 'D' 'C' 'P' 20
^  ^  \___________/  ^
|  |    format_id    hdcp version
|  descriptor_length = 5
descriptor_tag = 0x05 (registration_descriptor)
```

with this comment in the source, which is the kind of thing you only learn from an implementation:

> `// HDCP2.0 _and_ HDCP 2.1 specs say to set the version inside the HDCP descriptor to 0x20!!!`

So **the descriptor says `0x20` even for HDCP 2.1**. If we ever parse it, do not use it to
distinguish versions — use `wfd_content_protection` from M4.

**PCR.** AOSP emits PCR on its own PID `0x1000` in packets that are *pure adaptation field*:
`adaptation_field_control = b10`, `adaptation_field_length = 183` (`0xb7`), `PCR_flag = 1`, rest
stuffed with `0xff`, and **the continuity counter does not increment** (correct — CC only
increments on packets carrying payload). The clock is 27 MHz:

```
PCR      = now_us * 27
PCR_base = PCR / 300        (33 bits, the 90 kHz clock)
PCR_ext  = PCR % 300        (9 bits)
```

A sink should use PCR as the master clock reference but must tolerate its absence and its jumps —
`discontinuity_indicator` is available and Android never sets it, so **PCR discontinuities arrive
unannounced**. Practical approach: slave to the audio PTS for A/V sync, use PCR only to detect
gross drift, and re-base on any jump larger than a few hundred ms.

### 4.4 PES

AOSP's static PES header for a WFD access unit is 14 bytes and is documented inline
(`TSPacketizer.cpp:487-511`):

```
00 00 01              packet_start_code_prefix
<stream_id>           0xE0 video / 0xC0 audio / 0xBD private
<len_hi> <len_lo>     PES_packet_length
0x84                  '10' reserved | scrambling 00 | priority 0 |
                      data_alignment_indicator = 1 | copyright 0 | original 0
0x80                  PTS_DTS_flags = b10 (PTS only), all other flags 0
0x05                  PES_header_data_length
<5 bytes>             PTS, '0010' prefix + 3 marker bits
```

Points that matter for the sink:

* **PTS only, never DTS.** `PTS_DTS_flags = b10`. Since B-slices are forbidden (§3.2), DTS ≡ PTS
  and the decoder needs no reorder buffer. This is a real simplification — size the decode path
  accordingly.
* `data_alignment_indicator = 1` — each PES packet starts at an access-unit boundary.
* `PTS = timeUs * 9 / 100`, i.e. the 90 kHz clock (`TSPacketizer.cpp:874`). 33 bits with the usual
  marker-bit interleave.
* **`PES_packet_length` is set to a real value**, not 0, unless it would overflow:
  `if (PES_packet_length >= 65536) { CHECK(track->isVideo()); PES_packet_length = 0; }`
  (`:875-881`). So a sink sees *both* forms from Android depending on frame size: small frames
  carry a length, large I-frames carry 0. **The demuxer must handle `PES_packet_length == 0`
  (unbounded, terminated by the next PUSI) for video.** This is a common source of bugs.
* Stuffing bytes in the PES header are *meaningful* if `microsoft_color_space_conversion` is
  negotiated — the count minus one is the 4:4:4 recovery frame index (§3.12). Otherwise ignore
  them.
* `PES_private_data` may be present (AOSP plumbs it for HDCP; see §6).

### 4.5 H.264 constraints

From [MS-WFDPE] §2.7.1.1.1 and AOSP's encoder setup:

* Profiles: **Constrained Baseline (66, constraint_set 0xC0)** or **Constrained High (100,
  constraint_set 0x0C)** only.
* **No B-slices** in any profile — so DTS ≡ PTS and the decoder needs no reorder buffer.
* **CABAC: assume yes.** The R1 spec forbids it, but R2's RHP2 profile re-enables it and Windows
  advertises CABAC support for both Baseline and High. See the box in §3.2. Use a general decoder.
* Levels 3.1–4.2 in R1; 5.0–5.2 in the extension.
* **Annex-B byte stream** inside PES. The muxer prepends SPS+PPS to every IDR when
  `PREPEND_SPS_PPS_TO_IDR_FRAMES` is set (`TSPacketizer.cpp:474-478`) — which the WFD source path
  does. So the sink gets in-band parameter sets at every IDR and never needs out-of-band CSD.
  **Do not assume it, though**: recover by sending M13 if you see slices before any SPS.
* Slices: one slice per picture unless the sink advertised a non-zero `min-slice-size` (§3.2).
* When HDCP is active, every TS packet except the last of a PES payload carries a **multiple of 16
  bytes** of payload (`alignPayload` in `TSPacketizer.cpp:520-522` and `:894-896`) — an AES-CTR
  block-alignment requirement.

### 4.6 Audio

* **AAC**: `stream_type 0x0F`, ADTS-framed. AOSP prepends an ADTS header if the encoder did not
  (`prependADTSHeader`). 48 kHz, 2 ch, AAC-LC.
* **LPCM**: `stream_type 0x83`, `stream_id 0xBD`, with an LPCM audio stream descriptor in the PMT.
  48 kHz / 16-bit / 2 ch, **big-endian samples** (`S16BE`).

  The PES payload carries a **4-byte WFD LPCM header** before the samples. Layout, confirmed three
  ways — the spec (Table 106), AOSP `Converter.cpp:556-566`, and Samsung's Tizen `wfdpesparse.c`:

  | Byte | Field |
  |------|-------|
  | 0 | `sub_stream_ID` — always `0xA0` |
  | 1 | `number_of_frame_header` — always `0x06` |
  | 2 | `audio_emphasis` |
  | 3 | bits 7:6 bits-per-sample (`((b >> 4) \| 0x10)` → 16/20/24) · bits 5:3 sample rate (`1` = 44.1 k, `2` = 48 k, `4` = 96 k) · bits 2:0 channels − 1 |

  `A0 06` is a reliable sniff for "this is a WFD LPCM PES". Skip 4 bytes, then samples. lazycast
  skips exactly 20 bytes from the PES start (14 static header + 2 stuffing + 4 LPCM) and feeds
  `SND_PCM_FORMAT_S16_BE` with no byte swap — production-proven against Windows.

  > **A stock TS demuxer will get this wrong.** `0x83` collides: in Blu-ray/ATSC it means AC-3
  > TrueHD, and generic LPCM-in-TS is `0x8B`. Samsung forked GStreamer's `tsdemux` into
  > `wfdtsdemux` specifically for this, with `#define ST_PS_WFD_AUDIO_LPCM 0x83` guarded by
  > `#ifdef WFD_SPEC` sitting directly alongside `#define ST_BD_AUDIO_AC3_TRUE_HD 0x83`. Plan for a
  > WFD-specific demux path.
* **AC-3**: `stream_type 0x81`, `stream_id 0xBD`, AC-3 audio descriptor tag `0x81`. Windows
  advertises it as a sink; whether any source actually sends it is unverified, and AOSP cannot.
  Low priority.
* Audio latency is declared in `wfd_audio_codecs` in units of 5 ms (§3.3).

### 4.7 A/V sync and format change

* Both streams share the 90 kHz PTS base and the PCR. In practice: buffer audio to the video's
  presentation cadence, and if you must choose, keep audio continuous and drop video.
* Mid-stream resolution/frame-rate changes come in two flavours:
  1. **`wfd_av_format_change_timing`** (§3.11) — the source announces the PTS/DTS at which the new
     format begins, *before* it switches. Latch it, and re-init the decoder exactly at that PTS.
  2. **`microsoft_format_change_capability: supported`** — no RTSP announcement at all; the sink
     is expected to notice a new SPS/PPS in-band and reconfigure without a black frame. The source
     guarantees an IDR + new SPS/PPS at the switch ([MS-WFDPE] §2.3.1.1). Only advertise this if
     the wgpu/render path can genuinely resize without a visible glitch — otherwise Windows will
     change resolution on you and the user sees a flash.
  **Treat SPS/PPS-change detection as a first-class reconfiguration trigger**, parallel to
  `wfd_av_format_change_timing` — Windows uses only the in-band path, so a sink that only watches
  for the RTSP parameter will silently render garbage after a resolution change.

#### The numbers to design against

| Figure | Value | Source |
|--------|-------|--------|
| **End-to-end latency** | **≤ 250 ms** at max resolution/frame rate | Wi-Fi Alliance **certification requirement** |
| **Video frame drop** | **< 1.5 %** | ibid. |
| **A/V sync window** | audio may **lead by ≤ 45 ms**, **lag by ≤ 125 ms** | ibid. |
| `microsoft_latency_management_capability` | `low` < 50 ms · `normal` < 100 ms · `high` < 500 ms | [MS-WFDPE] §2.4.1.1 |
| Cursor latency | > 100 ms in-stream vs **< 30 ms** via the out-of-band `microsoft_cursor` channel | MS receiver requirements |
| Miracast-over-Infrastructure, wired | 100–166 ms, ~0 % loss | Mersive tech note |
| …over poor Wi-Fi | "multiple seconds", up to 10 % loss | ibid. |

The ±45/125 ms sync window is the concrete acceptance criterion for the presentation logic, and
≤ 250 ms end-to-end is what real senders are built against.

#### Everyone else buffers too much — don't copy them

| Sink | Buffering |
|------|-----------|
| intel/wds reference sink | **none** (`udpsrc ! rtpmp2tdepay ! decodebin`) |
| Tizen `wfdrtpbuffer` | 200 ms, `drop-on-latency=FALSE` |
| Tizen `wfdsrc` bin | **2000 ms** |
| GStreamer `tsdemux` | `latency` default **700 ms**, on top of the above |

**`drop-on-latency` is FALSE in every off-the-shelf default**, so no commodity sink actually
implements ground rule 4's drop-late-frames policy — which is exactly why dongles feel laggy. A
latency-first sink wants ~0–50 ms of buffering and should lean on PTS rather than PCR. Upstream
`tsdemux` agrees in practice: it auto-disables PCR after 1000 ms of PCR-less data
(`IGNORE_PCR_THRESHOLD`), and 1.30 added `ignore-continuity-counter` for "streams [that] are
poorly generated and reset the continuity counter."
## 5. UIBC — User Input Back Channel

UIBC carries touch/keyboard/mouse events from the sink back to the source. It is what makes the
Dell C6522QT's touch panel useful: without it, Miracast is a one-way projector. This is the single
highest-value optional feature in the protocol for our deployment.

### 5.1 Negotiation

Three steps, all in RTSP (§3.8 has the grammar):

1. **M3 response** — the sink advertises `wfd_uibc_capability` with **`port=none`**. Spec, §6.1.15:
   *"The WFD Source indicates the TCP port number to be used for UIBC in the tcp-port field … in
   RTSP M4 and/or M14 request messages. **The WFD Sink uses "none" for the tcp-port field** … in
   RTSP M3 response and M14 request messages."* The sink is saying *what* it can send, not *where*.
2. **M4 or M14** — the **source** sends `wfd_uibc_capability` back with the intersection it accepts
   and a real `port=<n>` on the source. M14 is also the mid-session *update* message. Constraint:
   *"The WFD Sink shall not update any UIBC parameter(s) now in use, before the next exchange of
   RTSP M14 request and response messages is successfully completed."*
3. **M15** — `wfd_uibc_setting: enable` / `disable`. **Either side may send it**, so a sink can
   unilaterally disable touchback (e.g. a "lock input" button on the panel) without tearing down.

Support is *also* signalled out of band, before RTSP even starts: **bit 0 of the WFD Extended
Capability bitmap** (subelement ID 7) in the WFD IE. A sink that intends to offer UIBC should set
it — see §1.

Per-message obligations (Miracast Table 6-9): `wfd-uibc-capability` is optional in the M3 request,
**mandatory in the M3 response if the M3 request asked for it**, optional in M4, mandatory in M14.
`wfd-uibc-setting` is optional in M4 and mandatory in M15.

### 5.2 Transport

**TCP. The source listens; the sink connects out.** Spec §4.11.2: *"this port at the WFD Source
shall be ready to accept incoming connections from the WFD Sink before sending a subsequent RTSP
M4 and/or M14 request message… Once established, **a single TCP connection** between the WFD
Source and the WFD Sink shall be used for the **duration of the WFD Session** for all UIBC data
exchange."*

Confirmed by MiracleCast (`src/uibc/miracle-uibcctl.c:34-49`, `socket()` + `connect()`), lazycast
(`d2.py:311`), and the Freescale/Android source-side patch which logs *"Create listening uibc tcp
channel on port %d"*.

Note this is the **opposite** of HDCP, where the sink listens (§6). See the direction table in
§2.1.

**There is no default port.** Observed: `7239` (Freescale Android — `DEFAULT_UIBC_PORT = 7239`),
`1000` (the spec's own example figure). Always read `port=` from the source's message.

**Set `TCP_NODELAY`.** Microsoft's receiver guidance is explicit: *"If you plan to implement UIBC
on a Miracast receiver, **disable the Nagle algorithm** since it queues frames before sending.
Queueing cursor and keyboard frames results in latency, which is perceived by the user as
sluggishness or delay."* lazycast does this (`d2.py:310`).

**Framing:** a plain byte stream with no delimiter but the 16-bit Length at bytes 2–3. The reader
must be a length-prefixed framer tolerant of partial reads and coalesced messages.

### 5.3 The UIBC message header

Spec §4.11.1, Figure 4-9:

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|Vers |T|         Reserved        | Input Cat |     Length      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|        Timestamp (16 bits, present only if T == 1)            |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

In octets:

```
byte 0 : bits 7..5 Version (= 0b000)  | bit 4 T | bits 3..0 Reserved[7:4]
byte 1 : bits 7..4 Reserved[3:0]      | bits 3..0 Input Category
byte 2..3 : Length (u16 big-endian)
byte 4..5 : Timestamp (u16 big-endian), only if T == 1
then      : input body
```

* **Version** — *"shall be set to `0b000`"*.
* **Input Category** — `0` = GENERIC, `1` = HIDC, `2`–`15` reserved.
* **Length** — *"**The length of the entire TCP payload** in units of 8 bits, **from bit offset 0**
  to the end of the UIBC Input Body (**including padding if any**)."* **It counts the header
  itself.** This is the classic mistake.
* **Timestamp** — *"the last 16 bits of the WFD Source marked RTP timestamp of the frames that are
  being displayed when user inputs are applied."* Setting `T=1` lets the source compensate input
  latency, and gives us a free end-to-end latency probe. Worth doing.
* **Padding** — the body *"should be padded up to an integer multiple of 16 bits"*.

Three independent confirmations of the length-includes-header rule:
MiracleCast computes `uibcBodyLen = genericPacketLen + 7` (4 header + 3 generic-message header)
and writes that into bytes 2–3 (`miracle-uibcctl.c:187,197-198`); lazycast's 14-byte HIDC frame
carries `0x000e`; and lazycast's 207-byte-plus-pad frame carries `0x00d0` = 208, proving the pad
byte is counted too.

### 5.4 GENERIC input body

The body is *"one or more Generic Input messages"*, each:

```
+--------+--------+--------+
| TypeID |     Length      |    then TypeID-specific "Describe" payload
+--------+--------+--------+
   1 B         2 B (BE)
```

This inner `Length` counts **only the Describe payload** — unlike the outer header Length. Two
different length conventions in one frame; model them as distinct types.

**Type IDs** (spec Table 4-5; matches MiracleCast's `MessageType` enum):

| ID | Spec name |
|----|-----------|
| 0 | Left Mouse Down / Touch Down |
| 1 | Left Mouse Up / Touch Up |
| 2 | Mouse Move / Touch Move |
| 3 | Key Down |
| 4 | Key Up |
| 5 | Zoom |
| 6 | Vertical Scroll |
| 7 | Horizontal Scroll |
| 8 | Rotate |
| 9–255 | Reserved |

> Beware: the Qualcomm UIBC patent family (e.g.
> [US20130013318A1](https://patents.google.com/patent/US20130013318A1/en)) publishes an **earlier
> draft** table shifted by one (TouchDown = 1 … Zoom = 8, no Rotate). That is not the shipped
> spec. If you find a table where Touch Down is 1, discard it.

**Touch Down / Up / Move (0/1/2)** — Describe length `5N + 1`:

```
+--------+   +--------+--------+--------+--------+--------+
| N ptrs |   | PtrID  |    X (16, BE)   |    Y (16, BE)   |  × N
+--------+   +--------+--------+--------+--------+--------+
```

*"Number of pointers of a multi-touch motion event. When set to `0x01`, it indicates a
single-touch motion event."* Origin (0,0) is the top-left of the display region.

Fully worked single touch-down at (960, 540), no timestamp — Describe = 6, generic message =
1+2+6 = 9, frame = 4+9 = 13 → odd → pad to 14:

```
00 00 00 0E   header: version 0, T=0, category 0 (GENERIC), total length 14
00            Type ID = 0 (Touch Down)
00 06         Describe length = 6
01            Number of pointers = 1
00            Pointer ID = 0
03 C0         X = 960
02 1C         Y = 540
00            padding
```

**Key Down / Up (3/4)** — Describe length always 5:

```
+--------+--------+--------+--------+--------+
|reserved|   Key code 1    |   Key code 2    |
+--------+--------+--------+--------+--------+
```

> **The key codes are ASCII, not HID usage IDs and not Windows VK.** Spec Table 4-9: *"The basic/
> extended ASCII code uses the lower one byte. The higher one byte is reserved for future ASCII
> compatible key codes. **The higher one byte shall be sent before the lower one byte.**"* Key
> code 2 is `0x0000` when absent.
>
> **Consequence: the GENERIC key path cannot express arrow keys, function keys, modifiers, or
> Escape.** This is precisely why Windows advertises `input_category_list=HIDC` only, and why
> lazycast sends raw USB HID keyboard reports. If you want a usable keyboard back-channel, you
> must implement HIDC.

MiracleCast confirms the layout: `genericPacketLen = 5`, `outData[7] = 0x00 // reserved`, key code
1 at bytes 8–9, key code 2 at 10–11, both high-byte-first (`miracle-uibcctl.c:316-353`).

**Zoom (5)** — Describe length 6: `X (2) | Y (2) | integer times to zoom (1) | fraction (1)`.
Integer part is **unsigned**; *"The unit of the fractional part shall be 1/256, and the sign of the
fractional part is always positive."*

**Vertical / Horizontal Scroll (6/7)** — Describe length 2, and it is **not** a signed integer and
**not** fixed point:

| Bits | Field |
|------|-------|
| 15:14 | Scroll Unit: `0b00` = pixels (normalised to the source display resolution from M4), `0b01` = mouse notch, `0b10`–`0b11` reserved |
| 13 | Direction. Vertical: `0` = down, `1` = up. Horizontal: `0` = right, `1` = left |
| 12:0 | Number of scroll units — **13-bit unsigned magnitude** |

Sign lives in bit 13 as a direction flag; magnitude is unsigned, max 8191.
**Prefer `0b01` (mouse notch)** — it sidesteps the ambiguity about whose resolution "pixels" are
normalised to.

**Rotate (8)** — Describe length 2: `integer (1, signed) | fraction (1)`, **in radians**.
*"A negative number indicates to rotate clockwise; a positive number indicates to rotate
counter-clockwise."* Fraction is 1/256 units and **always added positively**, so −0.5 rad is
`integer = 0xFF (−1), fraction = 128` → −1 + 128/256.

> **Do not copy MiracleCast's zoom/scroll code.** Its *comments* are quoted spec, but
> `getUIBCGenericZoomPacket` computes the Y coordinate and never writes it, and
> `getUIBCGenericScalePacket` does not implement the B15:B14 / B13 / B12:B0 packing its own comment
> describes. It also has an off-by-one buffer overrun at `while (offset <= uibcBodyLen)`. Its touch
> and key builders are sound.

### 5.5 Coordinate space — the thing to get right

The spec says two different things for two different fields, and this is the trap:

1. **Touch / mouse / zoom coordinates** are *"normalized with respect to the **negotiated
   resolution of the video stream**"*, origin at the top-left.
2. **Scroll, when the unit is pixels**, is *"normalized with respect to the **WFD Source display
   resolution that is conveyed in an RTSP M4 request message**"*.

Despite the word "normalized", these are plain pixel indices in the range `0 .. W-1` / `0 .. H-1` —
there is no 0..65535 fixed-point mapping. MiracleCast confirms: it divides the local event
coordinate by the panel-to-stream scale factor
(`temp = (int32_t)((double)temp / widthRatio)`, `miracle-uibcctl.c:222,228`).

For us: the C6522QT is a 4K panel that will typically render a 1920×1080 negotiated stream, quite
possibly letterboxed. A touch at physical pixel (3840, 2160) must be sent as (1920, 1080). You must
invert *exactly* the transform your renderer applied — subtract letterbox offsets, then scale by
`negotiated / rendered`. Getting this wrong gives the classic "pointer moves at half speed and only
reaches the top-left quadrant" bug.

The ETSI MirrorLink profile makes explicit what WFD leaves implicit (TS 103 544-17 §7.2.3): *"The
MirrorLink Client shall provide the coordinates within the framebuffer resolution of the current
WFD session."*

**Type-system consequence (ground rule 1):** make the UIBC coordinate a newtype constructible only
from a negotiated-resolution context — `NegotiatedVideoFormat::map_from_panel(PanelPoint) ->
SourcePixel`. Then a raw panel coordinate cannot typecheck into a UIBC frame, and the one place the
transform lives is the same `NegotiatedConfig` that came out of M4 (§9.2).

### 5.6 HIDC

HIDC (Input Category 1) forwards raw **USB or Bluetooth HID reports** rather than abstract events:
*"actual user input data is in the format as defined in an external HID specification."*

**HIDC message format** (spec Table 4-15):

| Field | Size | Value |
|-------|------|-------|
| HID Input Path | 1 | see below |
| HID Type | 1 | see below |
| Usage | 1 | **`0x00` = the value is a HID input *report*; `0x01` = the value is a HID *report descriptor*** |
| Length | 2 | length of the HIDC value in octets |
| HIDC value | var | the report or descriptor |

**HID Input Path**: `0` Infrared, `1` USB, `2` Bluetooth, `3` Zigbee, `4` Wi-Fi, `5`–`254`
reserved, `255` vendor specific. (Note the RTSP `hidc_cap_list` spells Bluetooth as `BT`.)

**HID Type**: `0` Keyboard, `1` Mouse, `2` Single Touch, `3` Multi Touch, `4` Joystick, `5` Camera,
`6` Gesture, `7` Remote controller, `8`–`254` reserved, `255` vendor specific.

**Descriptor registration rule:** *"For each HID interface type and HID type combination, a WFD
Sink should send its associated HID report descriptor to the WFD Source before it sends HID input
reports."* There is an escape hatch: *"If the HID input reports that a WFD Sink sends are based on
the default report descriptors for USB keyboard and mouse, the WFD Sink is not required to send HID
report descriptors."* The defaults are the canonical boot-keyboard and 3-button-mouse descriptors
from USB HID 1.11 Appendix E.6 and E.10.

#### Verified against real bytes accepted by Windows

lazycast (`d2.py:313-333`) sends six registration frames on connect, then live reports. Decoding
the first validates the whole layout:

```
10        version 000, T=1
01        Input Category = 1 (HIDC)
00 3E     Length = 62 (whole frame)
2A B6     Timestamp
01        HID Input Path = 1 (USB)
01        HID Type      = 1 (Mouse)
01        Usage         = 1 (report DESCRIPTOR)
00 33     HIDC value length = 51
05 01 09 02 A1 01 85 28 ...   Usage Page(Generic Desktop), Usage(Mouse),
                              Collection(Application), REPORT_ID 0x28, ...
```
Arithmetic: 6 + 3 + 2 + 51 = 62 = `0x3E` ✓

The other five: `m2` HID Type `0x00` Keyboard (`05 01 09 06`, Report ID `0x29`); `m3` HID Type
`0x07` Remote controller (`05 0C 09 01`, Consumer Control); `m4` HID Type `0x03` Multi Touch
(`05 0D 09 04`, Digitizers/Touch Screen, 623-byte descriptor); `m6` HID Type `0x06` Gesture
(`05 0D 09 02`, Digitizers/Pen) — 4+3+2+196 = 207, odd, **padded to 208 = `0x00D0`**, direct wire
proof that padding is counted in Length.

Live reports (`T = 0`, no timestamp):

```
mouse:    00 01 00 10 | 01 01 00 00 06 | 28 <buttons> <dx> <dy> <wheel> 00 | 00(pad)
keyboard: 00 01 00 12 | 01 00 00 00 09 | 29 <mods> 00 <keycode> 00 00 00 00 00
```

The mouse value is Report ID `0x28` + 5 bytes; the keyboard value is Report ID `0x29` + the
standard 8-byte USB boot-keyboard report. lazycast's `tohid[]` table (`d2.py:70-71`) is the
evdev → USB HID usage translation.

**These are the most valuable fixtures in this document** — real bytes a shipping Windows source
accepts. Check them in as `proto-miracast/tests/fixtures/uibc/`.

> **Do not use the Freescale/i.MX6 Android patch as a layout reference.** Its `parseUIBC` reads the
> Type ID at `data[6]` and the pointer count at `data[7]`, i.e. it assumes `T=1` *and* omits the
> 2-byte generic-message Length — off by two versus the spec. It is tuned to one particular peer.

### 5.7 What to implement, in order

1. **GENERIC touch** (Type IDs 0/1/2) — the interoperable path, ~14 bytes per event, and exactly
   what the panel needs. Advertise `generic_cap_list=Mouse, SingleTouch, MultiTouch`.
2. **HIDC keyboard + mouse** — required for Windows, which advertises HIDC-only, and the only way
   to send modifiers and arrow keys at all. Register descriptors on connect, then stream reports.
3. Set `T=1` and populate the timestamp from the currently-displayed frame's RTP timestamp.
4. Keep the sans-I/O split: a pure `UibcMessage -> Vec<u8>` encoder and a
   `fn(state, &[u8]) -> (state, Vec<UibcMessage>)` decoder, with the TCP actor a thin shell.

Recommended advertisement:

```
wfd_uibc_capability: input_category_list=GENERIC, HIDC;generic_cap_list=Mouse, SingleTouch, MultiTouch;hidc_cap_list=Keyboard/USB, Mouse/USB, MultiTouch/USB;port=none
```

**Parse leniently.** Windows' own sink emits a string that omits `generic_cap_list=` entirely —
not ABNF-conformant, but real:

```
wfd_uibc_capability: input_category_list=HIDC;hidc_cap_list=Keyboard/USB, Mouse/USB, MultiTouch/USB, Gesture/USB, RemoteControl/USB, Joystick/USB;port=none
```

Whitespace around `,` and `=` varies between senders. Accept all of it; emit the canonical form.

**Reality check:** UIBC is primarily a Windows feature. Stock Android's WFD source never
implemented it. Do not expect touchback from an Android phone.
## 6. HDCP 2.x content protection

**Conclusion first: we advertise `wfd_content_protection: none`, and everything except DRM-locked
video still mirrors.** The rest of this section is the justification and the defensive parsing we
still need.

### 6.1 What the parameter means

Grammar in §3.7. Miracast v2.3 §6.1.5: *"If content protection is not supported **or is not
currently possible for any reason**, the parameter is set to 'none'."* Answering `none`, or
omitting the parameter entirely, is explicitly legal.

The discovery layer carries a single bit — Device Info bit 8, `0x0100`, "Content Protection using
HDCP2.0" (§1.4). One bit, no version granularity. Leave it clear.

The source asks in the M3 *request*; the sink answers in the M3 *response*. Miracast v2.1 §6.4.3
adds the timing: *"the WFD Sink should start listening upon transmission of an RTSP M3 response
message, and the WFD Source should start sending AKE_Init upon receipt of the RTSP M3 response
message."*

### 6.2 Transport — the sink is the server here

Miracast v2.3 §4.7: *"AKE, locality check, and SKE messages … shall be transported over a TCP
connection that is **different from a TCP connection for RTSP messaging**. The WFD Sink shall
advertise its local TCP port ID for exchanging the HDCP 2.0 messages, using the
wfd-content-protection parameter. **The WFD Sink shall act as a TCP server for this
connection.**"*

Confirmed by AOSP, which is the source and dials out
(`WifiDisplaySource.cpp:1726-1730`, `mHDCP->initAsync(mClientInfo.mRemoteIP.c_str(), mHDCPPort)`).

**Note the asymmetry:** RTSP → sink connects. UIBC → sink connects. HDCP → **sink listens**.
Getting one backwards produces a silent hang, not an error. See the table in §2.1.

**Framing:** no length prefix. HDCP-IIA §4.2: *"Each packet must contain exactly one message. Each
packet payload commences with a `msg_id` … followed by parameters specific to each message."* The
length is a pure function of `msg_id` plus prior negotiated state, so a reader must be a
table-driven `msg_id → expected_len` machine. Two lengths are state-dependent:
`LC_Send_L_prime` (33 or 17, depending on the locality pre-compute bits) and `SKE_Send_Eks`
(25 or 57, depending on whether the receiver declared ≥ 2.3).

### 6.3 The handshake, for reference

Message IDs (HDCP Interface Independent Adaptation Spec Rev 2.3, Table 4.1). All multi-byte fields
big-endian.

| msg_id | Message | Direction | Payload after msg_id | Total |
|--------|---------|-----------|----------------------|-------|
| 1 | Null | | | |
| 2 | `AKE_Init` | Tx→Rx | `rtx` (8) | 9 |
| 3 | `AKE_Send_Cert` | Rx→Tx | `REPEATER` (1) + `certrx` (522) | 524 |
| 4 | `AKE_No_Stored_km` | Tx→Rx | `Ekpub_km` (128) | 129 |
| 5 | `AKE_Stored_km` | Tx→Rx | `Ekh_km` (16) + `m` (16) | 33 |
| 6 | `AKE_Send_rrx` | Rx→Tx | `rrx` (8) | 9 |
| 7 | `AKE_Send_H_prime` | Rx→Tx | `H'` (32) | 33 |
| 8 | `AKE_Send_Pairing_Info` | Rx→Tx | `Ekh_km` (16) | 17 |
| 9 | `LC_Init` | Tx→Rx | `rn` (8) | 9 |
| 10 | `LC_Send_L_prime` | Rx→Tx | `L'` (32), or 16 in pre-compute mode | 33 / 17 |
| 11 | `SKE_Send_Eks` | Tx→Rx | `Edkey_ks` (16) + `riv` (8) [+ HMAC (32) if Rx ≥ 2.3] | 25 / 57 |
| 12–17 | `RepeaterAuth_*` | | repeater topology / stream management | var |
| 18–20 | `Receiver_AuthStatus`, `AKE_Transmitter_Info`, `AKE_Receiver_Info` | | 2.2+ only | |

`certrx` is 522 bytes: Receiver ID (5) ‖ RSA modulus (128) ‖ exponent (3) ‖ reserved (2) ‖ **DCP
LLC RSA-3072 signature (384)**.

As a plain (non-repeater) sink you set `REPEATER = false` and msg_ids 12–17 never occur — worth
making unrepresentable in the typestate.

**The locality check budget is 7 ms, not 20 ms.** HDCP-IIA §2.3: *"Sets its watchdog timer to
**7 ms**. Locality check fails if the watchdog timer expires."* (The 20 ms figure that circulates
belongs to HDCP 2.2 *on HDMI*, a different adaptation.) Miracast's own informative Annex-C exists
precisely because 7 ms over Wi-Fi Direct is tight, and recommends setting TCP `URG`/`PSH`, IP TOS
for expedited forwarding, and WMM access category `AC_VI` on those two packets. Up to 1024 retries
are permitted with a fresh nonce each time.

### 6.4 How the AV stream would be encrypted

**AES-128-CTR**, key = `ks ⊕ lc128`, counter block `p = (riv ⊕ streamCtr) ‖ inputCtr`.

* `streamCtr` is **32 bits** (not 64): *"The HDCP Transmitter assigns streamCtr values where the
  **least significant bit is zero to the video PES** … **one to the audio PES**"*, and the receiver
  *"shall fail if streamCtr assignment does not comply"*.
* `inputCtr` is **64 bits**, per elementary stream, never reset after SKE.
* `riv` is 64 bits, from `SKE_Send_Eks`.
* Partial final blocks: *"the unused key stream bits produced by the AES-CTR module must be
  discarded, and not carried over to a subsequent PES packet."*

**The counters ride in the PES header, not in an RTP-level header.** There is no HDCP header
prepended to the RTP payload — a common misconception. HDCP-IIA §3.4 Table 3.1 defines a 128-bit
`PES_private_data` field:

```
reserved_bits      13     (zeros)
streamCtr[31..30]   2
marker_bit          1     (= 1)
streamCtr[29..15]  15
marker_bit          1
streamCtr[14..0]   15
marker_bit          1
reserved_bits      11     (zeros)
inputCtr[63..60]    4
marker_bit          1
inputCtr[59..45]   15
marker_bit          1
inputCtr[44..30]   15
marker_bit          1
inputCtr[29..15]   15
marker_bit          1
inputCtr[14..0]    15
marker_bit          1
                  ---
                  128 bits = 16 bytes
```

The marker bits exist to prevent `packet_start_code` emulation. Crucially: *"**The presence of the
PES Header HDCP Private Data block … serves to indicate that HDCP Encryption is enabled** and the
PES payload is encrypted. When HDCP Encryption is disabled, the … block is not included. HDCP does
not use the PES_scrambling_control bits."*

AOSP implements this bit-stuffing byte-for-byte in `MediaSender.cpp:451-489` — the best available
reference. It emits the field via `TSPacketizer::packetize(..., PES_private_data, 16, ...)`, which
sets `PES_extension_flag` (`TSPacketizer.cpp:924`, `0x81` vs `0x80`) and writes
`0x8e  // PES_private_data_flag` followed by the 16 bytes (`:939-942`).

Also: **only video is encrypted in practice** — AOSP `MediaSender.cpp:405`,
`if (mHDCP != NULL && !info.mIsAudio)`. The spec permits audio encryption at the source's
discretion.

The receiver *reseeds* `inputCtr` from every PES header rather than tracking it independently,
which is what makes packet loss survivable.

### 6.5 Why we will not implement it

The algorithms are public — AES-128-CTR, SHA-256, HMAC-SHA256, RSA, CTR-DRBG, all standard and all
available in Rust. What is not available is the **secrets**:

1. **`lc128`** — a 128-bit global constant distributed only under the HDCP license. Without it the
   AES key is wrong and every frame decodes to noise.
2. **A receiver device key set and `certrx`** — the certificate carries a DCP LLC RSA-3072
   signature that the transmitter verifies. There is no self-signed path and no way to forge it.

Digital Content Protection LLC licensing: adopters sign the HDCP License Agreement plus a 2.x
addendum and pay an annual adopter fee; receiver key sets cost **$3,000 for 10,000 keys**, $7,500
for 100,000, $15,000 for 1,000,000, issued only to a signatory legal entity after fees are paid in
full. The license also imposes **robustness rules** requiring protection against key extraction,
which in practice means a hardware root of trust — a Rust binary with keys in a file would violate
them even if we somehow had keys.

**There is no legitimate path to an open-source HDCP 2.x receiver.** Every open Miracast sink —
MiracleCast, lazycast, intel/wds — advertises `none`.

### 6.6 What actually breaks

**Nothing, for our use case.** The correct mental model: HDCP-over-WFD is not an access gate for
mirroring; it is a capability the *source's DRM stack* queries before it will render protected
content into the mirrored framebuffer.

* **Android still mirrors.** AOSP `WifiDisplaySource.cpp:939-943` logs *"Sink doesn't appear to
  support content protection"* / *"Sink does not support content protection"* and simply leaves
  `mUsingHDCP = false`. It also fails *open*: if `makeHDCP()` fails it logs *"Unable to instantiate
  HDCP component. **Not using HDCP after all.**"* And Android's `libstagefright_hdcp.so` is a vendor
  blob most AOSP builds do not ship, so most Android devices cannot do HDCP over Miracast anyway.
* **Windows still mirrors.** Three independent confirmations: lazycast ships `none` and works
  against Windows 8.1/10/11 (its README: *"Neither the key nor the hardware is available on Pi and
  therefore is not supported"*); **Windows' own Miracast sink advertises `none`** in captured M3
  responses; and Microsoft's receiver-requirements checklist — which lists every feature as
  Required / Recommended / Optional — **does not mention HDCP at all**.
* **What is blocked is DRM-protected video.** On Windows, PlayReady *"allows you to play content
  over Miracast output as soon as HDCP 2.0 or later is engaged"* — without it, Netflix/Prime/
  Disney+ render **black** on the mirrored output while the rest of the desktop mirrors normally.
  On Android the mechanism is orthogonal: apps mark windows `FLAG_SECURE` and SurfaceFlinger omits
  them from any non-secure virtual display, so you get a black rectangle regardless of what
  `wfd_content_protection` said — advertising HDCP would not help.

For a hackerspace panel that receives slides, browsers, terminals, and people's own video, this
costs nothing. Anyone who wants Netflix on the panel should use a streaming stick.

### 6.7 What we implement instead

1. Always emit `wfd_content_protection: none` in the M3 response.
2. Leave WFD IE Device Info bit 8 clear.
3. **Model it so it cannot drift.** `ContentProtection` should be *parseable* as
   `Hdcp2_0 { port } | Hdcp2_1 { port } | None` — we must parse an inbound value to reject it
   cleanly — but the M3-response builder should accept only the `None` variant. Ground rule 1: the
   deliberate non-support belongs in the types, not a comment.
4. **Defend the receive path.** A well-behaved source will never encrypt after we said `none`, but
   "should" is doing a lot of work. Detect it two ways and fail loudly rather than rendering green
   mush:
   * an `HDCP` registration descriptor in the PMT (tag `0x05`, length 5, `'H' 'D' 'C' 'P'`, version
     byte — see §4.3, and note the version byte is `0x20` even for 2.1, so never use it to infer
     the revision);
   * a video PES with `PES_extension_flag` set and 16 bytes of private data.

   Surface either as a typed `MiracastError::UnexpectedHdcpEncryption` and put a message on the
   panel.

Miracast v2.3 §4.7 even sanctions the graceful degradation: *"If the HDCP 2.0 session key
establishment fails, the WFD Sink … may send an RTSP M7 request message … to start an RTP
streaming. In this case, the WFD Source can only transmit audio and/or video content that is not
required to be protected."* That is precisely the mode we operate in permanently.
## 7. Real-world sender behaviour

The spec describes a protocol. This section describes what actually arrives on the wire, and what
a sink must do to be accepted.

### 7.1 Identifying who you are talking to

Do this first, because almost every workaround below is conditional on the peer.

**Windows** announces itself in the `Server:` header of the M2 response — the first thing the
source sends you that has any identity in it. [MS-WFDPE] §2.5.1.1 specifies it exactly:

```
Server: MSMiracastSource/10.00.10011.0000 guid/be113d06-9e40-43e4-98e6-540a325e9ced
```

**Android** identifies via `Server:` too, built from the build fingerprint —
`stagefright/1.1 (Linux;Android 4.1)`, sometimes with a vendor suffix (`:rockchip`).

Secondary tells: a source that asks for `microsoft_*` parameters in M3 is Windows; one that asks
for `intel_*` is Windows or an Intel WiDi stack.

### 7.2 Windows as a source

**The M3 extension set you must answer.** Windows queries a large set of `microsoft_*` parameters
and sessions degrade or fail without answers. From a working Windows 11 interop report
([miraclecast#471](https://github.com/albfan/miraclecast/issues/471)):

```
microsoft_cursor
microsoft_rtcp_capability
microsoft_latency_management_capability
microsoft_format_change_capability
microsoft_diagnostics_capability
microsoft_video_formats
microsoft_max_bitrate
microsoft_multiscreen_projection
microsoft_audio_mute
microsoft_color_space_conversion
wfd_idr_request_capability
```

**Answering `none` to each is sufficient to get a session.** Answer them; do not ignore them. Then
selectively upgrade: `microsoft_rtcp_capability: supported` buys you bitrate adaptation (§7.5 below),
and `microsoft_cursor` buys a sub-30 ms pointer.

**Other observed Windows behaviours:**

* **A 2-minute RTP liveness watchdog.** If no RTP arrives, Windows tears down with
  `microsoft_teardown_reason: C00D4278 No RTP data was provided for 2 minutes`. Since we are the
  one *receiving*, this fires when our SETUP advertised a port we are not actually bound to — the
  classic symptom is "connects, shows 'Connected', then drops after two minutes with no error on
  the sink side."
* Windows will `400 Bad Request` a `GET_PARAMETER rtsp://localhost/wfd1.0` it does not like.
* Windows prefers to be the **P2P Group Owner (intent 14)** — see §1.8. It also **prefers
  Miracast-over-Infrastructure** when the sink advertises it (§1.10), and will not attempt MICE
  over a WLAN lacking WPA2.
* Windows' own sink advertises `wfd_content_protection: none` and a UIBC capability string that is
  not ABNF-conformant (§5.7).
* Windows uses the RTP marker bit as an end-of-frame flag (§4.2) and changes video format in-band
  via SPS/PPS with no RTSP announcement (§4.7).
* Windows uses **CABAC**, which the R1 spec forbids (§3.2).

**Microsoft's own receiver checklist** is short and worth reading in full
([Wireless Projection … for Receiver Manufacturers](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/wireless-projection-receiver-manufacturers)):

| Feature | Status |
|---------|--------|
| WPS IE attributes `WPS:Manufacturer`, `WPS:Model` (unique), `WPS:Model-Number` (used as firmware version) | **Required** |
| Miracast over Infrastructure | Strongly recommended — *"the Windows 10 client will **prefer** that method"* |
| IP allocation in EAPOL-Key frames (skips DHCP) | Recommended |
| Extended Channel Switch Announcement (eCSA) | Recommended |
| Persistent P2P groups (reconnect instead of re-pair) | Recommended |
| TCP retry tuning during RTSP setup (no early exponential backoff) | Recommended |
| Hardware cursor | Recommended |
| UIBC, with **Nagle disabled** | Optional |
| HDCP | **not mentioned at all** |

### 7.3 Android as a source

**Timeline — the widely repeated version is wrong.** "Google dropped Miracast in Android 6.0" is a
*product* fact (the Nexus 5X/6P shipped without it), not a *code* fact. Verified by probing AOSP
tags directly:

| Component | Present through | Removed |
|-----------|-----------------|---------|
| `wifi-display/sink/` | 4.2.x only | [`6ea551fa`](https://android.googlesource.com/platform/frameworks/av/+/6ea551fa13b69e5ce359a7dba7485d857a005304), 2013-10-02, *"Remove obsolete miracast sink code and friends."* |
| `wifi-display/` (source) | `android-8.1.0_r81` | [`d0a98fa0`](https://android.googlesource.com/platform/frameworks/av/+/d0a98fa05f0f6719b93d000c4638230af06e0b99), 2017-09-18, *"remove Miracast sender code… to avoid introducing new HAL for HDCP."* |

**On stock Android 9 and later there is no source at all.** The Java scaffolding survives
(`WifiDisplayController.java`, `config_enableWifiDisplay`), but
`MediaPlayerService::listenForRemoteDisplay()` at `android-16.0.0_r1` is now:

```cpp
ALOGE("listenForRemoteDisplay is no longer supported!");
return NULL;
```

So stock AOSP ≥ 9 discovers and connects P2P, then never opens the RTSP session — users see
"device found, cast fails". **Every Android source you will actually meet is a vendor fork**
(Samsung Smart View, LG, Sony, Xiaomi), and almost all are forks of this same code, so the
behaviour below still models them.

* Android sets **GO intent 0** — it wants the sink to be Group Owner (§1.8).
* **`sendM3()` is called only from inside `onOptionsRequest()`.** If the sink never sends its own
  M2 `OPTIONS *`, **Android waits forever and times out at 30 s.** This is the single most common
  "connects but nothing happens" cause against Android.
* Android's negotiation ignores the sink's `native` field and
  `wfd_preferred_display_mode` entirely; it maximises `width × height × fps × (interlaced ? 1 : 2)`
  over the intersection of the two advertisements (§3.2). **The only way to steer it is which bits
  you set.**
* Audio: AAC by default, LPCM only if the `media.wfd.use-pcm-audio` system property is set. A sink
  must advertise both (§3.3). **`AC3` is never inspected at all**, and — the classic trap —
  `LPCM 00000001 00` (44.1 kHz, bit 0) *alone* fails, because Android tests `modes & 2`. Bit 1 is
  the only LPCM bit it looks at.

> ### ⚠ Stock AOSP has a hard 1280×720p30 ceiling
>
> ```cpp
> mSupportedSourceVideoFormats.setNativeResolution(VideoFormats::RESOLUTION_CEA, 5); // 1280x720 p30
> // Enable all resolutions up to 1280x720p30
> mSupportedSourceVideoFormats.enableResolutionUpto(RESOLUTION_CEA, 5, PROFILE_CHP, LEVEL_32);
> ```
>
> **Stock AOSP will never exceed CEA index 5 — 1280×720p30, CHP, Level 3.2.** A sink that
> advertises only 1080p modes gets `"Sink and source share no commonly supported video formats."`
> → `ERROR_UNSUPPORTED` → session dead.
>
> **Always advertise CEA bits 0–5**, however capable the panel is. Vendor forks raise the ceiling
> (a real vendor source was observed negotiating CEA bit 7 = 1080p30), but the floor costs nothing
> and is the difference between working and not.

> ### ⚠ A short `wfd_video_formats` will `abort()` the peer
>
> `VideoFormats::parseFormatSpec` does `CHECK_LE(offset + 58, size)` — an **abort**, not an error
> return. A value shorter than ~64 characters kills `mediaserver` on the phone. It then walks codec
> tuples with a hard-coded `offset += 60` stride, which is only correct when `max-hres`/`max-vres`
> are the literal `none none`.
>
> **Therefore: emit exactly one H.264 tuple, in the canonical fixed-width form, ending
> `none none`.** Anything R2-shaped (real `max-hres`/`max-vres` like `0780 0438`) goes in
> `wfd2_video_formats` only. This is exactly what the known-working MiracleCast-on-Windows config
> does.
* Android hard-codes the RTSP control port rather than reading it from your WFD IE, with exactly
  one quirk override for Broadcom dongles (§1.4).
* AOSP's parsers have hard-coded offsets that constrain what you may emit: a 60-byte stride per
  `wfd_video_formats` codec tuple, a literal `+8` skip past `"HDCP2.x "`, and a
  `strstr("wfd_idr_request\r\n")` substring match.
* **Stock Android never implemented UIBC.** Do not expect touchback from a phone.
* Android fails *open* on HDCP (§6.6).

### 7.4 The minimum a sink must get right

Ranked by how often each one is the actual bug:

1. **Connect out to the source's RTSP port; do not listen.** (§2.1) The single most common
   structural mistake, because the sink advertises a "Control Port" that sounds like a listen port.
2. **Be a proper incremental RTSP parser.** Sources coalesce M4+M5 into one TCP segment and split
   messages across segments. A `recv()`-per-message loop works right up until it doesn't.
3. **Run two independent CSeq counters**, one per direction.
4. **`wfd_client_rtp_ports` port1 must be `0`.** AOSP rejects the session outright otherwise
   ("Sink chose its wfd_client_rtp_ports poorly"). Confirmed across five implementations.
5. **Answer only what M3 asked for**, and answer every `microsoft_*` it asked for, even with
   `none`.
6. **Use the M4 `wfd_presentation_URL` verbatim** in M6/M7 — do not reconstruct it. Note M1/M3/M4/
   M5/M16 request-URIs use the literal string `rtsp://localhost/wfd1.0` while M6/M7 use the
   source's real IP.
7. **Advertise both LPCM and AAC.** LPCM 48 kHz 2 ch is a conformance requirement for any sink with
   speakers (§3.3).
8. **Parse PAT/PMT** rather than assuming PIDs (§4.3).
9. **Handle `PES_packet_length == 0`** for large video frames (§4.4).
10. **Tolerate a missing `Session:` header** — AOSP's own comment: *"the older dongles do not
    always include a Session: header."*

Known-good advertisement, assembled from the values that demonstrably work:

```
wfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play
wfd_audio_codecs: LPCM 00000002 00, AAC 00000001 00
wfd_video_formats: 00 00 03 10 0001FFFF 1FFFFFFF 00000FFF 00 0000 0000 00 none none
wfd_3d_video_formats: none
wfd_coupled_sink: none
wfd_connector_type: 05
wfd_uibc_capability: input_category_list=GENERIC, HIDC;generic_cap_list=Mouse, SingleTouch, MultiTouch;hidc_cap_list=Keyboard/USB, Mouse/USB, MultiTouch/USB;port=none
wfd_standby_resume_capability: none
wfd_content_protection: none
wfd_idr_request_capability: 1
```

(profile `03` = CBP + CHP, level `10` = 4.2; VESA capped at bit 28 per §3.2.)

### 7.5 Loss, and the absence of repair

**There is no NACK, no FEC, and no RTX anywhere in WFD.** The only loss-recovery primitive is
`wfd_idr_request` (M13) over the RTSP channel, and both Windows and Android honour it.

Consequences for the design:

* Make IDR-request a **first-class output of the pure protocol core**, so the decoder's error path
  can raise it and the test harness can assert on it.
* **AOSP's encoder default is `i-frame-interval = 15` — IDRs fifteen *seconds* apart**
  (`Converter.cpp`, comment: *"Iframes every 15 secs"*), at 5 Mbit/s CBR. That single constant is
  the whole justification for implementing M13: without it, any loss means up to 15 seconds of
  corruption, and a freshly-attached decoder waits that long for a first picture. It is also why
  `wfd_idr_request_capability: 1` fixes the classic "black screen for the first few seconds,
  audio fine" complaint.
* **Rate-limit it.** A real capture shows a Samsung sink firing M13 eight times back-to-back; each
  IDR collapses the bitrate, so an unthrottled request storm turns a lossy link into an unusable
  one.
* `microsoft_rtcp_capability: supported` is the only sanctioned back-pressure channel — Windows
  *"modulates the bitrate"* from RTCP receiver reports. The RTCP port arrives as the **second
  `server_port` in the SETUP response**, not in `wfd_client_rtp_ports`. Not supported on Win10
  v1507/v1511. Intel WiDi does the same thing with a proprietary post-PLAY
  `SET_PARAMETER: intel_enable_widi_rtcp: 53153`.

### 7.6 The Linux driver reality

This is where the project's risk actually lives.

**Chipset support, verified against current `torvalds/linux` `interface_modes` / interface
combinations.** This is a hardware *selection* decision, not a detail:

| Driver / chips | P2P_GO | P2P_CLIENT | P2P_DEVICE |
|---|---|---|---|
| **ath9k** (AR92xx/93xx PCIe) | ✅ | ✅ | ⚠️ chanctx only |
| **ath9k_htc** (AR9271/AR7010 USB) | ✅ | ✅ | ❌ |
| **ath10k** (QCA988x/9377/9984, WCN3990) | ✅ | ✅ | ✅ |
| **iwlwifi/mvm** (7260…AX2xx, BE2xx) | ✅ | ✅ | ✅ |
| **mt76 / mt792x** (mt7921/7922/7925) | ✅ | ✅ | ✅ |
| **mt76x02** (mt7610u, mt7612u USB) | ✅ | ✅ | ❌ |
| **rtw89** (8852AE/BE/CE, 8922AE) | ✅ | ✅ | ❌ |
| **brcmfmac** (BCM43xx, Pi CYW43455) | ✅ firmware-gated | ✅ | ✅ |
| **mt7601u** | ❌ | ❌ | ❌ — STATION only |
| **rtw88** (8822BE/CE, 8821CE, 8723DE) | ❌ | ❌ | ❌ — STATION\|AP\|ADHOC only |
| **rtl8xxxu** (in-tree RTL8188/8192/8723 USB) | ❌ | ❌ | ❌ |

**Recommendation: pin one adapter and make it a documented requirement.** ath9k / ath9k_htc /
ath10k for a fixed appliance; mt7612u (ALFA AWUS036ACHM, COMFAST CF-WU785AC) as the cheap USB
option. **Avoid rtw88, rtl8xxxu and mt7601u entirely.**

Capability check on real hardware:

```sh
iw phy phy0 info | grep -A15 'Supported interface modes'
iw list | grep -A12 'valid interface combinations'
iw dev   # look for 'type P2P-device'
```

Advertised support is not the same as working support: **mt7921 claims `P2P_GO` and still fails**
— `P2P-GO-NEG-SUCCESS role=GO` → `WPS-PBC-ACTIVE` → `P2P-GROUP-FORMATION-FAILURE`, while the same
hardware works as a client with `go_intent=0`.

Error strings worth recognising: `Failed to create interface p2p-wlan0-0: -95 (Operation not
supported)` (driver EOPNOTSUPP — try `p2p_no_group_iface=1`); `P2P: Failed to start listen mode`
(driver ignored `NL80211_CMD_REMAIN_ON_CHANNEL` — the P2P module hard-requires
`remain_on_channel`, `cancel_remain_on_channel`, `send_action`, `probe_req_report`).

> ### ⚠ NetworkManager structurally cannot host a Miracast sink
>
> Verified against NM `main`: the only `P2PDevice` methods NM ever calls are `Find`, `StopFind`,
> `Connect`, `Cancel`, `Disconnect` — **there is no `GroupAdd`**, so NM cannot create an autonomous
> GO at all. Its own docs concede `peer` is *"currently the only way to create or join a group"*.
> Worse, `nm_supplicant_interface_p2p_connect()` welds `go_intent` to **7** with no override
> ([NM#1968](https://gitlab.freedesktop.org/NetworkManager/NetworkManager/-/work_items/1968), open).
> A "Wi-Fi Display / Miracast Sink Support" MR was
> [closed unmerged](https://gitlab.freedesktop.org/NetworkManager/NetworkManager/-/merge_requests/2228).
>
> **castaway must own its own `wpa_supplicant` instance**, either by stopping NM or — better for an
> appliance — by giving NM `[keyfile] unmanaged-devices=interface-name:wlan1` and running our own
> supplicant against a dedicated radio. This is a deployment/packaging requirement, not just code.
>
> **iwd is not a substitute**: gnome-network-displays states flatly *"The use of iwd is currently
> not supported."*
* MiracleCast's architecture (`miracle-wifid` / `miracle-sinkctl` / `miracle-dispd`) exists to
  wrap `wpa_supplicant` rather than talk nl80211 directly, which is a reasonable pattern for us to
  copy — it isolates the part that varies by chipset.
* **The autonomous-GO strategy (§1.8) removes the GO-negotiation state machine but not the driver
  requirement.** You still need a chipset that can be a P2P GO.
* **Miracast-over-Infrastructure (§1.10) is the escape hatch.** It needs no P2P data path at all —
  mDNS, a TCP listener on 7250, and an RTSP client. Windows 10 v1703+ *prefers* it. It still
  requires the sink to beacon a WSC Vendor Extension so Windows knows to try, but that is
  advertisement work, not group-formation work.

> **Sequencing recommendation.** Build the protocol core (§2–§5) first against fixtures, then MICE,
> then Wi-Fi Direct. MICE gets a working Windows demo with the least hardware risk, and the RTSP
> session downstream of it is byte-identical either way.

### 7.7 Can a third-party app be a Miracast sink on Windows?

This is the load-bearing question for the deploy target, and the answer is **"not through any
supported sink API — but probably yes by building the group ourselves."** Three candidate paths,
in order of how much we'd own:

**(a) `WFDStartDisplaySink` — dead.** The Win32 native-WiFi function does exactly what we want:
*"Makes the PC discoverable / Sets the P2P device info / **Sets the Miracast IEs on all Wi-Fi
Direct frames with the device type as the sink** / Registers the callback."* But the requirements
block reads **"End of client support: Windows 10; End of server support: Windows Server 2016."**
([learn.microsoft.com](https://learn.microsoft.com/en-us/windows/win32/nativewifi/wfdstartdisplaysink)).
Not an option.

**(b) `Windows.Media.Miracast.MiracastReceiver` — exists, but UWP-shaped and poorly supported.**
Added in Windows 10 1903. The flow is `CreateSession(CoreApplicationView)` → `Start()` →
`MediaSourceCreated` → you render a `MediaSource` whose URI looks like
`mcrecv://192.168.137.247:7236/h-0000000c/192.168.137.1`. Three blockers:

* **`CoreApplicationView` is on Microsoft's explicit "not supported in desktop apps" list**
  ([winrt-api-desktop-app-support](https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/winrt-api-desktop-app-support)).
* It needs the `PrivateNetworkClientServer` capability, i.e. **package identity** (MSIX). Without
  it, `Start()` returns `AccessDenied`.
* Microsoft's own position on calling it from Win32 is *"using UWP class in win32 is not in the
  support range of winapi-SDK"*
  ([MS Q&A 201711](https://learn.microsoft.com/en-us/answers/questions/201711/calling-winrt-miracastreceiver-from-a-desktop-appl)).
  Field reports are grim: stream corrupts within seconds unless `RealTimePlayback` and
  `IsVideoFrameServerEnabled` are both set ([WUS #1145](https://github.com/microsoft/Windows-universal-samples/issues/1145));
  WPF/.NET Core gets the URI but cannot render it ([#1168](https://github.com/microsoft/Windows-universal-samples/issues/1168));
  WinUI 3 crashes ([WindowsAppSDK#3838](https://github.com/microsoft/WindowsAppSDK/issues/3838)).
  There is no `MiracastReceiver` sample anywhere in `microsoft/Windows-universal-samples`.

Taking this path also means the Windows slice is a **completely different implementation** from the
Linux slice — Windows owns the protocol and hands us pixels. That is the opposite of ground rule 5.

**(c) `Windows.Devices.WiFiDirect.WiFiDirectAdvertisement` — the promising one.** Windows 10 10240,
`UniversalApiContract` **v1.0**, far older and broader than the Miracast namespace. It exposes
**`IsAutonomousGroupOwnerEnabled`** *and* **`InformationElements`**
(`IVector<WiFiDirectInformationElement>`), plus `ListenStateDiscoverability` and
`SupportedConfigurationMethods`. In principle that is enough to stand up an autonomous GO and
inject our own WFD IE + WSC vendor extension on Windows, then run **castaway's own RTSP/RTP sink
over it — the same architecture as the Linux path**, with no dependency on `Windows.Media.Miracast`.

> **The spike to run first**, before committing to Miracast on Windows: does
> `WiFiDirectAdvertisementPublisher` work from an **unpackaged Win32 process**, and which manifest
> capability (`wiFiControl`? `wiFiDirectServices`?) does it require? A day or two of work that
> determines whether path (c) is real. If it is, the `MiracastBackend` trait has two honest impls
> and the protocol core is shared. If it isn't, Miracast is Linux-only for this project and the
> deploy story changes.

Note also that the WDDM custom-Miracast-stack model is deprecated and source-side only: *"Driver
developers should no longer implement a custom Miracast stack. Microsoft might remove support for
custom Miracast stacks in a future version of Windows."*
([wddm-display-miniport-driver-tasks](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/wddm-display-miniport-driver-tasks-to-support-miracast-wireless-displays)).

The built-in **Wireless Display** optional feature is a working Miracast sink on Windows — but it
is a separate full-screen app we cannot composite into castaway's UI, so it is a fallback for
users, not an implementation strategy. (On Windows 11 22H2+ it is not installed by default:
`DISM /Online /Add-Capability /CapabilityName:App.WirelessDisplay.Connect~~~~0.0.1.0`.)

## 8. Sources and fixtures

Everything here was fetched and inspected while writing this document unless marked
**unverified**. Per ground rule 9, all of it is a wire-behaviour source and a fixture mine — none
of it becomes a runtime dependency.

### 8.1 The specifications — and the good news

**The Wi-Fi Alliance Miracast specification is publicly downloadable.** This was the single most
useful discovery of this research; most secondhand Miracast documentation on the web is wrong in
at least one load-bearing detail, and several such errors were caught by reading the real text.

| Document | URL | Notes |
|---|---|---|
| **Miracast® Specification v2.3** (2024, 196 pp) | `https://www.wi-fi.org/system/files/Miracast_Specification_v2.3.pdf` | **Normative and current.** Tables 25–29 (WFD IE), 34–41 (video), 42–45 (audio), 91 (connector type); §6.1 ABNF for every parameter; §6.4 M1–M16, one subsection each. |
| Wi-Fi Display Technical Spec **v2.1.0** (2017, 196 pp) | [GitHub mirror](https://github.com/Seaworth/resources/blob/master/wifi_p2p/Wi-Fi_Display_Technical_Specification_v2.1_0.pdf) | Verified genuine (`Author: Wi-Fi Alliance`). Useful where v2.3 deleted things — it still carries the Describe tables for UIBC Touch Up / Key Down / Key Up that v2.3 dropped. |
| Wi-Fi Display Technical Spec **v1.1** | [GitHub mirror](https://raw.githubusercontent.com/codenumb/miracast/master/Wi-Fi_Display_Specification_v1.1.pdf) | The version [MS-WFDPE] extends. |
| **[MS-WFDPE]** Wi-Fi Display Protocol Extension (v9.0, 2024, 39 pp) | [landing](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-wfdpe/) · [PDF](https://winprotocoldocs-bhdugrdyduf5h2e4.b02.azurefd.net/MS-WFDPE/%5bMS-WFDPE%5d.pdf) | Every `microsoft_*` and `intel_*` parameter, the `Server:` header that identifies Windows, and a dozen verbatim M2/M3/TEARDOWN exchanges in §3. **Freely redistributable** under the Microsoft Open Specifications terms. |
| **[MS-MICE]** Miracast over Infrastructure (v6.0, 48 pp) | [landing](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-mice/) · [PDF](https://winprotocoldocs-bhdugrdyduf5h2e4.b02.azurefd.net/MS-MICE/%5bMS-MICE%5d.pdf) | §1.10. Byte-exact message examples and SHA-256 PIN test vectors. |
| **HDCP Interface Independent Adaptation** Rev 2.2 / 2.3 | [digital-cp.com](https://www.digital-cp.com/sites/default/files/specifications/HDCP%20Interface%20Independent%20Adaptation%20Specification%20Rev2_3.pdf) | Free. Message IDs, the 7 ms locality budget, the `PES_private_data` layout. |
| Microsoft receiver requirements | [learn.microsoft.com](https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/wireless-projection-receiver-manufacturers) | The Required/Recommended/Optional checklist. Short and worth reading in full. |
| ETSI TS 103 544-17 (MirrorLink over WFD) | [etsi.org](https://www.etsi.org/deliver/etsi_ts/103500_103599/10354417/01.03.00_60/ts_10354417v010300p.pdf) | Freely published PAS that profiles WFD; useful corroboration. Returns 403 to some automated fetchers — use a browser UA. |

IETF/ISO, all verified reachable: [RFC 2326](https://www.rfc-editor.org/rfc/rfc2326.txt) (RTSP 1.0
— WFD uses 1.0, **not** RFC 7826's 2.0), [RFC 3550](https://www.rfc-editor.org/rfc/rfc3550.txt)
and [3551](https://www.rfc-editor.org/rfc/rfc3551.txt) (RTP + payload types),
[RFC 2250](https://www.rfc-editor.org/rfc/rfc2250.txt) (**MPEG1/2 over RTP — this is the WFD
payload format**), [RFC 4566](https://www.rfc-editor.org/rfc/rfc4566.txt),
[RFC 2234](https://www.rfc-editor.org/rfc/rfc2234.txt) (ABNF — the spec cites this, not 5234).
ISO/IEC 13818-1 is paywalled; ITU-T [H.222.0](https://www.itu.int/rec/T-REC-H.222.0) is the
technically identical text and is free.

The **Wi-Fi Direct / P2P Technical Specification** is membership-gated with no free official copy.
In practice hostap's `src/p2p/p2p.c` + `src/common/ieee802_11_defs.h` plus Wireshark's
`packet-wifi-p2p.c` are a *better* reference than the PDF, and all are freely licensed.

### 8.2 Implementations worth mining

Licensing matters here. **openwfd (MIT), hostap (BSD-3), sigma-dut (Clear BSD) and AOSP
(Apache-2.0) may be studied closely and adapted.** MiracleCast (LGPL-2.1), gnome-network-displays
(GPL-3.0), gst-rtsp-server-wfd (LGPL-2), Wireshark (GPL-2+) and FFmpeg (LGPL-2.1+) are **read for
wire behaviour, reimplement** under ground rule 9. `lazycast` (GPL), `benzea/miracast` and
`ivygroup/miracast-sink` (no license) are read-only.

#### AOSP `frameworks/av` — the best single source

Google removed `wifi-display` in Android P, so pull an old tag. The `aosp-mirror` GitHub org does
**not** carry `platform_frameworks_av`; use googlesource, which serves per-directory tarballs:

```
curl -O "https://android.googlesource.com/platform/frameworks/av/+archive/refs/tags/android-7.1.2_r36/media/libstagefright/wifi-display.tar.gz"
```
(verified working; ~49 kB, 21 files, Apache-2.0. Also works for `android-6.0.1_r81`.)

| File | Why |
|------|-----|
| `VideoFormats.cpp` | **The complete CEA/VESA/HH tables** (lines 28–134), the `native` byte packing, profile/level ↔ `profile_idc`/`constraint_set`, the `getFormatSpec` printf, and `PickBestFormat` — the exact algorithm Android uses to choose a mode. |
| `source/WifiDisplaySource.cpp` | The M1/M3/M4/M5/M16 emitter and the M3-response parser, including the `sscanf` strings for `wfd_client_rtp_ports` and the `wfd_content_protection` validator. 1739 lines. |
| `Parameters.cpp` | The 92-line `text/parameters` parser — the reference for lexical tolerance. |
| `source/TSPacketizer.cpp` | The TS/PES muxer: PAT/PMT, stream types, PCR, the HDCP descriptor. |
| `rtp/RTPSender.cpp`, `rtp/RTPBase.h` | RTP framing; `kMaxUDPPacketSize`/`kMaxNumTSPacketsPerRTPPacket`. |
| `MediaSender.cpp` | The HDCP `PES_private_data` bit-stuffing (lines 451–489). |

**The sink-side files exist only at `android-4.2.2_r1`** — `sink/WifiDisplaySink.cpp`,
`sink/RTPSink.cpp` (the jitter buffer / reorder logic), `sink/TunnelRenderer.cpp` (depacketize →
render, with the drop-late-frames policy), `sink/LinearRegression.cpp` (clock-skew estimator).
These four are the closest thing to a working reference for our data plane. A convenient vendored
copy of that whole tree is at [ivygroup/miracast-sink](https://github.com/ivygroup/miracast-sink)
(`native/wifi-display/`), unlicensed — read only.

#### MiracleCast — the reference sink

<https://github.com/albfan/miraclecast>, LGPL-2.1, C, **actively maintained** (verified HEAD
`0b7f1f1`, 2026-03-10).

| Path | Content |
|------|---------|
| `src/ctl/ctl-sink.c` | The sink's M1–M7 handling; `check_and_response_option()` is the "answer only what was asked" pattern. |
| `src/shared/rtsp.c` | A 3274-line incremental RTSP tokenizer with a printf/scanf-style DSL. Read before writing ours. |
| **`test/test_rtsp.c`** | A 712-line table of `{parsed struct, raw wire bytes, malformed variants}` — **directly portable to Rust `#[test]` cases**. |
| `src/ctl/wfd.c` | CEA/VESA/HH index → (hres, vres, fps) tables. |
| `src/uibc/miracle-uibcctl.c` | UIBC packet emitters (correct for touch/key; see §5.4 for the zoom/scroll bugs). |
| `src/ctl/sinkctl.c:82-84` | Default advertised masks. |
| `src/ctl/wifictl.c:148` | The literal WFD IE fixture `000600111c4400c8`. |

#### lazycast — a working sink you can read in an afternoon

<https://github.com/homeworkc/lazycast>, GPL, Python, ~550 lines in `d2.py`. The most compact
complete sink in existence, and it works against Windows 10/11 and Android on a Raspberry Pi:
the exact M3 response Windows accepts (lines 224–267), the six HIDC UIBC registration blobs
(313–319) and runtime report frames (134–172), plus `newmice.py`/`mice.sh` for MICE.

#### openwfd — the only MIT-licensed WFD IE definitions

Original repo `dvdhrm/openwfd` is **deleted (404)**. Canonical:
<https://cgit.freedesktop.org/~dvdhrm/openwfd/>; working GitHub mirrors
[sunshinemyson/openwfd](https://github.com/sunshinemyson/openwfd) and
[heguolun/OpenWFD](https://github.com/heguolun/OpenWFD) (byte-identical tarballs). Abandoned ~2014,
but **MIT/X11 licensed**, which nothing else here is.

`src/openwfd/wfd_defs.h` (520 lines) is exactly what a Rust `bitflags!` layer needs —
`OPENWFD_WFD_IE_OUI_1_0 0x506f9a0a`, `..._DATA_MAX 251`, the role/availability/WSD/CP masks, the
default port, and complete CEA/VESA/HH masks. `tools/openwfd_ie.c` is an IE encoder/decoder.
`test/test_rtsp.c` has nasty edge-case fixtures — quoted header names with embedded escapes and
newlines. Steal those.

#### gst-rtsp-server-wfd — the most complete `wfd-kv` parser

<https://github.com/albfan/gst-rtsp-server-wfd>, LGPL-2, dead since 2014. Note **upstream
GStreamer has no WFD code at all** — every `gst-plugins-bad/gst-libs/gst/wfd/` path 404s. The file
you want is in this Samsung/Tizen-derived fork of `gst-rtsp-server`:

`gst/rtsp-server/gstwfdmessage.c` — **2258 lines handling every parameter in the spec** in one
`else if` chain, with a matching serializer. If you port one file, port this one (as a Rust enum
with one variant per parameter). `gstwfdmessage.h` (660 lines) has each parameter as a C struct;
`rtsp-client-wfd.c` (1865 lines) is the M1–M7 state machine.

#### gnome-network-displays — a modern, maintained conformance oracle

<https://gitlab.gnome.org/GNOME/gnome-network-displays>, GPL-3.0, C, active (HEAD `10c2dd6`,
2026-06-10). It is a **source**, so its `wfd_params_from_sink()` is the decoder for exactly the M3
responses our sink will emit — a free conformance oracle.

`src/wfd/wfd-params.c` (the M3 response parser, plus `microsoft_cursor` which is in no spec),
`src/wfd/wfd-audio-codec.c` (the ×5 ms latency unit), `src/wfd/wfd-video-codec.c` (a second copy of
the resolution tables — **has transcription bugs**, use AOSP's as canonical),
`src/nd-wfd-mice-*.c` (MICE, listening on 7250).

#### wpa_supplicant / hostap — WFD IE assembly and all of P2P

<https://w1.fi/cgit/hostap/plain/...> (HTTPS cgit works; `git://w1.fi/hostap.git` is firewalled).
Mirror: [vanhoefm/hostap-wpa3](https://github.com/vanhoefm/hostap-wpa3). BSD-3.

`src/common/ieee802_11_defs.h` — the WFD constants and subelement enum, the P2P attribute IDs, the
device/group capability bitmaps. `wpa_supplicant/wifi_display.c` — WFD IE composition per frame
type, and the documented subelement set per frame (lines 105–132). `src/p2p/p2p_go_neg.c:21-33` —
the GO-intent winner logic. `src/p2p/p2p_group.c:283-308` — the 251-byte IE fragmentation.
The control-interface commands we will drive are `SET wifi_display 1` and
`WFD_SUBELEM_SET <id> <hex>`.

#### Wireshark — a decoding oracle

`epan/dissectors/packet-wifi-display.c` (651 lines) — it does exist. ~60 `hf_wfd_subelem_*`
registrations enumerate every bit of the Device Information and Extended Capability fields; that is
a ready-made Rust bitfield spec, and it cross-validates the spec tables in §1.4/§1.5.
`packet-wifi-p2p.c` (1865 lines) covers the P2P IE. `packet-rtsp.c` has only
`#define RTSP_TCP_PORT_RANGE "554,8554,7236"` — **there is no dissector for the `wfd-kv` body**, so
Wireshark will not decode M3/M4 payloads for you.

#### sigma-dut — the certification harness

<https://github.com/qualcomm/sigma-dut> (the old `qca/sigma-dut` 301-redirects here), Clear BSD,
**active** (last push 2026-07-23). `miracast.c` `dlsym`s a proprietary vendor `.so` so it has no
RTSP strings, but it does give certification-blessed WFD IE values with a bit-decomposition comment:

```c
/*  WFD Source IE: 000601101c440036 ... WFD Sink IE: 000601511c440036 */
wpa_command(intf, "SET wifi_display 1");
wpa_command(intf, "WFD_SUBELEM_SET 0 000601511c440036");   /* sink */
wpa_command(intf, "WFD_SUBELEM_SET 11 00020001");
```

It also ships annotated raw-frame fixtures — `probe_req_P2P_Wildcard.txt`, `probe_req_wildcard.txt`,
`P2P_device_discovery_req.txt` — hex-byte-per-line with `#` comments, already fixture-shaped.

#### Also worth a look

`benzea/miracast` (Python, dead 2018, unlicensed) names the states explicitly — read it first, then
read the C. FFmpeg `libavformat/mpegts.c` is the pragmatic ground truth for TS demux.

#### Verified negatives — do not budget time

* **No Rust crate exists for Miracast/WFD.** crates.io searched for `miracast`, `wifi-display`,
  `wfd`, `uibc` — zero hits (`wfd` is a Windows file-dialog crate).
* **openscreen has nothing Miracast-related.** `github.com/google/openscreen` 404s; the project is
  at <https://chromium.googlesource.com/openscreen> and is Google Cast + OSP only.
* **Upstream GStreamer has no WFD code.**
* **`aosp-mirror/platform_frameworks_av` does not exist.**

Useful Rust crates for the media layer, if we prefer them to FFI: `mpeg2ts-reader` 0.18.2
(best-maintained pure-Rust TS parser), `mpeg2ts` 0.6.0 (decode + encode), `flowly-mpegts`,
`m2ts-packet`.

### 8.3 Packet captures — the weak link

**No public Miracast/WFD pcap could be found.** Checked and confirmed empty: the
[Wireshark SampleCaptures wiki](https://wiki.wireshark.org/SampleCaptures), `wireshark/test/captures/`,
[vanhoefm/wifi-example-captures](https://github.com/vanhoefm/wifi-example-captures) (WPA3/SAE/FT
only). packetlife.net was unreachable.

Useful **adjacent** captures for exercising the layers in isolation (all verified reachable):

* [`rtsp_with_data_over_tcp.cap`](https://wiki.wireshark.org/uploads/__moin_import__/attachments/SampleCaptures/rtsp_with_data_over_tcp.cap)
  — RTSP with interleaved binary data over TCP; same framing shape as WFD.
* [`mpeg2_mp2t_with_cc_drop01.pcap`](https://wiki.wireshark.org/uploads/__moin_import__/attachments/SampleCaptures/mpeg2_mp2t_with_cc_drop01.pcap)
  — MPEG2-TS **with continuity-counter drops**. Exactly the failure mode a sink must survive.
* [`rtp_example.raw.gz`](https://wiki.wireshark.org/uploads/__moin_import__/attachments/SampleCaptures/rtp_example.raw.gz)

**Capture is therefore an RE deliverable.** The practical path is two captures, which happens to
line up with the two test tiers in ground rule 6:

1. **Monitor mode during P2P discovery** → the 802.11 WFD IE fixtures.
2. **Once mirroring starts on Windows, a virtual adapter ("Microsoft Virtual WiFi …") appears —
   capture on that** and you get RTSP + RTP + TS in the clear with no monitor mode needed.

### 8.4 Patents as a cross-check

Google Patents renders WFD patents as searchable HTML with no paywall, and LG/Samsung/Qualcomm
routinely reproduce spec tables verbatim. Useful when a PDF's table extraction is mangled — but
never as a primary source, since they describe the version current at filing and sometimes describe
the applicant's *proposal* rather than the ratified spec.

* [US10051673B2](https://patents.google.com/patent/US10051673B2/en) — **the best IE oracle.**
  Reproduces the full subelement ID table and the Device Information bit interpretations.
* [EP3104551A1](https://patents.google.com/patent/EP3104551A1/en) — the M1–M9 message table.
* [WO2016048065A1](https://patents.google.com/patent/WO2016048065A1/en) — M10/M11/M12 semantics.
* [EP2803242A1](https://patents.google.com/patent/EP2803242A1/en) — Device Information subelement
  structure.

> Beware [US20130013318A1](https://patents.google.com/patent/US20130013318A1/en) (Qualcomm, UIBC):
> it publishes an **earlier draft** Generic Input Type ID table shifted by one. See §5.4.

### 8.5 Fixture inventory

**Tier 1 — pure-protocol, check these in now:**

1. **`wfd-kv` bodies.** Lift every literal `wfd_*:` line from Miracast v2.3 §6.4 and from
   [MS-WFDPE] §3 — roughly 40+ real M3/M4 bodies, both freely copyable for implementation purposes.
2. **RTSP framing edge cases.** Port the tables in `miraclecast/test/test_rtsp.c` and
   `openwfd/test/test_rtsp.c`: line-ending variants (`\r\n`, `\r\r`, `\n\n`, `\n\r\n`, `\n\r`),
   leading/interior whitespace, quoted-and-escaped header names, `Content-Length` handling.
3. **WFD IE blobs**, round-trip encode/decode: `000600111c4400c8` (MiracleCast/lazycast),
   `000601511c440036` and `000601101c440036` (sigma-dut, certification-blessed).
4. **P2P probe frames** from sigma-dut — already annotated hex.
5. **`wfd_video_formats` strings** — every row of the table in §3.2 must parse, and ours must
   re-emit byte-identically.
6. **UIBC frames** — the six lazycast HIDC registration blobs and the live mouse/keyboard reports
   (§5.6). Real bytes a shipping Windows source accepts.
7. **MS-MICE messages** — the four hexdumps and the two SHA-256 PIN vectors in §1.10.
8. **A negotiation oracle** — port AOSP's `PickBestFormat` scoring so we can *predict* what Android
   will choose from a given advertisement, and assert it in tests.

**Tier 2 — needs capture:**

9. A real Windows sender M1→M7 exchange (validates `wfdx_video_formats`, `microsoft_*`).
10. A real Android sender M1→M7 exchange.
11. An RTP/MPEG-TS stream with real jitter and CC drops; `mpeg2_mp2t_with_cc_drop01.pcap` is a
    stand-in until then.
## 9. How this lands in our workspace

Mapping onto architecture-substrate.md §2 and the ground rules.

### 9.1 Crate split

```
substrate-rtsp     shared RTSP 1.0 framing: request/response parse+emit, header map,
                   CSeq bookkeeping, incremental byte-stream decoder.
                   Shared with proto-airplay — but ONLY the framing.
proto-miracast     the WFD state machine, wfd-kv parameter types, the M1..M16 sequence,
                   UIBC codec. Owns its own semantics; depends on substrate-rtsp,
                   substrate-rtp, core.
substrate-rtp      RFC 3550 header parse/emit + jitter buffer. Shared with proto-airplay.
substrate-mpegts   TS/PES demux. Miracast-only today; a separate crate because it is a
                   self-contained parser with its own fixture corpus.
crypto-hdcp        only if we ever do content protection. See §6 — we probably never do.
```

Note the deliberate asymmetry with AirPlay: **the RTSP *framing* is shared, the RTSP *dialect* is
not.** AirPlay's RTSP has binary plist bodies, a `CSeq` that only counts one way, and its own
method set; WFD has `text/parameters` bodies and bidirectional client roles. Trying to share a
"RTSP session" type across both is exactly the mistake ground rule 2 forbids. What is genuinely
common is: request line / status line / header block / `Content-Length` body, and the incremental
decoder that turns a TCP byte stream into whole messages.

### 9.2 Typestate for the M-sequence

The M1→M7 handshake is the textbook case for ground rule 1. Sketch:

```rust
struct Session<S: State> { io: SessionIo, common: Common, state: S }

struct AwaitingM1;                       // nothing negotiated
struct AwaitingM3 { peer_methods: MethodSet }
struct AwaitingM4 { advertised: SinkCapabilities }
struct AwaitingTrigger { negotiated: NegotiatedConfig }   // has presentation_url, resolution, codec
struct Setup { negotiated: NegotiatedConfig, session_id: SessionId }
struct Playing { negotiated: NegotiatedConfig, session_id: SessionId, rtp: RtpBinding }
```

`Session<AwaitingM3>` has no `session_id` field to misuse; only `Session<AwaitingTrigger>` exposes
`fn send_setup(self) -> Session<Setup>`, so you cannot emit M6 before M4 has landed. The `Playing`
state is the only one carrying an `RtpBinding`, so the depacketiser cannot be started early.

`NegotiatedConfig` must be constructible **only** by the M4 handler, from the intersection of what
we advertised and what the source chose — which makes "we started decoding at a resolution we never
advertised" unrepresentable.

### 9.3 Parameter types, not a string map

Every parameter in §3 gets a newtype with `FromStr` + `Display` and a `thiserror` variant:

```rust
enum WfdParam {
    VideoFormats(VideoFormats),          // native, profile bitmap, level bitmap, 3 masks, ...
    AudioCodecs(Vec<AudioCodec>),
    ClientRtpPorts(ClientRtpPorts),      // enforces port1 == 0 at construction
    ContentProtection(ContentProtection),// None | Hdcp2_0{port} | Hdcp2_1{port}
    TriggerMethod(TriggerMethod),        // Setup|Play|Pause|Teardown — exhaustive
    ...
    Unknown { name: String, value: String },   // must exist: vendors invent parameters
}
```

`ResolutionIndex` should be a newtype over `(Table, u5)` that can only be built from a valid table
entry, so `CEA index 20` (undefined in R1) cannot be constructed. The `native` byte's
`(index << 3) | table` packing lives in exactly one `impl`.

The M3 *response builder* should be a struct with one field per parameter we can answer, and a
method that takes the source's requested-name list and emits only the intersection — mirroring
MiracleCast's `check_and_response_option()` but with the "did we forget one" check moved to the
type system.

### 9.4 What is fixture-testable without hardware

Everything in §2, §3, §4, §5 — that is, *all of the protocol*. Concretely, the pure-core tests
should cover:

* M1→M7 as a scripted transcript: feed the recorded source-side bytes in, assert the exact bytes
  out. The transcripts in §2.5 and the [MS-WFDPE] §3 examples are ready-made cases.
* `wfd_video_formats` round-trip: every real string in §3.2's table must parse, and re-emit
  byte-identically where it is one of ours.
* Negotiation: given a sink capability set and a source's M4, assert the chosen mode. Port AOSP's
  `PickBestFormat` scoring as an oracle so we can predict what Android will pick.
* RTP → TS → PES → Annex-B extraction from a captured `.rtpdump`/pcap of a real session.
* UIBC encode: the touch/key frames in §5 have known-good hex from lazycast to diff against.

The only things needing the real box are the Wi-Fi Direct group formation itself and the decode/
render path — exactly the isolation ground rule 6 asks for.

### 9.5 Threading

Per architecture-substrate.md §6: the RTSP actor and the RTP socket are tokio tasks; the TS demux
is cheap enough to stay on the runtime; H.264 decode goes to `spawn_blocking` or the pipeline's
decode thread. For live mirroring **drop late frames** — a Miracast sink that buffers to smooth
jitter has missed the point. The `wfd_av_format_change_timing` PTS is the one place where we must
*not* drop: it is the resync point.

### 9.6 Where the risk actually is

Ranked:

**Roughly 70 % of the risk in this protocol is not in the protocol.**

| Area | Estimate | Confidence |
|------|----------|-----------|
| RTSP/WFD sans-I/O core + parameter types + TS/RTP depacketiser, fixture-tested | **2–4 weeks** | High — bounded, and the fixtures already exist (§8.5) |
| MS-MICE path (mDNS + 7250 actor + 6 messages + 3 TLVs) | ~1 week | High |
| Linux Wi-Fi Direct backend (autonomous GO, WPS, DHCP, persistent groups) | **4–8 weeks** | Low — variance is almost entirely hardware |
| Windows platform story | unknown until the §7.7 spike | Very low |
| Decode robustness (15 s IDR gaps, mid-stream format change, loss recovery) | 1–2 weeks | Medium |

Ranked risks:

1. **Being discoverable at all.** Wi-Fi Direct autonomous GO with a WFD IE, on a chipset whose
   driver genuinely supports P2P GO (§7.6), with our own `wpa_supplicant` because NetworkManager
   cannot do it. Add our own DHCP server (GO) — neither NM nor dhcpcd cooperates, which is why both
   MiracleCast and lazycast ship their own.
2. **The Windows deploy target** (§7.7). Genuinely a "the answer might be no" question, unlike
   everything else here. **Spike it first** — a day or two of work that determines whether
   Miracast is cross-platform or Linux-only for this project.
3. **Interop long tail.** No conformance oracle exists; §7 is what a decade of community debugging
   produced and it will keep growing. Ground rules 1, 3 and 6 are unusually well matched to this:
   parse-don't-validate at the byte boundary, a sans-I/O core, and fixtures instead of hardware are
   exactly what turns this from a debugging treadmill into a growing regression suite.
4. Everything else. The protocol work is bounded and well-understood.

**Explicitly out of scope for a first cut:** HDCP 2.x (§6), UIBC (ship after touch works —
MiracleCast's is broken and Android never had it), all `microsoft_*` extensions (answer `none`),
`wfd2_*`/R2, and hardware cursor.

**Suggested order:** §7.7 spike → sans-I/O core against fixtures → MICE → Linux P2P behind the
trait → integration tests in a `nixosTest` VM against a scripted sender, before any real hardware.
