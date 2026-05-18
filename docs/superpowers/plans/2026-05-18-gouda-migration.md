# Gouda Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert the filler from running against the parmigiana rollup to running against the new gouda rollup (rollup chain id `792669`, running on the parmigiana host chain `3151908`).

**Architecture:** Mechanical migration. `KnownChains` already abstracts per-chain constants; the work is bumping the `signet-sdk` deps to a revision that knows about gouda, flipping six config/narrative strings, and updating one pricing-client `match` arm + the tests around it. No new modules, no new abstractions.

**Tech Stack:** Rust (edition 2024, MSRV 1.88), cargo, the `signet-sdk` crate family (`signet-constants`, `signet-orders`, `signet-tx-cache`, `signet-types`, dev: `signet-zenith`), alloy.

**Spec reference:** `docs/superpowers/specs/2026-05-18-gouda-migration-design.md`

**Worktree:** `/Users/swanpro/git/signet-filler/.claude/worktrees/gouda-migration` (branch `feat/gouda-migration`, already created and clean except for the spec/plan docs).

---

## Setup — confirm before Task 1

```bash
cd /Users/swanpro/git/signet-filler/.claude/worktrees/gouda-migration
git status                # should show clean tree (spec + plan already committed)
git branch --show-current # → feat/gouda-migration
```

If either is wrong, stop and reconcile before continuing. **Every code-editing command in this plan assumes the working directory is the worktree above.**

---

## File Structure

Files this plan creates or modifies, with their responsibility:

- **Modify** `Cargo.toml` — bump `signet-{constants,orders,tx-cache,types}` (and dev `signet-zenith`) to a git-pin on `signet-sdk@feat/gouda-known-chain` rev `ecce6a48a6af81c5f668c142b3a63452de02a146`; replace the `description` string.
- **Modify** `src/config.rs` — flip `DEFAULT_CHAIN_NAME`, `DEFAULT_HOST_RPC` (unchanged value, keep this one as-is — see Task 2), `DEFAULT_RU_RPC`; update env-var `desc` strings; update the `chain_name()` doc comment.
- **Modify** `src/fixed_pricing_client.rs` — replace the `"parmigiana"` arm of `wrapped_token_address` with a `"gouda"` arm; rename the `parmigiana_client` test helper to `gouda_client`; replace every `SignetSystemConstants::parmigiana()` with `::gouda()`.
- **Modify** `src/lib.rs` — change the crate-level doc comment from "Parmigiana testnet" to "Gouda testnet".
- **Modify** `README.md` — update the two affected rows of the env-var defaults table.
- **Modify** `CLAUDE.md` — change the opening one-line description to mention gouda.

Files **not** touched:
`src/chain_token_pair.rs`, `src/allowance.rs`, `src/erc20.rs`, `src/initialization.rs`, `src/metrics.rs`, `src/service.rs`, `src/filler_task/**`, `bin/filler.rs`, `.github/workflows/**`, `Dockerfile`. All of these already route through `SignetSystemConstants` / `SignetEnvironmentConstants` and need no edits.

---

## Task 1: Bump signet-sdk deps and verify clean build

**Files:**
- Modify: `Cargo.toml`
- (Auto) Modify: `Cargo.lock`

**Rationale:** This is the riskiest step. We pin to a revision of `signet-sdk` that bumps the workspace from 0.16.3 → 0.19.0 with breaking changes elsewhere in the SDK (`BundleKey`, typed headers). The filler doesn't appear to touch those surfaces, but the cargo build is the proof. Doing this first means we discover any incompatibility before sinking time into the rest of the migration.

- [ ] **Step 1.1: Read current `Cargo.toml`**

Read the file. Confirm the four signet-* deps are currently version-pinned and that the description is the parmigiana string. Current state (lines 1-30):

```toml
[package]
name = "signet-filler"
version = "0.1.0"
description = "Signet filler service for Parmigiana testnet"
# ...

[dependencies]
init4-bin-base = { version = "0.18.0-rc.13", features = ["aws"] }
signet-constants = "0.16.3"
signet-orders = "0.16.3"
signet-tx-cache = "0.16.3"
signet-types = "0.16.3"
# ...

[dev-dependencies]
signet-zenith = "0.16.3"
```

- [ ] **Step 1.2: Replace the description**

Edit `Cargo.toml` line 4:
```toml
description = "Signet filler service for Gouda testnet"
```

- [ ] **Step 1.3: Replace the four production signet-sdk deps**

Edit the four lines in `[dependencies]` to the git-pinned form. Add the TODO comment block above them so the next reader knows it is a temporary state:

```toml
# TODO(filler-gouda): revert to crates.io once signet-constants 0.19.x is published.
# Tracked via the deploy plan cascade in /tmp/2026-05-17-parmigiana-host-deploy-plan.md
# (signet-sdk #236 → bin-base #149 → storage #64 → node-components #144 → signet #105).
# Branch: signet-sdk@feat/gouda-known-chain.
signet-constants = { git = "https://github.com/init4tech/signet-sdk", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }
signet-orders    = { git = "https://github.com/init4tech/signet-sdk", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }
signet-tx-cache  = { git = "https://github.com/init4tech/signet-sdk", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }
signet-types     = { git = "https://github.com/init4tech/signet-sdk", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }
```

(The `branch = ...` key is informational only; cargo resolves by `rev`. Omitting it keeps the manifest one column narrower and avoids drift if the branch is renamed.)

- [ ] **Step 1.4: Replace the dev-dep signet-zenith**

Edit `[dev-dependencies]`:

```toml
signet-zenith = { git = "https://github.com/init4tech/signet-sdk", rev = "ecce6a48a6af81c5f668c142b3a63452de02a146" }
```

- [ ] **Step 1.5: Resolve and build**

Run:

```bash
cargo build
```

Expected: completes successfully with `Finished \`dev\` profile`. `Cargo.lock` is now updated.

If this fails with API errors, **stop and inspect** — the filler may be using a signet-sdk surface that changed between 0.16.3 and the gouda branch. Do not try to mass-patch call sites; surface the error to the requester. Likely-clean call sites (verified via grep at plan-write time): `Filler::new`, `FeePolicySubmitter::new`, `FillerOptions::new().with_deadline_offset(...)`, `TxCache::new_from_string`, `permit2::PERMIT2`, `permit2::is_order_nonce_consumed`, `stream::predicates::not_expired_at`, `OrderStreamExt`.

- [ ] **Step 1.6: Run the test suite (gates the commit)**

```bash
cargo test
```

Expected: all tests pass. The `fixed_pricing_client` tests still reference `parmigiana` symbols — those still exist in the 0.19 constants crate, so the tests stay green at this point. They get migrated to gouda in Task 4.

- [ ] **Step 1.7: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
chore(deps): pin signet-sdk to gouda branch

Temporary git-pin to signet-sdk@feat/gouda-known-chain
(rev ecce6a48a6af81c5f668c142b3a63452de02a146) so the filler can resolve
KnownChains::Gouda. Reverts to crates.io 0.19.x once the upstream
publish cascade lands.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

Expected: clean commit on `feat/gouda-migration`.

---

## Task 2: Flip config defaults

**Files:**
- Modify: `src/config.rs` (the three `DEFAULT_*` consts, three `#[from_env(desc = ...)]` strings, one doc comment)

- [ ] **Step 2.1: Update the three default constants**

In `src/config.rs`, the current defaults are at lines 19-21:

```rust
const DEFAULT_CHAIN_NAME: &str = "parmigiana";
const DEFAULT_HOST_RPC: &str = "https://host-rpc.parmigiana.signet.sh";
const DEFAULT_RU_RPC: &str = "wss://rpc.parmigiana.signet.sh";
```

Replace them with:

```rust
const DEFAULT_CHAIN_NAME: &str = "gouda";
const DEFAULT_HOST_RPC: &str = "https://host-rpc.parmigiana.signet.sh";
const DEFAULT_RU_RPC: &str = "wss://rpc.gouda.signet.sh";
```

**Note:** `DEFAULT_HOST_RPC` value is intentionally unchanged — gouda runs on the parmigiana host chain (`HOST_CHAIN_ID = 3151908`, see `signet_constants::gouda::HOST_CHAIN_ID`), so the host RPC endpoint is still parmigiana's. Only the rollup RPC moves to gouda.

- [ ] **Step 2.2: Update the three env-var `desc` strings**

Three `desc` strings need refreshing. Locate each by its `var = ...` line.

Current (in the `chain_name` field, around line 35):
```rust
desc = "Signet chain name [default: parmigiana]",
```
Replace with:
```rust
desc = "Signet chain name [default: gouda]",
```

Current (in `host_rpc`, around line 42):
```rust
desc = "URL for Host RPC node. This MUST be a valid HTTP or WS URL, starting with http://, \
    https://, ws:// or wss:// [default: https://host-rpc.parmigiana.signet.sh]",
```
**Leave this one as-is.** The host RPC default did not change.

Current (in `ru_rpc`, around line 50):
```rust
desc = "URL for Rollup RPC node. This MUST be a valid WS url starting with ws:// or \
    wss://. Http providers are not supported [default: wss://rpc.parmigiana.signet.sh]",
```
Replace with:
```rust
desc = "URL for Rollup RPC node. This MUST be a valid WS url starting with ws:// or \
    wss://. Http providers are not supported [default: wss://rpc.gouda.signet.sh]",
```

- [ ] **Step 2.3: Update the `chain_name()` doc comment**

Current (around line 115):
```rust
    /// The Signet chain name (e.g. "parmigiana").
    pub const fn chain_name(&self) -> &str {
```
Replace with:
```rust
    /// The Signet chain name (e.g. "gouda").
    pub const fn chain_name(&self) -> &str {
```

- [ ] **Step 2.4: Build and test**

```bash
cargo build
cargo test
```

Expected: both green. No assertions inspect default-value strings yet (that's Task 3).

- [ ] **Step 2.5: Smoke-test `--help`**

```bash
cargo run -- --help 2>&1 | grep -E 'CHAIN_NAME|RPC_URL'
```

Expected: three lines, the chain-name line ends with `[default: gouda]`, the host-RPC line still shows parmigiana, the rollup-RPC line ends with `[default: wss://rpc.gouda.signet.sh]`.

- [ ] **Step 2.6: Commit**

```bash
git add src/config.rs
git commit -m "$(cat <<'EOF'
feat(config): default to gouda

Flip the chain-name default to "gouda" and the rollup RPC default to
wss://rpc.gouda.signet.sh. Host RPC default stays on
host-rpc.parmigiana.signet.sh because gouda shares the parmigiana host
chain.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Lock in the default-chain regression with a test

**Files:**
- Modify: `src/config.rs` (add a `#[cfg(test)] mod tests` block)

**Rationale:** A one-line assertion that catches the two ways the dep bump and default flip can silently break: gouda disappearing from `KnownChains`, or `DEFAULT_CHAIN_NAME` regressing. Both would surface here before binary boot.

- [ ] **Step 3.1: Append a test module to `src/config.rs`**

Add at the end of the file (after the closing `}` of `pub fn config_from_env`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The default chain name must parse as a known Signet chain. Catches both a missing
    /// `KnownChains` variant (e.g. unintentional dep downgrade) and a typo in
    /// `DEFAULT_CHAIN_NAME`.
    #[test]
    fn default_chain_name_parses() {
        let constants: SignetConstants = DEFAULT_CHAIN_NAME
            .parse()
            .expect("DEFAULT_CHAIN_NAME must parse as a known Signet chain");
        assert_eq!(constants.system().ru_chain_id(), 792669, "gouda rollup chain id");
        assert_eq!(constants.system().host_chain_id(), 3151908, "parmigiana host chain id");
    }
}
```

- [ ] **Step 3.2: Run the new test**

```bash
cargo test --lib config::tests::default_chain_name_parses -- --nocapture
```

Expected: 1 passed.

- [ ] **Step 3.3: Run the full suite to confirm no regressions**

```bash
cargo test
```

Expected: all pass.

- [ ] **Step 3.4: Commit**

```bash
git add src/config.rs
git commit -m "$(cat <<'EOF'
test(config): assert default chain name resolves to gouda

Regression test: catches dep downgrades that remove gouda from
KnownChains, or future typos in DEFAULT_CHAIN_NAME.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Migrate the pricing client to gouda

**Files:**
- Modify: `src/fixed_pricing_client.rs` (one production `match` arm; one test helper rename; nine call sites; eight `::parmigiana()` references in tests)

- [ ] **Step 4.1: Update the production `wrapped_token_address` match**

Current (lines 41-47):
```rust
    fn wrapped_token_address(chain_name: &str) -> Option<Address> {
        match chain_name {
            "parmigiana" => Some(signet_constants::parmigiana::WRAPPED),
            "mainnet" => Some(signet_constants::mainnet::WRAPPED),
            _ => None,
        }
    }
```

Replace with:
```rust
    fn wrapped_token_address(chain_name: &str) -> Option<Address> {
        match chain_name {
            "gouda" => Some(signet_constants::gouda::WRAPPED),
            "mainnet" => Some(signet_constants::mainnet::WRAPPED),
            _ => None,
        }
    }
```

- [ ] **Step 4.2: Build to confirm `signet_constants::gouda::WRAPPED` resolves**

```bash
cargo build
```

Expected: success. If `error[E0432]: unresolved import \`signet_constants::gouda\`` appears, the dep pin from Task 1 didn't land — go back and verify Task 1's commit.

(Tests will now be broken — they still call `parmigiana_client(...)` and `SignetSystemConstants::parmigiana()`. We fix them in the next steps and commit Task 4 as one atomic change.)

- [ ] **Step 4.3: Rename the test helper**

In `src/fixed_pricing_client.rs`, locate the test helper around line 179:
```rust
    fn parmigiana_client(max_loss_percent: u8) -> FixedPricingClient {
        FixedPricingClient::new(
            &SignetSystemConstants::parmigiana(),
            "parmigiana",
            max_loss_percent,
        )
    }
```

Replace with:
```rust
    fn gouda_client(max_loss_percent: u8) -> FixedPricingClient {
        FixedPricingClient::new(
            &SignetSystemConstants::gouda(),
            "gouda",
            max_loss_percent,
        )
    }
```

- [ ] **Step 4.4: Rename every call site of the helper**

Within `src/fixed_pricing_client.rs`, replace every occurrence of `parmigiana_client(` with `gouda_client(`. Use Edit `replace_all`:

```
old: parmigiana_client(
new: gouda_client(
```

Expected count: 10 call sites (one definition already renamed in Step 4.3 plus nine usages — the editor's `replace_all` should report 10 replacements if Step 4.3's rename is included in the same Edit call, or 9 if Step 4.3 was applied first; either is fine).

After this step, no `parmigiana_client` symbol remains in the file.

- [ ] **Step 4.5: Update every `SignetSystemConstants::parmigiana()` to `::gouda()`**

Within `src/fixed_pricing_client.rs`, the helper plus seven tests call `SignetSystemConstants::parmigiana()`. Use Edit `replace_all`:

```
old: SignetSystemConstants::parmigiana()
new: SignetSystemConstants::gouda()
```

Expected count: 8 replacements (one in the helper from Step 4.3, plus seven test bodies that look up token addresses such as `host().tokens().usdc()`).

After this step, no `parmigiana` substring remains in `src/fixed_pricing_client.rs`.

- [ ] **Step 4.6: Verify the file is clean of parmigiana**

```bash
grep -n parmigiana src/fixed_pricing_client.rs
```

Expected: no output.

- [ ] **Step 4.7: Run the pricing-client tests**

```bash
cargo test --lib fixed_pricing_client
```

Expected: every test in `fixed_pricing_client::tests` passes. There should be no semantic drift — the token prices and decimals exposed by `SignetSystemConstants::gouda()` for WBTC/WETH/USDC/USDT match parmigiana's economics (same fixed-price USD anchoring used by `FixedPricingClient::new`), so all margin/threshold assertions hold unchanged.

- [ ] **Step 4.8: Run the full test suite**

```bash
cargo test
```

Expected: all pass.

- [ ] **Step 4.9: Commit**

```bash
git add src/fixed_pricing_client.rs
git commit -m "$(cat <<'EOF'
feat(pricing): switch to gouda chain constants

Update wrapped_token_address to recognize "gouda" instead of
"parmigiana"; mainnet arm unchanged. Tests now exercise
SignetSystemConstants::gouda(); token prices and decimals are
USD-anchored so all numeric assertions hold.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Update narrative strings

**Files:**
- Modify: `src/lib.rs` (one doc comment)
- Modify: `README.md` (two table rows)
- Modify: `CLAUDE.md` (one opening line)

- [ ] **Step 5.1: Update `src/lib.rs` crate doc**

Current (line 1):
```rust
//! Signet filler service for Parmigiana testnet
```
Replace with:
```rust
//! Signet filler service for Gouda testnet
```

- [ ] **Step 5.2: Update `README.md` env-var table**

Two cells of the env-var defaults table change. Lines 19-21 of `README.md` currently read:

```markdown
| `SIGNET_FILLER_CHAIN_NAME` | Signet chain name | `parmigiana` |
| `SIGNET_FILLER_HOST_RPC_URL` | URL for Host RPC node (http/https/ws/wss) | `https://host-rpc.parmigiana.signet.sh` |
| `SIGNET_FILLER_ROLLUP_RPC_URL` | URL for Rollup RPC node (ws/wss only) | `wss://rpc.parmigiana.signet.sh` |
```

Replace with:
```markdown
| `SIGNET_FILLER_CHAIN_NAME` | Signet chain name | `gouda` |
| `SIGNET_FILLER_HOST_RPC_URL` | URL for Host RPC node (http/https/ws/wss) | `https://host-rpc.parmigiana.signet.sh` |
| `SIGNET_FILLER_ROLLUP_RPC_URL` | URL for Rollup RPC node (ws/wss only) | `wss://rpc.gouda.signet.sh` |
```

(Host-RPC row keeps its parmigiana value; only the chain-name and rollup-RPC rows change.)

- [ ] **Step 5.3: Update `CLAUDE.md` opening line**

Current (line 3, the project description below the `# Signet Filler` heading):
```markdown
Order filler service for the Signet Parmigiana testnet. Monitors a transaction cache for pending orders, checks they are within an acceptable loss threshold using hardcoded exchange rates, and submits fill bundles targeting configurable consecutive blocks shortly before each block boundary.
```

Replace with:
```markdown
Order filler service for the Signet Gouda testnet. Monitors a transaction cache for pending orders, checks they are within an acceptable loss threshold using hardcoded exchange rates, and submits fill bundles targeting configurable consecutive blocks shortly before each block boundary.
```

- [ ] **Step 5.4: Build (doc comments don't have a separate check, but rustdoc warnings show in `cargo build`)**

```bash
cargo build
```

Expected: success.

- [ ] **Step 5.5: Commit**

```bash
git add src/lib.rs README.md CLAUDE.md
git commit -m "$(cat <<'EOF'
docs: rename narrative from parmigiana to gouda

Update crate doc, README defaults table, and CLAUDE.md opener so the
project description matches the rollup it now targets.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Final validation sweep

**Files:** none.

- [ ] **Step 6.1: No stray parmigiana / pecorino references**

```bash
grep -rnE 'parmigiana|pecorino' src/ Cargo.toml README.md CLAUDE.md
```

Expected: exactly **three** matches, all referencing the host-RPC URL (which legitimately stays on the parmigiana host):
- `src/config.rs` — the `DEFAULT_HOST_RPC` const value (`https://host-rpc.parmigiana.signet.sh`)
- `src/config.rs` — the `host_rpc` field's `#[from_env(desc = ...)]` string, which embeds the same default URL
- `README.md` — the host-RPC row of the env-var table

Zero matches for `pecorino`. If any **other** parmigiana reference appears, audit which task missed it and amend before opening the PR.

- [ ] **Step 6.2: Formatting**

```bash
cargo +nightly fmt --check
```

Expected: no diff. If diff appears, run `cargo +nightly fmt` and amend the previous commit (or commit a follow-up "chore: fmt" — whichever fits the prior task's scope).

- [ ] **Step 6.3: Clippy**

```bash
cargo clippy --all-targets -- -D warnings
```

Expected: clean exit.

- [ ] **Step 6.4: Test suite**

```bash
cargo test
```

Expected: all pass, including the new `default_chain_name_parses` test from Task 3 and the renamed `fixed_pricing_client::tests::*` tests from Task 4.

- [ ] **Step 6.5: `--help` smoke**

```bash
cargo run -- --help 2>&1 | grep -E 'CHAIN_NAME|RPC_URL'
```

Expected:
- CHAIN_NAME row: ends with `[default: gouda]`
- Host RPC row: ends with `[default: https://host-rpc.parmigiana.signet.sh]`
- Rollup RPC row: ends with `[default: wss://rpc.gouda.signet.sh]`

- [ ] **Step 6.6: Confirm history**

```bash
git log --oneline main..HEAD
```

Expected order (newest last): plan commit, then Task 1 deps, Task 2 config, Task 3 test, Task 4 pricing, Task 5 docs. Five new commits on `feat/gouda-migration` beyond the spec/plan commits that already exist.

- [ ] **Step 6.7: PR readiness checklist (do not open PR yet — that's a separate, user-driven step)**

Confirm before handing back:
- All six tasks ticked
- `cargo +nightly fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` all clean
- `Cargo.lock` committed (Step 1.7)
- The spec at `docs/superpowers/specs/2026-05-18-gouda-migration-design.md` matches what was built; no surprises in scope
- Grep sweep at Step 6.1 shows only the expected host-RPC residual

---

## Self-Review notes

(Inline so reviewers can audit.)

**Spec coverage:**
- Cargo dep bump + description → Task 1
- `src/config.rs` defaults + help text + doc comment → Task 2
- Regression test for default chain → Task 3 (added cheap to support the spec's "is cheap" hint)
- `src/fixed_pricing_client.rs` match arm + tests → Task 4
- `src/lib.rs` + `README.md` + `CLAUDE.md` narrative → Task 5
- Validation (fmt, clippy, test, grep, `--help`) → Task 6
- Dep strategy stop-gap with TODO → Step 1.3 inline comment
- Risks 1-4 from spec → mitigated by either Task 1.5/1.6 (compile-fail surfaces immediately) or scope exclusion noted in spec

**Placeholders:** none. Every code/string change is concrete.

**Type consistency:**
- `signet_constants::gouda::WRAPPED` — referenced in Task 4 Step 4.1; verified to exist as a `pub const WRAPPED: Address` in the gouda branch's `chains/gouda.rs`.
- `SignetSystemConstants::gouda()` — referenced in Task 3 Step 3.1 implicitly via `SignetConstants::from_str("gouda")`, and in Task 4 Steps 4.3/4.5 directly; the `gouda()` associated function is added on the gouda branch alongside the existing `parmigiana()`/`mainnet()`/`pecorino()` pattern.
- `constants.system().ru_chain_id()` and `host_chain_id()` — used in Task 3's test; existing API on `SignetSystemConstants`, unchanged between 0.16.3 and 0.19.

**Out-of-scope follow-ups (not in this plan):**
- Reverting the git-pin to `0.19.x` crates.io versions once the upstream cascade publishes — tracked by the `TODO(filler-gouda)` comment in Cargo.toml.
- PR creation and CI verification — handled by the user when ready (or via the `pr` skill if invoked separately).
- Live deploy of the new image to the gouda pod — node-ops, per the deploy plan.
