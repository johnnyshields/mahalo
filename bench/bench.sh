#!/usr/bin/env bash
#
# Quick benchmark: Mahalo vs comparison servers
#
# Usage:
#   ./bench.sh                  # Run all Rust frameworks
#   ./bench.sh mahalo           # Run only mahalo
#   ./bench.sh mahalo axum      # Run mahalo and axum
#
# Requirements:
#   - oha: cargo install oha
#
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_DIR="$(cd "$BENCH_DIR/.." && pwd)"
DURATION="${DURATION:-10}"
CONNECTIONS="${CONNECTIONS:-128}"
PORT=3000
URL="http://127.0.0.1:$PORT"

ALL_BINS=(bench-mahalo bench-axum bench-actix bench-hyper bench-xitca)

# Parse args: filter to specific binaries, or run all
if [ $# -gt 0 ]; then
    BINS=()
    for name in "$@"; do
        BINS+=("bench-$name")
    done
else
    BINS=("${ALL_BINS[@]}")
fi

echo "=== Mahalo Benchmark Suite ==="
echo "Duration: ${DURATION}s | Connections: ${CONNECTIONS} | Endpoints: /plaintext /json"
echo

cd "$WORKSPACE_DIR"

for bin in "${BINS[@]}"; do
    # Check if the binary source exists
    src="bench/src/bin/${bin#bench-}.rs"
    if [ ! -f "$src" ]; then
        echo "--- Skipping $bin (no source at $src) ---"
        echo
        continue
    fi

    echo "=== $bin ==="
    cargo build --release --bin "$bin" 2>/dev/null

    ./target/release/"$bin" &
    PID=$!

    # Wait for server to start
    for i in $(seq 1 20); do
        if curl -s -o /dev/null "$URL/plaintext" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done

    echo "--- /plaintext ---"
    oha -z "${DURATION}s" -c "$CONNECTIONS" --no-tui "$URL/plaintext" 2>&1 | \
        grep -E "Requests/sec|Slowest|Fastest|Average|50%|95%|99%"

    echo "--- /json ---"
    oha -z "${DURATION}s" -c "$CONNECTIONS" --no-tui "$URL/json" 2>&1 | \
        grep -E "Requests/sec|Slowest|Fastest|Average|50%|95%|99%"

    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
    echo
done
