# Signet Filler

A filler service for the Signet network that monitors pending orders and fills profitable ones.

The filler checks the transaction cache for pending orders shortly before each block boundary, evaluates their profitability using the Radius solver API, and submits fill bundles for profitable orders. It connects to both the host chain and rollup RPC endpoints, using a configurable signer for transaction signing.

## Configuration

Configuration is via environment variables. Run with `-h` or `--help` to see the full list:

```
signet-filler --help
```

### Environment Variables

| Variable | Description | Default |
|----------|-------------|--|
| `SIGNET_FILLER_CHAIN_NAME` | Signet chain name | `parmigiana` |
| `SIGNET_FILLER_HOST_RPC_URL` | URL for Host RPC node (http/https/ws/wss) | `https://host-rpc.parmigiana.signet.sh` |
| `SIGNET_FILLER_ROLLUP_RPC_URL` | URL for Rollup RPC node (ws/wss only) | `wss://rpc.parmigiana.signet.sh` |
| `SIGNET_FILLER_RADIUS_URL` | Base URL for the Radius solver API | `https://graham-findarticles-rugs-its.trycloudflare.com` |
| `SIGNET_FILLER_RADIUS_BEARER_TOKEN` | Bearer token for Radius solver API | N/A |
| `SIGNET_FILLER_BLOCK_LEAD_DURATION_MS` | How far before each block boundary to submit fill bundles, in milliseconds | `2000` |
| `SIGNET_FILLER_HEALTHCHECK_PORT` | Port for the healthcheck HTTP server | `8080` |
| `SIGNER_KEY` | AWS KMS key ID or local private key | N/A |
| `SIGNER_CHAIN_ID` | Chain ID for AWS signer [optional] | N/A |

## Limitations

### Order Shape Restrictions

Inputs and outputs are aggregated before pricing: inputs are grouped by token, outputs by (token, chain ID). Multiple raw entries that share a group are summed. The pricing engine then supports three group shapes:

- **Single input group, single output group** — one Radius quote.
- **Multiple input groups, single output group** — one quote per input group, summed.
- **Single input group, multiple output groups** — uses two-round quoting: first at full input to get per-group rates, then re-quoted at proportional split amounts for accuracy. Profitability is checked per-output group with costs expressed in input-token terms.

Orders with **multiple input groups and multiple output groups** are rejected, as allocating multiple input tokens across multiple output tokens requires an optimization strategy that is not yet designed.

A maximum of 10 output groups per order is enforced.

### Gas Costs Not Included

Profitability checks currently ignore gas costs. The filler pays gas to submit fill transactions on-chain, but `estimated_gas` and `estimated_gas_cost` in `FillCostEstimate` are always zero. This means marginally profitable orders may be accepted that are actually unprofitable after gas. Implementing gas estimation would require gas price feeds and fill transaction simulation.

### Single-Block Bundle Submission

Fill bundles currently target only the next block (N+1). If the bundle is not included in that block, it expires and the filler must wait for the next polling cycle to retry with fresh signatures.

A more reliable approach would be to submit bundles targeting multiple consecutive blocks (N+1, N+2, N+3) simultaneously, reusing the same signed fills. This would improve inclusion rates without additional signing overhead.

## Future Development

### Dynamic Block Lead Duration

The `SIGNET_FILLER_BLOCK_LEAD_DURATION_MS` setting is currently a static configuration value. This could be replaced with a dynamically calculated lead duration based on heuristics of historical inclusion rates — adjusting automatically to submit earlier when inclusion rates are low, or later when they are high, to balance freshness of order data against submission reliability.

## License
Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
