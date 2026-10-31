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

    // `Fail` refuses a disconnected segment, but only after the scan zeroed the never-committed bytes of the first
    // segment (every policy does that), so a refused open is not byte-for-byte a no-op.
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

    for (name, bytes) in seeds {
        fs::write(dir.join(name), bytes).unwrap();
    }
}
