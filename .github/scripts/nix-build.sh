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

# One derivation at a time, on both of the runner's cores. A private repo gets 2 vCPU and
# 7 GB, and `max-jobs = auto` reads only the first number: several builds at once, each
# with a peak working set, on a box that cannot hold one of the large ones comfortably.
#
# On the command line rather than in nix.conf: the installer owns that file, and a setting
# that silently did not apply is exactly how the previous attempt at this fixed nothing
# while looking like it had.
pacing=(--max-jobs 1 --cores 2)

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

# `raw` rather than the default progress bar: on a non-tty the bar re-emits its whole
# pending-builds list on every update, which is why a single job produced a 57 MB log —
# 495k lines, most of them the same line — and why GitHub truncated it 48 minutes before
# the job actually ended, taking the failure with it.
nix build "${pacing[@]}" --log-format raw --print-build-logs "$@"
status=$?

kill "$sampler" 2>/dev/null || true
exit "$status"
