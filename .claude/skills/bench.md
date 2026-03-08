# bench

Run the Mahalo benchmark suite comparing framework performance.

## Instructions

Run the benchmark suite from the `bench/` directory. There are two scripts:

- **`bench/run.sh`** — Full suite with all frameworks, warmup, JSON/text output, CPU pinning, process metrics. This is the default.
- **`bench/bench.sh`** — Quick comparison of Rust frameworks only (no warmup, no saved results).

### Default behavior

Run `bench/run.sh` with sensible defaults. Pass through any user-specified flags.

### Common flags for `run.sh`

| Flag | Description | Default |
|------|-------------|---------|
| `--rust-only` | Only Rust frameworks | all |
| `--framework NAME` | Single framework (mahalo, axum, actix, hyper, xitca, express, puma, falcon, elixir, granian, ferron) | all |
| `--duration N` | Seconds per scenario | 15 |
| `--connections N` | Concurrent connections | 128 |
| `--scenarios GROUP` | plaintext, json, techempower, api, pipeline, all | all |
| `--cores N` | Pin each server to N CPU cores | auto |
| `--warmup N` | Warmup seconds | 3 |

### Examples

```bash
# Run full suite
./bench/run.sh

# Quick Rust-only, plaintext only
./bench/run.sh --rust-only --scenarios plaintext

# Just mahalo vs axum, 30s duration
./bench/run.sh --framework mahalo --duration 30
# (run.sh only supports one --framework at a time; for two, use bench.sh)
./bench/bench.sh mahalo axum

# Quick bench (no warmup, no saved results)
./bench/bench.sh
```

### Prerequisites

- `oha` must be installed: `cargo install oha`
- Rust toolchain for Rust servers
- Optional: Node.js (express), Ruby+Bundler (puma, falcon), Elixir+Mix, Python+granian, ferron

### Results

`run.sh` saves results to `bench/results/` as both `.txt` and `.json` files with timestamps. After the run, show the user the results file path and key throughput numbers.
