# The phone half of a YouTube cast, scripted — so "can someone cast a video to this
# thing?" is a command that exits 0 or 1 instead of a person with a handset.
#
# Not a `nix flake check`: unlike the tier-2 VM tests this one needs the real internet,
# because YouTube's Lounge servers are a third party to the session and there is nothing
# to fake them with. DIAL carries no media — every part of a YouTube cast after the
# launch happens between the phone, those servers, and the *page* the receiver opened —
# so a test that stops at "the receiver answered 201" tests almost none of it. This
# drives the whole path and asserts the one thing that matters: the screen plays.
#
#   nix run .#yt-selfplay -- http://<receiver>:8080
#
# What it does, in the order a phone does it:
#   1. read dd.xml for Application-URL, check the app is stopped
#   2. POST a launch carrying a `pairingCode` we invented
#   3. wait for the receiver's page to register that code with YouTube  <- the cliff
#   4. bind to the resulting Lounge session as a remote control
#   5. queue a video, and wait for the screen to report it actually rolling
#   6. do that twice more, with a dwell between, because "it played the first one"
#      and "it plays what I tap while I browse" are different claims
#
# Step 3 is where a browser-less receiver fails, and it fails *silently*: DIAL says 201
# and `running`, the phone shows a connected session, and nothing ever plays. That is
# the failure this exists to name out loud.
{ pkgs }:

pkgs.writers.writePython3Bin "yt-selfplay" { flakeIgnore = [ "E501" "W503" ]; } ''
  import json
  import random
  import re
  import sys
  import time
  import urllib.error
  import urllib.parse
  import urllib.request
  import uuid

  LOUNGE = "https://www.youtube.com/api/lounge"
  # Big Buck Bunny (Blender Foundation, CC-BY): long enough to still be playing at the
  # end of a dwell, and about as unlikely to be taken down as YouTube gets.
  DEFAULT_VIDEOS = "aqz-KE-bpKQ"
  # For --expect-skip: carries a community-submitted `music_offtopic` segment over its
  # first few seconds, so the skip fires immediately instead of minutes in. Segments are
  # crowd-sourced and can be re-voted, so a failure here is worth checking against
  # sponsor.ajay.app before believing it is ours.
  SEGMENTED_VIDEO = "9bZkp7q19f0"
  # Seconds left playing between taps, so the sequence looks like someone browsing
  # rather than three commands in a burst.
  DWELL = 20

  STATES = {"-1": "UNSTARTED", "0": "ENDED", "1": "PLAYING", "2": "PAUSED",
            "3": "BUFFERING", "5": "CUED"}


  def http(url, data=None, headers=None, timeout=30):
      request = urllib.request.Request(url, data=data, headers=headers or {})
      try:
          with urllib.request.urlopen(request, timeout=timeout) as response:
              return response.status, response.read().decode("utf-8", "replace")
      except urllib.error.HTTPError as e:
          return e.code, e.read().decode("utf-8", "replace")


  def form(fields):
      return urllib.parse.urlencode(fields).encode()


  def post_form(url, fields, timeout=30):
      return http(url, data=form(fields),
                  headers={"Content-Type": "application/x-www-form-urlencoded"},
                  timeout=timeout)


  def dial_launch(base, pairing_code):
      """Steps 1-2: the DIAL surface, as a YouTube sender reads it."""
      base = base.rstrip("/")
      status, body = http(base + "/dial/dd.xml")
      if status != 200:
          raise SystemExit("no DIAL device description at " + base + "/dial/dd.xml (HTTP {}).\n"
                           "A receiver with no kiosk browser does not advertise DIAL at all — "
                           "check the log for 'DIAL disabled', and build with --features electron.".format(status))
      print("dd.xml: ok")

      status, body = http(base + "/dial/apps/YouTube")
      if status != 200:
          raise SystemExit("GET /dial/apps/YouTube -> {}".format(status))
      print("app state before launch: " + ("running" if "<state>running</state>" in body else "stopped"))

      status, body = post_form(base + "/dial/apps/YouTube",
                               {"pairingCode": pairing_code, "theme": "cl"})
      if status not in (200, 201):
          raise SystemExit("launch failed: {} {}".format(status, body[:200]))
      print("launch: {} (pairingCode={})".format(status, pairing_code))

      status, body = http(base + "/dial/apps/YouTube")
      if "<state>running</state>" not in body:
          raise SystemExit("the app did not flip to running: " + body[:200])


  def wait_for_screen(pairing_code, timeout=90):
      """Step 3: has the receiver's page claimed our pairing code with YouTube?

      This is the whole ballgame. The receiver never talks to the Lounge itself — the
      page it opened does, and only if it was actually opened, actually loaded, and
      actually got our launch parameters."""
      deadline = time.monotonic() + timeout
      while time.monotonic() < deadline:
          status, body = post_form(LOUNGE + "/pairing/get_screen",
                                   {"pairing_code": pairing_code})
          if status == 200:
              screen = json.loads(body)["screen"]
              print("screen registered the pairing code after {}s: {}".format(
                  int(timeout - (deadline - time.monotonic())), screen["name"]))
              return screen
          time.sleep(3)
      raise SystemExit(
          "the screen never registered our pairing code.\n"
          "DIAL accepted the launch and reports `running`, but nothing claimed the "
          "session, so a phone would sit on a connected-looking cast that can never "
          "play. Either no page was opened (browser-less build), or it opened and "
          "failed to load youtube.com/tv (check the receiver's log and its network).")


  def parse_chunks(text):
      """`<len>\n<json array>` framing. Returns (commands, characters consumed), so a
      caller streaming the channel can keep a partial trailing chunk buffered."""
      out, i = [], 0
      while i < len(text):
          match = re.match(r"\s*(\d+)\n", text[i:])
          if not match:
              break
          length = int(match.group(1))
          start = i + match.end()
          if len(text) < start + length:
              break  # still arriving
          chunk = text[start:start + length]
          i = start + length
          try:
              entries = json.loads(chunk)
          except json.JSONDecodeError:
              break
          for entry in entries:
              if isinstance(entry, list) and len(entry) == 2 and isinstance(entry[1], list):
                  inner = entry[1]
                  out.append((entry[0], inner[0], inner[1] if len(inner) > 1 else None))
      return out, i


  class Lounge:
      """A sender-side BrowserChannel session: bind, send commands, read the screen's."""

      def __init__(self, lounge_token, name="castaway-selfplay"):
          self.token = lounge_token
          self.name = name
          self.device_id = str(uuid.uuid4())
          self.rid = random.randint(10000, 99999)
          self.sid = None
          self.gsession = None
          self.aid = 0
          self.ofs = 0

      def _params(self, extra=None):
          params = {
              "device": "REMOTE_CONTROL",
              "id": self.device_id,
              "name": self.name,
              "app": "youtube-desktop",
              "mdx-version": "3",
              "loungeIdToken": self.token,
              "VER": "8",
              "v": "2",
              "t": "1",
              "CVER": "1",
          }
          params.update(extra or {})
          return urllib.parse.urlencode(params)

      def connect(self):
          """Step 4: open the channel and learn our SID/gsessionid from the reply."""
          self.rid += 1
          url = LOUNGE + "/bc/bind?" + self._params({"RID": str(self.rid)})
          status, body = post_form(url, {"count": "0"})
          if status != 200:
              raise SystemExit("bind failed: {} {}".format(status, body[:300]))
          for command in parse_chunks(body)[0]:
              self._absorb(command)
          if not (self.sid and self.gsession):
              raise SystemExit("no session in the bind response: " + body[:300])
          print("lounge session bound")

      def _absorb(self, command):
          aid, name, payload = command
          self.aid = max(self.aid, aid)
          if name == "c" and isinstance(payload, str):
              self.sid = payload
          elif name == "S" and isinstance(payload, str):
              self.gsession = payload

      def send(self, command, **args):
          self.rid += 1
          url = LOUNGE + "/bc/bind?" + self._params({
              "RID": str(self.rid), "SID": self.sid,
              "gsessionid": self.gsession, "AID": str(self.aid),
          })
          body = {"count": "1", "ofs": str(self.ofs), "req0__sc": command}
          self.ofs += 1
          for key, value in args.items():
              body["req0_" + key] = str(value)
          status, text = post_form(url, body)
          if status != 200:
              raise SystemExit("{} rejected: {} {}".format(command, status, text[:300]))

      def listen(self, seconds, on_command):
          """Read the receive channel incrementally — it is a long poll that never
          closes on its own, so waiting for EOF waits forever. Stops early when
          `on_command` returns True; returns whether that happened."""
          url = LOUNGE + "/bc/bind?" + self._params({
              "RID": "rpc", "SID": self.sid, "gsessionid": self.gsession,
              "AID": str(self.aid), "CI": "0", "TYPE": "xmlhttp",
          })
          deadline = time.monotonic() + seconds
          buffered = ""
          try:
              stream = urllib.request.urlopen(url, timeout=10)
          except urllib.error.HTTPError as e:
              print("   (receive channel -> {})".format(e.code))
              return False
          while time.monotonic() < deadline:
              try:
                  chunk = stream.read1(8192)
              except Exception as e:  # noqa: BLE001 - an idle channel just times out
                  if isinstance(e, TimeoutError) or "timed out" in str(e):
                      continue
                  print("   (receive channel: {})".format(type(e).__name__))
                  return False
              if not chunk:
                  return False
              buffered += chunk.decode("utf-8", "replace")
              commands, consumed = parse_chunks(buffered)
              buffered = buffered[consumed:]
              for command in commands:
                  self._absorb(command)
                  if on_command(command):
                      return True
          return False


  def playing(lounge, video_id, timeout=60):
      """Step 5: did the screen actually start rolling *this* video?

      Two questions, because either one alone is satisfied by the wrong thing.
      `onStateChange` reports the transport but not which video, so a screen happily
      still playing the *previous* tap answers it — that is exactly the "I browsed and
      it kept playing the first thing" failure. So once it says PLAYING, ask it
      outright what is on screen and hold it to the video we queued."""
      deadline = time.monotonic() + timeout
      last, shown, seen_times = {}, None, []
      while time.monotonic() < deadline:
          answer = {}

          def answered(command):
              _aid, name, payload = command
              if name == "nowPlaying" and isinstance(payload, dict) and payload.get("videoId"):
                  answer.update(payload)
                  return True
              return False

          lounge.send("getNowPlaying")
          window = min(20, max(5, deadline - time.monotonic()))
          if not lounge.listen(window, answered):
              continue
          last = answer
          state = str(answer.get("state"))
          if (answer.get("videoId"), state) != shown:  # only when something changes
              shown = (answer.get("videoId"), state)
              print("   <- nowPlaying video={} {} t={}".format(
                  answer.get("videoId"), STATES.get(state, state), answer.get("currentTime")))
          if answer.get("videoId") != video_id:
              continue  # still switching; a BUFFERING answer for it is on the way
          # The test is the clock, not the state code: leanback reports states beyond
          # the documented set (1081 has been seen with playback plainly running), and
          # "the position on the video we queued moved forward" is what "playing" means
          # anyway. A frozen or restarting screen never satisfies it.
          try:
              position = float(answer.get("currentTime") or 0)
          except ValueError:
              continue
          if any(position > earlier + 0.5 for earlier in seen_times):
              return True
          seen_times.append(position)

      if last.get("videoId") and last.get("videoId") != video_id:
          print("   the screen is playing {}, not the {} we queued".format(
              last.get("videoId"), video_id))
      return False


  def published_screen_id(base):
      """The screenId the receiver publishes for an already-running app, or None.

      This is the only way a sender that did not launch the app can find the screen —
      the other route to one is a pairing code, and supplying a pairing code means
      making a launch. A receiver that never publishes it strands every sender that
      arrives after the first: the app reads as `running`, the cast connects, and
      nothing can ever be queued to it.

      None covers both "nothing is running" and "running, but the receiver has not
      resolved the id yet", because from here they look the same and the caller is
      polling for the same answer either way."""
      status, body = http(base.rstrip("/") + "/dial/apps/YouTube")
      if status != 200:
          raise SystemExit("GET /dial/apps/YouTube -> {}".format(status))
      if "<state>running</state>" not in body:
          return None
      match = re.search(r"<screenId>([^<]+)</screenId>", body)
      return match.group(1) if match else None


  def app_is_running(base):
      status, body = http(base.rstrip("/") + "/dial/apps/YouTube")
      if status != 200:
          raise SystemExit("GET /dial/apps/YouTube -> {}".format(status))
      return "<state>running</state>" in body


  def wait_for_published_screen(base, timeout=90):
      """Poll until the *receiver* publishes the screen id of the page it launched.

      Deliberately not the same question as `wait_for_screen`. That one asks YouTube
      whether the page registered; this asks the receiver whether it noticed. The gap
      between the two is the receiver's own resolver — which is exactly the thing
      --reconnect exists to test, so it has to be waited for rather than assumed."""
      deadline = time.monotonic() + timeout
      while time.monotonic() < deadline:
          screen_id = published_screen_id(base)
          if screen_id:
              return screen_id
          time.sleep(2)
      raise SystemExit(
          "the receiver never published a <screenId> for the running app.\n"
          "A sender arriving now has no way to attach: it did not launch this app, so it "
          "holds no pairing code, and the app-info XML offers nothing else. This is the "
          "connected-but-never-plays failure — the receiver has to publish its screen id.")


  def token_for_screen(screen_id):
      """A screenId is public; the token to drive it is minted on demand."""
      status, body = post_form(LOUNGE + "/pairing/get_lounge_token_batch",
                               {"screen_ids": screen_id})
      if status != 200:
          raise SystemExit("get_lounge_token_batch -> {} {}".format(status, body[:200]))
      screens = json.loads(body).get("screens") or []
      if not screens or not screens[0].get("loungeToken"):
          raise SystemExit("no lounge token for screen " + screen_id)
      return screens[0]["loungeToken"]


  def expect_skip(lounge, video_id, timeout=120):
      """Watch for the receiver seeking past a segment, from the sender's seat.

      The oracle is a *discontinuity*: playback position that advances further than wall
      time did. Nothing else on this channel can do that — only a seek. Deliberately not
      "does the position match a SponsorBlock segment", because then the test and the
      implementation would be reading the same third-party answer and agreeing with each
      other rather than with reality."""
      deadline = time.monotonic() + timeout
      last = None
      while time.monotonic() < deadline:
          answer = {}

          def answered(command):
              _aid, name, payload = command
              if name == "nowPlaying" and isinstance(payload, dict) and payload.get("videoId"):
                  answer.update(payload)
                  return True
              return False

          lounge.send("getNowPlaying")
          if not lounge.listen(15, answered):
              continue
          if answer.get("videoId") != video_id:
              continue
          try:
              position = float(answer.get("currentTime") or 0)
          except ValueError:
              continue
          now = time.monotonic()
          if last is not None:
              elapsed = now - last[0]
              jumped = (position - last[1]) - elapsed
              if jumped > 1.5:
                  print("   position jumped {:.1f}s further than the {:.1f}s that passed"
                        .format(position - last[1], elapsed))
                  return True
          last = (now, position)
      return False


  def main():
      argv = sys.argv[1:]
      reconnect = "--reconnect" in argv
      skip_check = "--expect-skip" in argv
      argv = [a for a in argv if a not in ("--reconnect", "--expect-skip")]
      if not argv:
          raise SystemExit(
              "usage: yt-selfplay [--reconnect] [--expect-skip] <receiver-base-url> "
              "[videoId,videoId,...]")
      base = argv[0]
      videos = (argv[1] if len(argv) > 1 else
                (SEGMENTED_VIDEO if skip_check else DEFAULT_VIDEOS)).split(",")

      if reconnect:
          # The returning phone: the app is already running, and this sender never
          # launched it. Everything hangs on the screen id the receiver publishes.
          #
          # If nothing is running, be the first phone as well — launch, wait for the page
          # to register, and then throw that pairing code away and come back through the
          # front door. Two senders, one run, no operator. This used to demand that a
          # human had cast by hand beforehand, which is what stopped the D28 regression
          # from being a test anything could run (#96).
          #
          # The launch half deliberately keeps nothing: `wait_for_screen`'s token would
          # work, and using it would test the path a *launching* sender takes, which is
          # the path --reconnect exists not to take.
          if not app_is_running(base):
              print("nothing running; launching first, then re-attaching as a second sender")
              pairing_code = str(uuid.uuid4())
              dial_launch(base, pairing_code)
              wait_for_screen(pairing_code)
          screen_id = wait_for_published_screen(base)
          print("attaching to published screen " + screen_id)
          token = token_for_screen(screen_id)
      else:
          pairing_code = str(uuid.uuid4())
          dial_launch(base, pairing_code)
          token = wait_for_screen(pairing_code)["loungeToken"]

      lounge = Lounge(token)
      lounge.connect()

      failed = []
      for n, video in enumerate(videos, start=1):
          if n > 1:
              print("--- watching for {}s, the way someone browses before the next tap".format(DWELL))
              lounge.listen(DWELL, lambda command: False)
          print("--- tap {}: {}".format(n, video))
          # `videoId` names which entry to start on; without it the screen treats this
          # as an edit of the existing list and keeps playing what it already had.
          lounge.send("setPlaylist", videoIds=video, videoId=video, currentIndex=0,
                      currentTime=0, audioOnly="false", params="", playerParams="",
                      listId="")
          if skip_check:
              # Watch from the moment of the tap, not after confirming playback: a
              # segment at the head of the video is skipped within a second or two, so
              # anything that waits first misses the very event it is looking for. A
              # position that jumps is proof of playback as well as of the skip.
              if expect_skip(lounge, video):
                  print("tap {}: the receiver skipped a segment".format(n))
              else:
                  print("tap {}: nothing was skipped".format(n))
                  failed.append(video + " (no skip seen)")
          elif playing(lounge, video):
              print("tap {}: PLAYING".format(n))
          else:
              print("tap {}: never played".format(n))
              failed.append(video)

      if failed:
          print("FAIL: " + ", ".join(failed))
          return 1
      print("PASS: " + ("the receiver skipped a segment in every video"
                        if skip_check else
                        "the screen played every video the sender queued"))
      return 0


  if __name__ == "__main__":
      sys.exit(main())
''
