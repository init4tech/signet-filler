# Signet Filler

**Keep this file and the readme up to date.** After any change to the repo (new files, renamed modules, added dependencies, changed conventions, etc.), update the relevant sections of this document and the README.md before finishing.

Order filler service for the Signet Parmigiana testnet. Monitors a transaction cache for pending orders, checks they are within an acceptable loss threshold using hardcoded exchange rates, and submits fill bundles targeting configurable consecutive blocks shortly before each block boundary.

## Project Structure

```
bin/filler.rs - Binary entrypoint (tokio multi-thread runtime)
src/lib.rs - Library root, signal handling, module exports
src/config.rs - Environment-based configuration via `FromEnv` derive macro
src/allowance.rs - AllowanceCache with background Permit2 allowance refresh task (10-min interval)
src/chain_token_pair.rs - KnownToken enum, ChainTokenPair: (chain_id, token) identifier with human-readable Display
src/erc20.rs - Shared minimal ERC20 interface (balanceOf + allowance) used by allowance cache, preflight check, and startup balance report
src/initialization.rs - FillerContext with provider/signer/tx-cache connection (with retry and transient error classification), plus one-shot startup balance reporting for every known token
src/filler_task/mod.rs - FillerTask struct: slot-aligned filler loop, order processing pipeline (profitability scoring/sorting, budget check, Permit2 nonce check)
src/filler_task/in_flight.rs - InFlightTracker: tracks orders submitted in fill bundles whose Permit2 nonce has not yet been observed consumed (TTL = Permit2 deadline offset, i.e. `block_lead_duration + target_blocks * slot_duration + DEADLINE_DRIFT_BUFFER_SECS`); supplies earmarks to WorkingMap and short-circuits re-submission of in-flight orders
src/filler_task/preflight.rs - WorkingMap: per-cycle token budget tracking (fresh balances + cached allowances, pre-decremented by in-flight earmarks), ERC20 balance queries
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
- Order processing pipeline: in-flight reconciliation (queries Permit2 for each tracked order; landed orders have their tracker entry cleared and their hash inserted into the filled-orders LRU cache so the same-cycle filled-cache filter catches them) -> fetch -> expired-deadline filter -> filled-cache filter -> in-flight filter -> profitability score/sort -> per-order budget+nonce check -> submit bundle
- Fill bundles target a configurable number of consecutive blocks (`SIGNET_FILLER_TARGET_BLOCKS`, default 5); the Permit2 deadline offset is derived from `block_lead_duration + target_blocks * slot_duration`, plus a 5s drift buffer
- Orders per bundle can be capped via `SIGNET_FILLER_MAX_ORDERS_PER_BUNDLE` (default unset). When the selected order count exceeds the cap, orders are chunked (profitability order preserved) and each chunk is submitted as its own fill bundle sequentially - submitting sequentially ensures the most profitable chunk acquires the lowest nonce, and alloy's `CachedNonceManager` then hands out consecutive nonces so multiple bundles can land across the target-block window in profitability order
- Permit2 allowances are cached by a background task (10-min refresh); balances are queried fresh each cycle
- Per-cycle `WorkingMap` tracks running balance/allowance budgets, decremented as orders are accepted (MAX allowances are not decremented). At build time it is also pre-decremented by tokens earmarked by in-flight bundles so other orders selected in the same cycle don't greenlight against funds already committed
- `InFlightTracker` records each successfully-submitted order with TTL equal to the Permit2 deadline offset (`block_lead_duration + target_blocks * slot_duration + DEADLINE_DRIFT_BUFFER_SECS`), so an entry persists for the full window during which the original bundle could still land. Subsequent cycles skip orders that are still in-flight, and the nonce-consumed check clears the tracker entry early when it observes a bundle has landed
- At startup, `FillerContext::initialize` queries the filler's balance for every `KnownToken` on both chains and logs one line per token; a summary warning is emitted if no known token has a non-zero balance
- Graceful shutdown via `CancellationToken` propagated through all async tasks
