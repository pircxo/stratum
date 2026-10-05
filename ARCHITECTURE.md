# Architecture notes

Why Stratum is built the way it is, including the trade-offs and the
measured numbers — not just a list of features.

## Motivation

Most of the application-level work I've done (full-stack web apps, ETL
pipelines, dashboards) sits *on top of* a database without touching how
one actually stores or scans data. Stratum exists to close that gap
specifically: it's a small, hand-built version of the techniques that
make columnar analytical databases (Snowflake, DuckDB, DataFusion,
Polars) fast, built from scratch rather than read about.

## The on-disk format

A `.strat` file is a sequence of **row groups** — batches of up to 8192
rows — and within each row group, data is stored **column by column**
("chunks"), not row by row. After all row groups comes a **footer**:
for every chunk, its byte offset, length, and (for integer columns) a
**zone map** — the chunk's min and max value.

```text
[row group 0: col0 chunk][col1 chunk]...[colN chunk]
[row group 1: col0 chunk][col1 chunk]...[colN chunk]
...
[footer: bincode-serialized metadata]
[footer_len: u64][magic: 8 bytes]
```

Two decisions worth calling out:

- **The footer is at the end, not a fixed header at the start.** A
  streaming writer never has to know the final row count or chunk
  layout in advance, and never seeks backward to patch a header — it
  just keeps writing row groups and remembers their metadata in memory
  until `finish()`. This is the same trade-off Parquet makes, for the
  same reason.
- **Row groups, not one chunk per column for the whole file.** If
  Stratum stored each column as *one* chunk for the entire file, the
  zone map would cover the whole table — useless for pruning anything.
  Splitting into many row groups, each with its *own* zone map, is what
  makes pruning possible at all: a predicate can rule out *some* row
  groups without ruling out the whole column.

## Encodings — and their honest limits

- **Int64: plain, fixed-width.** Every value is 8 bytes, little-endian.
  No bit-packing, no delta encoding. This is a deliberately simple
  baseline — a v2 would add delta encoding for monotonically increasing
  columns (ids, timestamps) and bit-packing for low-cardinality integer
  columns, both of which are large wins in real systems and neither of
  which I implemented here in favour of spending the time on the query
  engine and pruning instead.
- **Utf8: dictionary-encoded.** Each chunk stores its distinct strings
  once (sorted, for deterministic output) plus one small integer index
  per row. This is the same idea Parquet, ORC, and Snowflake's own
  storage all use for string columns, because real string columns are
  usually far lower-cardinality than their row count.
- **No general compression** (no LZ4/Zstd block compression on top of
  the above). Real columnar formats layer general-purpose compression
  over type-specific encoding; Stratum stops at the encoding layer.

## Zone-map pruning — and when it actually helps

`query::executor::is_prunable` checks, for every predicate on an
integer column, whether a row group's `[min, max]` makes it
*impossible* for any row in that group to satisfy the predicate — e.g.
`value > 1000` can never be true in a group whose `max` is `900`. If
so, the group is skipped entirely: never read, never decoded.

This technique has a real limit, and the benchmark below is built to
show it honestly rather than hide it. **Pruning only helps when a
column's values are correlated with physical row order.** If a column's
values are scattered uniformly at random with no relationship to
insertion order, almost every row group's `[min, max]` already spans
nearly the whole domain, and there's nothing to prune — no amount of
clever indexing fixes that; the data itself has no structure to
exploit. Snowflake's own documentation on micro-partition pruning makes
the same point under the name "clustering." Columns that *do* have this
property in practice — timestamps, auto-increment ids, anything written
roughly in arrival order — are exactly where this pays off.

### Measured result

`cargo run --release --bin bench_prune` generates 2,000,000 rows where
`value` trends upward with row order (plus local noise — simulating a
realistic clustered column like a timestamp), then runs
`SELECT id FROM t WHERE value > 999000` three ways. One real run on the
machine this was built on (a 2-core cloud sandbox):

```
pruning + parallel         1.82 ms   row groups scanned:    3/245   rows returned: 3669
full scan, parallel       27.01 ms   row groups scanned:  245/245   rows returned: 3669
full scan, serial         49.00 ms   row groups scanned:  245/245   rows returned: 3669

Zone-map pruning skipped 242/245 row groups (98.8% of the table never decoded).
Pruned+parallel was 27.0x faster than a full serial scan on this run.
```

All three runs return the exact same 3,669 rows — `tests/end_to_end.rs`
has a dedicated test (`pruning_never_changes_the_result_only_how_much_gets_scanned`)
asserting pruned and unpruned results are identical, because the whole
point of this optimization is that it changes *how much work happens*,
never *what the answer is*. The 27x figure is specific to this run, this
data shape, and this (2-core) machine — more cores would widen the gap
between the parallel and serial full scans further; it's reported as
one real measurement, not a general claim.

## Parallel scanning

Row groups are independent — nothing about scanning one depends on
another — so surviving (non-pruned) row groups are scanned concurrently
with `rayon`'s parallel iterators. The part that makes this safe without
a lock: `TableReader` reads via
[`std::os::unix::fs::FileExt::read_at`](https://doc.rust-lang.org/std/os/unix/fs/trait.FileExt.html)
(a positioned read, `pread` under the hood) instead of `seek` + `read`.
`read_at` takes `&self`, not `&mut self`, and doesn't move a shared file
cursor, so multiple threads can read different byte ranges of the same
open file at once with no synchronization at all. This is Unix-specific
— a cross-platform v2 would need an abstraction over Windows'
equivalent (`seek_read`) — and that's noted rather than quietly assumed.

## The SQL subset, and where the line is drawn

The parser (`query::lexer` + `query::parser`) is hand-written —
tokenizer, then recursive-descent parser — specifically to practice that,
not to avoid a parser-generator crate out of principle. It supports:

```
SELECT (* | col, col, ...) FROM table
  [WHERE cond (AND cond)*]
  [ORDER BY col [ASC|DESC]]
  [LIMIT n]
```

No `OR`, no parentheses, no joins, no aggregates (`COUNT`/`SUM`/...), no
subqueries. That line is deliberate: each of those is a meaningfully
larger feature (`OR` alone means predicates can no longer be evaluated
as a simple AND-list for pruning *or* execution — it needs a proper
expression tree), and the project's actual goal was storage + pruning +
execution, not SQL coverage for its own sake. A `rejects_or_since_it_is_out_of_scope`
test documents this as an intentional boundary, not a bug someone will
"find."

## Testing strategy

- **Unit tests** for the pieces that are easy to get subtly wrong in
  isolation: the lexer (keyword case-insensitivity, negative numbers,
  `!=` vs `<>`), the parser (full statements, rejecting `OR`, rejecting
  trailing garbage), and the encodings (round-trips, and a dictionary
  size check against raw bytes).
- **End-to-end tests** (`tests/end_to_end.rs`) write a real `.strat`
  file and query it through the actual parser + executor — the same
  path the CLI uses — asserting on exact expected rows, not just "it
  didn't crash." These also assert the properties that matter most for
  a storage/query engine specifically: pruned and unpruned scans return
  identical results, and sequential and parallel scans return identical
  results.
- **23 tests total**, run in CI (`.github/workflows/ci.yml`) alongside
  `cargo clippy -- -D warnings` and `cargo fmt --check`.

### A real toolchain-drift bug, and the fix

CI failed once on this project in a way that's worth documenting rather
than quietly fixing: `decode_int64_chunk` used `bytes.chunks_exact(8)`,
which was clean under the local dev machine's clippy but failed CI's
`cargo clippy -- -D warnings` with `chunks_exact_to_as_chunks` — a lint
that only exists in a newer clippy than the one installed locally.
`dtolnay/rust-toolchain@stable` always resolves to *whatever's currently
stable*, so "works on my machine" and "passes CI" can silently drift
apart as new Rust releases ship new lints. The fix was two-part: rewrite
the decode loop to index by offset instead of `chunks_exact` (clearer
anyway, and not tied to any one clippy version's opinion), and pin the
toolchain explicitly — `rust-toolchain.toml` plus
`dtolnay/rust-toolchain@1.97.0` in CI — so "stable" can't quietly become
a moving target again.

## What a v2 would add first

In priority order, if this needed to handle more than a portfolio demo:

1. **Delta/bit-packed integer encoding** — the single biggest storage
   and scan-speed win left on the table, and it's a natural fit for
   exactly the clustered-column case pruning already targets.
2. **`OR` and parenthesized expressions** in the query language, via a
   proper expression AST (currently predicates are a flat AND-list).
3. **A cross-platform positioned-read abstraction**, so parallel
   scanning isn't Unix-only.
4. **Statistics-aware block skipping for strings** (currently zone maps
   only cover `Int64`) — lexicographic min/max per chunk would make
   range predicates on string columns pruneable too.
5. **Alembic/migration-style schema evolution** — right now a table's
   schema is fixed at creation; adding a column means rewriting the
   file.
