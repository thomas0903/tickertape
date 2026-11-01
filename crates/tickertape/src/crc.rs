//! CRC32C (Castagnoli) with a fast path for short inputs (ADR 0019).
//!
//! The `crc32c` crate interleaves three hardware CRC streams, which wins on long inputs but costs a fixed setup per
//! call. Most records are short, so inputs up to [`SHORT_MAX`] bytes use a plain loop over the CPU's CRC32C
//! instruction when it is available (checked at run time; std caches the result). Everything else, and every input
//! under Miri, goes to the crate. Both give bit-identical results.
//!
//! This is the only module besides `mmap` that contains `unsafe`: calls to CPU intrinsics, which take values, not
//! pointers, each after the feature check that makes it sound.

/// Inputs up to this many bytes take the serial hardware loop. Measured on an Apple M-series CPU: the loop was faster
/// than the crate at every length up to 512 bytes and slower at 1056.
const SHORT_MAX: usize = 512;

/// The CRC32C of `data`.
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    crc32c_append(0, data)
}

/// Continues `crc` (a finished CRC32C of earlier bytes) over `data`, as `crc32c::crc32c_append` does.
pub(crate) fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    if data.len() <= SHORT_MAX {
        if let Some(crc) = hw::append(crc, data) {
            return crc;
        }
    }
    crc32c::crc32c_append(crc, data)
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
mod hw {
    use std::arch::aarch64::{__crc32cb, __crc32cd};

    /// `None` if the CPU lacks the CRC extension.
    pub(super) fn append(crc: u32, data: &[u8]) -> Option<u32> {
        if !std::arch::is_aarch64_feature_detected!("crc") {
            return None;
        }
        // SAFETY: `serial` only requires the `crc` target feature, which the CPU has (checked just above).
        Some(unsafe { serial(crc, data) })
    }

    #[target_feature(enable = "crc")]
    // On Rust 1.86 and later the intrinsics are safe inside this function; on the MSRV they are `unsafe fn`s.
    #[allow(unused_unsafe)]
    unsafe fn serial(crc: u32, data: &[u8]) -> u32 {
        let mut crc = !crc;
        let mut words = data.chunks_exact(8);
        for word in &mut words {
            let mut bytes = [0; 8];
            bytes.copy_from_slice(word);
            // SAFETY: this function is compiled with, and only called on CPUs that have, the `crc` feature.
            crc = unsafe { __crc32cd(crc, u64::from_le_bytes(bytes)) };
        }
        for &byte in words.remainder() {
            // SAFETY: as above.
            crc = unsafe { __crc32cb(crc, byte) };
        }
        !crc
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
mod hw {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

    /// `None` if the CPU lacks SSE4.2.
    pub(super) fn append(crc: u32, data: &[u8]) -> Option<u32> {
        if !std::arch::is_x86_feature_detected!("sse4.2") {
            return None;
        }
        // SAFETY: `serial` only requires the `sse4.2` target feature, which the CPU has (checked just above).
        Some(unsafe { serial(crc, data) })
    }

    #[target_feature(enable = "sse4.2")]
    // On Rust 1.86 and later the intrinsics are safe inside this function; on the MSRV they are `unsafe fn`s.
    #[allow(unused_unsafe)]
    unsafe fn serial(crc: u32, data: &[u8]) -> u32 {
        let mut crc = u64::from(!crc);
        let mut words = data.chunks_exact(8);
        for word in &mut words {
            let mut bytes = [0; 8];
            bytes.copy_from_slice(word);
            // SAFETY: this function is compiled with, and only called on CPUs that have, the `sse4.2` feature.
            // The instruction's result fits in the low 32 bits.
            crc = unsafe { _mm_crc32_u64(crc, u64::from_le_bytes(bytes)) };
        }
        let mut crc = crc as u32;
        for &byte in words.remainder() {
            // SAFETY: as above.
            crc = unsafe { _mm_crc32_u8(crc, byte) };
        }
        !crc
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
    fn every_length_around_the_cutoff_matches_the_crate() {
        let data: Vec<u8> = (0..SHORT_MAX as u32 + 64)
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
        fn matches_the_crate(init in any::<u32>(), data in proptest::collection::vec(any::<u8>(), 0..1200)) {
            prop_assert_eq!(crc32c_append(init, &data), crc32c::crc32c_append(init, &data));
        }

        #[test]
        fn appending_in_pieces_matches_one_pass(data in proptest::collection::vec(any::<u8>(), 0..1200), cut in any::<usize>()) {
            let cut = cut % (data.len() + 1);
            prop_assert_eq!(crc32c_append(crc32c(&data[..cut]), &data[cut..]), crc32c(&data));
        }
    }
}
