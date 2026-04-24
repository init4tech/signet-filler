# Signet Filler

**Keep this file and the readme up to date.** After any change to the repo (new files, renamed modules, added dependencies, changed conventions, etc.), update the relevant sections of this document and the README.md before finishing.

Order filler service for the Signet Parmigiana testnet. Monitors a transaction cache for pending orders, checks they are within an acceptable loss threshold using hardcoded exchange rates, and submits fill bundles shortly before each block boundary.

## Project Structure

```
bin/filler.rs - Binary entrypoint (tokio multi-thread runtime)
src/lib.rs - Library root, signal handling, module exports
src/config.rs - Environment-based configuration via `FromEnv` derive macro
src/allowance.rs - AllowanceCache with background Permit2 allowance refresh task (10-min interval)
src/chain_token_pair.rs - KnownToken enum, ChainTokenPair: (chain_id, token) identifier with human-readable Display
src/filler_task/mod.rs - FillerTask struct: slot-aligned filler loop, order processing pipeline (profitability scoring/sorting, budget check, Permit2 nonce check)
src/filler_task/initialization.rs - Provider/signer/tx-cache connection with retry, transient error classification
src/filler_task/preflight.rs - WorkingMap: per-cycle token budget tracking (fresh balances + cached allowances), ERC20 balance queries
src/metrics.rs - Prometheus metric definitions and recording helpers (counters, gauges, histograms)
src/service.rs - Healthcheck HTTP server (axum, graceful shutdown via CancellationToken)
src/fixed_pricing_client.rs - Fixed pricing with hardcoded token exchange rates, profitability scoring, and max loss threshold
Dockerfile - Multi-stage cargo-chef Docker build (rust:bookworm → debian:bookworm-slim)
.github/workflows/filler-ecr-cd.yml - CD workflow: build and push Docker image to AWS ECR
```

## Build & Run

- **Rust edition**: 2024, MSRV 1.88
- **Build**: `cargo build`
- **Run**: Set env vars (see `--help`) then `cargo run`
- **Formatting**: `cargo +nightly fmt` (uses `rustfmt.toml` with `reorder_imports`, `use_field_init_shorthand`, `use_small_heuristics = "Max"`)

## Key Dependencies

- **init4-bin-base**: Shared init4 binary utilities (tracing init via `init4()`, AWS/local signer, provider configs, `FromEnv` derive)
- **signet-sdk crates** (`signet-constants`, `signet-orders`, `signet-tx-cache`, `signet-types`): Signet chain types and constants
- **alloy**: Ethereum provider/signer/types
- **backon**: Retry with exponential backoff for provider connections
- **axum**: HTTP server for healthcheck endpoint
- **metrics**: Prometheus metrics (counters, gauges, histograms) — exporter initialized by `init4-bin-base::init4()` on port 9000
- **eyre**: Error handling (`Result`, `WrapErr`)

## Conventions

- Config uses `FromEnv` derive macro from `init4-bin-base` — all env vars prefixed `SIGNET_FILLER_` with defaults applied after loading
- `Config` exposes only getter methods; construction is internal via `config_from_env()`
- Provider connections retry indefinitely on transient errors using `backon`
- The filler loop uses `tokio::time::interval_at` aligned to chain slot boundaries minus `block_lead_duration`
- Order processing pipeline: fetch -> filled-cache filter -> profitability score/sort -> per-order budget+nonce check -> submit bundle
- Permit2 allowances are cached by a background task (10-min refresh); balances are queried fresh each cycle
- Per-cycle `WorkingMap` tracks running balance/allowance budgets, decremented as orders are accepted (MAX allowances are not decremented)
- Graceful shutdown via `CancellationToken` propagated through all async tasks
