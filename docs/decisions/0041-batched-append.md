# 0041: Batched append publishes one wake-up per batch

- **Status:** accepted (amends the writer's wake-up rule of [0017](0017-reader-wake-ups.md) for batches; the
  publication protocol of [0003](0003-publication-protocol.md) and the format are unchanged)
- **Date:** 2026-11-20

## Context

The roadmap ("Path to v1.0" → "Finish eliminating avoidable work") asks for an evaluation of batched append and
batched notifications, with explicit publication, partial-failure and latency semantics, shipping only what
measurements support.

Before this record the C ABI's `tickertape_writer_append_batch` and the Python binding's `Writer.append_many` each
called `Writer::append` once per record. Each `append` does, for its one record:

1. Checks the writer state and the sequence number, computes the frame length and checks that the record fits in the
   active segment (rolling to a new one if it does not).
2. Writes the record's header fields and payload, then its frame CRC.
3. Release-stores the record's commit word: this publishes the record (ADR 0003).
4. Advances the write offset.
5. Publishes a wake-up (ADRs 0017 and 0021): a sequentially consistent increment of the shared event counter and a
   load of the waiter count, plus a wake-up system call (`futex` wake on Linux, `os_sync_wake_by_address_all` on
   macOS) whenever the waiter count is not zero.
6. Tells the prefault and early write-back helpers, if any, where it is, and on macOS, once the active segment is half
   full, starts preparing the next one (ADR 0022).

### What cannot be batched

- **One commit word per record.** Readers, `verify` and recovery walk the chain record by record and stop at the
  first empty commit word; format version 1 has no marker that commits several records at once. Every record
  needs its own release store, in order.
- **One fit check, header and CRC per record.** Records have their own lengths, so where each one starts, and
  whether it needs a roll, depends on the ones before it. The CRC covers one frame. Summing the batch's lengths up
  front could skip some fit checks when the whole batch fits, but that check is a comparison against a value the
  writer already holds, the batch may be an iterator of unknown length, and a roll can fall anywhere in the batch.
- **A roll's publication.** A roll seals the old segment, which readers must see to move on, so it publishes as it
  does today.

What remains is step 5: a batch can publish one wake-up instead of one per record.

### What that saves

The pull request records the measurements (paced bursts of 64-byte records with a reader following, on macOS). With
no reader blocked, the increment and load cost too little to measure against the rest of an append: one publication
per batch made no measurable difference. With a reader that catches up and blocks between bursts, each append made
while the reader is still registered as a waiter (until it has woken up and unregistered) issues a wake-up system
call, so batching cut the writer's time per record by several times for bursts of tens of records. The cost is
latency for blocked readers: one that is blocked learns of a batch's first record only when the batch ends (or at a
roll inside it).

## Options considered

1. **No batch API.** Callers loop over `append`. Costs nothing in the API, and leaves the extra wake-up system calls
   in exactly the case where a reader is waiting for data.
2. **`Writer::append_batch`, records visible one by one, one wake-up at the end** (chosen). Each record is committed
   as `append` commits it; only the wake-up waits for the batch to end.
3. **`append_batch` with every record visible at the end**, by storing the batch's commit words last to first so
   that the first record's word, stored last, publishes the whole batch at once. Readers stop at the first empty
   commit word, so they would see nothing and then everything. But a crash in the middle of such a batch leaves
   committed records after an empty commit word, which recovery treats as evidence of committed history after
   damage, not as a torn append (crash contract C8 and C11 (2), ADR 0035): the crash contract would change, for a
   gain no measurement asked for. It would also delay every record of the batch for polling readers.
4. **Deferred wake-ups without a batch API**: `append` skips the publication (always, or every N appends, or within
   a time window) and a later call or timer publishes. Works with `append_with`, but a blocked reader then depends
   on the caller remembering the call, or on a timer thread; forgetting it costs that reader up to its
   `Backoff::max_sleep` for each record. The batch call publishes by construction.

## Decision

Option 2: `Writer::append_batch(records) -> Result<Range<u64>, BatchError>`, where `records` is any
`IntoIterator<Item = NewRecord>` and the range holds the sequence numbers the records got.

- **Order and visibility.** Records are appended in order, each committed and visible to readers as soon as it is
  written, as consecutive `append` calls would make them. No reader can see a later record of the batch without the
  earlier ones (the commit-word chain of ADR 0003 is unchanged). A polling reader can read the first records of a
  batch before the call returns.
- **Wake-up.** When wake-ups are enabled (`WriterConfig::notify`), a batch that commits at least one record
  publishes one wake-up after the last record it appended, instead of one after each record, also when the batch
  stops early and when the caller's iterator panics after some records were appended. The publication comes before
  the caller's iterator is dropped, so the iterator's destructor cannot delay it. Each roll inside the batch also
  publishes, as it does for `append`, so a blocked reader can see the batch's first records at a roll, before the
  batch ends. An empty batch appends nothing, publishes nothing and returns the empty range at `next_seq`. A
  single-record `append` publishes right after its commit and the offset advance, before the prefault, write-back
  and spare-segment work, as before this record.
- **Why no wake-up is lost.** The event-count protocol of ADR 0017 needs every commit that a registered reader may
  have missed to be followed by an increment of the event counter. Every commit word of a batch is stored before the
  batch's publication, so a reader that checked before one of those stores sees the counter change. Commit words are
  stored exactly as consecutive appends store them, so the publication order has no new interleaving and the `loom`
  model of the commit-word protocol (`protocol_model.rs`) is unchanged.
- **Rolls.** A record that does not fit in the active segment rolls first, as with `append`; a batch can span
  segments.
- **Partial failure.** The batch stops at the first record that cannot be appended, for any error `append` returns.
  `BatchError` carries `appended` (the number appended, which is also the failing record's index), `first_seq` and
  the `error`. The appended records are committed and stay committed, with consecutive sequence numbers from
  `first_seq`; the iterator is not advanced past the failing record. To retry, append again from index `appended`.
- **Durability.** Unchanged from `append`: the writer's thread syncs nothing during a batch (a roll on macOS can wait
  for the thread preparing the spare segment, which syncs it; ADRs 0022, 0023 and 0038), and only a later checkpoint
  makes the batch's records durable.
- **Latency.** A blocked reader is woken at the first roll inside the batch or, without one, when the batch ends, so
  its delay for the batch's first record grows by the time the writer takes to write the records up to that point.
  Callers for whom that matters keep batches short.

The C ABI's `tickertape_writer_append_batch` (signature unchanged) and Python's `Writer.append_many` call
`append_batch`. The C function's `*appended_out` is set to the number appended when it returns; its documentation no
longer says it is updated as each record commits (a caller could not observe that during the call). If the call
unwinds inside the library (a bug), `*appended_out` still holds the number of records committed before the record
being appended, as before.

There is no batch form of `append_with`: a closure per record, or one closure for the batch, is a larger API to
stabilise, and what the measurements support batching is the wake-up alone. An encoder that wants one wake-up per
burst can encode the burst into its own buffers and pass them to `append_batch`, at the cost of one copy.

## Consequences

- New public items: `Writer::append_batch` and `BatchError`. The C ABI and Python API are unchanged.
- Tests: `tests/batch.rs` (order and returned range, an empty batch, a roll inside a batch, a failure in the middle
  with the iterator not advanced past it, a blocked reader woken by a batch, a panicking iterator still waking
  readers), `tests/cross_process.rs::two_readers_and_a_late_reader_batched` (readers in other processes, with
  rolls inside batches), and two unit tests in `writer.rs`: a single-record append has published its wake-up when
  the helper work starts, and a batch publishes before it drops the caller's iterator (also when the iterator
  panics). The C ABI's unit test of the count on unwinding now drives the batch iterator.
- `benches/benches/append.rs` gains an `append/batch` group (with and without a reader that blocks); `append` must
  not get slower, which the pull request shows with before and after runs.
- The roadmap item's other parts (profiling the readers and serialization paths under the sustained workloads)
  stay open.
