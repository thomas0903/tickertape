//! The newest segment of a journal: where it ends and how full it is. No iceoryx2.

use std::fs::{File, TryLockError};
use std::path::Path;

use tickertape::format;
use tickertape::{Reader, ReaderConfig};

use crate::Error;

/// How far the newest segment is filled, read from its first record to the end committed when the reader got there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tail {
    /// The journal's next sequence number: one past its last committed record.
    pub end_seq: u64,
    /// Size of the newest segment.
    pub segment_size: u64,
    /// Bytes of the newest segment in use: its header and its records, each rounded up to 8 bytes as the format
    /// lays them out (`docs/format.md` → Rolling).
    pub used: u64,
}

/// Reads the newest segment of the journal in `dir`.
///
/// # Errors
///
/// [`Error::Journal`] if the journal can't be read.
pub(crate) fn read(dir: &Path) -> Result<Tail, Error> {
    let segments = tickertape::inspect::list_segments(dir)?;
    let last = segments.last().ok_or_else(|| tickertape::Error::NotFound {
        dir: dir.to_path_buf(),
    })?;
    let mut reader = Reader::open(dir, ReaderConfig::default())?;
    reader.seek(last.base_seq)?;
    let mut tail = Tail {
        end_seq: last.base_seq,
        segment_size: last.segment_size,
        used: format::SEGMENT_HEADER_LEN as u64,
    };
    while let Some(record) = reader.try_next()? {
        // A writer may roll while this reader runs (a live recorder); count only the newest segment we started in.
        if record.seq != tail.end_seq {
            break;
        }
        let len = format::frame_len(record.payload.len()).ok_or_else(|| {
            Error::Usage(format!(
                "record {} is larger than a tickertape frame",
                record.seq
            ))
        })?;
        let next = tail.used + format::align8(u64::from(len));
        if next > tail.segment_size {
            break;
        }
        tail.used = next;
        tail.end_seq += 1;
    }
    Ok(tail)
}

/// True if a writer (a recorder or an import) holds the journal's writer lock right now.
///
/// Takes and releases a shared `flock` on `writer.lock`, the lock tickertape's writer holds exclusively while open
/// (`docs/format.md`). A writer that starts during the few microseconds the shared lock is held fails to open with
/// `Locked` and can be restarted.
///
/// # Errors
///
/// [`Error::Io`] if the lock file exists but can't be opened or locked.
pub(crate) fn has_live_writer(dir: &Path) -> Result<bool, Error> {
    let path = dir.join("writer.lock");
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(Error::Io {
                op: "open lock file",
                path,
                source,
            });
        }
    };
    match file.try_lock_shared() {
        Ok(()) => {
            // Released when `file` is closed, right away.
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(source)) => Err(Error::Io {
            op: "test lock",
            path,
            source,
        }),
    }
}

/// Follows the writer through segment rolls without asking it: knows the active segment's size and how many bytes
/// are in use, and predicts where the next records go, exactly as the writer decides (`docs/format.md` → Rolling).
/// Every rule comes from tickertape's public [`format`] module: records start at [`format::SEGMENT_HEADER_LEN`],
/// each takes `align8(frame_len(payload))` bytes, and a record fits if [`format::record_fits`] says so (it must end
/// at or before the segment size minus [`format::SEGMENT_TRAILER_LEN`]). Used by [`crate::placement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentMirror {
    size: u64,
    used: u64,
    new_segment_size: u64,
}

/// True if records with these payload lengths, appended in order from `used` in a segment of `size` bytes, all fit.
fn fit(size: u64, mut used: u64, payload_lens: &[usize]) -> bool {
    for &len in payload_lens {
        let Some(frame) = format::frame_len(len) else {
            return false;
        };
        if !format::record_fits(size, used, frame) {
            return false;
        }
        used += format::align8(u64::from(frame));
    }
    true
}

impl SegmentMirror {
    /// A mirror of the journal in `dir`, whose writer is open with segments of `new_segment_size` bytes.
    pub(crate) fn new(dir: &Path, new_segment_size: u64) -> Result<SegmentMirror, Error> {
        let tail = read(dir)?;
        Ok(SegmentMirror {
            size: tail.segment_size,
            used: tail.used,
            new_segment_size,
        })
    }

    /// True if records with these payload lengths, appended in this order, all go into the active segment.
    pub(crate) fn fits(&self, payload_lens: &[usize]) -> bool {
        fit(self.size, self.used, payload_lens)
    }

    /// True if records with these payload lengths, appended in this order, all fit in one new segment.
    pub(crate) fn fits_fresh(&self, payload_lens: &[usize]) -> bool {
        fit(
            self.new_segment_size,
            format::SEGMENT_HEADER_LEN as u64,
            payload_lens,
        )
    }

    /// The bytes left in the active segment for records: its size minus the trailer, minus the bytes in use.
    pub(crate) fn left(&self) -> u64 {
        (self.size - format::SEGMENT_TRAILER_LEN as u64).saturating_sub(self.used)
    }

    /// If a record with a payload of `first` bytes would still go into the active segment: the payload length of a
    /// record that takes exactly the space left, after which no record fits. `None` otherwise (`first` would start a
    /// new segment by itself).
    pub(crate) fn filler_len(&self, first: usize) -> Option<usize> {
        if !self.fits(&[first]) {
            return None;
        }
        // `left` is a multiple of 8 (record sizes and the segment layout are) and at least the size of the record
        // `first`, so at least a record header.
        let len = self.left() - format::RECORD_HEADER_LEN as u64;
        usize::try_from(len).ok()
    }

    /// Records that a record with a payload of `payload_len` bytes was appended. Returns true if it started a new
    /// segment.
    pub(crate) fn appended(&mut self, payload_len: usize) -> bool {
        let Some(len) = format::frame_len(payload_len) else {
            return false;
        };
        let rolled = !format::record_fits(self.size, self.used, len);
        if rolled {
            self.size = self.new_segment_size;
            self.used = format::SEGMENT_HEADER_LEN as u64;
        }
        self.used += format::align8(u64::from(len));
        rolled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickertape::{NewRecord, Writer, WriterConfig};

    /// The mirror predicts every roll of a real writer: after each append, a fresh read of the newest segment agrees
    /// with it.
    #[test]
    fn mirror_follows_the_writer_through_rolls() {
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default().segment_size(65_536).prefault(false);
        let mut writer = Writer::open(dir.path(), config).unwrap();
        let mut mirror = SegmentMirror::new(dir.path(), 65_536).unwrap();
        let mut rolls = 0;
        for i in 0..2_000usize {
            let len = (i * 37) % 3_000;
            let rolled = !mirror.fits(&[len]);
            writer
                .append(&NewRecord {
                    schema_id: 1,
                    venue_ts_ns: 0,
                    local_ts_ns: 0,
                    payload: &vec![0; len],
                })
                .unwrap();
            assert_eq!(mirror.appended(len), rolled);
            let tail = read(dir.path()).unwrap();
            assert_eq!(
                (mirror.size, mirror.used),
                (tail.segment_size, tail.used),
                "after record {i}"
            );
            if rolled {
                rolls += 1;
                assert_eq!(
                    tail.end_seq - 1,
                    tickertape::inspect::list_segments(dir.path())
                        .unwrap()
                        .last()
                        .unwrap()
                        .base_seq
                );
            }
        }
        assert!(rolls > 10, "{rolls} rolls");
        // Reopened with another segment size: the existing segment keeps its size, new ones get the new one.
        drop(writer);
        let config = WriterConfig::default()
            .segment_size(131_072)
            .prefault(false);
        let _writer = Writer::open(dir.path(), config).unwrap();
        let reopened = SegmentMirror::new(dir.path(), 131_072).unwrap();
        assert_eq!(reopened.size, 65_536);
        assert_eq!(reopened.used, mirror.used);
    }

    /// The space the mirror says is left is exactly what the real writer has left: a record of `filler_len` bytes
    /// stays in the active segment, and after it even an empty record starts a new one. Checked at every offset a
    /// 64 KiB segment can reach in 8-byte steps near its end, and for `first` records that do and don't fit.
    #[test]
    fn filler_takes_exactly_the_space_left() {
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default().segment_size(65_536).prefault(false);
        let mut writer = Writer::open(dir.path(), config).unwrap();
        let mut mirror = SegmentMirror::new(dir.path(), 65_536).unwrap();
        let append = |writer: &mut Writer, len: usize| {
            writer
                .append(&NewRecord {
                    schema_id: 1,
                    venue_ts_ns: 0,
                    local_ts_ns: 0,
                    payload: &vec![0; len],
                })
                .unwrap()
        };
        let segments = |dir: &Path| tickertape::inspect::list_segments(dir).unwrap().len();
        let mut fillers = 0;
        let mut too_little = 0;
        // Each round: a record that leaves `left` bytes in the segment, then a filler for a `first` record of 200
        // bytes if it would still fit, then an empty record.
        for left in (0..=1_200u64).step_by(8) {
            // Room for this round: if little is left, fill it and roll to a new segment.
            if mirror.left() < 4_096 {
                if let Some(rest) = mirror.filler_len(0) {
                    append(&mut writer, rest);
                    assert!(!mirror.appended(rest));
                }
                append(&mut writer, 0);
                assert!(
                    mirror.appended(0),
                    "an empty record after a filler starts a new segment"
                );
            }
            let pad = (mirror.left() - left - 40) as usize;
            append(&mut writer, pad);
            assert!(!mirror.appended(pad));
            assert_eq!(mirror.left(), left);
            let start = segments(dir.path());
            let filled = match mirror.filler_len(200) {
                Some(len) => {
                    assert!(left >= format::align8(240), "{left}");
                    append(&mut writer, len);
                    assert!(!mirror.appended(len));
                    assert_eq!(
                        segments(dir.path()),
                        start,
                        "the filler stays in the segment"
                    );
                    assert_eq!(mirror.left(), 0);
                    fillers += 1;
                    true
                }
                None => {
                    assert!(left < format::align8(240), "{left}");
                    too_little += 1;
                    false
                }
            };
            append(&mut writer, 0);
            let rolled = mirror.appended(0);
            assert_eq!(rolled, filled || left < 40, "left {left}");
            assert_eq!(
                segments(dir.path()) > start,
                rolled,
                "left {left}: the mirror and the writer agree on the roll"
            );
            let tail = read(dir.path()).unwrap();
            assert_eq!((mirror.size, mirror.used), (tail.segment_size, tail.used));
        }
        assert!(fillers > 100 && too_little > 20, "{fillers} {too_little}");
    }

    #[test]
    fn live_writer_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_live_writer(dir.path()).unwrap());
        let writer = Writer::open(
            dir.path(),
            WriterConfig::default().segment_size(65_536).prefault(false),
        )
        .unwrap();
        assert!(has_live_writer(dir.path()).unwrap());
        drop(writer);
        assert!(!has_live_writer(dir.path()).unwrap());
    }
}
