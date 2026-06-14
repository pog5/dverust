use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read};
use std::sync::Arc;

use brotli::Decompressor;
use memmap2::{Mmap, MmapMut};
use sha2::{Digest, Sha256};

use dverust::{decode_zigzag, Cmd, Header};

fn read_byte<R: Read>(r: &mut R) -> std::io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    match r.read(&mut b)? {
        0 => Ok(None),
        _ => Ok(Some(b[0])),
    }
}

fn read_varint<R: Read>(r: &mut R) -> std::io::Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        result |= ((b[0] & 0x7F) as u64) << shift;
        if b[0] & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok(result)
}

fn err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

fn apply(base_path: &str, patch_path: &str, out_path: &str) -> std::io::Result<()> {
    let mut pf = File::open(patch_path)?;
    let header = Header::read(&mut pf)?;
    let target_size = header.target_size as usize;

    let base_file = File::open(base_path)?;
    let base = Arc::new(unsafe { Mmap::map(&base_file)? });
    let base_len = base.len();
    let _ = base.advise(memmap2::Advice::Sequential);
    let _ = base.advise(memmap2::Advice::WillNeed); // async kernel readahead

    // Validate the base hash on a background thread, overlapped with the
    // reconstruction below (joined before we report success). The original
    // hashes the base serially up front; overlapping hides that ~full-file pass.
    let base_hash_job = {
        let base = Arc::clone(&base);
        let expected = header.base_hash;
        std::thread::spawn(move || -> bool { <[u8; 32]>::from(Sha256::digest(&base[..])) == expected })
    };

    if target_size == 0 {
        File::create(out_path)?;
        return if base_hash_job.join().unwrap() {
            Ok(())
        } else {
            Err(err("This file is not the correct base file."))
        };
    }

    let out_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out_path)?;
    out_file.set_len(target_size as u64)?;
    let mut target = unsafe { MmapMut::map_mut(&out_file)? };

    eprintln!("Applying patch...");

    // pf is positioned right after the header.
    let dec = Decompressor::new(pf, 1 << 16);
    let mut r = BufReader::with_capacity(1 << 20, dec);

    let mut bytes_written: usize = 0;
    let mut last_base_offset: i64 = 0;
    // Hash the output incrementally as it is produced (single pass, vs a second
    // full read-back pass over the finished file).
    let mut thasher = Sha256::new();

    let tptr = target.as_mut_ptr();

    while let Some(cmd) = read_byte(&mut r)? {
        if cmd == Cmd::Eof as u8 {
            break;
        }
        let start_bw = bytes_written;
        match cmd {
            x if x == Cmd::Copy as u8 => {
                let rel = decode_zigzag(read_varint(&mut r)?);
                let length = read_varint(&mut r)? as usize;
                let src = last_base_offset + rel;
                if src < 0 || (base_len as i64 - src) < length as i64 {
                    return Err(err("Out-of-bounds copy command"));
                }
                if target_size - bytes_written < length {
                    return Err(err("Copy command exceeds target size"));
                }
                let src = src as usize;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        base.as_ptr().add(src),
                        tptr.add(bytes_written),
                        length,
                    );
                }
                bytes_written += length;
                last_base_offset = src as i64 + length as i64;
            }
            x if x == Cmd::CopyTarget as u8 => {
                let distance = read_varint(&mut r)? as i64;
                let length = read_varint(&mut r)? as usize;
                let src = bytes_written as i64 - distance;
                if distance <= 0 || src < 0 {
                    return Err(err("Out-of-bounds target deduplication command"));
                }
                if target_size - bytes_written < length {
                    return Err(err("CopyTarget command exceeds target size"));
                }
                let src = src as usize;
                if (distance as usize) < length {
                    // overlapping LZ77-style copy via doubling memcpy
                    let mut remaining = length;
                    let mut cur_dst = bytes_written;
                    let mut chunk = distance as usize;
                    while remaining > 0 {
                        let to_copy = chunk.min(remaining);
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                tptr.add(src),
                                tptr.add(cur_dst),
                                to_copy,
                            );
                        }
                        cur_dst += to_copy;
                        remaining -= to_copy;
                        chunk += to_copy;
                    }
                } else {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            tptr.add(src),
                            tptr.add(bytes_written),
                            length,
                        );
                    }
                }
                bytes_written += length;
            }
            x if x == Cmd::Insert as u8 => {
                let length = read_varint(&mut r)? as usize;
                if target_size - bytes_written < length {
                    return Err(err("Invalid insert command."));
                }
                let dst = unsafe {
                    std::slice::from_raw_parts_mut(tptr.add(bytes_written), length)
                };
                r.read_exact(dst)?;
                bytes_written += length;
            }
            _ => return Err(err("Unknown patch command")),
        }
        // Region just written: [start_bw, bytes_written). Bytes are still in the
        // page cache, so hashing here avoids a separate read-back pass.
        let written = unsafe { std::slice::from_raw_parts(tptr.add(start_bw), bytes_written - start_bw) };
        thasher.update(written);
    }

    if !base_hash_job.join().unwrap() {
        return Err(err("This file is not the correct base file."));
    }

    let actual_target: [u8; 32] = thasher.finalize().into();
    if actual_target != header.target_hash {
        return Err(err(
            "Patch applied, but final validation failed. The output file is corrupted.",
        ));
    }
    // Dirty pages are written back asynchronously by the kernel; no synchronous
    // msync (the original doesn't force one either), which keeps wall time low.
    Ok(())
}

fn main() {
    // Accept: dvapply -o <out> <base> <patch>   (and tolerant ordering)
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut out = String::new();
    let mut files: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "-o" | "--output" => {
                i += 1;
                if i < raw.len() {
                    out = raw[i].clone();
                }
            }
            s => files.push(s.to_string()),
        }
        i += 1;
    }
    let patches: Vec<&String> = files.iter().filter(|f| f.ends_with(".dvp")).collect();
    let bases: Vec<&String> = files.iter().filter(|f| !f.ends_with(".dvp")).collect();
    if out.is_empty() || patches.is_empty() || bases.is_empty() {
        eprintln!("Usage: dvapply -o <output> <base> <patch.dvp>");
        std::process::exit(1);
    }
    if let Err(e) = apply(bases[0], patches[0], &out) {
        eprintln!("Error: {e}");
        let _ = std::fs::remove_file(&out);
        std::process::exit(1);
    }
}
