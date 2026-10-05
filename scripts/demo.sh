#!/usr/bin/env sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_dir"

# Existing terminal sessions may not have loaded rustup's shell setup.
if ! command -v cargo >/dev/null 2>&1; then
  cargo_env="${CARGO_HOME:-$HOME/.cargo}/env"
  if [ -f "$cargo_env" ]; then
    . "$cargo_env"
  fi
fi
if ! command -v cargo >/dev/null 2>&1; then
  printf '%s\n' 'Cargo was not found. Install Rust from https://rustup.rs/ and retry.' >&2
  exit 1
fi

demo_dir=$(mktemp -d "${TMPDIR:-/tmp}/stratum-demo.XXXXXX")
trap 'rm -rf "$demo_dir"' EXIT HUP INT TERM

cargo build --locked --release --bin stratum
stratum_cli="${CARGO_TARGET_DIR:-target}/release/stratum"
table="$demo_dir/readings.strat"

"$stratum_cli" load examples/readings.csv "$table" --schema id:int,sensor:str,value:int
"$stratum_cli" inspect "$table"
"$stratum_cli" query "$table" \
  'SELECT id, sensor, value FROM readings WHERE value > 1000 ORDER BY value DESC LIMIT 3' --explain
"$stratum_cli" query "$table" \
  'SELECT sensor, COUNT(*) AS samples, AVG(value) AS average FROM readings GROUP BY sensor ORDER BY average DESC'
"$stratum_cli" query "$table" \
  'SELECT COUNT(*) AS rows, MIN(value) AS minimum, MAX(value) AS maximum FROM readings' --explain
"$stratum_cli" query "$table" \
  "SELECT id, sensor FROM readings WHERE sensor IN ('alpha', 'gamma') AND NOT (value BETWEEN 300 AND 1200) ORDER BY id" --format csv
