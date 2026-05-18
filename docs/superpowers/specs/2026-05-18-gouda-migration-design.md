# Gouda Migration — Design

**Status:** Draft for review
**Date:** 2026-05-18
**Worktree / branch:** `.claude/worktrees/gouda-migration` / `feat/gouda-migration`

## Goal

Convert the filler from running against the **parmigiana** rollup to running against the new **gouda** rollup. Gouda is a fresh rollup chain (`792669` / `0xc185d`) that shares the **parmigiana host chain** (`3151908`). Deployment context, including the on-chain addresses and the upstream PR cascade, is captured in `/tmp/2026-05-17-parmigiana-host-deploy-plan.md`.

After this change the filler defaults to gouda, has no remaining parmigiana-specific code paths, and runs in node-ops's new gouda pods without any further filler-side configuration beyond per-environment env vars.

## Non-goals

- Maintaining a dual gouda + parmigiana code path inside the filler. The old parmigiana rollup (chain id `88888`) is parked, not in active use. Anyone needing to point a build at it can still do so by overriding the chain-name and RPC env vars, but it is not a supported default and gets no dedicated test coverage.
- Changes outside the filler repo. Genesis files, host contracts, sequencer config, and the upstream `signet-constants` cascade are handled in the deploy plan.
- Refactoring the chain-name plumbing into something more first-class. The `chain_name: String` config + `SignetConstants::from_str` parse is fine as-is — gouda just slots into the existing `KnownChains` enum.

## Background — what the filler currently has hard-coded

A grep across the repo (`grep -E 'parmigiana|gouda|pecorino|mainnet'`) shows the parmigiana coupling is small and localised:

| Location | Coupling | Required change |
|---|---|---|
| `Cargo.toml` (`description`, dep versions) | `0.16.3` signet crates; description says "Parmigiana testnet" | Bump deps to gouda-aware revision (see Dependency strategy); update description |
| `src/config.rs` `DEFAULT_CHAIN_NAME` / `DEFAULT_HOST_RPC` / `DEFAULT_RU_RPC` and the env-var help text | Defaults all point at parmigiana | Flip chain-name default to `gouda`, rollup RPC to `wss://rpc.gouda.signet.sh`, keep host RPC on `https://host-rpc.parmigiana.signet.sh` (gouda *is* on the parmigiana host) |
| `src/fixed_pricing_client.rs::wrapped_token_address` | Hard `match` on `"parmigiana"` / `"mainnet"` to pick the `WRAPPED` constant | Replace `"parmigiana"` arm with `"gouda"` arm using `signet_constants::gouda::WRAPPED`; keep `"mainnet"` arm |
| `src/fixed_pricing_client.rs` tests | All exercise `SignetSystemConstants::parmigiana()` via a `parmigiana_client(max_loss)` helper | Rename helper to `gouda_client`, switch constants to `SignetSystemConstants::gouda()`; behaviour-equivalent (token prices are USD-anchored) |
| `src/lib.rs` doc comment (`//! Signet filler service for Parmigiana testnet`) | Narrative only | Change to "Gouda testnet" |
| `README.md` | Narrative + the defaults column of the env-var table | Update to gouda defaults |
| `CLAUDE.md` (top description line) | Narrative only | Update to gouda |

Everything else — `src/chain_token_pair.rs`, `src/allowance.rs`, `src/initialization.rs`, `src/filler_task/**`, `src/erc20.rs`, `src/metrics.rs`, `src/service.rs`, the binary entry point — already reads addresses, chain ids, and the tx-cache URL through `config.constants()` (i.e. `SignetSystemConstants` / `SignetEnvironmentConstants`). Once the constants resolve to gouda there is nothing more to change.

The CD workflow (`.github/workflows/filler-ecr-cd.yml`) builds a single image — chain selection is purely runtime via env vars — so it needs no changes. Node-ops's gouda pod will just inject a different env block (`SIGNET_FILLER_CHAIN_NAME=gouda`, gouda rollup RPC URL, gouda signer KMS key).

## Dependency strategy

`signet-constants` 0.18.0 (current `max_version` on crates.io) does **not** know about gouda. Gouda support exists on `signet-sdk` branch `feat/gouda-known-chain`, which bumps the workspace to `0.19.0` but is **not yet published**. The deploy plan tracks the full publish cascade (signet-sdk #236 → bin-base / storage / node-components / signet) as still partly in draft.

To unblock the filler immediately:

1. Switch `signet-constants`, `signet-orders`, `signet-tx-cache`, and `signet-types` in `Cargo.toml` from version pins (`"0.16.3"`) to git-revision pins on `signet-sdk@feat/gouda-known-chain`, using a single `rev` for reproducibility. Concrete SHA as of spec-write time: **`ecce6a48a6af81c5f668c142b3a63452de02a146`** (tip of `feat/gouda-known-chain`; "fix(constants): use parmigiana host genesis timestamp for gouda's HOST_START_TIMESTAMP"). If the branch advances before the PR opens, refresh to the new tip and re-record in the PR description.
2. Add a `TODO(filler-gouda)` comment block in `Cargo.toml` that names the follow-up: revert to crates.io version pins (`"0.19"` or whatever the cascade lands at) once `signet-constants 0.19.x` is published. This is a one-line revert, not a refactor.

This is an explicit, documented stop-gap — the alternative (waiting for the cascade) blocks the filler indefinitely on work outside this repo's control.

## Design

### Architecture: unchanged

The filler's runtime structure stays exactly as it is. There is no new module, no new abstraction, no renamed type. The change is configuration values and a single `match` arm. The reason the migration is small is that all the chain-specific knobs already live behind one boundary: the `SignetConstants` produced by `chain_name.parse()`. We're just feeding it a different string.

### File-by-file changes

**`Cargo.toml`**
```toml
description = "Signet filler service for Gouda testnet"
# ... below ...
# TODO(filler-gouda): revert to crates.io once signet-constants 0.19.x is published
# (tracked in the deploy plan cascade).
signet-constants = { git = "https://github.com/init4tech/signet-sdk", branch = "feat/gouda-known-chain", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }
signet-orders    = { git = "https://github.com/init4tech/signet-sdk", branch = "feat/gouda-known-chain", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }
signet-tx-cache  = { git = "https://github.com/init4tech/signet-sdk", branch = "feat/gouda-known-chain", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }
signet-types     = { git = "https://github.com/init4tech/signet-sdk", branch = "feat/gouda-known-chain", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }

# dev-dependencies
signet-zenith    = { git = "https://github.com/init4tech/signet-sdk", branch = "feat/gouda-known-chain", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }
```
The `branch` is informational; `rev` is what cargo resolves against. If the branch advances before PR open, refresh the SHA in lockstep across all four (five with dev) entries.

**`src/config.rs`**
- `DEFAULT_CHAIN_NAME: &str = "gouda"`
- `DEFAULT_RU_RPC: &str = "wss://rpc.gouda.signet.sh"`
- `DEFAULT_HOST_RPC: &str` unchanged (`https://host-rpc.parmigiana.signet.sh`) — gouda runs on the parmigiana host chain.
- Env-var `desc` strings updated: `[default: parmigiana]` → `[default: gouda]`, parmigiana hostnames in defaults → gouda hostname for the rollup RPC default only.
- `chain_name()` doc comment: `e.g. "parmigiana"` → `e.g. "gouda"`.

**`src/fixed_pricing_client.rs`**
- `wrapped_token_address`: replace the `"parmigiana"` arm with a `"gouda"` arm pointing at `signet_constants::gouda::WRAPPED`. Keep the `"mainnet"` arm. Anything unknown still returns `None` and tokens beyond the on-chain ones are rejected by the existing `UnknownToken` path.
- Tests:
  - `fn parmigiana_client(...)` → `fn gouda_client(...)`, with `&SignetSystemConstants::gouda(), "gouda", …` inside.
  - Each test-site call updated.
  - Each `SignetSystemConstants::parmigiana()` inside individual tests → `::gouda()`.
  - No assertion needs to change: all the numeric checks (margin signs, threshold boundaries, overflow, cross-token normalization) depend only on the USD price table and decimals, both of which are identical for gouda's WBTC/WETH/USDC/USDT.

**`src/lib.rs`**
- Module doc: `//! Signet filler service for Gouda testnet`.

**`README.md`**
- Defaults column for `SIGNET_FILLER_CHAIN_NAME` → `gouda`.
- Defaults column for `SIGNET_FILLER_ROLLUP_RPC_URL` → `wss://rpc.gouda.signet.sh`.
- Host RPC default unchanged.

**`CLAUDE.md`**
- Opening line: "Order filler service for the Signet Gouda testnet."
- No structural changes to the file; the project layout section is still accurate.

### Validation strategy

1. **Compile / lint / test locally**:
   - `cargo +nightly fmt --check`
   - `cargo clippy --all-targets -- -D warnings`
   - `cargo test` (the renamed `fixed_pricing_client` tests must all still pass with no semantic changes)
2. **Help-text smoke**: `cargo run -- --help` shows `[default: gouda]` and the gouda rollup RPC, confirming the env-var help survived the rename.
3. **No-env smoke**: with `SIGNER_KEY` unset (or set), confirm `config_from_env` produces a `Config` whose `chain_name() == "gouda"` and whose `constants().system().ru_chain_id() == 792669`. A small `assert!`-level test inside `config.rs` is enough; we already exercise `Config` indirectly through binary boot, so a dedicated test is not strictly necessary but is cheap.
4. **Live**: out of scope for this PR — node-ops drives the actual pod deploy per the deploy plan. The filler PR's "merge readiness" checklist will explicitly defer the production smoke to that step.

### What we are explicitly NOT changing

- The `chain_name` field name. It is still semantically "the Signet network name". `KnownChains` from upstream is the source of truth.
- `chain_token_pair.rs`, `allowance.rs`, etc. — they don't care what chain we're on.
- The CD workflow. Image tagging is unchanged; node-ops will pull whatever ECR tag the merge produces.
- Mainnet support in `wrapped_token_address`. It costs nothing to keep, and removing it would surprise anyone who already uses it.

## Risks & mitigations

1. **Git-pinned deps reach `crates.io` mid-PR-review.** Mitigation: leave the TODO comment; the swap-back PR is a one-line edit. No code depends on the dep being a `git` pin specifically.
2. **`signet_constants::gouda` module name differs from spec.** Mitigation: this design assumes the module path from the `feat/gouda-known-chain` branch (`signet_constants::gouda::WRAPPED`). If the cascade renames it before publish, the only patch site is one line in `fixed_pricing_client.rs::wrapped_token_address`.
3. **Tx-cache URL for gouda is wrong.** Mitigation: the URL comes from `SignetEnvironmentConstants` (set by gouda's `chains/gouda.rs` to `https://transactions.gouda.signet.sh`). Not configured in the filler. If the URL changes upstream we get it automatically.
4. **Old parmigiana pod is still running and confuses operators.** Out of scope for the filler PR; flagged for the deploy plan / node-ops.

## Open questions

None. Defaults are stated above; the user-confirmed approach is "drop parmigiana entirely, gouda is the new default."

## Acceptance criteria

- `grep -E 'parmigiana|pecorino' src/ Cargo.toml README.md CLAUDE.md` returns nothing (only `mainnet` and `gouda` references survive in `wrapped_token_address`).
- `cargo build && cargo test && cargo clippy --all-targets -- -D warnings && cargo +nightly fmt --check` all clean.
- `cargo run -- --help` shows gouda defaults.
- PR description includes the pinned `signet-sdk` commit SHA and links the follow-up "revert to crates.io 0.19" task.
