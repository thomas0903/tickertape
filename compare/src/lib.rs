//! Common process protocol for the Rust comparison adapters. Every reader validates
//! ordering and record length. Payload MB/s excludes the adapter's 16-byte envelope.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use serde_json::json;
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig, clock};
use tickertape_bench::{Options, Result, histogram_json, intended_ns};

const SEGMENT_SIZE: u64 = 134_217_728;

fn wait_start(control: &Path, role: &str) -> Result<u64> {
    fs::write(control.join(format!("{role}.ready")), [])?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while !control.join("start").exists() {
        if Instant::now() > deadline {
            return Err("start barrier timed out".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(fs::read_to_string(control.join("start"))?.parse()?)
}

fn validate(data: &[u8], expected: u64, payload: usize) -> Result<u64> {
    if data.len() != payload + 16 || u64::from_le_bytes(data[..8].try_into()?) != expected {
        return Err("record sequence or length mismatch".into());
    }
    std::hint::black_box(&data[16..]);
    Ok(u64::from_le_bytes(data[8..16].try_into()?))
}

// Only the plain-file adapter uses this decoder. EOF may split either the header
// or body, so retain partial reads until publication is complete; never seek back.
struct FileReader {
    file: File,
    frame: Vec<u8>,
    filled: usize,
}
impl FileReader {
    fn poll(&mut self) -> Result<Option<&[u8]>> {
        while self.filled < self.frame.len() {
            match self.file.read(&mut self.frame[self.filled..]) {
                Ok(0) => return Ok(None),
                Ok(n) => self.filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let len = u32::from_le_bytes(self.frame[..4].try_into()?) as usize;
        if len != self.frame.len() - 4 {
            return Err("bad file frame length".into());
        }
        self.filled = 0;
        Ok(Some(&self.frame[4..]))
    }
}

enum Sink {
    Tape(Writer),
    File(File),
    Yaque(yaque::Sender),
    Queue(queue_file::QueueFile),
}
impl Sink {
    fn open(tool: &str, dir: &Path) -> Result<Self> {
        Ok(match tool {
            "tickertape" => Self::Tape(Writer::open(dir, WriterConfig::default())?),
            "plain-file" => Self::File(
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(dir.join("data"))?,
            ),
            "yaque" => Self::Yaque(
                yaque::SenderBuilder::new()
                    .segment_size(SEGMENT_SIZE)
                    .open(dir)?,
            ),
            "queue-file" => {
                let mut q = queue_file::QueueFile::open(dir.join("data"))?;
                q.set_sync_writes(false);
                // Keep header publication per add: disabling this would make a
                // faster buffered mode with weaker process-crash semantics.
                Self::Queue(q)
            }
            _ => return Err("unknown tool".into()),
        })
    }
    fn send(&mut self, ts: u64, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Tape(w) => {
                w.append(&NewRecord {
                    schema_id: 1,
                    venue_ts_ns: 0,
                    local_ts_ns: ts,
                    payload: &bytes[16..],
                })?;
            }
            Self::File(f) => f.write_all(bytes)?,
            Self::Yaque(w) => w.try_send(bytes).map_err(yaque::TrySendError::unwrap_io)?,
            Self::Queue(w) => w.add(bytes)?,
        }
        Ok(())
    }
    fn sync(&mut self) -> Result<Option<&'static str>> {
        Ok(match self {
            Self::Tape(w) => {
                w.checkpoint()?;
                Some("checkpoint")
            }
            Self::File(f) => {
                f.sync_all()?;
                Some("sync_all")
            }
            Self::Yaque(_) => None,
            Self::Queue(w) => {
                w.sync_all()?;
                Some("sync_all")
            }
        })
    }
}

fn run(tool: &str) -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let role = args.first().ok_or("expected writer or reader")?;
    if role != "writer" && role != "reader" {
        return Err("expected writer or reader".into());
    }
    let opts = Options::parse(
        &args[1..],
        &[
            "--dir",
            "--control",
            "--count",
            "--payload",
            "--rate",
            "--warmup",
            "--reader",
            "--mode",
        ],
        &[],
    )?;
    let dir = PathBuf::from(opts.required("--dir")?);
    let count = opts.number("--count", None)?;
    let payload: usize = opts.number("--payload", None)?.try_into()?;
    let rate = opts.number("--rate", Some(100_000))?;
    let warmup = opts.number("--warmup", Some(0))?;
    let mode = opts.required("--mode")?;
    let live = mode == "visible_latency";
    if count == 0 || rate == 0 || warmup >= count || ![64, 1024].contains(&payload) {
        return Err("invalid count/rate/warmup/payload".into());
    }
    let control = opts.path("--control");
    if role == "writer" {
        let mut sink = Sink::open(tool, &dir)?;
        let mut bytes = vec![0x5a; payload + 16 + usize::from(tool == "plain-file") * 4];
        let offset = if tool == "plain-file" { 4 } else { 0 };
        if offset == 4 {
            bytes[..4].copy_from_slice(&((payload + 16) as u32).to_le_bytes());
        }
        let t0 = if live {
            wait_start(control.as_deref().ok_or("missing control")?, "writer")?
        } else {
            0
        };
        let mut late = 0;
        let mut start = None;
        for i in 0..count {
            let intended = if live { intended_ns(t0, i, rate)? } else { 0 };
            if live {
                while clock::realtime_ns() < intended {
                    std::hint::spin_loop();
                }
                if clock::realtime_ns() - intended > 1000 {
                    late += 1;
                }
            }
            start.get_or_insert_with(Instant::now);
            bytes[offset..offset + 8].copy_from_slice(&i.to_le_bytes());
            bytes[offset + 8..offset + 16].copy_from_slice(&intended.to_le_bytes());
            // Plain file's prefix is passed only to write(2), not other tools.
            sink.send(intended, &bytes)?;
        }
        let seconds = start.ok_or("no sends")?.elapsed().as_secs_f64();
        let sync_start = Instant::now();
        let sync = if live { None } else { sink.sync()? };
        let flush_seconds = sync.map(|_| sync_start.elapsed().as_secs_f64());
        println!(
            "{}",
            json!({"count": count, "seconds": seconds, "records_per_s": count as f64 / seconds,
            "payload_mb_per_s": count as f64 * payload as f64 / seconds / 1e6,
            "achieved_records_per_s": count as f64 / seconds, "late_records": late,
            "flush_seconds": flush_seconds, "flush_operation": sync})
        );
        return Ok(());
    }
    let mut hist = Histogram::<u64>::new(3)?;
    let start = Instant::now(); // Includes fresh reader opening for W3.
    let deadline = start + Duration::from_secs(count / rate + 120);
    let mut expected = 0;
    let mut observe = |seq: u64, ts: u64, len: usize| -> Result<()> {
        if seq != expected || len != payload {
            return Err("record sequence or length mismatch".into());
        }
        if live && expected >= warmup {
            hist.record(
                clock::realtime_ns()
                    .checked_sub(ts)
                    .ok_or("wall clock moved backwards")?,
            )?;
        }
        expected += 1;
        Ok(())
    };
    let ready = || -> Result<()> {
        if live {
            wait_start(control.as_deref().ok_or("missing control")?, "reader")?;
        }
        Ok(())
    };
    let check_deadline = || -> Result<()> {
        if Instant::now() > deadline {
            return Err("reader timed out".into());
        }
        std::hint::spin_loop();
        Ok(())
    };
    let seconds = match tool {
        "tickertape" => {
            let cfg = ReaderConfig::default();
            let mut r = if opts.required("--reader")? == "mapped" {
                // SAFETY: this run owns a fresh journal, modified only by Writer.
                unsafe { Reader::open_mapped(&dir, cfg)? }
            } else {
                Reader::open(&dir, cfg)?
            };
            ready()?;
            for _ in 0..count {
                loop {
                    if let Some(record) = r.try_next()? {
                        std::hint::black_box(record.payload);
                        observe(record.seq, record.local_ts_ns, record.payload.len())?;
                        break;
                    }
                    check_deadline()?;
                }
            }
            start.elapsed().as_secs_f64()
        }
        "plain-file" => {
            let mut r = FileReader {
                file: File::open(dir.join("data"))?,
                frame: vec![0; payload + 20],
                filled: 0,
            };
            ready()?;
            for i in 0..count {
                loop {
                    if let Some(bytes) = r.poll()? {
                        observe(i, validate(bytes, i, payload)?, bytes.len() - 16)?;
                        break;
                    }
                    check_deadline()?;
                }
            }
            start.elapsed().as_secs_f64()
        }
        "yaque" if live => {
            let mut r = yaque::ReceiverBuilder::new()
                .save_every_nth(None)
                .save_every(None)
                .open(&dir)?;
            ready()?;
            for i in 0..count {
                loop {
                    match r.try_recv() {
                        Ok(record) => {
                            observe(i, validate(&record, i, payload)?, record.len() - 16)?;
                            record.commit()?;
                            break;
                        }
                        Err(yaque::TryRecvError::QueueEmpty) => check_deadline()?,
                        Err(e) => return Err(e.unwrap_io().into()),
                    }
                }
            }
            start.elapsed().as_secs_f64()
        }
        "yaque" => {
            let r = yaque::QueueIter::open(&dir)?;
            let mut iter = r.take(count as usize).enumerate();
            for (i, record) in iter.by_ref() {
                let record = record?;
                observe(
                    i as u64,
                    validate(&record, i as u64, payload)?,
                    record.len() - 16,
                )?;
            }
            start.elapsed().as_secs_f64()
        }
        "queue-file" => {
            if live {
                return Err("queue-file does not support concurrent processes".into());
            }
            let r = queue_file::QueueFile::open(dir.join("data"))?;
            let mut iter = r.iter();
            for i in 0..count {
                let bytes = iter.borrowed_next().ok_or("truncated queue")?;
                observe(i, validate(bytes, i, payload)?, bytes.len() - 16)?;
            }
            start.elapsed().as_secs_f64()
        }
        _ => return Err("unknown tool".into()),
    };
    if expected != count {
        return Err("incomplete replay".into());
    }
    println!(
        "{}",
        if live {
            histogram_json(&hist)
        } else {
            json!({"count": count, "seconds": seconds,
        "records_per_s": count as f64 / seconds, "payload_mb_per_s": count as f64 * payload as f64 / seconds / 1e6})
        }
    );
    Ok(())
}

pub fn main_for(tool: &str) {
    if let Err(e) = run(tool) {
        eprintln!("{tool}: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_follower_retains_partial_header_and_payload_at_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data");
        let mut writer = File::create(&path).unwrap();
        let mut reader = FileReader {
            file: File::open(&path).unwrap(),
            frame: vec![0; 84],
            filled: 0,
        };
        let mut frame = vec![0x5a; 84];
        frame[..4].copy_from_slice(&80_u32.to_le_bytes());
        frame[4..12].copy_from_slice(&0_u64.to_le_bytes());
        frame[12..20].copy_from_slice(&123_u64.to_le_bytes());
        assert!(reader.poll().unwrap().is_none());
        for chunk in frame.chunks(3) {
            writer.write_all(chunk).unwrap();
            if reader.filled + chunk.len() < 84 {
                assert!(reader.poll().unwrap().is_none());
            }
        }
        let bytes = reader.poll().unwrap().unwrap();
        assert_eq!(validate(bytes, 0, 64).unwrap(), 123);
        assert!(reader.poll().unwrap().is_none());
        writer.write_all(&frame).unwrap();
        assert!(reader.poll().unwrap().is_some());
    }

    #[test]
    fn rejects_wrong_sequence_and_frame_length() {
        assert!(validate(&[0; 80], 1, 64).is_err());
        assert!(validate(&[0; 16], 0, 64).is_err());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data");
        fs::write(&path, [0; 84]).unwrap();
        let mut reader = FileReader {
            file: File::open(path).unwrap(),
            frame: vec![0; 84],
            filled: 0,
        };
        assert!(reader.poll().is_err());
    }
}
