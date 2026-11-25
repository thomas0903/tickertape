//! `tickertape::latency` against `hdrhistogram`, which the benchmark tools use (ADR 0047 → Tests): the same values, and
//! quantiles within the two precisions of each other.

use tickertape::latency::Recorder;

/// SplitMix64.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[test]
fn quantiles_agree_with_hdrhistogram() {
    for seed in 1..=8u64 {
        let mut state = seed;
        let mut ours = Recorder::new();
        let mut theirs = hdrhistogram::Histogram::<u64>::new_with_bounds(1, 1 << 40, 3).unwrap();
        let n = [1, 2, 10, 999, 1_000, 50_000, 100_001, 200_000][seed as usize - 1];
        for _ in 0..n {
            // Every magnitude up to 2^40 ns, evenly.
            let r = next(&mut state);
            let value = (1u64 << (r % 40)) + (next(&mut state) & ((1 << (r % 40)) - 1));
            ours.record_ns(value);
            theirs.record(value).unwrap();
        }
        let ours = ours.view().snapshot();
        assert_eq!(ours.count(), theirs.len());
        for q in [0.0, 0.001, 0.1, 0.25, 0.5, 0.9, 0.99, 0.999, 0.9999, 1.0] {
            let a = ours.value_at_quantile(q).unwrap().as_nanos() as f64;
            // Also an upper bound, of a bucket at most 1/1000 wide.
            let b = theirs.value_at_quantile(q) as f64;
            assert!(
                a <= b * (1.0 + 1.0 / 64.0) && b <= a * 1.001 + 1.0,
                "seed {seed}, q {q}: {a} and {b}"
            );
        }
        let max = ours.max().unwrap().as_nanos() as u64;
        assert!(theirs.equivalent(max, theirs.max()));
        let mean = ours.mean().unwrap().as_nanos() as f64;
        assert!((mean / theirs.mean() - 1.0).abs() < 0.001);
    }
}
