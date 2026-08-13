#!/bin/bash
# scripts/run_benchmarks.sh
# Runs the terminal performance benchmarks and generates a report.

set -e

echo "=== FlashTerminal Benchmark Suite ==="
echo "Running criterion benchmarks..."

# Run the benchmarks
cargo bench -p benchmarks -- --save-baseline latest

echo ""
echo "=== Generating Performance Report ==="
cargo run -p benchmarks --release

echo ""
echo "Benchmark complete. Check the output above for budget comparisons."
echo "For detailed flamegraphs, run: cargo flamegraph --bin desktop"