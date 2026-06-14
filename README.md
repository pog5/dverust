# dverust

A faster Rust rewrite of [DumbVersion](https://github.com/thecatontheceiling/DumbVersion)
(`.dvp` ISO delta patches).

* `dvcreate <base> <target> <out.dvp>` — create a patch (≈ `DumbVersionCreator`)
* `dvapply -o <out> <base> <patch.dvp>` — apply a patch (≈ `DumbVersionPatcher`)

## Build

```
cargo build --release
```

Binaries land in `target/release/{dvcreate,dvapply}`. Prebuilt binaries for
Linux/Windows/macOS (x64 + arm64) are on the [releases page](https://github.com/pog5/dverust/releases).

## Compatibility

`dvcreate` writes **zstd** patches by default. `dvapply` reads both zstd and the
original C# Brotli format. For patches the upstream C# `DumbVersionPatcher` can
also apply, create with `DV_CODEC=brotli`.

## Tunables (env vars)

| var | default | notes |
|-----|---------|-------|
| `DV_CODEC` | `zstd` | `zstd` or `brotli` (Brotli = C#-compatible) |
| `DV_ZSTD_L` | `9` | zstd level |
| `DV_CDC_BITS` | `8` | FastCDC average chunk size = `2^bits` |
| `DV_CHUNKER` | `fastcdc` | also `gear`, `ram`, `ae`, `fixed` |
| `DV_SEGMENTS` | `1` | split create across N segments for more parallelism |

Defaults are tuned; the knobs are there for experimentation.
