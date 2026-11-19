//! Writes the seed inputs of the `recover_dir` target to `seeds/recover_dir/` (or to the directory given as the
//! first argument). The input grammar is documented in `fuzz_targets/recover_dir.rs`.
//!
//! ```text
//! cargo run --example gen_recover_seeds
//! ```
//!
//! Check what a seed does with `RECOVER_DIR_TRACE=1 cargo +nightly fuzz run recover_dir seeds/recover_dir/<name>`.

use std::fs;
use std::path::PathBuf;

const QUARANTINE: u8 = 0xff;
const FAIL: u8 = 0x00;

/// Offset selectors of the grammar.
const AT_CURSOR: u8 = 2;
const AT_LAST_RECORD: u8 = 3;
const AT_FIRST_RECORD: u8 = 1;
/// Followed by a `u16` offset.
const ABSOLUTE: u8 = 6;

#[derive(Default)]
struct Ops(Vec<u8>, u8);

impl Ops {
    fn op(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.extend_from_slice(bytes);
        self.1 += 1;
        self
    }

    /// `count` valid records of `plen` bytes of payload.
    fn records(&mut self, count: u8, plen: u16) -> &mut Self {
        let [lo, hi] = plen.to_le_bytes();
        self.op(&[0, count, lo, hi])
    }

    /// A seal whose target is the next segment's base.
    fn seal_to_next(&mut self) -> &mut Self {
        self.op(&[1, 1])
    }

    /// `bytes` at `sel` + `delta`.
    fn write(&mut self, sel: u8, delta: i8, bytes: &[u8]) -> &mut Self {
        let mut op = vec![2, sel, delta as u8, bytes.len() as u8];
        op.extend_from_slice(bytes);
        self.op(&op)
    }

    /// Zeroes `len` bytes at `sel` + `delta` (`sel` one of the named selectors).
    fn zero(&mut self, sel: u8, delta: i8, len: u16) -> &mut Self {
        let [lo, hi] = len.to_le_bytes();
        self.op(&[3, sel, delta as u8, lo, hi])
    }

    /// Zeroes `len` bytes at the absolute offset `at`.
    fn zero_at(&mut self, at: u16, len: u16) -> &mut Self {
        let [a, b] = at.to_le_bytes();
        let [lo, hi] = len.to_le_bytes();
        self.op(&[3, ABSOLUTE, a, b, 0, lo, hi])
    }

    /// Copies `len` bytes from `from` + `from_delta` to `to` + `to_delta` (named selectors).
    fn copy(&mut self, from: u8, from_delta: i8, to: u8, to_delta: i8, len: u16) -> &mut Self {
        let [lo, hi] = len.to_le_bytes();
        self.op(&[9, from, from_delta as u8, to, to_delta as u8, lo, hi])
    }

    fn flip(&mut self, sel: u8, delta: i8, bit: u8) -> &mut Self {
        self.op(&[5, sel, delta as u8, bit])
    }
}

/// A segment that continues after the previous one (or starts at 0).
fn continuing(ops: &Ops) -> Vec<u8> {
    let mut out = vec![0, ops.1];
    out.extend_from_slice(&ops.0);
    out
}

/// A segment with an absolute base.
fn absolute(base: u16, ops: &Ops) -> Vec<u8> {
    let mut out = vec![2];
    out.extend_from_slice(&base.to_le_bytes());
    out.push(ops.1);
    out.extend_from_slice(&ops.0);
    out
}

fn input(policy: u8, segments: &[Vec<u8>], extras: u8, tail: &[u8]) -> Vec<u8> {
    let mut out = vec![policy, segments.len() as u8 - 1];
    for s in segments {
        out.extend_from_slice(s);
    }
    out.push(extras);
    out.extend_from_slice(tail);
    out
}

fn main() {
    let dir = std::env::args_os().nth(1).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seeds/recover_dir"),
        PathBuf::from,
    );
    fs::create_dir_all(&dir).unwrap();
    let mut seeds: Vec<(&str, Vec<u8>)> = Vec::new();

    // A healthy journal: a full, sealed first segment and a second with records.
    let mut full = Ops::default();
    full.records(255, 600).seal_to_next();
    let mut second = Ops::default();
    second.records(20, 50);
    seeds.push((
        "clean_two_segments",
        input(
            QUARANTINE,
            &[continuing(&full), continuing(&second)],
            0,
            &[],
        ),
    ));

    // tests/recovery.rs::torn_final_record: a body written after the last record but never committed.
    let mut torn = Ops::default();
    torn.records(25, 100).write(AT_CURSOR, 4, &[0xab; 48]);
    seeds.push(("torn_tail", input(QUARANTINE, &[continuing(&torn)], 0, &[])));

    // tests/recovery.rs::malformed_checksum_is_quarantined_and_repaired: a payload byte of the last record flipped.
    let mut bad_crc = Ops::default();
    bad_crc.records(10, 100).flip(AT_LAST_RECORD, 41, 0);
    for (name, policy) in [("bad_crc", QUARANTINE), ("bad_crc_fail", FAIL)] {
        seeds.push((name, input(policy, &[continuing(&bad_crc)], 0, &[])));
    }

    // tests/recovery.rs::malformed_length_is_rejected: a commit word that points past the end of the segment.
    let mut bad_len = Ops::default();
    bad_len
        .records(4, 100)
        .write(AT_CURSOR, 0, &(u32::MAX - 1).to_le_bytes());
    seeds.push((
        "bad_length",
        input(QUARANTINE, &[continuing(&bad_len)], 0, &[]),
    ));

    // tests/recovery.rs::crash_between_link_and_seal: the next segment exists but the first was never sealed.
    let mut twelve = Ops::default();
    twelve.records(12, 40);
    seeds.push((
        "crash_between_link_and_seal",
        input(
            QUARANTINE,
            &[continuing(&twelve), continuing(&Ops::default())],
            0,
            &[],
        ),
    ));

    // A seal at the end of the newest segment whose successor is missing (crash during a roll).
    let mut sealed_last = Ops::default();
    sealed_last.records(5, 40).op(&[1, 0]);
    seeds.push((
        "sealed_without_successor",
        input(QUARANTINE, &[continuing(&sealed_last)], 0, &[]),
    ));

    // tests/recovery.rs::disconnected_segment_is_quarantined: a segment far ahead of the chain.
    let mut eight = Ops::default();
    eight.records(8, 40);
    for (name, policy) in [("disconnected", QUARANTINE), ("disconnected_fail", FAIL)] {
        seeds.push((
            name,
            input(
                policy,
                &[continuing(&eight), absolute(500, &Ops::default())],
                0,
                &[],
            ),
        ));
    }

    // `Fail` refuses a disconnected segment. Its base is above the first segment's next sequence number, which only
    // a committed record there could have produced, so the non-zero bytes after the first segment's records count as
    // committed history (ADR 0035) and the refusal comes before anything is zeroed.
    seeds.push((
        "disconnected_after_torn_tail_fail",
        input(
            FAIL,
            &[continuing(&torn), absolute(500, &Ops::default())],
            0,
            &[],
        ),
    ));

    // tests/recovery.rs::fail_policy_refuses_invalid_successor_header: the first segment is sealed and names a
    // successor whose header is invalid (the magic is damaged). `Quarantine` moves the successor away; `Fail` must
    // refuse and leave it where it is.
    let mut three = Ops::default();
    three.records(3, 40).seal_to_next();
    let mut bad_magic = Ops::default();
    bad_magic.flip(0, 0, 0);
    for (name, policy) in [
        ("invalid_successor_header", QUARANTINE),
        ("invalid_successor_header_fail", FAIL),
    ] {
        seeds.push((
            name,
            input(
                policy,
                &[continuing(&three), continuing(&bad_magic)],
                0,
                &[],
            ),
        ));
    }

    // tests/recovery.rs::dangling_checkpoint_is_refused: a valid slot naming a segment that does not exist.
    //   kind0 = 2 (raw fields), kind1 = 0; len = 0; slot 0: gen 1, seg 200 (no such segment) -> base 9999,
    //   next_seq 5, offset 64.
    let mut five = Ops::default();
    five.records(5, 40);
    let mut dangling = vec![2, 0, 0, 1, 200];
    dangling.extend_from_slice(&9999u16.to_le_bytes());
    dangling.extend_from_slice(&5u16.to_le_bytes());
    dangling.extend_from_slice(&64u16.to_le_bytes());
    seeds.push((
        "dangling_checkpoint",
        input(QUARANTINE, &[continuing(&five)], 1, &dangling),
    ));

    // tests/recovery.rs::checkpoint_marker_bounds_recovery: a checkpoint at the end of the second segment's records
    // (position index 0 is the last position written), followed by a torn tail after it.
    let mut after = Ops::default();
    after.records(20, 50).write(AT_CURSOR, 4, &[0xcd; 30]);
    let mut ckpt = vec![1, 0, 0];
    ckpt.extend_from_slice(&0u16.to_le_bytes());
    ckpt.push(3);
    seeds.push((
        "checkpoint_then_torn_tail",
        input(
            QUARANTINE,
            &[continuing(&full), continuing(&after)],
            1,
            &ckpt,
        ),
    ));

    // A checkpoint inside a record (position index 3, plus 4 bytes via a raw slot): the marker vouches for nothing.
    let mut forged = vec![2, 0, 0, 7, 0];
    forged.extend_from_slice(&3u16.to_le_bytes());
    forged.extend_from_slice(&100u16.to_le_bytes());
    seeds.push((
        "checkpoint_forged_offset",
        input(QUARANTINE, &[continuing(&five)], 1, &forged),
    ));

    // A corrupt record in the first segment below a checkpoint in the second: trusted, not re-read.
    let mut below = Ops::default();
    below
        .records(255, 600)
        .flip(AT_LAST_RECORD, 45, 3)
        .seal_to_next();
    let mut ckpt2 = vec![1, 0, 0];
    ckpt2.extend_from_slice(&0u16.to_le_bytes());
    ckpt2.push(1);
    seeds.push((
        "corrupt_below_checkpoint",
        input(
            QUARANTINE,
            &[continuing(&below), continuing(&second)],
            1,
            &ckpt2,
        ),
    ));

    // A stale temporary file, a pre-existing quarantine directory with two files, and a corrupt tail.
    //   extras: bit 1 quarantine dir, bits 3-4 = 2 files, bits 5-6 = 1 tmp file.
    let tmp = [0u8, 7];
    seeds.push((
        "tmp_and_existing_quarantine",
        input(QUARANTINE, &[continuing(&bad_crc)], 0b0011_0010, &tmp),
    ));

    // A header that names a different base than its file, then a header with the wrong size.
    let mut mismatch = Ops::default();
    mismatch.records(3, 40).op(&[6, 5, 0, 0]);
    seeds.push((
        "header_base_mismatch",
        input(
            QUARANTINE,
            &[continuing(&eight), continuing(&mismatch)],
            0,
            &[],
        ),
    ));

    // ADR 0035: storage damage zeroes the commit word of a record that has committed records after it (record 10
    // of 30, 100-byte payloads: 144-byte frames from offset 64). Recovery must preserve the tail, or refuse.
    let mut zeroed_word = Ops::default();
    zeroed_word.records(30, 100).zero_at(64 + 144 * 10, 4);
    for (name, policy) in [
        ("zeroed_commit_word", QUARANTINE),
        ("zeroed_commit_word_fail", FAIL),
    ] {
        seeds.push((name, input(policy, &[continuing(&zeroed_word)], 0, &[])));
    }

    // The same in a sealed first segment (record 50 of its 640-byte frames), with a second segment after it.
    let mut zeroed_sealed = Ops::default();
    zeroed_sealed
        .records(255, 600)
        .zero_at(64 + 640 * 50, 4)
        .seal_to_next();
    seeds.push((
        "zeroed_commit_word_sealed",
        input(
            QUARANTINE,
            &[continuing(&zeroed_sealed), continuing(&second)],
            0,
            &[],
        ),
    ));

    // The last record before a seal: the seal (and the successor) are the only evidence.
    let mut zeroed_last = Ops::default();
    zeroed_last
        .records(5, 40)
        .zero(AT_LAST_RECORD, 0, 4)
        .seal_to_next();
    for (name, policy) in [
        ("zeroed_commit_word_before_seal", QUARANTINE),
        ("zeroed_commit_word_before_seal_fail", FAIL),
    ] {
        seeds.push((
            name,
            input(
                policy,
                &[continuing(&zeroed_last), continuing(&three)],
                0,
                &[],
            ),
        ));
    }

    // A zeroed 600-byte block (lost write) covering several records, with intact records after it.
    let mut zeroed_block = Ops::default();
    zeroed_block.records(40, 100).zero_at(64 + 144 * 10, 600);
    seeds.push((
        "zeroed_block",
        input(QUARANTINE, &[continuing(&zeroed_block)], 0, &[]),
    ));

    // A torn append whose bytes hold a stale copy of the first record's frame: a valid frame, but with a sequence
    // number its position rules out, so it is still a torn append (zeroed in place, nothing quarantined).
    let mut torn_stale = Ops::default();
    torn_stale
        .records(25, 100)
        .write(AT_CURSOR, 4, &[0xab; 40])
        .copy(AT_FIRST_RECORD, 0, AT_CURSOR, 48, 144);
    seeds.push((
        "torn_tail_with_stale_frame",
        input(FAIL, &[continuing(&torn_stale)], 0, &[]),
    ));

    // Review round 1, item 2: the seal's marker zeroed (its CRC word intact) with two segments after it. The same
    // bytes as a crash between publishing the successor and sealing: the seal is written again, nothing is removed.
    let mut marker_zeroed = Ops::default();
    marker_zeroed
        .records(5, 40)
        .op(&[1, 0])
        .zero(AT_CURSOR, -8, 4);
    for (name, policy) in [
        ("seal_marker_zeroed", QUARANTINE),
        ("seal_marker_zeroed_fail", FAIL),
    ] {
        seeds.push((
            name,
            input(
                policy,
                &[
                    continuing(&marker_zeroed),
                    continuing(&three),
                    continuing(&eight),
                ],
                0,
                &[],
            ),
        ));
    }

    // Review round 1, item 3: a torn append of record 25 whose payload holds a frame with sequence number 26 (a copy
    // of record 0's 140-byte frame, renumbered and re-checksummed). With the torn frame's body complete (its CRC
    // stored for a 188-byte frame, commit word never stored) the embedded frame lies inside it: a torn append, zeroed
    // in place. Without the CRC (torn mid-body) it is the accepted false positive: preserved, or refused by `Fail`.
    let mut embedding = Ops::default();
    embedding
        .records(25, 100)
        .copy(AT_FIRST_RECORD, 0, AT_CURSOR, 48, 140)
        .write(AT_CURSOR, 56, &26u64.to_le_bytes())
        .op(&[8, AT_CURSOR, 48])
        .write(AT_CURSOR, 0, &188u32.to_le_bytes())
        .write(AT_CURSOR, 8, &25u64.to_le_bytes());
    let mut complete = Ops(embedding.0.clone(), embedding.1);
    complete.op(&[8, AT_CURSOR, 0]).zero(AT_CURSOR, 0, 4);
    embedding.zero(AT_CURSOR, 0, 4);
    for (name, ops, policy) in [
        ("torn_complete_body_holding_a_frame_fail", &complete, FAIL),
        ("torn_body_holding_a_frame", &embedding, QUARANTINE),
        ("torn_body_holding_a_frame_fail", &embedding, FAIL),
    ] {
        seeds.push((name, input(policy, &[continuing(ops)], 0, &[])));
    }

    for (name, bytes) in seeds {
        fs::write(dir.join(name), bytes).unwrap();
    }
}
