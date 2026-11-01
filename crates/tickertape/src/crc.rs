//! CRC32C (Castagnoli) over the CPU's CRC32C instruction (ADRs 0019 and 0024).
//!
//! When the CPU has the instruction (checked at run time; std caches the result), inputs of any length go through
//! one function compiled with that CPU feature, so every instruction is inlined:
//! - Long inputs run three independent CRC chains over three adjacent blocks at once, which hides the instruction's
//!   latency, then combine them with small shift tables.
//! - The remaining tail uses a plain serial loop.
//!
//! The `crc32c` crate is the fallback without the instruction and under Miri. Results are bit-identical.
//!
//! This is the only module besides `mmap` that contains `unsafe`: calls to CPU intrinsics, which take values, not
//! pointers, each after the feature check that makes it sound.

/// The CRC32C of `data`.
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

/// Bytes per lane in the three-lane loop: long blocks first, then short ones (ADR 0024).
#[cfg_attr(
    any(miri, not(any(target_arch = "aarch64", target_arch = "x86_64"))),
    allow(dead_code)
)]
const LANES: [usize; 2] = [256, 64];

/// For each lane size: `table[j][b]` is the CRC register after feeding that many zero bytes to the register value
/// `b << (8 * j)`. The CRC update is linear, so advancing any register over the lane is four lookups XORed.
#[cfg_attr(
    any(miri, not(any(target_arch = "aarch64", target_arch = "x86_64"))),
    allow(dead_code)
)]
type ShiftTables = [[[u32; 256]; 4]; 2];

/// Advances the CRC register `crc` over `table`'s number of zero bytes.
#[cfg_attr(
    any(miri, not(any(target_arch = "aarch64", target_arch = "x86_64"))),
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
    any(miri, not(any(target_arch = "aarch64", target_arch = "x86_64"))),
    allow(dead_code)
)]
#[inline]
fn word(bytes: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(bytes);
    u64::from_le_bytes(w)
}

/// Defines `update` (raw CRC register over a byte slice) and `build_tables` in an architecture module, from that
/// module's `step` (8 bytes) and `step_byte` (1 byte), all compiled with the CPU feature `$feature`.
#[cfg(all(any(target_arch = "aarch64", target_arch = "x86_64"), not(miri)))]
macro_rules! three_lane_crc {
    ($feature:literal) => {
        /// The shift tables, built once with the CPU instruction itself.
        fn tables() -> &'static super::ShiftTables {
            static TABLES: std::sync::OnceLock<Box<super::ShiftTables>> =
                std::sync::OnceLock::new();
            // SAFETY: only reached from `append`, after it checked that the CPU has the feature `build_tables` needs.
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

        /// Feeds `data` to the raw (already inverted) CRC register `crc`.
        #[target_feature(enable = $feature)]
        #[allow(unused_unsafe)]
        unsafe fn update(mut crc: u32, mut data: &[u8], tables: &super::ShiftTables) -> u32 {
            for (table, lane) in tables.iter().zip(super::LANES) {
                while data.len() >= 3 * lane {
                    let (a, rest) = data.split_at(lane);
                    let (b, rest) = rest.split_at(lane);
                    let (c, rest) = rest.split_at(lane);
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
                    data = rest;
                }
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
    };
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
mod hw {
    use std::arch::aarch64::{__crc32cb, __crc32cd};

    three_lane_crc!("crc");

    /// `None` if the CPU lacks the CRC extension.
    pub(super) fn append(crc: u32, data: &[u8]) -> Option<u32> {
        if !std::arch::is_aarch64_feature_detected!("crc") {
            return None;
        }
        let tables = tables();
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

#[cfg(all(target_arch = "x86_64", not(miri)))]
mod hw {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

    three_lane_crc!("sse4.2");

    /// `None` if the CPU lacks SSE4.2.
    pub(super) fn append(crc: u32, data: &[u8]) -> Option<u32> {
        if !std::arch::is_x86_feature_detected!("sse4.2") {
            return None;
        }
        let tables = tables();
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

#[cfg(any(miri, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
mod hw {
    pub(super) fn append(_crc: u32, _data: &[u8]) -> Option<u32> {
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
        // Every length through two full rounds of both lane sizes, and every serial tail.
        let data: Vec<u8> = (0..2 * 3 * (256 + 64) as u32 + 64)
            .map(|i| (i * 131 + 7) as u8)
            .collect();
        let step = if cfg!(miri) { 61 } else { 1 };
        for len in (0..data.len()).step_by(step) {
            for start in 0..8.min(data.len() - len) {
                let slice = &data[start..start + len];
                assert_eq!(
                    crc32c(slice),
                    crc32c::crc32c(slice),
                    "len {len} start {start}"
                );
            }
        }
    }

    // Apple silicon always has the CRC extension. Elsewhere the hardware path must be used exactly when the CPU
    // reports the feature (older x86-64 CPUs and feature-masked VMs legitimately take the fallback).
    #[cfg(not(miri))]
    #[test]
    fn hardware_path_is_used_when_available() {
        let used = hw::append(0, b"x").is_some();
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(used);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(used, std::arch::is_x86_feature_detected!("sse4.2"));
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        assert_eq!(used, std::arch::is_aarch64_feature_detected!("crc"));
        let _ = used;
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
        fn appending_in_pieces_matches_one_pass(data in proptest::collection::vec(any::<u8>(), 0..5000), cut in any::<usize>()) {
            let cut = cut % (data.len() + 1);
            prop_assert_eq!(crc32c_append(crc32c(&data[..cut]), &data[cut..]), crc32c(&data));
        }
    }
}
