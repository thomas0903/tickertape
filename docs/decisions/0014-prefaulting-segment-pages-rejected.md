# 0014: Prefaulting segment pages (rejected)

- **Status:** rejected
- **Date:** 2026-11-01

## Context

At 1,024-byte payloads and 100,000 records/s, the writer falls behind its schedule and the tail of writer-to-reader
latency reaches milliseconds (`docs/benchmarks.md`). Each new segment is preallocated, but its pages are faulted in
on first write, so first-touch page faults were the obvious suspect.

## Options considered

1. **`madvise(MADV_WILLNEED)` on the writer's mapping of each new segment**, so the kernel brings the pages in ahead
   of the writer.
2. **Touch every page synchronously when a segment is created** (one write per page), moving all first-touch faults
   to the roll.
3. **`MAP_POPULATE` on Linux**: equivalent to option 2 on Linux.
4. **A background thread that faults in pages ahead of the write position.**
5. **Do nothing.**

## Decision

Option 5. An exploratory, uncommitted spike (on the owner's Mac, APFS; the diff is below so the experiment can be
repeated) measured options 1 and 2 with `tt-latency` at 1,024 B and 100,000 records/s, two runs each against two
baseline runs at the same commit. Both made the tail clearly **worse** in every run, and option 2 far worse: each
roll now stalls while the whole segment is faulted in. Option 3 is option 2 on Linux and wasn't pursued. Option 4
would add a second thread writing into the shared mapping (new `unsafe` concurrency) for a benefit the results give
no reason to expect. These spike numbers are not recorded benchmark results and are deliberately not quoted here.

## Consequences

- The writer stays single-threaded and maps segments without prefaulting.
- Large-payload, high-rate users should expect millisecond tails from the writer itself; the operations guide's
  advice (larger segments, headroom) stands. A future attempt should start by profiling where the writer stalls
  (page faults, writeback throttling or page zeroing) before choosing a remedy.

## The spike (for reproduction; never merged)

```diff
diff --git a/crates/tickertape/src/mmap.rs b/crates/tickertape/src/mmap.rs
index 03f84af..be6f3b9 100644
--- a/crates/tickertape/src/mmap.rs
+++ b/crates/tickertape/src/mmap.rs
@@ -118,6 +118,23 @@ impl MapRw {
     /// Maps the first `len` bytes of `file` (opened read-write) for reading and writing.
     pub(crate) fn new(file: &File, len: usize) -> io::Result<MapRw> {
         let ptr = map(file, len, libc::PROT_READ | libc::PROT_WRITE)?;
+        // SPIKE: experiment only, not for merging.
+        match std::env::var("TT_PREFAULT").as_deref() {
+            Ok("willneed") => {
+                // SAFETY: spike.
+                unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_WILLNEED) };
+            }
+            Ok("touch") => {
+                let page = page_size();
+                let mut at = 0;
+                while at < len {
+                    // SAFETY: spike; the segment is fresh (never-written zero bytes), writer-owned.
+                    unsafe { std::ptr::write_volatile(ptr.as_ptr().add(at + page - 1), 0) };
+                    at += page;
+                }
+            }
+            _ => {}
+        }
         Ok(MapRw { ptr, len })
     }
 
```
