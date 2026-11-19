# 0037: Java binding over the C ABI

- **Status:** proposed (the Maven coordinates need the owner's agreement; the public API becomes stable at v1.0)
- **Date:** 2026-11-20

## Context

ADR [0031](0031-bindings-and-install-channels.md) adds a Java binding to v1.0: "Java, on the Foreign Function &
Memory API (final since JDK 22) over the C ABI, with no JNI code". Chronicle Queue and Aeron, the tools tickertape is
measured against, are JVM tools, and their users expect low allocation, explicit lifecycles and no surprises on hot
paths. ADR 0031 also says what makes a binding delivered: the full record and replay surface with errors mapped to the
language's own, documented ownership and lifetimes, the shared cross-language fixtures, a runnable example, an install
test from the built package on every supported platform, and a version rule tied to the C ABI's.

The C ABI (ADR [0028](0028-c-abi.md)) leaves several things to bindings: use after close, double close and concurrent
calls on one handle are undefined behaviour; a reader waiting forever cannot be interrupted; error details are per
operating-system thread; an exception must never escape a fill callback. In Java each of these would crash the JVM,
which a Java library must not do. The Python binding's review (ADR 0029, open) found defects where an interrupted read
lost records, so this design starts from "an exception from a read means nothing was consumed".

ADR numbers 0026 to 0035 are taken on `main` and in open pull requests, and an ADR for the C++ binding is being
written at the same time, so this one takes 0037.

## Options considered

**Native access.**
1. **`java.lang.foreign` (FFM) only**, with downcall handles generated at run time from descriptors. No native code
   of our own, no build step per platform beyond the C library, and the JIT compiles downcalls well.
2. **JNI** or `sun.misc.Unsafe`: excluded by ADR 0031 and on their way out of the JDK.
3. **jextract-generated bindings**: a build-time tool that is not in the JDK, and its output is a low-level mirror of
   the header that would still need the whole idiomatic layer on top.

**Minimum JDK.**
1. **22**, the first with the final FFM API. JDK 22 to 24 are out of support, and users of supported JDKs are on 21
   (no final FFM) or 25.
2. **25**, the current long-term-support release. Same FFM API, plus `--illegal-native-access` (JEP 472) so tests can
   turn a missing native-access flag into an error, and virtual threads that no longer pin on monitors (JEP 491).

**Zero-copy append.** The C ABI's `tickertape_writer_append_with` calls a fill callback once with a buffer inside the
mapped segment; a reserve/commit pair does not exist (ADR 0028 explains why). So the choice is between exposing the
callback, through an FFM upcall, and offering only copying appends.

**Record views.** A view must not be readable after the reader moves on or is closed: for a mapped reader the memory
may be unmapped (a crash, not an exception). FFM can enforce lifetimes only through arenas. An arena per record
allocates. A generation counter checked on every access allocates nothing, but it cannot cover a `MemorySegment`
handed out, and it cannot stop an access that has already passed the check when another thread closes the reader (the
first review of this binding crashed the JVM that way). One shared arena per reader, closed before the reader is freed,
allocates nothing per record and does cover that case: closing a shared arena waits for every access in progress.

**Errors.** Checked (`IOException` subclasses, like `java.nio.file`) or unchecked (like Chronicle Queue and Aeron).

## Decision

**Technology and build.** FFM only: no JNI, no `Unsafe`, no native code of our own. Minimum **JDK 25** (option 2): the
FFM API is the same as in 22, so nothing is lost for users of supported JDKs, and the tests can run with
`--illegal-native-access=deny`. Maven project in `bindings/java/` (the repository already uses Maven in `compare/`),
module `io.github.thomas0903.tickertape`, one exported package, no runtime dependencies; test dependencies are pinned
to exact versions (JUnit 6.1.3). The compiler runs with `-Xlint:all -Werror` and doclint; the restricted FFM calls are
in eight classes, each marked `@SuppressWarnings("restricted")`.

**Coordinates.** `io.github.thomas0903:tickertape`. The owner has not registered a Maven Central namespace; an
`io.github.<user>` namespace needs no domain, only a GitHub verification. This must be confirmed before the first
release (it is set only in `bindings/java/pom.xml`). The module name ends in digits (from the user name), which javac
warns about; the warning is suppressed on the module declaration.

**Surface** (`docs/java-api.md` has the details):

- `Writer.open(dir, WriterOptions)`; `append` from `byte[]`, `MemorySegment` or `ByteBuffer`; `appendWith(schemaId,
  venueTsNs, localTsNs, length, PayloadFiller)` (zero-copy); `appendBatch(AppendBatch)`; `checkpoint()`; `nextSeq()`;
  `lastRetentionError()`; `close()`.
- `Reader.open(dir, ReaderOptions)` (the safe reader) and `Reader.openMappedUnsafe(dir, ReaderOptions)`, whose name
  carries the precondition and whose documentation starts with it; `tryNext()`, `next(timeout)`, `next()` (forever),
  `readBatch(ReadBatch, timeout)`, `tryReadBatch`, `seek`, `position`, `close()`.
- `WriterOptions`, `ReaderOptions` and `Retention` are immutable Java values (each setter returns a new value) that the
  binding turns into the C option builders inside `open` and frees at once, so no option handle has a Java lifetime.
  `ReaderOptions.waitSlice` is the binding's own option (see Interruption).
- `Journal.verify`, `listSegments`, `readCheckpoint`, `applyRetention` (with dry run) return value objects
  (`VerifyReport`, `SegmentInfo`, `Checkpoint`, `RetentionReport`; see Value types). `Tickertape` has the versions,
  `loadLibrary(Path)`, `librarySource()` and `realtimeNs()`.
- Unsigned C fields are Java `long` and `int` with the same bits (as Aeron and Agrona do); the docs point to
  `Long.toUnsignedString` and `Integer.toUnsignedLong`.

Every C function is either wrapped (the binding's descriptor table, `Signatures.FUNCTIONS`) or listed here as
deliberately not wrapped (`Signatures.NOT_WRAPPED`). `HeaderTest` parses the header and fails if a function is in
neither, if a descriptor differs from the header's parameter and return types, if a struct layout differs, if a status
code or corruption reason has no mapping, if a function listed below is missing from this ADR, or if the versions
differ. Not wrapped:

| Function | Why |
|---|---|
| `tickertape_reader_next_copy` | Java copies from the borrowed view instead (`RecordView.copyPayloadTo`, `payloadToArray`, `copy`): the same number of copies for the safe reader, which already reads into its own buffer, and no held-back-record state for the caller to reason about. A copy that fails (destination too small) loses nothing, because the view stays valid until the next call. |
| `tickertape_status_name` | `Status.cName()` gives the same names without a call; `HeaderTest` checks them against the header. |
| `tickertape_corrupt_reason_name` | `CorruptReason.cName()`, likewise. |

**Zero-copy append: the callback, through one upcall stub per writer.** `appendWith` takes a `PayloadFiller`, a
functional interface whose `fill(PayloadBuffer)` may throw. The writer creates its upcall stub on first use and reuses
it. The stub is bound to a small context object (the filler of the append in progress, what it threw, the payload
buffer) that never references the writer, and lives in a shared arena that the writer's handle closes right after the
C writer is freed, by `close()` or by the Cleaner. An upcall stub keeps its target reachable from a global root until
its arena closes, so a stub bound to the writer (the first version) kept every writer that had used `appendWith`
alive forever: never cleaned, its journal locked for the life of the process. The upcall target runs everything, the
payload buffer's clean-up included, inside a catch of `Throwable`, records what it caught inside another, and returns
non-zero; so nothing ever unwinds through native frames (an exception escaping an FFM upcall terminates the JVM).
After the call, `appendWith` throws `FillAbortedException` with the filler's exception as its cause (an `Error` is
rethrown as it is), or, if the abort's clean-up failed, the library's `JournalIoException` with the filler's exception
as its cause. The `PayloadBuffer` is one object per writer, valid only on the thread running the filler and only while
it runs: it records that thread when the fill begins, and every method throws `WrongThreadException` on any other
thread and `IllegalStateException` after the filler returned. It writes through checked offsets into the mapped bytes,
and `segment()` hands out a `MemorySegment` whose confined arena, created on the filler's thread, closes when the
filler returns. (Without the thread check, `segment()` called on a helper thread created the arena there, and the
upcall's clean-up then failed to close it, which ended the JVM.) A call into the writer from inside the filler is
refused by the binding's guard (see Threads), which aborts the append.

We measured the upcall informally (`UpcallCostTest`, run on request; not under the benchmark rules, so no numbers here):
it adds a fixed cost per append, of the same order as a small copying append. So `append` stays the default for small
records, and `appendWith` pays off when the payload would otherwise be built in a temporary buffer first (larger
records, or records encoded field by field). Both allocate nothing per record.

**Ownership and lifetimes.**
- `Writer` and `Reader` are `AutoCloseable`; `close()` is idempotent. Each holds its C handle in a small guard object
  (`NativeHandle`). A `Cleaner` frees a handle whose owner became unreachable without being closed: a safety net only,
  documented as such. Every call, `close()` included, ends with `Reference.reachabilityFence(this)` in a `finally`,
  so an owner cannot become unreachable while its call is still running. For `close()` this matters beyond the
  library call: without the fence the owner could become phantom-reachable once `close()` had loaded its handle, the
  Cleaner could claim the clean-up, and the explicit `cleanable.clean()` would return at once, before the handle was
  freed, so `close()` returned with the native close still running (an immediate reopen met the old lock). With it, an
  explicit close always runs the clean-up on its own thread. The C close's error is captured right after the call, on
  the thread that made it (the details are per OS thread), and published to `close()` through a volatile field.
- `next` and friends return a `RecordView`: the **same object every time** (no allocation per record), valid until the
  next call on the reader other than `position()`, or until close. Validity is a generation counter that every reader
  call advances, and `close()` too, from the closing thread, before it frees anything; every accessor checks it and
  throws `IllegalStateException` on a stale view. The counter is advanced with a release store and read with an
  acquire load (the first version used a plain field: a thread reading a view's metadata in a compiled loop kept
  getting values after `close()` had returned, because the JIT hoisted the load out of the loop, and so did a copy of
  an empty payload, which touches no memory and so never meets the arena check). An acquire load costs a plain `mov`
  on x86-64 and an `ldar` on AArch64, and cannot be hoisted. The read path pays for that load once per accessor, and
  for the guard's pair of compare-and-sets (see Threads) once per guarded call (`tryNext`, the `next` methods, the
  batch reads, `seek` and the other reader methods, and `withPayload`), not per accessor; `ReadPathCostTest`, run on
  request, measures the path informally (its figures are in the pull request, not here: they were not taken under the
  benchmark rules). Reuse cuts both ways and is documented: a saved view
  shows the next record after the next read, a view kept from a batch is re-pointed by a later `get` after a refill,
  and a saved `PayloadBuffer` is usable again during a later fill on the same thread.
- **Every view access goes through memory whose lifetime the JVM enforces.** Each reader has one shared arena and one
  segment over the whole address space in it (`MemorySegment.NULL.reinterpret(Long.MAX_VALUE, arena, null)`). Typed
  reads (`get(ValueLayout.OfLong, offset)` and so on), bulk copies and the metadata read through that segment at
  `address + offset`, after bounds and generation checks, so no segment object is created per record. `close()` waits
  until no call is in progress and moves the handle to `CLOSING` (every call is refused from then on), advances the
  generation, closes the arena, and only then calls `tickertape_reader_close`; the Cleaner closes the arena before the
  C close too. Closing a shared arena is a thread-local handshake with every thread: an access in progress when the
  reader is closed either finishes or throws `IllegalStateException`, every later access, on any thread, throws, and
  none reads freed memory. (Closing refuses only while a segment of the arena is passed to a downcall, which the
  binding never does; it retries anyway, without spinning on an interrupted thread.) So closing a reader from another
  thread while a consumer reads its view is safe, which the guide's shutdown pattern needs. The cost is on close, not
  on reads, but it is not small: the handshake stops every running Java thread briefly and grows with the number of
  threads, for every reader closed (and every writer that used `appendWith`, whose upcall stub is in a shared arena).
  We accept it because readers are meant to be
  long-lived (seek instead of reopening); the guide says so, for code that opens short-lived readers. Reads add a
  liveness check of the arena next to the bounds check, which the JIT can hoist out of loops.
- A view holds its owner (reader or batch) strongly, and every accessor ends with
  `Reference.reachabilityFence(this)`, so an owner cannot be collected or cleaned while one of its views is in use; a
  batch's view reads through the batch's own buffer segment, whose scope is checked. (The first version's batch views
  held only the generation counter: once the batch was unreachable, the collector freed the payload memory under a
  view that still reported itself valid.)
- A view never hands out a long-lived `MemorySegment`: `withPayload(action)` gives the action a read-only segment in a
  confined arena that closes when the action returns, and for a reader's view holds the reader's guard meanwhile, so
  the action cannot move the reader and `close()` waits for it. For a batch's view, the batch counts the actions in
  progress and refuses to be read into meanwhile (before it invalidates anything or the reader moves): the first
  version only checked the generation before the action, so an action could refill its own batch and see its segment
  show another record. The one misuse the binding cannot catch is another thread *advancing* the reader while this
  thread reads its view (the safe reader may reuse its native buffer, the mapped reader may unmap the memory inside the
  call); that breaks the threading rule below and is documented as undefined for both readers.
- `ReadBatch` and `AppendBatch` own their native memory (automatic arenas: the garbage collector frees it).
  `ReadBatch.get(i)` returns the batch's view of record `i`: **one view object per index**, created on the first
  `get` of that index and reused for it by every later read, so views of different records never alias and a warm
  batch allocates nothing. We preferred this to one reusable view per batch (where `get(1)` silently re-pointed the
  view that `get(0)` returned), to a fresh view per `get` (an allocation per record), and to per-index accessors on the
  batch (a second copy of the view API). A batch's views are valid until the batch is filled again. The copying API:
  `RecordView.copy()` (a `RecordCopy` record), `payloadToArray()`, and `copyPayloadTo` a `byte[]`, `ByteBuffer` or
  `MemorySegment`.
- **Allocation.** The guarantee is what `AllocationTest` measures, and no more: once warm, each of the operations
  below allocates nothing on the Java heap per call, under the conditions given. The test measures each operation on
  its own with the thread's allocated-bytes counter around 20,000 calls (a batch call counts once; the first version
  counted 16 loop iterations per batch call, which hid a small allocation per call), allowing one byte per call, and
  the build runs it a second time with `-XX:-DoEscapeAnalysis`. That second run is the guarantee: an object built per
  call and removed only by escape analysis passes on one JIT and fails on another, as `copyPayloadTo(ByteBuffer)` did
  on CI's macOS runner (the `MemorySegment.ofBuffer` wrapper and its scope object for a direct buffer, built on every
  call, which escape analysis removed on the local machine but not on the runner, with the same JDK build). The
  operations, each with the `AllocationTest` method that measures it:
  - `append` with a native `MemorySegment`, passed to the library as it is, with no condition on its size
    (`writerCallsAllocateNothing`, `theScratchConditionIsTheBuffersCapacity`).
  - `append` with a `byte[]`, a heap `MemorySegment`, or a direct, heap or read-only heap `ByteBuffer`, which are
    copied through the writer's scratch buffer, of a payload that fits the buffer's current capacity
    (`writerCallsAllocateNothing`). The capacity starts at 256 bytes (`SCRATCH_INITIAL_BYTES` in `Writer.java`) and,
    when a copied payload does not fit, grows to the larger of that payload and twice the capacity; a native segment is passed
    as it is and does not grow it. `theScratchConditionIsTheBuffersCapacity` measures both sides: after 64-byte arrays
    an array of the full starting capacity allocates nothing, and the first `byte[]` append larger than the capacity
    allocates even after larger native-segment appends. Two earlier wordings were inexact: "no larger than one
    appended before" was not true across kinds, and "no larger than the largest copied payload before" denied the
    headroom the buffer already has (a writer warmed with 64-byte arrays appends 128 bytes without allocating). One
    exception: after a
    heap segment larger than `Integer.MAX_VALUE - 8` bytes, the next `ByteBuffer` append replaces the scratch buffer
    with one of its own size.
  - `appendBatch`; `nextSeq()` (`writerCallsAllocateNothing`).
  - `appendWith` whose filler uses `set`, `setByte`, `put(byte[])`, `put(MemorySegment)` of a native or heap segment,
    `put(ByteBuffer)` of a heap buffer, and `put(ByteBuffer)` of a direct buffer or a read-only heap buffer that is
    the same buffer of its kind as in the previous `put` of that kind (the writer remembers one of each; a copy from a
    new one allocates a small object); alternating one read-only and one direct buffer (`writerCallsAllocateNothing`).
  - `AppendBatch.add` with a `byte[]`, a native or heap `MemorySegment`, or a direct, heap or read-only heap
    `ByteBuffer`, into a batch that does not grow (records and payload bytes within its capacity, which for a buffer
    without an accessible array, a direct or a read-only heap one, must be at most `Integer.MAX_VALUE - 8` payload
    bytes: above that, each such `add` builds a wrapper). A batch that grows allocates (`writerCallsAllocateNothing`).
  - For both readers: `tryNext`, `next()` and a timed `next` that find a record; the view's metadata accessors
    (`seq()`, `schemaId()`, `venueTsNs()`, `localTsNs()`, `payloadLength()`) and `isValid()`, each measured on its own
    (an earlier version measured the six together as one figure), `getByte`, `get(layout)`, and `copyPayloadTo` a `MemorySegment`, a `byte[]`, a heap `ByteBuffer`, or a
    direct `ByteBuffer` that is the same buffer as in the previous copy by that view
    (`readerCallsAndViewsAllocateNothing`).
  - `tryReadBatch` and a timed `readBatch` that find records; `ReadBatch.get` of an index used before; and on a
    batch's view, the same accessors and copies as on a reader's view (`readerCallsAndViewsAllocateNothing`, which
    measures each of these on its own; the previous version measured `tryReadBatch`, `get`, `getByte` and `seq`
    together as one figure, and its list named accessors it did not measure).

  The guide's list (`docs/java-api.md` &rarr; Allocation) is this list.

  Without escape analysis the test found the same pattern, an object built on every call that only escape analysis
  removes, in three places: that wrapper in `copyPayloadTo(ByteBuffer)`, the same wrapper in
  `PayloadBuffer.put(ByteBuffer)` and `AppendBatch.add(ByteBuffer)`, and the `MemorySegment`s an upcall creates for
  the fill callback's pointer arguments (on every `appendWith`). The fill callback now receives its pointers as
  64-bit integers (`Signatures.POINTER_AS_LONG`; a pointer and an `int64_t` travel in the same register on every
  supported LP64 ABI, and `HeaderTest` still checks them as pointers); `AppendBatch` copies a direct buffer through a
  `ByteBuffer` view of its own memory; and a view, or a writer, remembers a segment over the last direct buffer it
  used (`BufferSegment`; a writer remembers two buffers, the last direct one and the last read-only heap one, in two
  slots, so a later fill from the other kind does not release the first, and `close()` releases both), so repeated
  use of one buffer allocates nothing and a new buffer costs one small object, which the documentation of those
  overloads states. Remembering a buffer keeps it reachable, and with it whatever its memory keeps alive (an arena whose clean-up
  action refers to the writer, say), so the rule is that nothing the caller passed in may be reachable from a global
  root. A writer's upcall stub is one (the JVM roots its target until the stub's arena is closed), and its target
  reaches the payload buffer; so the writer keeps its cache (`BufferCache`) itself and lends it to the payload buffer
  only for the length of each `appendWith`, and `close()` drops it. A dropped writer and its cached buffer then form at
  most a cycle the collector frees (the first version kept the cache in the payload buffer, where it kept a dropped
  writer, and its journal's lock, for ever; `LifetimeTest` covers both this and a reader's view). One case is left,
  and it is the caller's own root, not the binding's: a remembered buffer whose memory belongs to an automatic arena
  (`Arena.ofAuto()`) with a clean-up action that refers to the writer. The JDK's cleaner holds that action until the
  arena is unreachable, so an unclosed writer that still remembers such a buffer is not collected; `close()` drops
  the cache (both remembered buffers) and ends it. With a shared or confined arena nothing outside holds the action,
  and an unclosed writer is collected (`LifetimeTest`, with a shared arena whose buffer refers back to the writer). A view's cache is reachable only from the view, whose reader or batch no global root
  reaches. CI pins Temurin `25.0.5`, which
  resolves to 25.0.5+8 on Linux and to the macOS-only respin 25.0.5.1+2 on macOS; the test no longer depends on which.

**Value types.** A C struct that carries `struct_size` is designed to grow: later versions append fields
(`tickertape_verify_report`, `tickertape_retention_report`). Its Java type is a **final class with accessor methods
and no public constructor** (`VerifyReport`, `RetentionReport`), with `equals`, `hashCode` and `toString` over every
value, so a new field becomes a new accessor without breaking source or binary compatibility. A record would not allow
that: a new component changes its canonical constructor and its deconstruction pattern. Structs without `struct_size`
are frozen from 1.0 (`tickertape_checkpoint`, `tickertape_segment_info`, `tickertape_record`), and their Java types
may be records (`Checkpoint`, `SegmentInfo`; `RecordCopy` is the binding's own copy of a record); a struct added later
follows the same rule. `Status` and `CorruptReason` may gain constants in minor releases, as the C ABI may add codes
and reasons; their documentation says so, and that a `switch` over them needs a `default` branch (an unknown value
maps to `UNKNOWN` until the binding knows it). `InspectionTest` checks the class and record split.

**Threads.** A writer or reader is used by one thread at a time and may be handed to another thread between calls
(the C rule, ADR 0028). The binding enforces this cheaply: every call enters the handle's guard with one
compare-and-set (`IDLE` to `BUSY`) and leaves with another (`BUSY` to `IDLE`; the first version left with a plain
release store, which could not tell that a close was waiting). A call that finds the handle busy (another thread
is inside a call, or a filler or payload action calls back into its own handle) or closed throws
`IllegalStateException` without reaching the library, so use after close, double close and concurrent calls are
exceptions, not undefined behaviour, and the C ABI's `TICKERTAPE_E_BUSY` is unreachable. `close()` may be called
from any thread. On an idle handle it moves it to `CLOSING`. On a busy one it moves it to `CLOSE_PENDING`, which
`enter()` refuses and `isClosed()` reports (so a thread calling in a loop cannot starve the close, nothing is consumed
after the close began, and `isClosed()` means what it says, "close has started"), and waits; a blocked read notices
within one wait slice and throws `IllegalStateException`, and the call in progress, when it leaves, finds its
compare-and-set refused and hands the handle straight to the closer (`CLOSE_PENDING` to `CLOSING`), with no moment at
which another call could enter. The closer then frees the handle exactly once (for a reader, after closing its views'
arena) and marks it `CLOSED`. A `close()` that finds the handle `CLOSE_PENDING` or `CLOSING` waits for `CLOSED`, so
every `close()` returns after the C handle is freed and a writer's journal can be reopened at once. Closers wait on the
guard's monitor, not with `parkNanos` (which returns at once on an interrupted thread: the first version spun
there); an interrupt does not end the wait, and the interrupt status is restored when `close()` returns. Closing
from inside the handle's own filler or payload action is refused. `docs/java-api.md` lists exactly which misuse throws and which is undefined. Separate handles, and `Journal`'s functions, are independent and can run on any threads. `fork()`
does not arise in Java.

**Interruption and blocking.** A blocking read never passes `TICKERTAPE_WAIT_FOREVER` to the library. It waits in
slices of at most `waitSlice` (default 10 ms) and, between slices that returned nothing, checks
`Thread.interrupted()` (throwing `InterruptedException`) and the close request. A slice that returns a record returns
it at once, even if an interrupt arrived during the slice; the interrupt status then stays set for the caller, and the
next blocking call throws. So an exception from a read means nothing was consumed, and an interrupt is noticed within
one slice of waiting (validation and file I/O after a seek are not interrupted, as in C). The slice costs about 100
returns to Java per second per idle blocked reader at 10 ms (each restarting the library's back-off), and up to one
slice to notice an interrupt or a close; it does not delay records, which wake a notified reader at once. A blocking call on an already
interrupted thread throws before doing anything; `tryNext` never waits and ignores the interrupt status. The
interruption test interrupts a follower every 0.2 ms while a writer appends 20,000 records and checks that every
record arrives exactly once and in order.

**Virtual threads.** A downcall pins a virtual thread to its carrier for its duration, so a blocking read pins the
carrier for the whole wait (slice after slice), and `open`, `checkpoint` and an append that rolls pin it while they
run. The guidance: follow live journals from platform threads (the usual choice for low-latency consumers anyway), or
poll with `tryNext()` and park between polls, which unmounts the virtual thread. Short calls (appends, `tryNext`) are
fine on virtual threads.

**Errors.** Unchecked: `TickertapeException extends RuntimeException`, like Chronicle Queue and Aeron, so appends and
reads work inside lambdas and streams; every method documents what it throws. Each error status has its own subclass
(`InvalidArgumentException`, `JournalIoException` with `osErrorCode()` and its subclass `StorageFullException`,
`JournalLockedException`, `CorruptJournalException` with `reason()`, `UnsupportedFormatVersionException`,
`JournalNotFoundException`, `SequenceUnavailableException`, `RecordTooLargeException`, `DurabilityLostException`,
`SequenceExhaustedException`, `DiscardedHistoryException`, `InvalidConfigException`, `BufferTooSmallException` with
`requiredBytes()`, `FillAbortedException`, `HandleBusyException`, `InternalErrorException`, and
`BeaconInUseException`, which nothing throws until the C ABI creates reader beacons); a code the binding does not
know (a newer library) is a plain `TickertapeException` with `Status.UNKNOWN` and the raw `code()`. `Status` and
`CorruptReason` are enums with the C values and names; where a reason is reported, its raw C value is kept too
(`CorruptJournalException.reasonCode()`, `VerifyReport.firstProblemReasonCode()`), so a reason added by a newer library
is not reduced to `UNKNOWN`. `BatchAppendException` reports a partial batch: `appended()`, `firstSeq()`, and the
failing record's mapped exception as its cause. `appended()` is an `int`, as `AppendBatch.size()` is: callers resume
from that index into the batch, which has at most `Integer.MAX_VALUE` records, and the C count never exceeds the
batch's size. Mistakes Java can see itself (null, negative
lengths, out-of-range offsets, closed or busy handles) are the usual `NullPointerException`,
`IllegalArgumentException`, `IndexOutOfBoundsException` and `IllegalStateException`, thrown before the library is
called.

The C ABI keeps error details per OS thread. The binding reads them (code, message, OS error, corruption reason) right
after the failing call, in the same sequence of downcalls, with no Java code in between that can block; a virtual
thread is unmounted only when it blocks or yields, so the sequence runs on one OS thread. As a check, the thread's last
error code must equal the status the call returned; if not, the exception says the details are unavailable instead of
reporting another call's. The exception object is built only after all four values are read.

**API conventions, settled for 1.0.** Boolean accessors are `isXxx()` throughout the package (`isClosed()`,
`isMapped()`, `isValid()`, `isIntact()`, and in the options `isDurableOnly()`, `isNotifyEnabled()`,
`isPrefaultEnabled()`, renamed from `notifyEnabled()` and `prefaultEnabled()`); option setters keep the bare name. Every
`PayloadBuffer` write (`setByte`, the `set(layout, ...)` family and `put`) returns the buffer, so writes chain; they
returned `void` and the buffer inconsistently, and changing a return type after 1.0 would break binary
compatibility. `ApiDetailsTest` checks both rules over every public class. `RecordCopy` shares its payload array: the
constructor stores the array it is given and `payload()` returns it, so a copy costs one array (`RecordView.copy()`
allocates it); the documentation says that writing into it changes `equals` and `hashCode`, which compare content. We
preferred this to defensive copies in the constructor and the accessor, which would double the cost of every copy
for a misuse that the documentation covers.

**Paths.** The C ABI passes path bytes to the operating system unchanged, so the binding must pass the bytes the JDK
itself uses for a `Path`. It reads them from `Path.toUri()`: for a path of the default file system on Linux and macOS,
the URI's raw path is the **absolute** path's bytes, percent-encoded byte by byte (unreserved ASCII left as it is, a
`/` appended when the path is an existing directory that does not already end in one). The binding requires the
default file system (`path.getFileSystem() == FileSystems.getDefault()`, `IllegalArgumentException` otherwise), takes
`toUri().getRawPath()`, requires a leading `/`, decodes `%XX` to that byte and every other character to its ASCII byte,
strips trailing slashes except for the root, and passes the result NUL-terminated. A character above U+007F, a
malformed escape or an encoded NUL is refused with `IllegalArgumentException` naming the path, since it would mean the
JDK's form is not the expected one; the binding does not guess.

Rounds 2 to 4 of the review built the bytes from `Path.toString()` instead, and each round left a hole, because the text
does not determine the bytes. The first version encoded the text as UTF-8 and so named a sibling for a name that was
not valid in the file-system encoding (the JDK shows it with U+FFFD). The next encoded with `sun.jnu.encoding` and
refused a path whose text did not round-trip, and normalized to form D under macOS's
`jdk.nio.path.useNormalizationFormD`, which sent the wrong bytes for a listed entry stored in form C. The last refused
any path for which `Path.of(p.toString()).equals(p)` failed, which also refused a valid path that carries a trailing
slash in its bytes (`Path.of(URI.create("file:///tmp/journal%2F"))`: `Path.of` of its text strips the slash) and
reported it as an invalid file-system encoding, and misreported a form-C name that also contained U+FFFD. Reading the
URI removes the guess: an undecodable name on Linux and a listed form-C name under the property are now passed by
their own bytes rather than refused, and the charset lookup, the property handling and the round-trip check are gone.

The cost is that the URI form is the default provider's behaviour on Linux and macOS, not part of the `Path.toUri`
specification, which leaves the exact form to the provider. `PathsTest` pins it, asserting the exact bytes for an
ASCII absolute path, a relative path, an existing directory and a missing path, the `file:///tmp/journal%2F` path,
reserved URI characters, the root, an accented name made from a string under both normalization settings (the build
runs the class a second time with the property; the expected bytes are read back from the file system by the shell
after the JDK created the directory), a listed form-C entry on macOS, a name containing U+FFFD, the undecodable name on
Linux (the journal opened is the one with that name, not its decoded twin; skipped on macOS, whose file system refuses
the name), and a zip file system path refused. A JDK that changes the form fails those tests, or is refused at run
time, rather than passing other bytes silently. The supported range is unchanged: JDK 25 and later. Three consequences
are documented in the Javadoc (`Tickertape`, Paths, linked from every method that takes a journal path) and the guide:
the library receives the absolute path (a relative one resolved against the JVM's working directory, so paths in error
messages are absolute); `toUri()` may `stat` the path once per conversion (when a journal is opened or inspected, or a
retention destination set, never per record); and only the default file system is supported.

**Finding the library.** In this order, and the first library found must be compatible or loading fails (it never falls
through to a later candidate after a failure): `Tickertape.loadLibrary(path)` before first use; the system property
`tickertape.library.path`; a **natives JAR** for this platform on the class path; each directory of
`java.library.path`; the operating system's own search (`dlopen` of `libtickertape.so` or `.dylib`). The natives JAR
(classifier `natives-<os>-<arch>`: `linux-x86_64`, `linux-aarch64` and `macos-aarch64`, which CI tests, and
`macos-x86_64`, which the loader and the scripts know but no CI runner tests) carries the
library under `META-INF/tickertape/native/<platform>/` with its SHA-256 in a `.sha256` file; `META-INF` is never a
package, so the resource is visible from the class path and the module path alike. The binding extracts it into a new
directory that `Files.createTempDirectory` names randomly, with owner-only permissions (under `java.io.tmpdir`, or
`tickertape.library.extractDir`), checks the extracted file's SHA-256 against the JAR's, loads it, and deletes the file
and the directory at once (the loaded mapping stays valid). A missing or wrong hash refuses the library; a library
whose hash matched but which fails to load (a `noexec` temporary directory) gets an error that names
`tickertape.library.extractDir`; one that loads but fails the version or symbol check does not (the hint would
mislead). The file and the directory are deleted whatever happened; a failure to delete is added to the exception on
its way out as a suppressed exception, never in its place, and after a successful load it is only logged
(`System.Logger`). `Tickertape.loadLibrary(path)` records the request and loads under one lock, so two first calls with
different paths cannot cross over. The loader refuses a platform whose pointers or `size_t` (from the linker's
canonical layouts) are not 8 bytes. On the module path the natives JAR is the automatic module
`io.github.thomas0903.tickertape.natives`, which nothing requires, so it must be resolved explicitly
(`--add-modules`, or `requires` in the application's module), as the install test does. The natives
JAR is preferred over `java.library.path` because it is built for exactly this binding version, while the default
library path may hold an unrelated installed copy; an explicit path or property beats both, which is how a user selects
a specific build. The natives JARs are
built locally and in CI (`mvn -Pnatives package` after `scripts/stage-natives.sh`); publishing them is not part of this
decision's implementation. The Linux natives are for glibc; musl comes later with ADR 0031's musl builds.

**Versioning.** The binding's version is the version of the C library it is written against, and the binding is
released with the library, from the same tag. At load it calls `tickertape_abi_version()` and
`tickertape_version_number()` and applies ADR 0028's rule: the ABI version must equal the one in the header the binding
was written for, and the library must be exactly that version while the ABI version is 0 (before 1.0) and at least
that version from 1.0. An incompatible library is refused with an `UnsatisfiedLinkError` that names both versions and
the rule. `HeaderTest` fails if the binding's version constants differ from the header's or the `pom.xml` version
differs from them. From 1.0, the binding's public API follows semantic versioning on its own terms as well: a binding
1.x works with any 1.y library, y >= x; a binding that needed an incompatible API change would be 2.0, which the C ABI
rule does not force.

## Consequences

- JVM users get a binding with no native code of their own to build: the C library from Cargo (or, later, the natives
  JAR) plus one JAR with no dependencies.
- Tests (`bindings/java/src/test`, JUnit, on the module path with `--illegal-native-access=deny`): the whole surface;
  every status and corruption reason against the header; errors produced through the library (missing directory with
  `ENOENT`, empty directory, lock, configuration, record too large, partial batch, corruption with its reason and an
  unmoved cursor, retention and a sequence it removed, retention errors); lifetimes (use after close, views after the
  next call, payload segments after their scope, batches refilled and their views not aliasing, views keeping a
  collected batch's memory and a forgotten reader alive, the Cleaner releasing a forgotten writer with and without
  `appendWith`, a closed writer becoming unreachable); the close race (a consumer reading a view while another thread
  closes the reader, 200 times for each reader kind, after compiling the consumer's pass, and a consumer spinning on
  metadata, empty payloads or `isValid()`); a pending close (new calls refused and `isClosed()` true while it waits,
  the hand-over leaving no gap, a calling loop not starving it, an interrupted closer not spinning, a second closer
  waiting, reopening at once under collector pressure); a batch refused while its view is in `withPayload`;
  path conversion (the exact bytes passed for each kind of path, including undecodable names on Linux and form-C
  names on macOS); the API conventions above; the payload buffer refusing other threads;
  interruption (latency, no lost or duplicated records under constant interrupts, already-interrupted threads, close
  from another thread, virtual threads); threading (concurrent use refused, handles moved through an executor,
  readers on several threads, inspection beside a writer, two threads closing a writer together and reopening it);
  allocation; library loading in fresh JVMs (natives JAR
  extraction and clean-up, wrong or missing hash, `java.library.path`, nothing found, a file that is not the library,
  the module path with and without the native-access flag); the example; and the cross-language fixtures: a journal
  written from Java (every append form) reads back identically through the C ABI test program, the Rust CLI's `verify`
  and `export` and the Python decoder, a journal written through C reads back in Java, the C reader follows a Java
  writer live, and every released-format fixture in `tests/compat/` reads back in Java with the manifest's stream
  digests (safe and mapped readers, visible and durable-only) and is recovered and continued by a Java writer.
- CI: a new job `java binding (<os>)` on Linux x86-64, Linux arm64 and macOS builds the library, the CLI and the C
  programs, resolves every Maven plugin and dependency up front (`dependency:go-offline`, retried with `-U` because
  Maven Central once answered "not found" for an existing plugin; the tests themselves are never retried), runs the
  tests with JDK 25 (Temurin, pinned), builds the natives JAR, and runs the example from the two
  built JARs only, on the class path and the module path, failing on any warning (the install test ADR 0031 asks for).
- New docs: `docs/java-api.md`, `bindings/java/README.md`, the example `bindings/java/examples/Example.java`.
- Each release must keep the binding's version equal to the library's (the test fails otherwise), and a new C function
  needs a decision here before CI passes.
- Not covered: publishing to Maven Central and the release pipeline for the natives JARs (they need the owner's
  namespace, a signing key and a token; ADR 0031), musl Linux, Windows, JDKs before 25.
