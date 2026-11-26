//! Kill-loop helper: repeatedly attempt retention, never modifying existing archive files. Each run moves up to four
//! segments, so that a kill can land between the removals of one run (ADR 0051).
use std::io::Write;
use tickertape::retention::{self, Action, Policy};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let journal = std::path::PathBuf::from(args.next().expect("journal"));
    let action = Action::MoveTo(args.next().expect("destination").into());
    let force_copy = args.next().is_some();
    println!("ready");
    std::io::stdout().flush().unwrap();
    loop {
        let count = tickertape::inspect::list_segments(&journal)
            .expect("list")
            .len();
        let policy = Policy::default().max_segments(count.saturating_sub(4).max(1));
        let plan = retention::plan(&journal, &policy).expect("plan");
        if force_copy {
            tickertape::fault::fail_next(tickertape::fault::Point::RetentionRename, 18);
        }
        let _ = retention::apply(&journal, &plan, &action);
    }
}
