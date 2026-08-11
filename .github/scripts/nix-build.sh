#!/usr/bin/env bash
# Run a Nix build on a hosted runner, and say what the machine was doing if it dies.
#
# Both failure modes this workflow has had destroy their own evidence. Disk exhaustion
# surfaces as a compiler error in an unrelated crate; memory exhaustion kills the Nix
# daemon, or the runner agent itself, and the job ends with `exit code 143` and "the
# runner has received a shutdown signal" — no failing derivation, no build log, and no
# chance for an `if: failure()` step to run, because there is no longer a runner to run it
# on. So the sampler prints into the *same* stream as the build: whatever the last line
# before the silence says is what the machine looked like on its way out.
#
# Nothing here is diagnostic-only. `--max-jobs` is the fix the samples are there to
# confirm or refute.
set -uo pipefail

# One derivation at a time, on all of the runner's cores. `max-jobs = auto` would run
# several builds at once, each with a peak working set, on a box that cannot hold two of
# the large ones comfortably — that is the memory failure this script's sampler exists to
# catch. But `--cores` is per-derivation parallelism *inside* the one build, and the
# dominant phase here is a single crane derivation compiling many crates: cargo fans out
# to whatever it is given. This was `--cores 2` from when the repo was private and the
# runner had 2 vCPU; the repo is public now and the runner has 4, so the old number
# idled half the machine on every compile-bound job (#238).
#
# On the command line rather than in nix.conf: the installer owns that file, and a setting
# that silently did not apply is exactly how the previous attempt at this fixed nothing
# while looking like it had.
pacing=(--max-jobs 1 --cores "$(nproc)")

echo "::group::Nix settings that decide the peak"
echo "runner default:"
nix config show | grep -E '^(max-jobs|cores) ' || true
echo "this build:"
nix config show "${pacing[@]}" | grep -E '^(max-jobs|cores) ' || true
nix config show | grep -E '^substituters ' || true
echo "::endgroup::"

(
  while true; do
    printf 'sample %s  mem[%s]  disk[%s]  load[%s]\n' \
      "$(date -u +%H:%M:%S)" \
      "$(free -m | awk '/^Mem:/{printf "used=%sM avail=%sM", $3, $7} /^Swap:/{printf " swap=%sM/%sM", $3, $2}')" \
      "$(df -BG --output=avail / | tail -1 | tr -d ' ')" \
      "$(cut -d' ' -f1-3 /proc/loadavg)"
    # Who is actually holding it. Every guess so far about *what* wanted the memory has
    # been wrong; this is the line that would have answered it without a round trip.
    ps -eo rss=,comm= --sort=-rss | head -4 \
      | awk '{printf "         holding %6.0fM  %s\n", $1/1024, $2}'
    sleep 20
  done
) &
sampler=$!
trap 'kill "$sampler" 2>/dev/null || true' EXIT

# Not the default progress bar: on a non-tty the bar re-emits its whole pending-builds
# list on every update, which is why a single job produced a 57 MB log — 495k lines, most
# of them the same line — and why GitHub truncated it 48 minutes before the job actually
# ended, taking the failure with it.
#
# But the first attempt at that, `--log-format raw --print-build-logs`, threw away the
# other half: `raw` is the logger *without* build logs, and `--print-build-logs` cannot
# turn them back on — the flag is ignored whichever side of `--log-format` it sits on.
# So no derivation's output ever reached the step log, and every failure arrived as nix's
# quoted 25-line tail with the root error scrolled off the top of it. That cost a real
# investigation: `can't find crate for pipeline` is impossible on its own terms, and the
# actual cause (`E0433: cannot find module or crate 'rtc'`) was only found in a *different*
# job's log, where it happened to land inside that job's 25 lines (#332).
#
# Measured against Determinate Nix 3.17.3, building a derivation that prints 100 lines and
# exits 1 — chatter lines reaching the caller: `raw --print-build-logs` 0, `raw-with-logs`
# 100, `bar-with-logs` 100, `--print-build-logs` alone 100. `raw-with-logs` is the one that
# both streams and has no bar to re-emit.
nix build "${pacing[@]}" --log-format raw-with-logs "$@"
status=$?

kill "$sampler" 2>/dev/null || true
exit "$status"
