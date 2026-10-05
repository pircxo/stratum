# Contributing to Stratum

Stratum is a learning-by-building project: a small columnar storage engine
and SQL-subset query processor written to understand the database-internals
techniques real OLAP systems (Snowflake, DataFusion, DuckDB, Polars) use in
production, at a scale one person can actually read end to end.

## Ground rules

- **Correctness over cleverness.** A faster but subtly-wrong implementation
  of block pruning or dictionary decoding is worse than a simple, obviously
  correct one — this project would rather be small and right than large and
  shaky.
- **New logic needs a test.** Storage format changes need a round-trip test;
  query engine changes need an end-to-end query test with a known-correct
  expected result; parser changes need a parser test.
- **`cargo fmt` and `cargo clippy` clean before a PR.** CI enforces both.

## Project layout

```
src/storage/   on-disk columnar format: writer, reader, block/footer layout
src/encoding/  plain (Int64) and dictionary (Utf8) column encodings
src/query/     lexer, parser, AST, and the scan/filter/project executor
src/bin/       the `stratum` CLI (load + query) and the `bench_prune` benchmark
tests/         end-to-end integration tests
```

## Local development

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Reporting issues

Open a GitHub issue. Correctness bugs (wrong query results, a pruning
decision that drops rows it shouldn't) are the highest priority — please
include the exact schema, data, and query that reproduces it.
