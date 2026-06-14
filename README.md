# dverust — a fast Rust rewrite of DumbVersion

Drop-in, **format-compatible** reimplementation of
[DumbVersion](https://github.com/thecatontheceiling/DumbVersion) (`.dvp` ISO
delta patches) in Rust. Produces and consumes the exact same `.dvp` byte format
as the original C# NativeAOT tools — patches cross-apply in both directions.

* `dvcreate <base> <target> <out.dvp>` — create a patch (≈ `DumbVersionCreator`)
* `dvapply -o <out> <base> <patch.dvp>` — apply a patch (≈ `DumbVersionPatcher`)

## Why it's faster

The algorithm is a faithful port (FastCDC chunking, XXH3 chunk hashes, sorted
chunk index, 8-byte SIMD match extension, base + target self-dedup, Brotli
command stream, SHA-256 validation). The speedups are structural:

1. **Parallel base indexing.** The base ISO is split into per-core segments and
   chunked concurrently (`rayon`), then the chunk records are parallel-sorted.
   CDC boundaries are content-defined, so segments re-synchronise to the
   canonical chunking within one chunk — only a handful of boundary chunks
   differ from a serial pass, which is irrelevant for matching. On an 8 GB ISO
   this drops indexing from ~9 s to ~1.5 s on 24 cores.

2. **Overlapped hashing.** The two mandatory full-file SHA-256 passes (base and
   target) are inherently serial, so they run on background threads and hide
   under the parallel indexing and the generate phase. The header is written
   last, so the hashes are only needed at the very end.

3. **`opt-level = 3` + fat LTO + `panic = abort`**, single codegen unit.

`dvapply` got the same treatment: the output is hashed **incrementally as it is
written** (one pass, not a second read-back of the finished file), the base-hash
validation runs on a **background thread overlapped with reconstruction**, and
there is **no synchronous `msync`** (dirty pages write back lazily, as in the
original).

### Measured (24-core box, warm page cache, hyperfine)

| operation | DumbVersion (C# AOT) | dverust (Rust) | speedup |
|---|---:|---:|---:|
| XP create (589 MB → 630 MB)    | 2.86 s | 2.01 s | **1.42×** |
| win11 create (8.2 GB → 8.1 GB) | 18.74 s | 4.47 s | **4.19×** |
| XP apply                       | 1.36 s | 0.99 s | **1.37×** |
| win11 apply                    | 14.08 s | 11.41 s | **1.23×** |

Patches are the same size as the C# tool's (win11: 435,193,662 vs 435,296,933 —
the Rust patch is actually ~100 KB smaller) and `xdelta3` is beaten on size and
speed in every scenario. See `../bench.sh` and `../verify.sh`.

### Chunker frontier

FastCDC was benchmarked against larger-chunk FastCDC variants (`DV_CDC_BITS`) and
a different family, AE (Asymmetric Extremum). On the win11 pair, **nothing beat
FastCDC-256 on both axes**: larger FastCDC chunks are faster but produce larger
patches; AE finds marginally better boundaries (−0.2 % size) but is 2.7× slower.
So FastCDC-256 stays the default. `DV_CDC_BITS=9` is a near-Pareto option (~30 %
faster create for +0.5 % size) for anyone who wants it.

### Optional: `DV_SEGMENTS`

The match-finding (generate) phase is sequential by default so the output patch
is **byte-identical to the serial algorithm**. Setting `DV_SEGMENTS=N` splits
the target into N independently-matched segments for extra parallelism, at the
cost of some lost cross-segment self-dedup (≈ +3 % patch size at N=8). Off by
default (N=1) because for distribution the patch size usually matters more than
the last second of create time.

### Other tunables

* `DV_BROTLI_Q` (default `4`) — Brotli quality. `4` matches .NET
  `CompressionLevel.Optimal`. `11` = `SmallestSize` (much slower, smaller).
* `DV_BROTLI_W` (default `22`) — Brotli window (lgwin).

## On io_uring

The hot data path is **memory-mapped** (`mmap` + `madvise(SEQUENTIAL|WILLNEED)`).
This is deliberate and, for this workload, faster than an io_uring read backend:

* Match extension does **random** access all over the base file, so the base
  must be randomly addressable in memory anyway — `mmap` gives that for free.
* When the ISOs are in the page cache (every benchmark re-run, and the common
  "make many patches from one base" case), `mmap` reads cost **zero copies**.
  Reading the same bytes into an owned buffer via io_uring would add a full
  8 GB memcpy to the critical path and *regress* the warm case.
* `madvise(WILLNEED)` already triggers asynchronous kernel readahead — the same
  win io_uring readahead would provide — without that copy.

io_uring is the right tool when you need high-throughput **async** I/O over data
you stream once and access linearly (e.g. O_DIRECT bulk copy). If you are
patching on a cold-cache server and want an O_DIRECT io_uring read path, it can
be added behind a flag — ask and it's a small addition. It is intentionally not
the default because it would make the measured (warm) numbers worse.

## Build

```
cargo build --release
```

Binaries land in `target/release/{dvcreate,dvapply}`.
