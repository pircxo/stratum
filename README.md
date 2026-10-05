# Stratum

A columnar storage and analytical query engine built in Rust. Stratum owns
its binary file format, adaptive encodings, SQL lexer/parser, and execution
engine. It is a complete portfolio project with a deliberately bounded
scope: immutable single-table files, a useful SQL subset, a command-line
interface, regression tests, and a reproducible benchmark.

## Quick start

Install Rust with [rustup](https://rustup.rs/). The repository pins Rust
1.97.0, including Clippy and rustfmt. Linux, macOS, and Windows are supported
by the positioned-read implementation and covered by the CI configuration.

```sh
cargo build --locked --release --bin stratum
cargo run --locked --release --bin stratum -- load examples/readings.csv readings.strat --schema id:int,sensor:str,value:int
cargo run --locked --release --bin stratum -- inspect readings.strat
cargo run --locked --release --bin stratum -- query readings.strat "SELECT id, sensor, value FROM readings WHERE value > 1000 ORDER BY value DESC LIMIT 3" --explain
```

The query returns `(12, gamma, 1700)`, `(9, gamma, 1500)`, and
`(6, gamma, 1300)`. `--explain` writes row-group counts, scanned rows,
encoded bytes read, and execution time to stderr.

On Linux or macOS, run `sh scripts/demo.sh` for a self-contained demo that
also shows grouped aggregates, metadata-only queries, and CSV export. It
uses a temporary directory and cleans up automatically. On Windows, the
commands above work in PowerShell.

If Rust is installed but your Terminal reports `cargo: command not found`,
run `source "$HOME/.cargo/env"` to enable it in the current session. The
demo script loads this setup automatically when Cargo is missing from
`PATH`. For future terminals, add `source "$HOME/.cargo/env"` to your
shell startup file (usually `~/.zshrc` on macOS).

## Storage and execution

- **Row groups of up to 8,192 rows**, stored column by column with a footer
  containing schema, offsets, encodings, and statistics.
- **Adaptive Int64 encoding:** chooses the smallest of plain little-endian,
  frame-of-reference bit packing, and delta bit packing for each chunk.
- **Dictionary-encoded strings** with bit-packed indices. Filters compare
  each distinct string once and map the result through the indices.
- **Integer and string zone maps:** conservative min/max reasoning skips
  impossible row groups and avoids evaluating filters on groups that are
  proven to match completely.
- **Parallel batch scanning** through Rayon and positioned reads, plus
  late materialization of projection columns after filtering.
- **Partial grouped aggregates** merged across row groups, local top-k
  reduction for `ORDER BY ... LIMIT`, and metadata-only `COUNT`/`MIN`/`MAX`
  queries when no filter or grouping is needed.
- **Validated inputs:** type-checked rows, checked chunk decoding, bounded
  metadata, validated file layout, and CSV imports published only after
  the complete import succeeds.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the algorithms and trade-offs.

## SQL subset

```sql
SELECT (* | column [AS alias] | aggregate(column | *) [AS alias], ...)
FROM table
[WHERE expression]
[GROUP BY column, ...]
[ORDER BY column_or_alias_or_position [ASC | DESC], ...]
[LIMIT nonnegative_integer];
```

Expressions support `=`, `!=`, `<>`, `<`, `<=`, `>`, `>=`, `AND`, `OR`,
`NOT`, parentheses, `IN`, and `BETWEEN`. Precedence is `NOT`, then `AND`,
then `OR`. Literals are signed 64-bit integers and single-quoted strings;
use doubled quotes for an apostrophe: `'O''Brien'`. Column names and aliases
are case-sensitive; keywords are case-insensitive. Identifiers cannot be
reserved keywords and cannot contain spaces.

Aggregates are `COUNT(*)`, `COUNT(column)`, `SUM`, `AVG`, `MIN`, and `MAX`.
`SUM` and `AVG` require an integer column. `SUM` checks overflow; `AVG`
returns a floating-point value. Empty ungrouped aggregates return one row
with zero counts and `NULL` for other aggregates. Empty grouped queries
return no groups. `ORDER BY` on an aggregate query must refer to an output
name/alias or a 1-based output position.

```sh
cargo run --locked --release --bin stratum -- query readings.strat "SELECT sensor, COUNT(*) AS samples, AVG(value) AS average FROM t GROUP BY sensor ORDER BY average DESC"
cargo run --locked --release --bin stratum -- query readings.strat "SELECT id FROM t WHERE NOT (value BETWEEN 300 AND 1200) ORDER BY id" --format csv
cargo run --locked --release --bin stratum -- query readings.strat "SELECT COUNT(*), MIN(value), MAX(value) FROM t" --explain
```

The file supplied to `query` is the table. The `FROM` name is syntax only;
there is no catalog or lookup by table name. Ties retain physical input
order for projections. Groups have deterministic key order before any
explicit ordering. Result nulls sort first ascending and last descending.

## CSV and CLI behavior

`load` supports UTF-8 CSV with quoted commas, escaped quotes, multiline
fields, and CRLF. Header names must match the explicit schema in order;
use `--header=false` for headerless input. Integer fields are trimmed;
strings preserve their contents, including surrounding spaces.

Types accept `int`/`int64` and `str`/`string`/`utf8`. Imports refuse to
replace existing files, including the source CSV itself. Choose a new
output path when repeating the quick start. Failed imports leave no table
or temporary file behind. `query --format csv` writes a header and quoted
records to stdout; `--explain` remains on stderr so exports stay valid CSV.

`--no-pruning` and `--no-parallel` provide comparison modes. `inspect`
(`info` is an alias) shows schema, row groups, storage size, and encodings.
Use `cargo run --bin stratum -- --help` for command help.

## Reproducible benchmark

```sh
cargo run --locked --release --bin bench_prune -- --rows 2000000 --iterations 7 --threads 2 --seed 42
```

The benchmark generates clustered and random layouts with a fixed seed,
warms each mode, checks every result against a full serial scan, and
reports median execution times. One local macOS ARM64 run with two Rayon
workers produced:

| Layout / mode | Median | Groups scanned | Matching rows |
| --- | ---: | ---: | ---: |
| Clustered, pruning + parallel | 0.18 ms | 3 / 245 | 3,552 |
| Clustered, full scan + parallel | 2.85 ms | 245 / 245 | 3,552 |
| Clustered, full scan + serial | 5.19 ms | 245 / 245 | 3,552 |
| Random, pruning + parallel | 4.43 ms | 245 / 245 | 1,971 |
| Random, full scan + parallel | 4.46 ms | 245 / 245 | 1,971 |

Pruning skipped **98.8% of row groups** in the clustered workload. It
skipped none in the random workload. The roughly 29× improvement over a
serial full scan combines pruning and parallelism; it is specific to this
workload and machine. These are warm-cache timings, excluding table
creation and file open. They are not comparisons against another database.

## Verification

See [the recorded verification results](VERIFICATION.md) for the passing
test counts, benchmark evidence, and commands to reproduce the checks.

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
cargo doc --locked --no-deps
```

Tests cover encodings, SQL parsing, malformed files, CLI imports/exports,
query results, empty tables, overflow, and optimization equivalence.
Seeded differential tests compare 720 generated queries against a naive
row-oriented oracle in all four pruning/parallel modes. CI runs build,
tests, formatting, and Clippy across Linux, macOS, and Windows, plus a
release benchmark smoke check.

## Boundaries

Stored types are `Int64` and `Utf8`; floating-point values and nulls are
aggregate outputs only. There are no joins, subqueries, `HAVING`, arithmetic
expressions, stored nulls, updates, transactions, schema evolution, or
general-purpose block compression. Query results, sorting, and aggregate
groups fit in memory. Zone maps help clustered data; they are not indexes
that guarantee selective access to random data.

Format 2 uses the `STRTFTR2` trailer. Format 1 files from v0.1 are rejected
with a reload instruction. The format checks structure and encoded data,
but has no checksums, recovery log, or stable external compatibility
promise. Files must stay unchanged while open.

For CV wording and an interview walkthrough, see
[PORTFOLIO.md](PORTFOLIO.md). MIT licensed; see [LICENSE](LICENSE).
