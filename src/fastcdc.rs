//! FastCDC content-defined chunking — direct port of LibDumbVersion/FastCDC.cs.

pub const MIN_SIZE: usize = 64;
pub const AVG_SIZE: usize = 256;
pub const MAX_SIZE: usize = 1024;

const MASK_S: u32 = 0x7F;
const MASK_L: u32 = 0xFF;
const CENTER_SIZE: usize = AVG_SIZE - (MIN_SIZE + ((MIN_SIZE + 1) / 2));

/// Runtime-tunable FastCDC parameters, so the size/speed frontier can be swept
/// without recompiling. `from_env` reproduces the original constants by default.
#[derive(Clone, Copy)]
pub struct Params {
    pub min: usize,
    pub max: usize,
    pub center: usize,
    pub mask_s: u32,
    pub mask_l: u32,
    pub avg: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            min: MIN_SIZE,
            max: MAX_SIZE,
            center: CENTER_SIZE,
            mask_s: MASK_S,
            mask_l: MASK_L,
            avg: AVG_SIZE,
        }
    }
}

impl Params {
    /// Derive normalized FastCDC params from a power-of-two average (`avg = 2^bits`):
    /// min = avg/4, max = avg*4, easier `mask_s` (bits-1 ones) before `center`,
    /// harder `mask_l` (bits ones) after — matching the original's 0x7F/0xFF ratio.
    pub fn from_bits(bits: u32) -> Self {
        let avg = 1usize << bits;
        let min = avg / 4;
        Params {
            min,
            max: avg * 4,
            center: avg - (min + (min + 1) / 2),
            mask_s: (1u32 << (bits - 1)) - 1,
            mask_l: (1u32 << bits) - 1,
            avg,
        }
    }

    pub fn from_env() -> Self {
        match std::env::var("DV_CDC_BITS").ok().and_then(|v| v.parse::<u32>().ok()) {
            Some(8) | None => Params::default(),
            Some(b) => Params::from_bits(b),
        }
    }
}

/// Returns the chunk length at `data` (len = available bytes, capped at MAX_SIZE).
#[inline]
pub fn cdc_offset(data: &[u8]) -> usize {
    cdc_offset_p(data, &Params::default())
}

#[inline]
pub fn cdc_offset_p(data: &[u8], p: &Params) -> usize {
    let len = data.len();
    let mut hash: u32 = 0;
    let mut i = 0usize;
    let mut barrier = p.center.min(len);

    while i < barrier {
        hash = (hash >> 1).wrapping_add(GEAR[data[i] as usize]);
        i += 1;
        if i >= p.min && (hash & p.mask_s) == 0 {
            return i;
        }
    }

    barrier = p.max.min(len);
    while i < barrier {
        hash = (hash >> 1).wrapping_add(GEAR[data[i] as usize]);
        i += 1;
        if (hash & p.mask_l) == 0 {
            return i;
        }
    }

    i
}

/// AE (Asymmetric Extremum) content-defined chunking — a different family from
/// FastCDC. Cuts at the first byte that is a strict maximum over the preceding
/// `window` bytes (interrupting only when a larger value appears). No rolling
/// hash, no masks. `w` controls the expected chunk size (~ e*w).
#[inline]
pub fn ae_offset(data: &[u8], w: usize, max: usize) -> usize {
    let len = data.len().min(max);
    if len == 0 {
        return 0;
    }
    let mut max_val = data[0];
    let mut max_pos = 0usize;
    let mut i = 1usize;
    while i < len {
        let v = data[i];
        if v <= max_val {
            if i == max_pos + w {
                return i + 1;
            }
        } else {
            max_val = v;
            max_pos = i;
        }
        i += 1;
    }
    len
}

#[inline]
pub fn compute_hash(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

pub static GEAR: [u32; 256] = [
    0x5c95c078, 0x22408989, 0x2d48a214, 0x12842087, 0x530f8afb, 0x474536b9, 0x2963b4f1, 0x44cb738b,
    0x4ea7403d, 0x4d606b6e, 0x074ec5d3, 0x3af39d18, 0x726003ca, 0x37a62a74, 0x51a2f58e, 0x7506358e,
    0x5d4ab128, 0x4d4ae17b, 0x41e85924, 0x470c36f7, 0x4741cbe1, 0x01bb7f30, 0x617c1de3, 0x2b0c3a1f,
    0x50c48f73, 0x21a82d37, 0x6095ace0, 0x419167a0, 0x3caf49b0, 0x40cea62d, 0x66bc1c66, 0x545e1dad,
    0x2bfa77cd, 0x6e85da24, 0x5fb0bdc5, 0x652cfc29, 0x3a0ae1ab, 0x2837e0f3, 0x6387b70e, 0x13176012,
    0x4362c2bb, 0x66d8f4b1, 0x37fce834, 0x2c9cd386, 0x21144296, 0x627268a8, 0x650df537, 0x2805d579,
    0x3b21ebbd, 0x7357ed34, 0x3f58b583, 0x7150ddca, 0x7362225e, 0x620a6070, 0x2c5ef529, 0x7b522466,
    0x768b78c0, 0x4b54e51e, 0x75fa07e5, 0x06a35fc6, 0x30b71024, 0x1c8626e1, 0x296ad578, 0x28d7be2e,
    0x1490a05a, 0x7cee43bd, 0x698b56e3, 0x09dc0126, 0x4ed6df6e, 0x02c1bfc7, 0x2a59ad53, 0x29c0e434,
    0x7d6c5278, 0x507940a7, 0x5ef6ba93, 0x68b6af1e, 0x46537276, 0x611bc766, 0x155c587d, 0x301ba847,
    0x2cc9dda7, 0x0a438e2c, 0x0a69d514, 0x744c72d3, 0x4f326b9b, 0x7ef34286, 0x4a0ef8a7, 0x6ae06ebe,
    0x669c5372, 0x12402dcb, 0x5feae99d, 0x76c7f4a7, 0x6abdb79c, 0x0dfaa038, 0x20e2282c, 0x730ed48b,
    0x069dac2f, 0x168ecf3e, 0x2610e61f, 0x2c512c8e, 0x15fb8c06, 0x5e62bc76, 0x69555135, 0x0adb864c,
    0x4268f914, 0x349ab3aa, 0x20edfdb2, 0x51727981, 0x37b4b3d8, 0x5dd17522, 0x6b2cbfe4, 0x5c47cf9f,
    0x30fa1ccd, 0x23dedb56, 0x13d1f50a, 0x64eddee7, 0x0820b0f7, 0x46e07308, 0x1e2d1dfd, 0x17b06c32,
    0x250036d8, 0x284dbf34, 0x68292ee0, 0x362ec87c, 0x087cb1eb, 0x76b46720, 0x104130db, 0x71966387,
    0x482dc43f, 0x2388ef25, 0x524144e1, 0x44bd834e, 0x448e7da3, 0x3fa6eaf9, 0x3cda215c, 0x3a500cf3,
    0x395cb432, 0x5195129f, 0x43945f87, 0x51862ca4, 0x56ea8ff1, 0x201034dc, 0x4d328ff5, 0x7d73a909,
    0x6234d379, 0x64cfbf9c, 0x36f6589a, 0x0a2ce98a, 0x5fe4d971, 0x03bc15c5, 0x44021d33, 0x16c1932b,
    0x37503614, 0x1acaf69d, 0x3f03b779, 0x49e61a03, 0x1f52d7ea, 0x1c6ddd5c, 0x062218ce, 0x07e7a11a,
    0x1905757a, 0x7ce00a53, 0x49f44f29, 0x4bcc70b5, 0x39feea55, 0x5242cee8, 0x3ce56b85, 0x00b81672,
    0x46beeccc, 0x3ca0ad56, 0x2396cee8, 0x78547f40, 0x6b08089b, 0x66a56751, 0x781e7e46, 0x1e2cf856,
    0x3bc13591, 0x494a4202, 0x520494d7, 0x2d87459a, 0x757555b6, 0x42284cc1, 0x1f478507, 0x75c95dff,
    0x35ff8dd7, 0x4e4757ed, 0x2e11f88c, 0x5e1b5048, 0x420e6699, 0x226b0695, 0x4d1679b4, 0x5a22646f,
    0x161d1131, 0x125c68d9, 0x1313e32e, 0x4aa85724, 0x21dc7ec1, 0x4ffa29fe, 0x72968382, 0x1ca8eef3,
    0x3f3b1c28, 0x39c2fb6c, 0x6d76493f, 0x7a22a62e, 0x789b1c2a, 0x16e0cb53, 0x7deceeeb, 0x0dc7e1c6,
    0x5c75bf3d, 0x52218333, 0x106de4d6, 0x7dc64422, 0x65590ff4, 0x2c02ec30, 0x64a9ac67, 0x59cab2e9,
    0x4a21d2f3, 0x0f616e57, 0x23b54ee8, 0x02730aaa, 0x2f3c634d, 0x7117fc6c, 0x01ac6f05, 0x5a9ed20c,
    0x158c4e2a, 0x42b699f0, 0x0c7c14b3, 0x02bd9641, 0x15ad56fc, 0x1c722f60, 0x7da1af91, 0x23e0dbcb,
    0x0e93e12b, 0x64b2791d, 0x440d2476, 0x588ea8dd, 0x4665a658, 0x7446c418, 0x1877a774, 0x5626407e,
    0x7f63bd46, 0x32d2dbd8, 0x3c790f4a, 0x772b7239, 0x6f8b2826, 0x677ff609, 0x0dc82c11, 0x23ffe354,
    0x2eac53a6, 0x16139e09, 0x0afd0dbc, 0x2a4d4237, 0x56a368c7, 0x234325e4, 0x2dce9187, 0x32e8ea7e,
];
