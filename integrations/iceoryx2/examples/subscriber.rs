//! A typed iceoryx2 subscriber for the tests: prints each message as `<user header hex> <payload hex>`.
//!
//! Usage: `subscriber --service NAME [--kind tick|slice] --count N [--timeout-secs S]`
//!
//! Waits for the service to exist, subscribes (printing `subscribed` to stderr), and exits after `--count`
//! messages or `--timeout-secs` (default 20) without one.

use std::time::{Duration, Instant};

use iceoryx2::prelude::*;
use zerocopy::IntoBytes;

#[path = "common/types.rs"]
mod types;
use types::{Tick, TickHeader};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

macro_rules! run {
    ($node:expr, $name:expr, $payload:ty, $count:expr, $timeout:expr) => {{
        let started = Instant::now();
        let service = loop {
            match $node
                .service_builder($name)
                .publish_subscribe::<$payload>()
                .user_header::<TickHeader>()
                .open()
            {
                Ok(s) => break s,
                Err(e) if started.elapsed() > $timeout => return Err(format!("open: {e:?}").into()),
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        let buffer = service.static_config().subscriber_max_buffer_size();
        let subscriber = service.subscriber_builder().buffer_size(buffer).create()?;
        eprintln!("subscribed");
        let mut received = 0u64;
        let mut last = Instant::now();
        while received < $count {
            match subscriber.receive()? {
                Some(sample) => {
                    println!(
                        "{} {}",
                        hex(sample.user_header().as_bytes()),
                        hex(sample.payload().as_bytes())
                    );
                    received += 1;
                    last = Instant::now();
                }
                None if last.elapsed() > $timeout => break,
                None => std::thread::sleep(Duration::from_micros(200)),
            }
        }
    }};
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut service, mut slice, mut count, mut timeout) =
        (String::new(), false, u64::MAX, 20.0f64);
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or("missing value")?;
        match flag.as_str() {
            "--service" => service = value,
            "--kind" => slice = value == "slice",
            "--count" => count = value.parse()?,
            "--timeout-secs" => timeout = value.parse()?,
            _ => return Err(format!("unknown flag {flag}").into()),
        }
    }
    set_log_level_from_env_or(LogLevel::Error);
    let node = NodeBuilder::new()
        .name(&NodeName::new("tickertape-example-subscriber")?)
        .create::<ipc::Service>()?;
    let name = ServiceName::new(&service)?;
    let timeout = Duration::from_secs_f64(timeout);
    if slice {
        run!(node, &name, [u32], count, timeout);
    } else {
        run!(node, &name, Tick, count, timeout);
    }
    Ok(())
}
