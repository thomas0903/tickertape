use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use parquet::basic::{Compression, ConvertedType, LogicalType, Repetition, Type};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::{Field, Row, RowAccessor};
use predicates::prelude::*;
use tempfile::TempDir;
use tickertape::{NewRecord, Writer, WriterConfig};
use tickertape_cli::market_demo::{self, Action, Book, BookSide, Trade, TradeSide};

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("tickertape"))
}

fn payload(n: usize) -> (u32, Vec<u8>) {
    match n % 4 {
        0 => (
            1,
            market_demo::encode_trade(Trade {
                instrument_id: u32::MAX,
                side: TradeSide::Buy,
                price: -12_350_000_000,
                qty: 250_000_000,
                trade_id: u64::MAX,
            })
            .to_vec(),
        ),
        1 => (
            2,
            market_demo::encode_book(Book {
                instrument_id: 7,
                side: BookSide::Ask,
                action: Action::Delete,
                price: 10_000_000_000,
                qty: 0,
            })
            .to_vec(),
        ),
        2 => (u32::MAX, b"opaque".to_vec()),
        _ => (1, b"wrong length".to_vec()),
    }
}

fn journal(count: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut writer = Writer::open(dir.path(), WriterConfig::default().segment_size(65536)).unwrap();
    for n in 0..count {
        let (schema_id, payload) = payload(n);
        writer
            .append(&NewRecord {
                schema_id,
                venue_ts_ns: u64::MAX,
                local_ts_ns: 1_700_000_000_000_000_000,
                payload: &payload,
            })
            .unwrap();
    }
    dir
}

fn export(dir: &Path, file: &Path, extra: &[&str]) {
    command()
        .args(["export", "--format", "parquet", "--output"])
        .arg(file)
        .arg(dir)
        .args(extra)
        .assert()
        .success()
        .stdout("");
}

fn read(file: &Path) -> SerializedFileReader<File> {
    SerializedFileReader::new(File::open(file).unwrap()).unwrap()
}

fn rows(file: &Path) -> Vec<Row> {
    read(file)
        .get_row_iter(None)
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn parquet_multisegment_round_trip_and_decoded_columns() {
    let dir = journal(10_000);
    assert!(
        fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "tape"))
            .count()
            >= 3
    );
    let dest = TempDir::new().unwrap();
    let file = dest.path().join("all.parquet");
    export(dir.path(), &file, &["--decode", "market-demo"]);
    let reader = read(&file);
    let meta = reader.metadata();
    assert_eq!(meta.file_metadata().num_rows(), 10_000);
    assert_eq!(meta.file_metadata().schema().name(), "tickertape_record");
    let columns = meta.file_metadata().schema_descr().columns();
    let expected = [
        ("seq", Type::INT64, Some(LogicalType::integer(64, false))),
        (
            "schema_id",
            Type::INT32,
            Some(LogicalType::integer(32, false)),
        ),
        (
            "venue_ts_ns",
            Type::INT64,
            Some(LogicalType::integer(64, false)),
        ),
        (
            "local_ts_ns",
            Type::INT64,
            Some(LogicalType::integer(64, false)),
        ),
        ("payload", Type::BYTE_ARRAY, None),
        ("kind", Type::BYTE_ARRAY, None),
        (
            "instrument_id",
            Type::INT32,
            Some(LogicalType::integer(32, false)),
        ),
        ("side", Type::BYTE_ARRAY, None),
        ("price", Type::DOUBLE, None),
        ("qty", Type::DOUBLE, None),
        (
            "trade_id",
            Type::INT64,
            Some(LogicalType::integer(64, false)),
        ),
        ("action", Type::BYTE_ARRAY, None),
    ];
    assert_eq!(columns.len(), expected.len());
    for (i, (column, (name, physical, logical))) in columns.iter().zip(expected).enumerate() {
        assert_eq!(column.name(), name);
        assert_eq!(column.physical_type(), physical);
        assert_eq!(column.logical_type_ref(), logical.as_ref());
        if matches!(name, "kind" | "side" | "action") {
            assert_eq!(column.converted_type(), ConvertedType::UTF8);
        }
        assert_eq!(
            column.self_type().get_basic_info().repetition(),
            if i < 5 {
                Repetition::REQUIRED
            } else {
                Repetition::OPTIONAL
            }
        );
    }
    for group in meta.row_groups() {
        assert!(group.num_rows() <= 100_000);
        for column in group.columns() {
            assert_eq!(column.compression(), Compression::SNAPPY);
        }
    }
    let rows = rows(&file);
    assert_eq!(rows.first().unwrap().get_ulong(0).unwrap(), 0);
    assert_eq!(rows.last().unwrap().get_ulong(0).unwrap(), 9_999);
    for (n, row) in rows.iter().enumerate() {
        assert_eq!(row.get_ulong(0).unwrap(), n as u64);
        assert_eq!(row.get_uint(1).unwrap(), payload(n).0);
        assert_eq!(row.get_ulong(2).unwrap(), u64::MAX);
        assert_eq!(row.get_bytes(4).unwrap().data(), payload(n).1);
    }
    let trade = &rows[0];
    assert_eq!(trade.get_string(5).unwrap(), "trade");
    assert_eq!(trade.get_uint(6).unwrap(), u32::MAX);
    assert_eq!(trade.get_string(7).unwrap(), "buy");
    assert_eq!(trade.get_double(8).unwrap(), -123.5);
    assert_eq!(trade.get_double(9).unwrap(), 2.5);
    assert_eq!(trade.get_ulong(10).unwrap(), u64::MAX);
    assert_eq!(trade.get_column_iter().nth(11).unwrap().1, &Field::Null);
    let book = &rows[1];
    assert_eq!(book.get_string(5).unwrap(), "book");
    assert_eq!(book.get_string(7).unwrap(), "ask");
    assert_eq!(book.get_string(11).unwrap(), "delete");
    assert_eq!(book.get_column_iter().nth(10).unwrap().1, &Field::Null);
    for row in &rows[2..4] {
        assert!(
            row.get_column_iter()
                .skip(5)
                .all(|(_, f)| f == &Field::Null)
        );
    }
}

#[test]
fn parquet_ranges_empty_files_and_row_group_boundary() {
    let dir = journal(100_003);
    let dest = TempDir::new().unwrap();
    let file = dest.path().join("records.parquet");
    export(dir.path(), &file, &[]);
    let reader = read(&file);
    assert_eq!(reader.metadata().num_row_groups(), 2);
    assert_eq!(reader.metadata().row_group(0).num_rows(), 100_000);
    assert_eq!(reader.metadata().row_group(1).num_rows(), 3);
    assert_eq!(
        reader
            .metadata()
            .file_metadata()
            .schema_descr()
            .num_columns(),
        5
    );
    export(dir.path(), &file, &["--from", "99999", "--to", "100001"]);
    let rows = rows(&file);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get_ulong(0).unwrap(), 99_999);
    assert_eq!(rows[2].get_ulong(0).unwrap(), 100_001);
    export(
        dir.path(),
        &file,
        &["--from", "200000", "--decode", "market-demo"],
    );
    assert_eq!(read(&file).metadata().file_metadata().num_rows(), 0);
    assert_eq!(
        read(&file)
            .metadata()
            .file_metadata()
            .schema_descr()
            .num_columns(),
        12
    );
    let empty = journal(0);
    export(empty.path(), &file, &[]);
    assert_eq!(read(&file).metadata().file_metadata().num_rows(), 0);
}

#[test]
fn parquet_requires_file_and_protects_journal() {
    let dir = journal(4);
    for extra in [vec![], vec!["--output", "-"]] {
        command()
            .args(["export", "--format", "parquet"])
            .arg(dir.path())
            .args(extra)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("--output"));
    }
    let segment = dir.path().join("00000000000000000000.tape");
    let original = fs::read(&segment).unwrap();
    let dest = TempDir::new().unwrap();
    let alias = dest.path().join("alias");
    fs::hard_link(&segment, &alias).unwrap();
    for file in [&segment, &alias, &dir.path().join("new.parquet")] {
        command()
            .args(["export", "--format", "parquet", "--output"])
            .arg(file)
            .arg(dir.path())
            .assert()
            .code(1);
    }
    assert_eq!(fs::read(segment).unwrap(), original);
}

#[test]
fn parquet_corruption_finalizes_readable_prefix() {
    let dir = journal(10);
    let path = dir.path().join("00000000000000000000.tape");
    // First trade frame: 40-byte header + 32-byte payload, aligned to 8 bytes.
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(64 + 72 + 40)).unwrap();
    let mut b = [0];
    f.read_exact(&mut b).unwrap();
    f.seek(SeekFrom::Start(64 + 72 + 40)).unwrap();
    f.write_all(&[b[0] ^ 1]).unwrap();
    let dest = TempDir::new().unwrap();
    let file = dest.path().join("prefix.parquet");
    command()
        .args([
            "export",
            "--format",
            "parquet",
            "--decode",
            "market-demo",
            "--output",
        ])
        .arg(&file)
        .arg(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("checksum mismatch"));
    assert_eq!(rows(&file).len(), 1);
    assert_eq!(rows(&file)[0].get_ulong(0).unwrap(), 0);
}

#[test]
fn jsonl_market_demo_is_opt_in_and_null_for_unknown_records() {
    let dir = journal(4);
    let result = command()
        .args(["export", "--format", "jsonl", "--decode", "market-demo"])
        .arg(dir.path())
        .assert()
        .success();
    let rows: Vec<serde_json::Value> = String::from_utf8_lossy(&result.get_output().stdout)
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(
        rows[0]["decoded"],
        serde_json::json!({"kind":"trade", "instrument_id":u32::MAX, "side":"buy", "price":-123.5, "qty":2.5, "trade_id":u64::MAX, "action":null})
    );
    assert_eq!(rows[1]["decoded"]["kind"], "book");
    assert_eq!(rows[1]["decoded"]["action"], "delete");
    assert!(rows[1]["decoded"]["trade_id"].is_null());
    for row in &rows[2..] {
        assert!(row["decoded"].is_null());
    }
    let result = command()
        .args(["export", "--format", "jsonl"])
        .arg(dir.path())
        .assert()
        .success();
    assert!(!String::from_utf8_lossy(&result.get_output().stdout).contains("decoded"));
}

#[test]
fn duckdb_parquet_export() {
    let python = std::env::var_os("TICKERTAPE_PYTHON").unwrap_or_else(|| "python3".into());
    let available = ProcessCommand::new(&python)
        .args(["-c", "import duckdb; print(duckdb.__version__)"])
        .output();
    if !available.as_ref().is_ok_and(|r| r.status.success()) {
        assert_ne!(
            std::env::var("TICKERTAPE_REQUIRE_DUCKDB").as_deref(),
            Ok("1"),
            "DuckDB required but unavailable: {available:?}"
        );
        eprintln!("SKIP duckdb_parquet_export: Python/DuckDB unavailable: {available:?}");
        return;
    }
    let dir = journal(10_000);
    assert!(
        fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "tape"))
            .count()
            >= 3
    );
    let dest = TempDir::new().unwrap();
    let file = dest.path().join("sample.parquet");
    export(dir.path(), &file, &["--decode", "market-demo"]);
    let result = ProcessCommand::new(python).arg("-c").arg(r#"
import duckdb, sys
con = duckdb.connect()
print('DuckDB', duckdb.__version__)
result = con.execute('SELECT count(*), min(seq), max(seq) FROM read_parquet(?)', [sys.argv[1]]).fetchone()
print('count/min/max:', result)
assert result == (10000, 0, 9999)
result = con.execute("SELECT count(*), min(instrument_id), max(trade_id), min(price) FROM read_parquet(?) WHERE kind = 'trade'", [sys.argv[1]]).fetchone()
print('trades/instrument/trade_id/price:', result)
assert result == (2500, 4294967295, 18446744073709551615, -123.5)
result = con.execute("SELECT count(*) FROM read_parquet(?) WHERE kind = 'book' AND side = 'ask' AND action = 'delete'", [sys.argv[1]]).fetchone()
assert result == (2500,)
result = con.execute('SELECT make_timestamp_ns(local_ts_ns::BIGINT)::VARCHAR FROM read_parquet(?) LIMIT 1', [sys.argv[1]]).fetchone()
print('local timestamp:', result)
assert result == ('2023-11-14 22:13:20',)
assert con.execute('SELECT max(venue_ts_ns) FROM read_parquet(?)', [sys.argv[1]]).fetchone() == (18446744073709551615,)
"#).arg(file).output().unwrap();
    println!("{}", String::from_utf8_lossy(&result.stdout));
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn durable_only_parquet_export() {
    let dir = journal(4);
    // Retry briefly: a child forked by a parallel test may still hold the dropped writer's lock until its exec.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut writer = loop {
        match Writer::open(dir.path(), WriterConfig::default().segment_size(65536)) {
            Err(tickertape::Error::Locked { .. }) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            other => break other.unwrap(),
        }
    };
    let dest = TempDir::new().unwrap();
    let file = dest.path().join("durable.parquet");
    export(dir.path(), &file, &["--durable-only"]);
    assert!(rows(&file).is_empty());
    writer.checkpoint().unwrap();
    writer
        .append(&NewRecord {
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: b"later",
        })
        .unwrap();
    export(dir.path(), &file, &["--durable-only"]);
    let records = rows(&file);
    assert_eq!(records.len(), 4);
    assert_eq!(records.last().unwrap().get_ulong(0).unwrap(), 3);
    export(
        dir.path(),
        &file,
        &["--durable-only", "--from", "3", "--to", "10"],
    );
    assert_eq!(rows(&file).len(), 1);
    export(dir.path(), &file, &["--durable-only", "--from", "4"]);
    assert!(rows(&file).is_empty());
}
