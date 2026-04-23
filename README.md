# Signet Filler

A filler service for the Signet network that monitors pending orders and fills profitable ones.

The filler checks the transaction cache for pending orders shortly before each block boundary, scores their profitability against hardcoded exchange rates, and submits fill bundles for orders within the configured maximum loss threshold. Orders whose Permit2 deadline is earlier than the first target block's timestamp (minus a 5-second drift buffer for symmetry with the sign-side deadline offset) are dropped up front to avoid wasted RPC calls, and before checking nonces the remainder are filtered against the filler wallet's token balances and Permit2 allowances so that orders the filler cannot cover are discarded early. It connects to both the host chain and rollup RPC endpoints, using a configurable signer for transaction signing.

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
| `SIGNET_FILLER_BLOCK_LEAD_DURATION_MS` | How far before each block boundary to submit fill bundles, in milliseconds | `2000` |
| `SIGNET_FILLER_MAX_LOSS_PERCENT` | Maximum acceptable loss percent for order pricing (0-100) | `10` |
| `SIGNET_FILLER_TARGET_BLOCKS` | Number of consecutive blocks to target per fill bundle (1-10) | `5` |
| `SIGNET_FILLER_MAX_ORDERS_PER_BUNDLE` | Maximum orders per fill bundle. When set, orders in excess of the cap are split across additional bundles submitted sequentially in profitability order (must be > 0) | unset (no cap) |
| `SIGNET_FILLER_HEALTHCHECK_PORT` | Port for the healthcheck HTTP server | `8080` |
| `SIGNER_KEY` | AWS KMS key ID or local private key | N/A |
| `SIGNER_CHAIN_ID` | Chain ID for AWS signer [optional] | N/A |

## Limitations

### Fixed Pricing

The current implementation uses hardcoded USD exchange rates and decimal counts for a set of known tokens (USDC, USDT, WETH, WBTC, and the native/wrapped rollup token). Orders referencing any other token will be rejected with an `UnknownToken` error.

A future improvement could handle unknown tokens by querying the ERC-20 contract on-chain for `decimals()` and `totalSupply()`, then assuming total supply represents a fixed USD value (e.g. $10k) to derive a token price. This would allow the filler to process orders for arbitrary tokens rather than only the hardcoded set.

## Future Development

### Dynamic Block Lead Duration

The `SIGNET_FILLER_BLOCK_LEAD_DURATION_MS` setting is currently a static configuration value. This could be replaced with a dynamically calculated lead duration based on heuristics of historical inclusion rates — adjusting automatically to submit earlier when inclusion rates are low, or later when they are high, to balance freshness of order data against submission reliability.

## License
Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
