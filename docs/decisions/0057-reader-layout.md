# 0057: The layout of `Reader`

- **Status:** proposed
- **Date:** 2026-11-26
- **Relates to:** issue #86; found in PR #85 (ADR [0050](0050-empty-slot-in-history.md)); the per-record path's
  return values (`SlotOutcome` in `reader.rs`, issue #16); measured in `docs/benchmarks.md` (2026-11-26, the layout of
  `Reader`)

**In short.** The record path of both readers stores and loads the struct `Found` inside `Reader` 16 bytes at a time.
When one of those accesses straddles a page boundary, replay gets 15 to 30 % slower, on Linux x86-64 and aarch64
alike, with the same binary: it depends only on where the `Reader` is in memory. In 1.0.0 and on `main`, `Found` sits at
offset 400 of an 8-byte aligned struct, so a reader at one of three addresses per page is slow; about one process in
200 of the benchmarks below hit one by chance. `Reader` is now `#[repr(C, align(16))]`, in the order rustc had chosen,
so that `found` stays at 400 (a multiple of 16) and the record path compiles as before; with that alignment none of
these accesses can straddle a page. A test pins the alignment and the offsets of every field the record path uses, and
new fields go at the end. Grouping those fields at the start, as issue #86 proposed, made the zero-copy reader 2 to 3 %
slower on x86-64 and was dropped. The slowdown that PR #85 saw on an Apple M4 Pro (twice the time after a 40-byte
change) did not reproduce on Linux with any layout, and was not measured again on the Mac: that it is the same
mechanism is likely and not shown.

## Context

### What was known

While fixing #79, PR #85 added a 40-byte field to `Reader` (`segments_listed: Option<Listed>`, commit `07a0ac09c2`).
The zero-copy reader then replayed 64-byte records in twice the time on a busy Apple M4 Pro (`cargo bench -p
tickertape-bench --bench replay`, three interleaved rounds, informal), and so did 40 or 48 bytes of padding added to
`main`; 8 to 32 or 64 bytes did not. The per-record code was the same apart from field offsets. The PR boxed the field
(`47b7d8a1c0`), which avoided the slowdown without explaining it, and every later change to `reader.rs` was asked to
keep `size_of::<Reader>()` and the per-record assembly unchanged. A reviewer ruled out a cache-line split of `found`
(at offset 400 in both builds) and suspected code alignment. Earlier, #72 (a new module) and #94 (the writer) had
shown that unrelated code changes move these paths by a few per cent.

### Measurements (2026-11-26)

Google Cloud `c4-standard-4-lssd` (Intel Xeon 6985P-C) and `c4a-standard-4-lssd` (Neoverse-V2), Ubuntu 24.04, kernel
7.0.0-1017-gcp, rustc 1.100.0, release builds as in the repository, hardware counters through the VMs' PMU. The
repository's `replay` benchmark (criterion) and a probe that replays the same journals (1,000,000 records of 64 B,
100,000 of 1,024 B) with the `Reader` either on the stack, as the benchmark has it, or at a chosen address modulo 4096.
Builds interleaved in rotating order; the noise floor is the same binary under a second name. The commands, layouts and
all figures are in `docs/benchmarks.md`.

1. **The #85 experiment does not reproduce on Linux.** 26 builds of `main` (`b88c6d5f`): padding of 8 to 64 bytes left
   to rustc (it puts it at the end), the same at 8-byte alignment, `#[repr(C)]` with rustc's order and 8 to 64 bytes
   before every field or after every field, PR #85's own two commits, and four of them with every function and block
   aligned (`-C llvm-args=-align-all-functions=6 -C llvm-args=-align-all-nofallthru-blocks=5`). With the reader on the
   stack, every build replays within −2.1 to +2.3 % of `main` on x86-64 and −2.1 to +2.6 % on aarch64, for both readers
   and both sizes (the build with rustc's order under `#[repr(C)]`, the same layout, at −1.7 to +0.5 %): no step, no
   period, nothing that follows the code. The criterion benchmark agrees (within 2 %).
2. **It depends on the address, and only at a page boundary.** With the reader at 80 addresses (every 8 bytes in a
   cache line, every 64 bytes in a page), one build was reproducibly slower at one address on both machines: 40 bytes
   before every field, at 3648 modulo 4096 (x86-64 +22 %, aarch64 +13 %). There the 4 KiB boundary falls 8 bytes into
   `found`. A second sweep put the boundary at every offset of the struct, in steps of 8, for `main` (`42b2307e`,
   `found` at 400):

   | Boundary at | x86-64 mapped / safe 64 B | aarch64 mapped / safe 64 B |
   |---|---|---|
   | `found` + 8 | +14 to +24 % / +26 to +30 % | +15 to +16 % / +15 to +21 % |
   | `found` + 24 | +8 to +17 % / +11 to +16 % | none |
   | `found` + 40 | none | +16 % / +16 % |
   | any other offset | within noise | within noise |

   The build with 40 bytes before every field has the same slow offsets, 40 bytes later: the effect moves with `found`,
   not with the address or the size of the struct. A cache-line split costs nothing measurable (`found` + 8 at a line
   boundary inside a page, and `found` + 24 on aarch64, are not slow).
3. **The counters name the access.** On aarch64, `st_align_lat` (stores that pay for crossing a boundary) is 0.00 per
   record at every other offset and 1.0 at the slow ones, and `ld_align_lat` rises by 1. On x86-64 a boundary at `found` + 8 adds
   exactly one blocked store-to-load forward per record (`ld_blocks.store_forward` 7.94 → 8.98); at `found` + 24 none of
   the measured counters moves, and the line-split counters (`mem_inst_retired.split_*`) do not change at either. The disassembly shows why: the record path writes
   `found.header` with two 16-byte stores (`movups`/`stp q`), aarch64 also writes `payload_at` and `payload_len` as one
   16-byte pair, and `try_next` reads the header back with 16-byte loads.
4. **The decisive experiment.** Same binary, reader moved by 8 bytes (boundary at `found` + 0 → `found` + 8): fast →
   slow, on both machines. Same layout, alignment 16 → 8: no slow offset → slow at `found` + 8 again (x86-64 +16 % /
   +27 %, aarch64 +16 % / +17 %), with the counters back. The branch's layout (`found` at 0, `align(16)`) has no offset
   that is slow in both rounds, and `st_align_lat` is 0 at all of them.
5. **In practice.** On Linux the initial stack pointer is randomised within a page, so a reader on the stack lands at a
   new offset in each process. Of 1,802 processes of the runs above with `main`'s layout, 9 had a boundary at a hot
   offset of `found`; those at `found` + 8 replayed 64-byte records 14 to 21 % slower than the median of their build
   (1,024-byte records 3 to 16 %). A boxed reader (the C ABI, Python) lands wherever the allocator puts it, with the
   same odds.

**Also ruled out on these machines:** 4K aliasing (on x86-64, `ld_blocks.address_alias` reaches 7.3 per record at some
addresses with no change in time); the field offset as such (moving `found` by 8 to 64 bytes changes nothing unless a
boundary falls into it); code and loop alignment (the aligned builds are within noise, and the slowdown follows the
address with identical code); the struct's size (padding after every field changes nothing); a stack against a boxed
reader (the same at the same address offset).

### Not shown: the Apple M4 Pro

The Mac was not measured (another agent was taking timings on it). If the M4's stack offset within a page is the same
in each run of a binary, which the steady figures of #85 suggest, a change of `Reader`'s size moves the reader in the
benchmark's frame by the same amount every time, and its 16 KiB pages put a boundary at one of the slow offsets for some
sizes and not for others. Padding of 40 and 48 bytes moving the reader to the same 16-byte slot, and 32 and 64 not, would
fit. The size of the effect (twice the time, against 15 to 30 % here) does not follow from anything measured here.
What would settle it: on an idle M4, the probe's page sweep with 16 KiB placements (`16384 - j`) for `main` and for this
layout, and the #85 commits under the `replay` benchmark with the reader's address printed.

## Options considered

1. **Leave the layout to rustc and keep the rule "do not change `size_of::<Reader>()`".** Does not help: the slow
   addresses exist at every size, and the rule only kept the M4's one benchmark binary where it was.
2. **A hot part first: `#[repr(C)]` with the record path's fields at the start and `found` at offset 0**, as issue #86
   proposed, with or without `align(16)`. Measured on x86-64 against `main`, 30 to 40 interleaved rounds: the zero-copy
   reader replays 64-byte records 1.3 to 3.2 % slower (slower in 19 of 30 to 37 of 40 rounds; the copy of `main`: 0.3 % faster), with `found` at 0 and also with the
   scalar fields first and `found` at 224, with and without the alignment, and with every function and block aligned
   (so it is not where the code landed). The other three cases are level. The record path's instructions are the same
   apart from offsets (several fit a one-byte displacement now). Why this order is slower was not found. On aarch64 the
   first of these orders was 2.3 % faster.
3. **rustc's order, written out, with `#[repr(C, align(16))]`.** A page boundary is a multiple of 16, so a 16-byte
   access at a multiple of 16 from a 16-byte aligned struct never straddles one, wherever the reader is; `found` is at
   400 already. Every offset stays as it is, so the record path compiles as on `main`, and `main`'s page sweep showed
   no slow offset outside `found`. Costs 8 bytes of padding (456 → 464), and callers keep the reader in 16-byte
   aligned slots, which the x86-64 and aarch64 stacks and every allocator already give.
4. **Align to 64 (a cache line).** Would also rule out line splits, which cost nothing measurable here, and makes every
   `Reader` larger. An alignment above 16 is also more than Python's allocator guarantees for the objects that hold a
   reader in the Python binding (`Mutex<Option<State>>` inside a `#[pyclass]`). Not needed.
5. **Change the record path to avoid 16-byte accesses to `found`.** The compiler chooses them; there is no stable way to
   forbid them without `unsafe` or volatile accesses, and they are cheap when aligned.

## Decision

Option 3. `Reader` is `#[repr(C, align(16))]` with its fields in the order rustc chose for `main` at `42b2307e`, and
`OpenSegment` (inside it) and `Found` are `#[repr(C)]` in theirs. The fields the per-record path of either reader reads
or writes are marked "Hot" in `reader.rs`, at these offsets: `seg.size` 56, `seg.map` 80, `buf` 104,
`config.durable_only` 280, `waiting_since` 304, `offset` 328, `next_seq` 336, `durable_next_seq` 344, `bound` 368,
`skip_until` 376, `buf_start` 392, `found` 400 (`header` at 400, `payload_at` 432, `payload_len` 440). The other fields
(directory, the open segment's path, file and identity, the waker, the boxed `Waits`, the durable marker position, seek
state, two flags) sit between them at fixed offsets. `size_of::<Reader>()` is 464 bytes (456 before); `Reader` is opaque
in Rust and handed out by pointer in the C ABI, so neither its size nor its alignment is part of any interface.

**The rule** (in `reader.rs` at the struct and in `CONTRIBUTING.md`): new fields go at the end of `Reader` (after
`used_index`), where they move nothing, or behind a pointer. Moving a hot field, or adding a field the record path
touches, needs the `replay` benchmark before and after on x86-64 and aarch64 and an update of this record and of the
test; `found` stays at a multiple of 16 and the struct 16-byte aligned. This is a measured rule, not a model: the
reason one order is slower on x86-64 than another is not known.

**The guard.** `reader::tests::reader_layout` (64-bit Linux and macOS, where the sizes of `PathBuf`, `Vec` and `Instant`
are known) asserts the alignment, the offsets above, that `found` is at a multiple of 16, and the sizes of `Found` and
`OpenSegment`; its message names this record. It runs in CI with the other unit tests. The release checklist runs the
`replay` benchmark against the previous release on both architectures (interleaved, with a noise floor), which is what
can catch a slowdown the test cannot. No timing test runs in CI.

### Before and after

`main` (`42b2307e`) against this layout on the same base, on the machines above (aarch64 on a second VM of the same
type). Probe: 30 interleaved rounds, reader on the stack, two processes per round for the 64-byte cases; the figure is
the median of the per-round ratios against `main`, and the second copy of `main` (the same binary) is the noise floor.
Criterion: 6 interleaved rounds of the `replay` benchmark, builds without the probe.

| Probe, ratio to `main` | x86-64: this layout | x86-64: copy of `main` | aarch64: this layout | aarch64: copy of `main` |
|---|---|---|---|---|
| mapped, 64 B | 0.993 | 0.998 | 0.972 | 0.999 |
| safe, 64 B | 0.992 | 0.994 | 0.995 | 1.003 |
| mapped, 1,024 B | 0.978 | 1.001 | 0.993 | 1.001 |
| safe, 1,024 B | 0.993 | 1.003 | 0.997 | 1.007 |

| Criterion, median ms (`main` / its copy / this layout) | x86-64 | aarch64 |
|---|---|---|
| `replay/records/mapped/64` | 49.44 / 47.48 / 47.27 | 36.53 / 36.55 / 36.32 |
| `replay/records/safe/64` | 36.17 / 37.31 / 36.97 | 38.52 / 38.66 / 38.78 |
| `replay/records/mapped/1024` | 10.41 / 10.05 / 10.23 | 10.56 / 10.63 / 10.61 |
| `replay/records/safe/1024` | 13.01 / 12.71 / 12.55 | 15.04 / 15.13 / 14.98 |

The page sweep of this layout (a boundary at every 16th offset) has no slow position on either machine, and
`st_align_lat` is 0 at all of them; `main`, swept again in the same sessions, is slow at `found` + 8 and + 24 (x86-64)
and + 8 and + 40 (aarch64) as before. The record path compiles to the same instructions as on `main`: on aarch64 the
three functions are identical apart from addresses, on x86-64 three unaligned 16-byte moves of `found` became aligned
ones (`movups` → `movaps`).

**Padding at the end of the struct** (where new fields go): 64 bytes changes nothing on either machine (−0.3 to
+1.3 %); 40 bytes changes nothing on aarch64 (−0.4 to +0.2 %), but on x86-64 makes the zero-copy reader replay 64-byte
records 1.9 to 3.9 % slower than this layout (slower in 23 and 28 of 30 rounds in two sessions; also with every function
and block aligned, 2.1 %), while the other three cases stay within the noise. No hot offset moves there, the record
path is the same code, and why the size does this was not found. So adding a field at the end is not free of risk on
x86-64 at the scale of a few per cent; the release checklist's benchmark is what catches that.

## Consequences

- No reader is 15 to 30 % slower by the accident of its address any more, on Linux x86-64 and aarch64. Wider accesses
  the compiler may emit later to hot fields are covered while they stay within a 16-byte block of the struct; the test
  cannot see instructions, which is why the checklist keeps the benchmark.
- The old rule "do not change `size_of::<Reader>()`" becomes "add at the end, or behind a pointer, and run the
  benchmark". `Waits` stays boxed.
- **Not measured:** the Apple M4 Pro (above); other x86-64 and Arm microarchitectures; readers held by the bindings
  (their `Reader` is boxed or inside a Python object, with the same odds as any heap address, and now 16-byte aligned
  there); 32-bit targets (the test does not run there); `try_next` called through the C ABI. Not explained: why the
  hot-fields-first orders are slower on x86-64 (option 2), and why 40 bytes at the end are. An unrelated finding: on
  x86-64 every layout has about 8 blocked store-to-load forwards per record in the zero-copy reader's path (4 in the
  safe reader's), from values the record path copies through the stack; that is the kind of cost issue #16 removed
  once, and is left for another change.
