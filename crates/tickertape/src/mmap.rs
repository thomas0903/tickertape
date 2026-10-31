//! Memory maps and operating-system calls: the **only** module in this crate that contains `unsafe` code.
//!
//! Everything here is a thin, bounds-checked wrapper. The protocol reasoning lives in ADR 0003; each `unsafe`
//! block says which invariant it relies on and where that invariant is upheld.
//!
//! - [`MapRw`]: the writer's `PROT_READ | PROT_WRITE`, `MAP_SHARED` mapping. Commit words are stored with
//!   `Release` and loaded with `Acquire` through `AtomicU32::from_ptr` (the memory is writable, as `from_ptr`
//!   requires). Body bytes are written through `&mut [u8]` slices of exactly the uncommitted range.
//! - [`MapRo`]: the zero-copy reader's `PROT_READ`, `MAP_SHARED` mapping. Commit words are loaded with `Relaxed`
//!   (documented as permitted on read-only memory for 4-byte loads on x86-64 and aarch64), followed by an `Acquire`
//!   fence. Committed bytes are handed out only through an `unsafe fn`, whose contract is `Reader::open_mapped`'s.
//! - File-level calls: `flock`, preallocation, `fdatasync`/`fsync`/`F_FULLFSYNC`, `msync`, `pread`.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// Checks that `[offset, offset + len)` lies inside a mapping of `map_len` bytes. Returns the range end.
fn checked_range(map_len: usize, offset: u64, len: u64) -> Option<usize> {
    let end = offset.checked_add(len)?;
    if end > map_len as u64 {
        return None;
    }
    usize::try_from(end).ok()
}

/// Checks that a commit word at `offset` is 8-aligned and inside a mapping of `map_len` bytes.
fn checked_word(map_len: usize, offset: u64) -> Option<usize> {
    if offset % 8 != 0 {
        return None;
    }
    checked_range(map_len, offset, 4)?;
    usize::try_from(offset).ok()
}

fn last_os_error() -> io::Error {
    io::Error::last_os_error()
}

/// Maps `len` bytes of `file` from offset 0 with the given protection, shared.
fn map(file: &File, len: usize, prot: libc::c_int) -> io::Result<NonNull<u8>> {
    if len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot map zero bytes",
        ));
    }
    // SAFETY: mmap with a null hint, a valid open file descriptor, offset 0 and a non-zero length has no memory-safety
    // preconditions; it either returns a fresh mapping that no Rust object aliases yet, or MAP_FAILED.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(last_os_error());
    }
    NonNull::new(ptr.cast::<u8>()).ok_or_else(|| io::Error::other("mmap returned null"))
}

/// Unmaps a mapping created by [`map`].
///
/// # Safety
///
/// `ptr` and `len` must describe a live mapping returned by [`map`], and no reference into it may outlive this call.
unsafe fn unmap(ptr: NonNull<u8>, len: usize) {
    // SAFETY: guaranteed by the caller. A failure can only mean invalid arguments, which the caller rules out.
    unsafe {
        libc::munmap(ptr.as_ptr().cast(), len);
    }
}

/// The system page size.
pub(crate) fn page_size() -> usize {
    // SAFETY: sysconf has no memory-safety preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n <= 0 { 4096 } else { n as usize }
}

/// `msync(MS_SYNC)` on the pages covering `[offset, offset + len)` of a mapping.
fn msync(ptr: NonNull<u8>, map_len: usize, offset: u64, len: u64) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let end = checked_range(map_len, offset, len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "msync range out of bounds"))?;
    let page = page_size();
    let start = (offset as usize) / page * page;
    // SAFETY: `[start, end)` lies within the mapping (checked above; `start` is rounded down to a page boundary at or
    // after the mapping's page-aligned start). msync only flushes; it doesn't create references or change memory.
    let rc = unsafe { libc::msync(ptr.as_ptr().add(start).cast(), end - start, libc::MS_SYNC) };
    if rc != 0 {
        Err(last_os_error())
    } else {
        Ok(())
    }
}

/// The writer's read-write shared mapping of one segment file.
#[derive(Debug)]
pub(crate) struct MapRw {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: `MapRw` owns its mapping exclusively within this process (it is never cloned and hands out borrows tied to
// `&self`/`&mut self`), so moving it to another thread is like moving a `Vec<u8>`.
unsafe impl Send for MapRw {}

impl MapRw {
    /// Maps the first `len` bytes of `file` (opened read-write) for reading and writing.
    pub(crate) fn new(file: &File, len: usize) -> io::Result<MapRw> {
        let ptr = map(file, len, libc::PROT_READ | libc::PROT_WRITE)?;
        Ok(MapRw { ptr, len })
    }

    /// The mapping's length.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    fn word(&self, offset: u64) -> Option<&AtomicU32> {
        let off = checked_word(self.len, offset)?;
        // SAFETY: `off` is 8-aligned (so 4-aligned, as AtomicU32 requires; the mapping itself is page-aligned) and
        // `[off, off + 4)` is inside the mapping, which is readable and writable for as long as `self` lives (the
        // returned reference borrows `self`). Other processes access commit words only atomically (ADR 0003), and this
        // module never creates a non-atomic reference that covers a commit word while other processes may load it.
        Some(unsafe { AtomicU32::from_ptr(self.ptr.as_ptr().add(off).cast::<u32>()) })
    }

    /// Loads the commit word at `offset` with `Acquire`. `None` if misaligned or out of bounds.
    pub(crate) fn commit_word(&self, offset: u64) -> Option<u32> {
        Some(self.word(offset)?.load(Ordering::Acquire))
    }

    /// Stores the commit word at `offset` with `Release`, publishing every byte written before it.
    pub(crate) fn store_commit_word(&self, offset: u64, value: u32) -> Option<()> {
        self.word(offset)?.store(value, Ordering::Release);
        Some(())
    }

    /// Mutable access to `[offset, offset + len)`.
    ///
    /// The writer only calls this for bytes that no reader reads: the body of a record before its commit word is
    /// stored, a seal's CRC before its marker, or the uncommitted tail during recovery (never a commit word that a
    /// reader may be loading). Committed bytes are never modified (ADR 0003).
    pub(crate) fn bytes_mut(&mut self, offset: u64, len: u64) -> Option<&mut [u8]> {
        let end = checked_range(self.len, offset, len)?;
        let start = offset as usize;
        // SAFETY: the range is inside the mapping (checked); `&mut self` guarantees no other reference into the mapping
        // exists in this process for the returned lifetime; other processes never write the file (single writer, held
        // by the lock) and readers don't read these bytes until a later release store makes them visible.
        Some(unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(start), end - start) })
    }

    /// Shared access to `[offset, offset + len)`, for the writer's own validation and copying.
    ///
    /// Sound because only the writer (this process, holding the lock) ever writes the file, and `&self` excludes
    /// concurrent writes through [`MapRw::bytes_mut`] for the returned lifetime. Commit words inside the range are
    /// not being stored concurrently either, for the same reason.
    pub(crate) fn bytes(&self, offset: u64, len: u64) -> Option<&[u8]> {
        let end = checked_range(self.len, offset, len)?;
        let start = offset as usize;
        // SAFETY: as described above: in bounds, and no concurrent writer to these bytes exists while `&self` is held.
        Some(unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(start), end - start) })
    }

    /// `msync(MS_SYNC)` on the pages covering `[offset, offset + len)`.
    pub(crate) fn sync_range(&self, offset: u64, len: u64) -> io::Result<()> {
        msync(self.ptr, self.len, offset, len)
    }
}

impl Drop for MapRw {
    fn drop(&mut self) {
        // SAFETY: the mapping was created by `map` with this length, and every reference into it borrows `self`, so
        // none can outlive this drop.
        unsafe { unmap(self.ptr, self.len) }
    }
}

/// A zero-copy reader's read-only shared mapping of one segment file.
#[derive(Debug)]
pub(crate) struct MapRo {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: as for `MapRw`: the mapping is owned exclusively and only borrowed through `&self`.
unsafe impl Send for MapRo {}

impl MapRo {
    /// Maps the first `len` bytes of `file` (which may be opened read-only) for reading.
    pub(crate) fn new(file: &File, len: usize) -> io::Result<MapRo> {
        let ptr = map(file, len, libc::PROT_READ)?;
        Ok(MapRo { ptr, len })
    }

    /// Loads the commit word at `offset`: a `Relaxed` load, then an `Acquire` fence if it is non-zero, so that later
    /// reads of the body see everything written before the writer's release store. `None` if misaligned or out of
    /// bounds.
    pub(crate) fn commit_word(&self, offset: u64) -> Option<u32> {
        let off = checked_word(self.len, offset)?;
        // SAFETY: aligned and in bounds (checked); the mapping lives as long as `self`. The reference is created by a
        // pointer cast, not `AtomicU32::from_ptr` (which requires writable memory), and is used only for a 4-byte
        // `Relaxed` load, which `std::sync::atomic` documents as permitted on read-only memory on x86-64 and aarch64.
        let word = unsafe { &*self.ptr.as_ptr().add(off).cast::<AtomicU32>() };
        let value = word.load(Ordering::Relaxed);
        if value != 0 {
            fence(Ordering::Acquire);
        }
        Some(value)
    }

    /// Copies `[offset, offset + len)` out of the mapping using `Relaxed` 8-byte atomic loads, without creating any
    /// reference into the mapping. Sound whatever other processes do to these bytes (at worst the copy is torn),
    /// which is why readers validate a record from this copy before [`MapRo::committed_bytes`] may be used on it.
    /// `None` if the 8-byte words covering the range are not all inside the mapping.
    pub(crate) fn copy_atomic(&self, offset: u64, len: u64) -> Option<Vec<u8>> {
        let start = offset & !7;
        let end = offset.checked_add(len)?;
        let word_end = end.checked_add(7)? & !7;
        checked_range(self.len, start, word_end - start)?;
        let mut out = Vec::with_capacity((word_end - start) as usize);
        let mut at = start as usize;
        while (at as u64) < word_end {
            // SAFETY: `at` is 8-aligned (the mapping is page-aligned) and `[at, at + 8)` is inside the mapping
            // (checked above), which lives as long as `self`. The reference comes from a pointer cast and is used
            // only for an 8-byte `Relaxed` load, which `std::sync::atomic` documents as permitted on read-only memory
            // on x86-64 and aarch64; atomic loads never form a data race.
            let word = unsafe { &*self.ptr.as_ptr().add(at).cast::<AtomicU64>() };
            out.extend_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
            at += 8;
        }
        let skip = (offset - start) as usize;
        out.drain(..skip);
        out.truncate(len as usize);
        Some(out)
    }

    /// Shared access to `[offset, offset + len)`. `None` if out of bounds.
    ///
    /// # Safety
    ///
    /// The bytes must belong to a committed record that has already been validated (length, CRC, sequence) from a
    /// [`MapRo::copy_atomic`] copy taken after observing its non-zero commit word, and no process may modify them
    /// while the returned slice lives. This is `Reader::open_mapped`'s safety contract; the tickertape writer never
    /// modifies committed bytes. Validating first means a corrupted length can't make the slice cover bytes the
    /// writer is still appending to (barring a CRC collision, about 2^-32 per corrupted record).
    pub(crate) unsafe fn committed_bytes(&self, offset: u64, len: u64) -> Option<&[u8]> {
        let end = checked_range(self.len, offset, len)?;
        let start = offset as usize;
        // SAFETY: in bounds (checked), readable for `self`'s lifetime, and immutable per the caller's contract.
        Some(unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(start), end - start) })
    }
}

impl Drop for MapRo {
    fn drop(&mut self) {
        // SAFETY: created by `map` with this length; every reference into it borrows `self`.
        unsafe { unmap(self.ptr, self.len) }
    }
}

/// Tries to take an exclusive `flock` without blocking. `Ok(false)` if another process holds it.
pub(crate) fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    // SAFETY: flock on a valid descriptor has no memory-safety preconditions.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Reserves disk blocks for the first `len` bytes of `file` and sets its size to `len`, so that later writes into
/// that range don't need new blocks (modulo copy-on-write filesystems; see the crash contract).
pub(crate) fn preallocate(file: &File, len: u64) -> io::Result<()> {
    let len_off = libc::off_t::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "segment too large"))?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: fallocate on a valid descriptor has no memory-safety preconditions.
        let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len_off) };
        if rc != 0 {
            return Err(last_os_error());
        }
        Ok(())
    }
    #[cfg(target_vendor = "apple")]
    {
        let mut store = libc::fstore_t {
            fst_flags: libc::F_ALLOCATECONTIG,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: len_off,
            fst_bytesalloc: 0,
        };
        // SAFETY: F_PREALLOCATE takes a pointer to an `fstore_t`, which lives on our stack for the whole call.
        let mut rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) };
        if rc == -1 {
            store.fst_flags = libc::F_ALLOCATEALL;
            // SAFETY: as above.
            rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) };
        }
        if rc == -1 {
            return Err(last_os_error());
        }
        file.set_len(len)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        let _ = len_off;
        file.set_len(len)
    }
}

/// Flushes a file's data (and the metadata needed to read it) to the device: `fdatasync` on Linux, `fsync`
/// elsewhere. On macOS this does not flush the drive's cache; see [`full_barrier`].
pub(crate) fn sync_data(file: &File) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: fdatasync on a valid descriptor has no memory-safety preconditions.
    let rc = unsafe { libc::fdatasync(file.as_raw_fd()) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: fsync on a valid descriptor has no memory-safety preconditions.
    let rc = unsafe { libc::fsync(file.as_raw_fd()) };
    if rc != 0 {
        Err(last_os_error())
    } else {
        Ok(())
    }
}

/// `fsync` (used for directories, whose entries must be persisted).
pub(crate) fn sync_all(file: &File) -> io::Result<()> {
    // SAFETY: fsync on a valid descriptor has no memory-safety preconditions.
    let rc = unsafe { libc::fsync(file.as_raw_fd()) };
    if rc != 0 {
        Err(last_os_error())
    } else {
        Ok(())
    }
}

/// The platform's strongest flush: on macOS `fcntl(F_FULLFSYNC)`, which asks the drive to flush its whole write
/// cache (failing with `ENOTSUP` where unsupported, which callers report rather than downgrade); elsewhere
/// [`sync_data`], because Linux's `fdatasync`/`fsync` already flush the device cache.
pub(crate) fn full_barrier(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: F_FULLFSYNC takes no argument and has no memory-safety preconditions.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
        if rc == -1 {
            Err(last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        sync_data(file)
    }
}

/// True if `err` means "no space" or "quota exceeded".
pub(crate) fn is_storage_full(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(code) if code == libc::ENOSPC || code == libc::EDQUOT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(len: u64) -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.path().join("f"))
            .unwrap();
        preallocate(&file, len).unwrap();
        (dir, file)
    }

    #[test]
    fn rw_and_ro_mappings_share_bytes() {
        let (_dir, file) = temp_file(65536);
        let mut rw = MapRw::new(&file, 65536).unwrap();
        rw.bytes_mut(68, 4).unwrap().copy_from_slice(b"abcd");
        rw.store_commit_word(64, 40).unwrap();
        let ro = MapRo::new(&file, 65536).unwrap();
        assert_eq!(ro.commit_word(64), Some(40));
        assert_eq!(ro.copy_atomic(68, 4).unwrap(), b"abcd");
        assert_eq!(ro.copy_atomic(65, 7).unwrap()[3..], *b"abcd");
        assert!(ro.copy_atomic(65530, 8).is_none());
        // SAFETY: the bytes were written before the observed commit word and nothing modifies them afterwards.
        assert_eq!(unsafe { ro.committed_bytes(68, 4) }.unwrap(), b"abcd");
        assert_eq!(rw.commit_word(64), Some(40));
        rw.sync_range(64, 8).unwrap();
        sync_data(&file).unwrap();
    }

    #[test]
    fn bounds_and_alignment_are_checked() {
        let (_dir, file) = temp_file(65536);
        let mut rw = MapRw::new(&file, 65536).unwrap();
        assert!(
            rw.commit_word(65532).is_none(),
            "4-aligned but not 8-aligned"
        );
        assert!(rw.commit_word(65536).is_none());
        assert!(rw.commit_word(65528).is_some());
        assert!(rw.bytes_mut(65530, 7).is_none());
        assert!(rw.bytes(u64::MAX, 2).is_none());
        let ro = MapRo::new(&file, 65536).unwrap();
        assert!(ro.commit_word(4).is_none());
        // SAFETY: out-of-bounds requests return None without touching memory.
        assert!(unsafe { ro.committed_bytes(65535, 2) }.is_none());
    }

    #[test]
    fn lock_is_exclusive_across_handles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let a = File::create(&path).unwrap();
        let b = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert!(try_lock_exclusive(&a).unwrap());
        assert!(
            !try_lock_exclusive(&b).unwrap(),
            "flock locks are per open file description"
        );
        drop(a);
        assert!(try_lock_exclusive(&b).unwrap());
    }

    #[test]
    fn preallocate_sets_size_and_zeroes() {
        let (_dir, mut file) = temp_file(131072);
        assert_eq!(file.metadata().unwrap().len(), 131072);
        file.write_all(b"x").unwrap();
        let ro = MapRo::new(&file, 131072).unwrap();
        assert_eq!(ro.commit_word(131064), Some(0));
        sync_all(&file).unwrap();
        full_barrier(&file).unwrap();
    }

    #[test]
    fn storage_full_detection() {
        assert!(is_storage_full(&io::Error::from_raw_os_error(libc::ENOSPC)));
        assert!(!is_storage_full(&io::Error::from_raw_os_error(libc::EIO)));
    }
}
