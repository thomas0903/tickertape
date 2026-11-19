//! Follows a recording live with an ordinary tickertape reader and decodes it with the documented layout. No
//! iceoryx2: this example builds with `cargo build --example follow --no-default-features`.
//!
//! Usage: `follow --journal DIR [--durable-only] [--from SEQ] [--count N] [--idle-secs S]`
//!
//! Prints one line per record, `key=value` separated by spaces, byte strings in hex:
//!
//! ```text
//! descriptor seq=0 run=0 service=demo/ticks payload_type=... payload_size=24 header_type=... header_size=16
//! data seq=1 service=demo/ticks publisher=... local_ts=... venue_ts=... header=0000... payload=0100...
//! loss seq=7 service=demo/ticks kind=gap expected=5 received=9
//! undescribed seq=9 descriptor_seq=3
//! ```
//!
//! A record whose descriptor is no longer in the journal (retention removed its segment) is reported as
//! `undescribed` and skipped. It stops after `--count` data records, or after `--idle-secs` seconds without a new
//! record (default 5). With `--durable-only` it only returns records covered by a checkpoint.

use std::time::{Duration, Instant};

use tickertape::{Reader, ReaderConfig};
use tickertape_iceoryx2::Catalog;
use tickertape_iceoryx2::layout::{self, Decoded, LossKind};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut journal, mut durable_only, mut from, mut count, mut idle) =
        (None, false, None, u64::MAX, 5.0f64);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--durable-only" => durable_only = true,
            "--journal" => journal = args.next(),
            "--from" => from = Some(args.next().ok_or("--from SEQ")?.parse()?),
            "--count" => count = args.next().ok_or("--count N")?.parse()?,
            "--idle-secs" => idle = args.next().ok_or("--idle-secs S")?.parse()?,
            _ => {
                eprintln!(
                    "usage: follow --journal DIR [--durable-only] [--from SEQ] [--count N] [--idle-secs S]"
                );
                std::process::exit(1);
            }
        }
    }
    let journal = journal.ok_or("--journal DIR is required")?;
    let idle = Duration::from_secs_f64(idle);

    // The recorder may not have created the journal yet.
    let started = Instant::now();
    let mut reader = loop {
        match Reader::open(&journal, ReaderConfig::default().durable_only(durable_only)) {
            Ok(r) => break r,
            Err(tickertape::Error::NotFound { .. } | tickertape::Error::Io { .. })
                if started.elapsed() < idle =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e.into()),
        }
    };
    if let Some(seq) = from {
        reader.seek(seq)?;
    }
    let mut catalog = Catalog::new(&journal);
    let mut data = 0u64;
    while data < count {
        let Some(record) = reader.next_timeout(Some(idle))? else {
            eprintln!("follow: no new records for {idle:?}, stopping");
            break;
        };
        let seq = record.seq;
        match layout::decode(record.schema_id, record.payload)? {
            Decoded::Descriptor(d) => {
                println!(
                    "descriptor seq={seq} run={} service={} payload_type={} payload_size={} header_type={} header_size={}",
                    d.run_start_seq,
                    d.service_name,
                    d.payload.name,
                    d.payload.size,
                    d.user_header.name,
                    d.user_header.size
                );
                catalog.insert(seq, *d);
            }
            Decoded::Data(m) => {
                let Some(d) = catalog.resolve(seq, m.descriptor_seq)? else {
                    println!("undescribed seq={seq} descriptor_seq={}", m.descriptor_seq);
                    continue;
                };
                m.check(&d)?;
                println!(
                    "data seq={seq} service={} publisher={:x} local_ts={} venue_ts={} header={} payload={}",
                    d.service_name,
                    m.publisher_id,
                    record.local_ts_ns,
                    record.venue_ts_ns,
                    hex(m.user_header),
                    hex(m.payload)
                );
                data += 1;
            }
            Decoded::Loss(l) => {
                let Some(d) = catalog.resolve(seq, l.descriptor_seq)? else {
                    println!("undescribed seq={seq} descriptor_seq={}", l.descriptor_seq);
                    continue;
                };
                l.check(&d)?;
                let kind = match l.kind {
                    LossKind::Gap => "gap",
                    LossKind::Backwards => "backwards",
                };
                println!(
                    "loss seq={seq} service={} kind={kind} expected={} received={}",
                    d.service_name, l.expected, l.received
                );
            }
            // Zeros that end a segment early so that the descriptors start the next one: skipped.
            Decoded::Filler(_) => {}
            Decoded::Other => println!("other seq={seq} schema={:#x}", record.schema_id),
        }
    }
    Ok(())
}
