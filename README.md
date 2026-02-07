# Signet Filler

A filler service for the Signet network that monitors pending orders and fills profitable ones.

The filler checks the transaction cache for pending orders shortly before each block boundary, evaluates their profitability, and submits fill bundles for orders that meet the configured profit threshold. It connects to both the host chain and rollup RPC endpoints, using a configurable signer for transaction signing.

## Configuration

Configuration is via environment variables. Run with `-h` or `--help` to see the full list:

```
signet-filler --help
```

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `SIGNET_FILLER_CHAIN_NAME` | Signet chain name | `parmigiana` |
| `SIGNET_FILLER_HOST_RPC_URL` | URL for Host RPC node (http/https/ws/wss) | `https://host-rpc.parmigiana.signet.sh` |
| `SIGNET_FILLER_ROLLUP_RPC_URL` | URL for Rollup RPC node (ws/wss only) | `wss://rpc.parmigiana.signet.sh` |
| `SIGNET_FILLER_BLOCK_LEAD_DURATION_MS` | How far before each block boundary to submit fill bundles, in milliseconds | `500` |
| `SIGNET_FILLER_MIN_PROFIT_THRESHOLD_WEI` | Minimum profit threshold in wei | `100` |
| `SIGNET_FILLER_GAS_ESTIMATE_PER_ORDER` | Estimated gas per order fill | `150000` |
| `SIGNET_FILLER_GAS_PRICE_GWEI` | Assumed gas price in gwei | `1` |
| `SIGNER_KEY` | AWS KMS key ID or local private key | N/A |
| `SIGNER_CHAIN_ID` | Chain ID for AWS signer [optional] | N/A |

## Limitations

### Static Pricing

The current implementation uses a static pricing model that sums raw token amounts without price conversion. This means:

- Profitability calculations assume all input and output tokens have equivalent value per unit
- No price oracle integration or token price lookups
- No decimal normalization between different tokens

This approach works for **single-token orders** (e.g., same-asset transfers between rollup and host), but will produce incorrect profitability assessments for multi-token swaps.

Dynamic pricing with real token valuations will be added once integration with Radius' pricing API is available.

### Single-Block Bundle Submission

Fill bundles currently target only the next block (N+1). If the bundle is not included in that block, it expires and the filler must wait for the next polling cycle to retry with fresh signatures.

A more reliable approach would be to submit bundles targeting multiple consecutive blocks (N+1, N+2, N+3) simultaneously, reusing the same signed fills. This would improve inclusion rates without additional signing overhead.

## Future Development

### Dynamic Block Lead Duration

The `SIGNET_FILLER_BLOCK_LEAD_DURATION_MS` setting is currently a static configuration value. This could be replaced with a dynamically calculated lead duration based on heuristics of historical inclusion rates — adjusting automatically to submit earlier when inclusion rates are low, or later when they are high, to balance freshness of order data against submission reliability.

### Duplicate Fill Avoidance

When a fill bundle is submitted but hasn't landed on-chain yet, the order may still appear in the transaction cache on the next block cycle, resulting in a duplicate fill submission that will revert. This could be avoided by checking on-chain state after each block to distinguish "fill landed, don't retry" from "fill wasn't included, retry next block."

## License
Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
