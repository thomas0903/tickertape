//! CRC32C (Castagnoli) over the CPU's CRC32C instruction (ADRs 0019 and 0024).
//!
//! When the CPU has the instruction (checked at run time; std caches the result), inputs of any length go through
//! one function compiled with that CPU feature, so every instruction is inlined:
//! - Long inputs run three independent CRC chains over three adjacent blocks at once, which hides the instruction's
//!   latency, then combine them with small shift tables.
//! - The remaining tail uses a plain serial loop.
//!
//! The `crc32c` crate is the fallback without the instruction and under Miri. Results are bit-identical. Building
//! with `RUSTFLAGS="--cfg tickertape_force_soft_crc"` compiles out tickertape's own hardware path, so the whole test
//! suite can run on tickertape's fallback code (the calls into `crc32c` and the staging buffer of
//! [`crc32c_append_parts`]) on any machine (CONTRIBUTING.md). It forces nothing in the `crc32c` crate, which still
//! picks its own hardware implementation on CPUs that have the instruction.
//!
//! [`crc32c_append_parts`] computes the same CRC over words loaded one at a time from shared memory
//! ([`SharedWords`]), so the zero-copy reader can check a record without copying it first (ADR 0027): on the
//! hardware path each 8-byte atomic load feeds the CRC instruction directly.
//!
//! This is the only module besides `mmap` that contains `unsafe`: calls to CPU intrinsics, which take values, not
//! pointers, each after the feature check that makes it sound.

use crate::mmap::SharedWords;

/// The CRC32C of `data`.
#[inline(always)] // see `format::finish_record_body`
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    crc32c_append(0, data)
}

/// Continues `crc` (a finished CRC32C of earlier bytes) over `data`, as `crc32c::crc32c_append` does.
pub(crate) fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    match hw::append(crc, data) {
        Some(crc) => crc,
        None => crc32c::crc32c_append(crc, data),
    }
}

/// The CRC32C polynomial in the reflected bit order of a finished CRC.
const POLY: u32 = 0x82f6_3b78;

/// `a * b` modulo the CRC32C polynomial, both in the reflected bit order of a finished CRC (zlib's `multmodp`).
pub(crate) fn mul_mod(a: u32, mut b: u32) -> u32 {
    let mut product = 0;
    let mut bit = 1_u32 << 31;
    while bit != 0 {
        if a & bit != 0 {
            product ^= b;
        }
        b = if b & 1 != 0 { (b >> 1) ^ POLY } else { b >> 1 };
        bit >>= 1;
    }
    product
}

/// `x^(8n)` modulo the polynomial. Moving a CRC past `n` bytes is a multiplication by it:
/// `crc32c(A ‖ B) == mul_mod(x8n(B.len()), crc32c(A)) ^ crc32c(B)` (zlib's `crc32_combine`; CRC32C, like zlib's
/// CRC-32, starts from and finishes with all ones, so those terms cancel).
pub(crate) fn x8n(mut n: u64) -> u32 {
    let mut power = 1_u32 << 31; // x^0
    let mut square = 1_u32 << 23; // x^8, then x^16, x^32, ...: x^(8 * 2^k)
    while n != 0 {
        if n & 1 != 0 {
            power = mul_mod(square, power);
        }
        square = mul_mod(square, square);
        n >>= 1;
    }
    power
}

/// Continues `crc` over `head`, then the bytes of `words` (word `i` is bytes `[8i, 8i + 8)`, little-endian), then
/// `tail`: the same value as [`crc32c_append`] over the concatenation. Each word is loaded exactly once.
///
/// The hardware path below is marked `#[inline]` so that it can be inlined into this function whichever codegen unit each
/// is in. Measured with the default release profile (16 codegen units), mapped reader, 64-byte records, M4 Pro: after the
/// seek index, which does not call this function, had changed how the crate is split, replay was 3 to 4 % slower than
/// before, and level with it again with these attributes.
pub(crate) fn crc32c_append_parts(
    crc: u32,
    head: &[u8],
    words: SharedWords<'_>,
    tail: &[u8],
) -> u32 {
    match hw::append_parts(crc, head, words, tail) {
        Some(crc) => crc,
        None => soft_append_parts(crc, head, words, tail),
    }
}

/// [`crc32c_append_parts`] without tickertape's own hardware path: the words go through a small stack buffer, 256
/// bytes at a time, into the `crc32c` crate (which uses the CPU instruction itself where it has one, including
/// under `--cfg tickertape_force_soft_crc`). This is the only path that copies, and only into that buffer.
fn soft_append_parts(crc: u32, head: &[u8], words: SharedWords<'_>, tail: &[u8]) -> u32 {
    let mut crc = crc32c::crc32c_append(crc, head);
    let mut buf = [0u8; 256];
    let (chunks, rest) = words.chunks_exact(buf.len() / 8);
    for chunk in chunks.chain(std::iter::once(rest)) {
        let mut filled = 0;
        for (dest, word) in buf.chunks_exact_mut(8).zip(chunk.iter()) {
            dest.copy_from_slice(&word.to_le_bytes());
            filled += 8;
        }
        crc = crc32c::crc32c_append(crc, &buf[..filled]);
    }
    crc32c::crc32c_append(crc, tail)
}

/// Bytes per lane in the three-lane loop: long blocks first, then short ones (ADR 0024).
#[cfg_attr(
    any(
        miri,
        tickertape_force_soft_crc,
        not(any(target_arch = "aarch64", target_arch = "x86_64"))
    ),
    allow(dead_code)
)]
const LANES: [usize; 2] = [256, 64];

/// For each lane size: `table[j][b]` is the CRC register after feeding that many zero bytes to the register value
/// `b << (8 * j)`. The CRC update is linear, so advancing any register over the lane is four lookups XORed.
#[cfg_attr(
    any(
        miri,
        tickertape_force_soft_crc,
        not(any(target_arch = "aarch64", target_arch = "x86_64"))
    ),
    allow(dead_code)
)]
type ShiftTables = [[[u32; 256]; 4]; 2];

/// Advances the CRC register `crc` over `table`'s number of zero bytes.
#[cfg_attr(
    any(
        miri,
        tickertape_force_soft_crc,
        not(any(target_arch = "aarch64", target_arch = "x86_64"))
    ),
    allow(dead_code)
)]
#[inline]
fn shift(table: &[[u32; 256]; 4], crc: u32) -> u32 {
    table[0][(crc & 0xff) as usize]
        ^ table[1][((crc >> 8) & 0xff) as usize]
        ^ table[2][((crc >> 16) & 0xff) as usize]
        ^ table[3][(crc >> 24) as usize]
}

#[cfg_attr(
    any(
        miri,
        tickertape_force_soft_crc,
        not(any(target_arch = "aarch64", target_arch = "x86_64"))
    ),
    allow(dead_code)
)]
#[inline]
fn word(bytes: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(bytes);
    u64::from_le_bytes(w)
}

// Test instrumentation: three-lane blocks run on this thread so far, per lane size (`LANES[0]`, `LANES[1]`). Only
// test builds count, so the hot loops are unchanged in the library.
#[cfg(test)]
thread_local! {
    static BLOCKS: std::cell::Cell<[u64; 2]> = const { std::cell::Cell::new([0; 2]) };
}

#[cfg(test)]
#[cfg_attr(
    any(
        miri,
        tickertape_force_soft_crc,
        not(any(target_arch = "aarch64", target_arch = "x86_64"))
    ),
    allow(dead_code)
)]
fn note_block(lane_bytes: usize) {
    BLOCKS.with(|b| {
        let mut counts = b.get();
        counts[usize::from(lane_bytes != LANES[0])] += 1;
        b.set(counts);
    });
}

/// Defines `update` (raw CRC register over a byte slice) and `build_tables` in an architecture module, from that
/// module's `step` (8 bytes) and `step_byte` (1 byte), all compiled with the CPU feature `$feature`.
#[cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(tickertape_force_soft_crc)
))]
macro_rules! three_lane_crc {
    ($feature:literal) => {
        /// The shift tables, built once with the CPU instruction itself.
        fn tables() -> &'static super::ShiftTables {
            static TABLES: std::sync::OnceLock<Box<super::ShiftTables>> =
                std::sync::OnceLock::new();
            // SAFETY: only reached from `append` and `append_parts`, after they checked that the CPU has the feature
            // `build_tables` needs.
            TABLES.get_or_init(|| unsafe { build_tables() })
        }

        #[target_feature(enable = $feature)]
        // On Rust 1.86 and later the intrinsics are safe inside this function; on the MSRV they are `unsafe fn`s.
        #[allow(unused_unsafe)]
        unsafe fn build_tables() -> Box<super::ShiftTables> {
            let mut tables = Box::new([[[0u32; 256]; 4]; 2]);
            for (table, lane) in tables.iter_mut().zip(super::LANES) {
                for (j, row) in table.iter_mut().enumerate() {
                    for (b, entry) in row.iter_mut().enumerate() {
                        let mut crc = (b as u32) << (8 * j);
                        for _ in 0..lane / 8 {
                            // SAFETY: compiled with, and only called on CPUs that have, the feature.
                            crc = unsafe { step(crc, 0) };
                        }
                        *entry = crc;
                    }
                }
            }
            tables
        }

        /// Feeds `data` to the raw (already inverted) CRC register `crc`. Without `tables` (inputs too short for a
        /// three-lane block), only the serial loop runs.
        #[target_feature(enable = $feature)]
        #[allow(unused_unsafe)]
        unsafe fn update(
            mut crc: u32,
            mut data: &[u8],
            tables: Option<&super::ShiftTables>,
        ) -> u32 {
            for (table, lane) in tables.into_iter().flatten().zip(super::LANES) {
                let mut blocks = data.chunks_exact(3 * lane);
                for block in &mut blocks {
                    #[cfg(test)]
                    super::note_block(lane);
                    let (a, rest) = block.split_at(lane);
                    let (b, c) = rest.split_at(lane);
                    let (mut c0, mut c1, mut c2) = (crc, 0u32, 0u32);
                    for ((x, y), z) in a
                        .chunks_exact(8)
                        .zip(b.chunks_exact(8))
                        .zip(c.chunks_exact(8))
                    {
                        // SAFETY: compiled with, and only called on CPUs that have, the feature.
                        unsafe {
                            c0 = step(c0, super::word(x));
                            c1 = step(c1, super::word(y));
                            c2 = step(c2, super::word(z));
                        }
                    }
                    // Lane b started from zero: the register after a and b is a's register advanced over b, XOR b's.
                    crc = super::shift(table, super::shift(table, c0) ^ c1) ^ c2;
                }
                data = blocks.remainder();
            }
            let mut words = data.chunks_exact(8);
            for w in &mut words {
                // SAFETY: as above.
                crc = unsafe { step(crc, super::word(w)) };
            }
            for &byte in words.remainder() {
                // SAFETY: as above.
                crc = unsafe { step_byte(crc, byte) };
            }
            crc
        }

        /// As `update`, over words loaded one at a time from shared memory: each load feeds the CRC instruction
        /// directly, with the same three-lane blocks (in words) and the same serial tail.
        #[target_feature(enable = $feature)]
        #[inline]
        #[allow(unused_unsafe)]
        unsafe fn update_words(
            mut crc: u32,
            mut words: crate::mmap::SharedWords<'_>,
            tables: Option<&super::ShiftTables>,
        ) -> u32 {
            for (table, lane) in tables.into_iter().flatten().zip(super::LANES) {
                let lane = lane / 8;
                let (blocks, rest) = words.chunks_exact(3 * lane);
                for block in blocks {
                    #[cfg(test)]
                    super::note_block(8 * lane);
                    // Each block holds exactly three lanes, so both splits succeed.
                    let Some((a, rest)) = block.split_at_checked(lane) else {
                        continue;
                    };
                    let Some((b, c)) = rest.split_at_checked(lane) else {
                        continue;
                    };
                    let (mut c0, mut c1, mut c2) = (crc, 0u32, 0u32);
                    for ((x, y), z) in a.iter().zip(b.iter()).zip(c.iter()) {
                        // SAFETY: compiled with, and only called on CPUs that have, the feature.
                        unsafe {
                            c0 = step(c0, x);
                            c1 = step(c1, y);
                            c2 = step(c2, z);
                        }
                    }
                    crc = super::shift(table, super::shift(table, c0) ^ c1) ^ c2;
                }
                words = rest;
            }
            for w in words.iter() {
                // SAFETY: as above.
                crc = unsafe { step(crc, w) };
            }
            crc
        }

        /// `update` over `head`, then `update_words` over `words`, then `update` over `tail`, in one function
        /// compiled with the feature.
        #[target_feature(enable = $feature)]
        #[inline]
        #[allow(unused_unsafe)]
        unsafe fn update_parts(
            crc: u32,
            head: &[u8],
            words: crate::mmap::SharedWords<'_>,
            tail: &[u8],
            tables: Option<&super::ShiftTables>,
        ) -> u32 {
            // SAFETY: all three require only the feature, which this function is compiled with and only called with.
            unsafe {
                let crc = update(crc, head, None);
                let crc = update_words(crc, words, tables);
                update(crc, tail, None)
            }
        }

        /// `None` if the CPU lacks the feature; otherwise [`super::crc32c_append_parts`].
        #[inline]
        pub(super) fn append_parts(
            crc: u32,
            head: &[u8],
            words: crate::mmap::SharedWords<'_>,
            tail: &[u8],
        ) -> Option<u32> {
            if !detected() {
                return None;
            }
            // Short runs never reach a three-lane block, so they don't need (or build) the tables.
            let tables = (words.len() * 8 >= 3 * super::LANES[1]).then(tables);
            // SAFETY: `update_parts` only requires the target feature, which the CPU has (checked just above).
            Some(!unsafe { update_parts(!crc, head, words, tail, tables) })
        }
    };
}

#[cfg(all(target_arch = "aarch64", not(miri), not(tickertape_force_soft_crc)))]
mod hw {
    use std::arch::aarch64::{__crc32cb, __crc32cd};

    three_lane_crc!("crc");

    /// Whether the CPU has the CRC extension (std caches the answer).
    #[inline]
    fn detected() -> bool {
        std::arch::is_aarch64_feature_detected!("crc")
    }

    /// `None` if the CPU lacks the CRC extension.
    pub(super) fn append(crc: u32, data: &[u8]) -> Option<u32> {
        if !detected() {
            return None;
        }
        // Short inputs never reach a three-lane block, so they don't need (or build) the tables.
        let tables = (data.len() >= 3 * super::LANES[1]).then(tables);
        // SAFETY: `update` only requires the `crc` target feature, which the CPU has (checked just above).
        Some(!unsafe { update(!crc, data, tables) })
    }

    #[target_feature(enable = "crc")]
    #[inline]
    #[allow(unused_unsafe)]
    unsafe fn step(crc: u32, word: u64) -> u32 {
        // SAFETY: compiled with, and only called on CPUs that have, the `crc` feature.
        unsafe { __crc32cd(crc, word) }
    }

    #[target_feature(enable = "crc")]
    #[inline]
    #[allow(unused_unsafe)]
    unsafe fn step_byte(crc: u32, byte: u8) -> u32 {
        // SAFETY: as above.
        unsafe { __crc32cb(crc, byte) }
    }
}

#[cfg(all(target_arch = "x86_64", not(miri), not(tickertape_force_soft_crc)))]
mod hw {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

    three_lane_crc!("sse4.2");

    /// Whether the CPU has SSE4.2 (std caches the answer).
    #[inline]
    fn detected() -> bool {
        std::arch::is_x86_feature_detected!("sse4.2")
    }

    /// `None` if the CPU lacks SSE4.2.
    pub(super) fn append(crc: u32, data: &[u8]) -> Option<u32> {
        if !detected() {
            return None;
        }
        // Short inputs never reach a three-lane block, so they don't need (or build) the tables.
        let tables = (data.len() >= 3 * super::LANES[1]).then(tables);
        // SAFETY: `update` only requires the `sse4.2` target feature, which the CPU has (checked just above).
        Some(!unsafe { update(!crc, data, tables) })
    }

    #[target_feature(enable = "sse4.2")]
    #[inline]
    #[allow(unused_unsafe)]
    unsafe fn step(crc: u32, word: u64) -> u32 {
        // SAFETY: compiled with, and only called on CPUs that have, the `sse4.2` feature. The instruction's result
        // fits in the low 32 bits.
        unsafe { _mm_crc32_u64(u64::from(crc), word) as u32 }
    }

    #[target_feature(enable = "sse4.2")]
    #[inline]
    #[allow(unused_unsafe)]
    unsafe fn step_byte(crc: u32, byte: u8) -> u32 {
        // SAFETY: as above.
        unsafe { _mm_crc32_u8(crc, byte) }
    }
}

#[cfg(any(
    miri,
    tickertape_force_soft_crc,
    not(any(target_arch = "aarch64", target_arch = "x86_64"))
))]
mod hw {
    pub(super) fn append(_crc: u32, _data: &[u8]) -> Option<u32> {
        None
    }

    pub(super) fn append_parts(
        _crc: u32,
        _head: &[u8],
        _words: crate::mmap::SharedWords<'_>,
        _tail: &[u8],
    ) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn check_value() {
        // RFC 3720, appendix B.4, and the usual "123456789" check value.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xff; 32]), 0x62A8_AB43);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn every_length_through_the_lane_sizes_matches_the_crate() {
        // Every length through two full rounds of both lane sizes plus every serial tail, at all eight offsets.
        // Pseudorandom bytes, so that no two lanes hold the same data (a lane swap must change the result).
        const MAX: usize = 2 * 3 * (256 + 64) + 64;
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let data: Vec<u8> = (0..MAX + 7)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let step = if cfg!(miri) { 61 } else { 1 };
        for len in (0..=MAX).step_by(step) {
            for start in 0..8 {
                let slice = &data[start..start + len];
                assert_eq!(
                    crc32c(slice),
                    crc32c::crc32c(slice),
                    "len {len} start {start}"
                );
            }
        }
    }

    /// `bytes` as atomic words (zero-padded to a whole word), as they would be read from a mapping.
    fn atomics(bytes: &[u8]) -> Vec<std::sync::atomic::AtomicU64> {
        bytes
            .chunks(8)
            .map(|c| {
                let mut w = [0u8; 8];
                w[..c.len()].copy_from_slice(c);
                std::sync::atomic::AtomicU64::new(u64::from_le_bytes(w))
            })
            .collect()
    }

    #[test]
    fn parts_match_the_crate_for_every_word_count() {
        // Every word count through two full rounds of both lane sizes plus every serial tail (the three-lane
        // thresholds are 24 and 96 words), with heads and tails of several lengths, on both paths.
        const MAX_WORDS: usize = (2 * 3 * (256 + 64) + 64) / 8 + 1;
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let data: Vec<u8> = (0..MAX_WORDS * 8 + 64)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let step = if cfg!(miri) { 23 } else { 1 };
        for n in (0..=MAX_WORDS).step_by(step) {
            for (head, tail) in [(0, 0), (36, 0), (36, 1), (36, 7), (3, 5)] {
                let h = &data[..head];
                let w = &data[head..head + 8 * n];
                let t = &data[head + 8 * n..head + 8 * n + tail];
                let words = atomics(w);
                let words = SharedWords::new(&words);
                let whole = &data[..head + 8 * n + tail];
                let init = n as u32 ^ 0x5a5a_5a5a;
                let expected = crc32c::crc32c_append(init, whole);
                assert_eq!(
                    crc32c_append_parts(init, h, words, t),
                    expected,
                    "{n} words, head {head}, tail {tail}"
                );
                assert_eq!(
                    soft_append_parts(init, h, words, t),
                    expected,
                    "software path: {n} words, head {head}, tail {tail}"
                );
            }
        }
    }

    // Apple silicon always has the CRC extension. Elsewhere the hardware path must be used exactly when the CPU
    // reports the feature (older x86-64 CPUs and feature-masked VMs legitimately take the fallback). Both entry points
    // are checked: the slice CRC and the word-fed CRC that the zero-copy reader validates with (ADR 0027), so neither
    // can fall back to software unnoticed.
    #[cfg(not(miri))]
    #[test]
    fn hardware_path_is_used_when_available() {
        let atoms = atomics(&[0x5a; 16]);
        let used = hw::append(0, b"x").is_some();
        let used_parts = hw::append_parts(0, b"x", SharedWords::new(&atoms), b"y").is_some();
        assert_eq!(
            used_parts, used,
            "the word-fed CRC must take the hardware path exactly when the slice CRC does"
        );
        #[cfg(target_arch = "x86_64")]
        let detected = std::arch::is_x86_feature_detected!("sse4.2");
        #[cfg(target_arch = "aarch64")]
        let detected = std::arch::is_aarch64_feature_detected!("crc");
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        let detected = false;
        // The forced fallback build (`--cfg tickertape_force_soft_crc`) has no hardware path.
        assert_eq!(used, detected && !cfg!(tickertape_force_soft_crc));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(detected, "Apple silicon always has the CRC extension");
    }

    /// Performance only, so no result test notices it: once a run is long enough, both entry points build the shift
    /// tables and run the three-lane blocks (ADR 0024), long lanes first, then short ones.
    #[cfg(not(miri))]
    #[test]
    fn three_lane_blocks_run_for_long_inputs() {
        if hw::append(0, b"").is_none() {
            return; // the fallback has no lanes
        }
        let data: Vec<u8> = (0..4000_u32)
            .map(|i| (i.wrapping_mul(7) >> 1) as u8)
            .collect();
        let atoms = atomics(&data);
        let take = || BLOCKS.with(|b| b.replace([0; 2]));
        // (bytes, [long blocks of 3 x 256 bytes, short blocks of 3 x 64 bytes])
        for (len, expected) in [
            (0, [0, 0]),
            (184, [0, 0]),
            (192, [0, 1]),
            (760, [0, 3]),
            (768, [1, 0]),
            (960, [1, 1]),
            (2000, [2, 2]),
            (3840, [5, 0]),
        ] {
            take();
            assert_eq!(crc32c(&data[..len]), crc32c::crc32c(&data[..len]));
            assert_eq!(take(), expected, "slice CRC over {len} bytes");
            let words = SharedWords::new(&atoms[..len / 8]);
            let _ = crc32c_append_parts(0, b"head", words, b"t");
            assert_eq!(take(), expected, "word-fed CRC over {len} bytes");
        }
    }

    fn property_config() -> ProptestConfig {
        let mut config = ProptestConfig::default();
        if cfg!(miri) {
            // As in `format`: bounded under the interpreter, and no filesystem-backed failure persistence.
            config.cases = 2;
            config.failure_persistence = None;
        }
        config
    }

    proptest! {
        #![proptest_config(property_config())]

        #[test]
        fn matches_the_crate(init in any::<u32>(), data in proptest::collection::vec(any::<u8>(), 0..5000)) {
            prop_assert_eq!(crc32c_append(init, &data), crc32c::crc32c_append(init, &data));
        }

        #[test]
        fn moving_a_crc_past_bytes_matches_one_pass(a in proptest::collection::vec(any::<u8>(), 0..300), b in proptest::collection::vec(any::<u8>(), 0..3000)) {
            let mut joined = a.clone();
            joined.extend_from_slice(&b);
            prop_assert_eq!(mul_mod(x8n(b.len() as u64), crc32c(&a)) ^ crc32c(&b), crc32c(&joined));
        }

        #[test]
        fn parts_match_one_pass(init in any::<u32>(), data in proptest::collection::vec(any::<u8>(), 0..5000), cuts in any::<(usize, usize)>()) {
            let head = cuts.0 % (data.len() + 1);
            let words = (data.len() - head) / 8;
            let n = cuts.1 % (words + 1);
            let atoms = atomics(&data[head..head + 8 * n]);
            let parts = crc32c_append_parts(init, &data[..head], SharedWords::new(&atoms), &data[head + 8 * n..]);
            prop_assert_eq!(parts, crc32c::crc32c_append(init, &data));
        }

        #[test]
        fn appending_in_pieces_matches_one_pass(data in proptest::collection::vec(any::<u8>(), 0..5000), cut in any::<usize>()) {
            let cut = cut % (data.len() + 1);
            prop_assert_eq!(crc32c_append(crc32c(&data[..cut]), &data[cut..]), crc32c(&data));
        }
    }
}
