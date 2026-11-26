//! Tests of the later names that are not segments (issue #89, [`super::Strays`]): a reader at an empty slot reads all
//! of them at its first check after an open or a seek, every new one once, and of the others at most `STRAY_READS` per
//! listing, in turn, so that a name that becomes a segment is still found within the bound. They count header reads (a
//! test-only counter in `is_segment`) and make the checks due by moving the reader's timers back, so nothing here
//! depends on how long anything takes.

use super::{Reader, STRAY_READS, tests::header_reads};
use crate::format::{self, SegmentHeader};
use crate::{CorruptReason, Error, NewRecord, ReaderConfig, Result, Writer, WriterConfig};
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const SEGMENT: u64 = 65_536;
/// Far more than one listing reads again.
const STRAYS: u64 = 1_000;
const FIRST_STRAY: u64 = 1_000_000;
/// The stray that tests turn into a segment: in the middle, so that being the newest name helps it in no way.
const MIDDLE: u64 = FIRST_STRAY + STRAYS / 2;
/// Listings within which every stray is read again (`ceil(STRAYS / STRAY_READS)`).
const BOUND: u64 = STRAYS.div_ceil(STRAY_READS as u64);

/// A journal of one segment with a few records, checkpointed, and its writer (left open: the reader waits at the head).
fn journal(dir: &Path) -> Writer {
    let mut writer = Writer::open(dir, WriterConfig::default().segment_size(SEGMENT)).unwrap();
    for i in 0..20u64 {
        writer
            .append(&NewRecord {
                schema_id: 0,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &i.to_le_bytes(),
            })
            .unwrap();
    }
    writer.checkpoint().unwrap();
    writer
}

fn stray_path(dir: &Path, base: u64) -> PathBuf {
    dir.join(format::segment_file_name(base))
}

/// [`STRAYS`] empty files named like later segments; the one at [`MIDDLE`] full of zeros of a segment's size instead,
/// if `zeros` (a segment whose header never reached the disk: a header written in place makes it a segment).
fn strays(dir: &Path, zeros: bool) {
    for base in FIRST_STRAY..FIRST_STRAY + STRAYS {
        let len = if zeros && base == MIDDLE { SEGMENT } else { 0 };
        let file = std::fs::File::create(stray_path(dir, base)).unwrap();
        file.set_len(len).unwrap();
    }
}

fn header(base: u64) -> [u8; format::SEGMENT_HEADER_LEN] {
    format::encode_segment_header(&SegmentHeader {
        segment_size: SEGMENT,
        base_seq: base,
        created_unix_ns: 1,
    })
}

/// A reader of the kind given: the zero-copy one if `mapped`.
fn open(dir: &Path, mapped: bool, durable: bool) -> Reader {
    let config = ReaderConfig::default().durable_only(durable).notify(false);
    if mapped {
        // SAFETY: only the tickertape writer of `journal` modifies the segment files (the strays are other files), and
        // no payload is held while it appends.
        unsafe { Reader::open_mapped(dir, config) }.unwrap()
    } else {
        Reader::open(dir, config).unwrap()
    }
}

/// A reader at the head, after its first check (which read every later name).
fn at_head(dir: &Path, durable: bool) -> Reader {
    let mut reader = open(dir, false, durable);
    while reader.try_next().unwrap().is_some() {}
    reader
}

/// Writes the header of a segment into the stray at [`MIDDLE`], in place.
fn header_in_place(dir: &Path) {
    let file = OpenOptions::new()
        .write(true)
        .open(stray_path(dir, MIDDLE))
        .unwrap();
    file.write_all_at(&header(MIDDLE), 0).unwrap();
}

/// One call to `try_next` that makes the check with a listing (as if the reader had waited at the slot for over a
/// second), and how many headers it read.
fn check(reader: &mut Reader) -> (u64, Result<()>) {
    let past = Instant::now() - Duration::from_secs(2);
    reader.waiting_since = Some(past);
    if let Some(listed) = reader.wait.listed.as_mut() {
        listed.at = past;
    }
    let before = header_reads();
    let result = reader.try_next().map(|r| assert!(r.is_none()));
    (header_reads() - before, result)
}

/// Checks until one reports the slot at the cursor, each reading at most `STRAY_READS` remembered names (and the one
/// that reports, the segment once more to confirm); the number of checks that took.
fn checks_until_reported(reader: &mut Reader, base: u64) -> u64 {
    let at = (reader.seg.path.clone(), reader.offset);
    for n in 1..=BOUND + 2 {
        let (reads, result) = check(reader);
        match result {
            Ok(()) => assert_eq!(reads, STRAY_READS as u64, "check {n}"),
            Err(Error::Corrupt {
                path,
                offset,
                reason: CorruptReason::EmptyCommitWord(found),
                ..
            }) => {
                assert_eq!((path, offset, found), (at.0, at.1, base));
                assert!(reads <= STRAY_READS as u64 + 1, "{reads}");
                return n;
            }
            Err(e) => panic!("check {n}: {e:?}"),
        }
    }
    panic!("not reported within {} checks", BOUND + 2);
}

/// The issue's directory: a waiting reader of either kind reads every stray at its first check, and then at most
/// `STRAY_READS` of them per listing (before: all of them, at every listing). A seek makes it read them all again,
/// once.
#[test]
fn a_waiting_reader_reads_at_most_stray_reads_known_names_per_listing() {
    for mapped in [false, true] {
        waiting_reader_of_kind(mapped);
    }
}

fn waiting_reader_of_kind(mapped: bool) {
    let dir = tempfile::tempdir().unwrap();
    let writer = journal(dir.path());
    let mut reader = at_head(dir.path(), false);
    assert_eq!(check(&mut reader).0, 0, "no later names, no reads");
    strays(dir.path(), false);
    let mut reader = open(dir.path(), mapped, false);
    let before = header_reads();
    while reader.try_next().unwrap().is_some() {}
    assert_eq!(
        header_reads() - before,
        STRAYS,
        "the first check reads them all"
    );
    for n in 0..2 * BOUND {
        let (reads, result) = check(&mut reader);
        result.unwrap();
        assert_eq!(reads, STRAY_READS as u64, "check {n}");
    }
    // A new name is read at once, besides the turn of the others.
    std::fs::File::create(stray_path(dir.path(), FIRST_STRAY + STRAYS)).unwrap();
    let (reads, result) = check(&mut reader);
    result.unwrap();
    assert_eq!(reads, STRAY_READS as u64 + 1);
    // A name that goes away is forgotten, and does not count.
    std::fs::remove_file(stray_path(dir.path(), FIRST_STRAY)).unwrap();
    for _ in 0..BOUND + 1 {
        let (reads, result) = check(&mut reader);
        result.unwrap();
        assert_eq!(reads, STRAY_READS as u64);
    }
    // Moved by a seek, it reads them all at its first empty slot, as after the open.
    reader.seek(reader.position()).unwrap();
    let before = header_reads();
    assert!(reader.try_next().unwrap().is_none());
    assert_eq!(header_reads() - before, STRAYS);
    drop(writer);
}

/// `seek_to_end` forgets the remembered names when it succeeds, as a seek does, and moves them into the state it puts
/// back when it fails (`tests::a_failed_seek_to_end_restores_every_field`), without copying them. (It can't run beside
/// the strays above: it opens the newest name, as it did before, and fails on a file that is no segment.)
#[test]
fn seek_to_end_forgets_the_remembered_names() {
    let dir = tempfile::tempdir().unwrap();
    let writer = journal(dir.path());
    for mapped in [false, true] {
        let mut reader = open(dir.path(), mapped, false);
        reader.wait.strays.names = [(FIRST_STRAY, 7), (FIRST_STRAY + 1, 8)].into();
        reader.seek_to_end().unwrap();
        assert!(reader.wait.strays.names.is_empty(), "mapped: {mapped}");
    }
    drop(writer);
}

/// The listing goes on while there are strays, also in a directory whose modification time counts as settled (ADR 0050:
/// a listing that finds nothing in such a directory is not made again until it changes). A header written in place does
/// not change the directory, so a reader that took strays for nothing there would never see it.
#[test]
fn a_header_written_in_place_is_found_in_a_settled_directory() {
    let dir = tempfile::tempdir().unwrap();
    let writer = journal(dir.path());
    strays(dir.path(), true);
    std::fs::File::open(dir.path())
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(10))
        .unwrap();
    let mut reader = at_head(dir.path(), false);
    for _ in 0..2 {
        let (reads, result) = check(&mut reader);
        result.unwrap();
        assert_eq!(reads, STRAY_READS as u64);
    }
    header_in_place(dir.path());
    assert!(checks_until_reported(&mut reader, MIDDLE) <= BOUND);
    drop(writer);
}

/// A stray that a header written in place makes a segment, after the reader has read it and found none: found within
/// `ceil(STRAYS / STRAY_READS)` listings, whatever its place in the turn.
#[test]
fn a_stray_that_gets_a_header_in_place_is_found_within_the_bound() {
    for wait in 0..BOUND {
        let dir = tempfile::tempdir().unwrap();
        let writer = journal(dir.path());
        strays(dir.path(), true);
        let mut reader = at_head(dir.path(), false);
        for _ in 0..wait {
            let (reads, result) = check(&mut reader);
            result.unwrap();
            assert_eq!(reads, STRAY_READS as u64);
        }
        header_in_place(dir.path());
        let n = checks_until_reported(&mut reader, MIDDLE);
        assert!(n <= BOUND, "{wait}: {n} checks");
        drop(writer);
    }
}

/// A stray replaced by a segment renamed over it: another file, so read at the next listing.
#[test]
fn a_stray_replaced_by_rename_is_found_at_the_next_listing() {
    let dir = tempfile::tempdir().unwrap();
    let writer = journal(dir.path());
    strays(dir.path(), false);
    let mut reader = at_head(dir.path(), false);
    for _ in 0..2 {
        let (reads, result) = check(&mut reader);
        result.unwrap();
        assert_eq!(reads, STRAY_READS as u64);
    }
    let part = dir.path().join("replacement.part");
    let file = std::fs::File::create(&part).unwrap();
    file.set_len(SEGMENT).unwrap();
    file.write_all_at(&header(MIDDLE), 0).unwrap();
    std::fs::rename(&part, stray_path(dir.path(), MIDDLE)).unwrap();
    assert_eq!(checks_until_reported(&mut reader, MIDDLE), 1);
    drop(writer);
}

/// A durable-only reader lists the segments only at an empty slot below a marker it applied, where the marker read for
/// the check no longer covers it (a marker set back: ADR 0050). Here the marker it applied is stood in for by its
/// boundary. It reads the strays as a visible reader does, and finds a stray that became a segment within the bound.
#[test]
fn a_durable_only_reader_reads_at_most_stray_reads_known_names_per_listing() {
    let dir = tempfile::tempdir().unwrap();
    let writer = journal(dir.path());
    strays(dir.path(), true);
    let mut reader = at_head(dir.path(), true);
    assert_eq!(check(&mut reader).0, 0, "at the marker it lists nothing");
    reader.durable_next_seq = u64::MAX;
    let (reads, result) = check(&mut reader);
    result.unwrap();
    assert_eq!(reads, STRAYS, "its first check reads them all");
    for _ in 0..BOUND {
        let (reads, result) = check(&mut reader);
        result.unwrap();
        assert_eq!(reads, STRAY_READS as u64);
    }
    header_in_place(dir.path());
    assert!(checks_until_reported(&mut reader, MIDDLE) <= BOUND);
    drop(writer);
}
