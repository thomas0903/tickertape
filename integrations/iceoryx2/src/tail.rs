//! The newest segment of a journal: where it ends, how full it is, and which services it records. No iceoryx2.

use std::collections::BTreeSet;
use std::fs::{File, TryLockError};
use std::path::Path;

use tickertape::format;
use tickertape::{Reader, ReaderConfig};

use crate::Error;
use crate::layout::{Descriptor, SCHEMA_DESCRIPTOR};

/// What the newest segment holds, read from its first record to the end committed when the reader got there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tail {
    /// The journal's next sequence number: one past its last committed record.
    pub end_seq: u64,
    /// Size of the newest segment.
    pub segment_size: u64,
    /// Bytes of the newest segment in use: its header and its records, each rounded up to 8 bytes as the format
    /// lays them out (`docs/format.md` → Rolling).
    pub used: u64,
    /// Names of the services that have a valid descriptor in the newest segment. The recorder writes the descriptor
    /// of every service it records at the start of each segment, so while a recorder runs these are the services it
    /// records (plus any it stopped recording in this segment).
    pub services: BTreeSet<String>,
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
        services: BTreeSet::new(),
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
        if record.schema_id == SCHEMA_DESCRIPTOR
            && let Ok(d) = Descriptor::decode(record.payload)
        {
            tail.services.insert(d.service_name);
        }
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
/// are in use, and predicts whether the next record starts a new segment, exactly as the writer decides
/// (`docs/format.md` → Rolling). Used by the recorder to write descriptors at the start of each segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentMirror {
    size: u64,
    used: u64,
    new_segment_size: u64,
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

    /// True if a record with a payload of `payload_len` bytes would start a new segment.
    pub(crate) fn would_roll(&self, payload_len: usize) -> bool {
        match format::frame_len(payload_len) {
            Some(len) => !format::record_fits(self.size, self.used, len),
            None => false,
        }
    }

    /// Records that a record with a payload of `payload_len` bytes was appended.
    pub(crate) fn appended(&mut self, payload_len: usize) {
        let Some(len) = format::frame_len(payload_len) else {
            return;
        };
        if !format::record_fits(self.size, self.used, len) {
            self.size = self.new_segment_size;
            self.used = format::SEGMENT_HEADER_LEN as u64;
        }
        self.used += format::align8(u64::from(len));
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
            let rolled = mirror.would_roll(len);
            writer
                .append(&NewRecord {
                    schema_id: 1,
                    venue_ts_ns: 0,
                    local_ts_ns: 0,
                    payload: &vec![0; len],
                })
                .unwrap();
            mirror.appended(len);
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
