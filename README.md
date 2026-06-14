# dverust

A drop-in, much faster replacement for
[DumbVersion](https://github.com/thecatontheceiling/DumbVersion) — the tool for
distributing ISOs as small `.dvp` delta patches.

**Why use it**

- **Faster.** Creates patches several times quicker than the original, and
  applies them faster too.
- **Drop-in.** It still reads your existing DumbVersion patches — nothing to
  re-download or convert.
- **Everywhere.** Prebuilt binaries for Linux, Windows and macOS (x64 + arm64)
  on the [releases page](https://github.com/pog5/dverust/releases).
- **Same small patches** (or slightly smaller).

## Use it

```sh
# apply a patch: base ISO + patch -> rebuilt ISO
dvapply -o windows.iso base.iso windows.dvp

# create a patch from two ISOs
dvcreate base.iso target.iso out.dvp
```

Or build from source: `cargo build --release` → `target/release/{dvcreate,dvapply}`.

By default new patches use zstd; pass `DV_CODEC=brotli` to `dvcreate` if you need
a patch the original C# `DumbVersionPatcher` can also apply.
