//! Pre-mortem S2 — "relocation under load stalls the verification drain".
//!
//! The inline verification pool is bounded and its work channel blocks the
//! producer at `pool_size * 2` (`src/components/db.rs`, F3).  A relocation that
//! walks a tree occupies one worker for its whole duration; with `pool_size`
//! such units in flight the producer blocks and the entire drain stops behind
//! the slowest relocation.  Mean and p95 stay flat while the tail explodes, so
//! this test asserts **p99 and max**, never p95.
//!
//! Arm A reproduces the hazard with a deliberately slow relocation injected
//! into a saturated channel — it is the failing state.  Arm B is the shipped
//! configuration: `search_entries` passes `RelocationPolicy::Never`, so no unit
//! of work on the interactive lane can walk a tree, and the same saturated
//! channel drains inside the budget.

use kb::components::db::SEARCH_PATH_RELOCATION_POLICY;
use kb::components::verification::{verify_evidence, RelocationPolicy};
use kb::models::Evidence;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// Drain-latency budget for the interactive search path.
const BUDGET: Duration = Duration::from_millis(200);
/// How long one relocation unit is assumed to occupy a worker.
const SLOW_RELOCATION: Duration = Duration::from_millis(400);

const POOL_SIZE: usize = 4;
/// Enough tasks to saturate the `pool_size * 2` channel and block the producer.
const TASKS: usize = POOL_SIZE * 4;

const STRONG_EXCERPT: &str = "fn relocate_me(input: &str) -> usize {\n    input.as_bytes().len()\n}";

/// One unit of verification work handed to the pool.
///
/// `Evidence` is boxed so the slow variant does not pay for its size — the
/// channel holds these by value, exactly as the production pool does.
enum Unit {
    /// A real `verify_evidence` call under the given policy.
    Verify(Box<Evidence>, RelocationPolicy),
    /// A relocation that walks a tree, modelled as a fixed occupancy.
    SlowRelocation,
}

/// Replica of the `search_entries` bounded pool (`db.rs` Phase 3): bounded work
/// channel at `pool_size * 2`, unbounded result channel, producer sends all
/// tasks then drops the sender.  Returns each task's latency measured from the
/// moment the producer started sending — the quantity a caller waiting on the
/// drain actually experiences.
fn drain(units: Vec<Unit>, repo_root: &Path) -> Vec<Duration> {
    let total = units.len();
    let mut latencies = vec![Duration::ZERO; total];
    let work_chan_cap = (POOL_SIZE * 2).max(1);
    let started = Instant::now();

    std::thread::scope(|scope| {
        let (tx_work, rx_work) = crossbeam_channel::bounded::<(usize, Unit)>(work_chan_cap);
        let (tx_result, rx_result) = crossbeam_channel::unbounded::<(usize, Duration)>();

        for _ in 0..POOL_SIZE {
            let rx = rx_work.clone();
            let tx = tx_result.clone();
            scope.spawn(move || {
                for (idx, unit) in rx {
                    match unit {
                        Unit::Verify(ev, policy) => {
                            let _ = verify_evidence(&ev, repo_root, policy);
                        }
                        Unit::SlowRelocation => std::thread::sleep(SLOW_RELOCATION),
                    }
                    let _ = tx.send((idx, started.elapsed()));
                }
            });
        }
        drop(tx_result);
        drop(rx_work);

        for (idx, unit) in units.into_iter().enumerate() {
            let _ = tx_work.send((idx, unit));
        }
        drop(tx_work);

        for (idx, elapsed) in rx_result {
            latencies[idx] = elapsed;
        }
    });

    latencies
}

/// p99 by nearest-rank; with `TASKS` samples this is the slowest observation,
/// which is the point: the hazard lives in the tail.
fn quantile(latencies: &[Duration], q: f64) -> Duration {
    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // The cited file no longer holds the excerpt, so every row is
    // relocation-eligible: only the policy keeps the walk off this lane.
    fs::write(dir.path().join("old.rs"), "// the code moved away\n".repeat(4)).unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/new.rs"),
        format!("// head\n{STRONG_EXCERPT}\n"),
    )
    .unwrap();
    dir
}

fn relocation_eligible_evidence(i: usize) -> Box<Evidence> {
    let mut h = Sha256::new();
    h.update(STRONG_EXCERPT.as_bytes());
    Box::new(Evidence {
        id: format!("ev-{i}"),
        entry_id: format!("entry-{i}"),
        kind: "code".to_string(),
        citation_path: Some(format!("old.rs:0-{}", STRONG_EXCERPT.len())),
        citation_sha: None,
        citation_hash: format!("{:x}", h.finalize()),
        citation_excerpt: Some(STRONG_EXCERPT.to_string()),
        derived_from: None,
        recorded_at: None,
    })
}

/// Arm A — the hazard. With `pool_size` relocations in flight the bounded work
/// channel blocks the producer and every queued task inherits the stall.
#[test]
fn slow_relocation_under_saturation_blows_the_tail() {
    let repo = fixture_repo();
    let mut units: Vec<Unit> = (0..POOL_SIZE).map(|_| Unit::SlowRelocation).collect();
    units.extend(
        (POOL_SIZE..TASKS)
            .map(|i| Unit::Verify(relocation_eligible_evidence(i), RelocationPolicy::Never)),
    );

    let latencies = drain(units, repo.path());
    let p99 = quantile(&latencies, 0.99);
    let max = latencies.iter().copied().max().unwrap();

    assert!(
        p99 > BUDGET && max > BUDGET,
        "expected the injected relocations to blow the drain budget \
         (p99={p99:?}, max={max:?}, budget={BUDGET:?}) — if this stops holding, \
         the hazard model is wrong and Arm B proves nothing"
    );
}

/// Arm B — the shipped configuration. Every unit on the interactive lane runs
/// under `RelocationPolicy::Never`, so the same saturated channel drains well
/// inside the budget at p99 AND max.
#[test]
fn search_path_drain_stays_inside_the_tail_budget() {
    assert_eq!(
        SEARCH_PATH_RELOCATION_POLICY,
        RelocationPolicy::Never,
        "this budget only holds because the search path never relocates"
    );

    let repo = fixture_repo();
    let units: Vec<Unit> = (0..TASKS)
        .map(|i| Unit::Verify(relocation_eligible_evidence(i), SEARCH_PATH_RELOCATION_POLICY))
        .collect();

    let latencies = drain(units, repo.path());
    let p99 = quantile(&latencies, 0.99);
    let max = latencies.iter().copied().max().unwrap();

    assert!(
        p99 < BUDGET,
        "p99 drain latency {p99:?} exceeded budget {BUDGET:?}"
    );
    assert!(
        max < BUDGET,
        "max drain latency {max:?} exceeded budget {BUDGET:?}"
    );
}
