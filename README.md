# Stratum

A small columnar storage engine and SQL-subset query processor, written in
Rust, to learn — and demonstrate — the database-internals techniques real
OLAP systems (Snowflake, DataFusion, DuckDB, Polars) use in production:
block-based columnar layout, dictionary encoding, zone-map predicate
pushdown, and parallel vectorized scanning.

This isn't a wrapper around an existing engine. The on-disk file format,
the SQL lexer and parser, and the query executor are all hand-written —
on purpose, since the point was to actually build the pieces usually
hidden behind a database driver, not just use them.

## What it does

```bash
# Load a CSV into Stratum's own columnar format
cargo run --bin stratum -- load readings.csv readings.strat --schema id:int,sensor:str,value:int

# Query it
cargo run --bin stratum -- query readings.strat \
  "SELECT id, value FROM t WHERE value > 1000 ORDER BY value DESC LIMIT 10" --explain
```

`--explain` prints how much work the query actually did:

```
-- scan stats --
row groups scanned: 3/245 (242 pruned by zone map)
rows scanned:  24576
rows returned: 10
```

## Why it's organised this way

```
CSV ──load──▶  .strat file                    SELECT ... FROM t WHERE ...
               (row groups of 8192 rows,            │
                columnar chunks, zone-map            ▼
                min/max per chunk, footer    lexer → parser → AST
                at end of file — see                 │
                storage::format)                      ▼
                                          executor: prune row groups via
                                          zone map, decode survivors in
                                          parallel (rayon), filter +
                                          project + order + limit
```

- **Columnar, not row-oriented.** A query that only touches 2 of a
  table's 10 columns only ever decodes those 2 — see `storage::format`
  and `encoding`.
- **Zone-map pruning.** Every chunk records its column's min/max. A
  predicate like `value > 1000` can rule out an entire row group —
  skipping decode, not just filtering after decode — if that row group's
  max is already `<= 1000`. This is a simplified version of exactly what
  Snowflake calls micro-partition pruning.
- **A real (small) SQL parser**, hand-written lexer + recursive-descent
  parser, not a third-party crate. Scope is intentionally narrow —
  `SELECT`/`FROM`/`WHERE` (AND-only)/`ORDER BY`/`LIMIT` — and that limit
  is documented, not hidden.
- **Parallel scanning.** Row groups are independent, so surviving ones
  are scanned concurrently with `rayon`, using positioned reads
  (`pread`) so threads never contend on a shared file cursor.

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the full reasoning, measured
benchmark numbers, and an honest list of what v1 leaves out.

## Running it

```bash
cargo build
cargo test                                   # 23 tests: unit + end-to-end
cargo clippy --all-targets -- -D warnings
cargo fmt --check

cargo run --release --bin bench_prune         # real pruning benchmark, see ARCHITECTURE.md
```

## Status

This is a working v1, not a finished database: one numeric type
(`Int64`) and one string type (`Utf8`), rounded-rectangle-simple SQL (no
`OR`, no joins, no aggregates), and zone-map pruning only on integer
columns. `ARCHITECTURE.md` is explicit about what a v2 would add first.
I'd rather ship a smaller thing that's fully true than a bigger one with
quiet gaps.

## License

MIT — see `LICENSE`.
