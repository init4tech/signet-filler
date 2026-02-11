# Signet Filler

**Keep this file and the readme up to date.** After any change to the repo (new files, renamed modules, added dependencies, changed conventions, etc.), update the relevant sections of this document and the README.md before finishing.

Order filler service for the Signet Parmigiana testnet. Monitors a transaction cache for pending orders, evaluates profitability, and submits fill bundles shortly before each block boundary.

## Project Structure

```
bin/filler.rs - Binary entrypoint (tokio multi-thread runtime)
src/lib.rs - Library root, signal handling, module exports
src/config.rs - Environment-based configuration via `FromEnv` derive macro
src/filler_task/mod.rs - FillerTask struct: slot-aligned filler loop, order processing, Permit2 nonce fill-check, profitability checks
src/filler_task/initialization.rs - Provider/signer/tx-cache connection with retry, transient error classification
src/metrics.rs - Prometheus metric definitions and recording helpers (counters, gauges, histograms)
src/service.rs - Healthcheck HTTP server (axum, graceful shutdown via CancellationToken)
src/pricing/mod.rs - PricingClient trait, FillCostEstimate
src/pricing/radius_client.rs - Radius solver API pricing (queries GET /api/rfq/quote for profitability); supports 1→1, N→1, and 1→M order shapes with parallel quoting and two-round split estimation
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
- **signet-sdk crates** (`signet-constants`, `signet-orders`, `signet-tx-cache`, `signet-types`): Currently patched to git `main` branch
- **alloy**: Ethereum provider/signer/types
- **backon**: Retry with exponential backoff for provider connections
- **axum**: HTTP server for healthcheck endpoint
- **metrics**: Prometheus metrics (counters, gauges, histograms) — exporter initialized by `init4-bin-base::init4()` on port 9000
- **reqwest**: HTTP client for Radius solver API (with `json` feature)
- **serde**: Deserialization of Radius API responses
- **eyre**: Error handling (`Result`, `WrapErr`)

## Conventions

- Config uses `FromEnv` derive macro from `init4-bin-base` — all env vars prefixed `SIGNET_FILLER_` with defaults applied after loading
- `Config` exposes only getter methods; construction is internal via `config_from_env()`
- Provider connections retry indefinitely on transient errors using `backon`
- The filler loop uses `tokio::time::interval_at` aligned to chain slot boundaries minus `block_lead_duration`
- Graceful shutdown via `CancellationToken` propagated through all async tasks
