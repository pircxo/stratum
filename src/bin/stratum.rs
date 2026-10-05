//! The `stratum` CLI: load a CSV into a `.strat` table, then query it.
//!
//! ```text
//! stratum load data.csv data.strat --schema id:int,name:str,value:int
//! stratum query data.strat "SELECT id, value FROM t WHERE value > 100 LIMIT 5" --explain
//! ```
//!
//! The table name in the SQL itself is ignored — a `.strat` file is
//! already one specific table, so `FROM anything` just has to parse, not
//! resolve to a real name. That's a deliberate corner cut for this CLI,
//! not the query engine: `query::executor` takes a `&TableReader`
//! directly and never looks at `stmt.table` either.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};

use stratum::query::{execute, parse, ExecuteOptions, QueryResult};
use stratum::storage::{DataType, TableReader, TableWriter};
use stratum::{ColumnSchema, Value};

#[derive(Parser)]
#[command(
    name = "stratum",
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
        /// Set if the CSV's first line is a header row to skip.
        #[arg(long, default_value_t = true)]
        header: bool,
    },
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
        /// Print scan stats (row groups pruned, rows scanned) after the result.
        #[arg(long)]
        explain: bool,
    },
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
        Command::Query {
            table_path,
            sql,
            no_pruning,
            no_parallel,
            explain,
        } => run_query(&table_path, &sql, no_pruning, no_parallel, explain),
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

/// Deliberately naive: splits each line on commas, no quoted-field
/// support. Fine for the synthetic and benchmark data this project
/// loads; not a general-purpose CSV parser, and doesn't pretend to be.
fn run_load(csv_path: &Path, out_path: &Path, schema_spec: &str, header: bool) -> Result<()> {
    let schema = parse_schema(schema_spec)?;
    let file = File::open(csv_path).with_context(|| format!("opening {}", csv_path.display()))?;
    let mut lines = BufReader::new(file).lines();
    if header {
        lines.next();
    }

    let mut writer = TableWriter::create(out_path, schema.clone())
        .with_context(|| format!("creating {}", out_path.display()))?;

    let mut row_count = 0u64;
    for (line_no, line) in lines.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != schema.len() {
            bail!(
                "line {}: expected {} fields, found {}: {line:?}",
                line_no + 1,
                schema.len(),
                fields.len()
            );
        }

        let mut row = Vec::with_capacity(schema.len());
        for (field, col) in fields.iter().zip(schema.iter()) {
            let value = match col.dtype {
                DataType::Int64 => Value::Int64(field.trim().parse().with_context(|| {
                    format!(
                        "line {}: can't parse '{}' as int for column '{}'",
                        line_no + 1,
                        field,
                        col.name
                    )
                })?),
                DataType::Utf8 => Value::Utf8(field.trim().to_string()),
            };
            row.push(value);
        }
        writer.add_row(row)?;
        row_count += 1;
    }

    writer.finish()?;
    println!("Loaded {row_count} rows into {}", out_path.display());
    Ok(())
}

fn run_query(
    table_path: &Path,
    sql: &str,
    no_pruning: bool,
    no_parallel: bool,
    explain: bool,
) -> Result<()> {
    let reader = TableReader::open(table_path)?;
    let stmt = parse(sql).map_err(|e| anyhow!("{e}"))?;
    let opts = ExecuteOptions {
        use_pruning: !no_pruning,
        parallel: !no_parallel,
    };

    let (result, stats) = execute(&reader, &stmt, &opts)?;
    print_table(&result);

    if explain {
        eprintln!();
        eprintln!("-- scan stats --");
        eprintln!(
            "row groups scanned: {}/{} ({} pruned by zone map)",
            stats.row_groups_scanned,
            stats.row_groups_total,
            stats.row_groups_total - stats.row_groups_scanned
        );
        eprintln!("rows scanned:  {}", stats.rows_scanned);
        eprintln!("rows returned: {}", stats.rows_returned);
    }
    Ok(())
}

fn print_table(result: &QueryResult) {
    let header = result.columns.join(" | ");
    println!("{header}");
    println!("{}", "-".repeat(header.len()));
    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        println!("{}", cells.join(" | "));
    }
    println!(
        "({} row{})",
        result.rows.len(),
        if result.rows.len() == 1 { "" } else { "s" }
    );
}
