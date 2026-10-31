//! Writer lock tests (crash contract C5; C3 with a real process kill is in the M4 kill tests).

use tickertape::{Error, Writer};
use tickertape_tests::small_config;

#[test]
fn second_writer_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = Writer::open(dir.path(), small_config()).unwrap();
    tickertape_tests::append_n(&mut first, 3);
    let before: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();

    match Writer::open(dir.path(), small_config()) {
        Err(Error::Locked { pid, .. }) => assert_eq!(pid, Some(std::process::id())),
        other => panic!("expected Locked, got {other:?}"),
    }
    let after: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        before.len(),
        after.len(),
        "a refused writer must not create or remove files"
    );

    tickertape_tests::append_n(&mut first, 1);
    drop(first);
    let reopened = Writer::open(dir.path(), small_config()).unwrap();
    assert_eq!(reopened.next_seq(), 4);
}
