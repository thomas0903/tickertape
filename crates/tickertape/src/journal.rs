//! Directory-level helpers shared by the writer, the readers and inspection: naming, listing, opening and
//! validating segment files, and file identity.

use std::fs::{self, File};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, SegmentHeader};

/// Name of the lock file.
pub(crate) const LOCK_FILE: &str = "writer.lock";
/// Name of the checkpoint marker file.
pub(crate) const CHECKPOINT_FILE: &str = "checkpoint";
/// Name of the notify record (ADRs 0017 and 0021).
pub(crate) const NOTIFY_FILE: &str = "notify";
/// Name of the prepared, not yet published next segment (ADR 0022).
pub(crate) const SPARE_FILE: &str = "next-segment.tmp";
/// Name of the quarantine directory.
pub(crate) const QUARANTINE_DIR: &str = "quarantine";
/// Suffix of temporary segment files.
pub(crate) const TMP_SUFFIX: &str = ".tmp";

/// Path of the segment starting at `base_seq`.
pub(crate) fn segment_path(dir: &Path, base_seq: u64) -> PathBuf {
    dir.join(format::segment_file_name(base_seq))
}

/// Path of the temporary file for the segment starting at `base_seq`.
pub(crate) fn tmp_path(dir: &Path, base_seq: u64) -> PathBuf {
    dir.join(format!(
        "{}{TMP_SUFFIX}",
        format::segment_file_name(base_seq)
    ))
}

/// Lists the segments in `dir` as `(base_seq, path)`, sorted by `base_seq`.
pub(crate) fn list_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let entries = fs::read_dir(dir).map_err(|e| Error::io("list journal directory", dir, e))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("list journal directory", dir, e))?;
        if let Some(name) = entry.file_name().to_str() {
            if let Some(base) = format::parse_segment_file_name(name) {
                out.push((base, entry.path()));
            }
        }
    }
    out.sort_unstable_by_key(|(base, _)| *base);
    Ok(out)
}

/// Lists stale temporary segment files (`*.tape.tmp`) in `dir`.
pub(crate) fn list_tmp_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = fs::read_dir(dir).map_err(|e| Error::io("list journal directory", dir, e))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("list journal directory", dir, e))?;
        if let Some(name) = entry.file_name().to_str() {
            if let Some(stem) = name.strip_suffix(TMP_SUFFIX) {
                if format::parse_segment_file_name(stem).is_some() {
                    out.push(entry.path());
                }
            }
        }
    }
    Ok(out)
}

/// Reads and validates a segment's header from an open file: header rules, actual file size, and file name.
pub(crate) fn read_header(file: &File, path: &Path, base_from_name: u64) -> Result<SegmentHeader> {
    let actual = file
        .metadata()
        .map_err(|e| Error::io("stat segment", path, e))?
        .len();
    let mut buf = [0u8; format::SEGMENT_HEADER_LEN];
    if actual < buf.len() as u64 {
        return Err(Error::corrupt(
            path,
            0,
            CorruptReason::SizeMismatch {
                expected: format::SEGMENT_HEADER_LEN as u64,
                actual,
            },
        ));
    }
    file.read_exact_at(&mut buf, 0)
        .map_err(|e| Error::io("read segment header", path, e))?;
    if buf[..8] == format::MAGIC {
        let version = u16::from_le_bytes([buf[8], buf[9]]);
        if version != format::FORMAT_VERSION {
            return Err(Error::UnsupportedVersion {
                path: path.to_path_buf(),
                found: version,
                supported: format::FORMAT_VERSION,
            });
        }
    }
    let header = format::decode_segment_header(&buf).map_err(|r| Error::corrupt(path, 0, r))?;
    if header.segment_size != actual {
        return Err(Error::corrupt(
            path,
            0,
            CorruptReason::SizeMismatch {
                expected: header.segment_size,
                actual,
            },
        ));
    }
    if header.base_seq != base_from_name {
        return Err(Error::corrupt(
            path,
            0,
            CorruptReason::BaseSeqMismatch {
                from_name: base_from_name,
                from_header: header.base_seq,
            },
        ));
    }
    Ok(header)
}

/// A file's identity: (device, inode).
pub(crate) type Identity = (u64, u64);

/// The identity of an open file.
pub(crate) fn file_identity(file: &File, path: &Path) -> Result<Identity> {
    let meta = file
        .metadata()
        .map_err(|e| Error::io("stat segment", path, e))?;
    Ok((meta.dev(), meta.ino()))
}

/// The identity of whatever `path` names now, or `None` if nothing.
pub(crate) fn path_identity(path: &Path) -> Option<Identity> {
    fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Opens a directory so that it can be synced.
pub(crate) fn open_dir(dir: &Path) -> Result<File> {
    File::open(dir).map_err(|e| Error::io("open directory", dir, e))
}

/// Syncs a directory's entries.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    let f = open_dir(dir)?;
    crate::mmap::sync_all(&f).map_err(|e| Error::io("sync directory", dir, e))
}

/// Window size for [`scan_file`].
const SCAN_WINDOW: usize = 1 << 20;

/// A [`format::SegmentView`] over a segment file read with `pread` in windows, so that scanning a segment needs memory
/// for one window (or one record, if larger), never the whole file. An I/O error makes the view return `None`; the
/// error is kept and reported by [`scan_file`].
struct PreadView<'f> {
    file: &'f File,
    len: u64,
    /// The bytes at `[window.0, window.0 + window.1.len())`.
    window: std::cell::RefCell<(u64, Vec<u8>)>,
    error: std::cell::RefCell<Option<std::io::Error>>,
}

impl PreadView<'_> {
    /// Makes `[offset, offset + len)` (already checked against the file length) available in the window.
    fn fill(&self, offset: u64, len: u64) -> bool {
        let mut window = self.window.borrow_mut();
        let (start, buf) = &mut *window;
        if offset >= *start && offset + len <= *start + buf.len() as u64 {
            return true;
        }
        // Every caller reads in pieces of at most `max_piece`: the window never grows past it, whatever length a
        // damaged slot claims (validation streams longer bodies, `format::read_slot`).
        debug_assert!(len <= SCAN_WINDOW as u64, "pread window of {len} bytes");
        let want = len.max(SCAN_WINDOW as u64).min(self.len - offset);
        buf.resize(want as usize, 0);
        match self.file.read_exact_at(buf, offset) {
            Ok(()) => {
                *start = offset;
                true
            }
            Err(e) => {
                buf.clear();
                self.error.borrow_mut().get_or_insert(e);
                false
            }
        }
    }
}

impl format::SegmentView for PreadView<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn commit_word(&self, offset: u64) -> Option<u32> {
        if offset % format::RECORD_ALIGN as u64 != 0 {
            return None;
        }
        self.with_bytes(offset, format::COMMIT_WORD_LEN as u64, |b| {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        })
    }

    fn with_bytes<R>(&self, offset: u64, len: u64, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        if offset.checked_add(len)? > self.len || !self.fill(offset, len) {
            return None;
        }
        let window = self.window.borrow();
        let at = (offset - window.0) as usize;
        Some(f(&window.1[at..at + len as usize]))
    }

    fn max_piece(&self) -> u64 {
        SCAN_WINDOW as u64
    }
}

/// Scans a segment file from its first record slot, expecting `base` there, reading it with `pread` in bounded
/// windows (the file is never mapped, so other processes can't affect memory safety).
pub(crate) fn scan_file(path: &Path, base: u64) -> Result<format::ScanResult> {
    scan_file_from(path, format::SEGMENT_HEADER_LEN as u64, base)
}

/// [`scan_file`] from `offset`, expecting `seq` there.
pub(crate) fn scan_file_from(path: &Path, offset: u64, seq: u64) -> Result<format::ScanResult> {
    with_pread_view(path, |view| format::scan_segment(view, offset, seq))
}

/// The commit word at `offset` of a segment file, read with a fresh 4-byte `pread` (0 if it lies outside the file or
/// is misaligned).
pub(crate) fn commit_word_in_file(path: &Path, offset: u64) -> Result<u32> {
    let file = File::open(path).map_err(|e| Error::io("open segment", path, e))?;
    let len = file
        .metadata()
        .map_err(|e| Error::io("stat segment", path, e))?
        .len();
    let word_len = format::COMMIT_WORD_LEN as u64;
    if offset % format::RECORD_ALIGN as u64 != 0
        || offset.checked_add(word_len).is_none_or(|end| end > len)
    {
        return Ok(0);
    }
    let mut word = [0; format::COMMIT_WORD_LEN];
    file.read_exact_at(&mut word, offset)
        .map_err(|e| Error::io("read segment", path, e))?;
    Ok(u32::from_le_bytes(word))
}

/// [`format::find_history_after_empty`] on a segment file, read with `pread` in bounded windows.
pub(crate) fn history_after_empty_in_file(
    path: &Path,
    offset: u64,
    expected_seq: u64,
) -> Result<Option<format::LaterHistory>> {
    with_pread_view(path, |view| {
        format::find_history_after_empty(view, offset, expected_seq)
    })
}

fn with_pread_view<R>(path: &Path, f: impl FnOnce(&PreadView<'_>) -> R) -> Result<R> {
    let file = File::open(path).map_err(|e| Error::io("open segment", path, e))?;
    let len = file
        .metadata()
        .map_err(|e| Error::io("stat segment", path, e))?
        .len();
    let view = PreadView {
        file: &file,
        len,
        window: std::cell::RefCell::new((0, Vec::new())),
        error: std::cell::RefCell::new(None),
    };
    let result = f(&view);
    if let Some(e) = view.error.into_inner() {
        return Err(Error::io("read segment", path, e));
    }
    Ok(result)
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    use crate::{NewRecord, Writer, WriterConfig};

    #[test]
    fn windowed_scan_matches_an_in_memory_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut w =
            Writer::open(dir.path(), WriterConfig::default().segment_size(4 << 20)).unwrap();
        // 1,000-byte payloads straddle the 1 MiB window boundaries; enough records to seal the first segment.
        let payload = vec![7u8; 1000];
        for _ in 0..5000 {
            w.append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &payload,
            })
            .unwrap();
        }
        drop(w);
        for (base, path) in list_segments(dir.path()).unwrap() {
            let image = fs::read(&path).unwrap();
            let expected =
                format::scan_segment(&image[..], format::SEGMENT_HEADER_LEN as u64, base);
            assert_eq!(scan_file(&path, base).unwrap(), expected, "segment {base}");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "writes megabytes")]
    fn records_longer_than_the_window_are_read_in_pieces() {
        // Review round 2, item 2: the window never grows past `SCAN_WINDOW` (asserted in `PreadView::fill`), for a
        // valid record of 3 MiB, the same record with a bad checksum, and the search after an empty commit word.
        let dir = tempfile::tempdir().unwrap();
        let mut w =
            Writer::open(dir.path(), WriterConfig::default().segment_size(4 << 20)).unwrap();
        for payload in [vec![1u8; 100], vec![7u8; 3 << 20], vec![2u8; 100]] {
            w.append(&NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: &payload,
            })
            .unwrap();
        }
        drop(w);
        let (base, path) = list_segments(dir.path()).unwrap().pop().unwrap();
        let first = format::SEGMENT_HEADER_LEN as u64;
        let image = fs::read(&path).unwrap();
        let expected = format::scan_segment(&image[..], first, base);
        assert_eq!(expected.records, 3);
        assert_eq!(scan_file(&path, base).unwrap(), expected);
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[0; 4], first).unwrap();
        assert!(matches!(
            history_after_empty_in_file(&path, first, base).unwrap(),
            Some(format::LaterHistory::Record { seq: 1, .. })
        ));
        file.write_all_at(&[0xee], first + 200 + (1 << 20)).unwrap();
        file.write_all_at(&image[first as usize..first as usize + 4], first)
            .unwrap();
        let scan = scan_file(&path, base).unwrap();
        assert_eq!(scan.records, 1);
        assert_eq!(
            scan.end,
            format::ScanEnd::Invalid(CorruptReason::BadChecksum)
        );
        assert_eq!(commit_word_in_file(&path, first).unwrap(), 100 + 40);
        assert_eq!(commit_word_in_file(&path, first + 4).unwrap(), 0);
        assert_eq!(commit_word_in_file(&path, 4 << 20).unwrap(), 0);
    }
}
