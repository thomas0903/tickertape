//! Read-only inspection and verification, used by the CLI (`tickertape inspect`, `tickertape verify`).
//!
//! Nothing here takes the writer lock or modifies files, so it is safe to run while a writer is active. Against a
//! live journal, results describe a moment in time and the newest segment may grow during the scan.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, ScanEnd};
use crate::journal;

/// One segment file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SegmentInfo {
    /// The file.
    pub path: PathBuf,
    /// Sequence number of its first record.
    pub base_seq: u64,
    /// Its size in bytes.
    pub segment_size: u64,
    /// Format version from its header.
    pub format_version: u16,
    /// Creation time from its header, nanoseconds since the Unix epoch.
    pub created_unix_ns: u64,
}

/// Lists the segments in `dir`, oldest first, with their decoded headers.
///
/// A segment that retention (or writer recovery) removes between the directory listing and the opening of its file is
/// not an error: the directory is listed again, so the result leaves it out. So is a listing that comes back empty on
/// a live journal (btrfs omits entries created after a listing starts). Both are retried at most 8 times: a listing
/// still empty on the eighth attempt gives [`Error::NotFound`], and a listed segment that vanished on the eighth an
/// I/O error. A segment removed after it was opened can still be in the result.
///
/// # Errors
///
/// [`Error::NotFound`] if there are none; [`Error::Corrupt`] or
/// [`Error::UnsupportedVersion`] for an invalid header;
/// [`Error::Io`].
pub fn list_segments(dir: impl AsRef<Path>) -> Result<Vec<SegmentInfo>> {
    let dir = dir.as_ref();
    let mut attempt = 1;
    loop {
        if let Some(segments) = list_once(dir, attempt == LISTING_ATTEMPTS)? {
            return Ok(segments);
        }
        attempt += 1;
    }
}

/// Listings [`list_segments`] and [`verify`] make when segments disappear while they run. Retention removes the
/// oldest segments first, so the next listing already reflects a removal; more attempts are needed only if files
/// keep disappearing, and on the last one a missing file is reported as an I/O error.
const LISTING_ATTEMPTS: u32 = 8;
/// [`verify`] scans a segment that ends in a state a live writer passes through again after this pause, then after
/// pauses twice as long as the one before, up to [`TRANSIENT_LAST_PAUSE`] (one budget per segment: [`Rescans`]).
const TRANSIENT_FIRST_PAUSE: Duration = Duration::from_millis(10);
/// The longest of those pauses (10 + 20 + ... + 320 ms: 630 ms in all).
const TRANSIENT_LAST_PAUSE: Duration = Duration::from_millis(320);

/// Opens a listed segment. `Ok(None)` if it no longer exists, unless this is the `last` attempt.
fn open_listed(path: &Path, last: bool) -> Result<Option<File>> {
    crate::fault::hook(crate::fault::Point::InspectOpen);
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !last => Ok(None),
        Err(e) => Err(Error::io("open segment", path, e)),
    }
}

/// Lists the segment files. `Ok(None)` if there are none, unless this is the `last` attempt ([`Error::NotFound`]):
/// on a live journal a directory read can come back empty, because btrfs returns only entries that existed when the
/// directory was opened and still exist when it is read.
fn list_nonempty(dir: &Path, last: bool) -> Result<Option<Vec<(u64, PathBuf)>>> {
    let segments = journal::list_segments(dir)?;
    match (segments.is_empty(), last) {
        (false, _) => Ok(Some(segments)),
        (true, false) => Ok(None),
        (true, true) => Err(Error::NotFound {
            dir: dir.to_path_buf(),
        }),
    }
}

/// One attempt at [`list_segments`]: `Ok(None)` if a listed segment disappeared.
fn list_once(dir: &Path, last: bool) -> Result<Option<Vec<SegmentInfo>>> {
    let Some(segments) = list_nonempty(dir, last)? else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(segments.len());
    for (base, path) in segments {
        let Some(file) = open_listed(&path, last)? else {
            return Ok(None);
        };
        let header = journal::read_header(&file, &path, base)?;
        out.push(SegmentInfo {
            path,
            base_seq: base,
            segment_size: header.segment_size,
            format_version: format::FORMAT_VERSION,
            created_unix_ns: header.created_unix_ns,
        });
    }
    Ok(Some(out))
}

/// Reads the current checkpoint marker without taking the writer lock or modifying the journal.
///
/// Returns `Ok(None)` when `checkpoint` is missing or neither slot is valid. A truncated marker may still
/// contain a valid slot. This is an observation of the marker, not validation of the position it describes. The
/// writer updates the marker in place and neither retention nor recovery removes it, so reading it can't race with
/// them (durable-only readers read it the same way).
///
/// # Errors
///
/// [`Error::Io`] if the marker cannot be opened or read for a reason other than a missing file.
pub fn read_checkpoint(dir: impl AsRef<Path>) -> Result<Option<format::CheckpointSlot>> {
    let path = dir.as_ref().join(journal::CHECKPOINT_FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("open checkpoint", &path, e)),
    };
    let read = || read_checkpoint_slots(&file).map_err(|e| Error::io("read checkpoint", &path, e));
    if let Some(slot) = read()? {
        return Ok(Some(slot));
    }
    // The two slots are read one after the other, and a writer checkpointing meanwhile writes them in turn: both reads
    // can meet a slot being written. Once more before this counts as no marker.
    crate::fault::hook(crate::fault::Point::CheckpointReread);
    read()
}

/// One read of the marker's two slots (not of the 128 KiB file around them: seeks and durable-only readers read the
/// marker often). The result is that of [`format::decode_checkpoint`] on the file's contents.
fn read_checkpoint_slots(file: &File) -> std::io::Result<Option<format::CheckpointSlot>> {
    use std::os::unix::fs::FileExt;

    let mut current: Option<format::CheckpointSlot> = None;
    for offset in format::CHECKPOINT_SLOT_OFFSETS {
        let mut slot = [0; format::CHECKPOINT_SLOT_LEN];
        match file.read_exact_at(&mut slot, offset as u64) {
            Ok(()) => {}
            // A truncated marker: this slot isn't (completely) there.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => continue,
            Err(e) => return Err(e),
        }
        // The higher generation; on a tie the later slot.
        current = match (current, format::decode_checkpoint_slot(&slot)) {
            (Some(a), Some(b)) if a.generation > b.generation => Some(a),
            (a, b) => b.or(a),
        };
    }
    Ok(current)
}

/// How a segment ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SegmentEnd {
    /// Sealed: its successor holds the next record.
    Sealed,
    /// Not sealed: the newest segment, which the writer appends to.
    Open,
    /// Ends in a problem (listed in [`VerifyReport::problems`]).
    Damaged,
}

/// What [`verify`] found in one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SegmentReport {
    /// The segment's header. `None` if the header is invalid.
    pub info: Option<SegmentInfo>,
    /// The file.
    pub path: PathBuf,
    /// Number of valid records.
    pub records: u64,
    /// Sequence number after the last valid record (equals the base if there are none).
    pub end_seq: u64,
    /// Bytes used by records (and the seal, if any), including the header.
    pub used_bytes: u64,
    /// How the segment ends.
    pub end: SegmentEnd,
}

/// The result of [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifyReport {
    /// Segments checked, oldest first.
    pub segments: Vec<SegmentReport>,
    /// Number of valid records found.
    pub records: u64,
    /// Sequence number of the first record, if any.
    pub first_seq: Option<u64>,
    /// Sequence number of the last valid record, if any.
    pub last_seq: Option<u64>,
    /// Every problem found. Empty means the journal is intact.
    pub problems: Vec<Problem>,
}

/// A problem found by [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Problem {
    /// The segment.
    pub path: PathBuf,
    /// Byte offset.
    pub offset: u64,
    /// What is wrong.
    pub reason: CorruptReason,
}

/// Scans every segment in `dir`, checking every header, record, seal and the chain between segments.
///
/// Uncommitted bytes after the end of the newest segment (left by a crashed writer, cleaned up by the next one) are
/// not a problem. Anything that a reader would reject is, and so are states that only a crash leaves behind and the
/// next writer repairs (a sealed newest segment without a successor, or an unsealed segment that has one), and
/// evidence of committed records after an empty commit word ([`CorruptReason::EmptyCommitWord`], or
/// [`CorruptReason::UncheckedTail`] when the search stops at its work bound, reported at the empty word).
///
/// Against a live journal, the report describes the segments listed when the scan started, as the scan found them.
/// A writer appending or rolling during the scan is not a problem: a segment that ends in a state a writer passes
/// through (a record being written at the end of the newest segment, or an unsealed segment whose successor already
/// exists, because a rolling writer links the successor just before it seals) is scanned again, after pauses of
/// 10 ms doubling up to 320 ms (630 ms in all), until it no longer is. Nor are records the writer commits while
/// `verify` looks for committed history after an empty commit word at the end of the newest segment: if that word is
/// no longer empty, `verify` follows the writer and continues the scan from it (at most 8 times, then it describes
/// the segment as open where it got to), and an invalid record at the new end is scanned again with the pauses the
/// first scan left of those 630 ms (one budget per segment, never started again). If the word is still empty,
/// `verify` gives the writer 50 ms once (it may be filling a record whose payload holds frames) and reads the word
/// again before it reports [`CorruptReason::EmptyCommitWord`] (ADR 0035). So with a writer running, a problem in the
/// report is real unless the writer stayed in such a state for that long (descheduled or stopped between two
/// steps), or took longer than those 50 ms to fill a record whose payload holds valid frames with nearby sequence
/// numbers; run `verify` again to tell. A problem that is really there is always reported, only later. With no
/// writer running every reported problem is in the files (it need not be storage damage: a record torn by a crash
/// whose payload holds such frames is reported the same way, crash contract C1; and the next open need not meet
/// it, since `verify` visits every retained segment and scans records in each segment whose header passes
/// validation, while a writer's recovery scans records from the checkpoint onward, or from the oldest retained
/// segment when neither checkpoint slot decodes), and a journal a crash left in such a state, or with an empty commit word in front of committed records at the end
/// of the newest segment, takes those 630 ms or 50 ms longer to verify.
/// Retention is not a problem either: a segment it removes before the scan reaches it is not an error, and since
/// retention removes the oldest segments first, the report then starts at the oldest segment the scan could still
/// open, leaving out the removed history and what was found in it.
/// But a segment already open when retention removed it can stay in the report, with its records: the newest listed
/// segment always does, since nothing after it is opened. No problem is reported for its missing successor. As for
/// [`list_segments`], the listing is retried at most 8 times: still empty on the eighth attempt, the result is
/// [`Error::NotFound`].
///
/// # Errors
///
/// [`Error::NotFound`] or [`Error::Io`]. Corruption is reported in
/// [`VerifyReport::problems`], not as an error.
pub fn verify(dir: impl AsRef<Path>) -> Result<VerifyReport> {
    let dir = dir.as_ref();
    let mut attempt = 1;
    loop {
        if let Some(report) = verify_once(dir, attempt == LISTING_ATTEMPTS)? {
            return Ok(report);
        }
        attempt += 1;
    }
}

/// One attempt at [`verify`]: `Ok(None)` if the listing was empty, or a listed segment disappeared other than from
/// the oldest end, or the newest listed segment did.
fn verify_once(dir: &Path, last: bool) -> Result<Option<VerifyReport>> {
    let Some(segments) = list_nonempty(dir, last)? else {
        return Ok(None);
    };
    let mut report = VerifyReport {
        segments: Vec::new(),
        records: 0,
        first_seq: None,
        last_seq: None,
        problems: Vec::new(),
    };
    let mut expected_base = Some(segments[0].0);
    let count = segments.len();
    for (i, (base, path)) in segments.into_iter().enumerate() {
        let newest = i + 1 == count;
        let Some(file) = open_listed(&path, last)? else {
            // Removed after the listing. Removal from the oldest end (retention, archiving) also took everything
            // checked so far: start the report again from the next segment. Anything else: verify again.
            let oldest = journal::list_segments(dir)?.first().map(|(b, _)| *b);
            if newest || oldest.is_none_or(|oldest| base >= oldest) {
                return Ok(None);
            }
            report.segments.clear();
            report.problems.clear();
            expected_base = None;
            continue;
        };
        let mut seg = SegmentReport {
            info: None,
            path: path.clone(),
            records: 0,
            end_seq: base,
            used_bytes: 0,
            end: SegmentEnd::Damaged,
        };
        if let Some(expected) = expected_base.filter(|expected| *expected != base) {
            report.problems.push(Problem {
                path: path.clone(),
                offset: 0,
                reason: CorruptReason::BrokenChain(expected),
            });
        }
        let header = match journal::read_header(&file, &path, base) {
            Ok(h) => h,
            Err(Error::Corrupt { reason, offset, .. }) => {
                report.problems.push(Problem {
                    path: path.clone(),
                    offset,
                    reason,
                });
                report.segments.push(seg);
                continue;
            }
            Err(Error::UnsupportedVersion { .. }) => {
                report.problems.push(Problem {
                    path: path.clone(),
                    offset: 8,
                    reason: CorruptReason::BadSegmentHeader("unsupported format version"),
                });
                report.segments.push(seg);
                continue;
            }
            Err(e) => return Err(e),
        };
        seg.info = Some(SegmentInfo {
            path: path.clone(),
            base_seq: base,
            segment_size: header.segment_size,
            format_version: format::FORMAT_VERSION,
            created_unix_ns: header.created_unix_ns,
        });

        let mut scan = journal::scan_open(&file, &path, base)?;
        let transient = |end: ScanEnd| match end {
            // A live writer may have been mid-append while we copied the file.
            ScanEnd::Invalid(_) => newest,
            // A rolling writer publishes the next segment just before it seals this one.
            ScanEnd::Empty => !newest,
            _ => false,
        };
        // A writer descheduled in such a state stays in it: look again with growing pauses before reporting it. The
        // pauses are one budget per segment, shared with the scans that follow a live writer below: see `Rescans`.
        let mut rescans = Rescans::new();
        while transient(scan.end) && rescans.pause() {
            scan = journal::scan_open(&file, &path, base)?;
        }
        let mut later = later_history(&file, &path, &scan)?;
        let (mut follows, mut waited) = (0, false);
        while newest && later.is_some() {
            let mut word = journal::commit_word_open(&file, &path, scan.end_offset)?;
            if word == 0 && !waited {
                // A live writer may still be filling the record at the empty word with a payload that holds frames
                // (the accepted false positive, ADR 0035): give it 50 ms to commit, once. A budget of its own, not
                // `rescans`: this wait is for a different state, and what the rescans used must not shorten it.
                waited = true;
                pause(EMPTY_WORD_WAIT);
                word = journal::commit_word_open(&file, &path, scan.end_offset)?;
            }
            if word == 0 {
                break;
            }
            // The journal grew while we searched: a live writer committed the record at the empty word P and
            // appended after it, and those records were the evidence. The writer stores P's commit word before it
            // writes anything after P's frame, and evidence there includes a commit word stored after P's (a
            // record's or a seal's, each with release ordering), so evidence a live writer appended always comes
            // with a non-zero word at P; real damage at P stays zero. Continue the scan from P instead of reporting.
            if follows == MAX_FOLLOWS {
                // The writer keeps appending faster than we can search: stop here and describe the segment as open
                // at the last record reached, as for anything appended after `verify` returns. Not corruption.
                later = None;
                break;
            }
            follows += 1;
            let mut more = journal::scan_open_from(&file, &path, scan.end_offset, scan.next_seq)?;
            // The same race as for the first scan, at the new end: the writer was mid-append there. These rescans
            // take their pauses from the same budget as the first scan's (whatever it left), never a new one.
            while matches!(more.end, ScanEnd::Invalid(_)) && rescans.pause() {
                more = journal::scan_open_from(&file, &path, scan.end_offset, scan.next_seq)?;
            }
            scan = format::ScanResult {
                records: scan.records + more.records,
                ..more
            };
            later = later_history(&file, &path, &scan)?;
        }
        seg.records = scan.records;
        seg.end_seq = scan.next_seq;
        seg.used_bytes = scan.end_offset;
        match scan.end {
            ScanEnd::Seal => {
                seg.used_bytes += format::SEAL_LEN as u64;
                seg.end = SegmentEnd::Sealed;
                // A sealed newest segment is a problem only if its successor is missing while it is still in
                // place: if the writer rolled after the listing the successor exists now, and if retention has
                // since removed both (oldest first), this segment is gone too.
                if newest {
                    crate::fault::hook(crate::fault::Point::InspectSeal);
                    let successor = journal::segment_path(dir, scan.next_seq);
                    if journal::path_identity(&successor)?.is_none()
                        && journal::path_identity(&path)?
                            == Some(journal::file_identity(&file, &path)?)
                    {
                        report.problems.push(Problem {
                            path: path.clone(),
                            offset: scan.end_offset,
                            reason: CorruptReason::BrokenChain(scan.next_seq),
                        });
                    }
                }
            }
            // Evidence of committed records after an empty commit word, or a search stopped at its work bound: usually
            // storage damage (ADR 0035; crash contract C1 has the exception).
            ScanEnd::Empty if later.is_some() => report.problems.push(Problem {
                path: path.clone(),
                offset: scan.end_offset,
                reason: later.map_or(CorruptReason::EmptyCommitWord(0), |h| h.reason()),
            }),
            ScanEnd::Empty if newest => seg.end = SegmentEnd::Open,
            ScanEnd::Empty => report.problems.push(Problem {
                path: path.clone(),
                offset: scan.end_offset,
                reason: CorruptReason::BrokenChain(scan.next_seq),
            }),
            ScanEnd::Invalid(reason) => report.problems.push(Problem {
                path: path.clone(),
                offset: scan.end_offset,
                reason,
            }),
        }
        expected_base = Some(scan.next_seq);
        report.segments.push(seg);
    }
    // Totals over the segments reported (records in a segment run from its base to `end_seq`).
    report.records = report.segments.iter().map(|s| s.records).sum();
    let mut with_records = report.segments.iter().filter(|s| s.records > 0);
    report.first_seq = with_records.next().map(|s| s.end_seq - s.records);
    report.last_seq = report
        .segments
        .iter()
        .rev()
        .find(|s| s.records > 0)
        .map(|s| s.end_seq - 1);
    Ok(Some(report))
}

/// The pause [`verify`] gives a live writer that may still be filling the record at an empty commit word with a
/// payload that holds frames (ADR 0035), once per segment, before it reports that word.
const EMPTY_WORD_WAIT: Duration = Duration::from_millis(50);

/// How many times [`verify`] follows a live writer past the end of the newest segment (see the loop there) before it
/// stops and reports the end it reached as open.
const MAX_FOLLOWS: u32 = 8;

/// The rescans [`verify`] may make of one segment that a scan found in a state a live writer passes through: after
/// [`TRANSIENT_FIRST_PAUSE`], then after pauses twice as long as the one before, up to [`TRANSIENT_LAST_PAUSE`] (630 ms
/// in all).
///
/// How the retries compose. A segment has one such budget, used in this order: first by the rescans of its first scan
/// (an invalid record at the end of the newest segment, or an unsealed segment whose successor exists), then by the
/// rescans of an invalid record at the new end that the scans following a live writer meet (ADR 0035; newest segment
/// only). Those take the next pause the first scan left and never start the schedule again, keeping ADR 0035's rule
/// that the first and the continued scans share one retry per segment (now this schedule instead of a single 50 ms
/// retry): whichever position a writer is caught at, one that stays mid-append for 630 ms is stalled, and a budget per
/// follow would let `verify` sleep [`MAX_FOLLOWS`] times as long. The wait at an empty commit word in the follow loop
/// ([`EMPTY_WORD_WAIT`], once) is a budget of its own: it is for a different state (a record still being filled, not
/// one caught mid-append), and rescans that used the whole budget must not take it away.
///
/// So one attempt of `verify` sleeps at most 630 ms for each segment it lists other than the newest, and 680 ms for the
/// newest (630 ms of rescans and the 50 ms wait), however many rounds the follow loop makes; it makes at most
/// [`LISTING_ATTEMPTS`] attempts, more than one only when segments disappear under it. With no writer running, only a
/// journal a crash left in such a state, or damaged, pays these pauses.
struct Rescans {
    next: Duration,
}

impl Rescans {
    fn new() -> Self {
        Rescans {
            next: TRANSIENT_FIRST_PAUSE,
        }
    }

    /// Makes the next pause and returns `true`, or returns `false` without pausing once the budget is spent.
    fn pause(&mut self) -> bool {
        if self.next > TRANSIENT_LAST_PAUSE {
            return false;
        }
        pause(self.next);
        self.next *= 2;
        true
    }
}

/// Gives a live writer `duration` to finish what [`verify`] caught it in the middle of, before `verify` reads the
/// segment again. Every pause in `verify` goes through here, with both hooks: the test-only `tests::WAIT_HOOK` before
/// the pause and the fault-injection hook point [`crate::fault::Point::InspectRescan`] after it.
fn pause(duration: Duration) {
    #[cfg(test)]
    tests::WAIT_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
    std::thread::sleep(duration);
    crate::fault::hook(crate::fault::Point::InspectRescan);
}

/// Committed history after the empty slot a scan of the open segment `file` ended at, if it ended at one
/// ([`format::find_history_after_empty`]).
fn later_history(
    file: &File,
    path: &Path,
    scan: &format::ScanResult,
) -> Result<Option<format::LaterHistory>> {
    if scan.end != ScanEnd::Empty {
        return Ok(None);
    }
    journal::history_after_empty_open(file, path, scan.end_offset, scan.next_seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::FileExt;

    thread_local! {
        /// Runs at each of [`pause`]'s pauses on this thread, standing in for a live writer.
        pub(super) static WAIT_HOOK: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
    }

    /// Reading only the two slots gives what decoding the whole file gives: which slot is current, ties, invalid
    /// slots, and a marker truncated anywhere.
    #[test]
    fn read_checkpoint_agrees_with_decoding_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(journal::CHECKPOINT_FILE);
        assert_eq!(read_checkpoint(dir.path()).unwrap(), None);
        let slot = |generation: u64, next_seq: u64| {
            format::encode_checkpoint_slot(&format::CheckpointSlot {
                generation,
                next_seq,
                segment_base_seq: 0,
                offset: 64,
            })
        };
        let invalid = [0x5a; format::CHECKPOINT_SLOT_LEN];
        let second = format::CHECKPOINT_SLOT_OFFSETS[1];
        for (a, b, expected) in [
            (slot(3, 10), slot(4, 20), Some(20)),
            (slot(5, 30), slot(4, 20), Some(30)),
            (slot(4, 10), slot(4, 20), Some(20)),
            (slot(3, 10), invalid, Some(10)),
            (invalid, slot(4, 20), Some(20)),
            (invalid, invalid, None),
        ] {
            let mut file = vec![0; format::CHECKPOINT_FILE_LEN];
            file[..a.len()].copy_from_slice(&a);
            file[second..second + b.len()].copy_from_slice(&b);
            for len in [0, 63, 64, second, second + 63, second + 64, file.len()] {
                std::fs::write(&path, &file[..len]).unwrap();
                let read = read_checkpoint(dir.path()).unwrap();
                assert_eq!(
                    read,
                    format::decode_checkpoint(&file[..len]),
                    "length {len}"
                );
                if len == file.len() {
                    assert_eq!(read.map(|c| c.next_seq), expected);
                }
            }
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(read_checkpoint(dir.path()), Err(Error::Io { .. })));
    }

    #[test]
    fn follow_loop_retries_a_record_caught_mid_append() {
        // Review round 2, item 5. A live writer: verify finds the empty commit word of record 10 with record 11
        // after it, and while it waits the writer commits record 10 and is caught in the middle of record 12 (its
        // commit word stored in this simulation, its body not yet valid). The scan that follows must give the
        // writer a rescan from the budget the first scan left (all of it here), not report a bad checksum on a
        // healthy journal.
        let dir = tempfile::tempdir().unwrap();
        let mut w = crate::Writer::open(
            dir.path(),
            crate::WriterConfig::default().segment_size(format::SEGMENT_SIZE_UNIT),
        )
        .unwrap();
        for i in 0..13_u8 {
            w.append(&crate::NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &[i; 50],
            })
            .unwrap();
        }
        drop(w);
        let (_, path) = journal::list_segments(dir.path()).unwrap().pop().unwrap();
        let full = std::fs::read(&path).unwrap();
        let mut offsets = vec![format::SEGMENT_HEADER_LEN as u64];
        for seq in 0..13 {
            let at = *offsets.last().unwrap();
            let Ok(format::Slot::Record { next_offset, .. }) =
                format::read_slot(&full[..], at, seq)
            else {
                panic!("record {seq}");
            };
            offsets.push(next_offset);
        }
        let (r10, r12, end) = (
            offsets[10] as usize,
            offsets[12] as usize,
            offsets[13] as usize,
        );
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[0; 4], r10 as u64).unwrap();
        file.write_all_at(&vec![0; end - r12], r12 as u64).unwrap();
        let mut stage = 0;
        let (full_hook, path_hook) = (full.clone(), path.clone());
        WAIT_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path_hook)
                    .unwrap();
                match stage {
                    0 => {
                        file.write_all_at(&full_hook[r10..r10 + 4], r10 as u64)
                            .unwrap();
                        file.write_all_at(&full_hook[r12..r12 + 4], r12 as u64)
                            .unwrap();
                    }
                    _ => file.write_all_at(&full_hook[r12..end], r12 as u64).unwrap(),
                }
                stage += 1;
            }));
        });
        let report = verify(dir.path()).unwrap();
        WAIT_HOOK.with(|hook| hook.borrow_mut().take());
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(report.records, 13);
        assert_eq!(std::fs::read(&path).unwrap(), full);
    }

    #[test]
    fn follow_loop_rescans_share_the_first_scans_budget() {
        // The merge of the rescans with growing pauses and ADR 0035's follow loop: one budget of rescans per
        // segment, used first by the first scan and then by the scans that follow a live writer, and the 50 ms wait
        // at an empty commit word on top of it. The first scan finds record 5 mid-append and spends the whole budget
        // (six pauses) before the writer finishes it; it then finds the empty commit word of record 10 with record 11
        // after it. During the 50 ms wait the writer commits record 10 and is caught in the middle of record 12. The
        // scan that follows has no rescan left, so it reports record 12: the budget was not started again.
        let dir = tempfile::tempdir().unwrap();
        let mut w = crate::Writer::open(
            dir.path(),
            crate::WriterConfig::default().segment_size(format::SEGMENT_SIZE_UNIT),
        )
        .unwrap();
        for i in 0..13_u8 {
            w.append(&crate::NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &[i; 50],
            })
            .unwrap();
        }
        drop(w);
        let (_, path) = journal::list_segments(dir.path()).unwrap().pop().unwrap();
        let full = std::fs::read(&path).unwrap();
        let mut offsets = vec![format::SEGMENT_HEADER_LEN as u64];
        for seq in 0..13 {
            let at = *offsets.last().unwrap();
            let Ok(format::Slot::Record { next_offset, .. }) =
                format::read_slot(&full[..], at, seq)
            else {
                panic!("record {seq}");
            };
            offsets.push(next_offset);
        }
        let [r5, r6, r10, r12, end] = [5, 6, 10, 12, 13].map(|i| offsets[i] as usize);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&vec![0; r6 - r5 - 4], r5 as u64 + 4)
            .unwrap();
        file.write_all_at(&[0; 4], r10 as u64).unwrap();
        file.write_all_at(&vec![0; end - r12 - 4], r12 as u64 + 4)
            .unwrap();
        let pauses = std::rc::Rc::new(std::cell::Cell::new(0));
        let (full_hook, path_hook, pauses_hook) = (full.clone(), path.clone(), pauses.clone());
        WAIT_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path_hook)
                    .unwrap();
                pauses_hook.set(pauses_hook.get() + 1);
                match pauses_hook.get() {
                    // The last of the six rescans of the first scan: record 5 is complete.
                    6 => file.write_all_at(&full_hook[r5..r6], r5 as u64).unwrap(),
                    // The 50 ms wait at record 10's empty word: record 10 committed, record 12 mid-append.
                    7 => file
                        .write_all_at(&full_hook[r10..r10 + 4], r10 as u64)
                        .unwrap(),
                    _ => {}
                }
            }));
        });
        let report = verify(dir.path()).unwrap();
        WAIT_HOOK.with(|hook| hook.borrow_mut().take());
        assert_eq!(
            pauses.get(),
            7,
            "six rescans, then the 50 ms wait, and no more"
        );
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert_eq!(report.problems[0].offset, r12 as u64);
        assert_eq!(report.problems[0].reason, CorruptReason::BadChecksum);
        assert_eq!(report.records, 12);
    }

    #[test]
    fn checkpoint_missing_invalid_valid_and_truncated() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_checkpoint(dir.path()).unwrap(), None);
        let path = dir.path().join("checkpoint");
        std::fs::write(&path, [0; 64]).unwrap();
        assert_eq!(read_checkpoint(dir.path()).unwrap(), None);
        let slot = format::CheckpointSlot {
            generation: 2,
            next_seq: 7,
            segment_base_seq: 0,
            offset: 512,
        };
        std::fs::write(&path, format::encode_checkpoint_slot(&slot)).unwrap();
        assert_eq!(read_checkpoint(dir.path()).unwrap(), Some(slot));
        std::fs::write(&path, [0; 63]).unwrap();
        assert_eq!(read_checkpoint(dir.path()).unwrap(), None);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(read_checkpoint(dir.path()), Err(Error::Io { .. })));
    }
}
