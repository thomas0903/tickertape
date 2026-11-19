#![no_main]

use libfuzzer_sys::fuzz_target;
use tickertape::format::{decode_checkpoint, decode_record_body, decode_segment_header};

fuzz_target!(|data: &[u8]| {
    if let (Some(Ok(commit)), Some(Ok(seq)), Some(body)) = (
        data.get(..4).map(<[u8; 4]>::try_from),
        data.get(4..12).map(<[u8; 8]>::try_from),
        data.get(12..),
    ) {
        let (commit, seq) = (u32::from_le_bytes(commit), u64::from_le_bytes(seq));
        let by_body = decode_record_body(commit, body, seq).map(|(header, _)| header);
        // The zero-copy reader's word-by-word decoder must agree exactly (ADR 0027). The hook exists only in
        // fuzzing builds (`cargo fuzz` sets `--cfg fuzzing`).
        #[cfg(fuzzing)]
        if let Some(by_words) =
            tickertape::format::decode_record_words_for_fuzzing(commit, body, seq)
        {
            assert_eq!(by_words, by_body);
        }
        let _ = by_body;
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
