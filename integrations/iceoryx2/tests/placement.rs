//! Where descriptors lie (review round 2): with small segments, every segment the recorder writes holds the
//! descriptors of the records in it, at every message size the journal accepts, so that every segment left after
//! retention or `tickertape archive` decodes on its own with `info`, `replay` and `export`; and the replayer's
//! live-target guard knows what the journal records wherever the descriptors happen to lie.

#![cfg(feature = "cli")]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use tickertape::format::{align8, frame_len};

const SEGMENT: u64 = 65_536;
/// Service names of one length, so that every recording's descriptor has the same length.
const NAME_LEN: usize = 72;
/// The data record's own header plus the publisher's `TickHeader` user header.
const DATA_OVERHEAD: usize = tickertape_iceoryx2::layout::DATA_HEADER_LEN + 16;

/// Size of a record with this payload, as it occupies a segment.
fn frame(payload: usize) -> u64 {
    align8(u64::from(frame_len(payload).unwrap()))
}

/// The publisher example sending `count` `[u8]` messages of `size` bytes, without dropping any.
fn publisher(service: &str, size: usize, count: u64, rate: u64) -> Proc {
    // Small buffers for large messages: iceoryx2 preallocates every sample the service can need.
    let buffer = ((4usize << 20) / (size + 16)).clamp(4, 4096).to_string();
    Proc::spawn(
        &example("publisher"),
        &[
            "--service",
            service,
            "--kind",
            "bytes",
            "--size",
            &size.to_string(),
            "--count",
            &count.to_string(),
            "--rate",
            &rate.to_string(),
            "--wait-subscribers",
            "1",
            "--wait-secs",
            "60",
            "--blocking",
            "--buffer",
            &buffer,
        ],
    )
}

/// The CLI recorder with 64 KiB segments.
fn recorder(journal: &Path, services: &[&str], max_messages: Option<u64>) -> Proc {
    let mut args = vec![
        "record".to_string(),
        journal.to_str().unwrap().to_string(),
        "--segment-size".into(),
        SEGMENT.to_string(),
        "--counter".into(),
        "header:0:8".into(),
        "--checkpoint-interval-ms".into(),
        "50".into(),
        "--json".into(),
    ];
    for s in services {
        args.push("--service".into());
        args.push(s.to_string());
    }
    if let Some(n) = max_messages {
        args.push("--max-messages".into());
        args.push(n.to_string());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Proc::spawn(&cli(), &args)
}

/// Records `count` messages of `size` bytes of one new service; returns the journal and the service name.
fn record_one(dir: &Path, size: usize, count: u64) -> (PathBuf, String) {
    let journal = dir.join(format!("j-{size}"));
    let service = unique_of_len(&format!("place-{size}"), NAME_LEN);
    let rec = recorder(&journal, &[&service], Some(count));
    let publisher = publisher(&service, size, count, 20_000);
    let (status, out, err) = rec.wait(SECS(120));
    assert!(status.success(), "record {size}: {err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["messages"], count, "{report}");
    assert_eq!(report["services"][0]["gaps"], 0, "{report}");
    let (status, _, err) = publisher.wait(SECS(60));
    assert!(status.success(), "publisher {size}: {err}");
    (journal, service)
}

fn json(args: &[&str]) -> serde_json::Value {
    serde_json::from_str(&run_ok(&cli(), args, SECS(120))).unwrap()
}

/// Asserts that `info`, `replay` and `export` decode every data record of the journal in `journal` except the
/// `undescribed` ones, and skip exactly those.
fn assert_readers_decode(journal: &Path, placement: &Placement, out_dir: &Path) {
    let path = journal.to_str().unwrap();
    let decodable = placement.data + placement.loss - placement.undescribed;
    let info = json(&["info", path, "--json"]);
    assert_eq!(
        info["undescribed_records"], placement.undescribed,
        "{info} {placement:?}"
    );
    let messages: u64 = info["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["messages"].as_u64().unwrap() + s["loss_records"].as_u64().unwrap())
        .sum();
    assert_eq!(messages, decodable, "{info} {placement:?}");
    assert_eq!(info["filler_records"], placement.fillers, "{info}");

    let prefix = format!("{}/", unique("placed-target"));
    let replay = json(&[
        "replay",
        path,
        "--json",
        "--linger-ms",
        "0",
        "--prefix",
        &prefix,
    ]);
    assert_eq!(replay["undescribed"], placement.undescribed, "{replay}");
    assert_eq!(
        replay["messages"].as_u64().unwrap() + placement.loss,
        decodable,
        "{replay} {placement:?}"
    );

    // Every service with messages left exports them all (a service with only a descriptor left has nothing to export).
    for s in info["services"].as_array().unwrap() {
        if s["messages"] == 0 {
            continue;
        }
        let name = s["name"].as_str().unwrap();
        let file = out_dir.join(format!("{}.iox2", unique("export").replace('/', "_")));
        let export = json(&[
            "export",
            path,
            "--service",
            name,
            "--output",
            file.to_str().unwrap(),
            "--json",
        ]);
        assert_eq!(export["messages"], s["messages"], "{export}");
        assert_eq!(export["undescribed"], placement.undescribed, "{export}");
    }
}

/// Removes all but the newest two segments of copies of `journal`, once by deleting them (retention) and once by
/// moving them away (archiving), and checks that every remaining record decodes; returns the placement found in
/// the deleted-from copy.
fn assert_survives_removal(journal: &Path, dir: &Path) -> Placement {
    let mut retained = None;
    for (k, mode) in ["retention", "archive"].iter().enumerate() {
        let copy = dir.join(format!("{}-{mode}", journal.file_name().unwrap().display()));
        copy_journal(journal, &copy);
        let to = dir.join(format!(
            "{}-{mode}-archived",
            journal.file_name().unwrap().display()
        ));
        std::fs::create_dir_all(&to).unwrap();
        let removed = archive(&copy, 2, (k == 1).then_some(to.as_path()));
        assert!(
            removed > 0,
            "{mode}: nothing removed from {}",
            copy.display()
        );
        let placement = check_placement(&copy);
        assert_eq!(placement.segments, 2, "{mode}");
        assert_readers_decode(&copy, &placement, dir);
        retained.get_or_insert(placement);
    }
    retained.unwrap()
}

/// The descriptor length of the recording in `journal` (its first record).
fn descriptor_len(journal: &Path) -> usize {
    let mut reader =
        tickertape::Reader::open(journal, tickertape::ReaderConfig::default()).unwrap();
    let first = reader.try_next().unwrap().unwrap();
    assert_eq!(
        first.schema_id,
        tickertape_iceoryx2::layout::SCHEMA_DESCRIPTOR
    );
    first.payload.len()
}

/// Enough messages of `size` bytes for about four and a half segments (at least eight).
fn count_for(size: usize) -> u64 {
    ((SEGMENT * 9 / 2) / frame(DATA_OVERHEAD + size)).max(8)
}

/// One service, messages from 1 byte to the largest a 64 KiB segment holds, including about 1 KiB (which used to
/// leave the descriptors at the end of the previous segment) and the sizes around each boundary: the largest
/// message that shares a segment with its descriptor, the next ones (each then has a segment of its own, the
/// documented exception), and the largest the journal accepts.
#[test]
fn every_segment_holds_the_descriptors_of_its_records_at_every_size() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    // About 1 KiB first: it also tells the descriptor length, which is the same for every recording here.
    let (journal, _) = record_one(dir.path(), 1000, count_for(1000));
    let d = frame(descriptor_len(&journal));
    let area = SEGMENT - 8 - 64;
    let largest = (tickertape::format::max_payload_len(SEGMENT) as usize) - DATA_OVERHEAD;
    let largest_shared = (area - d) as usize - (frame(DATA_OVERHEAD) as usize);
    assert_eq!(frame(DATA_OVERHEAD + largest_shared) + d, area);
    let mut sizes = vec![(1000, journal)];
    for size in [
        1,
        7,
        100,
        1008,
        1024,
        4000,
        16_000,
        32_700,
        largest_shared - 8,
        largest_shared - 1,
        largest_shared,
        largest_shared + 1,
        largest - 1,
        largest,
    ] {
        sizes.push((size, record_one(dir.path(), size, count_for(size)).0));
    }
    for (size, journal) in &sizes {
        let size = *size;
        let placement = check_placement(journal);
        let records = placement.data + placement.loss;
        assert!(placement.segments >= 4, "{size}: {placement:?}");
        assert_eq!(placement.not_leading, 0, "{size}: {placement:?}");
        if size <= largest_shared {
            assert_eq!(placement.exceptions, 0, "{size}: {placement:?}");
        } else {
            assert_eq!(placement.exceptions, records, "{size}: {placement:?}");
        }
        assert_readers_decode(journal, &placement, dir.path());
        let retained = assert_survives_removal(journal, dir.path());
        if size <= largest_shared {
            assert_eq!(retained.undescribed, 0, "{size}: {retained:?}");
        }
    }
}

/// Two services whose descriptors differ in size, the second opened in the middle of the run: each segment holds
/// the descriptors of the records in it, and the segments the recorder starts open with the descriptors of every
/// service open at that point. Removing older segments leaves nothing undescribed.
#[test]
fn two_services_one_opened_mid_run() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let small = unique_of_len("small", 40);
    let large = unique_of_len("large", 180);
    let (n_small, n_large) = (600, 400);
    let rec = recorder(&journal, &[&small, &large], Some(n_small + n_large));
    let first = publisher(&small, 1000, n_small, 1000);
    wait_until(SECS(60), "three segments of the first service", || {
        tickertape::inspect::list_segments(&journal).is_ok_and(|s| s.len() >= 3)
    });
    let second = publisher(&large, 900, n_large, 2000);
    let (status, out, err) = rec.wait(SECS(120));
    assert!(status.success(), "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["messages"], n_small + n_large, "{report}");
    for p in [first, second] {
        let (status, _, err) = p.wait(SECS(60));
        assert!(status.success(), "{err}");
    }
    let placement = check_placement(&journal);
    assert_eq!(placement.exceptions, 0, "{placement:?}");
    assert!(placement.segments >= 8, "{placement:?}");
    // Only the segment in which the second service was opened has its descriptor after the start.
    assert!(placement.not_leading <= 1, "{placement:?}");
    assert_readers_decode(&journal, &placement, dir.path());
    let retained = assert_survives_removal(&journal, dir.path());
    assert_eq!(retained.undescribed, 0, "{retained:?}");
}

/// With messages of about 1 KiB (1,008 bytes: with these descriptors, each full segment has room left for a
/// descriptor but not for another message) and a live recorder, the replayer refuses to publish onto names the journal is
/// recording, whichever segment holds their descriptors.
#[test]
fn live_target_guard_with_large_messages() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let service = unique_of_len("live-large", NAME_LEN);
    let rec = recorder(&journal, &[&service], None);
    let publisher = publisher(&service, 1008, 0, 500);
    wait_until(SECS(60), "four segments", || {
        tickertape::inspect::list_segments(&journal).is_ok_and(|s| s.len() >= 4)
    });
    let path = journal.to_str().unwrap();
    let map = format!("{service}={service}");
    for _ in 0..5 {
        for args in [
            vec!["--original-names"],
            vec!["--map", map.as_str(), "--original-names"],
        ] {
            let (status, _, err) = run(
                &cli(),
                &[&["replay", path, "--linger-ms", "0"][..], &args].concat(),
                SECS(60),
            );
            assert_eq!(status.code(), Some(1), "{args:?}: {err}");
            assert!(
                err.contains("recording that service right now"),
                "{args:?}: {err}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    rec.signal("TERM");
    let (status, _, err) = rec.wait(SECS(30));
    assert!(status.success(), "{err}");
    drop(publisher);
    let placement = check_placement(&journal);
    assert_eq!(placement.exceptions, 0, "{placement:?}");
    assert_eq!(placement.not_leading, 0, "{placement:?}");
}

/// An import longer than a segment repeats its descriptor as the recorder does, so it survives removal of older
/// segments too.
#[test]
fn an_import_places_its_descriptor_in_every_segment() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let mut file = String::from(
        "service_name = \"imported/large\"\n\n[iceoryx2_version]\nmajor = 0\nminor = 10\npatch = 0\n\n[details]\n\
         file_format_version = 1\nmessaging_pattern = \"PublishSubscribe\"\n\n[details.types.payload]\n\
         variant = \"Dynamic\"\ntype_name = \"u8\"\nsize = 1\nalignment = 1\n\n\
         [details.types.user_header]\nvariant = \"FixedSize\"\ntype_name = \"()\"\nsize = 0\nalignment = 1\n\n\
         [details.types.system_header]\nvariant = \"FixedSize\"\ntype_name = \"S\"\nsize = 4\nalignment = 4\n\n\n\
         ### Recorded Data Start ###\n",
    );
    for i in 0..400u32 {
        let payload: String = (0..1008)
            .map(|k| format!("{:02x} ", (i + k) as u8))
            .collect();
        file.push_str(&format!(
            "time:     {i}\nsys head: 00 00 00 00 \nusr head: \npayload:  {payload}\n\n"
        ));
    }
    let input = dir.path().join("large.txt");
    std::fs::write(&input, file).unwrap();
    let report = tickertape_iceoryx2::upstream::import(
        &input,
        &journal,
        &tickertape_iceoryx2::upstream::ImportOptions::default()
            .start_unix_ns(1_700_000_000_000_000_000)
            .writer_config(
                tickertape::WriterConfig::default()
                    .segment_size(SEGMENT)
                    .prefault(false),
            ),
    )
    .unwrap();
    assert_eq!(report.messages, 400);
    let placement = check_placement(&journal);
    assert!(placement.segments >= 6, "{placement:?}");
    assert_eq!(placement.exceptions, 0, "{placement:?}");
    assert_eq!(placement.not_leading, 0, "{placement:?}");
    assert!(placement.fillers > 0, "{placement:?}");
    assert_readers_decode(&journal, &placement, dir.path());
    let retained = assert_survives_removal(&journal, dir.path());
    assert_eq!(retained.undescribed, 0, "{retained:?}");
}
