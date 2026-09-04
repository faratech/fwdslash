//! Throughput smoke test for the resolver. `#[ignore]`d by default — timing
//! thresholds flake on loaded machines and in debug builds — so run it
//! explicitly:
//!
//! ```text
//! cargo test -p fsw-path --release -- --ignored --nocapture
//! ```
//!
//! The threshold is deliberately loose (0.5 M resolves/s): this catches an
//! order-of-magnitude regression, not jitter. The deterministic contract that
//! actually gates commits lives in `allocations.rs`.

use std::time::Instant;

mod common;

use fsw_path::{RenderBuf, resolve};

#[test]
#[ignore = "timing smoke; run with --release -- --ignored"]
fn resolver_throughput() {
    let mut buf = RenderBuf::with_capacity(512);
    let contexts: Vec<_> = common::contexts().collect();

    // Warm-up.
    for (input, ctx) in &contexts {
        let _ = resolve(input, ctx, &mut buf);
    }

    const PASSES: u32 = 1_000;
    let started = Instant::now();
    let mut checksum = 0_u64;
    for _ in 0..PASSES {
        for (input, ctx) in &contexts {
            checksum += resolve(input, ctx, &mut buf).is_ok() as u64;
        }
    }
    let elapsed = started.elapsed();
    let total = u64::from(PASSES) * contexts.len() as u64;
    let per_op = elapsed.as_nanos() / u128::from(total);

    println!("{total} resolves in {elapsed:?} ({per_op} ns/op)");
    println!("(checksum {checksum}; ok-cases only, just to block dead-code elimination)");

    // ~0.5 M resolves/s floor. Measured: tens of ns/op in release; debug
    // builds are not a valid run for this test.
    let per_op_u64 = u64::try_from(per_op).unwrap_or(u64::MAX);
    assert!(
        per_op_u64 < 2_000,
        "resolver throughput collapsed: {per_op} ns/op (floor is 2,000)"
    );
}
