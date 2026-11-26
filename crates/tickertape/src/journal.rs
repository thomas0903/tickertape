//! Directory-level helpers shared by the writer, the readers and inspection: naming, listing, opening and
//! validating segment files, and file identity.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirEntryExt, FileExt, MetadataExt};
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

/// Gives the blocks of an unpublished segment file (a temporary segment or the spare, `file` opened at `path`) back to
/// the filesystem and removes its name. Truncated first: unlinking and closing free nothing while another descriptor
/// of the file is open, such as the copy a child process forked at that moment holds until its `exec`, and the roll
/// that has just run out of space tries again right after (ADR 0049 → amendment of 2026-11-27). Errors are ignored:
/// the name is a leftover the next open removes.
pub(crate) fn discard_unpublished(file: &File, path: &Path) {
    let _ = file.set_len(0);
    let _ = fs::remove_file(path);
}

/// Lists the segments in `dir` as `(base_seq, path)`, sorted by `base_seq`.
pub(crate) fn list_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    for entry in list_entries(dir)? {
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

/// Starts a listing of `dir`.
fn list_entries(dir: &Path) -> Result<fs::ReadDir> {
    crate::fault::note_access(crate::fault::Access::ListJournal);
    fs::read_dir(dir).map_err(|e| Error::io("list journal directory", dir, e))
}

/// A segment's name as one listing of the directory showed it ([`list_bases`]): nothing of the file was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Listed {
    /// Base sequence number, from the name.
    pub(crate) base: u64,
    /// The directory entry's inode number, if the entry names a regular file. Only meaningful compared with that of
    /// another listing of the same directory: a different number is a different file.
    pub(crate) inode: Option<u64>,
}

/// Lists the segments in `dir` by name only, sorted by base, as [`list_segments`] does. (The type of each entry comes with
/// the listing on the supported file systems; where a file system reports none, `file_type` asks with an `lstat` per
/// entry.)
pub(crate) fn list_bases(dir: &Path) -> Result<Vec<Listed>> {
    list_bases_and(dir, |_| {})
}

/// [`list_bases`], which also passes every other name that is valid UTF-8 to `other`, from the same listing.
pub(crate) fn list_bases_and(dir: &Path, mut other: impl FnMut(&str)) -> Result<Vec<Listed>> {
    let mut out = Vec::new();
    for entry in list_entries(dir)? {
        let entry = entry.map_err(|e| Error::io("list journal directory", dir, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(base) = format::parse_segment_file_name(name) else {
            other(name);
            continue;
        };
        let inode = entry
            .file_type()
            .is_ok_and(|t| t.is_file())
            .then(|| entry.ino());
        out.push(Listed { base, inode });
    }
    out.sort_unstable_by_key(|s| s.base);
    Ok(out)
}

/// How many bases after the oldest one [`Bases::later`] keeps.
pub(crate) const LATER_BASES: usize = 7;

/// The base sequence numbers [`find_bases`] picks out of a listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Bases {
    /// The oldest segment's base (`None` if the listing has no segments).
    pub(crate) oldest: Option<u64>,
    /// The newest segment's base that is `<= seq` (`None` if there is none).
    pub(crate) floor: Option<u64>,
    /// The next [`LATER_BASES`] bases after `oldest`, ascending (`None` past the last one listed).
    pub(crate) later: [Option<u64>; LATER_BASES],
}

impl Bases {
    /// What the listing would have been without its oldest segment, if it holds another one: the next base becomes
    /// the oldest, and a floor that was the oldest base is gone (no other listed base was `<= seq`). `None` if the
    /// listing holds no base after the oldest, or none that [`Bases::later`] kept.
    pub(crate) fn without_oldest(self) -> Option<Bases> {
        let removed = self.oldest?;
        let next = self.later[0]?;
        let mut later = [None; LATER_BASES];
        later[..LATER_BASES - 1].copy_from_slice(&self.later[1..]);
        Some(Bases {
            oldest: Some(next),
            floor: self.floor.filter(|f| *f != removed),
            later,
        })
    }

    /// Takes `base` into the oldest bases kept, in order.
    fn keep_if_old(&mut self, base: u64) {
        let mut carry = match self.oldest {
            None => {
                self.oldest = Some(base);
                return;
            }
            Some(oldest) if base < oldest => {
                self.oldest = Some(base);
                oldest
            }
            // Most bases of a long listing are newer than every one kept.
            Some(_) if self.later[LATER_BASES - 1].is_some_and(|newest| base > newest) => return,
            Some(_) => base,
        };
        for slot in &mut self.later {
            match *slot {
                None => {
                    *slot = Some(carry);
                    return;
                }
                Some(kept) if carry < kept => {
                    *slot = Some(carry);
                    carry = kept;
                }
                Some(_) => {}
            }
        }
    }
}

/// Picks the oldest bases and the newest base `<= seq` out of one listing of `dir`, as [`list_segments`] would list
/// it, without collecting or sorting the names: time proportional to the directory's entries, memory constant.
pub(crate) fn find_bases(dir: &Path, seq: u64) -> Result<Bases> {
    let mut out = Bases::default();
    for entry in list_entries(dir)? {
        let entry = entry.map_err(|e| Error::io("list journal directory", dir, e))?;
        if let Some(base) = entry
            .file_name()
            .to_str()
            .and_then(format::parse_segment_file_name)
        {
            out.keep_if_old(base);
            if base <= seq {
                out.floor = Some(out.floor.map_or(base, |f| f.max(base)));
            }
        }
    }
    Ok(out)
}

/// From one listing of `dir`: the bases above `seq`, each with the inode number the listing gives for its name
/// (`d_ino`, no `stat`), and whether a segment with base `seq` exists. Memory for the bases above only (none, on a
/// journal whose reader is at the head).
pub(crate) fn bases_from(dir: &Path, seq: u64) -> Result<(Vec<(u64, u64)>, bool)> {
    let entries = fs::read_dir(dir).map_err(|e| Error::io("list journal directory", dir, e))?;
    let (mut above, mut at) = (Vec::new(), false);
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("list journal directory", dir, e))?;
        if let Some(base) = entry
            .file_name()
            .to_str()
            .and_then(format::parse_segment_file_name)
        {
            match base.cmp(&seq) {
                std::cmp::Ordering::Greater => above.push((base, entry.ino())),
                std::cmp::Ordering::Equal => at = true,
                std::cmp::Ordering::Less => {}
            }
        }
    }
    Ok((above, at))
}

/// Lists stale temporary segment files (`*.tape.tmp`) in `dir`.
pub(crate) fn list_tmp_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in list_entries(dir)? {
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

/// The identity of whatever `path` names now, or `None` if nothing does ("not found").
///
/// Any other failure (`EACCES`, `EIO`, `ENOMEM`, ...) says nothing about whether the file is still in place, so it
/// is returned as an error rather than taken for a removal.
pub(crate) fn path_identity(path: &Path) -> Result<Option<Identity>> {
    match fs::metadata(path) {
        Ok(m) => Ok(Some((m.dev(), m.ino()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io("stat segment", path, e)),
    }
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

/// The window of [`marker_reached`]: the record right before a marker's position usually ends in the page before it.
const MARKER_WINDOW: usize = 4096;

/// [`format::marker_reached`] for the segment open as `file` (`size` bytes, its header validated, `path` in errors),
/// read with `pread`: whether its records reach the position of the checkpoint slot `slot`, exactly as
/// [`Writer::open`](crate::Writer::open) decides it (ADR 0056). Reads only the record right before the slot's offset
/// (the backward probe from the offset to that record's start, and the record): one `pread` of 4 KiB, or more for a
/// longer record; when the records do not reach the position, the probe reads back to the segment's first slot.
///
/// # Errors
///
/// [`Error::Io`] if a read fails (the answer is then unknown, not `false`).
pub(crate) fn marker_reached(
    file: &File,
    path: &Path,
    size: u64,
    slot: &format::CheckpointSlot,
) -> Result<bool> {
    let view = PreadView::new(file, size, MARKER_WINDOW).ending_at(slot.offset);
    let reached = format::marker_reached(&view, slot);
    match view.take_error() {
        Some(e) => Err(Error::io("read checkpoint segment", path, e)),
        None => Ok(reached),
    }
}

/// Window size for [`scan_open`].
pub(crate) const SCAN_WINDOW: usize = 1 << 20;

/// A [`format::SegmentView`] over a segment file read with `pread` in windows, so that scanning a segment needs memory
/// for one window of at most [`SCAN_WINDOW`] bytes, never the whole file: callers read a larger record in pieces of
/// at most that size ([`format::SegmentView::max_piece`]). An I/O error makes the view return `None`; the
/// error is kept and reported by [`scan_open`].
pub(crate) struct PreadView<'f> {
    file: &'f File,
    len: u64,
    /// Bytes to fetch per `pread` (more if one request is larger).
    window_len: u64,
    /// The bytes at `[window.0, window.0 + window.1.len())`.
    window: std::cell::RefCell<(u64, Vec<u8>)>,
    error: std::cell::RefCell<Option<std::io::Error>>,
    /// Set by [`PreadView::ending_at`]: a read that ends at or before this offset fetches the window that ends there,
    /// or a whole number of windows before it, instead of the window that starts at the read.
    end: Option<u64>,
}

impl<'f> PreadView<'f> {
    /// A view of `file`, whose length is `len`, fetching `window_len` bytes per `pread`.
    pub(crate) fn new(file: &'f File, len: u64, window_len: usize) -> PreadView<'f> {
        PreadView {
            file,
            len,
            window_len: window_len as u64,
            window: std::cell::RefCell::new((0, Vec::new())),
            error: std::cell::RefCell::new(None),
            end: None,
        }
    }

    /// The same view for reading backward from `end`: the windows are laid out to end at `end`, so that a probe that
    /// walks back from `end` (as [`format::record_ends_at`] does) and then validates the record it finds reads each
    /// byte before `end` once, one window per `pread`. Reads after `end` are fetched as by [`PreadView::new`].
    pub(crate) fn ending_at(mut self, end: u64) -> PreadView<'f> {
        self.end = Some(end);
        self
    }

    /// Forgets the cached window, so that the next access reads the file again.
    pub(crate) fn invalidate(&self) {
        self.window.borrow_mut().1.clear();
    }

    /// The first I/O error a read hit, if any (the view returned `None` for it).
    pub(crate) fn take_error(&self) -> Option<std::io::Error> {
        self.error.borrow_mut().take()
    }

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
        // With `end`, the window that ends at `end` or a whole number of windows before it and holds the read, or
        // (where the read straddles two of them) one that starts at the read.
        let from = match self.end {
            Some(end) if offset + len <= end && self.window_len > 0 => {
                let tile_end = end - (end - (offset + len)) / self.window_len * self.window_len;
                tile_end.saturating_sub(self.window_len).min(offset)
            }
            _ => offset,
        };
        let want = (offset + len - from)
            .max(self.window_len)
            .min(self.len - from);
        buf.resize(want as usize, 0);
        match self.file.read_exact_at(buf, from) {
            Ok(()) => {
                *start = from;
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

/// [`scan_open`] of the segment file at `path` (used by this module's tests).
#[cfg(test)]
pub(crate) fn scan_file(path: &Path, base: u64) -> Result<format::ScanResult> {
    let file = File::open(path).map_err(|e| Error::io("open segment", path, e))?;
    scan_open(&file, path, base)
}

/// Scans an open segment file from its first record slot, expecting `base` there, reading it with `pread` in bounded
/// windows (the file is never mapped, so other processes can't affect memory safety); `path` names it in errors.
pub(crate) fn scan_open(file: &File, path: &Path, base: u64) -> Result<format::ScanResult> {
    scan_open_from(file, path, format::SEGMENT_HEADER_LEN as u64, base)
}

/// [`scan_open`] from `offset`, expecting `seq` there.
pub(crate) fn scan_open_from(
    file: &File,
    path: &Path,
    offset: u64,
    seq: u64,
) -> Result<format::ScanResult> {
    with_pread_view(file, path, |view| format::scan_segment(view, offset, seq))
}

/// The commit word at `offset` of an open segment file, read with a fresh 4-byte `pread` (0 if it lies outside the
/// file or is misaligned).
pub(crate) fn commit_word_open(file: &File, path: &Path, offset: u64) -> Result<u32> {
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

/// [`format::find_history_after_empty`] on an open segment file, read with `pread` in bounded windows.
pub(crate) fn history_after_empty_open(
    file: &File,
    path: &Path,
    offset: u64,
    expected_seq: u64,
) -> Result<Option<format::LaterHistory>> {
    with_pread_view(file, path, |view| {
        format::find_history_after_empty(view, offset, expected_seq)
    })
}

fn with_pread_view<R>(file: &File, path: &Path, f: impl FnOnce(&PreadView<'_>) -> R) -> Result<R> {
    let len = file
        .metadata()
        .map_err(|e| Error::io("stat segment", path, e))?
        .len();
    let view = PreadView::new(file, len, SCAN_WINDOW);
    let result = f(&view);
    if let Some(e) = view.take_error() {
        return Err(Error::io("read segment", path, e));
    }
    Ok(result)
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    use crate::{NewRecord, Writer, WriterConfig};

    #[test]
    fn find_bases_agrees_with_the_sorted_listing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_bases(dir.path(), 5).unwrap(), Bases::default());
        // Names in no particular order, and files that are not segments: a temporary segment, the marker, the
        // prepared next segment, a name that only looks like one.
        let bases = [40u64, 7, 1000, 0, 123_456_789, 41];
        for base in bases {
            fs::write(segment_path(dir.path(), base), b"").unwrap();
        }
        for other in [
            tmp_path(dir.path(), 3),
            dir.path().join(CHECKPOINT_FILE),
            dir.path().join(SPARE_FILE),
            dir.path().join("00000000000000000002.tape.bak"),
        ] {
            fs::write(other, b"").unwrap();
        }
        let listed: Vec<u64> = list_segments(dir.path())
            .unwrap()
            .into_iter()
            .map(|(b, _)| b)
            .collect();
        assert_eq!(listed, [0, 7, 40, 41, 1000, 123_456_789]);
        let by_name = list_bases(dir.path()).unwrap();
        assert_eq!(by_name.iter().map(|s| s.base).collect::<Vec<_>>(), listed);
        for s in by_name {
            let meta = fs::metadata(segment_path(dir.path(), s.base)).unwrap();
            assert_eq!(s.inode, Some(meta.ino()), "segment {}", s.base);
        }
        for seq in [
            0,
            1,
            6,
            7,
            8,
            40,
            41,
            999,
            1000,
            1001,
            123_456_789,
            u64::MAX,
        ] {
            let mut later = [None; LATER_BASES];
            for (slot, base) in later.iter_mut().zip(listed.iter().skip(1)) {
                *slot = Some(*base);
            }
            let expected = Bases {
                oldest: listed.first().copied(),
                floor: listed.iter().rev().find(|b| **b <= seq).copied(),
                later,
            };
            assert_eq!(find_bases(dir.path(), seq).unwrap(), expected, "seq {seq}");
            // Without the oldest segment, as a listing taken after its removal would show it.
            let without = expected.without_oldest().unwrap();
            assert_eq!(without.oldest, Some(7), "seq {seq}");
            assert_eq!(
                without.later[..4],
                [Some(40), Some(41), Some(1000), Some(123_456_789)]
            );
            assert_eq!(without.later[4..], [None; LATER_BASES - 4]);
            let floor = listed.iter().rev().find(|b| **b <= seq && **b > 0).copied();
            assert_eq!(without.floor, floor, "seq {seq}");
        }
        fs::remove_file(segment_path(dir.path(), 0)).unwrap();
        let mut later = [None; LATER_BASES];
        later[..4].copy_from_slice(&[Some(40), Some(41), Some(1000), Some(123_456_789)]);
        assert_eq!(
            find_bases(dir.path(), 3).unwrap(),
            Bases {
                oldest: Some(7),
                floor: None,
                later,
            }
        );
        // Only the oldest bases are kept: one listing of more segments than that, in any order.
        for base in (2_000..2_020).rev() {
            fs::write(segment_path(dir.path(), base), b"").unwrap();
        }
        let bases = find_bases(dir.path(), u64::MAX).unwrap();
        assert_eq!(bases.oldest, Some(7));
        assert_eq!(bases.floor, Some(123_456_789));
        assert_eq!(
            bases.later,
            [40, 41, 1000, 2_000, 2_001, 2_002, 2_003].map(Some)
        );
        assert_eq!(bases.without_oldest().unwrap().later[6], None);
        let mut last = bases;
        for _ in 0..LATER_BASES {
            last = last.without_oldest().unwrap();
        }
        assert_eq!(
            (last.oldest, last.later),
            (Some(2_003), [None; LATER_BASES])
        );
        assert_eq!(last.without_oldest(), None);
    }

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
        let open = File::open(&path).unwrap();
        assert!(matches!(
            history_after_empty_open(&open, &path, first, base).unwrap(),
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
        assert_eq!(commit_word_open(&open, &path, first).unwrap(), 100 + 40);
        assert_eq!(commit_word_open(&open, &path, first + 4).unwrap(), 0);
        assert_eq!(commit_word_open(&open, &path, 4 << 20).unwrap(), 0);
    }
}
