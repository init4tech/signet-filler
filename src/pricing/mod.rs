mod static_client;

pub use static_client::StaticPricingClient;

use alloy::primitives::U256;
use core::future::Future;
use signet_types::SignedOrder;

#[derive(Debug, Clone)]
pub struct FillCostEstimate {
    pub estimated_gas: u64,
    pub estimated_gas_cost: U256,
    pub total_input_value: U256,
    pub total_output_value: U256,
}

pub trait PricingClient: Send + Sync {
    type Error: core::error::Error + Send + Sync + 'static;

    fn estimate_fill_cost(
        &self,
        order: &SignedOrder,
    ) -> impl Future<Output = Result<FillCostEstimate, Self::Error>> + Send;

    fn is_profitable(
        &self,
        order: &SignedOrder,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}
