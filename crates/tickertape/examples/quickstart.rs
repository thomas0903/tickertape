//! The README quickstart (kept here so that it compiles in CI). Run: `cargo run -p tickertape --example quickstart`.

use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

fn main() -> tickertape::Result<()> {
    let dir = std::env::temp_dir().join(format!("tickertape-quickstart-{}", std::process::id()));

    // Writer process: append, and checkpoint when you need durability, not just visibility.
    let mut writer = Writer::open(&dir, WriterConfig::default())?;
    let seq = writer.append(&NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: tickertape::clock::realtime_ns(),
        payload: b"hello",
    })?;
    writer.checkpoint()?;

    // Reader process (or the same one): replay from any sequence number, then follow live.
    let mut reader = Reader::open(&dir, ReaderConfig::default())?;
    reader.seek(seq)?;
    while let Some(record) = reader.next_timeout(Some(std::time::Duration::from_secs(1)))? {
        println!("{} {:?}", record.seq, record.payload);
    }

    drop(writer);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
