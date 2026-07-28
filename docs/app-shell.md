# The app shell — screens, navigation, and getting home

The design record for GitHub #23, and the thing eight other issues are waiting on. Read
this before changing `pipeline`'s layer model, the kiosk input router, or anything that
decides what is on the glass.

Companion to DECISION-LOG **D38**, which records the calls and what they cost.

---

## 1. The problem, stated properly

Every protocol in this project so far is one a *sender* initiates. Someone picks the panel
on their phone, and the receiver's job is to accept whatever arrives. The panel never has
to ask a question, so it has never needed a way to ask one.

GameStream broke that (D37): the panel is the client, so *it* has to choose which host to
dial and which app to launch. There is nowhere to make that choice from, so the choice
lives in `castaway.toml` — which makes the one protocol that could be walk-up the only one
that is not. That is backwards, and it is the immediate reason this document exists.

It is not a GameStream problem. The same wall is in front of local media (#11, "file picker
or library?"), output-device selection (#12, "GUI configurable"), the intercom view (#29),
the visualizer (#15), and any future protocol where the panel offers rather than accepts.
And the inverse — *leaving* a thing that is on screen — has no answer either: #27 wants an
idle YouTube app to "kick back to home screen", and there is no home screen to kick back to.

### What exists today

One interactive surface: the transport strip (D33), and only while a session publishes
metadata. Everything else on the panel is a picture.

The idle scene is a 4K RGBA bitmap rasterised **once at startup** and installed as a
background layer. Every row on it is an instruction aimed at your phone — "Cast →
dma.space/screen#cast". There is no command that can change it, so it cannot react to
anything: not a host appearing, not a protocol going down, not the time of day.

"What is on screen" is not modelled anywhere. It is an emergent property of which
compositor layers currently exist, and the closest thing to a mode in the tree is
`BrowserRole`, which has two variants and describes only the browser.

### The framing this builds to

> The attract screen tells you what to do **from your phone**. The home screen adds what
> you can do **from the glass**.

Cast, AirPlay, DLNA and Spotify stay instructions — there is nothing to tap, because the
tap happens on your device. GameStream, local media, the intercom and the visualizer
become tiles, because the panel is the thing that has to act.

---

## 2. What the shell is

A **screen stack** with exactly one owner of the glass, living in `pipeline` next to the
render loop, driven by models over the existing `RenderCommand` channel.

Three rules it exists to enforce:

1. **One thing owns the screen at a time, and it is explicit.** Today preemption is
   implicit in z-order and a `set_screen_release` callback bolted on after the fact (D28's
   fix for YouTube owning the panel forever). The shell makes "who has the glass" a value
   you can read, log, and test.
2. **Every screen is reachable and leaveable.** A screen that can be entered and not left
   is a bug the shell should make unrepresentable — which is what the return-to-home
   gesture and the idle policy are for.
3. **Screens are models, not pixels.** `RenderCommand::NowPlaying` already sends a few
   hundred bytes of metadata and rasterises on the render thread at the true surface size.
   Every screen follows that; nothing ships a 33 MB buffer down a channel.

### Screens, concretely

- **Home** — identity, the instruction rows that exist today, and a row of tiles for
  things the panel can start. Replaces the baked attract bitmap.
- **Picker** — a list with a title and a back affordance. Generic over what it lists:
  GameStream hosts, then that host's apps, then later media files and output devices. This
  is the screen that closes Q44.
- **Session** — something is playing. Video, or the now-playing card plus transport strip.
  Roughly what exists today.
- **Browser** — a cast surface (YouTube leanback, Cast app surfaces) filling the panel.

A stack rather than a set, because "back" has to mean something: Home → Picker(hosts) →
Picker(apps) → Session, and back pops.

### What it is not

Not a widget toolkit, and not a general layout engine. Screens are hand-composed the way
the now-playing card is, using the same primitives. If that becomes painful, the answer is
more primitives, not a framework.

---

## 3. Why native, not a web page

Recorded in D38; the short version is three facts:

There is exactly **one browser instance**, and it is spoken for. YouTube leanback lives in
it, and Cast app-native surfaces (#16) will too. A web shell would contend with cast
content for the same window, and the arbitration between "the UI" and "the content" would
have to be invented anyway — in a place where a page crash takes the navigation with it.

The **portable build has no browser at all** and must still have a UI. `castaway-portable`
is what CI builds and what runs where Electron cannot.

The **drawing primitives already exist**, which was the main argument against native and
turns out not to hold. `transport.rs` contains an antialiased signed-distance-field
rasteriser — circle, rounded box, segment, triangle, resolution-independent — and
`nowplaying_card.rs` can decode an arbitrary image and draw it centre-cropped into a
square. Between them that is most of an icon-and-tile toolkit. Both are private; the work
is promoting them into `text.rs`, not writing them.

The cost is honest and worth stating: no CSS, no layout engine, no free animation curves,
and #24's mascot-and-flourish ambitions are more work than they would be in a page.

---

## 4. Getting home from a fullscreen browser

The hard part, and the one with a landmine in it.

Touch today goes: winit → `KioskApp::route_input` → the transport strip gets first refusal
→ everything else is forwarded to Electron over CDP, unconditionally. There is no
interception stage and no gesture recognition on the Rust side by design ("Chromium does
its own gesture recognition on the far side").

### The gesture: a swipe and a pill

**Swipe in from the left edge** returns to Home. Left, not bottom: the transport strip
already claims the bottom-centre 62% × 20% of the glass and takes touches before anything
else, so a bottom-edge gesture would fight it.

**A home pill** appears on any touch and fades after a few seconds. The pill is for the
person who has never used the panel — a hackerspace screen is used by guests, and a
discoverable affordance matters more than an elegant one. The gesture is for when the pill
is in the way.

Both, not either: the pill is discoverable and the gesture is fast, and they cost the same
hit-test.

### The landmine: stolen contacts must be cancelled

If the shell claims a contact mid-drag, Electron has **already received `touchStart`** and
will never receive an end. The browser host keeps that contact in its point map forever,
so the page believes a finger is still down — permanently, for the life of the session.

Every id the shell steals must therefore be followed by a synthesised
`ToBrowser::Touch { phase: Cancel }`. The plumbing exists end to end and maps to
Chromium's `touchCancel`, but **nothing has ever sent it**, so it is untested. This is the
first thing to write a test for, not the last.

### Where it hooks in

`KioskApp::route_input` is the only viable place: it is upstream of both the transport
strip and the browser sink, and it already sees every phase including moves — which
`offer_to_transport` deliberately does not.

`offer_to_transport`'s contract is the precedent to copy, including its subtlety:
*consuming is separate from acting.* A touch that lands in the gesture zone but does not
complete a swipe must still be swallowed rather than falling through to a page underneath.

Two things must be built that do not exist:

- **A contact table.** Nothing on the Rust side tracks simultaneous touches; ids are
  carried faithfully to the browser and then forgotten. A swipe needs per-contact start
  position, start time and travel.
- **A frame-time-driven animator.** There is no frame clock anywhere and no delta-time;
  the loop runs uncapped with Mailbox present mode. Transitions must be driven from
  `Instant`, not from frame counts.

---

## 5. What has to change underneath

### Layer identity is the risky part

`LayerId` is a **closed six-variant enum**, and layers are drawn by sorting on `z`.
`NowPlaying` and the browser's attract-widget role **already collide at z = -5**, with the
tie broken by `HashMap` iteration order — nondeterministic, and harmless today only
because those two never coexist in practice.

A shell adds surfaces, so this needs reworking rather than extending, and it should land
**before** any screen is built on top of it. A fullscreen browser sits at z = +5, above
everything but the OSD, so the navigation affordance needs to be above that.

### The attract scene becomes a live screen

Add a `RenderCommand` variant carrying an `AttractScene`-shaped model and rasterise on the
render thread, exactly as `NowPlaying` does. This also fixes a latent bug: the attract
image is currently baked at a hardcoded 3840×2160 and GPU-upscaled to whatever the panel
actually is, and is never re-rendered on resize.

### Two bugs found while mapping this, neither caused by it

- **The browser's coordinate mapper clamps out-of-rect touches instead of rejecting
  them.** On the idle screen, where the browser is a small clock card in the corner, a
  touch anywhere on the 65-inch panel is squashed into that card and delivered to the clock
  page. `proto-miracast`'s `map_from_panel` returns `None` outside its rect and is the
  correct model.
- **`transport_owns` does not know whether the strip is visible.** It answers from the
  layout rect and whether a card exists, not from whether anything is covering it. The
  strip renders *below* video, so a video session that also publishes metadata would leave
  it invisible under the picture while still swallowing that part of the glass. Not proven
  to be reachable — it needs a test before it needs a fix.

### And one gap worth knowing

`input-touch`'s `evdev` and `winuser` backends are **empty feature flags with no code**.
Every touch today arrives through winit. That is fine on the dev box and unproven on the
Dell panel, whose touch arrives over USB HID.

---

## 6. Order of work

Each step is meant to be independently landable and independently useful.

1. **Layer identity + z-ordering rework.** No visible change. Deterministic ordering, room
   for shell surfaces above a fullscreen browser.
2. **The screen model, and Home as a live screen.** Replaces the baked bitmap; fixes the
   resize bug. Still no navigation — Home is simply what the idle screen becomes.
3. **Tiles and the picker.** Home gets tappable tiles; the picker screen exists and can
   list things.
4. **The GameStream picker.** First real consumer: hosts → apps → stream. Closes Q44 and
   makes #33 walk-up.
5. **The home gesture.** Swipe, pill, contact table, and the cancel-synthesis test.
6. **Transitions.** The animator, and slide/fade between screens.
7. **PiP** (#28), which unlocks the intercom (#29).
8. **Idle policy** (#27): nothing playing and nobody connected returns to Home.

Theming (#24) threads through 2–6 rather than being a phase. Brand assets are already
vendored at `crates/pipeline/assets/brand/` with their provenance recorded.

## 7. How this gets tested

The same way the rest of the renderer is, and the harness already exists: layout and
hit-testing are pure and unit-tested without a GPU, and `attract_preview` /
`screen_preview` / `card_preview` dump composed surfaces to PNG for a human to look at.
Every screen gets both.

The parts that genuinely need care, because they are the ones that fail silently:

- **A screen you can enter and not leave.** The stack should be exercised as a state
  machine — every screen reachable, every screen leaveable, back always terminating at
  Home.
- **Cancel synthesis.** Assert that stealing a contact mid-drag sends a cancel for that
  id, because the failure is a page that thinks a finger is down forever and looks like a
  broken website.
- **Hit-test versus draw agreement.** D33's rule: a control cannot be drawn where it
  cannot be pressed, or pressed where nothing is drawn. One layout serves both.
