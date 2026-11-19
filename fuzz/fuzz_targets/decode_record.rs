#![no_main]

use libfuzzer_sys::fuzz_target;
use tickertape::CorruptReason;
use tickertape::format::{
    MAX_FRAME_LEN, RECORD_HEADER_LEN, decode_checkpoint, decode_record_body,
    decode_segment_header, finish_record_body,
};

/// Decodes `body` with the safe reader's decoder and, in fuzzing builds, checks that the zero-copy reader's
/// word-by-word decoder agrees exactly (ADR 0027; the hook exists only with `--cfg fuzzing`, which `cargo fuzz` sets).
fn decode_both(commit: u32, body: &[u8], seq: u64) -> Result<(), CorruptReason> {
    let by_body = decode_record_body(commit, body, seq).map(|(header, _)| header);
    #[cfg(fuzzing)]
    if let Some(by_words) = tickertape::format::decode_record_words_for_fuzzing(commit, body, seq) {
        assert_eq!(by_words, by_body);
    }
    by_body.map(|_| ())
}

fuzz_target!(|data: &[u8]| {
    if let (Some(Ok(commit)), Some(Ok(seq)), Some(body)) = (
        data.get(..4).map(<[u8; 4]>::try_from),
        data.get(4..12).map(<[u8; 8]>::try_from),
        data.get(12..),
    ) {
        let (commit, seq) = (u32::from_le_bytes(commit), u64::from_le_bytes(seq));
        let _ = decode_both(commit, body, seq);
        // The same bytes with the CRC recomputed, so that the checks after it (sequence number, flags) are reached
        // too: damaged input almost never carries a valid CRC by chance.
        if (RECORD_HEADER_LEN as u32..=MAX_FRAME_LEN).contains(&commit)
            && body.len() as u64 + 4 == u64::from(commit)
        {
            let mut fixed = body.to_vec();
            finish_record_body(commit, &mut fixed);
            assert_ne!(
                decode_both(commit, &fixed, seq),
                Err(CorruptReason::BadChecksum)
            );
        }
    }
    let _ = decode_checkpoint(data);
    if let Some(body) = data.get(12..) {
        let _ = decode_checkpoint(body);
    }
    for offset in [0, 12] {
        if let Some(bytes) = data
            .get(offset..offset + 64)
            .and_then(|bytes| bytes.try_into().ok())
        {
            let _ = decode_segment_header(bytes);
        }
    }
});
