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
- **New logic needs a test.** Storage format changes need a round-trip test
  and a corrupt-input test; query engine changes need an end-to-end query
  test with a known-correct expected result, and new SQL features should be
  added to the random query generator and oracle in `tests/differential.rs`;
  parser changes need a parser test.
- **`cargo fmt` and `cargo clippy` clean before a PR.** CI enforces both.

## Project layout

```
src/storage/     on-disk columnar format: writer, reader + footer validation
src/encoding.rs  bit-packing, frame-of-reference, delta and dictionary codecs
src/query/       lexer, parser, AST, and the executor (pruning, vectorized
                 filters, top-k, parallel aggregation)
src/bin/         the `stratum` CLI (load / inspect / query) and `bench_prune`
tests/           end-to-end, differential (vs. a naive oracle), storage
                 validation, and CLI tests
```

## Local development

```bash
cargo build --locked --all-targets
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
cargo doc --locked --no-deps
```

## Reporting issues

Open a GitHub issue. Correctness bugs (wrong query results, a pruning
decision that drops rows it shouldn't) are the highest priority — please
include the exact schema, data, and query that reproduces it.

Use `sh scripts/demo.sh` for the example workflow. For engine changes,
run the seeded differential tests as part of `cargo test`; they compare
pruning and parallel execution against a simple reference evaluator.
Document format changes explicitly and update the footer magic version.
