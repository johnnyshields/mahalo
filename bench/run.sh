#!/usr/bin/env bash
#
# Mahalo Framework Benchmark Suite
#
# Usage:
#   ./run.sh                    # Run all available frameworks
#   ./run.sh --rust-only        # Only Rust frameworks (Mahalo, Axum, Actix)
#   ./run.sh --framework NAME   # Run a specific framework
#   ./run.sh --duration 30      # Custom duration in seconds (default: 15)
#   ./run.sh --connections 256  # Custom concurrent connections (default: 128)
#   ./run.sh --scenarios all    # Scenarios: plaintext, json, all (default: all)
#
# Requirements:
#   - oha (HTTP benchmarking tool): cargo install oha
#   - Rust toolchain (for Mahalo, Axum, Actix servers)
#   - Optional: Node.js (for Express)
#   - Optional: Ruby + Bundler (for Rack/Puma)
#   - Optional: Elixir + Mix (for Plug/Cowboy)

set -euo pipefail

BENCH_DIR="$(cd "$(dirname "$0")" && pwd)"
RESULTS_DIR="$BENCH_DIR/results"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
RESULTS_FILE="$RESULTS_DIR/bench_${TIMESTAMP}.txt"
RESULTS_JSON="$RESULTS_DIR/bench_${TIMESTAMP}.json"

# Defaults
DURATION=15
CONNECTIONS=128
SCENARIOS="all"
RUST_ONLY=false
FRAMEWORK_FILTER=""
WARMUP=3

# Port assignments
PORT_MAHALO=3000
PORT_AXUM=3001
PORT_ACTIX=3002
PORT_EXPRESS=3003
PORT_PUMA=3005
PORT_ELIXIR=3007
PORT_FERRON=3006

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --rust-only) RUST_ONLY=true; shift ;;
        --framework) FRAMEWORK_FILTER="$2"; shift 2 ;;
        --duration) DURATION="$2"; shift 2 ;;
        --connections) CONNECTIONS="$2"; shift 2 ;;
        --scenarios) SCENARIOS="$2"; shift 2 ;;
        --warmup) WARMUP="$2"; shift 2 ;;
        --help|-h)
            head -18 "$0" | tail -16
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

mkdir -p "$RESULTS_DIR"

# Track all PIDs for cleanup
PIDS=()

cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    PIDS=()
}

trap cleanup EXIT INT TERM

log() { echo -e "${BLUE}[bench]${NC} $*"; }
ok()  { echo -e "${GREEN}[  ok ]${NC} $*"; }
skip() { echo -e "${YELLOW}[ skip]${NC} $*"; }
err() { echo -e "${RED}[error]${NC} $*"; }

# Check for oha
check_oha() {
    if ! command -v oha &>/dev/null; then
        err "oha not found. Install with: cargo install oha"
        echo "  oha is a modern HTTP benchmarking tool (like wrk, written in Rust)"
        exit 1
    fi
    ok "oha found: $(oha --version 2>&1 || echo 'unknown version')"
}

# Wait for a server to be ready
wait_for_server() {
    local port=$1
    local name=$2
    local max_wait=60
    local waited=0

    while ! curl -sf "http://127.0.0.1:${port}/plaintext" > /dev/null 2>&1; do
        sleep 0.2
        waited=$((waited + 1))
        if [ $waited -ge $((max_wait * 5)) ]; then
            err "$name failed to start on port $port within ${max_wait}s"
            return 1
        fi
    done
    ok "$name ready on port $port"
}

# Run a single benchmark scenario
run_benchmark() {
    local name=$1
    local port=$2
    local endpoint=$3
    local label="${name} ${endpoint}"

    echo -e "\n${CYAN}>>> ${BOLD}${label}${NC} (${CONNECTIONS} connections, ${DURATION}s)"

    # Warmup
    oha -z "${WARMUP}s" -c "$CONNECTIONS" -w --no-tui \
        "http://127.0.0.1:${port}${endpoint}" > /dev/null 2>&1 || true
    sleep 0.5

    # Actual benchmark - capture text output (oha JSON output has bugs with latency)
    local text_out
    text_out=$(oha -z "${DURATION}s" -c "$CONNECTIONS" -w --no-tui \
        "http://127.0.0.1:${port}${endpoint}" 2>&1) || true

    if [ -z "$text_out" ]; then
        err "No output from oha for $label"
        return 1
    fi

    # Parse metrics from text output
    local rps avg_latency slowest fastest p50 p90 p99 total_reqs
    rps=$(echo "$text_out" | grep "Requests/sec:" | awk '{print $2}')
    avg_latency=$(echo "$text_out" | grep "Average:" | head -1 | awk '{print $2, $3}')
    slowest=$(echo "$text_out" | grep "Slowest:" | awk '{print $2, $3}')
    fastest=$(echo "$text_out" | grep "Fastest:" | awk '{print $2, $3}')
    p50=$(echo "$text_out" | grep "50.00%" | awk '{print $3, $4}')
    p90=$(echo "$text_out" | grep "90.00%" | awk '{print $3, $4}')
    p99=$(echo "$text_out" | grep "99.00%" | awk '{print $3, $4}')

    # Extract total 2xx responses from status code distribution
    total_reqs=$(echo "$text_out" | grep -oP '\[\d+\] \K\d+' | head -1 || echo "0")
    local status_line
    status_line=$(echo "$text_out" | grep '\[200\]' | head -1 | xargs || echo "n/a")

    # Format output
    printf "  ${BOLD}%-20s${NC} %14s req/s\n" "Throughput:" "$rps"
    printf "  %-20s %14s\n" "Avg Latency:" "$avg_latency"
    printf "  %-20s %14s\n" "Fastest:" "$fastest"
    printf "  %-20s %14s\n" "Slowest:" "$slowest"
    printf "  %-20s %14s\n" "p50 Latency:" "$p50"
    printf "  %-20s %14s\n" "p90 Latency:" "$p90"
    printf "  %-20s %14s\n" "p99 Latency:" "$p99"
    printf "  %-20s %14s\n" "Responses:" "$status_line"

    # Append to results file
    {
        echo "=== $label ==="
        echo "  Throughput:     $rps req/s"
        echo "  Avg Latency:    $avg_latency"
        echo "  Fastest:        $fastest"
        echo "  Slowest:        $slowest"
        echo "  p50 Latency:    $p50"
        echo "  p90 Latency:    $p90"
        echo "  p99 Latency:    $p99"
        echo "  Responses:      $status_line"
        echo ""
    } >> "$RESULTS_FILE"

    # Build JSON entry with jq for proper escaping
    local rps_num="${rps//,/}"
    jq -n \
        --arg fw "$name" \
        --arg ep "$endpoint" \
        --argjson rps "${rps_num:-0}" \
        --arg avg "$avg_latency" \
        --arg fast "$fastest" \
        --arg slow "$slowest" \
        --arg p50v "$p50" \
        --arg p90v "$p90" \
        --arg p99v "$p99" \
        --arg resp "$status_line" \
        '{framework:$fw,endpoint:$ep,rps:$rps,avg_latency:$avg,fastest:$fast,slowest:$slow,p50:$p50v,p90:$p90v,p99:$p99v,responses:$resp}' \
        >> "${RESULTS_JSON}.tmp"
}

# Framework launchers
should_run() {
    local name=$1
    if [ -n "$FRAMEWORK_FILTER" ] && [ "$FRAMEWORK_FILTER" != "$name" ]; then
        return 1
    fi
    return 0
}

build_rust() {
    log "Building Rust servers (release)..."
    (cd "$BENCH_DIR/.." && cargo build -p mahalo-bench --release 2>&1 | tail -1)
}

start_mahalo() {
    should_run "mahalo" || return 0
    log "Starting Mahalo..."
    PORT=$PORT_MAHALO "$BENCH_DIR/../target/release/bench-mahalo" &
    PIDS+=($!)
    wait_for_server $PORT_MAHALO "Mahalo"
}

start_axum() {
    should_run "axum" || return 0
    log "Starting Axum (raw)..."
    PORT=$PORT_AXUM "$BENCH_DIR/../target/release/bench-axum" &
    PIDS+=($!)
    wait_for_server $PORT_AXUM "Axum (raw)"
}

start_actix() {
    should_run "actix" || return 0
    log "Starting Actix-web..."
    PORT=$PORT_ACTIX "$BENCH_DIR/../target/release/bench-actix" &
    PIDS+=($!)
    wait_for_server $PORT_ACTIX "Actix-web"
}

start_express() {
    should_run "express" || return 0
    if ! command -v node &>/dev/null; then
        skip "Express (Node.js not found)"
        return 0
    fi
    if [ ! -d "$BENCH_DIR/frameworks/node/node_modules" ]; then
        log "Installing Node dependencies..."
        (cd "$BENCH_DIR/frameworks/node" && npm install --silent 2>&1 | tail -1)
    fi
    log "Starting Express..."
    (cd "$BENCH_DIR/frameworks/node" && PORT=$PORT_EXPRESS node express.js) &
    PIDS+=($!)
    wait_for_server $PORT_EXPRESS "Express"
}

start_puma() {
    should_run "puma" || return 0
    if ! command -v ruby &>/dev/null || ! command -v bundle &>/dev/null; then
        skip "Puma/Rack (Ruby/Bundler not found)"
        return 0
    fi
    if [ ! -f "$BENCH_DIR/frameworks/ruby/Gemfile.lock" ]; then
        log "Installing Ruby dependencies..."
        (cd "$BENCH_DIR/frameworks/ruby" && bundle install --quiet 2>&1 | tail -1)
    fi
    log "Starting Puma (Rack)..."
    (cd "$BENCH_DIR/frameworks/ruby" && \
        bundle exec puma -p $PORT_PUMA -w 4 -t 8:8 --quiet config.ru) &
    PIDS+=($!)
    wait_for_server $PORT_PUMA "Puma (Rack)"
}

start_ferron() {
    should_run "ferron" || return 0
    if ! command -v ferron &>/dev/null; then
        skip "Ferron (binary not found, install with: cargo install ferron)"
        return 0
    fi
    log "Starting Ferron..."
    local config="$BENCH_DIR/frameworks/ferron/ferron.kdl"
    ferron --config "$config" &
    PIDS+=($!)
    wait_for_server $PORT_FERRON "Ferron"
}

start_elixir() {
    should_run "elixir" || return 0
    if ! command -v elixir &>/dev/null; then
        skip "Plug/Cowboy (Elixir not found)"
        return 0
    fi
    if [ ! -d "$BENCH_DIR/frameworks/elixir/deps" ]; then
        log "Fetching Elixir dependencies..."
        (cd "$BENCH_DIR/frameworks/elixir" && mix deps.get --quiet 2>&1 | tail -1)
    fi
    log "Starting Plug/Cowboy..."
    (cd "$BENCH_DIR/frameworks/elixir" && PORT=$PORT_ELIXIR mix run --no-halt) &
    PIDS+=($!)
    wait_for_server $PORT_ELIXIR "Plug/Cowboy"
}

# Run benchmarks for a framework
bench_framework() {
    local name=$1
    local port=$2

    if [ "$SCENARIOS" = "all" ] || [ "$SCENARIOS" = "plaintext" ]; then
        run_benchmark "$name" "$port" "/plaintext"
    fi
    if [ "$SCENARIOS" = "all" ] || [ "$SCENARIOS" = "json" ]; then
        run_benchmark "$name" "$port" "/json"
    fi
}

# Print summary table
print_summary() {
    echo -e "\n${BOLD}${CYAN}============================================${NC}"
    echo -e "${BOLD}${CYAN}  BENCHMARK SUMMARY${NC}"
    echo -e "${BOLD}${CYAN}============================================${NC}"
    echo -e "  Duration: ${DURATION}s | Connections: ${CONNECTIONS} | Warmup: ${WARMUP}s"
    echo -e "  Date: $(date)"
    echo -e "${CYAN}--------------------------------------------${NC}"

    if [ -f "$RESULTS_FILE" ]; then
        cat "$RESULTS_FILE"
    fi

    # Combine JSON results into an array
    if [ -f "${RESULTS_JSON}.tmp" ]; then
        jq -s '.' "${RESULTS_JSON}.tmp" > "$RESULTS_JSON" 2>/dev/null || true
        rm -f "${RESULTS_JSON}.tmp"
        echo -e "\n${GREEN}Results saved to:${NC}"
        echo "  Text: $RESULTS_FILE"
        echo "  JSON: $RESULTS_JSON"
    fi
}

# Main
main() {
    echo -e "${BOLD}${CYAN}"
    echo "  ╔══════════════════════════════════════════╗"
    echo "  ║     Mahalo Framework Benchmark Suite     ║"
    echo "  ╚══════════════════════════════════════════╝"
    echo -e "${NC}"

    check_oha

    {
        echo "Mahalo Framework Benchmark"
        echo "Date: $(date)"
        echo "Duration: ${DURATION}s | Connections: ${CONNECTIONS} | Warmup: ${WARMUP}s"
        echo "System: $(uname -srm)"
        echo "CPUs: $(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
        echo ""
    } > "$RESULTS_FILE"

    # Build Rust binaries once before starting any server
    build_rust

    # Start all servers
    log "Starting servers..."
    start_mahalo
    start_axum
    start_actix

    if [ "$RUST_ONLY" = false ]; then
        start_ferron
        start_express
        start_puma
        start_elixir
    fi

    echo ""
    log "All servers started. Beginning benchmarks..."

    # Run benchmarks
    should_run "mahalo"  && bench_framework "Mahalo"       $PORT_MAHALO
    should_run "axum"    && bench_framework "Axum (raw)"   $PORT_AXUM
    should_run "actix"   && bench_framework "Actix-web"    $PORT_ACTIX

    if [ "$RUST_ONLY" = false ]; then
        if should_run "ferron" && curl -sf "http://127.0.0.1:${PORT_FERRON}/plaintext" > /dev/null 2>&1; then
            bench_framework "Ferron" $PORT_FERRON
        fi
        if should_run "express" && curl -sf "http://127.0.0.1:${PORT_EXPRESS}/plaintext" > /dev/null 2>&1; then
            bench_framework "Express" $PORT_EXPRESS
        fi
        if should_run "puma" && curl -sf "http://127.0.0.1:${PORT_PUMA}/plaintext" > /dev/null 2>&1; then
            bench_framework "Puma (Rack)" $PORT_PUMA
        fi
        if should_run "elixir" && curl -sf "http://127.0.0.1:${PORT_ELIXIR}/plaintext" > /dev/null 2>&1; then
            bench_framework "Plug/Cowboy" $PORT_ELIXIR
        fi
    fi

    print_summary
}

main
