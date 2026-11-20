# Java API

tickertape has a Java binding for JDK 25 and newer, in [`bindings/java/`](../bindings/java/README.md). It calls the C
ABI ([`c-api.md`](c-api.md)) through the Foreign Function and Memory API (`java.lang.foreign`), with no JNI and no
native code of its own, and has no runtime dependencies. The Javadoc documents every class; this page explains the
rules they share. The design and its reasons are in [ADR 0037](decisions/0037-java-binding.md).

> **Status:** not stable before tickertape 1.0, like the C ABI it is built on. The binding requires exactly the
> library version it was written for until then (see [Versioning](#versioning)). It is not published to Maven Central
> yet: build it from source.

Example: [`bindings/java/examples/Example.java`](../bindings/java/examples/Example.java) records, follows live and
replays a journal; it runs from source with the `java` launcher.

```java
import io.github.thomas0903.tickertape.*;
import java.lang.foreign.ValueLayout;
import java.nio.file.Path;
import java.util.concurrent.TimeUnit;

public class Hello {
    public static void main(String[] args) throws InterruptedException {
        Path dir = Path.of("/tmp/journal");
        try (Writer writer = Writer.open(dir)) {
            long seq = writer.append(1, 0, Tickertape.realtimeNs(), "hello".getBytes());
            writer.checkpoint(); // durable, not only visible

            try (Reader reader = Reader.open(dir)) {
                reader.seek(seq);
                RecordView r = reader.next(1, TimeUnit.SECONDS); // null if nothing arrives in time
                // The view is borrowed: valid until the next call on the reader. Copy what you keep.
                System.out.println(r.seq() + " " + new String(r.payloadToArray()));
            }
        }
    }
}
```

## Building and running

The binding needs `libtickertape`, built from source with Cargo (Rust 1.85 or newer), and a JDK 25 with Maven:

```bash
cargo build --release -p tickertape-ffi                    # target/release/libtickertape.{so,dylib}
mvn -f bindings/java/pom.xml package -DskipTests           # bindings/java/target/tickertape-0.4.0.jar
```

Run with the JAR on the class path, native access enabled, and the library's location:

```bash
java --enable-native-access=ALL-UNNAMED -cp bindings/java/target/tickertape-0.4.0.jar:app.jar \
     -Dtickertape.library.path=target/release/libtickertape.so com.example.App
```

On the module path the module is `io.github.thomas0903.tickertape` and the flag is
`--enable-native-access=io.github.thomas0903.tickertape`. The natives JAR (below) is the automatic module
`io.github.thomas0903.tickertape.natives`; nothing requires it, so the module system leaves it out unless it is
resolved: add `--add-modules io.github.thomas0903.tickertape.natives` (or `requires io.github.thomas0903.tickertape.natives`
in the application's module), as `bindings/java/scripts/install-test.sh` does. Without the flag, JDK 25 prints a warning on first use
(and later JDKs refuse); `--illegal-native-access=deny` turns a missing flag into an error, which is how the tests run.

**Finding the library.** The binding loads `libtickertape` on first use, in this order:

1. `Tickertape.loadLibrary(path)`, called before any other use;
2. the system property `tickertape.library.path` (a file);
3. a natives JAR for this platform on the class path (see below);
4. each directory of `java.library.path`;
5. the operating system's own search (`LD_LIBRARY_PATH`, the loader cache, `DYLD_LIBRARY_PATH`).

The first library found must be compatible or loading fails with an `UnsatisfiedLinkError` that says what was tried
and why; the binding never falls back to a later candidate after a failure. `Tickertape.librarySource()` tells you
where the library came from. So a natives JAR on the class path wins over `-Djava.library.path`, and
`-Dtickertape.library.path=<file>` (or `Tickertape.loadLibrary`) selects one specific build over both, for example a
library you built yourself.

**The natives JAR.** `tickertape-<version>-natives-<platform>.jar` carries the library and its SHA-256. CI builds and
tests it for `linux-x86_64`, `linux-aarch64` and `macos-aarch64`; the loader also recognises `macos-x86_64`, and the
scripts can stage it, but no CI runner tests that platform. The binding extracts it into a new directory
with a random name that only the current user can read (under `java.io.tmpdir`, or `-Dtickertape.library.extractDir`
if the temporary directory does not allow executable files), checks the hash, loads it, and deletes the file and the
directory at once. If the extracted library fails to load although its hash matched (typically a `noexec` temporary
directory), the error names `-Dtickertape.library.extractDir`; a library that loads but is of another version, or
lacks a function, gets no such hint. Build it with:

```bash
bindings/java/scripts/stage-natives.sh target/release linux-x86_64
mvn -f bindings/java/pom.xml -Pnatives package -DskipTests
```

The Linux libraries are built for glibc.

## Conventions

- **Unsigned values.** Sequence numbers, timestamps and sizes are unsigned 64-bit in the format, and schema ids
  unsigned 32-bit. Java has `long` and `int` with the same bits: use `Long.toUnsignedString`,
  `Long.compareUnsigned` and `Integer.toUnsignedLong` where the top bit can be set (timestamps can be up to
  2^64 - 1).
- **Options** (`WriterOptions`, `ReaderOptions`, `Retention`) are immutable values: each setter returns a new value.
  They are copied into the library when a writer or reader is opened.
- **Paths** must be on the default file system; a path of any other (a zip file system, say) throws
  `IllegalArgumentException`. The library receives the bytes the JDK itself uses for the path, so a name in any
  encoding or Unicode normalization form, including one that is not valid in the file-system encoding (on Linux, bytes
  that are not UTF-8 under a UTF-8 locale) and an entry stored in form C that a directory listing returned under
  macOS's `-Djdk.nio.path.useNormalizationFormD`, reaches the operating system as the JDK holds it. The binding reads
  those bytes from `Path.toUri()`, whose `file:` URI carries the absolute path's bytes percent-encoded one by one, and
  decodes them; it does not reconstruct them from `toString()`. What follows from that:
  - an absolute path is passed as its bytes. A relative path is passed the way the JDK passes it to the operating
    system for its own file operations. The JDK decides that once, at start-up: relative when its default directory
    (`user.dir`) has the bytes of the process's working directory, resolved against the default directory otherwise
    (`-Duser.dir=` naming another directory). It does not expose the decision, so the binding infers it the first time
    it converts a relative path: **relative** if the working directory, read with the C library's `getcwd` through the
    FFM linker, has the default directory's bytes, or if the JDK's own `stat` of `Path.of("")` reaches the working
    directory while the default directory's path does not (the working directory was renamed after start-up); as its
    **absolute** bytes otherwise, or if the inference fails (native access refused, a `stat` failing), in which case
    that conversion passes the absolute bytes and the next one tries again. A relative path passed relative keeps its
    own bytes (the empty path as `.`): the binding strips from the absolute bytes those of the default directory and the
    separator after them, and passes the absolute bytes if that prefix does not match, so a journal reached by a short
    relative path from a deep working directory opens even when its absolute path would exceed the platform's
    path-length limit, as the JDK opens it. Passed absolute, it reaches the journal the JDK's own `Files` methods see,
    and the path-length limit applies to that absolute form, as it does to the JDK's own file operations then. A process
    that changes its working directory through native code (`chdir`) is outside what the binding follows, as it is for
    the JDK. Known limit: with `-Duser.dir=` naming a directory other than the process's working directory, directories
    renamed (or a symbolic link replaced) around the binding's first conversion of a relative path can leave the
    binding and the JDK resolving relative paths against different directories; pass absolute journal paths when
    `-Duser.dir` is used. Trailing slashes are removed (except from the root);
  - `toUri()` may `stat` the path once (to tell whether it is a directory), and for a relative path the working
    directory once more (until the inference has succeeded, also a `getcwd` and up to three more `stat`s), which
    happens when a journal is opened or inspected, or a retention destination is set (`Retention.moveTo`), never per
    record.

  The exact URI form is the default provider's behaviour on Linux and macOS, not something the `Path.toUri`
  specification fixes; the binding's tests pin it, and a URI not in that form is refused with
  `IllegalArgumentException` rather than guessed at. The Javadoc of `Tickertape` (Paths) says the same.
- **Names.** Boolean accessors are `isXxx()` everywhere (`isClosed()`, `isMapped()`, `isValid()`, `isIntact()`, and
  in the options `isDurableOnly()`, `isNotifyEnabled()`, `isPrefaultEnabled()`); option setters are the bare name
  (`notify(boolean)`). Every `PayloadBuffer` write (`setByte`, `set`, `put`) returns the buffer, so writes chain.
- **Timeouts** are a `long` with a `TimeUnit`, or a `Duration`. Zero or negative does not wait. A timeout bounds only
  the wait for a record: validating the records skipped after a seek, and file I/O, are not interrupted.
- **Errors** are unchecked exceptions (see [Errors](#errors)).

## Ownership and lifetimes

| Object | Create | Release |
|---|---|---|
| `Writer` | `Writer.open` | `close()` |
| `Reader` | `Reader.open`, `Reader.openMappedUnsafe` | `close()` |
| `RecordView` | returned by `tryNext`, `next`, `ReadBatch.get` | nothing to release; valid until the next call (reader) or read (batch) |
| `AppendBatch`, `ReadBatch` | constructors | garbage collected (they own only memory) |

- Writers and readers are `AutoCloseable`; use try-with-resources. `close()` is idempotent. A writer or reader that
  becomes unreachable without being closed is closed by a `Cleaner` eventually; that is a safety net, not a way to
  manage them (a forgotten writer keeps the journal locked until the garbage collector runs).
- **A `RecordView` is borrowed.** The reader returns the same view object every time, so reading allocates nothing.
  It is valid until the next call on that reader (any method except `position()` and `isClosed()`), or until the
  reader is closed. Every accessor checks this and throws `IllegalStateException` on a view that is no longer valid.
  Copy what you need to keep: `copy()` (a `RecordCopy`), `payloadToArray()`, or `copyPayloadTo(byte[] | ByteBuffer |
  MemorySegment)`.
- **Reused objects come back to life.** Because objects are reused, a reference you kept can become valid again,
  showing something else: a saved `RecordView` of a reader shows the new record after the next successful read (it is
  the same object); a view kept from a batch is re-pointed at the new record of its index by a later `get` after a
  refill; and a saved `PayloadBuffer` is usable again on the filling thread during a later fill of the same writer
  (it is one object per writer). Never keep these past their validity expecting them to stay stale.
- **Views read only memory the JVM keeps alive.** A reader's view reads the payload through a segment in the reader's
  shared arena, which `close()` closes before it frees the reader. An access in progress when the reader is closed
  either finishes or throws `IllegalStateException`, and every later access throws; none reads freed memory. The
  accessors that touch no payload memory (the metadata, `payloadLength()`, a copy of an empty payload, `isValid()`)
  see the close through the view's generation counter, which `close()` advances before it frees anything and which
  every accessor reads with an acquire load, so a view read in a loop on another thread stops at its next accessor. So
  `close()` from another thread is safe while a consumer reads the current view (`CloseRaceTest` races the two, for
  both readers, with payloads, metadata and empty payloads). A view also holds its reader or batch strongly, so neither
  is garbage collected, or cleaned, while one of its views is still in use.
- **Reading a payload in place.** Typed reads take a layout, as `MemorySegment` does:
  `view.get(ValueLayout.JAVA_LONG_UNALIGNED, 8)`. Payloads have no alignment guarantee, so use the `_UNALIGNED`
  layouts (in native byte order, or with `withOrder`). For code that needs a `MemorySegment`,
  `view.withPayload(segment -> ...)` passes a read-only segment, confined to the calling thread, that is closed when
  the action returns, so it cannot outlive the record. Meanwhile the reader refuses other calls, so the action must
  not use the reader, and a `close()` from another thread waits for the action to return. For a batch's view, reading
  into that batch while the action runs throws `IllegalStateException` (it would change the payload under the action);
  reading into another batch is fine.
- **Batches.** A `ReadBatch` holds the records of one `readBatch` call in its own memory. `get(i)` returns the batch's
  view of record `i`: one view object per index, so `get(0)` and `get(1)` can be used side by side and never alias.
  The first `get` of an index creates its view; every later read reuses it. All views are valid until the batch is
  filled again (then they throw until `get` positions them again), and they stay readable after the reader is closed,
  since the memory is the batch's. An `AppendBatch` copies payloads in when they are added; an `add` whose copy
  throws (a closed segment, a segment confined to another thread, a direct buffer over a closed arena) leaves the
  batch unchanged. Reuse it with `clear()`.
- **A `RecordCopy` shares its array.** Its constructor stores the payload array it is given and `payload()` returns
  that same array, without copies (`RecordView.copy()` gives it a new array of its own). Writing into the array
  changes the record, including `equals` and `hashCode`, which compare the payload's content: do not change one that
  is in a set or a map key; clone the array to change a payload.
- **Allocation.** The guarantee is exactly what `AllocationTest` measures: once warm, each of these allocates nothing
  on the Java heap per call, under the conditions given (the test measures each operation on its own, a batch call
  counting once, and runs a second time with the JIT's escape analysis off, so nothing relies on the JIT removing a
  per-call object):
  - `append` with a native `MemorySegment`, which is passed to the library as it is (no condition on its size).
  - `append` with a `byte[]`, a heap `MemorySegment`, or a direct, heap or read-only heap `ByteBuffer`, which are
    copied through the writer's scratch buffer: of a payload that fits the buffer's current capacity, which starts at
    256 bytes (the constant `SCRATCH_INITIAL_BYTES` in `Writer.java`). When a copied payload does not fit, the new
    capacity is the larger of the payload and twice the old capacity, where the doubled figure is capped at
    `Integer.MAX_VALUE - 8` bytes: with a capacity of 1,200,000,000 bytes, a 1,300,000,000-byte payload grows it to
    `Integer.MAX_VALUE - 8` bytes, not 2,400,000,000. So a writer warmed with 64-byte arrays appends 256-byte arrays
    without allocating. A native segment is passed as it is and does not grow the buffer, so the first `byte[]` append
    larger than the capacity allocates even after larger native-segment appends. One exception: a copied payload above
    the cap (a heap segment can be far above it; a `byte[]` or `ByteBuffer` only by the few bytes up to
    `Integer.MAX_VALUE`) grows the buffer to exactly its size, which is too large to back a `ByteBuffer`. By source:
    a `byte[]` or heap segment above the cap is copied into that buffer segment to segment (`MemorySegment.copy`); a
    `ByteBuffer` above the cap (direct, heap or read-only heap, with `Integer.MAX_VALUE - 7` to `Integer.MAX_VALUE`
    bytes remaining) is copied into it through a segment over the buffer (`MemorySegment.ofBuffer`, a wrapper
    allocated on each such call), reusing a scratch buffer that is already large enough; a later `ByteBuffer` append
    at or below the cap replaces a scratch buffer that large with one of that append's size. In every case the record
    then reaches the library, which appends it or, when it does not fit in one segment, refuses it with its usual
    `RecordTooLargeException`.
  - `appendBatch`; `nextSeq()`.
  - `appendWith` whose filler uses `set`, `setByte`, `put(byte[])`, `put(MemorySegment)` (native or heap), or
    `put(ByteBuffer)` of a heap buffer, or of a direct or read-only heap buffer that is the same as in the previous
    `put` of its kind; alternating one direct and one read-only heap buffer is fine. The first copy from another
    buffer of a kind allocates a small object. The writer remembers two buffers, the last direct one and the last
    read-only heap one, so a later fill from the other kind does not release the first; it keeps each reachable until
    another of its kind replaces it, and `close()` releases both. Close a writer you are done with: the `Cleaner` is
    a backstop, and it cannot free an unclosed writer that remembers a buffer whose memory belongs to an automatic
    arena (`Arena.ofAuto()`) with a clean-up action that refers back to the writer, because the JDK's cleaner holds
    that action; with a shared or confined arena the unclosed writer is collected.
  - `AppendBatch.add` with a `byte[]`, a native or heap `MemorySegment`, or a direct, heap or read-only heap
    `ByteBuffer`, while the batch does not grow; a batch that grows allocates, so size it up front or let it grow
    once and reuse it. For a buffer without an accessible array (a direct or a read-only heap buffer) this holds only
    up to a payload capacity of `Integer.MAX_VALUE - 8` bytes (above it, each such `add` builds a wrapper).
  - For both readers: `tryNext`, `next()` and a timed `next` that find a record; the view's metadata accessors
    (`seq()`, `schemaId()`, `venueTsNs()`, `localTsNs()`, `payloadLength()`) and `isValid()`, each measured on its own;
    `getByte`, `get(layout)`, and `copyPayloadTo` a `MemorySegment`, a `byte[]`, a heap `ByteBuffer`, or
    the same direct `ByteBuffer` as that view's previous copy (the view remembers it, and keeps it reachable while the
    view is); the first copy into another direct buffer allocates a small object.
  - `tryReadBatch` and a timed `readBatch` that find records; `ReadBatch.get` of an index used before; and on a
    batch's view, the same accessors and copies as on a reader's view (above).

  Everything else may allocate; in particular `withPayload`, `PayloadBuffer.segment()`, and the copying methods that
  return new objects (`copy()`, `payloadToArray()`) do.

## Writing

```java
WriterOptions options = WriterOptions.defaults()
        .segmentSize(64L << 20)
        .retention(Retention.delete().maxSegments(16));
try (Writer writer = Writer.open(Path.of("/data/journal"), options)) {
    long seq = writer.append(QUOTE, venueTs, Tickertape.realtimeNs(), payload);          // byte[]
    writer.append(QUOTE, venueTs, Tickertape.realtimeNs(), segment);                      // MemorySegment
    writer.append(QUOTE, venueTs, Tickertape.realtimeNs(), byteBuffer);                   // ByteBuffer, position kept
    writer.appendWith(QUOTE, venueTs, Tickertape.realtimeNs(), 24, p -> p                 // zero-copy
            .set(ValueLayout.JAVA_LONG_UNALIGNED, 0, price)
            .set(ValueLayout.JAVA_LONG_UNALIGNED, 8, quantity)
            .set(ValueLayout.JAVA_LONG_UNALIGNED, 16, orderId));
    writer.checkpoint();
}
```

- `append` copies the payload into the journal. A native `MemorySegment` is passed to the library as it is; arrays,
  heap segments and buffers are first copied into the writer's scratch memory.
- `appendWith` avoids that copy: your `PayloadFiller` writes the payload straight into the journal's mapped segment
  through a `PayloadBuffer` (zeroed, exactly the requested length, valid only while the filler runs). It costs an
  upcall from the library into Java, so for small records the copying `append` is as fast or faster; it pays off when
  you would otherwise build the payload in a temporary buffer first. **Throwing from the filler aborts the append:**
  nothing is committed, the writer zeroes and syncs the reserved bytes (slow, so aborting is for exceptional cases),
  and `appendWith` throws `FillAbortedException` with your exception as its cause. If that sync fails, it throws a
  `JournalIoException` instead: a storage incident, after which the writer refuses appends and checkpoints (see
  [`c-api.md`](c-api.md#a-failed-clean-up-after-an-aborted-fill)). The filler must not call its writer. The
  `PayloadBuffer` belongs to the filler's thread: on any other thread each of its methods (and its `segment()`) throws
  `WrongThreadException`, and after the filler returns, `IllegalStateException`. Nothing a filler does, including
  handing the buffer to another thread, can make an exception escape the upcall.
- `appendBatch` appends an `AppendBatch` with one call into the library. Every record is checked first, so an argument
  error appends nothing; if an append fails part-way, the records before it stay appended and
  `BatchAppendException.appended()` says how many. Retry from that index; it is an `int`, like `AppendBatch.size()`,
  because it indexes the batch.
- `nextSeq()` is the sequence number the next append will get. `lastRetentionError()` returns the latest error of the
  writer's retention, which never fails appends or checkpoints.

## Reading

```java
try (Reader reader = Reader.open(Path.of("/data/journal"))) {   // at the oldest record
    reader.seek(1000);                                         // next record is seq 1000
    while (running) {
        RecordView r;
        try {
            r = reader.next(100, TimeUnit.MILLISECONDS);       // null: nothing new yet
        } catch (DiscardedHistoryException e) {                // recovery or retention replaced the segment
            reader.seek(reader.position());                    // SequenceUnavailableException if retention removed it
            continue;
        }
        if (r != null) {
            handle(r.seq(), r.get(ValueLayout.JAVA_LONG_UNALIGNED, 0));
        }
    }
}
```

- `Reader.open` is the **safe reader**: it reads the files with `pread` into its own buffer and validates the copy, so
  it is memory-safe whatever other processes do to the files.
- `Reader.openMappedUnsafe` is the **mapped reader**: payloads are read straight from a memory mapping, without a
  copy. **Precondition: while it exists, no process other than a tickertape writer may modify the journal's segment
  files.** A tickertape writer never changes committed bytes, so this holds in normal operation; if something else
  truncates a segment, the JVM receives `SIGBUS` and dies. The binding cannot check this, hence the name.
- `tryNext()` never waits; `next(timeout)` waits up to the timeout and returns `null` if nothing arrived; `next()`
  waits as long as it takes. `readBatch(batch, timeout)` waits for the first record only, then takes what is already
  committed.
- **A failed read consumed nothing.** After any exception from a reader, the next read returns the record that was
  next before. A batch read that hit an error after some records returns those records, and the next read throws the
  error. A record too large for a `ReadBatch`'s buffer throws `BufferTooSmallException` with the size it needs and
  stays next.
- `ReaderOptions.defaults().durableOnly(true)` gives a reader that returns only records covered by a checkpoint (see
  [Durability](#durability)).
- **Starting before the writer.** Opening a journal whose writer has not started throws `JournalIoException` with
  `osErrorCode()` 2 (`ENOENT`); while the writer creates the first segment, `JournalNotFoundException`. A loop that
  waits for the writer retries on exactly these two (the example does).

## Interruption and closing

Blocking reads (`next` with a timeout or without, `readBatch`) wait in slices of at most
`ReaderOptions.waitSlice(...)`, 10 ms by default. Between slices they check the thread's interrupt status and throw
`InterruptedException` (clearing it), and notice a `close()` from another thread (throwing `IllegalStateException`).
So an interrupt or a close ends a wait within one slice; from the moment `close()` starts, the reader refuses every
new call, so a thread reading in a loop cannot delay the close or consume a record after it began. A record that arrives during a slice is returned at once, even
if the thread was interrupted meanwhile: the interrupt status then stays set, and the next blocking call throws. A
blocking call on an already interrupted thread throws before doing anything. `tryNext()` and `tryReadBatch()` never
wait and ignore the interrupt status.

`close()` itself waits for a call in progress without spinning, and an interrupt does not end that wait: it returns
once the handle is freed, with the thread's interrupt status still set.

**Closing a reader stops every thread briefly.** Closing a reader ends every access to its views by closing a shared
arena, and the JDK does that with a handshake with every thread of the JVM: each running Java thread is stopped
briefly at a safepoint-like poll, and the cost grows with the number of threads. So does closing a writer that used
`appendWith` (its upcall stub lives in a shared arena too). Code that opens short-lived readers often, in a JVM with
many threads, pays this every time and briefly disturbs every other thread: keep a reader open and `seek` it instead
of reopening, or use one long-lived reader per consumer.

The slice has two costs. A reader blocked with nothing to read returns to Java once per slice, about 100 times a
second at 10 ms, and each new slice starts the library's back-off again (its spins and yields); and an interrupt or a
close can take up to one slice to be noticed. The slice does not delay records: with `notify` on (the default) a new
record wakes the waiting reader at once. A longer slice saves wake-ups and notices interrupts later; a shorter one the
reverse.

## Durability

The binding uses the crash contract's two levels ([`crash-contract.md`](crash-contract.md)):

| Level | Reached when | Means |
|---|---|---|
| **Visible** | an append returns | Readers on this host can read the record, and it survives the writer process dying. It can still be lost in a power loss or OS crash |
| **Durable** | a later `checkpoint()` returns | The record is on stable storage, under the crash contract's storage assumptions |

`close()` does not checkpoint. A failed checkpoint poisons the writer: later checkpoints throw
`DurabilityLostException` while appends keep working; treat it as an incident. A durable-only reader returns only
records below the checkpoint marker's `next_seq` (crash contract C13), and nothing without a valid marker; its records
survive a power loss under the crash contract's conditions, including that no checkpoint failed earlier in the same
boot.

## Errors

Every error from the library is a `TickertapeException` (unchecked), with one subclass per C status code:

| Exception | Status | Meaning |
|---|---|---|
| `InvalidArgumentException` | `INVALID_ARGUMENT` | The library rejected an argument (Java checks most arguments itself first) |
| `JournalIoException` | `IO` | A system call failed; `osErrorCode()` is its `errno` |
| `StorageFullException` (a `JournalIoException`) | `STORAGE_FULL` | `ENOSPC` or `EDQUOT`; a failed append can be retried later |
| `JournalLockedException` | `LOCKED` | Another writer has the journal open |
| `CorruptJournalException` | `CORRUPT` | Journal bytes failed validation; `reason()` says why. A reader's position does not move |
| `UnsupportedFormatVersionException` | `UNSUPPORTED_VERSION` | A segment has an unsupported format version |
| `JournalNotFoundException` | `NOT_FOUND` | The directory has no segments yet (a missing directory is `JournalIoException` with `ENOENT`) |
| `SequenceUnavailableException` | `SEQ_UNAVAILABLE` | The sequence number is older than the oldest segment |
| `RecordTooLargeException` | `RECORD_TOO_LARGE` | The payload does not fit in a segment |
| `DurabilityLostException` | `DURABILITY_LOST` | An earlier checkpoint, or the clean-up after an aborted fill, failed |
| `SequenceExhaustedException` | `SEQUENCE_EXHAUSTED` | Sequence numbers are exhausted |
| `DiscardedHistoryException` | `DISCARDED` | Recovery or retention replaced or removed the reader's segment; call `seek` |
| `InvalidConfigException` | `INVALID_CONFIG` | A configuration value is out of range |
| `BufferTooSmallException` | `BUFFER_TOO_SMALL` | A `ReadBatch` buffer is too small; `requiredBytes()` |
| `FillAbortedException` | `ABORTED` | A `PayloadFiller` threw; nothing was appended |
| `HandleBusyException` | `BUSY` | Not reachable through the binding (its guard throws `IllegalStateException` first) |
| `InternalErrorException` | `PANIC` | A bug in tickertape was caught; the handle is broken, close it |
| `BeaconInUseException` | `BEACON_IN_USE` | A reader beacon's name is held by a running reader; not thrown yet (no C function creates beacons) |
| `TickertapeException` | `UNKNOWN` | A code this binding does not know; `code()` has it |

`status()` and `code()` give the status on every exception; `BatchAppendException` reports a partial batch append. The
`Status` and `CorruptReason` enums carry the C values and names. Both may gain constants in minor releases (as the C
ABI may add codes and reasons), so a `switch` over them needs a `default` branch. Mistakes Java can detect itself are the usual
`NullPointerException`, `IllegalArgumentException`, `IndexOutOfBoundsException` and `IllegalStateException` (a closed
writer or reader, a view that is no longer valid, concurrent use), thrown before the library is called.

The C ABI records error details per operating-system thread; the binding reads them right after the failing call, on
the same thread, so they are reliable on platform and virtual threads alike.

## Threading

- A `Writer` or `Reader` is used by one thread at a time, and a reader's views with it. It may be handed to another
  thread between calls, with the usual happens-before (an executor, a lock, a queue). The binding checks this on every
  call with an atomic compare-and-set on entry and another on exit (once per call such as `tryNext` or `withPayload`;
  a view's accessors take no guard, they read the view's generation with one acquire load each): a call that finds
  the handle in use by another thread, or closed, throws `IllegalStateException` and does nothing.
- `close()` may be called from any thread, also while another thread blocks in a read or reads the current view. From
  the moment it starts, `isClosed()` is true and every new call is refused. It waits for a call in progress to finish
  (a blocked read ends within one wait slice), which hands the handle straight to it, ends every access to the
  reader's views, then frees the handle on the closing thread (an error from the C close is reported by that
  `close()`). A second `close()` running at the same time waits for the first, so when any `close()` returns the handle
  is freed and, for a writer, the journal can be opened again.

What the binding turns into an exception, on any thread (`IllegalStateException` unless stated):

- any call on a closed writer or reader, a call while another thread is inside a call on the same handle, and a call
  (or `close()`) from inside a filler or a `withPayload` action into its own handle;
- any use of a reader's view after the next call on the reader or after `close()`, including a `close()` on another
  thread while the view is being read (the access in progress finishes or throws; every later one throws);
- any use of a batch's view after the batch is filled again, and reading into a batch while one of its views is
  inside `withPayload`;
- a `PayloadBuffer` or its `segment()` after the filler returned, or on another thread (`WrongThreadException`), and a
  `withPayload` segment after the action returned, or on another thread (`WrongThreadException`).

What it cannot detect, and is undefined:

- reading a reader's view on one thread while another thread advances the same reader (a read or `seek`), with either
  reader: the safe reader may reuse its native buffer for the next record, so the view can show bytes of another
  record or of memory the reader has released; with the mapped reader it can also touch an unmapped page and crash
  the JVM. This breaks the one-thread-at-a-time rule above;
- using an `AppendBatch` or a `ReadBatch` from two threads at once (they are not thread-safe);
- breaking the mapped reader's precondition (see [Reading](#reading)): a truncated segment file kills the JVM with
  `SIGBUS`.
- Separate readers are independent and can run on separate threads and processes. `Journal`'s functions can be called
  from any thread, also while a writer is active.
- A writer starts background helper threads by default (prefaulting, on Linux early write-back, and on macOS
  preparing the next segment); `WriterOptions.prefault(false)` gives a writer that starts none.
- **Virtual threads.** A call into the library pins a virtual thread to its carrier while it runs. Appends and
  `tryNext()` are short. A blocking read pins its carrier for the whole wait, and `open`, `checkpoint` and an append
  that rolls to a new segment pin it while they run. Follow live journals from platform threads, or poll with
  `tryNext()` and park between polls.

## Inspection and retention

- `Journal.verify(dir)` checks every segment and returns a `VerifyReport` (records, sequence range, problems and the
  first one, with its reason and the reason's raw code, `firstProblemReasonCode()`, for a reason this binding does not
  know yet). Corruption is in the report, not thrown. Beside an active writer, a segment that ends in a state the
  writer passes through is scanned again before it is reported: an invalid-looking tail of the newest segment (a
  record being written) and an unsealed segment whose successor exists (a roll in progress). The pauses are 10 ms
  doubling to 320 ms, 630 ms in total, so only a writer stalled in such a state for longer can make it report a
  problem that is not there; verify again to tell (see [`operations.md`](operations.md)).
- `VerifyReport` and `RetentionReport` are final classes with accessor methods, not records, because their C structs
  can gain fields in later versions; `Checkpoint`, `SegmentInfo` and `RecordCopy` are records, since their structs are
  frozen.
- `Journal.listSegments(dir)` lists the segments; `Journal.readCheckpoint(dir)` reads the checkpoint marker (empty if
  there is none).
- Retention (ADR 0016) removes a sealed, checkpoint-covered prefix of old segments. Build `Retention.delete()` or
  `Retention.moveTo(dir)` with limits (`maxSegments`, `maxBytes`, `maxAge`); there is no default that deletes. Pass it
  to `WriterOptions.retention` to run after every checkpoint, or to `Journal.applyRetention(dir, retention, dryRun)`
  to run once.

## Versioning

The binding's version is the version of the C library it is written for (0.4.0 here), and it is released with the
library. At load it checks the library with the C ABI's rule (ADR 0028):

- **Before 1.0** the C ABI is unstable (ABI version 0), so the library must be exactly the binding's version.
- **From 1.0** the ABI version must be equal and the library at least as new as the binding: binding 1.x works with
  any 1.y library, y >= x.

An incompatible library is refused with an `UnsatisfiedLinkError` that names both versions. `Tickertape.bindingVersion()`,
`libraryVersion()` and `abiVersion()` report them.

## Testing

```bash
bindings/java/scripts/build-native.sh                 # libtickertape, the CLI and the C test programs
mvn -f bindings/java/pom.xml verify                   # with JAVA_HOME pointing at a JDK 25
```

The tests run on the module path with `--illegal-native-access=deny`. They check the binding against the C header
(every function wrapped or deliberately not, descriptors, struct layouts, status codes, corruption reasons and
versions), and include the cross-language fixtures: a journal written from Java reads back identically through the
C ABI test program, the Rust CLI and the Python decoder; journals written through C and by every released version
(`tests/compat/`) read back in Java. Tests that need the CLI, the C programs or Python are skipped when those are
missing, unless `-Dtickertape.test.requireTools=true` (as in CI). `-Dtickertape.native.dir=<dir>` points at a library
built elsewhere (for example with a different `CARGO_TARGET_DIR`).
