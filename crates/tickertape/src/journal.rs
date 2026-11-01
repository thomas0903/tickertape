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
/// Name of the notify file (ADR 0017).
pub(crate) const NOTIFY_FILE: &str = "notify";
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
