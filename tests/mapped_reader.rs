//! The zero-copy reader validates exactly as the safe reader does (ADR 0027): the same records accepted, the same
//! records rejected, the same errors. Checked over every payload length up to a few KiB, records that straddle page
//! boundaries or end exactly at the end of a segment's record area, every corruption class, and random journals.
//!
//! Files are only corrupted while no reader has them open, as `Reader::open_mapped`'s safety contract requires.

use std::fs;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use tickertape::format::{self, RecordHeader};
use tickertape::{CorruptReason, Error, NewRecord, OwnedRecord, Reader, ReaderConfig, Writer};
use tickertape_tests::{SMALL_SEGMENT, open_raw, payload, segments, small_config};

/// What a reader returned, in order, until the first empty slot or error.
#[derive(Debug, PartialEq, Eq)]
struct Trace {
    records: Vec<OwnedRecord>,
    /// `None`: stopped at an empty slot. `Some(debug)`: the error.
    end: Option<String>,
}

fn trace(dir: &Path, mapped: bool) -> Trace {
    let opened = if mapped {
        // SAFETY: nothing modifies the journal while this reader exists (corruption happens before it is opened).
        unsafe { Reader::open_mapped(dir, ReaderConfig::default()) }
    } else {
        Reader::open(dir, ReaderConfig::default())
    };
    let mut reader = match opened {
        Ok(reader) => reader,
        Err(e) => {
            return Trace {
                records: Vec::new(),
                end: Some(format!("open: {e:?}")),
            };
        }
    };
    let mut records = Vec::new();
    loop {
        match reader.next_owned() {
            Ok(Some(record)) => records.push(record),
            Ok(None) => return Trace { records, end: None },
            Err(e) => {
                return Trace {
                    records,
                    end: Some(format!("{e:?}")),
                };
            }
        }
    }
}

/// Both readers' traces, asserted equal.
fn both(dir: &Path) -> Trace {
    let safe = trace(dir, false);
    let mapped = trace(dir, true);
    assert_eq!(
        mapped, safe,
        "mapped reader differs from the safe reader in {dir:?}"
    );
    mapped
}

fn record_for(seq: u64, len: usize) -> (u32, u64, u64, Vec<u8>) {
    (
        (seq % 7) as u32,
        seq.wrapping_mul(0x9e37_79b9_7f4a_7c15),
        seq ^ 0xa5a5,
        payload(seq, len),
    )
}

fn append(writer: &mut Writer, len: usize) {
    let seq = writer.next_seq();
    let (schema_id, venue_ts_ns, local_ts_ns, data) = record_for(seq, len);
    writer
        .append(&NewRecord {
            schema_id,
            venue_ts_ns,
            local_ts_ns,
            payload: &data,
        })
        .expect("append");
}

/// A record found by walking a segment file: its offset, frame length and header.
#[derive(Debug, Clone, Copy)]
struct Placed {
    offset: u64,
    len: u32,
    header: RecordHeader,
}

/// Every record in a segment file, in order, and the offset where the walk stopped.
fn walk(path: &Path) -> (Vec<Placed>, u64) {
    let image = fs::read(path).unwrap();
    let header = format::decode_segment_header(image[..64].try_into().unwrap()).unwrap();
    let mut offset = format::SEGMENT_HEADER_LEN as u64;
    let mut seq = header.base_seq;
    let mut out = Vec::new();
    while let Ok(format::Slot::Record {
        header,
        len,
        next_offset,
    }) = format::read_slot(&image[..], offset, seq)
    {
        out.push(Placed {
            offset,
            len,
            header,
        });
        offset = next_offset;
        seq += 1;
    }
    (out, offset)
}

fn check_contents(trace: &Trace, lens: &[usize]) {
    assert_eq!(trace.records.len(), lens.len());
    for (record, &len) in trace.records.iter().zip(lens) {
        let (schema_id, venue_ts_ns, local_ts_ns, data) = record_for(record.seq, len);
        assert_eq!(
            (record.schema_id, record.venue_ts_ns, record.local_ts_ns),
            (schema_id, venue_ts_ns, local_ts_ns),
            "header of {}",
            record.seq
        );
        assert_eq!(record.payload, data, "payload of {}", record.seq);
    }
}

#[test]
fn every_payload_length_page_boundaries_and_segment_ends() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let mut writer = Writer::open(&journal, small_config()).unwrap();
    // Every payload length from 0 to 4,200 bytes: each alignment of the last word, every length around the CRC's
    // three-lane thresholds (192 and 768 bytes of words), and, across 64 KiB segments, many records that straddle
    // 4 KiB and 16 KiB page boundaries and end segments.
    let mut lens: Vec<usize> = (0..=4200).collect();
    for &len in &lens {
        append(&mut writer, len);
    }
    // Then fill the current segment's record area exactly (the next record must end at `segment_size - 8`), so the
    // following roll puts the seal in the trailer.
    let (_, last) = segments(&journal).pop().unwrap();
    drop(writer);
    let (_, end) = walk(&last);
    let mut writer = Writer::open(&journal, small_config()).unwrap();
    let room = (SMALL_SEGMENT - 8 - end) as usize;
    let fill = if room >= 40 {
        room - 40
    } else {
        // Not enough room for a record: a 1,000-byte one rolls into a new segment, then fill that one.
        append(&mut writer, 1000);
        lens.push(1000);
        (SMALL_SEGMENT - 8 - 64 - align8(40 + 1000)) as usize - 40
    };
    append(&mut writer, fill);
    lens.push(fill);
    // The largest payload a segment holds, in a segment of its own, then a few short records after it.
    let max = format::max_payload_len(SMALL_SEGMENT) as usize;
    append(&mut writer, max);
    lens.push(max);
    for len in [0, 1, 7, 8, 9] {
        append(&mut writer, len);
        lens.push(len);
    }
    drop(writer);

    let trace = both(&journal);
    assert_eq!(trace.end, None);
    check_contents(&trace, &lens);

    // The journal really has the shapes this test is about.
    let mut straddles_4k = 0;
    let mut straddles_16k = 0;
    let mut ends_area = 0;
    let mut whole_area = 0;
    for (_, path) in segments(&journal) {
        for placed in walk(&path).0 {
            let end = placed.offset + u64::from(placed.len);
            straddles_4k += usize::from(placed.offset / 4096 != (end - 1) / 4096);
            straddles_16k += usize::from(placed.offset / 16384 != (end - 1) / 16384);
            ends_area +=
                usize::from(placed.offset + align8(u64::from(placed.len)) == SMALL_SEGMENT - 8);
            whole_area += usize::from(placed.header.seq > 0 && placed.len as usize == max + 40);
        }
    }
    assert!(
        straddles_4k > 1000,
        "{straddles_4k} records straddle a 4 KiB page"
    );
    assert!(
        straddles_16k > 300,
        "{straddles_16k} records straddle a 16 KiB page"
    );
    assert!(
        ends_area >= 2,
        "{ends_area} records end exactly at the record area's end"
    );
    assert_eq!(whole_area, 1);
}

fn align8(n: u64) -> u64 {
    format::align8(n)
}

/// Payload lengths at and around the CRC's three-lane thresholds (192 and 768 bytes of payload words) and well past
/// them, so that corruption also lands in records the three-lane loop validates.
const LANE_LENS: [usize; 7] = [191, 192, 767, 768, 800, 1300, 2000];

/// A journal of three 64 KiB segments (two sealed) with varied payload lengths, closed cleanly. Records 10 to 16 have
/// the payload lengths in [`LANE_LENS`].
fn base_journal(root: &Path) -> (PathBuf, Vec<usize>) {
    let journal = root.join("base");
    let mut writer = Writer::open(&journal, small_config()).unwrap();
    let mut lens = Vec::new();
    let mut seq = 0u64;
    while segments(&journal).len() < 3 || lens.len() < 400 {
        let len = match seq.checked_sub(10) {
            Some(i) if (i as usize) < LANE_LENS.len() => LANE_LENS[i as usize],
            _ => ((seq * 131) % 700) as usize,
        };
        append(&mut writer, len);
        lens.push(len);
        seq += 1;
    }
    drop(writer);
    (journal, lens)
}

/// A copy of `from` at `root/name`.
fn copy_journal(from: &Path, root: &Path, name: &str) -> PathBuf {
    let to = root.join(name);
    fs::create_dir(&to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
    to
}

fn write_at(path: &Path, offset: u64, bytes: &[u8]) {
    open_raw(path).write_all_at(bytes, offset).unwrap();
}

fn read_at(path: &Path, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0; len];
    open_raw(path).read_exact_at(&mut buf, offset).unwrap();
    buf
}

/// Rewrites the body of the record at `placed` with `edit` applied to it, then recomputes its CRC, so that only the
/// edited field is wrong.
fn rewrite_with_valid_crc(path: &Path, placed: Placed, edit: impl FnOnce(&mut [u8])) {
    let mut body = read_at(path, placed.offset + 4, placed.len as usize - 4);
    edit(&mut body);
    format::finish_record_body(placed.len, &mut body);
    write_at(path, placed.offset + 4, &body);
}

/// What the readers must report for one corruption case.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// Every record before `seq`, then the given reason at the record's offset.
    Corrupt(u64, fn(&CorruptReason) -> bool),
    /// Every record before `seq`, then an empty slot.
    EndAt(u64),
    /// Every record, unchanged.
    Unchanged,
}

fn check_expect(trace: &Trace, expect: Expect, total: usize, case: &str) {
    match expect {
        Expect::Unchanged => {
            assert_eq!(trace.end, None, "{case}");
            assert_eq!(trace.records.len(), total, "{case}");
        }
        Expect::EndAt(seq) => {
            assert_eq!(trace.end, None, "{case}");
            assert_eq!(trace.records.len() as u64, seq, "{case}");
        }
        Expect::Corrupt(seq, _) => {
            assert_eq!(trace.records.len() as u64, seq, "{case}: {:?}", trace.end);
            let end = trace.end.as_deref().unwrap_or("");
            assert!(end.starts_with("Corrupt"), "{case}: {end}");
        }
    }
}

#[test]
fn every_corruption_class_is_reported_identically() {
    let root = tempfile::tempdir().unwrap();
    let (base, lens) = base_journal(root.path());
    let total = lens.len();
    assert_eq!(both(&base).records.len(), total);
    let segs = segments(&base);
    let (first, _) = walk(&segs[0].1);
    let (middle, middle_end) = walk(&segs[1].1);
    let (last, _) = walk(&segs[segs.len() - 1].1);
    // Targets: a record in the middle of a segment whose length is not a multiple of 8 (so it has padding), the last
    // record before a seal, the first record of a segment, and the journal's last record.
    let mid = *first
        .iter()
        .skip(5)
        .find(|p| p.len % 8 != 0 && p.len > 48)
        .unwrap();
    let before_seal = *middle.last().unwrap();
    let segment_start = middle[0];
    let tail = *last.last().unwrap();
    let lane_records: Vec<(String, Placed)> = LANE_LENS
        .iter()
        .map(|&n| {
            let placed = *first
                .iter()
                .find(|p| p.len as usize == n + format::RECORD_HEADER_LEN)
                .unwrap();
            (format!("payload {n}"), placed)
        })
        .collect();

    type Damage = Box<dyn Fn(&Path, Placed) + Send + Sync>;
    type Case = (String, PathBuf, Placed, Damage, Expect);
    let mut cases: Vec<Case> = Vec::new();
    let mut add = |name: String, path: &PathBuf, placed: Placed, f: Damage, e: Expect| {
        cases.push((name, path.clone(), placed, f, e));
    };
    let mut targets: Vec<(String, &PathBuf, Placed)> = vec![
        ("mid".into(), &segs[0].1, mid),
        ("before-seal".into(), &segs[1].1, before_seal),
        ("segment-start".into(), &segs[1].1, segment_start),
        ("tail".into(), &segs[segs.len() - 1].1, tail),
    ];
    for (label, placed) in &lane_records {
        targets.push((label.clone(), &segs[0].1, *placed));
    }
    for (label, path, placed) in targets {
        let seq = placed.header.seq;
        let bad = |s: u64| Expect::Corrupt(s, |_| true);
        // Commit word states.
        add(
            format!("{label}: commit word 0"),
            path,
            placed,
            Box::new(|p, at| write_at(p, at.offset, &0u32.to_le_bytes())),
            Expect::EndAt(seq),
        );
        for word in [
            1u32,
            39,
            40,
            u32::MAX - 1,
            (SMALL_SEGMENT as u32) - 8,
            placed.len + 8,
            placed.len - 1,
            placed.len + 1,
        ] {
            if word != placed.len {
                add(
                    format!("{label}: commit word {word}"),
                    path,
                    placed,
                    Box::new(move |p, at| write_at(p, at.offset, &word.to_le_bytes())),
                    bad(seq),
                );
            }
        }
        // A frame that ends exactly at the end of the segment covers the 8-byte trailer, which never holds a record:
        // a bad length, not a checksum failure. One word shorter, it ends at the trailer and is a valid length, so
        // the CRC decides.
        let to_end = (SMALL_SEGMENT - placed.offset) as u32;
        add(
            format!("{label}: commit word reaching the segment end ({to_end})"),
            path,
            placed,
            Box::new(move |p, at| write_at(p, at.offset, &to_end.to_le_bytes())),
            Expect::Corrupt(seq, |r| matches!(r, CorruptReason::BadLength(_))),
        );
        let to_trailer = to_end - 8;
        if to_trailer != placed.len {
            add(
                format!("{label}: commit word reaching the trailer ({to_trailer})"),
                path,
                placed,
                Box::new(move |p, at| write_at(p, at.offset, &to_trailer.to_le_bytes())),
                Expect::Corrupt(seq, |r| matches!(r, CorruptReason::BadChecksum)),
            );
        }
        add(
            format!("{label}: seal marker with a wrong CRC"),
            path,
            placed,
            Box::new(|p, at| write_at(p, at.offset, &u32::MAX.to_le_bytes())),
            bad(seq),
        );
        // Every byte of the CRC field and the header fields, and payload bytes at each end and at word boundaries.
        let body_len = placed.len as usize - 4;
        // Every header byte for the first four targets; the records at the lane lengths add only a sample of them,
        // since the header is checked the same way whatever the length, and concentrate on the payload.
        let mut positions: Vec<usize> = if label.starts_with("payload") {
            vec![0, 3, 4, 11, 12, 16, 19, 20, 35]
        } else {
            (0..36).collect()
        };
        // Payload bytes: the first and last ones, word boundaries, the three-lane thresholds and block boundaries
        // (a short block is 3 x 64 bytes, a long one 3 x 256), and every 64th byte of long payloads, so each lane of
        // each block is hit.
        let mut payload_bytes = vec![0, 1, 7, 8, 9, 63, 64, 191, 192, 193, 255, 256, 511, 512];
        payload_bytes.extend([767, 768, 769, 959, 960, 1535, 1536]);
        payload_bytes.extend((0..body_len).step_by(64));
        for p in payload_bytes.into_iter().map(|p| 36 + p).chain([
            body_len.saturating_sub(9),
            body_len - 8,
            body_len - 1,
        ]) {
            if p >= 36 && p < body_len && !positions.contains(&p) {
                positions.push(p);
            }
        }
        for p in positions {
            add(
                format!("{label}: body byte {p}"),
                path,
                placed,
                Box::new(move |f, at| {
                    let at_byte = at.offset + 4 + p as u64;
                    let b = read_at(f, at_byte, 1)[0];
                    write_at(f, at_byte, &[b ^ 0x10]);
                }),
                bad(seq),
            );
        }
        // Padding is not covered by the CRC: both readers accept it.
        for p in body_len..(align8(u64::from(placed.len)) as usize - 4) {
            add(
                format!("{label}: padding byte {p}"),
                path,
                placed,
                Box::new(move |f, at| {
                    write_at(f, at.offset + 4 + p as u64, &[0xee]);
                }),
                Expect::Unchanged,
            );
        }
        // Wrong fields under a valid CRC.
        add(
            format!("{label}: wrong seq, valid CRC"),
            path,
            placed,
            Box::new(|f, at| rewrite_with_valid_crc(f, at, |b| b[4] ^= 1)),
            Expect::Corrupt(seq, |r| matches!(r, CorruptReason::BadSequence { .. })),
        );
        // Each flag byte, lowest and highest bit, so no flag byte can be ignored.
        for byte in 16..20 {
            for bit in [0x01_u8, 0x80] {
                add(
                    format!("{label}: flag byte {byte} = {bit:#04x}, valid CRC"),
                    path,
                    placed,
                    Box::new(move |f, at| rewrite_with_valid_crc(f, at, |b| b[byte] = bit)),
                    Expect::Corrupt(seq, |r| matches!(r, CorruptReason::BadFlags)),
                );
            }
        }
        // Both a wrong sequence number and a flag: the sequence number is checked first, by both readers.
        add(
            format!("{label}: wrong seq and flags, valid CRC"),
            path,
            placed,
            Box::new(|f, at| {
                rewrite_with_valid_crc(f, at, |b| {
                    b[4] ^= 1;
                    b[17] = 0x10;
                })
            }),
            Expect::Corrupt(seq, |r| matches!(r, CorruptReason::BadSequence { .. })),
        );
        // Torn: the commit word is there but the end of the body, or all of it, is not.
        add(
            format!("{label}: torn last word"),
            path,
            placed,
            Box::new(|f, at| {
                let end = at.offset + u64::from(at.len);
                write_at(f, end - 8, &[0; 8]);
            }),
            bad(seq),
        );
        add(
            format!("{label}: body all zero"),
            path,
            placed,
            Box::new(|f, at| write_at(f, at.offset + 4, &vec![0; at.len as usize - 4])),
            bad(seq),
        );
    }
    // Seals: a wrong CRC, a seal at the first slot, and a valid seal whose successor is missing.
    let seal_path = segs[1].1.clone();
    add(
        "seal: wrong CRC".into(),
        &seal_path,
        before_seal,
        Box::new(move |f, _| {
            let b = read_at(f, middle_end + 4, 1)[0];
            write_at(f, middle_end + 4, &[b ^ 1]);
        }),
        Expect::Corrupt(before_seal.header.seq + 1, |r| {
            matches!(r, CorruptReason::BadSeal)
        }),
    );
    add(
        "seal: CRC of the next sequence number".into(),
        &seal_path,
        before_seal,
        Box::new(move |f, at| {
            let crc = format::seal_crc(at.header.seq + 2);
            write_at(f, middle_end + 4, &crc.to_le_bytes());
        }),
        Expect::Corrupt(before_seal.header.seq + 1, |r| {
            matches!(r, CorruptReason::BadSeal)
        }),
    );
    add(
        "seal: at the first slot".into(),
        &seal_path,
        segment_start,
        Box::new(|f, at| {
            write_at(f, at.offset, &u32::MAX.to_le_bytes());
            write_at(
                f,
                at.offset + 4,
                &format::seal_crc(at.header.seq).to_le_bytes(),
            );
        }),
        Expect::Corrupt(segment_start.header.seq, |r| {
            matches!(r, CorruptReason::BadSeal)
        }),
    );
    let successor = segs[2].1.clone();
    add(
        "seal: successor missing".into(),
        &seal_path,
        before_seal,
        Box::new(move |f, _| {
            let name = successor.file_name().unwrap().to_owned();
            fs::remove_file(f.parent().unwrap().join(name)).unwrap();
        }),
        Expect::Corrupt(before_seal.header.seq + 1, |r| {
            matches!(r, CorruptReason::BrokenChain(_))
        }),
    );

    let run = |i: usize, (name, path, placed, corrupt, expect): &Case| {
        let (placed, expect) = (*placed, *expect);
        let dir = copy_journal(&base, root.path(), &format!("case{i}"));
        let file = dir.join(path.file_name().unwrap());
        corrupt(&file, placed);
        let trace = both(&dir);
        check_expect(&trace, expect, total, name);
        if let Expect::Corrupt(_, reason) = expect {
            // Re-read the typed error from the mapped reader (the trace keeps only its text).
            // SAFETY: nothing modifies the journal while this reader exists.
            let mut r = unsafe { Reader::open_mapped(&dir, ReaderConfig::default()) }.unwrap();
            let err = loop {
                match r.next_owned() {
                    Ok(Some(_)) => continue,
                    Ok(None) => panic!("{name}: no error"),
                    Err(e) => break e,
                }
            };
            match &err {
                Error::Corrupt { reason: got, .. } => assert!(reason(got), "{name}: {got:?}"),
                other => panic!("{name}: {other:?}"),
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    };
    // Each case works on its own copy of the journal, so they run on several threads.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(case) = cases.get(i) else { break };
                    run(i, case);
                }
            });
        }
    });
}

/// Builds a journal from `lens`, applies `damage` (segment index, offset in the file, XOR mask, or zero a range),
/// and returns its directory.
fn random_journal(root: &Path, lens: &[usize], damage: &[(usize, u64, u8, bool)]) -> PathBuf {
    let journal = root.join("j");
    let mut writer = Writer::open(&journal, small_config()).unwrap();
    for &len in lens {
        append(&mut writer, len);
    }
    drop(writer);
    let segs = segments(&journal);
    for &(seg, offset, mask, zero) in damage {
        let path = &segs[seg % segs.len()].1;
        // Never the segment header: both readers parse it the same way, with `pread`, before any mapping exists.
        let offset = 64 + offset % (SMALL_SEGMENT - 64);
        if zero {
            let len = (SMALL_SEGMENT - offset).min(u64::from(mask) + 1);
            write_at(path, offset, &vec![0; len as usize]);
        } else {
            let b = read_at(path, offset, 1)[0];
            write_at(path, offset, &[b ^ mask.max(1)]);
        }
    }
    journal
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn mapped_and_safe_readers_agree_on_random_journals(
        lens in prop::collection::vec(prop_oneof![0_usize..64, 0_usize..1100, 0_usize..5000], 1..120),
        damage in prop::collection::vec((any::<usize>(), any::<u64>(), any::<u8>(), any::<bool>()), 0..3),
    ) {
        let root = tempfile::tempdir().unwrap();
        let journal = random_journal(root.path(), &lens, &damage);
        let safe = trace(&journal, false);
        let mapped = trace(&journal, true);
        prop_assert_eq!(&mapped, &safe);
        if damage.is_empty() {
            prop_assert_eq!(&mapped.end, &None);
            check_contents(&mapped, &lens);
        }
    }
}

/// A record that is complete in the mapping (header, payload and CRC all written) but whose commit word is not yet
/// stored is invisible to both readers, however often they poll; once the writer stores the commit word, they return
/// exactly that record. Covers every CRC path (lengths through the three-lane thresholds) and rolls, where the hook
/// runs after the old segment's seal and with the new segment's first slot still empty.
#[test]
fn a_complete_but_unpublished_record_stays_invisible_until_its_commit_word() {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let mut writer = Writer::open(&journal, small_config()).unwrap();
    append(&mut writer, 10);
    // SAFETY: only the tickertape writer modifies the journal, and it never modifies committed bytes.
    let mapped = Rc::new(RefCell::new(
        unsafe { Reader::open_mapped(&journal, ReaderConfig::default()) }.unwrap(),
    ));
    let safe = Rc::new(RefCell::new(
        Reader::open(&journal, ReaderConfig::default()).unwrap(),
    ));
    for reader in [&mapped, &safe] {
        assert_eq!(reader.borrow_mut().next_owned().unwrap().unwrap().seq, 0);
        assert!(reader.borrow_mut().try_next().unwrap().is_none());
    }

    let lens = [0, 5, 191, 192, 767, 768, 800, 2000, 4000];
    for round in 0..8 {
        for len in lens {
            let seq = writer.next_seq();
            let segment_count = segments(&journal).len();
            let ran = Rc::new(Cell::new(false));
            let (m, sf, r, j) = (mapped.clone(), safe.clone(), ran.clone(), journal.clone());
            tickertape::fault::before_next_publish(move || {
                // The record really is complete: at the first slot without a valid record in the active segment,
                // the commit word is still zero and the body validates against the length about to be stored.
                let (_, path) = segments(&j).pop().unwrap();
                let (_, at) = walk(&path);
                let frame = format::frame_len(len).unwrap();
                assert_eq!(read_at(&path, at, 4), [0; 4], "seq {seq}: commit word");
                let body = read_at(&path, at + 4, frame as usize - 4);
                let decoded = format::decode_record_body(frame, &body, seq).map(|(h, _)| h.seq);
                assert_eq!(
                    decoded,
                    Ok(seq),
                    "seq {seq}: body not complete before publication"
                );
                for _ in 0..200 {
                    assert!(
                        m.borrow_mut().try_next().unwrap().is_none(),
                        "mapped reader saw unpublished record {seq}"
                    );
                    assert!(
                        sf.borrow_mut().try_next().unwrap().is_none(),
                        "safe reader saw unpublished record {seq}"
                    );
                }
                r.set(true);
            });
            append(&mut writer, len);
            assert!(ran.get(), "round {round}, seq {seq}: the hook didn't run");
            let rolled = segments(&journal).len() > segment_count;
            for (kind, reader) in [("mapped", &mapped), ("safe", &safe)] {
                let rec = reader
                    .borrow_mut()
                    .next_owned()
                    .unwrap()
                    .unwrap_or_else(|| panic!("{kind} reader: record {seq} missing"));
                let (schema_id, venue_ts_ns, local_ts_ns, data) = record_for(seq, len);
                assert_eq!(
                    (rec.seq, rec.schema_id, rec.venue_ts_ns, rec.local_ts_ns),
                    (seq, schema_id, venue_ts_ns, local_ts_ns),
                    "{kind} reader, seq {seq} (rolled: {rolled})"
                );
                assert_eq!(rec.payload, data, "{kind} reader, seq {seq}");
                assert!(reader.borrow_mut().try_next().unwrap().is_none());
            }
        }
    }
    assert!(segments(&journal).len() >= 2, "the test must cover a roll");
}
