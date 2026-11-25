//! Reading an index: what a reader's seek and the builder's check of an existing file have in common (ADR 0045, steps
//! 3, 4, 6 and 8a). Everything here is an ordinary read into this process's own memory, also for mapped readers.

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::atomic::{Ordering, fence};

use super::codec::{self, Entry, Index};
use super::dir::{self, IndexDir};
use crate::format::{self, SEGMENT_HEADER_LEN};

/// Why an index is not used.
#[derive(Debug)]
pub(crate) enum Rejected {
    /// There is none.
    Missing,
    /// It could not be opened or read.
    Io(io::Error),
    /// Its file or its content is not what a reader uses.
    Unusable(&'static str),
    /// A reader would not use it because of its owner or mode; the content has not been looked at.
    Permissions(&'static str),
}

impl From<io::Error> for Rejected {
    fn from(e: io::Error) -> Rejected {
        Rejected::Io(e)
    }
}

/// An index file's bytes, and the segment's first 64 bytes to check them against ([`Loaded::index`]).
#[derive(Debug)]
pub(crate) struct Loaded {
    bytes: Vec<u8>,
    segment_header: [u8; SEGMENT_HEADER_LEN],
    /// Why readers ignore the file although its content is right (its owner or mode); only with `lenient`.
    pub(crate) ignored: Option<&'static str>,
}

impl Loaded {
    /// The index, if it passes every check of step 4; otherwise what is wrong with it.
    pub(crate) fn index(&self) -> Result<Index<'_>, &'static str> {
        codec::decode(&self.bytes, &self.segment_header)
    }
}

/// Step 3 and the read of step 4: opens and reads the index of the segment open as `segment`, whose base is `base`. With
/// `lenient`, a wrong owner or mode is noted in the result instead of refused.
pub(crate) fn load(
    dir: &IndexDir,
    segment: &File,
    base: u64,
    lenient: bool,
) -> Result<Loaded, Rejected> {
    let file = open(dir, base)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Rejected::Unusable("not a regular file"));
    }
    let count = codec::entries_in(meta.len())
        .ok_or(Rejected::Unusable("its length is not that of an index"))?;
    let owner = segment.metadata()?.uid();
    let ignored = if meta.uid() != owner && meta.uid() != 0 {
        Some("its owner is neither the segment's nor root")
    } else if meta.mode() & 0o022 != 0 {
        Some("it is writable by its group or by others")
    } else {
        None
    };
    if let Some(why) = ignored.filter(|_| !lenient) {
        return Err(Rejected::Permissions(why));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(codec::HEADER_LEN + count * codec::ENTRY_LEN)
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    bytes.resize(codec::HEADER_LEN + count * codec::ENTRY_LEN, 0);
    file.read_exact_at(&mut bytes, 0)?;
    let mut segment_header = [0; SEGMENT_HEADER_LEN];
    segment.read_exact_at(&mut segment_header, 0)?;
    Ok(Loaded {
        bytes,
        segment_header,
        ignored,
    })
}

/// Opens the index of segment `base`, telling a name that is not a file to be read apart from a failure.
fn open(dir: &IndexDir, base: u64) -> Result<File, Rejected> {
    dir.open_file(&dir::index_name(base))
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ENOENT) => Rejected::Missing,
            Some(libc::ELOOP) => Rejected::Unusable("a symbolic link"),
            Some(libc::ENXIO | libc::EOPNOTSUPP) => Rejected::Unusable("not a regular file"),
            _ => Rejected::Io(e),
        })
}

/// The version of the index of segment `base`, if a regular file with an index's magic has its name.
pub(crate) fn version(dir: &IndexDir, base: u64) -> Option<u16> {
    let file = open(dir, base).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut head = [0; 10];
    file.read_exact_at(&mut head, 0).ok()?;
    codec::version_of(&head)
}

/// Step 6: whether the segment's seal is where the index says, naming the number it says. A commit word has been
/// observed by a `pread`, so an `Acquire` fence follows (ADR 0003): what the writer stored before the seal happens
/// before what follows.
pub(crate) fn sealed(segment: &File, index: &Index<'_>) -> io::Result<bool> {
    let mut seal = [0; format::SEAL_LEN];
    segment.read_exact_at(&mut seal, index.seal_offset)?;
    fence(Ordering::Acquire);
    Ok(seal[..4] == format::SEAL_MARKER.to_le_bytes()
        && seal[4..] == format::seal_crc(index.next_seq).to_le_bytes())
}

/// Step 8a: whether the segment holds the entry's commit word and CRC32C at its offset.
pub(crate) fn in_place(segment: &File, entry: &Entry) -> io::Result<bool> {
    let mut first = [0; 8];
    segment.read_exact_at(&mut first, entry.offset)?;
    fence(Ordering::Acquire);
    Ok(first == entry.first_bytes())
}
