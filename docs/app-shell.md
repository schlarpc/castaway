# The app shell — screens, navigation, and getting home

The design record for GitHub #23, and the thing eight other issues are waiting on. Read
this before changing `pipeline`'s layer model, the kiosk input router, or anything that
decides what is on the glass.

Companion to DECISION-LOG **D38**, which records the calls and what they cost.

**Status: built.** §6 lists what landed and what did not. The rest of this document is
written as the design it was, because the reasoning is still why the code looks like this
— where the shipped thing differs, §6 says so.

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

*As built, this got simpler:* everything is a tile. What differs is what a tile does. A
receiver protocol's tile opens a screen telling you what to tap on your own device; a
client protocol's tile sends the panel off to do something. The distinction is one
`Option` on the tile, and the shell answers the first kind without a round trip.

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

- **Home** — identity and a tile per service. *Changed during the build:* the plan kept
  the per-service instruction rows and added tiles beside them. Seeing it rendered settled
  it — six protocols' worth of instructions was the first thing anyone saw and none of it
  was about what they were doing. The rows are gone and every service is a tile.

  *Laid out once, twice corrected.* One `layout()` places the title, tagline, heading,
  tiles and footer together; the renderer and the hit test both read it. The first version
  laid the screen out twice — the text from one set of constants and the tiles from
  another — which lined up only for one particular title, and put the tile grid at an
  absolute height so a longer name silently closed the gap above it. Everything now shares
  one left edge and one rhythm measured down from the title.

  The grid gets a **fixed box** (under the heading, above the footer, left of the widget
  card) and solves columns and tile size to fit it, choosing the arrangement that makes
  tiles largest and breaking ties toward the fewest empty cells. The version before it
  used a constant tile size and grew downward: at seven tiles the third row started below
  the footer and ran off the bottom of the panel. Nothing on this screen scrolls, so the
  box is the constraint and the tiles are what gives.
  *The marks are vendored SVGs* (`crates/pipeline/assets/glyphs/`), rasterised into a
  coverage mask and tinted with the tile's accent. They began as hand-rolled distance
  fields, and every glyph that looked wrong was a geometry bug here rather than in any
  artwork — because there was no artwork. That is also the only way DLNA, Spotify and
  YouTube are the marks that exist rather than our impression of them.

  *The background palette* is `theme::ThemeChoice`, a config option defaulting to `auto`:
  the calendar picks, `plain` never decorates, and naming a season wears it all year. The
  calendar is Pride (June), Trans Day of Visibility (31 March), International Asexuality
  Day (6 April), Lesbian Visibility Week (22–28 April), Pansexual Pride Day (24 May),
  Non-Binary Awareness Week (8–14 July), Bisexual Awareness Week (16–23 September), Ace
  Week (22–28 October), Intersex Awareness Day (26 October), Halloween (30–31 October)
  and the twelve days of Christmas (25 December – 5 January).

  October holds three of those, so the calendar needs a precedence rule: **a single-day
  observance outranks a week it falls inside**, because the week has six other days to be
  seen on and the day has none. Intersex Awareness Day therefore wins the 26th from Ace
  Week, which contains it. Halloween is only the two days it is actually about — a week of
  it crowded out the rest of the month for one night.

  A season's hues are mixed only **22%** into the panel's own dark ramp, because a flag at
  full strength is a lightbox and every screen here is white text. That constraint is what
  decides which flags work: the ones carrying white, grey or black stripes — ace,
  non-binary, lesbian — give up most of their identity in the mix and land near the floor.
  Two tests hold both ends of it, one that no season is bright enough to lose white text
  on and one that none washes out below the chroma Halloween already ships at. A third
  walks the year to prove no `season()` arm shadows a later one, because a palette nobody
  can reach is a palette that exists only in the config file.

- **Service** — one service's instructions, opened by its tile. Did not exist in the plan;
  it is where the rows went.
- **Picker** — a list with a title and a back affordance. Generic over what it lists:
  GameStream hosts, then that host's apps, then settings and output devices (see §6
  item 12), later media files. This is the screen that closes Q44.
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
Chromium's `touchCancel`, but nothing had ever sent it.

*As built:* the reserved edge means the shell never steals a contact mid-drag — one that
starts there is never forwarded in the first place. But the case remains for fingers
already down when the panel is taken away, so `InputSink::cancel_all` exists and going
home calls it. `tests/home_gesture.rs` covers it.

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

## 6. What was built

Landed, in this order, each independently useful:

1. **Layer identity + z-ordering.** Paint order *is* `LayerId`'s declaration order; there
   is no depth to pass and so no collision to have. `ShellOverlay` sits above a fullscreen
   cast, which is what makes a way out of one possible.
2. **The screen model, and Home as a live screen.** `ScreenStack` with Home as a field
   rather than element zero, so "never empty" is structural. Fixed the resize bug on the
   way past.
3. **The shape kit**, promoted out of `transport.rs` so the shell could use it.
4. **Home as a launcher, and the screens behind it.** Per-service instructions moved off
   the idle screen and behind their tiles — six protocols' worth of them was the first
   thing anyone saw and none of it was about what they were doing.
5. **Input routing.** Transport, then shell, then browser; the middle only where nothing
   covers it.
6. **The Moonlight picker.** Tile → hosts → apps → streaming. Closes Q44. Pairing is
   part of the walk-up now: pressing an unpaired host shows a panel-generated PIN and
   waits for it to be typed into Sunshine's web UI — one pairing at a time, a
   three-minute panel-side timeout (the protocol layer rightly has none), and a retry
   row on failure. The config-driven startup pairing shares the same adapter call.
7. **The home gesture.** Left-edge swipe and a fading pill, with `cancel_all` so nothing
   is left holding a finger.
8. **PiP and the idle return** (#28, #27). Bringing the shell forward demotes a playing
   video to a corner instead of stopping it; an ending session returns Home unless someone
   is using the panel.

9. **Picker scrolling.** Scroll lives on the model in rows; a drag moves it; chevron hints
   say there is more. Only whole rows fully on screen are laid out.
10. **Theming (#24, partial).** `pipeline::theme` is one palette for every surface, with
    dma.space's accents. The typeface did not land — see below.
11. **Transitions.** A crossfade, and the home gesture drags it: part-way through a swipe
    the panel is part-way through the navigation, so letting go without finishing puts it
    back. A slide was the obvious choice and the compositor cannot do it — there is no
    clipping, so an incoming screen parked off-surface would be drawn across whatever is
    beside it. A crossfade needs only opacity, which is a uniform write rather than a
    33 MB re-raster per frame.
12. **Settings (D40, first slice of #12).** A gear tile, always last on Home; behind it a
    menu of settings, each drilling into a choice list — all pickers, so back, scrolling
    and transitions came for free. `PickerItem::marked` is the one primitive it added: a
    choice row is a selection, not a doorway, so the row in effect wears a check where the
    others wear the go-somewhere chevron. The catalog (`app::settings::Setting`) is the
    seam: `shell_nav` renders whatever the catalog describes and knows no setting by name,
    the way the picker knows nothing about GameStream. One setting exists — output
    device — and applying it writes `castaway.toml` back through `toml_edit` without
    disturbing a byte the operator wrote (D40 records why not serde).

The typeface stays DejaVu, deliberately: nobody reading a wall from across a room is
identifying the body face, and matching dma.space's Inter would mean vendoring static
instances (it is packaged only as a variable font, which `ab_glyph` cannot take a weight
axis from) and re-checking every surface's metrics for a difference invisible at that
distance. The brand is carried by the palette, the mascot and the wordmark.

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
