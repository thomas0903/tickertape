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

**Record views.** A view must not be readable after the reader moves on: for a mapped reader the memory may be unmapped
(a crash, not an exception). FFM can enforce lifetimes only through arenas, and an arena per record allocates; a
generation counter checked on every access allocates nothing but cannot cover a `MemorySegment` handed out.

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
- `Journal.verify`, `listSegments`, `readCheckpoint`, `applyRetention` (with dry run) return records (`VerifyReport`,
  `SegmentInfo`, `Checkpoint`, `RetentionReport`). `Tickertape` has the versions, `loadLibrary(Path)`,
  `librarySource()` and `realtimeNs()`.
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
functional interface whose `fill(PayloadBuffer)` may throw. The writer creates its upcall stub on first use (bound to the
writer, in an automatic arena) and reuses it. The upcall target catches every `Throwable` and returns non-zero, so
nothing ever unwinds through native frames (an exception escaping an FFM upcall terminates the JVM); after the call,
`appendWith` throws `FillAbortedException` with the filler's exception as its cause (an `Error` is rethrown as it is),
or, if the abort's clean-up failed, the library's `JournalIoException` with the filler's exception as its cause. The
`PayloadBuffer` is one object per writer, valid only while the filler runs: it writes through checked offsets into the
mapped bytes, and `segment()` hands out a `MemorySegment` whose confined arena closes when the filler returns. A call
into the writer from inside the filler is refused by the binding's guard (see Threads), which aborts the append.

We measured the upcall informally (`UpcallCostTest`, run on request; not under the benchmark rules, so no numbers here):
it adds a fixed cost per append, of the same order as a small copying append. So `append` stays the default for small
records, and `appendWith` pays off when the payload would otherwise be built in a temporary buffer first (larger
records, or records encoded field by field). Both allocate nothing per record.

**Ownership and lifetimes.**
- `Writer` and `Reader` are `AutoCloseable`; `close()` is idempotent. Each holds its C handle in a small guard object
  (`NativeHandle`). A `Cleaner` frees a handle whose owner became unreachable without being closed: a safety net only,
  documented as such. Every call ends with `Reference.reachabilityFence(this)`, so an owner cannot become unreachable
  while its call is still in the library.
- `next` and friends return a `RecordView`: the **same object every time** (no allocation per record), valid until the
  next call on the reader other than `position()`, or until close. Validity is a generation counter that every reader
  call advances; every accessor checks it and throws `IllegalStateException` on a stale view. Typed reads
  (`get(ValueLayout.OfLong, offset)` and so on), bulk copies and the metadata go through a segment over the whole
  address space at `address + offset`, after bounds and generation checks, so no segment object is created per record.
  A view never hands out a long-lived `MemorySegment`: `withPayload(action)` gives the action a read-only segment in a
  confined arena that closes when the action returns, and holds the reader's guard meanwhile, so the action cannot
  move the reader. The one case the checks cannot catch is another thread closing or advancing the reader *while* this
  thread reads a view; that breaks the threading rule below and is documented.
- `ReadBatch` and `AppendBatch` own their native memory (automatic arenas: the garbage collector frees it); a batch's
  views are valid until the batch is filled again. The copying API: `RecordView.copy()` (a `RecordCopy` record),
  `payloadToArray()`, and `copyPayloadTo` a `byte[]`, `ByteBuffer` or `MemorySegment`.
- After warm-up, `append` (all forms), `appendWith`, `appendBatch`, `tryNext`, a timed `next` that finds a record,
  `readBatch` and the view accessors allocate nothing. `AllocationTest` checks this with the thread's allocated-bytes
  counter around 50,000 operations of each.

**Threads.** A writer or reader is used by one thread at a time and may be handed to another thread between calls
(the C rule, ADR 0028). The binding enforces this cheaply: every call enters the handle's guard with one
compare-and-set (`IDLE` to `BUSY`) and leaves with a release store. A call that finds the handle busy (another thread
is inside a call, or a filler or payload action calls back into its own handle) or closed throws
`IllegalStateException` without reaching the library, so use after close, double close and concurrent use are
exceptions, never undefined behaviour, and the C ABI's `TICKERTAPE_E_BUSY` is unreachable. `close()` may be called
from any thread: it requests the close, waits for a call in progress to finish (a blocked read notices within one wait
slice and throws `IllegalStateException`), then frees the handle exactly once; closing from inside the handle's own
filler is refused. Separate handles, and `Journal`'s functions, are independent and can run on any threads. `fork()`
does not arise in Java.

**Interruption and blocking.** A blocking read never passes `TICKERTAPE_WAIT_FOREVER` to the library. It waits in
slices of at most `waitSlice` (default 10 ms) and, between slices that returned nothing, checks
`Thread.interrupted()` (throwing `InterruptedException`) and the close request. A slice that returns a record returns
it at once, even if an interrupt arrived during the slice; the interrupt status then stays set for the caller, and the
next blocking call throws. So an exception from a read means nothing was consumed, and an interrupt is noticed within
one slice of waiting (validation and file I/O after a seek are not interrupted, as in C). A blocking call on an already
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
`requiredBytes()`, `FillAbortedException`, `HandleBusyException`, `InternalErrorException`); a code the binding does not
know (a newer library) is a plain `TickertapeException` with `Status.UNKNOWN` and the raw `code()`. `Status` and
`CorruptReason` are enums with the C values and names. `BatchAppendException` reports a partial batch: `appended()`,
`firstSeq()`, and the failing record's mapped exception as its cause. Mistakes Java can see itself (null, negative
lengths, out-of-range offsets, closed or busy handles) are the usual `NullPointerException`,
`IllegalArgumentException`, `IndexOutOfBoundsException` and `IllegalStateException`, thrown before the library is
called.

The C ABI keeps error details per OS thread. The binding reads them (code, message, OS error, corruption reason) right
after the failing call, in the same sequence of downcalls, with no Java code in between that can block; a virtual
thread is unmounted only when it blocks or yields, so the sequence runs on one OS thread. As a check, the thread's last
error code must equal the status the call returned; if not, the exception says the details are unavailable instead of
reporting another call's. The exception object is built only after all four values are read.

**Finding the library.** In this order, and the first library found must be compatible or loading fails (it never falls
through to a later candidate after a failure): `Tickertape.loadLibrary(path)` before first use; the system property
`tickertape.library.path`; a **natives JAR** for this platform on the class path; each directory of
`java.library.path`; the operating system's own search (`dlopen` of `libtickertape.so` or `.dylib`). The natives JAR
(classifier `natives-<os>-<arch>`: `linux-x86_64`, `linux-aarch64`, `macos-aarch64`, `macos-x86_64`) carries the
library under `META-INF/tickertape/native/<platform>/` with its SHA-256 in a `.sha256` file; `META-INF` is never a
package, so the resource is visible from the class path and the module path alike. The binding extracts it into a new
directory that `Files.createTempDirectory` names randomly, with owner-only permissions (under `java.io.tmpdir`, or
`tickertape.library.extractDir`), checks the extracted file's SHA-256 against the JAR's, loads it, and deletes the file
and the directory at once (the loaded mapping stays valid). A missing or wrong hash refuses the library. The natives
JAR is preferred over `java.library.path` because it is built for exactly this binding version, while the default
library path may hold an unrelated installed copy; an explicit path or property beats both. The natives JARs are
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
  next call, payload segments after their scope, batches refilled, the Cleaner releasing a forgotten writer);
  interruption (latency, no lost or duplicated records under constant interrupts, already-interrupted threads, close
  from another thread, virtual threads); threading (concurrent use refused, handles moved through an executor,
  readers on several threads, inspection beside a writer); allocation; library loading in fresh JVMs (natives JAR
  extraction and clean-up, wrong or missing hash, `java.library.path`, nothing found, a file that is not the library,
  the module path with and without the native-access flag); the example; and the cross-language fixtures: a journal
  written from Java (every append form) reads back identically through the C ABI test program, the Rust CLI's `verify`
  and `export` and the Python decoder, a journal written through C reads back in Java, the C reader follows a Java
  writer live, and every released-format fixture in `tests/compat/` reads back in Java with the manifest's stream
  digests (safe and mapped readers, visible and durable-only) and is recovered and continued by a Java writer.
- CI: a new job `java binding (<os>)` on Linux x86-64, Linux arm64 and macOS builds the library, the CLI and the C
  programs, runs the tests with JDK 25 (Temurin, pinned), builds the natives JAR, and runs the example from the two
  built JARs only, on the class path and the module path, failing on any warning (the install test ADR 0031 asks for).
- New docs: `docs/java-api.md`, `bindings/java/README.md`, the example `bindings/java/examples/Example.java`.
- Each release must keep the binding's version equal to the library's (the test fails otherwise), and a new C function
  needs a decision here before CI passes.
- Not covered: publishing to Maven Central and the release pipeline for the natives JARs (they need the owner's
  namespace, a signing key and a token; ADR 0031), musl Linux, Windows, JDKs before 25.
