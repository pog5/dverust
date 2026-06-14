# dverust — a fast Rust rewrite of DumbVersion

Faster reimplementation of
[DumbVersion](https://github.com/thecatontheceiling/DumbVersion) (`.dvp` ISO
delta patches) in Rust.

* `dvcreate <base> <target> <out.dvp>` — create a patch (≈ `DumbVersionCreator`)
* `dvapply -o <out> <base> <patch.dvp>` — apply a patch (≈ `DumbVersionPatcher`)

`dvapply` auto-detects the codec from the patch and **applies both the original
C# Brotli patches and dverust's default zstd patches**. By default `dvcreate`
writes **zstd** patches (faster + smaller, see below); `DV_CODEC=brotli` writes
the original byte-format that the C# `DumbVersionPatcher` can also apply.

## Why it's faster

The algorithm is a faithful port (FastCDC chunking, XXH3 chunk hashes, sorted
chunk index, 8-byte SIMD match extension, base + target self-dedup, compressed
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

dverust (zstd default) vs the C# NativeAOT build (Brotli), each using its own
native format:

| operation | DumbVersion (C# AOT) | dverust (Rust) | speedup |
|---|---:|---:|---:|
| XP create (589 MB → 630 MB)    | 2.85 s | 1.33 s | **2.14×** |
| win11 create (8.2 GB → 8.1 GB) | 18.75 s | 3.99 s | **4.70×** |
| XP apply                       | 1.15 s | 0.62 s | **1.84×** |
| win11 apply                    | 12.30 s | 8.80 s | **1.40×** |

Patch sizes are comparable: win11 434.27 MB vs 435.30 MB (~1 MB smaller), XP
355.93 MB vs 355.88 MB (49 KB / 0.014 % larger — compressor noise). `xdelta3`
is beaten on size and speed in every scenario. See `../bench.sh`, `../verify.sh`.

### Compression codec — zstd by default

The compression layer, not the chunker, was the remaining lever. **zstd
(multithreaded, level 9) beats Brotli-q4 on every axis** on both ISO pairs:

| operation     | Brotli-q4 | zstd-9  | result            |
|---------------|----------:|--------:|-------------------|
| XP create     | 2.01 s    | 1.25 s  | 1.60× faster      |
| win11 create  | 4.99 s    | 4.37 s  | 1.14× faster      |
| XP apply      | 945 ms    | 707 ms  | 1.34× faster      |
| win11 apply   | 8.63 s    | 8.58 s  | tie (I/O-bound)   |
| XP size       | 356.11 MB | 355.93 MB | smaller         |
| win11 size    | 435.19 MB | 434.27 MB | −0.9 MB         |

zstd is the default (`DV_CODEC=zstd`, level via `DV_ZSTD_L`, default 9). It uses
a distinct magic (`DUMBVER\x02`), so zstd patches are **not** applicable by the
C# `DumbVersionPatcher` (dverust applies both). `DV_CODEC=brotli` restores the
original C#-cross-compatible format. The win11 apply is a tie because that path
is dominated by 8 GB writeback + SHA-256, not decompression; the decode-bound XP
apply shows zstd's real decompress advantage.

### Chunker frontier

Five chunking families were benchmarked on the win11 pair against the FastCDC-256
default (`DV_CHUNKER`, `DV_CDC_BITS`, …). **None beats FastCDC on both create
speed and patch size:**

| chunker            | create | patch size  | vs default        |
|--------------------|-------:|------------:|-------------------|
| FastCDC-256 (def)  | 4.58 s | 435.19 MB   | —                 |
| Gear-256 (1 mask)  | 4.11 s | 435.41 MB   | faster, +0.05 %   |
| Gear-512           | 3.47 s | 437.31 MB   | faster, +0.5 %    |
| RAM (w=128)        | 3.41 s | 451.18 MB   | faster, +3.7 %    |
| RAM (w=240)        | 3.41 s | 466.55 MB   | faster, +7.2 %    |
| AE (w=94)          | 16.9 s | 434.18 MB   | slower, −0.23 %   |
| fixed-256          | 45.8 s | 8.16 GB     | no dedup at all   |
| FastCDC b9–b12     | 3.4 s  | 437–482 MB  | faster, bigger    |

FastCDC's normalized two-mask chunking packs more anchor points (35 M chunks vs
Gear's 27.6 M) for slightly better dedup at the same average size — Gear is ~10 %
faster but ~0.05 % larger; the extremum families (AE/RAM) either cost speed or
size; fixed-size shows why content-defined chunking exists (a single insertion
shifts every boundary, so the patch degenerates to the whole ISO). FastCDC-256
is the sweet spot and stays the default. All alternatives remain selectable via
env vars for experimentation.

### Optional: `DV_SEGMENTS`

The match-finding (generate) phase is sequential by default so the output patch
is **byte-identical to the serial algorithm**. Setting `DV_SEGMENTS=N` splits
the target into N independently-matched segments for extra parallelism, at the
cost of some lost cross-segment self-dedup (≈ +3 % patch size at N=8). Off by
default (N=1) because for distribution the patch size usually matters more than
the last second of create time.

### Other tunables

* `DV_CODEC` (default `zstd`) — `zstd` or `brotli` (Brotli = C#-compatible).
* `DV_ZSTD_L` (default `9`) — zstd level. Higher = smaller/slower; >15 rarely
  pays off because the insert bytes are already-compressed ISO data.
* `DV_BROTLI_Q` (default `4`) — Brotli quality (matches .NET `Optimal`).
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
