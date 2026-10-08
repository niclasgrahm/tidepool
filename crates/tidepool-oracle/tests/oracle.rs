//! Tidepool against DuckDB, on random projects.
//!
//! `TIDEPOOL_ORACLE_SEEDS` sets how many seeds to run (default 40, 25 queries
//! each: 1000 comparisons, fast enough for every `just ci`); the nightly run
//! and the M3 exit criterion use far more. `TIDEPOOL_ORACLE_SEED=<n>` runs
//! just that one, to reproduce a failure.

#[test]
fn tidepool_agrees_with_duckdb() {
    let queries = 25;
    let seeds: Vec<u64> = if let Ok(seed) = std::env::var("TIDEPOOL_ORACLE_SEED") {
        vec![seed.parse().unwrap_or(0)]
    } else {
        let count: u64 = std::env::var("TIDEPOOL_ORACLE_SEEDS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(40);
        (0..count).collect()
    };
    let mut compared = 0;
    for seed in seeds {
        match tidepool_oracle::check_seed(seed, queries) {
            Ok(n) => compared += n,
            Err(report) => panic!("{report}"),
        }
    }
    println!("{compared} queries agreed");
}

/// The incremental oracle: live answers checked against DuckDB after every
/// epoch. `TIDEPOOL_ORACLE_SEEDS` / `TIDEPOOL_ORACLE_SEED` as above.
#[test]
fn live_answers_agree_with_duckdb_after_every_epoch() {
    let seeds: Vec<u64> = if let Ok(seed) = std::env::var("TIDEPOOL_ORACLE_SEED") {
        vec![seed.parse().unwrap_or(0)]
    } else {
        let count: u64 = std::env::var("TIDEPOOL_ORACLE_SEEDS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(40);
        (0..count).collect()
    };
    let mut compared = 0;
    for seed in seeds {
        match tidepool_oracle::check_live(seed, 6) {
            Ok(n) => compared += n,
            Err(report) => panic!("{report}"),
        }
    }
    println!("{compared} live answers agreed");
}

/// The model-change oracle (M7): data written under an earlier version of
/// each project, the model changed midway with a background build and
/// writes going on, then queries checked against DuckDB.
/// `TIDEPOOL_ORACLE_SEEDS` / `TIDEPOOL_ORACLE_SEED` as above.
#[test]
fn answers_agree_with_duckdb_after_a_model_change() {
    let seeds: Vec<u64> = if let Ok(seed) = std::env::var("TIDEPOOL_ORACLE_SEED") {
        vec![seed.parse().unwrap_or(0)]
    } else {
        let count: u64 = std::env::var("TIDEPOOL_ORACLE_SEEDS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(40);
        (0..count).collect()
    };
    let mut compared = 0;
    for seed in seeds {
        match tidepool_oracle::check_migration(seed, 10) {
            Ok(n) => compared += n,
            Err(report) => panic!("{report}"),
        }
    }
    println!("{compared} queries agreed after a model change");
}
