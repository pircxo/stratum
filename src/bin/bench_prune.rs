//! A real, measured benchmark — not a claim. Generates a synthetic
//! table, then runs the same selective query with zone-map pruning on
//! and off, reporting exactly how many row groups each run actually
//! touched and how long each took. The numbers this prints are whatever
//! this run on this machine actually produced; see ARCHITECTURE.md for
//! what they were on the machine this project was built on, caveats
//! included (notably: a 2-core sandbox, so the parallel-scan numbers
//! understate what typical infra hardware would show).
//!
//! Run with `cargo run --release --bin bench_prune` — in debug mode the
//! unoptimized decode loop dominates and the numbers are meaningless.

use std::time::Instant;

use anyhow::Result;
use rand::Rng;

use stratum::query::{execute, parse, ExecuteOptions};
use stratum::storage::{DataType, TableReader, TableWriter};
use stratum::{ColumnSchema, Value};

const ROW_COUNT: u64 = 2_000_000;
const VALUE_RANGE: i64 = 1_000_000;
/// Chosen so the predicate below matches roughly 0.1% of rows — a
/// realistically selective analytical filter, the case pruning is
/// supposed to help most.
const THRESHOLD: i64 = 999_000;

fn main() -> Result<()> {
    let path = std::env::temp_dir().join("stratum_bench_prune.strat");

    println!("Generating {ROW_COUNT} synthetic rows...");
    generate_dataset(&path, ROW_COUNT)?;
    println!("Wrote {}\n", path.display());

    let reader = TableReader::open(&path)?;
    let sql = format!("SELECT id FROM t WHERE value > {THRESHOLD}");
    let stmt = parse(&sql).expect("hand-written query is valid");

    println!("Query: {sql}\n");

    let pruned = run_and_time(
        &reader,
        &stmt,
        ExecuteOptions {
            use_pruning: true,
            parallel: true,
        },
        "pruning + parallel",
    );
    let unpruned = run_and_time(
        &reader,
        &stmt,
        ExecuteOptions {
            use_pruning: false,
            parallel: true,
        },
        "full scan, parallel",
    );
    let unpruned_serial = run_and_time(
        &reader,
        &stmt,
        ExecuteOptions {
            use_pruning: false,
            parallel: false,
        },
        "full scan, serial",
    );

    println!();
    println!(
        "Zone-map pruning skipped {}/{} row groups ({:.1}% of the table never decoded).",
        unpruned.1.row_groups_total - pruned.1.row_groups_scanned,
        pruned.1.row_groups_total,
        100.0 * (unpruned.1.row_groups_total - pruned.1.row_groups_scanned) as f64
            / pruned.1.row_groups_total as f64
    );
    if pruned.0 > 0.0 {
        println!(
            "Pruned+parallel was {:.1}x faster than a full serial scan on this run.",
            unpruned_serial.0 / pruned.0
        );
    }
    println!(
        "\n(Numbers are from this run, on this machine — see ARCHITECTURE.md for discussion.)"
    );

    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn run_and_time(
    reader: &TableReader,
    stmt: &stratum::query::SelectStmt,
    opts: ExecuteOptions,
    label: &str,
) -> (f64, stratum::query::ScanStats) {
    let start = Instant::now();
    let (result, stats) = execute(reader, stmt, &opts).expect("benchmark query must succeed");
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{label:22} {elapsed:8.2} ms   row groups scanned: {:4}/{:<4}   rows returned: {}",
        stats.row_groups_scanned,
        stats.row_groups_total,
        result.rows.len()
    );
    (elapsed, stats)
}

/// `value` trends upward with row order plus a small amount of noise —
/// deliberately, not an oversight. A column whose values are scattered
/// uniformly at random across the *whole* domain within every single
/// row group (e.g. a pure `rng.gen_range(0..MAX)` with no correlation to
/// insertion order) defeats zone-map pruning entirely: with 8192 random
/// draws per block, almost every block's min/max already spans nearly
/// the full range, so there's nothing to prune regardless of the query.
/// That's not a bug in pruning, it's the actual, well-known limit of the
/// technique — Snowflake's own docs on micro-partition pruning make the
/// same point about clustering. Real columns this technique helps with
/// (timestamps, auto-increment ids, anything correlated with insert
/// order) look like this one: locally narrow-range, globally wide-range.
fn generate_dataset(path: &std::path::Path, row_count: u64) -> Result<()> {
    let schema = vec![
        ColumnSchema {
            name: "id".to_string(),
            dtype: DataType::Int64,
        },
        ColumnSchema {
            name: "value".to_string(),
            dtype: DataType::Int64,
        },
    ];
    let mut writer = TableWriter::create(path, schema)?;
    let mut rng = rand::thread_rng();
    let noise = VALUE_RANGE / 200; // local jitter, much narrower than the full range
    for id in 0..row_count {
        let trend = (id as i128 * VALUE_RANGE as i128 / row_count as i128) as i64;
        let value = (trend + rng.gen_range(-noise..=noise)).clamp(0, VALUE_RANGE - 1);
        writer.add_row(vec![Value::Int64(id as i64), Value::Int64(value)])?;
    }
    writer.finish()?;
    Ok(())
}
