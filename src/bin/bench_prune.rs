//! A real, measured benchmark — not a claim. Generates the same synthetic
//! data in two physical layouts and reports what the storage format and
//! executor actually do with each: bytes per column under adaptive
//! encoding, and the same queries with pruning/parallelism on and off —
//! exactly how many row groups and bytes each run touched and how long it
//! took (median of `--iterations`, after warm-up). Every execution is
//! checked against a full serial scan. See ARCHITECTURE.md for one recorded
//! run and its caveats.
//!
//! - **Clustered**: `value` trends upward with row order plus local
//!   jitter, like a timestamp or an auto-increment id. Locally
//!   narrow-range, globally wide-range — what both zone-map pruning and
//!   bit-packing need.
//! - **Random**: `value` is uniform over the whole range with no relation
//!   to row order. Every row group's min/max spans nearly the whole
//!   domain, so pruning has nothing to work with. That's the real limit of
//!   the technique, not a bug (Snowflake's docs make the same point under
//!   "clustering").
//!
//! Run with `cargo run --release --bin bench_prune` — in debug mode the
//! unoptimized decode loop dominates and the numbers are meaningless.

use std::path::Path;
use std::time::Instant;

use anyhow::{ensure, Result};
use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use stratum::query::ast::{CompareOp, Expr, Literal};
use stratum::query::{execute, parse, ExecuteOptions, ScanStats};
use stratum::storage::{DataType, TableReader, TableWriter};
use stratum::{ColumnSchema, Value};

const VALUE_RANGE: i64 = 1_000_000;
/// Chosen so the predicate matches roughly 0.1% of rows — a
/// realistically selective analytical filter, the case pruning is
/// supposed to help most.
const THRESHOLD: i64 = 999_000;

#[derive(Parser)]
#[command(about = "Measure zone-map pruning, encoding and parallel scans on synthetic data")]
struct Args {
    /// Rows to generate per layout.
    #[arg(long, default_value_t = 2_000_000)]
    rows: u64,
    /// Runs per query after warm-up; the median time is reported.
    #[arg(long, default_value_t = 7)]
    iterations: usize,
    /// Worker threads for parallel scans (default: all cores).
    #[arg(long)]
    threads: Option<usize>,
    /// Seed for deterministic data generation.
    #[arg(long, default_value_t = 42)]
    seed: u64,
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Clustered,
    Random,
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(args.rows > 0, "--rows must be at least 1");
    ensure!(args.rows <= i64::MAX as u64, "--rows must fit Int64 ids");
    ensure!(args.iterations > 0, "--iterations must be at least 1");
    if let Some(n) = args.threads {
        ensure!(n > 0, "--threads must be at least 1");
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()?;
    }
    println!(
        "{} rows per layout, median of {} runs after warm-up, {} threads, seed {}\n",
        args.rows,
        args.iterations,
        rayon::current_num_threads(),
        args.seed
    );

    let directory = tempfile::tempdir()?;
    for layout in [Layout::Clustered, Layout::Random] {
        let path = directory.path().join(format!("{layout:?}.strat"));
        generate(&path, layout, args.rows, args.seed)?;
        let result = bench_layout(&path, layout, &args);
        let _ = std::fs::remove_file(&path);
        result?;
    }
    println!("(Numbers are from this run, on this machine — see ARCHITECTURE.md for discussion.)");
    Ok(())
}

fn bench_layout(path: &Path, layout: Layout, args: &Args) -> Result<()> {
    let reader = TableReader::open(path)?;
    println!("== Layout: {layout:?} ==");
    println!(
        "{:.1} MiB on disk, {} row groups",
        reader.file_len() as f64 / (1024.0 * 1024.0),
        reader.row_groups().len()
    );
    for (i, col) in reader.schema().iter().enumerate() {
        let bytes: u64 = reader
            .row_groups()
            .iter()
            .map(|rg| rg.chunks[i].length)
            .sum();
        let per_row = bytes as f64 / args.rows as f64;
        let note = match col.dtype {
            DataType::Int64 => format!("{:.1}x smaller than 8-byte plain", 8.0 / per_row),
            DataType::Utf8 => "dictionary, bit-packed indices".to_string(),
        };
        println!("  {:8} {per_row:>7.3} bytes/row  {note}", col.name);
    }

    let opts = |use_pruning, parallel| ExecuteOptions {
        use_pruning,
        parallel,
    };
    let selective = format!("SELECT id FROM t WHERE value > {THRESHOLD}");
    println!("\n{selective}");
    let pruned = run(
        &reader,
        &selective,
        opts(true, true),
        "pruning + parallel",
        args,
    );
    run(
        &reader,
        &selective,
        opts(false, true),
        "full scan, parallel",
        args,
    );
    let serial = run(
        &reader,
        &selective,
        opts(false, false),
        "full scan, serial",
        args,
    );
    println!(
        "Pruned {}/{} groups; {:.1}x faster than a full serial scan",
        pruned.1.row_groups_total - pruned.1.row_groups_scanned,
        pruned.1.row_groups_total,
        serial.0 / pruned.0
    );

    if let Layout::Clustered = layout {
        let group_by = "SELECT sensor, COUNT(*) AS n, AVG(value) FROM t GROUP BY sensor";
        println!("\n{group_by}");
        let par = run(
            &reader,
            group_by,
            opts(true, true),
            "parallel partials",
            args,
        );
        let ser = run(&reader, group_by, opts(true, false), "serial", args);
        println!(
            "Parallel aggregation {:.1}x faster than serial",
            ser.0 / par.0
        );

        let meta = "SELECT COUNT(*), MIN(value), MAX(value) FROM t";
        println!("\n{meta}");
        run(
            &reader,
            meta,
            opts(true, true),
            "from footer metadata",
            args,
        );
    }
    println!();
    Ok(())
}

fn run(
    reader: &TableReader,
    sql: &str,
    opts: ExecuteOptions,
    label: &str,
    args: &Args,
) -> (f64, ScanStats) {
    let stmt = parse(sql).expect("hand-written query is valid");
    let mut reference_stmt = stmt.clone();
    // A true predicate prevents the metadata-only fast path, so even
    // COUNT/MIN/MAX are checked against decoded input outside the timer.
    if reference_stmt.filter.is_none() {
        reference_stmt.filter = Some(Expr::compare("id", CompareOp::Ge, Literal::Int(i64::MIN)));
    }
    let (reference, _) = execute(
        reader,
        &reference_stmt,
        &ExecuteOptions {
            use_pruning: false,
            parallel: false,
        },
    )
    .expect("reference scan must succeed");
    let (warmup, _) = execute(reader, &stmt, &opts).expect("warm-up must succeed");
    assert_eq!(
        warmup.rows, reference.rows,
        "warm-up disagrees with reference"
    );
    let mut timings = Vec::with_capacity(args.iterations);
    let mut last = None;
    for _ in 0..args.iterations {
        let start = Instant::now();
        let out = execute(reader, &stmt, &opts).expect("benchmark query must succeed");
        timings.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(
            out.0.columns, reference.columns,
            "result columns disagree with reference"
        );
        assert_eq!(
            out.0.rows, reference.rows,
            "result rows disagree with reference"
        );
        last = Some(out);
    }
    let (result, stats) = last.expect("at least one iteration");
    timings.sort_by(f64::total_cmp);
    let middle = timings.len() / 2;
    let median = if timings.len().is_multiple_of(2) {
        (timings[middle - 1] + timings[middle]) / 2.0
    } else {
        timings[middle]
    };
    println!(
        "  {label:22} {median:8.2} ms   groups scanned {:>4}/{:<4}  read {:>9}  rows {}",
        stats.row_groups_scanned,
        stats.row_groups_total,
        format!("{:.2} MiB", stats.bytes_read as f64 / (1024.0 * 1024.0)),
        result.rows.len()
    );
    (median, stats)
}

/// `id` 0, 1, 2, ... (the ideal delta-encoding case), `value` per the
/// layout, and `sensor`, one of 16 strings (the dictionary case). Seeded,
/// so every run generates identical data.
fn generate(path: &Path, layout: Layout, rows: u64, seed: u64) -> Result<()> {
    let col = |name: &str, dtype| ColumnSchema {
        name: name.to_string(),
        dtype,
    };
    let schema = vec![
        col("id", DataType::Int64),
        col("value", DataType::Int64),
        col("sensor", DataType::Utf8),
    ];
    let sensors: Vec<String> = (0..16).map(|i| format!("sensor-{i:02}")).collect();
    let mut writer = TableWriter::create(path, schema)?;
    let mut rng = StdRng::seed_from_u64(seed);
    let jitter = VALUE_RANGE / 200;
    for id in 0..rows {
        let value = match layout {
            Layout::Clustered => {
                let trend = (id as i128 * VALUE_RANGE as i128 / rows as i128) as i64;
                (trend + rng.gen_range(-jitter..=jitter)).clamp(0, VALUE_RANGE - 1)
            }
            Layout::Random => rng.gen_range(0..VALUE_RANGE),
        };
        writer.add_row(vec![
            Value::Int64(id as i64),
            Value::Int64(value),
            Value::Utf8(sensors[rng.gen_range(0..sensors.len())].clone()),
        ])?;
    }
    writer.finish()
}
