# Verification results

Recorded October 6, 2026, on macOS ARM64 using the pinned Rust 1.97.0
toolchain. These are local verification results. GitHub runs the configured
Linux, macOS, and Windows checks after a push; their live status is available
in [GitHub Actions](https://github.com/pircxo/stratum/actions).

| Check | Result |
| --- | --- |
| Unit tests | 29 passed |
| CLI integration tests | 6 passed |
| Differential tests | 2 passed |
| Engine end-to-end tests | 22 passed |
| Storage validation tests | 9 passed |
| Total | **68 passed, 0 failed** |
| Clippy with warnings treated as errors | Passed |
| Formatting | Passed |
| Documentation with warnings treated as errors | Passed |
| Demo script | Completed successfully |

The differential suite checks 720 seeded queries against a simple
reference evaluator in four execution modes: pruning on/off and
parallel/sequential scanning.

## Benchmark

On 2 million clustered rows, zone maps skipped **242 of 245 row groups
(98.8%)**. The selective query scanned 3 groups and returned the same 3,552
rows as the full scans. It pruned no groups on the random layout.

The measured clustered medians were 0.18 ms with pruning and parallelism,
2.85 ms for a parallel full scan, and 5.19 ms for a serial full scan.
These warm-cache measurements use two worker threads and seven iterations
after warm-up; the speed ratio is specific to this workload and machine.

See [the recorded benchmark and methodology](ARCHITECTURE.md#recorded-benchmark).

## Reproduce

Install Rust as described in the [quick start](README.md#quick-start), then
run these commands from the repository directory:

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
sh scripts/demo.sh
cargo run --locked --release --bin bench_prune -- --rows 2000000 --iterations 7 --threads 2 --seed 42
```

The demo is a command-line program, so run its command in a terminal.
It imports the included sample, inspects the table, executes SQL queries,
and exports CSV. For CV wording, see [Presenting Stratum](PORTFOLIO.md).
