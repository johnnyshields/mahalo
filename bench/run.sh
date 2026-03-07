#!/usr/bin/env bash
#
# Mahalo Framework Benchmark Suite
#
# Usage:
#   ./run.sh                    # Run all available frameworks
#   ./run.sh --rust-only        # Only Rust frameworks
#   ./run.sh --framework NAME   # Run a specific framework
#   ./run.sh --duration 30      # Custom duration in seconds (default: 15)
#   ./run.sh --connections 256  # Custom concurrent connections (default: 128)
#   ./run.sh --scenarios all    # Scenario group (see below)
#   ./run.sh --cores 4          # Pin each server to N cores (default: auto)
#
# Scenario groups:
#   plaintext     - Just /plaintext
#   json          - Just /json
#   techempower   - TechEmpower suite: plaintext, json, db, queries, fortunes, updates, cached-queries
#   api           - REST API: path params, search, POST echo
#   pipeline      - Middleware-heavy: browser page with full plug stack
#   all           - Everything (default)
#
# Frameworks:
#   Rust:    mahalo, axum, actix, xitca, may-minihttp
#   Node:    express
#   Ruby:    puma, falcon
#   Elixir:  elixir
#   Python:  granian
#   Static:  ferron
#
# Requirements:
#   - oha (HTTP benchmarking tool): cargo install oha
#   - Rust toolchain (for Rust servers)
#   - Optional: Node.js, Ruby+Bundler, Elixir+Mix, Python+granian

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
CORES=""  # empty = no pinning

# Port assignments
PORT_MAHALO=3000
PORT_AXUM=3001
PORT_ACTIX=3002
PORT_EXPRESS=3003
PORT_PUMA=3005
PORT_FERRON=3006
PORT_ELIXIR=3007
PORT_GRANIAN=3008
PORT_FALCON=3009
PORT_MAY_MINIHTTP=3010
PORT_XITCA=3011

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
        --cores) CORES="$2"; shift 2 ;;
        --help|-h)
            head -35 "$0" | tail -33
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

mkdir -p "$RESULTS_DIR"

# ─── Track PIDs and their names ──────────────────────────────────────
declare -A PID_NAMES=()
PIDS=()

cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    PIDS=()
    PID_NAMES=()
}

trap cleanup EXIT INT TERM

log() { echo -e "${BLUE}[bench]${NC} $*"; }
ok()  { echo -e "${GREEN}[  ok ]${NC} $*"; }
skip() { echo -e "${YELLOW}[ skip]${NC} $*"; }
err() { echo -e "${RED}[error]${NC} $*"; }

# ─── CPU pinning helper ─────────────────────────────────────────────
# If --cores is set, pin the next launched process to a specific CPU range.
# This ensures apples-to-apples comparison: each server gets the same cores.
# Usage: run_pinned <cmd> [args...]
# The core range auto-advances so the oha client gets separate cores.
NEXT_CORE=0
TOTAL_CORES=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 8)

run_pinned() {
    if [ -n "$CORES" ] && command -v taskset &>/dev/null; then
        local end_core=$(( NEXT_CORE + CORES - 1 ))
        if [ $end_core -ge "$TOTAL_CORES" ]; then
            # Wrap around — reset and don't pin this one
            NEXT_CORE=0
            "$@" &
        else
            taskset -c "${NEXT_CORE}-${end_core}" "$@" &
        fi
        NEXT_CORE=$(( end_core + 1 ))
    else
        "$@" &
    fi
}

# ─── Process metrics ────────────────────────────────────────────────
# Capture RSS (KB), CPU% for a given PID
capture_process_metrics() {
    local pid=$1
    local name=$2

    if [ ! -d "/proc/$pid" ] 2>/dev/null; then
        return
    fi

    local rss_kb cpu_pct vsz_kb threads
    # Use ps for cross-platform metrics
    read -r rss_kb vsz_kb cpu_pct threads < <(
        ps -p "$pid" -o rss=,vsz=,pcpu=,nlwp= 2>/dev/null | tail -1 | awk '{print $1, $2, $3, $4}'
    ) || true

    if [ -n "$rss_kb" ] && [ "$rss_kb" != "0" ]; then
        local rss_mb
        rss_mb=$(awk "BEGIN {printf \"%.1f\", $rss_kb / 1024}")
        local vsz_mb
        vsz_mb=$(awk "BEGIN {printf \"%.1f\", ${vsz_kb:-0} / 1024}")
        printf "  ${BOLD}%-20s${NC} %8s MB RSS | %8s MB VSZ | %5s%% CPU | %4s threads\n" \
            "Process ($name):" "$rss_mb" "$vsz_mb" "${cpu_pct:-?}" "${threads:-?}"

        {
            echo "  Process ($name):  ${rss_mb} MB RSS | ${vsz_mb} MB VSZ | ${cpu_pct:-?}% CPU | ${threads:-?} threads"
        } >> "$RESULTS_FILE"
    fi
}

# Capture metrics for all running servers
capture_all_metrics() {
    echo -e "\n${CYAN}>>> ${BOLD}Process Metrics (after load)${NC}"
    echo "" >> "$RESULTS_FILE"
    echo "=== Process Metrics ===" >> "$RESULTS_FILE"
    for pid in "${PIDS[@]}"; do
        local name="${PID_NAMES[$pid]:-unknown}"
        capture_process_metrics "$pid" "$name"
    done
    echo "" >> "$RESULTS_FILE"
}

# ─── Check tools ────────────────────────────────────────────────────
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
    local check_path=${3:-/plaintext}
    local max_wait=60
    local waited=0

    while ! curl -sf "http://127.0.0.1:${port}${check_path}" > /dev/null 2>&1; do
        sleep 0.2
        waited=$((waited + 1))
        if [ $waited -ge $((max_wait * 5)) ]; then
            err "$name failed to start on port $port within ${max_wait}s"
            return 1
        fi
    done
    ok "$name ready on port $port"
}

# Register a launched background process
register_pid() {
    local name=$1
    PIDS+=($!)
    PID_NAMES[$!]="$name"
}

# ─── Run a single benchmark scenario ────────────────────────────────
run_benchmark() {
    local name=$1
    local port=$2
    local endpoint=$3
    local method=${4:-GET}
    local extra_args=${5:-}
    local label="${name} ${method} ${endpoint}"

    echo -e "\n${CYAN}>>> ${BOLD}${label}${NC} (${CONNECTIONS} connections, ${DURATION}s)"

    # Build oha args
    local oha_args=(-z "${DURATION}s" -c "$CONNECTIONS" -w --no-tui)
    if [ "$method" != "GET" ]; then
        oha_args+=(-m "$method")
    fi
    # shellcheck disable=SC2086
    if [ -n "$extra_args" ]; then
        oha_args+=($extra_args)
    fi

    local url="http://127.0.0.1:${port}${endpoint}"

    # Warmup
    oha -z "${WARMUP}s" -c "$CONNECTIONS" -w --no-tui "$url" > /dev/null 2>&1 || true
    sleep 0.5

    # Actual benchmark
    local text_out
    text_out=$(oha "${oha_args[@]}" "$url" 2>&1) || true

    if [ -z "$text_out" ]; then
        err "No output from oha for $label"
        return 1
    fi

    # Parse metrics
    local rps avg_latency slowest fastest p50 p90 p99
    rps=$(echo "$text_out" | grep "Requests/sec:" | awk '{print $2}')
    avg_latency=$(echo "$text_out" | grep "Average:" | head -1 | awk '{print $2, $3}')
    slowest=$(echo "$text_out" | grep "Slowest:" | awk '{print $2, $3}')
    fastest=$(echo "$text_out" | grep "Fastest:" | awk '{print $2, $3}')
    p50=$(echo "$text_out" | grep "50.00%" | awk '{print $3, $4}')
    p90=$(echo "$text_out" | grep "90.00%" | awk '{print $3, $4}')
    p99=$(echo "$text_out" | grep "99.00%" | awk '{print $3, $4}')

    local status_line
    status_line=$(echo "$text_out" | grep '\[200\]\|\[302\]' | head -1 | xargs || echo "n/a")

    # Display
    printf "  ${BOLD}%-20s${NC} %14s req/s\n" "Throughput:" "$rps"
    printf "  %-20s %14s\n" "Avg Latency:" "$avg_latency"
    printf "  %-20s %14s\n" "Fastest:" "$fastest"
    printf "  %-20s %14s\n" "Slowest:" "$slowest"
    printf "  %-20s %14s\n" "p50 Latency:" "$p50"
    printf "  %-20s %14s\n" "p90 Latency:" "$p90"
    printf "  %-20s %14s\n" "p99 Latency:" "$p99"
    printf "  %-20s %14s\n" "Responses:" "$status_line"

    # Append to text results
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

    # Build JSON entry
    local rps_num="${rps//,/}"
    jq -n \
        --arg fw "$name" \
        --arg ep "$endpoint" \
        --arg mt "$method" \
        --argjson rps "${rps_num:-0}" \
        --arg avg "$avg_latency" \
        --arg fast "$fastest" \
        --arg slow "$slowest" \
        --arg p50v "$p50" \
        --arg p90v "$p90" \
        --arg p99v "$p99" \
        --arg resp "$status_line" \
        '{framework:$fw,endpoint:$ep,method:$mt,rps:$rps,avg_latency:$avg,fastest:$fast,slowest:$slow,p50:$p50v,p90:$p90v,p99:$p99v,responses:$resp}' \
        >> "${RESULTS_JSON}.tmp"
}

# ─── Framework launchers ────────────────────────────────────────────

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
    run_pinned env PORT=$PORT_MAHALO "$BENCH_DIR/../target/release/bench-mahalo"
    register_pid "mahalo"
    wait_for_server $PORT_MAHALO "Mahalo"
}

start_axum() {
    should_run "axum" || return 0
    log "Starting Axum (raw)..."
    run_pinned env PORT=$PORT_AXUM "$BENCH_DIR/../target/release/bench-axum"
    register_pid "axum"
    wait_for_server $PORT_AXUM "Axum (raw)"
}

start_actix() {
    should_run "actix" || return 0
    log "Starting Actix-web..."
    run_pinned env PORT=$PORT_ACTIX "$BENCH_DIR/../target/release/bench-actix"
    register_pid "actix"
    wait_for_server $PORT_ACTIX "Actix-web"
}

start_xitca() {
    should_run "xitca" || return 0
    log "Starting xitca-web..."
    run_pinned env PORT=$PORT_XITCA "$BENCH_DIR/../target/release/bench-xitca"
    register_pid "xitca"
    wait_for_server $PORT_XITCA "xitca-web"
}

start_may_minihttp() {
    should_run "may-minihttp" || return 0
    log "Starting may-minihttp..."
    run_pinned env PORT=$PORT_MAY_MINIHTTP "$BENCH_DIR/../target/release/bench-may-minihttp"
    register_pid "may-minihttp"
    wait_for_server $PORT_MAY_MINIHTTP "may-minihttp"
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
    run_pinned env PORT=$PORT_EXPRESS node "$BENCH_DIR/frameworks/node/express.js"
    register_pid "express"
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
    register_pid "puma"
    wait_for_server $PORT_PUMA "Puma (Rack)"
}

start_falcon() {
    should_run "falcon" || return 0
    if ! command -v ruby &>/dev/null || ! command -v bundle &>/dev/null; then
        skip "Falcon (Ruby/Bundler not found)"
        return 0
    fi
    if [ ! -f "$BENCH_DIR/frameworks/ruby/Gemfile.lock" ]; then
        log "Installing Ruby dependencies..."
        (cd "$BENCH_DIR/frameworks/ruby" && bundle install --quiet 2>&1 | tail -1)
    fi
    log "Starting Falcon (Rack)..."
    (cd "$BENCH_DIR/frameworks/ruby" && \
        bundle exec falcon serve --bind http://0.0.0.0:$PORT_FALCON --count 4 --quiet config.ru) &
    register_pid "falcon"
    wait_for_server $PORT_FALCON "Falcon (Rack)"
}

start_ferron() {
    should_run "ferron" || return 0
    if ! command -v ferron &>/dev/null; then
        skip "Ferron (binary not found, install with: cargo install ferron)"
        return 0
    fi
    log "Starting Ferron..."
    local config="$BENCH_DIR/frameworks/ferron/ferron.kdl"
    run_pinned ferron --config "$config"
    register_pid "ferron"
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
    register_pid "elixir"
    wait_for_server $PORT_ELIXIR "Plug/Cowboy"
}

start_granian() {
    should_run "granian" || return 0
    if ! command -v granian &>/dev/null; then
        skip "Granian (not found, install with: pip install granian)"
        return 0
    fi
    local workers=${CORES:-4}
    log "Starting Granian (ASGI, ${workers} workers)..."
    run_pinned granian --interface asgi --host 0.0.0.0 --port $PORT_GRANIAN \
        --workers "$workers" --log-level warning "$BENCH_DIR/frameworks/python/app.py:app"
    register_pid "granian"
    wait_for_server $PORT_GRANIAN "Granian (ASGI)"
}

# ─── Scenario definitions ───────────────────────────────────────────

# Full-framework servers support all scenarios.
# Low-level servers (xitca, may-minihttp, ferron) only support TechEmpower.

bench_plaintext() {
    local name=$1 port=$2
    run_benchmark "$name" "$port" "/plaintext"
}

bench_json() {
    local name=$1 port=$2
    run_benchmark "$name" "$port" "/json"
}

bench_techempower() {
    local name=$1 port=$2
    run_benchmark "$name" "$port" "/plaintext"
    run_benchmark "$name" "$port" "/json"
    run_benchmark "$name" "$port" "/db"
    run_benchmark "$name" "$port" "/queries?queries=20"
    run_benchmark "$name" "$port" "/fortunes"
    run_benchmark "$name" "$port" "/updates?queries=20"
    run_benchmark "$name" "$port" "/cached-queries?count=100"
}

bench_api() {
    local name=$1 port=$2
    run_benchmark "$name" "$port" "/api/users/42"
    run_benchmark "$name" "$port" "/api/search?q=User&page=1&limit=20"
    run_benchmark "$name" "$port" "/api/echo" "POST" '-d {"name":"test","email":"test@example.com"} -T application/json'
}

bench_pipeline() {
    local name=$1 port=$2
    run_benchmark "$name" "$port" "/browser/page"
}

# Full scenario suite (for full-framework servers)
bench_all_full() {
    local name=$1 port=$2
    bench_techempower "$name" "$port"
    bench_api "$name" "$port"
    bench_pipeline "$name" "$port"
    run_benchmark "$name" "$port" "/redirect"
}

# TechEmpower-only suite (for low-level servers)
bench_all_limited() {
    local name=$1 port=$2
    bench_techempower "$name" "$port"
}

# Run the chosen scenario group — full framework
run_scenarios_full() {
    local name=$1 port=$2
    case "$SCENARIOS" in
        plaintext)    bench_plaintext "$name" "$port" ;;
        json)         bench_json "$name" "$port" ;;
        techempower)  bench_techempower "$name" "$port" ;;
        api)          bench_api "$name" "$port" ;;
        pipeline)     bench_pipeline "$name" "$port" ;;
        all)          bench_all_full "$name" "$port" ;;
        *)            err "Unknown scenario group: $SCENARIOS"; exit 1 ;;
    esac
}

# Run the chosen scenario group — limited (TechEmpower only)
run_scenarios_limited() {
    local name=$1 port=$2
    case "$SCENARIOS" in
        plaintext)    bench_plaintext "$name" "$port" ;;
        json)         bench_json "$name" "$port" ;;
        techempower)  bench_techempower "$name" "$port" ;;
        api|pipeline) skip "$name does not support $SCENARIOS scenarios" ;;
        all)          bench_all_limited "$name" "$port" ;;
        *)            err "Unknown scenario group: $SCENARIOS"; exit 1 ;;
    esac
}

# ─── Print summary ──────────────────────────────────────────────────

print_summary() {
    echo -e "\n${BOLD}${CYAN}============================================${NC}"
    echo -e "${BOLD}${CYAN}  BENCHMARK SUMMARY${NC}"
    echo -e "${BOLD}${CYAN}============================================${NC}"
    echo -e "  Duration: ${DURATION}s | Connections: ${CONNECTIONS} | Warmup: ${WARMUP}s"
    echo -e "  Scenarios: ${SCENARIOS}"
    if [ -n "$CORES" ]; then
        echo -e "  CPU cores per server: ${CORES}"
    fi
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

# ─── Helper: run if server is reachable ──────────────────────────────

try_bench_full() {
    local name=$1 port=$2
    if should_run "$name" && curl -sf "http://127.0.0.1:${port}/plaintext" > /dev/null 2>&1; then
        run_scenarios_full "$name" "$port"
    fi
}

try_bench_limited() {
    local name=$1 port=$2
    if should_run "$name" && curl -sf "http://127.0.0.1:${port}/plaintext" > /dev/null 2>&1; then
        run_scenarios_limited "$name" "$port"
    fi
}

# ─── Main ────────────────────────────────────────────────────────────

main() {
    echo -e "${BOLD}${CYAN}"
    echo "  ╔══════════════════════════════════════════╗"
    echo "  ║     Mahalo Framework Benchmark Suite     ║"
    echo "  ╚══════════════════════════════════════════╝"
    echo -e "${NC}"

    check_oha

    # System info
    local cpu_count
    cpu_count=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo unknown)
    log "System: $(uname -srm) | CPUs: $cpu_count"
    if [ -n "$CORES" ]; then
        log "CPU pinning: ${CORES} cores per server (taskset)"
    fi

    {
        echo "Mahalo Framework Benchmark"
        echo "Date: $(date)"
        echo "Duration: ${DURATION}s | Connections: ${CONNECTIONS} | Warmup: ${WARMUP}s"
        echo "Scenarios: ${SCENARIOS}"
        echo "System: $(uname -srm)"
        echo "CPUs: $cpu_count"
        [ -n "$CORES" ] && echo "Pinned cores per server: $CORES"
        echo ""
    } > "$RESULTS_FILE"

    # Build Rust binaries
    build_rust

    # ── Start servers ────────────────────────────────────────────

    log "Starting servers..."

    # Full-framework Rust servers
    start_mahalo
    start_axum
    start_actix

    # Low-level Rust servers
    start_xitca
    start_may_minihttp

    # Non-Rust frameworks
    if [ "$RUST_ONLY" = false ]; then
        start_ferron
        start_express
        start_puma
        start_falcon
        start_elixir
        start_granian
    fi

    echo ""
    log "All servers started. Beginning benchmarks..."

    # ── Run benchmarks ───────────────────────────────────────────

    # Full-framework Rust servers (all scenarios)
    should_run "mahalo"  && run_scenarios_full "Mahalo"     $PORT_MAHALO  || true
    should_run "axum"    && run_scenarios_full "Axum"       $PORT_AXUM    || true
    should_run "actix"   && run_scenarios_full "Actix-web"  $PORT_ACTIX   || true

    # Low-level Rust servers (TechEmpower only)
    should_run "xitca"         && run_scenarios_limited "xitca-web"     $PORT_XITCA         || true
    should_run "may-minihttp"  && run_scenarios_limited "may-minihttp"  $PORT_MAY_MINIHTTP  || true

    # Non-Rust frameworks
    if [ "$RUST_ONLY" = false ]; then
        try_bench_limited "ferron"  $PORT_FERRON
        try_bench_full    "express" $PORT_EXPRESS  || true
        try_bench_full    "puma"    $PORT_PUMA     || true
        try_bench_full    "falcon"  $PORT_FALCON   || true
        try_bench_full    "elixir"  $PORT_ELIXIR   || true
        try_bench_full    "granian" $PORT_GRANIAN  || true
    fi

    # ── Capture process metrics after load ───────────────────────
    capture_all_metrics

    print_summary
}

main
