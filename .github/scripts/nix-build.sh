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

# One derivation at a time, using every core. A hosted runner is 4 vCPU and 16 GB, and
# `max-jobs = auto` reads only the first number: four builds at once, each using every
# core, is sixteen concurrent compiler processes and four peak working sets stacked on top
# of each other. This tree has several large Rust link steps, and they do not fit that way.
#
# On the command line rather than in nix.conf: the installer owns that file, and a setting
# that silently did not apply is exactly how the previous attempt at this fixed nothing
# while looking like it had.
pacing=(--max-jobs 1 --cores 4)

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
      "$(free -m | awk '/^Mem:/{printf "used=%sM avail=%sM", $3, $7}')" \
      "$(df -BG --output=avail / | tail -1 | tr -d ' ')" \
      "$(cut -d' ' -f1-3 /proc/loadavg)"
    sleep 20
  done
) &
sampler=$!
trap 'kill "$sampler" 2>/dev/null || true' EXIT

nix build "${pacing[@]}" --print-build-logs "$@"
status=$?

kill "$sampler" 2>/dev/null || true
exit "$status"
