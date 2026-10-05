# Architecture

Stratum demonstrates the storage and execution techniques of a small
analytical engine. Its storage, encodings, parser, and executor are
implemented in this repository; Rayon supplies parallel scheduling,
`bincode` serializes metadata, and the `csv` crate handles CSV syntax.

## Data flow

```text
CSV records -> type-checked rows -> buffered row group
                                    -> column encodings + zone maps
                                    -> chunks + footer -> .strat file

SQL -> lexer -> parser -> AST -> bind names and types
    -> prune row groups -> evaluate typed column batches
    -> decode projection/aggregate columns for surviving groups
    -> project or build partial aggregates -> merge -> order -> limit
```

`src/storage/` owns the file format and IO, `src/encoding.rs` owns chunk
encodings, `src/query/` owns parsing and execution, and `src/bin/` exposes
the engine through the CLI and benchmark.

## Format 2

```text
[row group 0: column 0 chunk][column 1 chunk]...[column N chunk]
[row group 1: column 0 chunk][column 1 chunk]...[column N chunk]
...
[bincode FileFooter]
[footer length: u64 little-endian][magic: 8 bytes, STRTFTR2]
```

Each group holds at most 8,192 rows. Tests can choose smaller groups.
Chunks have a byte offset, encoded length, encoding tag, and typed zone
map. The footer holds the schema, total rows, and group metadata. It lives
at the end so the writer can stream groups without seeking back to patch
a header. Metadata grows with the number of groups and columns, rather
than the number of individual rows.

The writer validates schema and row types before mutating its buffers.
`finish` flushes the last group, writes the footer and trailer, flushes
the buffered writer, and syncs the file. A schema is immutable after
creation. `TableWriter::create` refuses to overwrite a file; `new` accepts
an empty caller-owned file. The CLI stages imports beside the destination
and uses `persist_noclobber` after successful completion, preserving any
existing destination and cleaning up failed imports. File data is synced;
there is no parent-directory sync or crash-recovery protocol.

The reader checks magic, a bounded footer length (256 MiB), schema, row
counts, contiguous chunk coverage, offsets, encoding/type agreement, and
ordered zone maps. Decoders check lengths, bit widths, dictionary indices,
and UTF-8. These checks catch structural corruption; without checksums,
a bit flip that still represents valid values can go undetected. Files
must not be modified concurrently with queries.

Format 1 used plain integers and 32-bit dictionary indices. Its trailer
is recognized and rejected with an instruction to reload the source CSV;
format 2 is a deliberate compatibility break. `bincode` enum layouts are
part of this internal format, so changing their order requires another
format version.

## Adaptive integer encoding

Each chunk picks the smallest candidate, breaking ties in favor of simpler
decoding: plain, frame of reference, then delta.

| Encoding | Representation | Useful data |
| --- | --- | --- |
| Plain | 8-byte little-endian values | Full-width scattered integers |
| Frame of reference | Chunk minimum, bit width, packed offsets from minimum | Narrow ranges in any order |
| Delta bit packed | First value, then frame-of-reference packed consecutive differences | IDs, timestamps, near-constant steps |

The writer derives candidate sizes from counts and min/max statistics
without encoding all candidates. It encodes only the selected one. Packing
uses a `u128` accumulator to hold up to 64 new bits and 7 leftover bits;
round-trip tests cover all widths from 0 through 64. Wrapping differences
and reconstruction preserve signed extremes across `i64::MIN` and
`i64::MAX`.

Zero-width packing stores no value bits. For 8,192 sequential IDs, all
deltas are one, so an entire chunk is only 17 bytes: the first value plus
the delta minimum/width header. This is an ideal synthetic pattern, not a
compression ratio to expect for arbitrary integers.

## String dictionaries

Each string chunk stores sorted distinct strings, an index width, and one
bit-packed index per row. A one-value dictionary needs zero index bits;
a 16-value dictionary needs four. Strings stay in dictionary form after
decoding. A predicate compares the literal with each dictionary entry,
then maps that boolean result through the row indices. It avoids per-row
string allocation and repeated comparison of the same value. Projection
clones strings only for output rows. Dictionary overhead can outweigh its
benefit on short, high-cardinality strings; no alternative string encoding
is implemented.

## Zone-map proofs

A chunk's min/max gives two conservative facts about a filter: it may be
impossible for any row to match, or every row may be guaranteed to match.
For `x > v`, a group is impossible when `max <= v` and fully matched when
`min > v`. Other comparisons use analogous bounds. String bounds compare
UTF-8 strings lexicographically with the same Rust ordering used by the
filter. They do not implement locale-aware collation.

For expressions:

| Expression | Prove no rows match | Prove every row matches |
| --- | --- | --- |
| `A AND B` | Either side impossible | Both sides fully match |
| `A OR B` | Both sides impossible | Either side fully matches |
| `NOT A` | A fully matches | A impossible |

These rules never prune a matching row on valid data. They can miss a
pruning opportunity when column correlations matter: per-column min/max
cannot establish every relationship between predicates. Fully matched
groups bypass filter-column IO unless that column is also needed for
projection or aggregation. A filtered `COUNT(*)` can therefore read zero
column bytes when every retained group is proven to match.

## Batch execution and late materialization

Binding resolves names to column indices and validates literal types once.
Filters operate on typed column batches and boolean masks, rather than
constructing a `Value` for every comparison. Boolean operators combine
masks. This is batch vectorization, without explicit SIMD intrinsics.

For each group, the executor first prunes or proves full matching. For a
partially matching group it loads filter columns and produces selected
row indices. Other needed columns are loaded only when at least one row
survives. Unreferenced columns are never read. This is late materialization
at column-chunk granularity: it does not read individual selected cells
from compressed chunks.

Rayon assigns independent groups to workers. Unix uses positioned reads;
Windows uses repeated `seek_read` calls with explicit offsets. Exact-read
handling accounts for short reads and interruptions. The executor collects
results in physical group order, so serial and parallel projection scans
have deterministic tie behavior. The full result is materialized in
memory; there is no spill-to-disk operator or streaming result protocol.

## Ordering and aggregates

Projection ordering supports source columns, output aliases, and 1-based
output positions, with multiple keys. Stable sorting preserves input
order on ties. With a limit k, each group can retain its local top k before
the final merge: any row ranked below k in its own group cannot enter the
global top k. This reduces merged output; the implementation still sorts
the local rows, rather than using a bounded heap or selection algorithm.

Grouped queries build one hash table per group and merge partial
accumulators by key. `COUNT`, `SUM`, `AVG`, `MIN`, and `MAX` have mergeable
states. Integer sums use `i128`, then `SUM` checks the final `Int64` range;
`AVG` converts the total to a floating-point result. `MIN` and `MAX` support
both stored types. Global aggregates over no rows return one row: counts
are zero and the others are null. Grouped empty input yields no rows.
Groups are emitted in key order before explicit ordering for determinism.

Unfiltered, ungrouped `COUNT`, `MIN`, and `MAX` can be answered from footer
row counts and zone maps alone. All requested aggregates must support this
path; adding `SUM` or `AVG` requires data scans. Metadata-only answers
still validate column names and types. There are no stored nulls, so
`COUNT(column)` equals `COUNT(*)`.

## SQL boundary

The grammar is documented in `src/query/ast.rs` and the README. `IN` and
`BETWEEN` lower to comparison trees, sharing the same binding, pruning,
and mask evaluation. Precedence is `NOT` > `AND` > `OR`; parentheses
change grouping. The lexer supports doubled quotes in string literals.
Only keyword matching is case-insensitive.

There are no joins, subqueries, `HAVING`, scalar arithmetic, quoted
identifiers, SQL comments, general scalar functions, or null predicates.
The `FROM` identifier is not resolved against a catalog: the caller
supplies one `TableReader`. `ORDER BY` on aggregate queries must name an
output column/alias or position. Null aggregate outputs sort first
ascending and last descending. These choices define Stratum's subset;
they do not claim full compatibility with a SQL standard.

## Recorded benchmark

Command:

```sh
cargo run --locked --release --bin bench_prune -- --rows 2000000 --iterations 7 --threads 2 --seed 42
```

Recorded October 6, 2026 (Asia/Tbilisi), on local macOS ARM64 with Rust
1.97.0, release optimization and two Rayon workers. Each mode gets a warm-up
and seven timed iterations; numbers are medians. Data generation, writing,
and opening the reader are outside the timer. Result equivalence checks
run outside the timer for every execution. OS page caches are warm; modes
run in a fixed sequence. This is a reproducible demonstration, not a
controlled cross-database performance study.

| Workload / mode | Median | Groups scanned | Encoded MiB read | Output rows |
| --- | ---: | ---: | ---: | ---: |
| Clustered, pruning + parallel | 0.18 ms | 3 / 245 | 0.03 | 3,552 |
| Clustered, full scan + parallel | 2.85 ms | 245 / 245 | 3.34 | 3,552 |
| Clustered, full scan + serial | 5.19 ms | 245 / 245 | 3.34 | 3,552 |
| Random, pruning + parallel | 4.43 ms | 245 / 245 | 4.77 | 1,971 |
| Random, full scan + parallel | 4.46 ms | 245 / 245 | 4.77 | 1,971 |
| Random, full scan + serial | 8.16 ms | 245 / 245 | 4.77 | 1,971 |
| Grouped COUNT/AVG, parallel | 26.53 ms | 245 / 245 | 4.34 | 16 |
| Grouped COUNT/AVG, serial | 40.74 ms | 245 / 245 | 4.34 | 16 |
| Unfiltered COUNT/MIN/MAX | <0.01 ms | 0 / 245 | 0.00 | 1 |

The selective query is `SELECT id FROM t WHERE value > 999000`. Clustered
values trend upward with local jitter; random values are uniform across
the full domain. Both tables have sequential IDs and 16 sensor labels.
The clustered table occupies 4.4 MiB and the random table 5.8 MiB.
Sequential IDs cost about 0.002 encoded bytes per row; clustered values
1.751, random values 2.501, and sensor labels 0.526. Footer overhead is
included in total file size but excluded from column chunk byte counts.

Pruning skips 242/245 groups (98.8%) for clustered values. The measured
28.7× improvement versus the serial baseline combines parallelism and
pruning; versus the parallel baseline it is about 15.8× using rounded
timings. Random data provides no pruning opportunities. More threads,
other selectivity, cold IO, different cardinality, or different data order
can change the results substantially.

## Verification and next steps

Unit tests cover packing widths, extreme values, adaptive choices,
dictionaries, lexing, and parsing. Integration tests cover exact query
answers, aggregate semantics, overflow, top-k ties, metadata-only reads,
malformed layouts, truncated files, schema/row validation, and complete
CLI import/export behavior. Differential tests generate 720 queries over
12 seeded tables and compare each against a naive in-memory evaluator in
all four pruning/parallel combinations, including grouped aggregation.

CI runs the same build, test, formatting, and Clippy checks on Linux,
macOS, and Windows. A release benchmark smoke run checks the benchmark
path; no unstable performance threshold gates merges.

Natural extensions are checksummed chunks, a streaming/spilling executor,
a bounded-heap top-k operator, more stored types with null bitmaps, and
compression layered over encodings. Schema evolution, transactions, and
joins require larger format/execution designs. They remain beyond this
portfolio project's supported scope.
