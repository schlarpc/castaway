"""Drive the Android emulator through pairing and playback against castaway (#225).

The counterpart of `checks.android-bt`'s builder: everything here happens *inside* the
running emulator, over adb, the way a person would do it on a phone — which is the whole
point. The pairing consent is a real Settings dialog found by `uiautomator dump` and
tapped by resource-id, not an adb backdoor; Android has no shell command that pairs.

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
REF_WAV = os.environ["REF_WAV"]
VLC_APK = os.environ["VLC_APK"]


def adb(*args, timeout=60):
    """One adb call, captured; a dead adb is a failed check, not a hang."""
    proc = subprocess.run(
        [ADB, *args], capture_output=True, text=True, timeout=timeout
    )
    return proc.returncode, proc.stdout + proc.stderr


def shell(*args, timeout=60):
    return adb("shell", *args, timeout=timeout)


def wait_for(what, predicate, timeout, interval=2):
    """Poll until `predicate()` is truthy, with the failure naming the step."""
    print(f"waiting for {what} (up to {timeout}s)", flush=True)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            print(f"  {what}: ok", flush=True)
            return value
        time.sleep(interval)
    raise SystemExit(f"FAIL: timed out waiting for {what}")


def log_has(needle):
    try:
        with open(CASTAWAY_LOG, encoding="utf-8", errors="replace") as f:
            return needle in f.read()
    except FileNotFoundError:
        return False


def ui_dump():
    shell("uiautomator", "dump", "/sdcard/ui.xml")
    _, xml = shell("cat", "/sdcard/ui.xml")
    return xml


def find_node(xml, needle):
    """Center of the first node whose text/content-desc contains `needle`."""
    for m in re.finditer(r"<node[^>]*>", xml):
        node = m.group(0)
        text = "".join(
            re.findall(r'(?:text|content-desc)="([^"]*)"', node)
        ).lower()
        if needle.lower() in text:
            b = re.search(r'bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', node)
            if b:
                x1, y1, x2, y2 = map(int, b.groups())
                if x2 > x1 and y2 > y1:
                    return (x1 + x2) // 2, (y1 + y2) // 2
    return None


def tap(pos):
    shell("input", "tap", str(pos[0]), str(pos[1]))


def tap_when_visible(needle, timeout=60):
    pos = wait_for(
        f"ui node containing {needle!r}", lambda: find_node(ui_dump(), needle), timeout
    )
    tap(pos)


def main():
    # Boot. `wait-for-device` alone reports the *transport*, not the OS; the property
    # is what says Android is actually up.
    wait_for("adb device", lambda: adb("get-state")[1].strip() == "device", 300, 5)
    wait_for(
        "boot completed",
        lambda: shell("getprop", "sys.boot_completed")[1].strip() == "1",
        300,
        5,
    )

    # The receiver must already be on the phy: Bluetooth comes up and inquires below.
    wait_for("castaway discoverable", lambda: log_has("bluetooth: discoverable"), 120)

    rc, out = shell("cmd", "bluetooth_manager", "enable")
    print(f"bluetooth enable: rc={rc} {out.strip()}", flush=True)

    # Pair through the real Settings flow: scan screen, device row, consent dialog.
    shell("am", "start", "-a", "android.settings.BLUETOOTH_SETTINGS")
    time.sleep(3)
    tap_when_visible("pair new device", 60)
    tap_when_visible("castaway", 120)

    # The consent dialog's affirmative is android:id/button1 with text "Pair". Matching
    # the row again here would re-open the dialog forever, so this one is by id.
    def pair_button():
        xml = ui_dump()
        m = re.search(
            r'<node[^>]*text="Pair"[^>]*resource-id="android:id/button1"[^>]*>', xml
        ) or re.search(
            r'<node[^>]*resource-id="android:id/button1"[^>]*text="Pair"[^>]*>', xml
        )
        if not m:
            return None
        b = re.search(r'bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', m.group(0))
        x1, y1, x2, y2 = map(int, b.groups())
        return (x1 + x2) // 2, (y1 + y2) // 2

    tap(wait_for("the Pair consent button", pair_button, 60))

    # Our side's account: the link key arrived and was stored (#68), and — the #211
    # assertion — the phone registered for VOLUME_CHANGED and heard its INTERIM.
    wait_for("castaway: paired", lambda: log_has("bluetooth: paired"), 120)
    wait_for(
        "the volume registration (#211)",
        lambda: log_has("peer registered for volume changes; answering interim"),
        120,
    )
    # Android connects A2DP on its own after pairing; the codec assertion lives in the
    # builder, which reads the log after everything else has passed.
    wait_for("an A2DP stream", lambda: log_has("bluetooth: stream configured"), 120)

    # The sender: VLC, pinned APK, playing the reference over the negotiated codec.
    print("installing VLC", flush=True)
    rc, out = adb("install", "-g", VLC_APK, timeout=300)
    if rc != 0:
        raise SystemExit(f"FAIL: vlc install: {out}")
    adb("push", REF_WAV, "/sdcard/Music/ref.wav", timeout=120)
    shell(
        "am", "start",
        "-a", "android.intent.action.VIEW",
        "-d", "file:///sdcard/Music/ref.wav",
        "-t", "audio/x-wav",
        "-n", "org.videolan.vlc/.StartActivity",
    )

    # Playback reached the pipeline when the now-playing card fills in. The reference
    # is six seconds; the margin covers VLC's spin-up and the A2DP sink delay.
    wait_for("playback reaching the pipeline", lambda: log_has("NOW PLAYING"), 120)
    time.sleep(20)
    shell("input", "keyevent", "KEYCODE_MEDIA_PAUSE")
    print("drive complete", flush=True)


if __name__ == "__main__":
    main()
