"""Generate a 64x64 blue-noise ranking matrix via Ulichney's void-and-cluster method.

Output: crates/pipeline/assets/bluenoise64.bin — 4096 little-endian u16 ranks (0..4095),
row-major. Deterministic (fixed seed) so the asset is reproducible.
"""

import numpy as np

N = 64
SIGMA = 1.9
rng = np.random.default_rng(0x5CA1AB1E)

# Toroidal gaussian energy kernel, applied via FFT.
coords = np.arange(N)
d = np.minimum(coords, N - coords).astype(np.float64)
dx, dy = np.meshgrid(d, d)
kernel = np.exp(-(dx**2 + dy**2) / (2 * SIGMA**2))
kernel_f = np.fft.rfft2(kernel)


def energy(pattern):
    return np.fft.irfft2(np.fft.rfft2(pattern.astype(np.float64)) * kernel_f, s=(N, N))


def tightest_cluster(pattern):
    e = energy(pattern)
    e[pattern == 0] = -np.inf
    return np.unravel_index(np.argmax(e), e.shape)


def largest_void(pattern):
    e = energy(pattern)
    e[pattern == 1] = np.inf
    return np.unravel_index(np.argmin(e), e.shape)


# Initial pattern: ~10% random ones, then relax until stable.
ones = N * N // 10
pattern = np.zeros((N, N), dtype=np.int8)
idx = rng.choice(N * N, ones, replace=False)
pattern.flat[idx] = 1
while True:
    c = tightest_cluster(pattern)
    pattern[c] = 0
    v = largest_void(pattern)
    if v == c:
        pattern[c] = 1
        break
    pattern[v] = 1

rank = np.full((N, N), -1, dtype=np.int32)

# Phase 1: rank the initial ones by removing tightest clusters.
work = pattern.copy()
for r in range(ones - 1, -1, -1):
    c = tightest_cluster(work)
    work[c] = 0
    rank[c] = r

# Phase 2: rank up to half full by filling largest voids.
work = pattern.copy()
for r in range(ones, N * N // 2):
    v = largest_void(work)
    work[v] = 1
    rank[v] = r

# Phase 3: rank the rest by finding the tightest cluster of ZEROS (= largest void of
# the inverse); equivalently insert into the emptiest spot of the remaining zeros.
for r in range(N * N // 2, N * N):
    v = largest_void(work)
    work[v] = 1
    rank[v] = r

assert sorted(rank.flatten().tolist()) == list(range(N * N))
rank.astype("<u2").tofile("crates/pipeline/assets/bluenoise64.bin")

# Quick quality report: radially averaged power spectrum should be low at low freq.
norm = (rank.astype(np.float64) + 0.5) / (N * N) - 0.5
spec = np.abs(np.fft.fft2(norm)) ** 2
spec[0, 0] = 0
freqs = np.fft.fftfreq(N)
fr = np.sqrt(freqs[:, None] ** 2 + freqs[None, :] ** 2)
low = spec[fr < 0.1].mean()
high = spec[fr > 0.3].mean()
print(f"low-freq power {low:.4f} vs high-freq {high:.4f} (blue noise wants low << high)")
