//! A typed iceoryx2 publisher for the tests and the quick start.
//!
//! Usage: `publisher --service NAME [--kind tick|slice] [--count N] [--rate HZ] [--wait-subscribers N]
//! [--wait-secs S] [--skip-every K] [--notify EVENT] [--buffer N] [--linger-ms MS] [--blocking]`
//!
//! Sends `--count` messages (0: until killed) at `--rate` per second, on an absolute schedule. Each message has a
//! `TickHeader` user header with a counter (0, 1, 2, ...) and the send time, and a payload derived from the
//! counter (see `common/types.rs`): a `Tick`, or with `--kind slice` a `[u32]` of 1 to 16 elements.
//! `--skip-every K` leaves out one counter value after every K messages, to simulate loss. If it creates the
//! service, it creates it with `subscriber_max_buffer_size` = `--buffer` (default 4096). With `--blocking` it
//! requires the service without safe overflow (and creates it so), so a full subscriber buffer makes it wait
//! (iceoryx2's default backpressure, retry until delivered) instead of dropping the oldest sample: delivery to
//! subscribers that exist is exact. With `--count 0` it sends until SIGTERM or SIGINT.

use std::time::{Duration, Instant};

use iceoryx2::prelude::*;

#[path = "common/types.rs"]
mod types;
use types::{Tick, TickHeader};

struct Args {
    service: String,
    slice: bool,
    count: u64,
    rate: f64,
    wait_subscribers: usize,
    wait: Duration,
    skip_every: u64,
    notify: Option<String>,
    buffer: usize,
    linger: Duration,
    blocking: bool,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        service: String::new(),
        slice: false,
        count: 100,
        rate: 1000.0,
        wait_subscribers: 0,
        wait: Duration::from_secs(10),
        skip_every: 0,
        notify: None,
        buffer: 4096,
        linger: Duration::from_millis(300),
        blocking: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--service" => a.service = value()?,
            "--kind" => a.slice = value()? == "slice",
            "--count" => a.count = value()?.parse().map_err(|_| "--count")?,
            "--rate" => a.rate = value()?.parse().map_err(|_| "--rate")?,
            "--wait-subscribers" => {
                a.wait_subscribers = value()?.parse().map_err(|_| "--wait-subscribers")?
            }
            "--wait-secs" => {
                a.wait = Duration::from_secs_f64(value()?.parse().map_err(|_| "--wait-secs")?)
            }
            "--skip-every" => a.skip_every = value()?.parse().map_err(|_| "--skip-every")?,
            "--notify" => a.notify = Some(value()?),
            "--buffer" => a.buffer = value()?.parse().map_err(|_| "--buffer")?,
            "--blocking" => a.blocking = true,
            "--linger-ms" => {
                a.linger = Duration::from_millis(value()?.parse().map_err(|_| "--linger-ms")?)
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    if a.service.is_empty() || !a.rate.is_finite() || a.rate <= 0.0 {
        return Err("--service is required and --rate must be positive".into());
    }
    Ok(a)
}

macro_rules! open_or_create {
    ($node:expr, $name:expr, $payload:ty, $buffer:expr, $blocking:expr) => {{
        let make = || {
            let b = $node
                .service_builder($name)
                .publish_subscribe::<$payload>()
                .user_header::<TickHeader>();
            if $blocking {
                b.enable_safe_overflow(false)
            } else {
                b
            }
        };
        let started = Instant::now();
        loop {
            match make().open() {
                Ok(s) => break s,
                Err(e) => {
                    if let Ok(s) = make()
                        .subscriber_max_buffer_size($buffer)
                        .history_size(0)
                        .create()
                    {
                        break s;
                    }
                    if started.elapsed() > Duration::from_secs(10) {
                        eprintln!("publisher: can't open or create the service: {e:?}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }};
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse().unwrap_or_else(|e| {
        eprintln!("publisher: {e}");
        std::process::exit(1);
    });
    set_log_level_from_env_or(LogLevel::Error);
    let node = NodeBuilder::new()
        .name(&NodeName::new("tickertape-example-publisher")?)
        .create::<ipc::Service>()?;
    let name = ServiceName::new(&args.service)?;
    let notifier = match &args.notify {
        Some(event) => Some(
            node.service_builder(&ServiceName::new(event)?)
                .event()
                .open_or_create()?
                .notifier_builder()
                .create()?,
        ),
        None => None,
    };
    let start_send = |publish: &mut dyn FnMut(u64, TickHeader) -> bool,
                      subscribers: &dyn Fn() -> usize| {
        let waited = Instant::now();
        while subscribers() < args.wait_subscribers {
            if waited.elapsed() > args.wait {
                eprintln!(
                    "publisher: only {} subscribers after {:?}",
                    subscribers(),
                    args.wait
                );
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!("publisher: sending on {}", args.service);
        let start = Instant::now();
        let (mut sent, mut failed, mut counter) = (0u64, 0u64, 0u64);
        while args.count == 0 || sent + failed < args.count {
            let due = start + Duration::from_secs_f64((sent + failed) as f64 / args.rate);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let header = TickHeader {
                counter,
                venue_ts_ns: tickertape::clock::realtime_ns(),
            };
            if publish(counter, header) {
                sent += 1;
            } else {
                failed += 1;
            }
            if let Some(n) = &notifier {
                let _ = n.notify();
            }
            counter += 1;
            if args.skip_every > 0 && (sent + failed) % args.skip_every == 0 {
                counter += 1;
            }
        }
        std::thread::sleep(args.linger);
        println!("sent {sent} failed {failed}");
    };
    if args.slice {
        let service = open_or_create!(node, &name, [u32], args.buffer, args.blocking);
        let publisher = service
            .publisher_builder()
            .initial_max_slice_len(16)
            .create()?;
        start_send(
            &mut |counter, header| {
                let values = types::slice_for_counter(counter);
                let Ok(mut sample) = publisher.loan_slice_uninit(values.len()) else {
                    return false;
                };
                *sample.user_header_mut() = header;
                sample.write_from_slice(&values).send().is_ok()
            },
            &|| service.dynamic_config().number_of_subscribers(),
        );
    } else {
        let service = open_or_create!(node, &name, Tick, args.buffer, args.blocking);
        let publisher = service.publisher_builder().create()?;
        start_send(
            &mut |counter, header| {
                let Ok(mut sample) = publisher.loan_uninit() else {
                    return false;
                };
                *sample.user_header_mut() = header;
                sample
                    .write_payload(Tick::for_counter(counter))
                    .send()
                    .is_ok()
            },
            &|| service.dynamic_config().number_of_subscribers(),
        );
    }
    Ok(())
}
