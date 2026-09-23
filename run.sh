#!/usr/bin/env bash
# Sweep object counts, free vs pinned. Extra settings are passed through:
#   ./run.sh                    # idle machine
#   ./run.sh load=16            # all 16 logical CPUs busy
#   ./run.sh load=16 scatter=1  # busy + shuffled NPC access
set -e
cargo build --release
BIN=./target/release/tick-sim

for n in 100000 1000000 4000000 10000000; do
  for pin in none 3; do
    "$BIN" objects=$n pin=$pin seconds=15 "$@"
  done
done
