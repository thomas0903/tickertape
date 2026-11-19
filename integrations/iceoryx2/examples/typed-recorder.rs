//! A typed recorder: records a `Tick` (or `[u32]`) service with a `TickHeader` user header, using only iceoryx2's
//! public API. Reads `venue_ts_ns` from the header and checks the header's counter for lost messages.
//!
//! Usage: `typed-recorder --journal DIR --service NAME [--kind tick|slice] [--max-messages N]
//! [--checkpoint-ms MS] [--buffer N]`
//!
//! Stops on SIGINT or SIGTERM (or after `--max-messages`), checkpoints, and prints the report as JSON.

use std::time::Duration;

use tickertape_iceoryx2::layout::{CounterField, CounterLocation};
use tickertape_iceoryx2::recorder::{Recorder, TypedService};

#[path = "common/types.rs"]
mod types;
use types::{Tick, TickHeader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut journal, mut service, mut slice, mut max, mut checkpoint_ms, mut buffer) = (
        String::new(),
        String::new(),
        false,
        None,
        1000u64,
        1024usize,
    );
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or("missing value")?;
        match flag.as_str() {
            "--journal" => journal = value,
            "--service" => service = value,
            "--kind" => slice = value == "slice",
            "--max-messages" => max = Some(value.parse()?),
            "--checkpoint-ms" => checkpoint_ms = value.parse()?,
            "--buffer" => buffer = value.parse()?,
            _ => return Err(format!("unknown flag {flag}").into()),
        }
    }
    iceoryx2::prelude::set_log_level_from_env_or(iceoryx2::prelude::LogLevel::Error);
    let counter = CounterField {
        location: CounterLocation::UserHeader,
        offset: types::COUNTER_OFFSET,
        width: 8,
    };
    let mut builder = Recorder::builder(&journal)
        .checkpoint_interval(Duration::from_millis(checkpoint_ms))
        .buffer_size(buffer)
        .on_open(|name, seq| {
            eprintln!("typed-recorder: recording \"{name}\" (descriptor at sequence {seq})")
        });
    builder = if slice {
        builder.typed(
            TypedService::<[u32], TickHeader>::new(&service)
                .venue_ts(|h| h.venue_ts_ns)
                .counter(counter),
        )
    } else {
        builder.typed(
            TypedService::<Tick, TickHeader>::new(&service)
                .venue_ts(|h| h.venue_ts_ns)
                .counter(counter),
        )
    };
    if let Some(n) = max {
        builder = builder.max_messages(n);
    }
    let report = builder.build()?.run()?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
