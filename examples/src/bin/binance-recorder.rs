//! Optional public Binance trade recorder. Build with `--features live`; no account or key is used.

use std::error::Error;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tickertape::{NewRecord, Writer, WriterConfig};
use tickertape_examples::Event;
use tickertape_examples::live::parse_trade;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
type Socket = WebSocket<MaybeTlsStream<TcpStream>>;
const POLL: Duration = Duration::from_millis(200);
const USAGE: &str = "usage: binance-recorder --dir <DIR> [--symbols btcusdt,ethusdt] [--url <wss url>] [--max-records N] [--max-seconds S] [--checkpoint-every N (default 1000)]";

#[derive(Debug)]
struct Args {
    dir: String,
    symbols: Vec<String>,
    url: String,
    max_records: Option<u64>,
    max_seconds: Option<u64>,
    checkpoint_every: u64,
}

fn parse(arguments: impl Iterator<Item = String>) -> std::result::Result<Args, String> {
    let mut args = Args {
        dir: String::new(),
        symbols: vec!["btcusdt".into(), "ethusdt".into()],
        url: String::new(),
        max_records: None,
        max_seconds: None,
        checkpoint_every: 1000,
    };
    let mut it = arguments;
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--dir" => args.dir = value,
            "--symbols" => {
                args.symbols = value.split(',').map(|s| s.to_ascii_lowercase()).collect();
            }
            "--url" => args.url = value,
            "--max-records" => {
                args.max_records = Some(value.parse().map_err(|_| "bad --max-records")?);
            }
            "--max-seconds" => {
                args.max_seconds = Some(value.parse().map_err(|_| "bad --max-seconds")?);
            }
            "--checkpoint-every" => {
                args.checkpoint_every = value.parse().map_err(|_| "bad --checkpoint-every")?;
            }
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    if args.dir.is_empty() || args.checkpoint_every == 0 {
        return Err("--dir is required; --checkpoint-every must be positive".into());
    }
    if args.symbols.len() > 1024
        || args.symbols.iter().enumerate().any(|(i, s)| {
            s.is_empty()
                || !s.bytes().all(|b| b.is_ascii_alphanumeric())
                || args.symbols[..i].contains(s)
        })
    {
        return Err("--symbols needs 1–1024 distinct, comma-separated alphanumeric symbols".into());
    }
    if args.url.is_empty() {
        // Official public combined trade streams (verified 2026-10-31):
        // https://developers.binance.com/en/docs/products/spot/web-socket-streams
        let streams = args
            .symbols
            .iter()
            .map(|s| format!("{s}@trade"))
            .collect::<Vec<_>>();
        args.url = format!(
            "wss://stream.binance.com:9443/stream?streams={}",
            streams.join("/")
        );
    }
    let request = args
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| e.to_string())?;
    if request.uri().scheme_str() != Some("wss") || request.uri().host().is_none() {
        return Err("--url must be a wss URL".into());
    }
    if request.uri().query().is_some_and(|q| {
        q.split('&')
            .any(|pair| pair.split('=').next() == Some("timeUnit"))
    }) {
        return Err(
            "--url must use Binance's default millisecond timestamps (omit timeUnit)".into(),
        );
    }
    Ok(args)
}

#[derive(Debug)]
struct Stats {
    records: u64,
    skipped: u64,
    last_price: Vec<Option<i64>>,
    started: Instant,
    reported: Instant,
}

impl Stats {
    fn new(symbols: usize) -> Self {
        Self {
            records: 0,
            skipped: 0,
            last_price: vec![None; symbols],
            started: Instant::now(),
            reported: Instant::now(),
        }
    }

    fn stopped(&self, args: &Args, stop: &AtomicBool) -> bool {
        stop.load(Ordering::Relaxed)
            || args.max_records.is_some_and(|n| self.records >= n)
            || args
                .max_seconds
                .is_some_and(|s| self.started.elapsed() >= Duration::from_secs(s))
    }

    fn io_timeout(&self, args: &Args, cap: Duration) -> Duration {
        args.max_seconds
            .map_or(cap, |s| {
                Duration::from_secs(s)
                    .saturating_sub(self.started.elapsed())
                    .min(cap)
            })
            .max(Duration::from_millis(1))
    }

    fn report(&mut self, args: &Args, force: bool) {
        if !force && self.reported.elapsed() < Duration::from_secs(3) {
            return;
        }
        let prices = args
            .symbols
            .iter()
            .zip(&self.last_price)
            .map(|(symbol, price)| {
                let price = price.map_or_else(
                    || "-".into(),
                    |p| format!("{}.{:08}", p / 100_000_000, p % 100_000_000),
                );
                format!("{symbol}={price}")
            })
            .collect::<Vec<_>>();
        eprintln!(
            "binance-recorder: records={} skipped={} {}",
            self.records,
            self.skipped,
            prices.join(" ")
        );
        self.reported = Instant::now();
    }
}

fn connect(args: &Args, stats: &Stats, stop: &AtomicBool) -> Result<Socket> {
    let request = args.url.as_str().into_client_request()?;
    let uri = request.uri();
    let host = uri.host().ok_or("URL needs a host")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    // The system resolver is blocking. TCP and handshake I/O have bounded waits, including on idle feeds.
    let addresses = (host, port).to_socket_addrs()?;
    let mut last_error = io::Error::other("no addresses for stream host");
    for address in addresses {
        if stats.stopped(args, stop) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopping").into());
        }
        let timeout = stats.io_timeout(args, Duration::from_secs(3));
        let stream = match TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => stream,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        // Cloned handles share socket options; retain one to adjust waits after the TLS handshake.
        let control = stream.try_clone()?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(64 * 1024))
            .max_frame_size(Some(64 * 1024));
        let (socket, _) = tungstenite::client_tls_with_config(request, stream, Some(config), None)
            .map_err(|e| io::Error::other(e.to_string()))?;
        control.set_read_timeout(Some(POLL))?;
        control.set_write_timeout(Some(POLL))?;
        return Ok(socket);
    }
    Err(last_error.into())
}

fn record(args: &Args, writer: &mut Writer, stats: &mut Stats, stop: &AtomicBool) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    while !stats.stopped(args, stop) {
        match connect(args, stats, stop) {
            Ok(mut socket) => {
                eprintln!("binance-recorder: connected");
                while !stats.stopped(args, stop) {
                    stats.report(args, false);
                    let message = socket.read();
                    let local_ts_ns = tickertape::clock::realtime_ns();
                    match message {
                        Ok(Message::Text(text)) => {
                            let Some(trade) = parse_trade(&text, &args.symbols) else {
                                stats.skipped += 1;
                                continue;
                            };
                            let payload = trade.event.encode();
                            writer.append(&NewRecord {
                                schema_id: trade.event.schema_id(),
                                venue_ts_ns: trade.venue_ts_ns,
                                local_ts_ns,
                                payload: &payload,
                            })?;
                            stats.records += 1;
                            if let Event::Trade {
                                instrument_id,
                                price,
                                ..
                            } = trade.event
                            {
                                stats.last_price[instrument_id as usize] = Some(price);
                            }
                            if stats.records % args.checkpoint_every == 0 {
                                writer.checkpoint()?;
                            }
                            backoff = Duration::from_secs(1);
                        }
                        Ok(Message::Ping(_)) => {
                            // tungstenite queues an identical-payload pong; flush it immediately.
                            if let Err(error) = socket.flush() {
                                eprintln!("binance-recorder: pong failed: {error}");
                                break;
                            }
                        }
                        Ok(Message::Close(_)) => {
                            let _ = socket.flush();
                            eprintln!("binance-recorder: server closed the stream");
                            break;
                        }
                        Ok(Message::Pong(_)) => {}
                        Ok(_) => stats.skipped += 1,
                        Err(tungstenite::Error::Io(ref error))
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock
                                    | io::ErrorKind::TimedOut
                                    | io::ErrorKind::Interrupted
                            ) => {}
                        Err(error) => {
                            eprintln!("binance-recorder: disconnected: {error}");
                            break;
                        }
                    }
                }
                let _ = socket.close(None);
            }
            Err(error) => eprintln!("binance-recorder: connection failed: {error}"),
        }
        if stats.stopped(args, stop) {
            break;
        }
        eprintln!(
            "binance-recorder: reconnecting in {} s (capture may have gaps)",
            backoff.as_secs()
        );
        let waiting = Instant::now();
        while waiting.elapsed() < backoff && !stats.stopped(args, stop) {
            stats.report(args, false);
            std::thread::sleep(POLL.min(backoff.saturating_sub(waiting.elapsed())));
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
    Ok(())
}

fn run(args: &Args, stop: &AtomicBool) -> Result<Stats> {
    let mut writer = Writer::open(&args.dir, WriterConfig::default())?;
    let mut stats = Stats::new(args.symbols.len());
    for (id, symbol) in args.symbols.iter().enumerate() {
        eprintln!("binance-recorder: instrument_id={id} symbol={symbol}");
    }
    let result = record(args, &mut writer, &mut stats, stop);
    // Attempt a final checkpoint even if appending or a periodic checkpoint failed.
    let checkpoint = writer.checkpoint();
    stats.report(args, true);
    checkpoint?;
    result?;
    Ok(stats)
}

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.iter().any(|s| s == "--help" || s == "-h") {
        println!(
            "{USAGE}\n\nOptional public trade capture. Side is the taker's side (buy = !m).\nSymbol order defines instrument ids; keep it unchanged when resuming a journal.\nURL overrides must emit trade messages with default millisecond timestamps.\nCtrl-C checkpoints and exits. Disconnects can leave capture gaps."
        );
        return Ok(());
    }
    let args = parse(arguments.into_iter()).map_err(|error| format!("{error}\n{USAGE}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
    run(&args, &stop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use tickertape::{Reader, ReaderConfig};

    const TRADE: &str = r#"{"e":"trade","s":"BTCUSDT","t":42,"p":"68420.12","q":"0.00123","T":1793400000123,"m":false}"#;

    fn local_args(dir: &std::path::Path, listener: &TcpListener) -> Args {
        // Only tests bypass the CLI's wss requirement, for a loopback-only mock server.
        Args {
            dir: dir.to_str().unwrap().into(),
            symbols: vec!["btcusdt".into()],
            url: format!("ws://{}", listener.local_addr().unwrap()),
            max_records: Some(2),
            max_seconds: Some(10),
            checkpoint_every: 1,
        }
    }

    #[test]
    fn loopback_reconnect_skips_bad_messages_and_replies_to_ping() {
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let args = local_args(temp.path(), &listener);
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                if attempt == 0 {
                    socket.send(Message::Text("malformed".into())).unwrap();
                    socket
                        .send(Message::Ping(b"probe".to_vec().into()))
                        .unwrap();
                    assert_eq!(
                        socket.read().unwrap(),
                        Message::Pong(b"probe".to_vec().into())
                    );
                }
                socket.send(Message::Text(TRADE.into())).unwrap();
                if attempt == 0 {
                    socket.close(None).unwrap();
                } else {
                    assert!(matches!(socket.read().unwrap(), Message::Close(_)));
                }
            }
        });
        let stats = run(&args, &AtomicBool::new(false)).unwrap();
        server.join().unwrap();
        assert_eq!(stats.records, 2);
        assert_eq!(stats.skipped, 1);
        let mut reader = Reader::open(temp.path(), ReaderConfig::default()).unwrap();
        for _ in 0..2 {
            let record = reader.try_next().unwrap().unwrap();
            assert_eq!(record.venue_ts_ns, 1_793_400_000_123_000_000);
            assert!(record.local_ts_ns > 0);
            assert_eq!(
                Event::decode(record.schema_id, record.payload),
                Some(parse_trade(TRADE, &args.symbols).unwrap().event)
            );
        }
        assert!(reader.try_next().unwrap().is_none());
        let checkpoint = tickertape::inspect::read_checkpoint(temp.path())
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.next_seq, 2);
        assert!(checkpoint.generation >= 3); // Two periodic checkpoints and the final checkpoint.
    }

    #[test]
    fn idle_connection_obeys_time_limit_and_stop_flag() {
        for signal in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut args = local_args(temp.path(), &listener);
            args.max_seconds = Some(if signal { 10 } else { 1 });
            let stop = Arc::new(AtomicBool::new(false));
            let signal_flag = Arc::clone(&stop);
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                // Write one event, then idle. Final checkpoint must persist this incomplete batch.
                socket.send(Message::Text(TRADE.into())).unwrap();
                if signal {
                    // A ping/pong barrier ensures the event was processed before setting the stop flag.
                    socket.send(Message::Ping(Vec::new().into())).unwrap();
                    assert!(matches!(socket.read().unwrap(), Message::Pong(_)));
                    signal_flag.store(true, Ordering::Relaxed);
                }
                assert!(matches!(socket.read().unwrap(), Message::Close(_)));
            });
            args.checkpoint_every = 1000;
            let started = Instant::now();
            let stats = run(&args, &stop).unwrap();
            server.join().unwrap();
            assert_eq!(stats.records, 1);
            assert!(started.elapsed() < Duration::from_secs(5));
            assert_eq!(
                tickertape::inspect::read_checkpoint(temp.path())
                    .unwrap()
                    .unwrap()
                    .next_seq,
                1,
            );
        }
    }

    #[test]
    fn validates_cli_and_builds_combined_url() {
        let args = parse(
            ["--dir", "journal", "--symbols", "ETHUSDT,BTCUSDT"]
                .map(String::from)
                .into_iter(),
        )
        .unwrap();
        assert_eq!(args.symbols, ["ethusdt", "btcusdt"]);
        assert_eq!(
            args.url,
            "wss://stream.binance.com:9443/stream?streams=ethusdt@trade/btcusdt@trade"
        );
        for extra in [
            ["--symbols", ""],
            ["--symbols", "btcusdt,BTCUSDT"],
            ["--symbols", "btcusdt/ethusdt"],
            ["--checkpoint-every", "0"],
            ["--max-seconds", "-1"],
            ["--max-records", "abc"],
            ["--url", "ws://localhost/trades"],
            ["--url", "wss://example.com/ws?timeUnit=MICROSECOND"],
        ] {
            assert!(
                parse(
                    ["--dir", "journal"]
                        .into_iter()
                        .chain(extra)
                        .map(String::from)
                )
                .is_err()
            );
        }
    }
}
