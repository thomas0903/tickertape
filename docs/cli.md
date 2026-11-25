# Command-line interface (M5–M6)

Install/build the `tickertape-cli` package; its binary is `tickertape`. Commands use the public library API
and never take the writer lock (`index`, in a directory without a checkpoint marker, tests it). **`archive` is the only
command that changes a journal's data**, and `index` writes only the seek indexes in its `seek-index/`; all other
commands never write segments, the checkpoint marker or anything else there. Paths may name a journal with an active writer.
The one other write is that a following `tail`, while it waits for new records, registers itself as a waiter in the
writer's shared-memory wake-up region (ADRs 0017, 0021), which is outside the journal directory and holds no
journal data. `status --output` writes a file that must be outside the journal directory.

All commands support `--help`. Exit codes: **0** success (including help, exhausted input, count reached or idle
timeout), **1** usage, I/O or missing-journal error, **2** corrupt data or an unsupported journal format (for
`status`, also a journal the writer would refuse), **3** (`status` only) a health threshold is breached.
Errors and recovery warnings go to stderr. JSON results go to stdout (or the selected export file).

## Commands

- `tail <DIR> [--from <SEQ> | --from-end] [--count <N>] [--no-follow] [--format text|json]
  [--timeout-secs <S>] [--durable-only] [--seek-index] [--decode market-demo]`: default starts at the oldest record and follows forever. `--from` includes SEQ;
  `--from-end` starts at the end of the journal as observed at startup (`Reader::seek_to_end`): it reads the newest
  segment from the checkpoint on, not the records of the segments before it, so
  damaged records there are not reported (the oldest segment's header is still checked when the journal is opened). `--count 0` writes nothing. `--no-follow` stops when
  the reader reaches the current committed end. `--timeout-secs` accepts a nonnegative decimal number and stops
  after that many idle seconds; it resets after each record. Text shows sequence, schema, timestamps, payload
  length and the first 32 payload bytes as lowercase hex, followed by `…` when truncated. JSON is the record
  JSONL contract below. Each line is flushed for live consumers. After discarded history, tail warns and seeks
  to the sequence reported by the reader. If retention or archiving removed that sequence (a `--durable-only` tail
  learns of a removal when the checkpoint moves past its position), it continues at the oldest sequence still in the
  journal, with a warning on standard error naming the skipped records (`records A to B were removed before tail
  read them; continuing at sequence C`). Warnings never go to standard output, so the JSON lines stay intact. `--decode market-demo` appends ` decoded=<kind> instrument=… side=…
  price=… qty=…` (plus ` trade_id=…` or ` action=…`) to each text line, or ` decoded=-` when the record isn't a
  market-demo event; with `--format json` it adds the same `decoded` object as `export --decode` (see below).
  `--durable-only` returns only records below the current checkpoint marker's `next_seq` and waits for later
  checkpoints using the usual idle timeout. With no valid checkpoint, it returns nothing until one appears;
  `--no-follow` stops at that boundary and `--from-end` starts at the checkpoint observed at startup.
- `inspect <DIR> [--json]`: checksum scan with per-segment metadata, totals, checkpoint and problems. Text
  displays creation time as RFC 3339 UTC, used bytes and percent. An invalid header has unavailable metadata.
  Missing/invalid checkpoint slots yield `none` in text or `null` in JSON; a checkpoint I/O error fails the command.
- `verify <DIR> [--json]`: checksum scan and chain validation, totals and every problem. Both inspect and verify
  return 2 when the scan reports problems. On a live journal the scan describes observations during the scan,
  not an atomic snapshot; states a running writer passes through are scanned again after pauses that add up to
  630 ms (plus the time of the scans) before they are reported, and records the writer commits while `verify` looks
  behind an empty commit word are followed, not reported (see `operations.md` → Checking a journal). Where a
  segment's records end at an empty commit word (the newest segment, or an older one that is damaged or whose roll
  was interrupted, which it first scans again with the same pauses in case a rolling writer is about to seal it),
  it searches the rest of the segment for committed records hidden behind that word (`EmptyCommitWord`, or
  `UncheckedTail`, ADR 0035; an older segment with nothing found there is reported as `BrokenChain`): it reads
  backward from the segment's end to find the last non-zero byte, then checks the candidates up to it, and can stop
  early at the first evidence or at its work budget. It used to stop at the end of the records. On an empty 128 MiB segment this took
  21 ms instead of 9 ms (warm page cache, Apple M4 Pro); with a cold cache or slow storage it costs reading the
  unwritten rest of the newest segment. `inspect` runs the same scan.
- `export <DIR> --format jsonl|parquet [--output <FILE>|-] [--from <SEQ>] [--to <SEQ>]
  [--decode market-demo] [--durable-only] [--seek-index]`: writes records from the
  inclusive range; JSONL defaults to stdout and `--output -` also selects stdout for JSONL. The output file is created or
  truncated, but a path naming an existing journal entry (including via symlinks/hard links), or any path inside
  the journal, is rejected (an entry that retention removes while this check walks the directory is skipped, unless
  the inode number in its directory entry is the output file's; the files in `seek-index/` are not journal data and are
  not compared).
  `--to` must not precede `--from`.
  A first safe-reader pass, the startup scan, counts the selected records. It stops at `--to`, at the first empty
  slot it sees, or, with `--durable-only`, at the checkpoint bound described below; without `--to`, a visible scan
  therefore also counts records the writer appends while it runs, for as long as the writer stays ahead of it. A
  second pass, starting at the record the first started at, emits exactly the counted records, never following later
  appends. Both passes compute a fingerprint of the records they read (sequence number, schema id, both timestamps,
  payload length and bytes). If the second pass read different records than the scan counted (recovery replaced
  records the scan had read and the writer appended others with the same sequence numbers; crash contract C8, C11),
  the export ends with exit 1 and `journal changed during export: the records read differ from those the startup
  scan counted; the output is not a consistent export`, after writing what the second pass read (a Parquet file is
  finalized). The fingerprint is a 64-bit hash that is not cryptographic: it catches a journal that changed in this
  way, not records constructed to collide. Under retention or archiving (crash contract C14), a segment that retention removed after a pass had
  read it loses nothing: the pass continues at that position on the current chain. It does the same after recovery
  replaced the segment; records recovery replaced there are reported by the fingerprint check. Records removed before
  a pass read them end the export with exit 1 and `export stopped at sequence N: records N to M were removed from the
  journal (by retention or archiving) before export read them`. If the second pass finds fewer records than the scan
  counted, it ends with exit 1 and `journal changed during export: captured records are no longer available`.
  The destination is not created or truncated when the failure is found before the first record is written: a
  startup scan that fails before counting a record reports its failure without opening the destination, and the
  second pass reads its first record before opening it. A failure found after that leaves the records written before
  it in the output, finalizes a Parquet file as a readable file, and exits 1 (2 for corruption or an unsupported
  journal format, as below).
  Corruption encountered in the selected startup scan is reported after the valid prefix has been written. An empty
  journal or an empty selection exports no records, with exit 0. Parquet requires a file destination (missing
  `--output` or `-` returns 1). Parquet uses Snappy compression
  and row groups of at most 100,000 records. On corruption the valid prefix is finalized as a readable file
  before returning 2. Both formats share the startup scan and inclusive range semantics.
  `--durable-only` additionally caps both formats at the checkpoint marker's exclusive `next_seq` observed before
  the startup scan, even if the writer checkpoints again during export. With no valid marker the export is empty.
  If the marker then stops covering records below that bound (it becomes missing or invalid) before the scan reaches
  the bound or `--to`, whichever is lower, the export ends with exit 1 and `journal changed during export: the
  checkpoint marker no longer covers records it covered when export started`, without opening the destination.
  Tail and export use the durability guarantee and storage assumptions in [crash contract C13](crash-contract.md)
  and [ADR 0010](decisions/0010-durable-only-reads.md); if the writer never checkpoints, neither exposes records.

- `stats <DIR> [--from <SEQ>] [--to <SEQ>] [--durable-only] [--seek-index] [--json]`: summarize committed records
  with totals, per-schema counts/shares/payload lengths, overall payload percentiles, separate venue/local
  timestamp summaries, and the largest gap between consecutive local timestamps. Ranges are inclusive;
  `--to` must not precede `--from`. One nonblocking startup scan accumulates the selected records once,
  stopping at the first uncommitted slot or upper bound. Like export's capture scan, this observes the end
  during scanning; it is not an atomic snapshot of a live writer. It never waits for future appends or rereads
  records for output. `--durable-only` freezes the checkpoint's exclusive `next_seq` before the scan; no
  valid checkpoint means an empty report. Corruption returns **2**, with **no stats output** (including no
  valid-prefix report); input I/O errors return **1** with no stats output. Diagnostics go to stderr.
  Text is compact and aligned; `--json` emits the object documented below.
  Memory does not grow with record count: a fixed payload histogram, bounded timestamp sort chunks and
  per-schema aggregates are retained, alongside the public reader's buffers and segment listing. Exact
  second counts for unordered input spill to two anonymous temporary files outside the journal when needed;
  temporary disk usage grows linearly with nonzero timestamp count. Temporary storage errors return **1**.
  If `TMPDIR` resolves inside the journal, spilling fails without creating files there.

- `archive <DIR> (--to <DEST> | --delete) [--keep-segments N] [--max-bytes B] [--older-than DUR]
  [--dry-run] [--json]`: requires at least one limit and exactly one action. Removes only a contiguous sealed
  prefix strictly before the checkpoint segment, stopping at the first ineligible segment or error. Without a
  valid checkpoint nothing is eligible. Limits combine with OR; counts and bytes include every segment, including
  the active one, so checkpoint eligibility can prevent reaching the limit. Age uses the successor header's
  creation time as seal time. `DUR` is a nonnegative integer followed by `d`, `h`, `m` or `s` (for example `30d`,
  `12h`, `90m`, `3600s`). `B` is a nonnegative integer with an optional uppercase `K`, `M`, `G` or `T` suffix
  multiplying by 1024, 1024², 1024³ or 1024⁴. Overflow is a usage error. `--delete` and the plan check each
  segment's seal from the end of the file and do not read its records; `--to` reads and validates the whole segment
  before moving it (crash contract C14). `--dry-run` prints only the plan and changes no files; otherwise the command applies the plan and reports removal counts.
  With `--to`, a dry run can therefore succeed for a segment with a damaged record at which the move itself stops
  (exit 2), after moving the segments before it. `--to` requires an
  existing directory; moves never replace existing entries, even identical copies from an interrupted move.
  Same-filesystem moves use a no-replace rename; `EXDEV` uses a synced copy before source removal. Earlier
  removals remain in effect on failure. Exit **0** includes nothing to do, **1** is usage/I/O/conflict, **2** is
  corruption where retention validates the checkpoint or candidate chain. See [operations](operations.md) for
  interrupted-copy conflict resolution. No JSON output is produced on failure; diagnostics go to stderr.

- `status <DIR> [--format text|json|prometheus | --json] [--readers-dir <DIR>] [--watch <INTERVAL> [--samples <N>]]
  [--output <FILE>] [--max-checkpoint-age <DUR>] [--min-headroom <B>] [--min-headroom-segments <N>]
  [--max-reader-lag <N>]`: one sample of the journal's operational state (ADR
  [0032](decisions/0032-operational-metrics.md)): the writer's head, the segments, the checkpoint marker and an
  estimate of its age, the exposure (records not covered by the checkpoint, which a power loss can remove), storage
  headroom, the writer's PID (best effort), and every reader that publishes a beacon
  (`tickertape::status::ReaderBeacon`) in `<DIR>/readers` or `--readers-dir`. It never takes the writer lock or
  touches it, writes nothing into the journal, and opens every file non-blocking (a FIFO can't hang it). Values it
  can't establish are null, never guessed; estimates are named `estimated_*`. It reads only the newest segment, so
  `head.verified` and `exposure.verified` say whether it validated the records between the checkpoint and the head;
  `verify` checks everything. When they are false, a healthy verdict (exit 0, `health ok (unverified)`) does not mean
  that the writer would open the journal: a segment the sample didn't read may be missing.
  Cost: one `stat` per segment, a `pread` scan of the newest segment from the checkpoint position (or from its
  start if the checkpoint is in an older segment), a few headers, `statvfs`, and a few small reads per beacon; with
  `--watch`, each sample after the first scans only what was appended since the previous one.
  `--watch` samples every `INTERVAL` (an integer followed by `ms`, `s`, `m` or `h`) until interrupted, or `--samples`
  times; from the second sample on it reports append rates and an estimated time until the volume is full. A sample
  that fails is reported (a JSON line with `error`, a text `error` line, or `tickertape_up 0`) and the watch goes on.
  `--output` writes each sample to a new, uniquely named temporary file in the same directory (created exclusively,
  never following a symlink) and renames it over `FILE`, so a node_exporter textfile collector never reads a partial
  file. `FILE` is resolved when each output is written: a `FILE` whose directory doesn't exist is an output that can't
  be written (exit codes below). Checked before every replacement: `FILE` must not be or lie inside the journal
  directory, its `quarantine/` or the beacon directory. The paths with symlinks resolved decide; the paths as given are
  compared too, as an extra refusal, where neither has a `..` (after a symlink, `..` leads to the parent of the
  symlink's target, so only the resolved path says where such a path leads). So neither the symlink through which the
  journal was given nor a `quarantine/` that is a symlink to another directory can be named. `FILE` must not have a name
  a writer gives quarantined files (20 digits, ending `.bin` or `.bin.partial`). A `FILE` that doesn't exist is created;
  an existing one is replaced only if it is a regular file (not a symlink, directory, FIFO or device) that is an earlier
  output of `tickertape status` in any of the three formats (recognised by its first bytes: `journal     /`,
  `{"schema_version":<n>,"journal":"/` or `# HELP tickertape_status_timestamp_seconds `) or empty, and that no entry of
  the journal directory, `quarantine/` or the beacon directory reaches through a symlink or a hard link (an empty file
  the journal's directories can't be listed to rule out is refused; a file that starts like an earlier output is taken
  for one when they can't be listed). Anything else is refused: remove it, or choose another path. So a sample that
  fails always replaces an earlier output with `tickertape_up 0`, also when the journal can't be listed at all
  (unreadable, not a directory, a symlink loop), and replaces nothing else except in the two cases below (the archive
  directory of `archive` is not known to `status`, but nothing in it is an earlier status output). Two cases remain,
  both needing someone to have arranged the files: when the journal's directories can't be listed, a file outside the
  journal that starts with one of the three prefixes is taken for an earlier output even if a journal entry points to
  it; and the check and the rename are not one atomic step, so a file another process puts at `FILE`'s name in between
  is replaced. Give `--output` a path in a directory only the monitoring job writes to.
  `--watch` with `--format prometheus` needs `--output`. Thresholds (`DUR` and `B` as for `archive`):
  `--max-checkpoint-age` is breached when records are uncovered and the checkpoint's estimated age is above `DUR` or
  unknown, or there is no checkpoint (an idle, fully checkpointed journal and an empty one are healthy); an age
  unknown because the marker kept changing while it was read (`marker_changing`, a checkpoint just happened) is not a
  breach;
  `--min-headroom` when fewer than `B` bytes are available; `--min-headroom-segments` when fewer than `N` more
  segments are estimated to fit; `--max-reader-lag` when a **live** (or `uncertain`) reader beacon lags by more than
  `N` records (stale beacons, whose owner died, never count). Exit codes: **0** healthy; **3** a threshold is
  breached; **2** the sample found the journal corrupt, as `verify` would: the head at invalid bytes, a checkpoint
  marker the writer refuses (missing segment, invalid offset or segment header; also a marker in a directory without
  segments), a segment the listing proves missing between the checkpoint and the head, a damaged newest segment
  header, an unsupported format (2 takes precedence over 3); **1** usage or I/O errors and a missing journal. **0**
  with `head.verified` false is "healthy as far as the sample can tell", not "the writer would open it". When the
  output (the `--output` file, or standard output) can't be written, refused or failed, the sample's verdict wins: 2,
  3, or 1 for a failed sample, with the output's failure on standard error too; a healthy sample then exits **1**.
  With `--watch` it reports health in each sample and exits 0 when `--samples` are done; an output it can't write
  stops it with **1**.
  On a one-shot sample, `last_local_ts_ns` of the head is null when the checkpoint is at the head (the scan starts
  there and sees no record).

- `index <DIR> [--rebuild | --check | --remove] [--json]`: writes, checks or removes the seek indexes of sealed segments
  (ADR 0045, crash contract C15): files in `<DIR>/seek-index/` that let a reader which opts in start a seek inside a
  segment (`--seek-index` of `tail`, `export` and `stats`; `ReaderConfig::seek_index`). Without an option it builds what
  is missing or outdated: for the sealed segments before the segment of the checkpoint marker, which it makes durable
  first, also beside a live writer; without a valid marker, for every sealed segment, if no writer has the journal open
  (an archive). Each segment it indexes is read and validated once. An index that is right is kept (it checks the
  index, the seal and the last indexed record); `--rebuild` writes every one anew, also one of a newer version, which is
  otherwise kept. The user running it must own the journal's files (root gets no exception); it creates `seek-index/`
  only as the journal directory's owner, with that directory's group and mode, if the user is in that group and the
  directory is not writable by everyone. `--check` changes nothing, reads every segment that has or could have an index,
  and compares every entry with it. `--remove` deletes every index and leaves the directory; what else is in it stays.
  Text output is one line per segment: the file, the state, and the reason if there is one. Exit **0** also for segments
  that have or get no index; **1** usage or I/O error, missing journal, no marker and an open writer, a `seek-index/` that
  may not be used, or a segment of another user (`not_permitted`); **2** a marker that names a missing segment or
  position, or with `--check` a wrong index. Indexes built before a failure stay. The states are below.

`--seek-index` (`tail`, `export`, `stats`) lets the command's seek to `--from` start at an indexed record. The output is
the same; records before that one are not read, so damage there, which otherwise stops the command with exit 2, goes
unnoticed. Do not use it where someone who may not write the segments can write `seek-index/` (crash contract C15).

## Machine-readable contracts

All JSON is UTF-8, compact, and ends with a newline. Integer fields are unsigned JSON numbers with their Rust
widths listed below; nullable fields use JSON `null`. Consumers must preserve u64 precision. Object key order
is fixed for record JSONL; consumers of inspect/verify should use names. File names are basenames, converted to
UTF-8 with replacement characters for non-UTF-8 names. Reason strings are human-readable diagnostics, not codes.

Every `--json` document (`verify`, `inspect`, `stats`, `archive`, `index` and `status`) starts with `"schema_version":1`. Each
command's schema is versioned on its own. Within a version, fields may be added and, where this page says a set may
grow, enumerated string values; consumers must ignore fields they do not know. Removing a field or changing its
meaning bumps that command's `schema_version`. Record JSONL has no `schema_version`: its keys are fixed.

### Record JSONL (`tail --format json`, `export --format jsonl`)

Exactly one object per record, keys in this order:

```json
{"seq":42,"schema_id":3,"venue_ts_ns":0,"local_ts_ns":17,"payload_b64":"aGVsbG8="}
```

`seq`, `venue_ts_ns`, `local_ts_ns`: u64; `schema_id`: u32; `payload_b64`: string, standard RFC 4648 base64
with padding (empty payload becomes `""`). No wrapper, metadata lines or summary appear in JSONL.

### `verify --json`

```text
{"schema_version":1,"segments":u64,"records":u64,"first_seq":u64|null,"last_seq":u64|null,
 "problems":[{"file":string,"offset":u64,"reason":string}, ...]}
```

`records` counts valid records found across the scan; sequence bounds are null when there are none. A damaged
journal may contain valid records in later segments; totals do not imply a continuous valid prefix.

### `inspect --json`

```text
{"schema_version":1,"segments":[{"file":string,"base_seq":u64|null,"size_bytes":u64|null,
 "created_unix_ns":u64|null,"created_utc":string|null,"records":u64,"end_seq":u64,
 "used_bytes":u64,"used_percent":number|null,"end_state":"sealed"|"open"|"damaged"|"unknown"}, ...],
 "totals":{"segments":u64,"records":u64,"first_seq":u64|null,"last_seq":u64|null},
 "checkpoint":{"generation":u64,"next_seq":u64,"segment_base_seq":u64,"offset":u64}|null,
 "problems":[{"file":string,"offset":u64,"reason":string}, ...]}
```

`created_utc` always uses nine fractional digits and `Z`. Missing metadata and percent are null for an invalid
header. `end_seq` is exclusive, after the last valid record in that segment (or its filename's base sequence if
none). `used_bytes` includes header and seal when validated; it is zero for an invalid header. `used_percent`
is `100 * used_bytes / size_bytes`. `unknown` reserves handling of future library end states.

### `stats --json`

```text
{"schema_version":1,"totals":{"records":u64,"first_seq":u64|null,"last_seq":u64|null,"payload_bytes":u64},
 "schemas":[{"schema_id":u32,"records":u64,"share_percent":number,"payload_bytes":u64,
 "min_payload_bytes":u64,"mean_payload_bytes":number,"max_payload_bytes":u64}, ...],
 "payload_length":{"min_bytes":u64|null,"p50_bytes":u64|null,"p90_bytes":u64|null,
 "p99_bytes":u64|null,"max_bytes":u64|null},
 "venue_ts":TIME,"local_ts":TIME,
 "largest_local_gap":{"gap_s":number,"from_seq":u64,"to_seq":u64}|null}

TIME = {"records":u64,"min_ns":u64|null,"max_ns":u64|null,"min_utc":string|null,
 "max_utc":string|null,"span_s":number|null,"mean_rate_per_s":number|null,
 "busiest_second":{"records":u64,"start_ns":u64,"start_utc":string}|null,"decreases":u64}
```

`number` denotes a floating-point JSON number. Consumers should use field names, not object key order.
`totals` covers all selected records; payload byte totals exclude record headers and alignment padding.
`schemas` is sorted by numeric id, with `share_percent = 100 * schema records / total records` and
`mean_payload_bytes = schema payload bytes / schema records`. All lengths describe opaque payloads.
Overall min/max and schema min/mean/max use actual lengths. Percentiles use nearest-rank
`ceil(percent * records / 100)`: exact integer lengths through **4096 bytes (4 KiB)**, then power-of-two
bucket **upper bounds** (4097–8192 → 8192, 8193–16384 → 16384, etc.). The histogram is bounded; only the
percentile rank within that histogram is exact above 4 KiB.

For each timestamp field independently, zero means unset and is excluded from counts, extrema and buckets.
`min_ns`/`max_ns` are unsigned Unix-epoch nanoseconds; UTC strings are RFC 3339 with nine fractional digits
and `Z`, computed without locale or time-zone dependence. `span_s = (max_ns - min_ns) / 1e9`;
`mean_rate_per_s = nonzero records / span_s`, or null when the span is zero or unavailable.
`busiest_second` is always a **fixed UTC second**, `ts / 1e9`, not a sliding window, for ordered and unordered
input alike. It counts every set timestamp in that bucket; `start_ns` is the inclusive second boundary and
`start_utc` its UTC representation. Ties choose the earliest second.
`decreases` counts adjacent selected record pairs with both timestamps nonzero and the second smaller;
zero breaks adjacency, rather than comparing across missing timestamps. This definition also applies
independently to local timestamps.

`largest_local_gap` uses adjacent selected records with both local timestamps nonzero and the second at
least the first. Its `gap_s` is the unsigned difference divided by 1e9; ties choose the first pair in sequence
order. A zero gap is valid. It is null when no eligible pair exists. Gaps and decreases do not cross range bounds.
An empty selection has zero totals, null sequence bounds, an empty schema array, null payload lengths,
zero time counts/decreases, null time extrema/UTC/span/rate/busiest second, and a null local gap.
A field with no set timestamps likewise has null time summaries and zero counts/decreases.

### `archive --json`

```text
{"schema_version":1,"dry_run":bool,
 "segments":[{"file":string,"base_seq":u64,"size_bytes":u64,"sealed_unix_ns":u64,
              "reasons":["max_segments"|"max_bytes"|"max_age", ...]}, ...],
 "report":{"removed_segments":u64,"removed_bytes":u64,"already_removed_segments":u64}|null}
```

`segments` is the plan in removal order (oldest first). `file` is a basename. `size_bytes` is the full segment
size; `sealed_unix_ns` is the successor's creation timestamp. `reasons` lists all limits requesting each removal
in count, bytes, age order, evaluated after preceding planned removals. `report` is null for a dry run; otherwise
it counts files and full bytes removed by this run, and planned files already absent at the oldest end due to a
concurrent archiver. A successful empty plan yields an empty `segments` array and zero report counts (or null in
a dry run). JSON numbers preserve their u64 interpretation; consumers should use field names.

### `index --json`

```text
{"schema_version":1,"action":"build"|"check"|"remove",
 "segments":[{"file":string,"base_seq":u64,"state":string,"ignored_by_readers":bool,"reason":string|null}, ...],
 "removed":u64|null}
```

`segments` has one element per segment, oldest first (empty for `remove`). `file` is the segment's basename, `reason` a
human-readable diagnostic. `removed` counts the files `--remove` deleted and is null otherwise. The set of states may
grow:

| `state` | Action | Meaning |
|---|---|---|
| `built` | build | An index was written |
| `valid` | both | The index is right; with `--check`, every entry was compared with the segment. `ignored_by_readers` (`--check`): it is right, but readers ignore the file for its owner or mode (`reason` says which); a build writes it anew |
| `missing` | check | The segment could have an index and has none |
| `ignored` | both | A file that readers do not use because of its content: damaged, stale, another version, not a regular file. A build reports only a newer version, which it keeps |
| `wrong` | check | A reader would use the index, and an entry in it is not on the segment's chain: forged. Exit 2 |
| `not_needed` | both | No record starts outside the segment's first block (every 64 KiB segment) |
| `not_sealed` | both | The segment does not end in a valid seal: it is being written, or is damaged (`reason`) |
| `not_covered` | both | Not before the checkpoint marker's segment |
| `not_permitted` | build | The segment is another user's, or the index cannot be given its group. Exit 1 |
| `vanished` | both | Removed or replaced meanwhile; run it again |

### `status --json`

```text
{"schema_version":1,"journal":string,"sampled_unix_ns":u64,
 "head":{"next_seq":u64,"segment_base_seq":u64,"offset":u64,"segment_size_bytes":u64,
         "end":"open"|"sealed"|"invalid","invalid_reason":string|null,"last_local_ts_ns":u64|null,"verified":bool,
         "chain_broken":bool},
 "segments":{"count":u64,"first_seq":u64,"total_bytes":u64,"oldest_created_unix_ns":u64|null,
             "before_checkpoint":u64},
 "checkpoint":{"generation":u64,"next_seq":u64,"segment_base_seq":u64,"offset":u64,
               "problem":"missing_segment"|"invalid_segment_header"|"invalid_offset"|null,
               "modified_unix_ns":u64|null,"estimated_age_s":number|null,
               "age_unknown_reason":"no_modification_time"|"modification_time_in_future"|
                                    "modification_time_before_segment"|"marker_resized"|"marker_changing"|
                                    "older_slot_lost"|null}|null,
 "exposure":{"records":u64|null,"bytes":u64|null,"estimated_age_s":number|null,"oldest_local_ts_ns":u64|null,
             "verified":bool},
 "storage":{"available_bytes":u64,"total_bytes":u64,"next_segment_size_bytes":u64,"spare_segment":bool,
            "estimated_rolls_left":u64,"estimated_time_to_full_s":number|null},
 "rates":{"interval_s":number,"records_per_s":number,"bytes_per_s":number|null}|null,
 "writer":{"pid":u32|null,"alive":bool|null},
 "readers_dir":{"path":string,"state":"listed"|"missing"|"unusable","error":string|null},
 "readers":[{"name":string,"state":"live"|"stale"|"uncertain"|"invalid"|"unreadable"|"changing",
             "pid":u32|null,
             "next_seq":u64|null,"durable_only":bool|null,"seek_pending":bool|null,
             "lag_records":u64|null,"lag_bytes":u64|null,
             "lag_unknown_reason":"segment_gone"|"other_history"|"segment_unreadable"|"checkpoint_unusable"|
                                   "ahead_of_sample"|null,
             "unread_age_s":number|null,"oldest_unread_local_ts_ns":u64|null,
             "updated_unix_ns":u64|null,"update_age_s":number|null}, ...],
 "health":{"ok":bool,"corrupt":bool,"verified":bool,
           "breaches":[{"code":"head_invalid"|"checkpoint_invalid"|"chain_broken"|"checkpoint_age"|
                                "headroom_bytes"|"headroom_segments"|"reader_lag","reader":string|null}, ...]}}
```

With `--watch`, a sample that fails is the line
`{"schema_version":1,"journal":string,"sampled_unix_ns":u64,"error":{"code":"not_found"|"corrupt"|"unsupported_version"|"io","message":string}}`.

`schema_version` is 1. Fields may be added without changing it; removing a field or changing its meaning bumps it.
With `--watch`, each sample is one line. `journal` is the canonical absolute path of the journal directory (if the
directory doesn't exist when the command starts, the absolute path as given: the samples fail with `not_found`, a
one-shot `--output` file gets `tickertape_up 0`, and a watch goes on sampling). Times
ending in `_unix_ns` are nanoseconds since the Unix epoch by the observer's wall clock (or, for `updated_unix_ns`,
the reader's), except `last_local_ts_ns`, `oldest_local_ts_ns` and `oldest_unread_local_ts_ns`, which are records'
caller-supplied `local_ts_ns` (null if unset). Durations ending in `_s` are seconds. `null` means the value could
not be established.

- `head`: where the next record goes, as far as the newest segment shows. `end` is `sealed` only when the newest
  segment ends in a seal and its successor didn't exist when checked (a writer died during a roll); `invalid` when
  bytes there still fail validation after re-reading (corruption: `verified` is then false and `exposure` unknown).
  `verified` is true if the sample validated every record from the checkpoint (or, without one, from the start of
  the oldest segment) to the head; false when the head is `invalid` or the checkpoint is in an older segment, whose
  successors it didn't read. `chain_broken` is true when the segment names and sizes
  alone prove a segment missing in between (the next segment starts beyond what the previous one can hold): the
  writer refuses such a journal, `exposure` is unknown and `health.corrupt` is true. Other missing segments leave
  `verified` false.
- `segments.before_checkpoint`: segments older than the checkpoint segment, which retention may remove.
- `checkpoint`: the marker's current slot, or null if no slot is valid. `problem` is set when the writer would refuse
  the marker (its segment missing or with an invalid header, or an offset outside the record area); then `exposure`
  is unknown and `health.corrupt` is true. `modified_unix_ns` is the marker file's modification time and
  `estimated_age_s` the time since then, by the observer's clock at the end of the sample; null with
  `age_unknown_reason` when the time is detectably unreliable (`marker_resized`: the marker is not at its full
  length, because something cut it; a writer leaves it so until its next checkpoint succeeds, then extends it;
  `older_slot_lost`: the other slot doesn't hold the generation before the current one, as every checkpoint leaves
  it, so the marker lost bytes and was extended, for example by a v0.4.0 writer at open; `marker_changing`: the
  marker was rewritten during every read, which `--max-checkpoint-age` doesn't count as a breach since it means a
  checkpoint just happened). It is wrong if something else writes the file or a copy didn't keep times.
- `exposure`: `records` not covered by the checkpoint (all records from the oldest segment if there is none); `bytes`
  are the segment-file footprint from the checkpoint position to the head (0 when no record is uncovered, null if a
  segment involved is missing); `estimated_age_s` is 0 with no uncovered records, otherwise the checkpoint's
  estimated age (null with no checkpoint or an unknown age).
- `storage`: `available_bytes` is what unprivileged users may still allocate (`statvfs`), the observed fact.
  `estimated_rolls_left` is how many more segments of `next_segment_size_bytes` fit (the prepared spare's size if
  there is one, else the newest segment's); a prepared spare whose blocks are allocated is `spare_segment` and is
  not counted, since publishing it can still fail on a full volume. Writes into the active segment need no new space
  (except on btrfs), so the roll after these fails. `estimated_time_to_full_s` needs `rates`.
- `rates`: null on the first sample and after the history changed (the journal replaced, a segment repaired);
  `bytes_per_s` counts segment-file bytes.
- `readers_dir`: the beacon directory read; `unusable` (with the OS error) when it exists but couldn't be listed, in
  which case `readers` is empty.
- `readers`: one entry per regular file in the beacon directory with a valid beacon name, sorted by name. `live`
  means a process holds the beacon's lock; `stale` means its owner died (the values are its last report);
  `uncertain` means the lock couldn't be probed (locks unsupported); `invalid` and `unreadable` entries have null
  values, and so do `changing` ones (the name was replaced by another file during every attempt to read it). `lag_*` compare the beacon's last reported position with this sample's head, or with the checkpoint for a
  `durable_only` reader; they are null with `lag_unknown_reason` when the beacon's segment is gone (`segment_gone`)
  or is not the file this journal has under that name (`other_history`: another journal sharing the directory, a
  replaced journal, a repaired segment), when its header can't be read (`segment_unreadable`), when a durable-only
  reader's checkpoint marker is unusable (`checkpoint_unusable`), or when the segment is newer than every segment the
  sample listed (`ahead_of_sample`: the writer created it after the sample listed the journal; the next sample sees
  it). `lag_bytes` is also null while a seek is pending (`seek_pending`). `unread_age_s` is the
  observer's clock minus the first unread record's `local_ts_ns` (0 when caught up; null if that timestamp is later
  than the observer's clock).
- `health`: `ok` if nothing is breached; `corrupt` if a breach is corruption (`head_invalid`, `checkpoint_invalid`,
  `chain_broken`, always checked; the command then exits 2); `verified` is `head.verified`: when false, `ok` means
  healthy as far as the sample can tell, not that the writer would open the journal.

### `status --format prometheus`

Text exposition format 0.0.4. Every metric is a gauge with the label `journal="<canonical DIR>"`; reader metrics
also have `reader="<name>"`. Metrics without a value in this sample (for example the checkpoint's age without a
checkpoint, or when it is unreliable) are omitted. A sample that fails has only `tickertape_status_timestamp_seconds`
and `tickertape_up 0`. `--watch` needs `--output` with this format.

| Metric | Value |
|---|---|
| `tickertape_status_timestamp_seconds` | Sample time; alert when it stops advancing |
| `tickertape_up` | 1 if the sample succeeded, 0 if it failed |
| `tickertape_healthy` | 1 if no given threshold is breached and no corruption was found |
| `tickertape_corrupt` | 1 if the head is at invalid bytes or the checkpoint marker is unusable |
| `tickertape_head_next_seq`, `tickertape_head_invalid`, `tickertape_head_verified` | Head |
| `tickertape_segments`, `tickertape_segments_bytes`, `tickertape_first_seq`, `tickertape_segments_before_checkpoint`, `tickertape_oldest_segment_created_timestamp_seconds` | Segments |
| `tickertape_checkpoint_present`, `tickertape_checkpoint_invalid`, `tickertape_checkpoint_generation`, `tickertape_checkpoint_next_seq`, `tickertape_checkpoint_estimated_age_seconds` | Checkpoint |
| `tickertape_exposure_records`, `tickertape_exposure_bytes`, `tickertape_exposure_estimated_age_seconds`, `tickertape_exposure_verified` | Exposure |
| `tickertape_storage_available_bytes`, `tickertape_storage_size_bytes`, `tickertape_storage_spare_segment`, `tickertape_storage_estimated_rolls_left`, `tickertape_storage_estimated_time_to_full_seconds` | Storage |
| `tickertape_append_records_per_second` | With `--watch`, from the second sample |
| `tickertape_writer_alive` | 1 if the PID in `writer.lock` exists (best effort) |
| `tickertape_readers_dir_usable` | 0 if the beacon directory exists but couldn't be listed |
| `tickertape_reader_live`, `tickertape_reader_lag_records`, `tickertape_reader_lag_bytes`, `tickertape_reader_unread_age_seconds`, `tickertape_reader_update_age_seconds` | Per reader beacon (`reader_live` is 1 for `live`, 0 for `stale`, and absent for `uncertain`, `invalid`, `unreadable` and `changing` beacons, so alert with `unless tickertape_reader_live == 0` rather than `and tickertape_reader_live == 1` to count `uncertain` ones as `--max-reader-lag` does; lag metrics are absent when the lag is unknown) |

### Parquet (`export --format parquet`)

The base schema is exactly:

```text
message tickertape_record {
  required int64 seq (INTEGER(64,false));
  required int32 schema_id (INTEGER(32,false));
  required int64 venue_ts_ns (INTEGER(64,false));
  required int64 local_ts_ns (INTEGER(64,false));
  required binary payload;
}
```

Unsigned integer annotations preserve every bit of u64/u32 values. Timestamps are raw unsigned nanoseconds
since the Unix epoch, with 0 meaning not set. They are not Parquet timestamp logical types, whose signed range
cannot represent all u64 values. In DuckDB 2.0.0 (exercised by `duckdb_parquet_export`), convert values within
BIGINT's range with:

```sql
SELECT seq, make_timestamp_ns(local_ts_ns::BIGINT) AS local_time
FROM read_parquet('events.parquet');
```

### Market demo decoder (`export --decode market-demo`)

Decoding is opt-in for JSONL and Parquet and never changes the original payload. Parquet appends these flat
columns in order, all optional:

```text
optional binary kind (UTF8);
optional int32 instrument_id (INTEGER(32,false));
optional binary side (UTF8);
optional double price;
optional double qty;
optional int64 trade_id (INTEGER(64,false));
optional binary action (UTF8);
```

JSONL appends a `"decoded"` key after `payload_b64`: either `null` or an object with keys `kind`,
`instrument_id`, `side`, `price`, `qty`, `trade_id`, `action` in that order. For example:

```json
{"kind":"trade","instrument_id":7,"side":"buy","price":123.5,"qty":2.0,"trade_id":42,"action":null}
```

`kind` is `trade` or `book`. Trades have `buy`/`sell` sides, a u64 `trade_id`, and null `action`. Book updates
have `bid`/`ask` sides, `upsert`/`delete` actions, and null `trade_id`. Unknown schema ids, wrong payload lengths,
unknown side/action values, or nonzero reserved bytes are not decodable: the JSON object and all decoded Parquet
columns are null. These records still export successfully.

The producer layout is little-endian:

| Schema id | Length | Fields at byte offsets |
| --- | --- | --- |
| 1 (trade) | 32 bytes | instrument_id u32 @0; side u8 @4 (1 buy, 2 sell); three zero bytes @5; price i64 @8; qty i64 @16; trade_id u64 @24 |
| 2 (book) | 24 bytes | instrument_id u32 @0; side u8 @4 (1 bid, 2 ask); action u8 @5 (1 upsert, 2 delete); two zero bytes @6; price i64 @8; qty i64 @16 |

Price and quantity are signed fixed-point integers (value × 10^8); decoded doubles are `raw as f64 / 1e8`.
Large fixed-point integers can lose precision in the decoded doubles; raw payload bytes remain lossless.
