//! The `stratum` CLI: load a CSV into a `.strat` table, inspect it, query it.
//!
//! ```text
//! stratum load data.csv data.strat --schema id:int,name:str,value:int
//! stratum inspect data.strat
//! stratum query data.strat "SELECT name, COUNT(*) AS n FROM t GROUP BY name ORDER BY n DESC" --explain
//! ```
//!
//! The table name in the SQL itself is ignored — a `.strat` file is
//! already one specific table, so `FROM anything` just has to parse, not
//! resolve to a real name. `query::executor` takes a `&TableReader`
//! directly and never looks at `stmt.table` either.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, ensure, Context, Result};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};

use stratum::query::{execute, parse, ExecuteOptions, QueryResult};
use stratum::storage::{DataType, Encoding, TableReader, TableWriter};
use stratum::{ColumnSchema, Value};

#[derive(Parser)]
#[command(
    name = "stratum",
    version,
    about = "A small columnar storage engine and SQL-subset query processor"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load a CSV file into a new .strat table.
    Load {
        csv_path: PathBuf,
        out_path: PathBuf,
        /// Comma-separated name:type pairs, e.g. "id:int,name:str,value:int"
        #[arg(long)]
        schema: String,
        /// Whether the CSV's first record is a header row to skip.
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        header: bool,
    },
    /// Show a table's schema, size, and per-column encodings.
    #[command(alias = "info")]
    Inspect { table_path: PathBuf },
    /// Run one SQL query against a .strat table.
    Query {
        table_path: PathBuf,
        sql: String,
        /// Disable zone-map block pruning (useful for comparing against bench_prune).
        #[arg(long)]
        no_pruning: bool,
        /// Scan row groups sequentially instead of with rayon.
        #[arg(long)]
        no_parallel: bool,
        /// Print scan stats (row groups pruned, bytes read, time) to stderr.
        #[arg(long)]
        explain: bool,
        /// Output format.
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    /// Aligned, human-readable columns.
    Table,
    /// RFC 4180 CSV with a header row.
    Csv,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Load {
            csv_path,
            out_path,
            schema,
            header,
        } => run_load(&csv_path, &out_path, &schema, header),
        Command::Inspect { table_path } => run_inspect(&table_path),
        Command::Query {
            table_path,
            sql,
            no_pruning,
            no_parallel,
            explain,
            format,
        } => run_query(&table_path, &sql, no_pruning, no_parallel, explain, format),
    }
}

fn parse_schema(spec: &str) -> Result<Vec<ColumnSchema>> {
    spec.split(',')
        .map(|part| {
            let (name, ty) = part
                .split_once(':')
                .ok_or_else(|| anyhow!("invalid schema entry '{part}', expected name:type"))?;
            let dtype = match ty.trim().to_ascii_lowercase().as_str() {
                "int" | "int64" => DataType::Int64,
                "str" | "utf8" | "string" => DataType::Utf8,
                other => bail!("unknown type '{other}' in schema (use 'int' or 'str')"),
            };
            Ok(ColumnSchema {
                name: name.trim().to_string(),
                dtype,
            })
        })
        .collect()
}

/// Loads all-or-nothing: rows are written to a temp file next to the
/// destination, which is renamed into place only after the footer is
/// written. A bad row anywhere leaves no output (and no temp file)
/// behind, and an existing file — including the input itself — is never
/// overwritten.
fn run_load(csv_path: &Path, out_path: &Path, schema_spec: &str, header: bool) -> Result<()> {
    let start = Instant::now();
    let schema = parse_schema(schema_spec)?;
    ensure!(
        !out_path.exists(),
        "{} already exists; refusing to overwrite it",
        out_path.display()
    );
    let mut csv = csv::ReaderBuilder::new()
        .has_headers(header)
        .from_path(csv_path)
        .with_context(|| format!("opening {}", csv_path.display()))?;
    if header {
        let headers = csv.headers().context("reading CSV header")?;
        ensure!(
            headers.len() == schema.len()
                && headers
                    .iter()
                    .zip(&schema)
                    .all(|(name, col)| name == col.name),
            "CSV header must match schema names and order"
        );
    }

    let dir = match out_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temp file in {}", dir.display()))?;
    let mut writer = TableWriter::new(tmp.as_file().try_clone()?, schema.clone())?;

    let mut row_count = 0u64;
    for record in csv.records() {
        let record = record.context("reading CSV")?;
        let line = record.position().map_or(0, |p| p.line());
        if record.len() != schema.len() {
            bail!(
                "line {line}: expected {} fields, found {}",
                schema.len(),
                record.len()
            );
        }
        let mut row = Vec::with_capacity(schema.len());
        for (field, col) in record.iter().zip(&schema) {
            row.push(match col.dtype {
                DataType::Int64 => Value::Int64(field.trim().parse().with_context(|| {
                    format!(
                        "line {line}: can't parse {field:?} as int for column '{}'",
                        col.name
                    )
                })?),
                // Strings are kept byte-for-byte, surrounding spaces included.
                DataType::Utf8 => Value::Utf8(field.to_string()),
            });
        }
        writer.add_row(row)?;
        row_count += 1;
    }
    writer.finish()?;
    tmp.persist_noclobber(out_path)
        .map_err(|e| e.error)
        .with_context(|| format!("moving the table into place at {}", out_path.display()))?;

    let size = std::fs::metadata(out_path)?.len();
    println!(
        "Loaded {row_count} rows into {} ({}) in {:.2?}",
        out_path.display(),
        human_bytes(size),
        start.elapsed()
    );
    Ok(())
}

fn encoding_name(e: Encoding) -> &'static str {
    match e {
        Encoding::Plain => "plain",
        Encoding::BitPacked => "bit-packed",
        Encoding::DeltaBitPacked => "delta",
        Encoding::Dictionary => "dictionary",
    }
}

fn run_inspect(table_path: &Path) -> Result<()> {
    let reader = TableReader::open(table_path)?;
    let rows = reader.row_count();
    println!("File: {}", table_path.display());
    println!("Size: {}", human_bytes(reader.file_len()));
    println!("Rows: {rows}");
    println!("Row groups: {}", reader.row_groups().len());
    println!("Columns:");
    for (i, col) in reader.schema().iter().enumerate() {
        let bytes: u64 = reader
            .row_groups()
            .iter()
            .map(|rg| rg.chunks[i].length)
            .sum();
        let encodings: BTreeSet<&str> = reader
            .row_groups()
            .iter()
            .map(|rg| encoding_name(rg.chunks[i].encoding))
            .collect();
        let encodings = if encodings.is_empty() {
            "no data".to_string()
        } else {
            encodings.into_iter().collect::<Vec<_>>().join(", ")
        };
        let per_row = if rows == 0 {
            String::new()
        } else {
            format!(", {:.2} bytes/row", bytes as f64 / rows as f64)
        };
        println!(
            "  {}: {:?} ({encodings}) — {}{per_row}",
            col.name,
            col.dtype,
            human_bytes(bytes)
        );
    }
    Ok(())
}

fn run_query(
    table_path: &Path,
    sql: &str,
    no_pruning: bool,
    no_parallel: bool,
    explain: bool,
    format: Format,
) -> Result<()> {
    let reader = TableReader::open(table_path)?;
    let stmt = parse(sql).map_err(|e| anyhow!("{e}"))?;
    let opts = ExecuteOptions {
        use_pruning: !no_pruning,
        parallel: !no_parallel,
    };

    let start = Instant::now();
    let (result, stats) = execute(&reader, &stmt, &opts)?;
    let elapsed = start.elapsed();
    match format {
        Format::Table => print_result(&result),
        Format::Csv => write_csv(&result)?,
    }

    if explain {
        eprintln!();
        eprintln!("-- scan stats --");
        if stats.metadata_only {
            eprintln!(
                "answered from footer metadata alone: 0/{} row groups read",
                stats.row_groups_total
            );
        } else {
            let mut line = format!(
                "row groups scanned: {}/{} ({} pruned by zone map",
                stats.row_groups_scanned,
                stats.row_groups_total,
                stats.row_groups_total - stats.row_groups_scanned
            );
            if stats.row_groups_fully_matched > 0 {
                line += &format!(
                    "; {} fully matched, so the filter was never evaluated",
                    stats.row_groups_fully_matched
                );
            }
            eprintln!("{line})");
        }
        eprintln!("rows scanned:  {}", stats.rows_scanned);
        eprintln!("rows returned: {}", stats.rows_returned);
        eprintln!(
            "bytes read:    {} of {}",
            human_bytes(stats.bytes_read),
            human_bytes(reader.file_len())
        );
        eprintln!("time:          {elapsed:.2?}");
    }
    Ok(())
}

fn print_result(result: &QueryResult) {
    let mut table = vec![result.columns.clone()];
    table.extend(
        result
            .rows
            .iter()
            .map(|row| row.iter().map(Value::to_string).collect()),
    );
    print_aligned(&table);
    println!(
        "({} row{})",
        result.rows.len(),
        if result.rows.len() == 1 { "" } else { "s" }
    );
}

fn write_csv(result: &QueryResult) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = csv::Writer::from_writer(stdout.lock());
    out.write_record(&result.columns)?;
    for row in &result.rows {
        out.write_record(row.iter().map(Value::to_string))?;
    }
    out.flush()?;
    Ok(())
}

/// Prints `table[0]` as a header, a rule, then the rest, columns padded
/// to their widest cell.
fn print_aligned(table: &[Vec<String>]) {
    let cols = table[0].len();
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            table
                .iter()
                .map(|r| r[c].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |row: &[String]| {
        row.iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect::<Vec<_>>()
            .join(" | ")
            .trim_end()
            .to_string()
    };
    println!("{}", line(&table[0]));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-")
    );
    for row in &table[1..] {
        println!("{}", line(row));
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}
