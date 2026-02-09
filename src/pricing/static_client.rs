use super::{FillCostEstimate, PricingClient};
use alloy::primitives::U256;
use init4_bin_base::deps::tracing::{instrument, trace};
use signet_types::SignedOrder;

#[derive(Debug, Clone)]
pub struct StaticPricingClient {
    min_profit_threshold: U256,
    gas_estimate_per_order: u64,
    gas_price_gwei: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum StaticPricingError {
    #[error("order has no inputs")]
    NoInputs,
    #[error("order has no outputs")]
    NoOutputs,
}

impl StaticPricingClient {
    pub const fn new(
        min_profit_threshold: U256,
        gas_estimate_per_order: u64,
        gas_price_gwei: u64,
    ) -> Self {
        Self { min_profit_threshold, gas_estimate_per_order, gas_price_gwei }
    }

    fn calculate_total_input_value(&self, order: &SignedOrder) -> U256 {
        order.permit().permit.permitted.iter().fold(U256::ZERO, |acc, perm| acc + perm.amount)
    }

    fn calculate_total_output_value(&self, order: &SignedOrder) -> U256 {
        order.outputs().iter().fold(U256::ZERO, |acc, output| acc + output.amount)
    }
}

impl PricingClient for StaticPricingClient {
    type Error = StaticPricingError;

    #[instrument(skip(self, order), fields(order_hash = %order.order_hash()))]
    async fn estimate_fill_cost(
        &self,
        order: &SignedOrder,
    ) -> Result<FillCostEstimate, Self::Error> {
        if order.permit().permit.permitted.is_empty() {
            return Err(StaticPricingError::NoInputs);
        }
        if order.outputs().is_empty() {
            return Err(StaticPricingError::NoOutputs);
        }

        let total_input_value = self.calculate_total_input_value(order);
        let total_output_value = self.calculate_total_output_value(order);

        let estimated_gas_cost = U256::from(self.gas_estimate_per_order)
            * U256::from(self.gas_price_gwei)
            * U256::from(1_000_000_000u64);

        trace!(
            total_input = %total_input_value,
            total_output = %total_output_value,
            gas_cost = %estimated_gas_cost,
            "estimated fill cost"
        );

        Ok(FillCostEstimate {
            estimated_gas: self.gas_estimate_per_order,
            estimated_gas_cost,
            total_input_value,
            total_output_value,
        })
    }

    #[instrument(skip(self, order), fields(order_hash = %order.order_hash()))]
    async fn is_profitable(&self, order: &SignedOrder) -> Result<bool, Self::Error> {
        let estimate = self.estimate_fill_cost(order).await?;

        let total_cost = estimate
            .total_output_value
            .saturating_add(estimate.estimated_gas_cost)
            .saturating_add(self.min_profit_threshold);

        let profitable = estimate.total_input_value >= total_cost;

        trace!(
            input = %estimate.total_input_value,
            cost = %total_cost,
            profitable = profitable,
            "profitability check"
        );

        Ok(profitable)
    }
}
