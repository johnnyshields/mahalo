# Benchmark Results

Single-thread, 128 concurrent connections, 10-second runs using [oha](https://github.com/hatoo/oha). All servers bound to `0.0.0.0:3000` with mimalloc allocator. Measured on WSL2 Linux 6.6.

## /plaintext

| Framework | Requests/sec | Avg Latency |
|-----------|-------------|-------------|
| xitca-web | 222,483 | 0.57ms |
| actix-web | 219,970 | 0.58ms |
| **Mahalo** | 198,927 | 0.64ms |
| Hyper (raw) | 168,653 | 0.76ms |
| Axum | 152,755 | 0.84ms |

## /json

| Framework | Requests/sec | Avg Latency |
|-----------|-------------|-------------|
| actix-web | 209,693 | 0.61ms |
| xitca-web | 197,983 | 0.64ms |
| **Mahalo** | 194,075 | 0.66ms |
| Hyper (raw) | 159,161 | 0.80ms |
| Axum | 141,923 | 0.90ms |

## Running the benchmarks

```bash
# All frameworks
bash bench/bench.sh

# Specific frameworks
bash bench/bench.sh mahalo
bash bench/bench.sh mahalo axum actix

# Custom duration and connections
DURATION=30 CONNECTIONS=256 bash bench/bench.sh
```

## Notes

- Mahalo uses a thread-per-core architecture with compio (io_uring on Linux) and turbine epoch-arena buffer pools.
- The `/plaintext` endpoint uses `sync_plug_fn` with `put_resp_body_static` for zero-copy responses.
- The `/json` endpoint serializes `{"message":"Hello, World!"}` via serde_json.
- All Rust frameworks use mimalloc as the global allocator.
- Mahalo runs the "bare" pipeline (zero middleware) for these TechEmpower-style endpoints.
