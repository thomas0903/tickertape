#![no_main]

use libfuzzer_sys::fuzz_target;
use tickertape::format::{SegmentView, decode_segment_header, read_slot, scan_segment};

fuzz_target!(|data: &[u8]| {
    let base_seq = data
        .get(..64)
        .and_then(|bytes| bytes.try_into().ok())
        .and_then(|bytes| decode_segment_header(bytes).ok())
        .map_or(0, |header| header.base_seq);
    let _ = scan_segment(data, 64, base_seq);
    let _ = read_slot(data, 64, base_seq);
    let _ = read_slot(data, SegmentView::len(data).saturating_sub(8), base_seq);
    for bytes in data.as_chunks::<8>().0.iter().take(4) {
        let offset = u64::from_le_bytes(*bytes);
        let _ = read_slot(data, offset, base_seq);
        let _ = read_slot(data, offset & !7, base_seq);
    }
});
