use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use brotli::CompressorWriter;
use memmap2::Mmap;
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use dverust::fastcdc::{self, AVG_SIZE};
use dverust::{encode_zigzag, write_varint, Cmd, Header};

/// Selectable chunker so alternatives to FastCDC can be measured on the same
/// pipeline. The choice only affects `create`; the `.dvp` format is identical.
#[derive(Clone, Copy)]
enum Chunker {
    Fast(fastcdc::Params),
    Ae { w: usize, max: usize },
    Ram { w: usize, max: usize },
    Gear { mask: u32, min: usize, max: usize },
    Fixed { size: usize },
}

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

impl Chunker {
    fn from_env() -> Self {
        match std::env::var("DV_CHUNKER").as_deref() {
            Ok("ae") => Chunker::Ae { w: env_usize("DV_AE_W", 94), max: env_usize("DV_MAX", 1024) },
            Ok("ram") => Chunker::Ram { w: env_usize("DV_RAM_W", 70), max: env_usize("DV_MAX", 1024) },
            Ok("gear") => Chunker::Gear {
                mask: env_usize("DV_GEAR_MASK", 0xFF) as u32,
                min: env_usize("DV_MIN", 64),
                max: env_usize("DV_MAX", 1024),
            },
            Ok("fixed") => Chunker::Fixed { size: env_usize("DV_FIXED", 256) },
            _ => Chunker::Fast(fastcdc::Params::from_env()),
        }
    }
    #[inline]
    fn cut(&self, data: &[u8]) -> usize {
        match self {
            Chunker::Fast(p) => fastcdc::cdc_offset_p(data, p),
            Chunker::Ae { w, max } => fastcdc::ae_offset(data, *w, *max),
            Chunker::Ram { w, max } => fastcdc::ram_offset(data, *w, *max),
            Chunker::Gear { mask, min, max } => fastcdc::gear_offset(data, *mask, *min, *max),
            Chunker::Fixed { size } => fastcdc::fixed_offset(data, *size),
        }
    }
    #[inline]
    fn max(&self) -> usize {
        match self {
            Chunker::Fast(p) => p.max,
            Chunker::Ae { max, .. } | Chunker::Ram { max, .. } | Chunker::Gear { max, .. } => *max,
            Chunker::Fixed { size } => *size,
        }
    }
}

#[inline]
fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

#[inline]
fn match_len_fwd(a: &[u8], b: &[u8], max: usize) -> usize {
    let mut m = 0;
    while max - m >= 8 {
        let x = u64::from_le_bytes(a[m..m + 8].try_into().unwrap());
        let y = u64::from_le_bytes(b[m..m + 8].try_into().unwrap());
        let d = x ^ y;
        if d == 0 {
            m += 8;
        } else {
            return m + (d.trailing_zeros() / 8) as usize;
        }
    }
    while m < max && a[m] == b[m] {
        m += 1;
    }
    m
}

#[inline]
fn match_len_bwd(src: &[u8], sp: usize, tgt: &[u8], tp: usize, max: usize) -> usize {
    let mut m = 0;
    while max - m >= 8 {
        let a = u64::from_le_bytes(src[sp - m - 8..sp - m].try_into().unwrap());
        let b = u64::from_le_bytes(tgt[tp - m - 8..tp - m].try_into().unwrap());
        let d = a ^ b;
        if d == 0 {
            m += 8;
        } else {
            return m + (d.leading_zeros() / 8) as usize;
        }
    }
    while m < max && src[sp - m - 1] == tgt[tp - m - 1] {
        m += 1;
    }
    m
}

/// Parallel content-defined chunk index. The base is split into per-core
/// segments; CDC boundaries are content-defined so segments re-synchronise to
/// the canonical chunking within one chunk, leaving only a handful of boundary
/// chunks differing from a serial pass — negligible for matching.
fn build_index(base: &[u8], ck: Chunker) -> Vec<(u64, u64)> {
    let len = base.len();
    if len == 0 {
        return Vec::new();
    }
    let nthreads = rayon::current_num_threads().max(1);
    let seg = (len / nthreads).max(1);
    let ranges: Vec<(usize, usize)> = (0..nthreads)
        .map(|i| {
            let s = (i * seg).min(len);
            let e = if i == nthreads - 1 { len } else { ((i + 1) * seg).min(len) };
            (s, e)
        })
        .filter(|(s, e)| s < e)
        .collect();

    let cmax = ck.max();
    let parts: Vec<Vec<(u64, u64)>> = ranges
        .par_iter()
        .map(|&(s, e)| {
            let mut v: Vec<(u64, u64)> = Vec::with_capacity((e - s) / AVG_SIZE + 16);
            let mut off = s;
            while off < e {
                let remaining = (e - off).min(cmax);
                let clen = ck.cut(&base[off..off + remaining]);
                let h = fastcdc::compute_hash(&base[off..off + clen]);
                v.push((h, off as u64));
                off += clen;
            }
            v
        })
        .collect();

    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut recs: Vec<(u64, u64)> = Vec::with_capacity(total);
    for p in &parts {
        recs.extend_from_slice(p);
    }
    recs.par_sort_unstable();
    recs
}

/// One emitted patch operation, with *absolute* offsets so segments can be
/// generated independently and serialised afterwards.
enum Ins {
    /// Copy from base at absolute `src` for `len` bytes.
    Copy { src: i64, len: u64 },
    /// Copy already-written target `distance` bytes back, for `len` bytes.
    CopyTarget { distance: u64, len: u64 },
    /// Literal bytes taken from the target mmap at `off` for `len` bytes.
    Insert { off: u64, len: u64 },
}

/// Diff a single target segment [start, end) against the shared base index.
/// All matches are confined to the segment so segments never overlap, and the
/// target self-dedup cache is segment-local.
#[allow(unused_assignments)]
fn diff_segment(base: &[u8], records: &[(u64, u64)], target: &[u8], start: usize, end: usize, ck: Chunker) -> Vec<Ins> {
    const CACHE_BITS: u32 = 20;
    const CACHE_SIZE: usize = 1 << CACHE_BITS;
    const CACHE_MASK: u64 = (CACHE_SIZE as u64) - 1;
    const STRICT: usize = AVG_SIZE;
    let cmax = ck.max();

    let mut out: Vec<Ins> = Vec::new();
    let mut cache_keys = vec![0u64; CACHE_SIZE];
    let mut cache_vals = vec![u64::MAX; CACHE_SIZE];

    let mut offset = start;
    let mut is_inserting = false;
    let mut insert_start = 0usize;
    let mut insert_len = 0usize;

    macro_rules! flush_insert {
        () => {{
            if is_inserting {
                if insert_len > 0 {
                    out.push(Ins::Insert { off: insert_start as u64, len: insert_len as u64 });
                }
                is_inserting = false;
            }
        }};
    }

    while offset < end {
        let remaining = (end - offset).min(cmax);
        let chunk_len = ck.cut(&target[offset..offset + remaining]);
        let hash = fastcdc::compute_hash(&target[offset..offset + chunk_len]);

        // ---- base match ----
        let mut idx = records.partition_point(|&(h, _)| h < hash);
        let mut best_base_offset: i64 = -1;
        let mut best_match_len: usize = 0;
        while idx < records.len() && records[idx].0 == hash {
            let base_match = records[idx].1 as usize;
            let max_expand = (base.len() - base_match).min(end - offset);
            let ml = match_len_fwd(&base[base_match..], &target[offset..], max_expand);
            if ml > best_match_len {
                best_match_len = ml;
                best_base_offset = base_match as i64;
                if ml == max_expand {
                    break;
                }
            }
            idx += 1;
        }

        // ---- target self match ----
        let mut best_tgt_offset: i64 = -1;
        let mut best_tgt_len: usize = 0;
        let cidx = (hash & CACHE_MASK) as usize;
        if cache_keys[cidx] == hash && cache_vals[cidx] != u64::MAX {
            let to = cache_vals[cidx] as usize;
            let max_expand = end - offset;
            best_tgt_len = match_len_fwd(&target[to..], &target[offset..], max_expand);
            best_tgt_offset = to as i64;
        }

        let mut max_match_len = 0usize;
        let mut use_base = false;
        if best_match_len >= best_tgt_len && best_match_len > 0 {
            max_match_len = best_match_len;
            use_base = true;
        } else if best_tgt_len > 0 {
            max_match_len = best_tgt_len;
        }

        let accept = if max_match_len > 0 {
            if is_inserting {
                max_match_len >= STRICT
            } else {
                max_match_len >= chunk_len
            }
        } else {
            false
        };

        if accept {
            let forward_match_len = max_match_len;
            let mut back_match = 0usize;
            if is_inserting {
                let src_off = if use_base { best_base_offset } else { best_tgt_offset } as usize;
                let max_back = insert_len.min(src_off);
                let (s, sp) = if use_base {
                    (base, best_base_offset as usize)
                } else {
                    (&target[..], best_tgt_offset as usize)
                };
                back_match = match_len_bwd(s, sp, target, offset, max_back);
            }

            if back_match > 0 {
                insert_len -= back_match;
                if use_base {
                    best_base_offset -= back_match as i64;
                } else {
                    best_tgt_offset -= back_match as i64;
                }
                max_match_len += back_match;
            }

            flush_insert!();

            if use_base {
                out.push(Ins::Copy { src: best_base_offset, len: max_match_len as u64 });
            } else {
                let cur_write = offset as i64 - back_match as i64;
                let distance = (cur_write - best_tgt_offset) as u64;
                out.push(Ins::CopyTarget { distance, len: max_match_len as u64 });
            }

            offset += forward_match_len;
        } else {
            if !is_inserting {
                is_inserting = true;
                insert_start = offset;
                insert_len = chunk_len;
            } else {
                insert_len += chunk_len;
            }
            cache_keys[cidx] = hash;
            cache_vals[cidx] = offset as u64;
            offset += chunk_len;
        }
    }

    flush_insert!();
    out
}

fn create(base_path: &str, target_path: &str, patch_path: &str) -> std::io::Result<()> {
    // .NET CompressionLevel.Optimal == Brotli quality 4 (SmallestSize would be 11).
    let q: u32 = std::env::var("DV_BROTLI_Q").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let lgwin: u32 = std::env::var("DV_BROTLI_W").ok().and_then(|v| v.parse().ok()).unwrap_or(22);
    let ck = Chunker::from_env();
    // Default zstd-9: faster create + apply and smaller patches than Brotli.
    // DV_CODEC=brotli restores the original C#-cross-compatible format.
    let codec = match std::env::var("DV_CODEC").as_deref() {
        Ok("brotli") => dverust::Codec::Brotli,
        _ => dverust::Codec::Zstd,
    };
    let zstd_level: i32 = std::env::var("DV_ZSTD_L").ok().and_then(|v| v.parse().ok()).unwrap_or(9);

    let t0 = Instant::now();
    let base_file = File::open(base_path)?;
    let base = Arc::new(unsafe { Mmap::map(&base_file)? });
    let _ = base.advise(memmap2::Advice::Sequential);
    let _ = base.advise(memmap2::Advice::WillNeed); // kick off async kernel readahead
    let target_file = File::open(target_path)?;
    let target = Arc::new(unsafe { Mmap::map(&target_file)? });
    let _ = target.advise(memmap2::Advice::Sequential);
    let _ = target.advise(memmap2::Advice::WillNeed);
    let target_len = target.len();

    let base_filename = Path::new(base_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .replace(['/', '\\', ':'], "");
    let header_size = dverust::MAGIC.len() + 8 + 32 + 32 + 2 + base_filename.as_bytes().len();

    // SHA-256 is inherently serial, so run both passes on background threads and
    // overlap them with the (parallel) indexing and generate phases. The header
    // is written last, so the hashes are only needed at the very end.
    let bh = {
        let base = Arc::clone(&base);
        std::thread::spawn(move || sha256(&base[..]))
    };
    let th = {
        let target = Arc::clone(&target);
        std::thread::spawn(move || sha256(&target[..]))
    };

    eprintln!("Indexing base...");
    let records = build_index(&base, ck);
    eprintln!("Base file indexed: {} unique chunks", records.len());
    eprintln!("Took {:.2}s", t0.elapsed().as_secs_f64());

    let t1 = Instant::now();
    eprintln!("Generating Patch...");

    // ---- parallel match-finding over target segments ----
    // More segments = more parallelism but more lost cross-segment self-dedup,
    // so this is a separate, smaller knob than the index thread count.
    let nthreads: usize = std::env::var("DV_SEGMENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1) // 1 == byte-for-byte the same patch the serial algorithm produces
        .clamp(1, rayon::current_num_threads().max(1) * 4);
    let seg = (target_len / nthreads).max(1);
    let bounds: Vec<(usize, usize)> = (0..nthreads)
        .map(|i| {
            let s = (i * seg).min(target_len);
            let e = if i == nthreads - 1 { target_len } else { ((i + 1) * seg).min(target_len) };
            (s, e)
        })
        .filter(|(s, e)| s < e)
        .collect();

    let segments: Vec<Vec<Ins>> = bounds
        .par_iter()
        .map(|&(s, e)| diff_segment(&base, &records, &target, s, e, ck))
        .collect();

    // ---- serial serialisation into the single compressed stream ----
    let serialize = |w: &mut dyn Write| -> std::io::Result<()> {
        let mut last_base_offset: i64 = 0;
        for seg_ins in &segments {
            for ins in seg_ins {
                match *ins {
                    Ins::Copy { src, len } => {
                        w.write_all(&[Cmd::Copy as u8])?;
                        write_varint(w, encode_zigzag(src - last_base_offset))?;
                        write_varint(w, len)?;
                        last_base_offset = src + len as i64;
                    }
                    Ins::CopyTarget { distance, len } => {
                        w.write_all(&[Cmd::CopyTarget as u8])?;
                        write_varint(w, distance)?;
                        write_varint(w, len)?;
                    }
                    Ins::Insert { off, len } => {
                        w.write_all(&[Cmd::Insert as u8])?;
                        write_varint(w, len)?;
                        w.write_all(&target[off as usize..(off + len) as usize])?;
                    }
                }
            }
        }
        w.write_all(&[Cmd::Eof as u8])
    };

    let mut out = File::create(patch_path)?;
    out.seek(SeekFrom::Start(header_size as u64))?;
    let mut out = match codec {
        dverust::Codec::Brotli => {
            let mut w = CompressorWriter::new(out, 1 << 16, q, lgwin);
            serialize(&mut w)?;
            w.flush()?;
            w.into_inner()
        }
        dverust::Codec::Zstd => {
            let mut w = zstd::stream::write::Encoder::new(out, zstd_level)?;
            // Multithreaded compression: the serial Brotli pass is replaced by a
            // worker pool, so the compress step scales with cores on big inserts.
            let _ = w.multithread(rayon::current_num_threads() as u32);
            serialize(&mut w)?;
            w.finish()?
        }
    };

    let base_hash = bh.join().expect("base hash thread panicked");
    let target_hash = th.join().expect("target hash thread panicked");
    let header = Header { target_size: target_len as i64, base_hash, target_hash, base_filename, codec };
    debug_assert_eq!(header.size(), header_size);
    out.seek(SeekFrom::Start(0))?;
    header.write(&mut out)?;
    out.flush()?;

    eprintln!("Patch created");
    eprintln!("Took {:.2}s", t1.elapsed().as_secs_f64());
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("Usage: dvcreate <base> <target> <output.dvp>");
        std::process::exit(1);
    }
    if let Err(e) = create(&args[1], &args[2], &args[3]) {
        eprintln!("{e}");
        let _ = std::fs::remove_file(&args[3]);
        std::process::exit(1);
    }
}
