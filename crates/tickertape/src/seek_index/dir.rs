//! The directory `seek-index/` and the names in it (ADR 0045 → Names). Every access goes through one descriptor of
//! it, opened without following a symlink, so that nothing here can ever reach another journal's files.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use rustix::fs::{AtFlags, Mode, OFlags};

use super::DIR_NAME;
use crate::fault::{self, Point};

/// Digits of a base sequence number in a name.
const BASE_DIGITS: usize = 20;
const INDEX_SUFFIX: &[u8] = b".idx";
const TMP_SUFFIX: &[u8] = b".idx-tmp";

/// The name of the index of the segment starting at `base`.
pub(crate) fn index_name(base: u64) -> String {
    format!("{base:020}.idx")
}

/// The name of an index of that segment while it is being written.
pub(crate) fn tmp_name(base: u64, pid: u32, n: u64) -> String {
    format!(".{base:020}.{pid}.{n}.idx-tmp")
}

/// What a name in `seek-index/` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Name {
    /// The index of the segment with this base, or a name reserved for a later kind of index of it.
    Index(u64),
    /// An index being written.
    Temporary,
}

/// What `name` is, or `None` if it has no meaning there.
pub(crate) fn parse_name(name: &[u8]) -> Option<Name> {
    if let Some(rest) = name.strip_prefix(b".") {
        let (_, rest) = split_base(rest)?;
        let mut parts = rest.strip_suffix(TMP_SUFFIX)?.split(|&b| b == b'.');
        // `.{pid}.{n}`: an empty part before the first dot, then the two numbers.
        let (empty, pid, n) = (parts.next()?, parts.next()?, parts.next()?);
        let number = |b: &[u8]| (1..=20).contains(&b.len()) && b.iter().all(u8::is_ascii_digit);
        return (empty.is_empty() && parts.next().is_none() && number(pid) && number(n))
            .then_some(Name::Temporary);
    }
    let (base, rest) = split_base(name)?;
    rest.starts_with(INDEX_SUFFIX).then_some(Name::Index(base))
}

/// Splits a leading base sequence number (exactly 20 digits, at most `u64::MAX - 1`) off `name`.
fn split_base(name: &[u8]) -> Option<(u64, &[u8])> {
    let (digits, rest) = name.split_at_checked(BASE_DIGITS)?;
    let base = digits.iter().try_fold(0_u64, |value, &digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })?;
    (base != u64::MAX).then_some((base, rest))
}

/// An open `seek-index/`.
#[derive(Debug)]
pub(crate) struct IndexDir(File);

impl IndexDir {
    /// Opens `seek-index/` of the journal in `journal`. `None` if there is none, or what has the name is a symlink or
    /// not a directory: those count as absent.
    pub(crate) fn open(journal: &Path) -> io::Result<Option<IndexDir>> {
        let opened = retry(|| rustix::fs::open(journal.join(DIR_NAME), DIR_FLAGS, Mode::empty()));
        match opened {
            Ok(fd) => Ok(Some(IndexDir(fd.into()))),
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Opens `seek-index/` in the directory open as `journal`.
    pub(crate) fn open_at(journal: &File) -> io::Result<IndexDir> {
        let fd = retry(|| rustix::fs::openat(journal, DIR_NAME, DIR_FLAGS, Mode::empty()))?;
        Ok(IndexDir(fd.into()))
    }

    /// The directory itself.
    pub(crate) fn file(&self) -> &File {
        &self.0
    }

    /// Opens `name` for reading, without following a symlink and without blocking on a FIFO.
    pub(crate) fn open_file(&self, name: &str) -> io::Result<File> {
        let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = retry(|| rustix::fs::openat(&self.0, name, flags, Mode::empty()))?;
        Ok(fd.into())
    }

    /// Creates `name`, which must not exist, with mode 0600.
    pub(crate) fn create(&self, name: &str) -> io::Result<File> {
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mode = Mode::RUSR | Mode::WUSR;
        let fd = retry(|| rustix::fs::openat(&self.0, name, flags, mode))?;
        Ok(fd.into())
    }

    /// Renames `from` over `to`, both in this directory.
    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        retry(|| rustix::fs::renameat(&self.0, from, &self.0, to)).map_err(io::Error::from)
    }

    /// Removes a name. `Ok(false)` if it was not there.
    pub(crate) fn unlink(&self, name: &std::ffi::OsStr) -> io::Result<bool> {
        fault::inject(Point::SeekIndexUnlink)?;
        match retry(|| rustix::fs::unlinkat(&self.0, name, AtFlags::empty())) {
            Ok(()) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The names with a meaning, by one listing through a directory stream opened for it (on btrfs a stream shows the
    /// entries of the moment it was opened, so one that is read again would not do). Collected before anything is
    /// removed: removing entries while reading the stream can make it skip others. A listing that fails partway gives
    /// the names it had returned, and the error.
    pub(crate) fn list(&self) -> (Vec<(OsString, Name)>, io::Result<()>) {
        let mut out = Vec::new();
        let result = self.list_into(&mut out);
        fault::filter_listing(&mut out, |(name, _)| name.as_os_str());
        (out, result)
    }

    fn list_into(&self, out: &mut Vec<(OsString, Name)>) -> io::Result<()> {
        fault::inject(Point::SeekIndexList)?;
        for entry in rustix::fs::Dir::read_from(&self.0)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if let Some(kind) = parse_name(name) {
                out.push((OsString::from_vec(name.to_vec()), kind));
                fault::inject(Point::SeekIndexListEntry)?;
            }
        }
        Ok(())
    }
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Retries a system call that a signal interrupted.
fn retry<T>(call: impl FnMut() -> rustix::io::Result<T>) -> rustix::io::Result<T> {
    rustix::io::retry_on_intr(call)
}

/// What [`sweep`] did, and what is left of it to do after recovery.
#[derive(Debug, Default)]
pub(crate) struct Swept {
    /// The directory, if there is one.
    dir: Option<IndexDir>,
    /// Indexes of segments older than the journal's oldest.
    orphans: BTreeSet<OsString>,
    removed: u64,
    /// Names that could not be removed (until a later attempt does), and other failures.
    stuck: BTreeSet<OsString>,
    errors: u64,
}

impl Swept {
    /// Applies the platform barrier to the directory, if there is one: best effort, a failure is counted.
    pub(crate) fn barrier(&mut self) {
        let applied = self.dir.as_ref().map(|dir| {
            fault::inject(Point::SeekIndexBarrier)
                .and_then(|()| crate::mmap::full_barrier(dir.file()))
        });
        if matches!(applied, Some(Err(_))) {
            self.errors = self.errors.saturating_add(1);
        }
    }

    /// After recovery: removes the indexes of segments that are gone.
    pub(crate) fn remove_orphans(&mut self) {
        for name in std::mem::take(&mut self.orphans) {
            self.unlink(name);
        }
    }

    /// How many names were removed, and how many failures there were.
    pub(crate) fn counts(&self) -> (u64, u64) {
        (
            self.removed,
            self.errors.saturating_add(self.stuck.len() as u64),
        )
    }

    fn unlink(&mut self, name: OsString) {
        let Some(dir) = &self.dir else { return };
        match dir.unlink(&name) {
            Ok(removed) => {
                self.removed = self.removed.saturating_add(u64::from(removed));
                self.stuck.remove(&name);
            }
            Err(_) => {
                self.stuck.insert(name);
            }
        }
    }
}

/// Recovery step 2a (ADR 0045 → Life cycle): removes every temporary file and every index of a segment at or above
/// `domain`, the start of the recovery domain, before recovery changes anything; in two passes, each with a directory
/// stream of its own, as the builder's fence needs. Then syncs the directory. Best effort: an optional cache never
/// stops a writer, and an index that stays is still checked by value (T5).
///
/// `oldest` is the base of the journal's oldest segment: indexes below it are left for [`Swept::remove_orphans`].
pub(crate) fn sweep(journal: &Path, domain: u64, oldest: Option<u64>) -> Swept {
    let mut swept = Swept::default();
    match IndexDir::open(journal) {
        Ok(Some(dir)) => swept.dir = Some(dir),
        Ok(None) => return swept,
        Err(_) => {
            swept.errors = 1;
            return swept;
        }
    }
    for _ in 0..2 {
        let Some((names, listed)) = swept.dir.as_ref().map(IndexDir::list) else {
            return swept;
        };
        for (name, kind) in names {
            match kind {
                Name::Temporary => swept.unlink(name),
                Name::Index(base) if base >= domain => swept.unlink(name),
                Name::Index(base) if oldest.is_some_and(|o| base < o) => {
                    swept.orphans.insert(name);
                }
                Name::Index(_) => {}
            }
        }
        if listed.is_err() {
            swept.errors = swept.errors.saturating_add(1);
        }
        fault::hook(Point::SeekIndexListed);
    }
    let synced = swept.dir.as_ref().map(|dir| {
        fault::inject(Point::SeekIndexSync).and_then(|()| crate::mmap::sync_all(dir.file()))
    });
    if matches!(synced, Some(Err(_))) {
        swept.errors = swept.errors.saturating_add(1);
    }
    fault::hook(Point::SeekIndexRemoved);
    swept
}

/// Removes the index of the segment starting at `base` after the segment is gone (retention, `archive`). Best effort.
pub(crate) fn remove_index(journal: &Path, base: u64) {
    if let Ok(Some(dir)) = IndexDir::open(journal) {
        let _ = dir.unlink(std::ffi::OsStr::from_bytes(index_name(base).as_bytes()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let index = |base| Some(Name::Index(base));
        let temporary = Some(Name::Temporary);
        for (name, expected) in [
            ("00000000000000000000.idx", index(0)),
            ("00000000000000001000.idx", index(1000)),
            ("18446744073709551614.idx", index(u64::MAX - 1)),
            ("18446744073709551615.idx", None),
            ("99999999999999999999.idx", None),
            ("00000000000000000000.idx2", index(0)),
            ("00000000000000000000.idx.bak", index(0)),
            ("00000000000000000000.idx-tmp", index(0)),
            ("00000000000000000000.id", None),
            ("0000000000000000000.idx", None),
            ("000000000000000000000.idx", None),
            ("0000000000000000000a.idx", None),
            ("+0000000000000000000.idx", None),
            ("00000000000000000000.IDX", None),
            ("00000000000000000000.tape", None),
            ("", None),
            (".", None),
            ("foo.idx-tmp", None),
            (".00000000000000000007.123.456.idx-tmp", temporary),
            (
                ".00000000000000000007.1.18446744073709551615.idx-tmp",
                temporary,
            ),
            (".18446744073709551615.1.1.idx-tmp", None),
            (".00000000000000000007.123.idx-tmp", None),
            (".00000000000000000007.123.456.789.idx-tmp", None),
            (".00000000000000000007..456.idx-tmp", None),
            (".00000000000000000007.123..idx-tmp", None),
            (".00000000000000000007.123.4x6.idx-tmp", None),
            (".00000000000000000007.123.456.idx-tmp2", None),
            (".00000000000000000007x123.456.idx-tmp", None),
            (".00000000000000000007.123.456.idx", None),
            (
                ".00000000000000000007.123456789012345678901.4.idx-tmp",
                None,
            ),
            (".7.123.456.idx-tmp", None),
        ] {
            assert_eq!(parse_name(name.as_bytes()), expected, "{name}");
        }
        assert_eq!(parse_name(b"00000000000000000003.idx\xff"), index(3));
        assert_eq!(parse_name(tmp_name(7, 1, 2).as_bytes()), temporary);
        assert_eq!(
            parse_name(tmp_name(u64::MAX - 1, u32::MAX, u64::MAX).as_bytes()),
            temporary
        );
        assert_eq!(parse_name(index_name(9).as_bytes()), index(9));
    }
}
