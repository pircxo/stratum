# Presenting Stratum

## CV entry

**Stratum — Columnar Storage and Query Engine** | Rust, SQL, Rayon

- Built a columnar engine with a custom binary format, adaptive integer
  encoding, dictionary encoding, and integer/string zone-map pruning.
- Implemented a SQL parser and batch query executor with boolean filters,
  grouped aggregates, parallel scanning, late materialization, and
  metadata-only queries.
- Skipped 242 of 245 row groups (98.8%) in a reproducible benchmark over
  2 million clustered rows; verified correctness with seeded differential
  tests across serial/parallel and pruned/full scans.

Repository: [github.com/pircxo/stratum](https://github.com/pircxo/stratum)

For a shorter entry:

> Built a Rust columnar query engine with adaptive encoding, SQL
> aggregation, and parallel predicate pushdown; pruned 98.8% of row groups
> in a reproducible 2-million-row benchmark.

The pruning percentage describes this synthetic clustered workload.
Timing ratios depend on hardware and data layout; use the recorded
benchmark in [ARCHITECTURE.md](ARCHITECTURE.md#recorded-benchmark) when discussing speed.

[Verification results and reproduction commands](VERIFICATION.md)

## Five-minute interview demo

Run `sh scripts/demo.sh` from the repository. It creates a temporary table,
prints the encodings, executes filtered and grouped queries, demonstrates
an aggregate answered without reading column data, and exports CSV. It
cleans up its temporary files automatically.

Then run:

```sh
cargo run --locked --release --bin bench_prune -- --rows 2000000 --iterations 7 --threads 2 --seed 42
```

Explain the clustered and random layouts side by side. The clustered
column has narrow ranges per group, so the zone maps exclude almost the
entire table. Random data spans the whole value domain in each group, so
the same optimization has little scope to help.

## Topics to be ready to explain

1. Why row groups make pruning useful, and why the footer is at the end.
2. How adaptive encoding chooses between plain, frame-of-reference, and
   delta bit packing without encoding every candidate.
3. Why pruning `A OR B` needs both sides to be impossible, while pruning
   `A AND B` needs only one; how `NOT` uses the all-match proof.
4. How typed batches and dictionary-level string comparisons avoid
   cloning a string for every filter evaluation.
5. Why projection columns are decoded only after some rows pass a filter.
6. How partial aggregates merge across row groups, and why `SUM` uses
   `i128` before checking the final `Int64` result.
7. Why the final top k must be contained in the union of each group's top
   k, and how stable sorting preserves ties.
8. What the reference tests establish, and what the benchmark actually
   measures: warm-cache medians, excluding data generation and file open.

## Scope

This is a finished portfolio implementation of a bounded analytical
engine. It has immutable single-table files, two stored types, no joins,
transactions, updates, stored nulls, or full SQL compatibility. Query
results and aggregate groups must fit memory. The file format validates
structure but has no checksums, encryption, or recovery log. These limits
are design trade-offs to discuss in an interview.
