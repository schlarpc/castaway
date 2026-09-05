"""Drive a real Play Services Cast sender at castaway over the TAP segment (#225).

The counterpart of `checks.android-cast`'s builder. Everything here happens *inside* the
running emulator, over adb, and the sender is Android's own system Cast picker — the
surface #226 was invisible on. Nothing in this script speaks CASTv2: the phone's Play
Services does, and our side is judged from its journal and from the wire.

The script is deliberately chatty on stdout: when a step times out, the last thing it
printed is the diagnosis.
"""

import os
import re
import subprocess
import sys
import time

ADB = os.environ["ADB"]
CASTAWAY_LOG = os.environ["CASTAWAY_LOG"]
RECEIVER_NAME = os.environ.get("RECEIVER_NAME", "castaway")
MIRROR_SECONDS = int(os.environ.get("MIRROR_SECONDS", "20"))

# The segment, from the builder rather than repeated here: the same two addresses
# configure the tap, pin the advertisement and judge the capture, and a copy of them in
# this file would be a fourth place to change and the one nothing would catch.
RECEIVER_IP = os.environ["RECEIVER_IP"]
GUEST_IP = os.environ["GUEST_IP"]
# The emulator's Virtio Wi-Fi sits behind its own SLIRP on the *same* subnet; see the
# note in `main` for why that matters and why Wi-Fi gets switched off.
GUEST_WIFI_IP = os.environ["GUEST_WIFI_IP"]


def adb(*args, timeout=60):
    """One adb call, captured; a dead adb is a failed check, not a hang."""
    try:
        proc = subprocess.run([ADB, *args], capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return 1, f"<adb {' '.join(args)} timed out after {timeout}s>"
    return proc.returncode, proc.stdout + proc.stderr


def shell(*args, timeout=60):
    return adb("shell", *args, timeout=timeout)


def wait_for(what, predicate, timeout, interval=2, diagnose=None):
    """Poll until `predicate()` is truthy, with the failure naming the step.

    `diagnose` is what the *step* knows and its name does not — the interface list
    behind "no address on the segment", the adapter state behind "no A2DP stream" —
    printed when the poll expires. Optional only because not every step has one; a
    step that does and omits it is #386: the segment-address poll timed out on the
    2026-08-28 nightly having printed nothing but its own name, so nothing in the run
    could say whether eth0 was up with the wrong address, up with none, or absent.
    This file's docstring promises the last thing printed is the diagnosis, and this
    is what keeps that true on the path where it matters.

    Best-effort: a step that has already lost the device must still report the step it
    lost rather than an exception raised trying to describe it.

    The elapsed time is printed on success too. A step that starts creeping towards its
    ceiling is a timeout about to become a flake, and the run that says "ok (after 12s)"
    every night is the one that makes "ok (after 170s)" legible when it happens.
    """
    print(f"waiting for {what} (up to {timeout}s)", flush=True)
    began = time.monotonic()
    deadline = began + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            print(f"  {what}: ok (after {time.monotonic() - began:.0f}s)", flush=True)
            return value
        time.sleep(interval)
    if diagnose is not None:
        try:
            print(f"  what {what} found instead:\n{diagnose()}", flush=True)
        except Exception as exc:  # noqa: BLE001 — a failed dump must not mask the step
            print(f"  (the diagnostic for {what} raised {exc!r})", flush=True)
    raise SystemExit(f"FAIL: timed out waiting for {what} after {timeout}s")


# `CASTAWAY_LOG` is the app's own journal, which `logging.rs` writes without ANSI on
# purpose. Stripping anyway costs nothing and closes the trap that a *stdout* capture —
# which is coloured — would set: tracing colours its field names, so a needle spanning a
# message and one of its fields ("… connected peer=10.0.2.15") silently never matches,
# and the peer assertion below is the one that has to be exact.
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def log_has(needle):
    try:
        with open(CASTAWAY_LOG, encoding="utf-8", errors="replace") as f:
            return needle in ANSI.sub("", f.read())
    except FileNotFoundError:
        return False


def log_tail(lines=40):
    """The end of castaway's own journal — the diagnosis for a step that waited on it.

    A `log_has` poll that expires has, by construction, no idea what the journal *does*
    say; this is what `wait_for`'s `diagnose` prints so the run does not have to be
    reproduced to find out (#386).
    """
    try:
        with open(CASTAWAY_LOG, encoding="utf-8", errors="replace") as f:
            return "".join(ANSI.sub("", f.read()).splitlines(keepends=True)[-lines:])
    except FileNotFoundError:
        return f"<no journal at {CASTAWAY_LOG}>"


def ui_dump():
    """A UI tree, tolerating the one failure this screen actually produces.

    The Cast picker scans for as long as it is open, so its spinner never lets the
    window go idle and `uiautomator dump` reports "could not get idle state" instead of
    a tree. That is not an error to fail on — it is the normal state of a scanning
    picker — so an empty dump is a retry, which the caller's poll already provides.
    """
    shell("uiautomator", "dump", "/sdcard/ui.xml", timeout=90)
    rc, xml = shell("cat", "/sdcard/ui.xml")
    return xml if rc == 0 and "<node" in xml else ""


def find_node(xml, needle):
    """Center of the first node whose text/content-desc contains `needle`."""
    for m in re.finditer(r"<node[^>]*>", xml):
        node = m.group(0)
        text = "".join(re.findall(r'(?:text|content-desc)="([^"]*)"', node)).lower()
        if needle.lower() in text:
            b = re.search(r'bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', node)
            if b:
                x1, y1, x2, y2 = map(int, b.groups())
                if x2 > x1 and y2 > y1:
                    return (x1 + x2) // 2, (y1 + y2) // 2
    return None


def tap(pos):
    shell("input", "tap", str(pos[0]), str(pos[1]))


def main():
    # Boot. `wait-for-device` alone reports the *transport*, not the OS; the property is
    # what says Android is actually up.
    wait_for("adb device", lambda: adb("get-state")[1].strip() == "device", 300, 5)
    wait_for(
        "boot completed",
        lambda: shell("getprop", "sys.boot_completed")[1].strip() == "1",
        420,
        5,
    )

    # The segment, asserted before anything is blamed on Cast. The goldfish image gives
    # eth0 SLIRP's fixed address and never DHCPs, which is why the TAP needs no DHCP
    # server — but if that ever stops being true, every later step fails for a reason
    # that has nothing to do with casting, so it is checked head-on here.
    #
    # Polled rather than read once: eth0 is configured a good while *after*
    # `sys.boot_completed` goes to 1 — it came up as link index 15 on this image — and a
    # single read right after boot reports only `lo` and blames the network for a race.
    #
    # Five minutes rather than the three this had on the 2026-08-28 nightly, where it
    # expired (#386). Three was a guess and so is five; what is not a guess is that the
    # ceiling costs a healthy run nothing — the poll returns the moment the address is
    # there, and the "ok (after Ns)" it prints is what will say whether a slow runner was
    # ever the explanation. `diagnose` is the other half: the two candidate causes — a
    # hosted runner slower than the ceiling, and a guest that has stopped taking SLIRP's
    # fixed address — look identical from the step's name alone, and a longer timeout
    # would otherwise just hide the second one for longer.
    wait_for(
        f"the guest's address on the segment ({GUEST_IP})",
        lambda: GUEST_IP in shell("ip", "-o", "-4", "addr", "show")[1],
        300,
        # All address families for the second dump, and not `ip link`: `-4` omits an
        # interface that is up with no IPv4 address, which is exactly the shape this poll
        # expires on, and `ip link show` is refused for the adb shell user on this image
        # ("request send failed: Permission denied", #387). The all-family form lists
        # every link, addressed or not, and is not refused.
        diagnose=lambda: (
            f"addresses:\n{shell('ip', '-o', '-4', 'addr', 'show')[1]}"
            f"links:\n{shell('ip', '-o', 'addr', 'show')[1]}"
            f"routes:\n{shell('ip', '-o', 'route', 'show')[1]}"
        ),
    )
    print(f"guest addresses:\n{shell('ip', '-o', '-4', 'addr', 'show')[1]}", flush=True)
    # Wi-Fi off, and this is load-bearing rather than tidying.
    #
    # The emulator gives the guest *two* networks: eth0 on our TAP, and a Virtio Wi-Fi
    # wlan0 behind the emulator's own SLIRP — and SLIRP's subnet is the same one the TAP
    # segment uses, with the host mapped to the same address at its far end. So a
    # sender that resolves us there and routes over Wi-Fi reaches castaway through
    # NAT on the host's loopback, having never touched the segment. That is exactly what
    # happened the first time this ran: discovery crossed the tap (SLIRP forwards no
    # multicast) and the CASTv2 connection then arrived from 127.0.0.1.
    #
    # Turning Wi-Fi off leaves eth0 as the only network, so there is one path and it is
    # the one under test. The peer-address assertion further down is the guard that keeps
    # saying so if this ever stops being true.
    shell("svc", "wifi", "disable")
    wait_for(
        "wlan0 to leave the segment",
        lambda: GUEST_WIFI_IP not in shell("ip", "-o", "-4", "addr", "show")[1],
        120,
        diagnose=lambda: f"addresses:\n{shell('ip', '-o', '-4', 'addr', 'show')[1]}",
    )
    wait_for(
        "the guest reaching the receiver's address",
        lambda: shell("ping", "-c", "2", "-W", "2", RECEIVER_IP)[0] == 0,
        120,
        diagnose=lambda: (
            f"ping:\n{shell('ping', '-c', '2', '-W', '2', RECEIVER_IP)[1]}"
            f"addresses:\n{shell('ip', '-o', '-4', 'addr', 'show')[1]}"
            f"routes:\n{shell('ip', '-o', 'route', 'show')[1]}"
        ),
    )

    # Our side must be advertising before the picker is opened, or the first scan finds
    # nothing and Play Services backs off for longer than this check waits.
    wait_for(
        "castaway advertising Cast",
        lambda: log_has("CASTv2 TLS listener ready"),
        180,
        diagnose=log_tail,
    )

    # The sender: Android's own system Cast picker. This is the surface #226 was
    # invisible on — GMS filters it by the DNS-SD sub-types in the mDNS answer, so
    # "the row exists" is the assertion that the advertisement is right.
    shell("am", "start", "-a", "android.settings.CAST_SETTINGS")

    def receiver_row():
        return find_node(ui_dump(), RECEIVER_NAME)

    # Generous: Play Services' scanner batches its browse and the first answer can land
    # after a back-off. A picker that never lists us is #226 all over again.
    row = wait_for(
        f"{RECEIVER_NAME!r} in the system Cast picker",
        receiver_row,
        240,
        # What the picker *is* showing is the whole diagnosis here: an empty scan, some
        # other receiver, or a row whose text has moved. #226 was invisible without it.
        diagnose=lambda: f"the picker:\n{ui_dump()[:4000]}\nour journal:\n{log_tail()}",
    )

    print("tapping the receiver row", flush=True)
    tap(row)

    # Screen mirroring is a MediaProjection capture, so SystemUI asks the person first:
    # `screen_share_permission_dialog`, with a scope spinner and a Cancel/Start pair.
    wait_for(
        "the screen-share consent dialog",
        lambda: find_node(ui_dump(), "Start casting?"),
        90,
    )
    if os.environ.get("CAST_DRIVE_DEBUG"):
        print("--- the consent dialog ---", flush=True)
        print(ui_dump()[:6000], flush=True)

    # The spinner matters. It defaults to "A single app", which sends one app's window
    # and then wants an app chosen; "Entire screen" is what a person means by casting
    # their phone, and it is the one that starts mirroring from the dialog itself.
    spinner = find_node(ui_dump(), "A single app")
    if spinner:
        print("consent dialog: switching the scope to the entire screen", flush=True)
        tap(spinner)
        tap(
            wait_for(
                "the 'Entire screen' choice",
                lambda: find_node(ui_dump(), "Entire screen"),
                30,
            )
        )
        # The dropdown closing with the new scope selected is the condition; the spinner
        # no longer reading "A single app" is how that is visible.
        wait_for(
            "the scope to settle on the entire screen",
            lambda: not find_node(ui_dump(), "A single app"),
            30,
        )

    # By id: the affirmative's *text* is "Start casting", which also matches the dialog's
    # own title, and tapping a title does nothing while the poll above happily reports
    # success.
    def start_button():
        xml = ui_dump()
        for m in re.finditer(r"<node[^>]*>", xml):
            node = m.group(0)
            if 'resource-id="android:id/button1"' not in node:
                continue
            b = re.search(r'bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', node)
            if b:
                x1, y1, x2, y2 = map(int, b.groups())
                return (x1 + x2) // 2, (y1 + y2) // 2
        return None

    print("consent dialog: starting the cast", flush=True)
    tap(wait_for("the consent dialog's affirmative", start_button, 60))

    # Everything from here is judged from our own journal: the sender connects over TLS
    # on 8009, challenges us, and we answer. Device auth against *real* Play Services is
    # what #40 said a borrowed credential could not pass, and it does.
    wait_for(
        "the sender's CASTv2 connection",
        lambda: log_has("CASTv2 sender connected"),
        120,
        diagnose=log_tail,
    )

    # …and it came over the segment. Without this the check would pass on a connection
    # that reached us through the emulator's NAT (see the Wi-Fi note above) — green, and
    # testing nothing the TAP exists to test.
    if not log_has(f"CASTv2 sender connected peer={GUEST_IP}"):
        raise SystemExit(
            "FAIL: a sender connected, but not from the guest's address on the segment. "
            "Something is routing around the TAP; the journal's peer= says what."
        )
    wait_for(
        "device auth against real Play Services",
        lambda: log_has("answered a sender's device-auth challenge"),
        120,
        diagnose=log_tail,
    )
    # Deliberately not asserted here: `GET_APP_AVAILABILITY` and the `eureka_info` probe.
    # Both are real and both are answered — they are how #226 was found — but Play
    # Services asks them from its *device prober*, on its own connection and its own
    # schedule, not on the one that carries a launch. Requiring them in sequence with the
    # mirror makes this check fail on GMS's timing rather than on our behaviour, which it
    # did. `proto-cast`'s own tests pin those two answers.

    # …and then it mirrors. The picker's tap is a screen-mirror request, so a completed
    # OFFER/ANSWER is the phone agreeing to send its screen to us over RTP — the thing
    # #226 closed as "still unproven".
    wait_for(
        "a negotiated mirroring session",
        lambda: log_has("Cast mirroring negotiated"),
        180,
        # Whatever the phone is showing instead is the diagnosis, and it is gone as soon
        # as the emulator is killed. This was a `try`/`except SystemExit` around the call
        # until `wait_for` grew somewhere to put it (#386).
        diagnose=lambda: f"the screen:\n{ui_dump()[:4000]}\nour journal:\n{log_tail()}",
    )
    wait_for(
        "the RTP receive loop",
        lambda: log_has("Cast mirroring RTP receive loop started"),
        60,
        diagnose=log_tail,
    )

    # Let the phone actually send for a while; the builder counts what landed on the
    # wire out of the segment capture, which is the half a log line cannot fake.
    print(f"mirroring for {MIRROR_SECONDS}s", flush=True)
    time.sleep(MIRROR_SECONDS)

    print("drive complete", flush=True)


if __name__ == "__main__":
    sys.exit(main())
