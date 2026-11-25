#![no_main]

//! The seek index decoder on any bytes (ADR 0045): no panic, no allocation, and what it accepts obeys the rules a reader
//! relies on before it looks at the segment. The input is the segment's 64-byte header and then the index; with the
//! first byte's lowest bit set, the header is copied into the index and its checksum computed, so that the fuzzer gets
//! past them.

use libfuzzer_sys::fuzz_target;
use tickertape::format::{SEGMENT_HEADER_LEN, SegmentHeader, encode_segment_header};
use tickertape::seek_index::codec;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, data)) = data.split_first() else {
        return;
    };
    let header: [u8; SEGMENT_HEADER_LEN];
    let mut index: Vec<u8>;
    if mode & 1 == 0 {
        let Some((first, rest)) = data.split_first_chunk::<SEGMENT_HEADER_LEN>() else {
            return;
        };
        header = *first;
        index = rest.to_vec();
    } else {
        header = encode_segment_header(&SegmentHeader {
            segment_size: u64::from(mode >> 1 | 1) * 65_536,
            base_seq: 1_000,
            created_unix_ns: 7,
        });
        index = data.to_vec();
        if index.len() >= codec::HEADER_LEN {
            index[..8].copy_from_slice(&codec::MAGIC);
            index[64..codec::HEADER_LEN].copy_from_slice(&header);
            codec::write_crc(&mut index);
        }
    }
    let Ok(decoded) = codec::decode(&index, &header) else {
        return;
    };
    let (mut end, mut seq, mut block) = (64, None, 0);
    for entry in decoded.entries() {
        assert!(entry.offset % 8 == 0 && entry.offset >= end);
        assert!(entry.offset / u64::from(decoded.spacing) > block);
        assert!(seq.is_none_or(|before| before < entry.seq) && entry.seq < decoded.next_seq);
        assert_eq!(decoded.floor(entry.seq), Some(entry));
        block = entry.offset / u64::from(decoded.spacing);
        end = entry.offset + u64::from(entry.len);
        seq = Some(entry.seq);
    }
    assert!(seq.is_some() && end <= decoded.seal_offset);
    assert_eq!(decoded.floor(0), None);
});
