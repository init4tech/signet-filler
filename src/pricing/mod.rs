mod radius_client;

pub use radius_client::RadiusPricingClient;

use alloy::primitives::U256;
use core::future::Future;
use signet_types::SignedOrder;

/// Breakdown of estimated costs and values for filling an order.
#[derive(Debug, Clone, Copy)]
pub struct FillCostEstimate {
    /// Estimated gas units required. Currently always zero — gas estimation
    /// is not yet implemented, so profitability checks ignore gas costs.
    pub estimated_gas: u64,
    /// Estimated gas cost in wei. Currently always zero — gas estimation
    /// is not yet implemented, so profitability checks ignore gas costs.
    pub estimated_gas_cost: U256,
    /// Sum of all input token amounts.
    pub total_input_value: U256,
    /// Sum of all output token amounts.
    pub total_output_value: U256,
}

/// Estimates fill costs and evaluates order profitability.
pub trait PricingClient: Send + Sync {
    /// Error type returned by pricing operations.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Estimate the cost of filling `order`.
    fn estimate_fill_cost(
        &self,
        order: &SignedOrder,
    ) -> impl Future<Output = Result<FillCostEstimate, Self::Error>> + Send;

    /// Return whether filling `order` is profitable.
    fn is_profitable(
        &self,
        order: &SignedOrder,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}
