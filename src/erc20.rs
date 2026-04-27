use crate::FillProviderType;
use alloy::{
    primitives::{Address, U256},
    providers::Provider,
    sol,
};
use signet_constants::NATIVE_TOKEN_ADDRESS;

sol! {
    /// Minimal ERC20 interface covering the read-only calls used across the filler:
    /// balance queries on startup and in the per-cycle budget check, and allowance queries for the
    /// Permit2 allowance cache.
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

/// Queries the on-chain balance of `account` for `token`. Native tokens use `eth_getBalance`;
/// ERC20 tokens use `balanceOf`.
pub(crate) async fn query_balance(
    provider: &FillProviderType,
    account: Address,
    token: Address,
) -> eyre::Result<U256> {
    if token == NATIVE_TOKEN_ADDRESS {
        Ok(provider.get_balance(account).await?)
    } else {
        Ok(IERC20::new(token, provider).balanceOf(account).call().await?)
    }
}
