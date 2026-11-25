//! Record encoding shared by the soak test (`tests/soak.rs`) and its helpers (`tt-soak-writer`, `tt-soak-reader`).
//!
//! Every record identifies the writer incarnation ("epoch") that wrote it, and its payload is a pure function of
//! `(seq, epoch)`, so any process can check a record on its own, and records that a recovery discarded (and whose
//! sequence numbers a later writer reused) can be told apart from their replacements.

/// Segment size used by the soak: the smallest allowed, so rolls and retention happen often.
pub const SEGMENT: u64 = 65_536;

/// Segment size of the soak with seek indexes: a segment of one block gets none.
pub const INDEXED_SEGMENT: u64 = 2 * SEGMENT;

/// Set in the environment of the soak's processes when it runs with seek indexes: writers use [`INDEXED_SEGMENT`], and
/// readers for which it is `reader` use the indexes.
pub const INDEXES: &str = "TT_SOAK_INDEXES";

fn mix(mut z: u64) -> u64 {
    // splitmix64
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Payload length of record `seq` from writer `epoch`: mostly 16 to 2,015 bytes, one in 64 records 8,000 to
/// 39,999 bytes.
pub fn payload_len(seq: u64, epoch: u32) -> usize {
    let h = mix(seq ^ (u64::from(epoch) << 40));
    let r = (h >> 8) as usize;
    if h % 64 == 0 {
        8_000 + r % 32_000
    } else {
        16 + r % 2_000
    }
}

/// Writes the payload of record `seq` from writer `epoch` into `out`: `seq` (u64 LE), `epoch` (u32 LE), length
/// (u32 LE), then pseudo-random bytes derived from all three.
pub fn payload(seq: u64, epoch: u32, out: &mut Vec<u8>) {
    let len = payload_len(seq, epoch);
    out.clear();
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&epoch.to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes());
    let mut x = mix(seq.wrapping_mul(0x2545_f491_4f6c_dd1d) ^ u64::from(epoch)) | 1;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let take = (len - out.len()).min(8);
        out.extend_from_slice(&x.to_le_bytes()[..take]);
    }
}

/// Checks a record against the encoding (schema id = epoch, venue timestamp = seq). Returns the epoch.
pub fn check(
    seq: u64,
    schema_id: u32,
    venue_ts_ns: u64,
    data: &[u8],
    scratch: &mut Vec<u8>,
) -> Result<u32, String> {
    let epoch = schema_id;
    if venue_ts_ns != seq {
        return Err(format!(
            "record {seq}: venue_ts_ns {venue_ts_ns} != seq (epoch {epoch})"
        ));
    }
    payload(seq, epoch, scratch);
    if data != scratch.as_slice() {
        return Err(format!(
            "record {seq}: payload of {} bytes differs from the expected {} bytes for epoch {epoch}",
            data.len(),
            scratch.len()
        ));
    }
    Ok(epoch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trip() {
        let mut a = Vec::new();
        let mut s = Vec::new();
        for seq in 0..2_000 {
            payload(seq, 3, &mut a);
            assert_eq!(a.len(), payload_len(seq, 3));
            assert_eq!(check(seq, 3, seq, &a, &mut s), Ok(3));
            assert!(check(seq, 4, seq, &a, &mut s).is_err());
            assert!(check(seq + 1, 3, seq + 1, &a, &mut s).is_err());
        }
    }
}
