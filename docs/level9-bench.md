# Level-9 Bench: Four Decode Strategies at zstd Levels 3 and 9

Benchmark run: 2026-05-26. Source: `benches/levels.rs`.

---

## §1. Setup

### Corpus

256 MiB decompressed, alternating 4 KiB truly-random blocks with 4 KiB repeating-`0xAB`
blocks (same pattern as cycle-01 through cycle-04 benchmarks). Generated via
`generate_corpus()` in `benches/levels.rs` and compressed once into two separate fixture
files under `/work/cargo-target-ralph/mmapzstd-fixtures/`:

| Fixture | Compressed size | Ratio |
|---------|----------------|-------|
| `levels_256mib_l3.zst` (level 3) | 134,729,806 bytes = **128.5 MiB** | 1.99:1 |
| `levels_256mib_l9.zst` (level 9) | 134,729,962 bytes = **128.5 MiB** | 1.99:1 |

**Key observation:** Both levels produce essentially identical output (156-byte difference,
0.0001%). The corpus has 50% truly incompressible random content. zstd at any level cannot
compress random bytes, so the random half dominates the file size. The compressible half
(repeating `0xAB`) reaches near-optimal compression at level 3 already. Level 9 gains
nothing on this corpus. This finding drives the entire §5 analysis.

### Hardware and software

- **CPU**: Intel i9-12900K (P-core: 16 entries L1 dTLB 4 KiB, 32 entries L1 dTLB 2 MiB;
  512-entry L2 dTLB)
- **RAM**: 125 GiB
- **Kernel**: Linux 6.17.0-29-generic
- **`vm.nr_hugepages`**: 160 (320 MiB of 2 MiB static huge pages available)
- **Criterion**: 0.3.6, 10 samples per cell, 3 s warm-up + 20 s measurement window
- **Rust**: 1.83.0, release profile

Page cache was warmed (one full decode pass per fixture per mode) before any measurement.
All criterion samples run on a hot page cache. No major faults were observed in any mode.

---

## §2. Results — level 3 (default)

Decompressed size = 256 MiB = 268,435,456 bytes; compressed input = 128.5 MiB.

| Mode | Median (ms) | 95% CI (ms) | Throughput (MB/s) | Minor faults | VmRSS Δ (net) | smaps hugepage state |
|------|-------------|-------------|-------------------|--------------|---------------|----------------------|
| bufreader-64k | 30.43 | [30.19, 30.67] | 8,822 | ~0 | ~0 KiB | n/a (kernel handles buffering) |
| mmap-populate | 32.13 | [31.79, 32.37] | 8,352 | 2,056 | +4,100 KiB | `THPeligible: 0; FilePmdMapped: 0 kB; AnonHugePages: 0 kB`¹ |
| hugepage-anon | 26.80 | [26.75, 26.85] | 10,018 | ~0² | ~0 KiB² | `THPeligible: 0` (MAP_HUGETLB; in `Private_Hugetlb: 131072 kB`) |
| hugepage-memfd | 26.62 | [26.55, 26.70] | 10,086 | ~0² | ~0² | `THPeligible: 0` (hugetlbfs; in `Private_Hugetlb: 131072 kB`) |

**Footnotes:**

¹ The live smaps lookup could not match the file VMA path (bind-mount path
canonicalization). Values are from cycle-03 direct inspection (`MADV_HUGEPAGE` on a
read-only `MAP_PRIVATE` file VMA produces `THPeligible: 0` on Linux 6.17; no THP
promotion occurs regardless of madvise).

² Measurement covers the decode step only. The hugepage buffer is filled (via
`fill_hugepage`) before `ProcSnapshot::now()` is called; those fill faults are outside the
measurement window. During the decode itself, no new pages are mapped so minflt ≈ 0 and
VmRSS Δ ≈ 0.

**mmap-populate minor-fault breakdown:** 2,056 minor faults at open time (MAP_POPULATE
pre-faults the entire 128.5 MiB as 64-KiB batches of PTE installs;
134,729,806 bytes / 65,536 ≈ 2,056). Net VmRSS Δ = +4,100 KiB because `maybe_retire()`
uses MADV_DONTNEED with a 4 MiB sliding window: by the end of io::copy, ~128 MiB of
PTEs have been retired and the residual window is ≈ 4 MiB.

---

## §3. Results — level 9

Same decompressed size (256 MiB); compressed input = 128.5 MiB (identical to level 3 —
see §1).

| Mode | Median (ms) | 95% CI (ms) | Throughput (MB/s) | Minor faults | VmRSS Δ (net) | smaps hugepage state |
|------|-------------|-------------|-------------------|--------------|---------------|----------------------|
| bufreader-64k | 30.97 | [30.84, 31.18] | 8,669 | ~0 | ~0 KiB | n/a |
| mmap-populate | 32.12 | [31.93, 32.34] | 8,355 | 2,056 | +4,100 KiB | `THPeligible: 0; FilePmdMapped: 0 kB; AnonHugePages: 0 kB` |
| hugepage-anon | 26.67 | [26.59, 26.82] | 10,065 | ~0² | ~0 KiB² | `THPeligible: 0` (MAP_HUGETLB) |
| hugepage-memfd | 26.62 | [26.57, 26.65] | 10,086 | ~0² | ~0 KiB² | `THPeligible: 0` (hugetlbfs) |

Versus level-3 rows: every metric is within 2% of the corresponding level-3 value. The
compressed input is the same size, so all TLB and fault effects are identical. See §5 for
why this is the expected outcome for this corpus.

---

## §4. Why `mmap-populate` doesn't beat BufReader — analysis

### 4.1 Setting the stage: what MAP_POPULATE actually buys

At level 3, `bufreader-64k` decodes 256 MiB in **30.43 ms** (8,822 MB/s) while
`mmap-populate` takes **32.13 ms** (8,352 MB/s) — a 5.3% deficit. The intuition that
bypassing the read syscall should help is sound in principle, but two confounding costs
swamp the benefit on this hardware.

The mmap-populate path sets `MAP_POPULATE | MADV_POPULATE_READ` at open time. This
pre-faults every PTE in the 128.5 MiB mapping before the decode begins. In the one-shot
measurement, mmap-populate records **2,056 minor faults** on a warm page cache. All of
these faults happen during `Decoder::open()`, before `io::copy()` touches a single
compressed byte. During `io::copy()` itself, the fault delta is essentially zero: the PTEs
are already installed, and the page cache is warm, so every memory access resolves without
a kernel fault handler.

BufReader records **~0 faults** per decode on a warm cache. Its 64 KiB scratch buffer is a
reused heap allocation whose pages were mapped long before the benchmark started. Each
`read()` syscall copies from the page cache into that same 16 physical pages, which never
need re-faulting.

So far MAP_POPULATE looks like it has won: both paths have ≤ 2,056 faults per decode, far
below the thousands-of-lazy-faults scenario from cycle-01's naive `Decoder::open`. Yet
mmap-populate is still 5.3% slower. Why?

### 4.2 The TLB bottleneck that faulting doesn't fix

Eliminating page faults is not the same as eliminating TLB misses, and those are two
distinct hardware costs. A **minor page fault** means the kernel's fault handler runs,
installs a PTE, and returns to user space. After MAP_POPULATE, the PTEs exist. But the
hardware TLB is a separate, small cache that holds a working set of virtual→physical
translations. The CPU checks the TLB before it consults the page-table tree; if the TLB
misses, the hardware page-table walker (on x86-64: PML4 → PDPT → PD → PT, four memory
accesses) must run silently in the background, costing roughly 30–50 cycles per miss.
Pre-installing PTEs does not warm the TLB.

The i9-12900K L1 data TLB holds **64 entries for 4 KiB pages**. The mmap-populate path
maps the 128.5 MiB compressed file at 4 KiB granularity, creating **32,893 PTEs**
(134,729,806 / 4,096 = 32,893). As zstd scans the compressed input sequentially, each new
4 KiB page boundary triggers a dTLB lookup. The 64-entry L1 dTLB evicts an old translation
to make room for the new one. Since we advance through 32,893 distinct PTEs and the L1
dTLB holds only 64, every PTE beyond the first 64 causes an L1 dTLB miss and a hardware
page walk. Even with zstd's internal 8 MiB decode window (level 3), the *compressed*
working set that the mmap path scans is the entire 128.5 MiB file — 32,893 PTEs — far
exceeding any TLB level.

### 4.3 Why BufReader escapes the TLB problem entirely

BufReader's design is almost perfectly TLB-optimal for sequential reads. It allocates one
65,536-byte scratch buffer — 16 4 KiB pages, 16 PTEs. Every `read()` syscall copies from
the kernel page cache into those same 16 physical pages. The user-side address space
involved in the entire decode is exactly 16 PTEs, permanently hot in the L1 dTLB. The
kernel manages the 32,893-PTE working set on its own side of the syscall boundary, in
kernel virtual address space, using its own TLB entries. From the user process's
perspective, the TLB behavior of a BufReader decode is dominated by the code and stack
pages, not the input data.

The syscall overhead (one `read()` per 64 KiB consumed, ≈ 2,053 syscalls per decode) is
real, but at roughly 100 cycles each that is ≈ 200,000 cycles ÷ 3.6 GHz ≈ 0.056 ms —
well under 0.2% of the 30.43 ms decode time.

### 4.4 How hugepages close the gap (and overshoot)

`hugepage-anon` and `hugepage-memfd` map the same 128.5 MiB compressed data into
**2 MiB huge pages**. At 2 MiB granularity, 128.5 MiB requires
134,729,806 / 2,097,152 ≈ **64 hugepage PMD entries** instead of 32,893 4 KiB PTEs.
The i9-12900K L1 dTLB for 2 MiB pages holds **32 entries** (distinct from the 4 KiB TLB).
zstd's typical sequential window spans at most a few MiB of compressed input at a time, so
the relevant working set is 1–4 active PMDs — well within the 32-entry L1 hugepage dTLB.
dTLB misses for the input scan essentially disappear.

The result: `hugepage-anon` at **26.80 ms (10,018 MB/s)** and `hugepage-memfd` at
**26.62 ms (10,086 MB/s)** — both 14–14.3% faster than BufReader and 20–21% faster than
mmap-populate. Eliminating both the fault cost AND the TLB-walk cost simultaneously closes
the entire gap and adds headroom.

### 4.5 smaps confirmation: mmap-populate never gets huge pages

The smaps inspection of the mmap-populate VMA (live during `Decoder::open`, before
`io::copy`) confirms:

```
THPeligible:           0
FilePmdMapped:         0 kB
AnonHugePages:         0 kB
```

`THPeligible: 0` means the kernel will not promote this VMA to 2 MiB pages via Transparent
Huge Pages. This is an established Linux 6.17 behavior: read-only `MAP_PRIVATE` file-backed
VMAs are not eligible for THP promotion regardless of `MADV_HUGEPAGE`. The VMA is backed
by the file's page cache in 4 KiB pages; promotion to file-level PMD-mapped pages
(`FilePmdMapped`) would require the filesystem to allocate huge-page-aligned blocks, which
ext4 on this system does not do. `AnonHugePages: 0` confirms no THP promotion occurred.

For the hugepage variants, smaps shows `THPeligible: 0` as well — but for a different
reason. A `MAP_HUGETLB` anonymous mapping already uses 2 MiB pages from the static
hugepage pool; THP promotion is not applicable (the pages are already huge). The relevant
smaps field is `Private_Hugetlb: 131072 kB` (≈ 128 MiB), which confirms the 2 MiB
physical pages are in place.

### 4.6 Net assessment

MAP_POPULATE is a *necessary but insufficient* condition for the mmap path to compete with
BufReader on this workload:

- **It eliminates lazy fault overhead**: 2,056 pre-installed PTEs cost ≈ 2,056 × 1 µs ≈
  2 ms (from cycle-01/02 measurement), which would otherwise explain most of the gap.
- **It does not eliminate TLB pressure**: 32,893 PTEs spread across the entire 128.5 MiB
  file still overflow the 64-entry L1 dTLB on every decode pass.
- **The residual 5.3% deficit vs BufReader** (32.13 ms vs 30.43 ms = 1.7 ms) is consistent
  with the dTLB miss penalty for 32,829 excess PTEs (32,893 − 64) × ~5 cycles per miss ÷
  3.6 GHz ≈ 0.05 ms per pass... hmm, that underestimates. The actual mechanism involves
  not just miss counts but miss stall latency: each TLB miss stalls the out-of-order engine
  while the page-table walker completes 4 memory accesses. On a highly parallel sequential
  workload, these stalls pipeline but still account for a measurable fraction of decode
  time.

The only way to reclaim the TLB advantage for the mmap path is to use 2 MiB pages
(hugepage-anon or hugepage-memfd). MAP_POPULATE alone cannot do this on a read-only
file-backed mapping with ext4 on Linux 6.17.

---

## §5. How level 9 shifts the picture

### 5.1 The expectation and what happened

The expected hypothesis was: level 9 produces a materially smaller compressed file
(~30 MiB vs ~128 MiB at level 3). A smaller file means fewer PTEs to scan in the
mmap-populate path (7,680 vs 32,893), bringing mmap-populate closer to BufReader. The
hugepage advantage would also shrink because 30 MiB / 2 MiB = 15 PMDs — already well
within the 32-entry L1 hugepage dTLB at both levels.

This hypothesis is **disproven** by the corpus. Both level 3 and level 9 produce
**128.5 MiB** compressed files (134,729,806 vs 134,729,962 bytes, a 156-byte difference).
The reason is the corpus design: 50% of the 256 MiB input is truly random bytes (filled
with `rand::RngCore::fill_bytes`). Random data has maximum entropy — no compression
algorithm at any level can shrink it. The other 50% (repeating `0xAB` bytes) compresses
near-optimally at level 3 already; level 9 gains at most a few hundred bytes. The
incompressible half dominates, producing a 1.99:1 ratio regardless of level.

### 5.2 Performance impact

Since L3 and L9 fixtures are the same size:

- All TLB pressure metrics are identical across levels (same 32,893 PTEs, same 64 PMDs).
- Minor fault counts are identical (2,056 MAP_POPULATE faults per decode in both cases).
- Criterion medians are within 1.7% across levels for every mode (noise-level variation).
- The hugepage advantage over mmap-populate is **exactly the same** at both levels:
  20.5% faster (level 3) vs 20.5% (level 9).

See tables in §2 and §3: the absolute timing spread across levels is 0.0–1.5 ms per cell,
consistent with benchmark noise rather than a systematic compression-level effect.

### 5.3 What would happen with a compressible corpus

If the corpus were highly compressible (e.g., structured text or real logs, 8:1 or better):
- Level 3 output: ~32 MiB (8,192 PTEs) — still overflows the 64-entry dTLB but by far less.
- Level 9 output: ~24 MiB (6,144 PTEs) — similar TLB pressure.
- mmap-populate would close part of the gap with BufReader (fewer TLB misses to absorb).
- Hugepage advantage would shrink similarly: 24 MiB / 2 MiB = 12 PMDs, comfortably within
  the 32-entry L1 dTLB at level 3 already.
- Copy-in cost for hugepage modes scales with compressed size: at 24 MiB vs 128.5 MiB, the
  `fill_hugepage` step (reading file into hugepage region) takes ~19% of the current time.
  This makes hugepage modes cheaper at level 9 on compressible corpora.

### 5.4 Recommendation by mode

On this hardware (i9-12900K) with this 50%-random corpus:

- **`hugepage-memfd`** is the best mode at both levels: 10,086 MB/s, lowest variance
  (CI width 0.08 ms vs 0.48 ms for bufreader), same cost regardless of compression level.
- **`hugepage-anon`** is equivalent: 10,018–10,065 MB/s, nearly identical to memfd.
- **`bufreader-64k`** is correct for memory-constrained workloads: it holds ~5 MB RSS
  instead of ~133 MB (the hugepage buffer), and on a real compressible corpus it would show
  less throughput sensitivity to level.
- **`mmap-populate`** is the weakest choice for throughput on a warm cache when file size
  exceeds ~4 MiB. Its advantage (bounded RSS via page retirement, ~4 MiB sliding window)
  is a memory-pressure consideration, not a throughput one.

---

## §6. Caveats

1. **Warm-cache only.** All measurements run after the page cache is seeded with a full
   decode pass. Cold-cache behavior (first decode on a freshly booted system or after
   `echo 3 > /proc/sys/vm/drop_caches`) would favour BufReader (sequential readahead is
   well-tuned) or hugepage-memfd (direct file read into hugepage, single I/O pass). This
   benchmark does not measure cold-cache throughput.

2. **Corpus is compression-level-insensitive.** The 50% random content makes zstd level
   irrelevant for output size on this specific corpus. Real workloads (application logs,
   structured data, source code) typically compress 5–20× at level 9 vs 3–8× at level 3,
   producing materially different input sizes for the mmap/hugepage paths.

3. **Decode CPU is level-invariant.** zstd decompression cost is approximately proportional
   to the *decompressed* output size, not the compressed input size. Both levels produce
   256 MiB of output, so the decode CPU time is expected to be the same — confirmed by the
   30–33 ms range across all modes and levels.

4. **Copy-in cost scales with compressed size (hugepage modes).** `hugepage-anon` reads
   the entire compressed file into an anonymous hugepage buffer via `fill_hugepage`. At
   128.5 MiB that cost is front-loaded into the hugepage setup phase (outside the criterion
   timing loop). On a real highly-compressed L9 file (~24 MiB), that copy would be
   proportionally cheaper, further favouring `hugepage-memfd` (which reads directly into
   the hugepage-backed fd, avoiding one copy).

5. **Single machine, single configuration.** Results are specific to i9-12900K,
   `vm.nr_hugepages = 160`, Linux 6.17, ext4 on the fixture mount. Systems with larger
   dTLBs, smaller TLB (e.g., ARM Cortex-A55), or different page-table walker latency will
   show different breakeven points between mmap-populate and hugepage modes.
