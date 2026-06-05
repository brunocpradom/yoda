#!/usr/bin/env bash
# Launch Yoda against the CURRENT directory, no matter where this script lives or
# is called from. It locates its own binary (building it on first run) and then
# execs it, so Yoda's "project directory" is wherever you invoked this from.
#
# Usage:
#   ~/code/yoda_harness/run.sh        # from any directory
#   (or symlink it onto your PATH — see README)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
BIN="$SCRIPT_DIR/target/release/yoda"

if [[ ! -x "$BIN" ]]; then
  echo "Building Yoda (first run, release mode)…" >&2
  cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"
fi

exec "$BIN" "$@"
