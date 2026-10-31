//! Crash recovery on real journal directories.
//!
//! The input is a compact description of a journal directory (grammar below). The target builds that directory in a
//! fresh temporary directory, runs `Writer::open` on it and checks the invariants of `docs/format.md` → Recovery:
//!
//! 1. `Writer::open` never panics (an error is fine).
//! 2. If it succeeds: `inspect::verify` reports no problems, a safe `Reader` reads a gap-free run of records that
//!    ends exactly at `Writer::next_seq`, and appending one record works and is readable.
//! 3. Recovery is idempotent: opening again succeeds and does nothing.
//! 4. Nothing is deleted except stale `*.tape.tmp` files: every repair preserved the bytes it removed in
//!    `quarantine/`, one file per `RepairedTail` / `QuarantinedSegment` action.
//! 5. With `RecoveryPolicy::Fail`, a refused `open` leaves every committed byte alone (it may have zeroed
//!    never-committed bytes, sealed a segment and created a missing successor, as every policy does), and a successful one never quarantines.
//!
//! Records before a checkpoint marker are trusted, not re-read (format.md, step 2). So when a marker is present,
//! problems that `verify` finds inside that trusted region are not findings, and a marker whose segment does not
//! hold valid records up to the position it names (forged, or damaged by the generator afterwards) only gets the
//! checks that do not depend on the marker.
//!
//! # Input grammar
//!
//! Integers are little-endian; reading past the end of the input yields zeros. Offsets are bytes in the segment
//! file; every segment is 65,536 bytes before its operations run.
//!
//! ```text
//! input   = policy:u8 nseg:u8 segment{1 + nseg % 4} extras:u8 [checkpoint] [tmp{n}]
//! policy  = Fail if < 64, else Quarantine
//! segment = base_mode:u8 [base arg] nops:u8 op{nops % 17}
//!           base_mode % 4: 0 = continue after the previous segment's records (0 for the first)
//!                          1 = continue, plus a gap: u16     2 = absolute: u16     3 = absolute: u64
//! op      = opcode:u8 args           (opcode % 10)
//!   0 records  count:u8 (0 = 1) plen:u16     `count` valid records at the cursor, stops when full.
//!                                            plen bit 15 set: (plen & 0x7fff) % 20000 bytes, else plen % 700
//!   1 seal     target:u8 [u16]               valid seal at the cursor. target % 4: 0 = next record's sequence
//!                                            number, 1 = the next segment's base, 2 = next + 1 + u8, 3 = u16
//!   2 write    off len:u8 bytes              len % 49 input bytes at `off`
//!   3 zero     off len:u16
//!   4 resize   kind:u8 n:u16                 kind & 1 = 0: truncate to n bytes; 1: extend to 65,536 + n
//!   5 flip     off bit:u8
//!   6 header   delta:u8 size:u8 created:u8   re-encode a valid header: base + delta (signed), size 65,536 or
//!                                            131,072 (size & 1), so a file-size mismatch is possible
//!   7 fixcrc                                 recompute the header CRC
//!   8 recrc    off                           recompute the CRC of the record at `off` (forges bad seq or flags)
//!   9 copy     from to len:u16               copy `len % 2048` bytes, e.g. a stale record frame into a new place
//! off     = sel:u8 delta:u8 [u16]            sel: 0 = 0, 1 = 64, 2 = cursor, 3 = last record, 4 = size - 8,
//!                                            5 = cursor + 8, otherwise absolute u16; then + delta (signed)
//! extras  = bit 0 checkpoint file, bit 1 `quarantine/` dir (bit 2: a regular file instead), bits 3-4 number of
//!           pre-existing `quarantine/pre-N.bin` files, bits 5-6 number of stray `*.tape.tmp` files
//! checkpoint = kind0:u8 kind1:u8 len:u8 slot{2}
//!           len % 4: 0 = 131,072 bytes, 1 = 64, 2 = 65,600, 3 = 100
//!           kind % 4: 0 = zeros, 1 = valid slot at a position the generator wrote (pos:u16, 0 = the last one written, counting
//!                         back; gen:u8),
//!                     2 = valid slot with raw fields (gen:u8 seg:u8 next_seq:u16 offset:u16;
//!                         seg < nseg names that segment's base, otherwise base:u16),
//!                     3 = 64 bytes of input; kind bit 2 flips a bit in the slot's CRC
//! tmp     = sel:u8 [base:u16] len:u8         file `{base}.tape.tmp` (len % 16 bytes); sel < nseg = that
//!                                            segment's base, otherwise base:u16
//! ```

#![no_main]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use libfuzzer_sys::fuzz_target;
use tickertape::format::{self, CheckpointSlot, RecordHeader, SegmentHeader};
use tickertape::inspect::{self, Problem};
use tickertape::{
    CorruptReason, NewRecord, OwnedRecord, Reader, ReaderConfig, RecoveryAction, RecoveryPolicy,
    Writer, WriterConfig,
};

const SEG: usize = 65_536;

struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Input<'a> {
    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos = self.pos.saturating_add(1);
        b
    }

    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }

    fn u64(&mut self) -> u64 {
        let mut b = [0; 8];
        b.iter_mut().for_each(|x| *x = self.u8());
        u64::from_le_bytes(b)
    }

    fn bytes(&mut self, n: usize) -> &'a [u8] {
        let start = self.pos.min(self.data.len());
        let end = start.saturating_add(n).min(self.data.len());
        self.pos = self.pos.saturating_add(n);
        &self.data[start..end]
    }
}

/// A position a checkpoint marker may legitimately name: (segment base, offset, next sequence number).
type Position = (u64, u64, u64);

/// One segment being built in memory.
struct Seg {
    base: u64,
    img: Vec<u8>,
    cursor: usize,
    seq: u64,
    last_rec: usize,
    /// Offsets of seals whose target is the next segment's base, resolved once all bases are known.
    pending_seals: Vec<usize>,
}

impl Seg {
    fn put(&mut self, at: usize, bytes: &[u8]) {
        if at < self.img.len() {
            let n = bytes.len().min(self.img.len() - at);
            self.img[at..at + n].copy_from_slice(&bytes[..n]);
        }
    }

    fn off(&self, inp: &mut Input) -> usize {
        let sel = inp.u8();
        let base: i64 = match sel {
            0 => 0,
            1 => 64,
            2 => self.cursor as i64,
            3 => self.last_rec as i64,
            4 => self.img.len() as i64 - 8,
            5 => self.cursor as i64 + 8,
            _ => i64::from(inp.u16()),
        };
        let delta = i64::from(inp.u8() as i8);
        (base + delta).clamp(0, self.img.len() as i64) as usize
    }

    fn put_seal(&mut self, at: usize, next_seq: u64) {
        self.put(at, &format::SEAL_MARKER.to_le_bytes());
        self.put(at + 4, &format::seal_crc(next_seq).to_le_bytes());
    }

    fn run_op(&mut self, inp: &mut Input, positions: &mut Vec<Position>) {
        match inp.u8() % 10 {
            0 => {
                let count = inp.u8().max(1);
                let sel = inp.u16();
                let plen = if sel & 0x8000 != 0 {
                    usize::from(sel & 0x7fff) % 20_000
                } else {
                    usize::from(sel) % 700
                };
                let payload = vec![0x5a ^ (self.seq as u8); plen];
                for _ in 0..count {
                    let Some(len) = format::frame_len(plen) else {
                        break;
                    };
                    let end = self.cursor + format::align8(u64::from(len)) as usize;
                    if end > SEG - 8 || self.seq == u64::MAX {
                        break;
                    }
                    let mut body = vec![0; len as usize - 4];
                    format::encode_record_body(
                        &RecordHeader {
                            seq: self.seq,
                            schema_id: self.seq as u32,
                            venue_ts_ns: self.seq.wrapping_mul(10),
                            local_ts_ns: self.seq.wrapping_mul(10).wrapping_add(1),
                        },
                        &payload,
                        &mut body,
                    );
                    positions.push((self.base, self.cursor as u64, self.seq));
                    self.put(self.cursor + 4, &body);
                    self.put(self.cursor, &len.to_le_bytes());
                    self.last_rec = self.cursor;
                    self.cursor = end;
                    self.seq += 1;
                }
            }
            1 => {
                let at = self.cursor;
                if at + format::SEAL_LEN <= self.img.len() {
                    match inp.u8() % 4 {
                        0 => self.put_seal(at, self.seq),
                        1 => self.pending_seals.push(at),
                        2 => {
                            let delta = u64::from(inp.u8()) + 1;
                            self.put_seal(at, self.seq.wrapping_add(delta));
                        }
                        _ => {
                            let next = u64::from(inp.u16());
                            self.put_seal(at, next);
                        }
                    }
                    positions.push((self.base, at as u64, self.seq));
                    self.cursor += format::SEAL_LEN;
                } else {
                    let _ = inp.u8();
                }
            }
            2 => {
                let at = self.off(inp);
                let n = usize::from(inp.u8()) % 49;
                let bytes = inp.bytes(n);
                self.put(at, bytes);
            }
            3 => {
                let at = self.off(inp);
                let n = usize::from(inp.u16());
                let end = at.saturating_add(n).min(self.img.len());
                self.img[at..end].fill(0);
            }
            4 => {
                let kind = inp.u8();
                let n = usize::from(inp.u16());
                if kind & 1 == 0 {
                    self.img.truncate(n);
                } else {
                    self.img.resize(SEG + n, 0);
                }
            }
            5 => {
                let at = self.off(inp);
                let bit = inp.u8() % 8;
                if let Some(b) = self.img.get_mut(at) {
                    *b ^= 1 << bit;
                }
            }
            6 => {
                let delta = i64::from(inp.u8() as i8);
                let size = if inp.u8() & 1 == 0 { 65_536 } else { 131_072 };
                let created = u64::from(inp.u8());
                let header = format::encode_segment_header(&SegmentHeader {
                    segment_size: size,
                    base_seq: self.base.wrapping_add(delta as u64),
                    created_unix_ns: created,
                });
                self.put(0, &header);
            }
            7 => {
                if self.img.len() >= format::SEGMENT_HEADER_LEN {
                    let crc = crc32c::crc32c(&self.img[..60]);
                    self.put(60, &crc.to_le_bytes());
                }
            }
            8 => {
                let at = self.off(inp);
                let len = self
                    .img
                    .get(at..at + 4)
                    .map_or(0, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
                let end = at.saturating_add(len as usize);
                if (format::RECORD_HEADER_LEN as u32..format::MAX_FRAME_LEN).contains(&len)
                    && end <= self.img.len()
                {
                    format::finish_record_body(len, &mut self.img[at + 4..end]);
                }
            }
            _ => {
                let from = self.off(inp);
                let to = self.off(inp);
                let n = usize::from(inp.u16()) % 2048;
                let n = n.min(self.img.len() - from);
                let chunk = self.img[from..from + n].to_vec();
                self.put(to, &chunk);
            }
        }
    }
}

/// What the generator built, for the checks.
struct Built {
    policy: RecoveryPolicy,
    /// `quarantine/*.bin` files that existed before `open`.
    prequarantined: usize,
    /// Segment files as written, by name.
    segments: BTreeMap<String, Vec<u8>>,
}

fn build(data: &[u8], dir: &Path) -> Built {
    let mut inp = Input { data, pos: 0 };
    let policy = if inp.u8() < 64 {
        RecoveryPolicy::Fail
    } else {
        RecoveryPolicy::Quarantine
    };
    let nseg = 1 + usize::from(inp.u8() % 4);
    let mut positions = Vec::new();
    let mut segs: Vec<Seg> = Vec::new();
    for _ in 0..nseg {
        let prev_end = segs.last().map_or(0, |s: &Seg| s.seq);
        let base = match inp.u8() % 4 {
            0 => prev_end,
            1 => prev_end.wrapping_add(u64::from(inp.u16())),
            2 => u64::from(inp.u16()),
            _ => inp.u64(),
        };
        let mut img = vec![0; SEG];
        img[..64].copy_from_slice(&format::encode_segment_header(&SegmentHeader {
            segment_size: SEG as u64,
            base_seq: base,
            created_unix_ns: 1,
        }));
        let mut seg = Seg {
            base,
            img,
            cursor: 64,
            seq: base,
            last_rec: 64,
            pending_seals: Vec::new(),
        };
        for _ in 0..inp.u8() % 17 {
            seg.run_op(&mut inp, &mut positions);
        }
        positions.push((seg.base, seg.cursor as u64, seg.seq));
        segs.push(seg);
    }
    for i in 0..segs.len() {
        let next = segs.get(i + 1).map_or(segs[i].seq, |n| n.base);
        for at in std::mem::take(&mut segs[i].pending_seals) {
            segs[i].put_seal(at, next);
        }
    }
    let bases: Vec<u64> = segs.iter().map(|s| s.base).collect();

    let mut segments = BTreeMap::new();
    for seg in &segs {
        let name = format::segment_file_name(seg.base);
        fs::write(dir.join(&name), &seg.img).unwrap();
        segments.insert(name, seg.img.clone());
    }

    let extras = inp.u8();
    if extras & 1 != 0 {
        let kinds = [inp.u8(), inp.u8()];
        let len = [131_072, 64, 65_600, 100][usize::from(inp.u8() % 4)];
        let mut file = vec![0u8; format::CHECKPOINT_FILE_LEN];
        for (kind, at) in kinds.into_iter().zip(format::CHECKPOINT_SLOT_OFFSETS) {
            let mut slot = [0u8; format::CHECKPOINT_SLOT_LEN];
            match kind % 4 {
                0 => {}
                1 => {
                    let pos = usize::from(inp.u16());
                    let generation = u64::from(inp.u8());
                    let (segment_base_seq, offset, next_seq) =
                        positions[positions.len() - 1 - pos % positions.len()];
                    slot = format::encode_checkpoint_slot(&CheckpointSlot {
                        generation,
                        next_seq,
                        segment_base_seq,
                        offset,
                    });
                }
                2 => {
                    let generation = u64::from(inp.u8());
                    let sel = usize::from(inp.u8());
                    let segment_base_seq = bases
                        .get(sel)
                        .copied()
                        .unwrap_or_else(|| u64::from(inp.u16()));
                    let next_seq = u64::from(inp.u16());
                    let offset = u64::from(inp.u16());
                    slot = format::encode_checkpoint_slot(&CheckpointSlot {
                        generation,
                        next_seq,
                        segment_base_seq,
                        offset,
                    });
                }
                _ => {
                    let raw = inp.bytes(format::CHECKPOINT_SLOT_LEN);
                    slot[..raw.len()].copy_from_slice(raw);
                }
            }
            if kind & 4 != 0 {
                slot[60] ^= 1;
            }
            file[at..at + slot.len()].copy_from_slice(&slot);
        }
        file.truncate(len);
        fs::write(dir.join("checkpoint"), file).unwrap();
    }

    let mut prequarantined = 0;
    if extras & 2 != 0 {
        if extras & 4 != 0 {
            fs::write(dir.join("quarantine"), b"not a directory").unwrap();
        } else {
            fs::create_dir(dir.join("quarantine")).unwrap();
            prequarantined = usize::from((extras >> 3) & 3);
            for n in 0..prequarantined {
                fs::write(dir.join(format!("quarantine/pre-{n}.bin")), [n as u8; 5]).unwrap();
            }
        }
    }
    for _ in 0..(extras >> 5) & 3 {
        let sel = usize::from(inp.u8());
        let base = bases
            .get(sel)
            .copied()
            .unwrap_or_else(|| u64::from(inp.u16()));
        let len = usize::from(inp.u8() % 16);
        let name = format!("{}.tmp", format::segment_file_name(base));
        fs::write(dir.join(name), vec![0xee; len]).unwrap();
    }

    Built {
        policy,
        prequarantined,
        segments,
    }
}

/// True if `img` (the segment with base `base`) holds valid records from offset 64 up to `offset`, ending at `next_seq`.
fn prefix_intact(img: &[u8], base: u64, offset: u64, next_seq: u64) -> bool {
    let mut copy = img.to_vec();
    let Some(rest) = copy.get_mut(offset as usize..) else {
        return false;
    };
    rest.fill(0);
    let scan = format::scan_segment(&copy[..], 64, base);
    scan.end_offset == offset && scan.next_seq == next_seq
}

fn segment_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if format::parse_segment_file_name(&name).is_some() {
            out.insert(name, fs::read(entry.path()).unwrap());
        }
    }
    out
}

fn count_bin(dir: &Path) -> usize {
    fs::read_dir(dir.join("quarantine")).map_or(0, |entries| {
        entries
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".bin"))
            })
            .count()
    })
}

fn base_of(path: &Path) -> Option<u64> {
    format::parse_segment_file_name(path.file_name()?.to_str()?)
}

/// What recovery trusted without re-reading.
#[derive(Clone, Copy)]
enum Trust {
    /// No marker: everything was validated.
    Nothing,
    /// A marker whose segment really holds valid records up to its offset and next sequence number: segments
    /// below `.0`, and `.0`'s bytes below `.1`, were trusted.
    Before(u64, u64),
    /// A marker that vouches for records that are not there (forged, or the generator damaged them afterwards:
    /// the storage assumptions of the crash contract are broken). Recovery trusts it, so the journal need not be
    /// consistent with it; only marker-independent checks apply.
    Vouchless,
}

/// Checks `verify` and a full read of the journal, which must end at `next_seq`.
fn check_journal(dir: &Path, trust: Trust, next_seq: u64, last: Option<&NewRecord<'_>>) {
    if matches!(trust, Trust::Vouchless) {
        return;
    }
    let report = inspect::verify(dir).expect("verify");
    let in_trusted = |p: &Problem| match (trust, base_of(&p.path)) {
        (Trust::Before(start, offset), Some(base)) => {
            base < start || (base == start && p.offset < offset)
        }
        _ => false,
    };
    // A problem inside the trusted region also breaks the chain checks of the segments after it.
    let any_trusted = report.problems.iter().any(in_trusted);
    for p in &report.problems {
        let chain = matches!(p.reason, CorruptReason::BrokenChain(_));
        assert!(
            in_trusted(p) || (any_trusted && chain),
            "verify found {p:?} after recovery ({:?})",
            report.problems
        );
    }
    if report.problems.is_empty() {
        read_chain(dir, next_seq, last);
    }
}

fn read_chain(dir: &Path, next_seq: u64, last: Option<&NewRecord<'_>>) {
    let mut reader =
        Reader::open(dir, ReaderConfig::default()).expect("reader opens after recovery");
    let mut want = reader.position();
    let mut final_record: Option<OwnedRecord> = None;
    while let Some(rec) = reader.next_owned().expect("reader fails after recovery") {
        assert_eq!(rec.seq, want, "gap in the recovered chain");
        want += 1;
        final_record = Some(rec);
    }
    assert_eq!(
        want, next_seq,
        "chain does not end at the writer's next_seq"
    );
    if let Some(last) = last {
        let rec = final_record.expect("appended record is readable");
        assert_eq!(rec.seq, next_seq - 1);
        assert_eq!(rec.schema_id, last.schema_id);
        assert_eq!(rec.payload, last.payload);
    }
}

/// With `RecoveryPolicy::Fail`, a refused open may still have done the cleanup that every policy does (format.md,
/// Recovery step 4: zero the never-committed bytes after the last record, seal a segment whose successor was
/// published, and create the missing successor of a sealed segment), because that happens during the scan, before a later position or segment is found to be corrupt. It
/// must not have touched anything a reader can see: every valid record, and any slot that is not an empty one.
fn assert_refusal_kept_committed_bytes(
    dir: &Path,
    before: &BTreeMap<String, Vec<u8>>,
    marker: Option<CheckpointSlot>,
    why: &str,
) {
    let after = segment_files(dir);
    for name in before.keys() {
        assert!(
            after.contains_key(name),
            "Fail: a refused open removed {name}: {why}"
        );
    }
    // The only file it may have created is the empty successor of a sealed segment (completing a roll).
    for (name, new) in &after {
        if !before.contains_key(name) {
            let base = format::parse_segment_file_name(name).unwrap();
            let fresh = new.len() == SEG
                && format::decode_segment_header(new[..64].try_into().unwrap())
                    .is_ok_and(|h| h.base_seq == base)
                && new[64..].iter().all(|&b| b == 0);
            assert!(
                fresh,
                "Fail: a refused open created {name}, which is not an empty segment: {why}"
            );
        }
    }
    for (name, old) in before {
        let new = &after[name];
        let Some(base) = format::parse_segment_file_name(name) else {
            continue;
        };
        let header_ok = old.len() == SEG
            && format::decode_segment_header(old[..64].try_into().unwrap())
                .is_ok_and(|h| h.base_seq == base && h.segment_size == SEG as u64);
        if !header_ok {
            assert!(
                new == old,
                "Fail: a refused open changed {name} (bad header): {why}"
            );
            continue;
        }
        // Recovery starts at the marker, and does not look at what is before it.
        let (start, seq) = match marker {
            Some(m) if base < m.segment_base_seq => {
                assert!(
                    new == old,
                    "Fail: a refused open changed {name}, below the checkpoint: {why}"
                );
                continue;
            }
            Some(m) if base == m.segment_base_seq => (m.offset, m.next_seq),
            _ => (64, base),
        };
        let end = format::scan_segment(&old[..], start, seq).end_offset as usize;
        let end = end.max(start as usize);
        assert!(
            new[..end] == old[..end],
            "Fail: a refused open changed records of {name}: {why}"
        );
        if end + 8 > SEG {
            continue;
        }
        if old[end..end + 4] != [0; 4] {
            let diff: Vec<usize> = (0..SEG).filter(|&i| new[i] != old[i]).take(4).collect();
            assert!(
                diff.is_empty(),
                "Fail: a refused open changed {name}, whose records end at {end} with a non-empty slot, at {diff:?}: {why}"
            );
        } else {
            let marker = format::SEAL_MARKER.to_le_bytes();
            assert!(new[end..end + 4] == [0; 4] || new[end..end + 4] == marker);
            assert!(
                (end + 8..SEG).all(|i| new[i] == old[i] || new[i] == 0),
                "Fail: a refused open wrote non-zero bytes into the empty tail of {name}: {why}"
            );
        }
    }
}

fn run(data: &[u8]) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let built = build(data, dir);
    let config = WriterConfig::default()
        .segment_size(SEG as u64)
        .recovery(built.policy);
    let before = segment_files(dir);
    let marker = inspect::read_checkpoint(dir).ok().flatten();

    let writer = match Writer::open(dir, config.clone()) {
        Ok(writer) => writer,
        Err(e) => {
            if std::env::var_os("RECOVER_DIR_TRACE").is_some() {
                eprintln!("open failed: {e}");
            }
            // 5: a refused open leaves the segments alone.
            if built.policy == RecoveryPolicy::Fail {
                assert_refusal_kept_committed_bytes(dir, &before, marker, &e.to_string());
            }
            return;
        }
    };
    if std::env::var_os("RECOVER_DIR_TRACE").is_some() {
        eprintln!("{:#?}", writer.recovery_report());
    }
    let report = writer.recovery_report().clone();
    let next_seq = writer.next_seq();
    assert_eq!(report.next_seq, next_seq);

    // 4: repairs preserve what they remove.
    let mut preserved = 0;
    for action in &report.actions {
        match action {
            RecoveryAction::RepairedTail {
                segment,
                offset,
                len,
                quarantine_file,
                ..
            } => {
                preserved += 1;
                assert_ne!(built.policy, RecoveryPolicy::Fail, "Fail repaired a tail");
                let name = segment.file_name().unwrap().to_str().unwrap();
                let old = &before[name];
                let offset = *offset as usize;
                let kept = fs::read(quarantine_file).unwrap();
                assert!(!kept.is_empty());
                assert_eq!(
                    kept[..],
                    old[offset..offset + kept.len()],
                    "quarantined bytes"
                );
                assert!(old[offset + kept.len()..].iter().all(|&b| b == 0));
                assert_eq!(*len as usize, old.len() - offset);
                let new = fs::read(segment).unwrap();
                assert_eq!(
                    new[..offset],
                    old[..offset],
                    "bytes before the repair changed"
                );
                assert!(new[offset..].iter().all(|&b| b == 0), "tail not zeroed");
            }
            RecoveryAction::QuarantinedSegment {
                segment,
                quarantine_file,
                ..
            } => {
                preserved += 1;
                assert_ne!(
                    built.policy,
                    RecoveryPolicy::Fail,
                    "Fail quarantined a segment"
                );
                let name = segment.file_name().unwrap().to_str().unwrap();
                assert_eq!(fs::read(quarantine_file).unwrap(), before[name]);
            }
            _ => {}
        }
    }
    assert_eq!(
        count_bin(dir),
        built.prequarantined + preserved,
        "quarantine files do not match the repairs: {report:?}"
    );
    for name in before.keys() {
        let quarantined = report.actions.iter().any(|a| {
            matches!(a, RecoveryAction::QuarantinedSegment { segment, .. }
                if segment.file_name().and_then(|n| n.to_str()) == Some(name))
        });
        assert!(
            dir.join(name).exists() || quarantined,
            "{name} disappeared without being quarantined"
        );
    }
    debug_assert_eq!(built.segments.len(), before.len());

    let trust = match marker {
        None => Trust::Nothing,
        Some(m) => {
            let name = format::segment_file_name(m.segment_base_seq);
            if before
                .get(&name)
                .is_some_and(|img| prefix_intact(img, m.segment_base_seq, m.offset, m.next_seq))
            {
                Trust::Before(m.segment_base_seq, m.offset)
            } else {
                Trust::Vouchless
            }
        }
    };
    if report.had_checkpoint {
        assert!(marker.is_some());
    }

    // 2: the recovered journal is valid and readable.
    check_journal(dir, trust, next_seq, None);

    // 3: recovery is idempotent.
    drop(writer);
    let mut writer = Writer::open(dir, config).expect("second open succeeds");
    assert!(
        writer.recovery_report().actions.is_empty(),
        "second open repaired again: {:?} (first: {report:?})",
        writer.recovery_report()
    );
    assert_eq!(writer.next_seq(), next_seq);

    // 2: appending works.
    if next_seq < u64::MAX {
        let payload = [0xc3u8; 33];
        let rec = NewRecord {
            schema_id: 77,
            venue_ts_ns: 1,
            local_ts_ns: 2,
            payload: &payload,
        };
        let seq = writer.append(&rec).expect("append after recovery");
        assert_eq!(seq, next_seq);
        check_journal(dir, trust, next_seq + 1, Some(&rec));
    }
}

fuzz_target!(|data: &[u8]| run(data));
