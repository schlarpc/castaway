#!/usr/bin/env python3
"""The signal `bluetooth-vm` sends over A2DP, and the check that it survived (#186).

Two subcommands, one waveform definition:

    signal.py make REF.wav          write the reference a real A2DP source will play
    signal.py check REF.wav REC.wav compare the receiver's recording against it

A linear chirp on each channel, sweeping opposite ways: 200 Hz up to 8 kHz on the left,
8 kHz down to 200 Hz on the right, after a silent lead-in.

*A chirp* rather than tones, and this is where the VM differs from
`pipeline::audio_decode::tests::sweep`, which sums four fixed tones. Any periodic signal
correlates just as well against itself one period late, and that costs nothing when the
search window is 40 ms of codec latency — which is all the cargo-tier test has to
consider. Here the recording starts when the receiver does and the audio arrives whenever
a real source gets around to sending it, so the search is *the whole file* and a signal
with a 50 ms period has a thousand equally good answers in it. A chirp has one.

*Opposite directions* for the two channels, because identical ones make a left/right swap
and a mono collapse invisible and both are real failures — a planar frame read through a
packed accessor produces exactly the second. An up-chirp and a down-chirp are close to
orthogonal, so a swap does not merely score worse, it scores near zero.

What this does **not** assert, and why
--------------------------------------

Not pacing, and not completeness. The emulated link delivers under half of what is sent,
in bursts of about 40 ms separated by a fifth of a second — measured, and unchanged by
giving btvirt an ordinary laptop's buffers instead of its hardcoded 192-byte single-buffer
ones, so it is the emulator itself and not a number anybody can tune. The source plays in
real time regardless, so the receiver's mixer does exactly what it should: it emits on the
sound card's clock and fills what did not arrive in time with silence.

That shapes the measurement, and the direction matters. Asking "is this part of the
reference somewhere in the recording" measures the *link*, because on this link most of
the reference never arrives and the answer is no through no fault of the receiver. So the
question is asked the other way round: every window of audio in the recording is located
in the reference, and has to be there. That measures the receiver, which is the only
thing here under test — audio it produced that nobody sent it is a defect no matter how
much else went missing.

The numbers, all measured. Correct audio over the real link: 80% of windows match, the
rest being the ones that straddle a splice. Recordings built to be wrong — channels
swapped, collapsed to mono, resampled at 44.1 against 48, decoded at half rate, replaced
with noise — score 15%, caught outright, 0%, 7% and 0%.

Polarity is worth a paragraph, because it is where the obvious version of this test is
silently blind. A single channel matched on its own cannot see an inversion at all:
for a signal that is locally a sinusoid, inverting it and delaying it half a cycle are
the same thing, and the search is over delays — an inverted chirp scores +0.98 against
the original. What sees it is holding the two channels to *one* alignment. The right
channel is scored at the lag the left one chose, so a flip on either channel, or on
both, has to be undone by a half-cycle shift that the other channel does not want; the
two sweep in opposite directions, so no single lag satisfies them. Measured: both
channels flipped scores 15%, one channel flipped 12%, against 80% for correct audio.

Every matched window is also scored against the opposite channel at that same instant —
that is what makes a swap a failure — and the positions they map to in the reference have
to increase. A slow link delays audio and a lossy one drops it; neither reorders it, so
windows that walk backwards through the reference are coincidences rather than content.
"""

import struct
import sys
import wave

import numpy as np

# The recording and the reference are both at the mixer's own rate, so nothing here
# resamples: a rate mismatch is a failure to report, not a difference to paper over.
RATE = 48_000
CHANNELS = 2
# Full scale would clip on the way through PipeWire's own volume handling; -6 dBFS leaves
# room and is still 90 dB above the noise floor of anything under test.
AMPLITUDE = 0.5
SECONDS = 6.0
# Silent for the first 50 ms, so a recording that begins mid-signal is obvious.
LEAD_IN = 0.05
# The band the chirps sweep. The top stays under 8 kHz: SBC's highest subband is where a
# lossy codec spends the least of its budget, and a threshold that had to tolerate 15 kHz
# would be too loose to catch anything else.
LOW_HZ = 200.0
HIGH_HZ = 8000.0

# A run of exact zeros in *both* channels at least this long is the mixer's padding
# rather than the signal. Half a millisecond: the padding runs are milliseconds, and the
# two channels sweep in opposite directions so their zero crossings do not line up —
# there is no half-millisecond of simultaneous silence anywhere in the reference.
MIN_GAP = RATE // 2000
# What gets measured: one window of audio, located in the reference. Its length is the
# whole trick, and it is bounded from both sides.
#
# A run of audio is *not* one continuous piece of the reference. When the link loses a
# packet those samples are simply absent, and the mixer emits what comes next right up
# against what came before — a splice, with no silence to mark it. A window longer than
# the gap between two splices spans one and matches nothing, however good the decode.
# Measured on this link: at 25 ms, 55% of windows matched and the rest scored ~0.8 on the
# left channel and *negative* on the right, which is that splice; at 10 ms, the splices
# fall between windows.
#
# Downward, the limit is uniqueness: 10 ms of this chirp sweeps ~13 Hz, and it still
# places to a sample because phase does the rest — matched windows score 1.000 against a
# cross-channel 0.4-0.6. Shorter would start to be a tone, and a tone is somewhere in a
# chirp more than once.
WINDOW = RATE // 100
# Samples dropped from each end of a run of audio. Its edges are where the mixer changed
# between padding and audio, which is a boundary rather than a measurement.
TRIM = RATE // 1000
# Fewer usable windows than this and the session did not produce enough to judge — which
# is a failure of the session, not a pass by default.
MIN_WINDOWS = 20


def chirp(t: np.ndarray, start_hz: float, end_hz: float) -> np.ndarray:
    """A linear chirp from `start_hz` to `end_hz` across the whole of `t`."""
    rate = (end_hz - start_hz) / (2 * t[-1])
    return np.sin(2 * np.pi * (start_hz * t + rate * t**2))


def reference() -> np.ndarray:
    """The waveform to send, as interleaved 16-bit samples."""
    t = np.arange(int(RATE * SECONDS)) / RATE
    left = chirp(t, LOW_HZ, HIGH_HZ)
    right = chirp(t, HIGH_HZ, LOW_HZ)
    envelope = np.where(t < LEAD_IN, 0.0, 1.0)
    stereo = np.stack([left, right], axis=1) * envelope[:, None] * AMPLITUDE
    return (stereo * 32767.0).astype("<i2")


def make(path: str) -> None:
    samples = reference()
    with wave.open(path, "wb") as w:
        w.setnchannels(CHANNELS)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(samples.tobytes())
    print(f"wrote {path}: {len(samples) / RATE:.2f} s, {RATE} Hz x {CHANNELS}ch")


def read(path: str) -> np.ndarray:
    """One WAV file as float columns per channel, insisting on the shape we expect."""
    with wave.open(path, "rb") as w:
        if (w.getframerate(), w.getnchannels(), w.getsampwidth()) != (RATE, CHANNELS, 2):
            raise SystemExit(
                f"{path} is {w.getframerate()} Hz x {w.getnchannels()}ch "
                f"x {w.getsampwidth() * 8}-bit; expected {RATE} Hz x {CHANNELS}ch x 16-bit"
            )
        frames = w.getnframes()
        if frames == 0:
            raise SystemExit(f"{path} holds no audio at all")
        raw = np.frombuffer(w.readframes(frames), dtype="<i2")
    return raw.astype(np.float64).reshape(-1, CHANNELS) / 32768.0


def bursts(rec: np.ndarray) -> list[tuple[int, int]]:
    """The spans of the recording that are audio rather than the mixer's padding.

    A run of exact zeros in *both* channels of at least MIN_GAP is padding: the mixer
    emits on the sound card's clock and fills what the source did not supply in time, and
    on this link that is most of the file. Everything else is decoded audio, and each
    span of it is what gets checked.

    The run-length summary printed here is the honest measure of the link, and it is the
    number to read first when this test fails: a path keeping up produces one long run.
    """
    silent = np.all(rec == 0.0, axis=1)
    edges = np.flatnonzero(np.diff(np.concatenate(([False], silent, [False]))))
    starts, ends = edges[0::2], edges[1::2]
    keep = (ends - starts) >= MIN_GAP
    padding = np.zeros(len(rec), dtype=bool)
    for start, end in zip(starts[keep], ends[keep]):
        padding[start:end] = True

    spans = _spans(~padding)
    for name, mask in (("audio", ~padding), ("silence", padding)):
        lengths = np.array([end - start for start, end in _spans(mask)])
        if len(lengths):
            ms = lengths / RATE * 1000
            print(
                f"  {name} runs: {len(ms)}, shortest {ms.min():.2f} ms, "
                f"median {np.median(ms):.2f} ms, longest {ms.max():.2f} ms"
            )
    print(
        f"  {(~padding).sum() / RATE:.2f} s of audio in a "
        f"{len(rec) / RATE:.2f} s recording"
    )
    return spans


def _spans(mask: np.ndarray) -> list[tuple[int, int]]:
    """Start and end of every run of True in `mask`."""
    edges = np.flatnonzero(np.diff(np.concatenate(([False], mask, [False]))))
    return list(zip(edges[0::2].tolist(), edges[1::2].tolist()))


class Recording:
    """One recording, prepared once so every probe is a cheap transform against it."""

    def __init__(self, rec: np.ndarray) -> None:
        self.rec = rec
        self.size = 1 << int(np.ceil(np.log2(len(rec) + WINDOW)))
        self.spectra = [np.fft.rfft(rec[:, c], self.size) for c in range(CHANNELS)]
        self.energy = [
            np.concatenate(([0.0], np.cumsum(rec[:, c] ** 2))) for c in range(CHANNELS)
        ]

    def match(self, probe: np.ndarray, channel: int) -> tuple[float, int]:
        """The best normalised correlation of `probe` anywhere in `channel`, and its lag.

        An FFT rather than a lag loop because the search space is the whole recording: a
        receiver that has been running for a minute before anything is played has a lag
        of millions of samples, and there is no honest smaller window to search. The
        normalisation is a sliding energy over the recording, so a loud passage cannot
        win on amplitude alone, and the score stays signed — an inverted copy scores -1
        rather than 1, which is the point of not taking an absolute value here.
        """
        n = len(probe)
        span = len(self.rec) - n
        if span <= 0:
            return 0.0, 0
        corr = np.fft.irfft(
            self.spectra[channel] * np.conj(np.fft.rfft(probe, self.size)), self.size
        )
        energy = self.energy[channel]
        window = np.sqrt(np.maximum(energy[n:] - energy[:-n], 0.0))[: span + 1]
        denominator = window * np.sqrt(np.sum(probe**2))
        scores = np.where(
            denominator > 0, corr[: span + 1] / np.maximum(denominator, 1e-12), 0.0
        )
        lag = int(np.argmax(scores))
        return float(scores[lag]), lag

    def score_at(self, probe: np.ndarray, channel: int, lag: int) -> float:
        """The same normalised correlation, at one chosen lag rather than the best one.

        This is what ties the two channels together. Scored where the *other* channel
        said the audio is, a channel that has been inverted comes back at -1 — whereas
        left to find its own alignment it would slide half a cycle and score +1, which is
        how a per-channel polarity flip passes a test that measures each channel alone.
        """
        n = len(probe)
        if lag < 0 or lag + n > len(self.rec):
            return 0.0
        segment = self.rec[lag : lag + n, channel]
        denominator = np.sqrt(np.sum(segment**2) * np.sum(probe**2))
        if denominator <= 0:
            return 0.0
        return float(np.dot(segment, probe) / denominator)


def check(ref_path: str, rec_path: str, floor: float, needed: float) -> None:
    ref = read(ref_path)
    raw = read(rec_path)
    print(f"reference {len(ref) / RATE:.2f} s, recording {len(raw) / RATE:.2f} s")

    peak = np.max(np.abs(raw))
    rms = float(np.sqrt(np.mean(raw**2)))
    print(f"recording peak {peak:.4f}, rms over the whole file {rms:.5f}")
    if peak < 1e-3:
        raise SystemExit(
            "the recording is silent: the session was established and nothing came out of it"
        )

    # The reference is what gets searched, and every probe comes out of the *recording*.
    # That is the whole design. Asking "is this part of the reference in the recording"
    # measures the link, because on this one most of the reference never arrives; asking
    # "is this thing the receiver produced part of the reference" measures the receiver,
    # which is the only thing here under test.
    index = Recording(ref)
    # Every run of audio, cut into windows. A long run contributes several rather than
    # one, so a splice inside it costs the window that contains it and not the rest.
    windows = [
        at
        for start, end in bursts(raw)
        for at in range(start + TRIM, end - TRIM - WINDOW + 1, WINDOW)
    ]
    if len(windows) < MIN_WINDOWS:
        raise SystemExit(
            f"only {len(windows)} windows of {WINDOW / RATE * 1000:.0f} ms of audio: "
            "the session produced too little to say anything about"
        )

    matched, found_at = [], []
    for start in windows:
        span = raw[start : start + WINDOW]
        # The left channel finds where in the reference this came from; the right is
        # scored *there*, so the two are held to one instant rather than each finding
        # its own — which is what makes a per-channel polarity flip visible.
        _, at = index.match(span[:, 0], 0)
        left = index.score_at(span[:, 0], 0, at)
        right = index.score_at(span[:, 1], 1, at)
        # And each against the wrong channel *at the same instant*. Not "anywhere in the
        # other channel", which is what this asked first and is unsound at this length: a
        # 10 ms window is nearly a tone, and every tone in the up-chirp is also somewhere
        # in the down-chirp, so a free-to-align cross score reaches 0.98 on audio that is
        # perfectly correct. At one lag the two channels are at different frequencies by
        # construction, and correct audio scores near zero here.
        cross_left = index.score_at(span[:, 0], 1, at)
        cross_right = index.score_at(span[:, 1], 0, at)
        ok = min(left, right) >= floor
        print(
            f"{'match' if ok else '  -  '} {start / RATE:6.2f} s"
            f"  L {left:+.3f} (other {cross_left:+.3f})"
            f"  R {right:+.3f} (other {cross_right:+.3f})"
            f"  from {at / RATE:5.2f} s of the reference"
        )
        if not ok:
            continue
        for name, score, other in (
            ("left", left, cross_left),
            ("right", right, cross_right),
        ):
            if other >= score:
                raise SystemExit(
                    f"the {name} channel of the window at {start / RATE:.2f} s matches "
                    f"the reference's other channel at least as well ({other:+.3f} vs "
                    f"{score:+.3f}): the channels are swapped, collapsed to mono, or "
                    "inverted"
                )
        matched.append(start)
        found_at.append(at)

    fraction = len(matched) / len(windows)
    print(
        f"{len(matched)}/{len(windows)} windows of audio are the reference, at "
        f">= {floor} on both channels ({fraction:.0%})"
    )
    if fraction < needed:
        raise SystemExit(
            f"only {fraction:.0%} of what the receiver played is audio that was sent to "
            f"it, below {needed:.0%}: that is a different waveform, not a late one"
        )
    # In order, because audio arrives in order. A slow link delays audio and a lossy one
    # drops it; neither reorders it, so matches that jump backwards through the reference
    # are coincidences rather than content.
    out_of_order = [i for i in range(1, len(found_at)) if found_at[i] < found_at[i - 1]]
    if out_of_order:
        raise SystemExit(
            f"{len(out_of_order)} of {len(found_at)} windows come from earlier in the "
            "reference than the window before them: this is matching by chance"
        )
    print("OK: what the receiver played is what was sent, in order, on both channels")


def main(argv: list[str]) -> None:
    if len(argv) >= 3 and argv[1] == "make":
        make(argv[2])
    elif len(argv) >= 4 and argv[1] == "check":
        floor = float(argv[4]) if len(argv) > 4 else 0.9
        needed = float(argv[5]) if len(argv) > 5 else 0.6
        check(argv[2], argv[3], floor, needed)
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    # `struct` is imported for the error message below and nowhere else: a truncated WAV
    # is the failure this script is most likely to hit first, and `wave` reports it as a
    # bare struct.error with no filename in it.
    try:
        main(sys.argv)
    except (wave.Error, struct.error) as e:
        raise SystemExit(f"{' '.join(sys.argv[1:])}: {e}") from e
