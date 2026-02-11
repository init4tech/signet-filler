use super::{FillCostEstimate, PricingClient};
use alloy::primitives::{Address, U256};
use futures_util::future::try_join_all;
use init4_bin_base::deps::tracing::{debug, instrument, trace};
use serde::Deserialize;
use signet_types::SignedOrder;

/// Maximum number of output groups (groups aggregated by <token, chain ID>) supported per order.
const MAX_SUPPORTED_OUTPUT_GROUPS: usize = 10;

/// Precision multiplier for weight calculations in [`compute_output_splits`].
const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);

/// An aggregated input for a unique token.
#[derive(Debug, Clone, Copy)]
struct InputGroup {
    token: Address,
    amount: U256,
}

/// An aggregated output requirement for a unique (token, chain) pair.
#[derive(Debug, Clone, Copy)]
struct OutputGroup {
    token: Address,
    chain_id: u32,
    required: U256,
}

/// A round-1 quote result pairing an [`OutputGroup`] with the amount of output token the full
/// input would yield.
#[derive(Debug, Clone, Copy)]
struct OutputQuote {
    group: OutputGroup,
    /// Amount of output token quoted for the full input amount.
    quoted: U256,
}

/// A computed input allocation for an [`OutputGroup`].
#[derive(Debug, Clone, Copy)]
struct InputSplit {
    group: OutputGroup,
    /// Amount of input token to allocate to this output group.
    amount: U256,
}

/// Pricing client that queries the Radius solver API for quotes.
#[derive(Debug, Clone)]
pub struct RadiusPricingClient {
    client: reqwest::Client,
    base_url: String,
    bearer_token: String,
    rollup_chain_id: u64,
}

/// Quote response from the Radius solver API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuoteResponse {
    amount_out: String,
}

/// Error response body from the Radius solver API.
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    message: String,
}

/// Errors that can occur when querying the Radius solver API.
#[derive(Debug, thiserror::Error)]
pub enum RadiusPricingError {
    /// Order has no inputs.
    #[error("order has no inputs")]
    NoInputs,
    /// Order has no outputs.
    #[error("order has no outputs")]
    NoOutputs,
    /// Order has too many output groups (groups aggregated by <token, chain ID>) to price.
    #[error("order has too many output groups ({0}, max {MAX_SUPPORTED_OUTPUT_GROUPS})")]
    TooManyOutputs(usize),
    /// Multiple input groups with multiple output groups are not supported.
    #[error("orders with multiple input groups and multiple output groups are not supported")]
    MultipleInputsMultipleOutputs,
    /// Arithmetic overflow during pricing calculations.
    #[error("arithmetic overflow during pricing calculation")]
    Overflow,
    /// HTTP request to the Radius API failed.
    #[error("radius API request failed: {0:?}")]
    RequestFailed(#[source] reqwest::Error),
    /// Radius API returned an error response.
    #[error("radius API error ({error_code}): {message}")]
    ApiError {
        /// HTTP status code.
        status: u16,
        /// Error code from the response body.
        error_code: String,
        /// Human-readable error message.
        message: String,
    },
    /// Failed to parse the quote response.
    #[error("invalid quote response: {0}")]
    InvalidResponse(Box<dyn core::error::Error + Send + Sync>),
}

/// Compute how to split a single input amount across multiple output groups based on round-1
/// full-amount quotes.
///
/// Each output group is weighted by how much of the full-amount quote it consumes
/// (`required / quoted`), and the input is split proportionally.
fn compute_output_splits(
    input_amount: U256,
    quotes: &[OutputQuote],
) -> Result<Vec<InputSplit>, RadiusPricingError> {
    // weight_i = required_i * PRECISION / quoted_i
    let weights: Vec<U256> = quotes
        .iter()
        .map(|quote| {
            quote
                .group
                .required
                .checked_mul(PRECISION)
                .and_then(|scaled| scaled.checked_div(quote.quoted))
                .ok_or(RadiusPricingError::Overflow)
        })
        .collect::<Result<_, _>>()?;

    let total_weight = checked_sum(&weights)?;

    quotes
        .iter()
        .zip(&weights)
        .map(|(quote, weight)| {
            input_amount
                .checked_mul(*weight)
                .and_then(|scaled| scaled.checked_div(total_weight))
                .map(|amount| InputSplit { group: quote.group, amount })
                .ok_or(RadiusPricingError::Overflow)
        })
        .collect()
}

/// Sum a slice of [`U256`] values with checked arithmetic.
fn checked_sum(values: &[U256]) -> Result<U256, RadiusPricingError> {
    values
        .iter()
        .try_fold(U256::ZERO, |sum, val| sum.checked_add(*val).ok_or(RadiusPricingError::Overflow))
}

impl RadiusPricingClient {
    /// Create a new [`RadiusPricingClient`].
    pub fn new(base_url: String, bearer_token: String, rollup_chain_id: u64) -> Self {
        Self { client: reqwest::Client::new(), base_url, bearer_token, rollup_chain_id }
    }

    /// Request a quote from the Radius solver API for the given order parameters.
    async fn request_quote(
        &self,
        from_token: Address,
        to_token: Address,
        to_chain_id: u32,
        amount_in: U256,
        user_address: Address,
    ) -> Result<U256, RadiusPricingError> {
        let url = format!("{}/api/rfq/quote", self.base_url);

        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .query(&[
                ("fromChainId", self.rollup_chain_id.to_string()),
                ("toChainId", to_chain_id.to_string()),
                ("fromToken", from_token.to_string()),
                ("toToken", to_token.to_string()),
                ("amountIn", amount_in.to_string()),
                ("userAddress", user_address.to_string()),
            ])
            .send()
            .await
            .map_err(RadiusPricingError::RequestFailed)?;

        let status = response.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            return Err(match response.json::<ErrorResponse>().await {
                Ok(body) => RadiusPricingError::ApiError {
                    status: status_code,
                    error_code: body.error,
                    message: body.message,
                },
                Err(_) => RadiusPricingError::ApiError {
                    status: status_code,
                    error_code: "UNKNOWN".to_string(),
                    message: format!("HTTP {status_code}"),
                },
            });
        }

        let quote: QuoteResponse = response
            .json()
            .await
            .map_err(|error| RadiusPricingError::InvalidResponse(Box::new(error)))?;
        quote
            .amount_out
            .parse::<U256>()
            .map_err(|error| RadiusPricingError::InvalidResponse(Box::new(error)))
    }

    /// Quote multiple input groups against a single output group in parallel and return the checked
    /// sum of all quoted output amounts.
    async fn quote_inputs(
        &self,
        input_groups: &[InputGroup],
        output_group: &OutputGroup,
        owner: Address,
    ) -> Result<U256, RadiusPricingError> {
        let futures = input_groups.iter().map(|input| {
            debug!(
                from_token = %input.token,
                to_token = %output_group.token,
                amount_in = %input.amount,
                to_chain_id = output_group.chain_id,
                "requesting radius quote for input"
            );
            self.request_quote(
                input.token,
                output_group.token,
                output_group.chain_id,
                input.amount,
                owner,
            )
        });

        let quotes = try_join_all(futures).await?;
        let total = checked_sum(&quotes)?;

        trace!(total_quoted = %total, num_inputs = input_groups.len(), "aggregated input quotes");
        Ok(total)
    }

    /// Two-round quoting for a single input group against multiple output groups.
    ///
    /// Round 1 quotes the full input against each output group to get rates,
    /// [`compute_output_splits`] determines proportional splits, and round 2 re-quotes at split
    /// amounts.
    ///
    /// Returns a [`FillCostEstimate`] with values expressed in input-token terms:
    /// `total_input_value` is the input amount, `total_output_value` is the effective cost to cover
    /// all outputs.
    async fn quote_outputs(
        &self,
        input_group: &InputGroup,
        output_groups: &[OutputGroup],
        owner: Address,
    ) -> Result<FillCostEstimate, RadiusPricingError> {
        // Round 1: quote full input amount against each output group.
        let round1_futures = output_groups.iter().map(|output| {
            debug!(
                from_token = %input_group.token,
                to_token = %output.token,
                amount_in = %input_group.amount,
                to_chain_id = output.chain_id,
                "requesting round-1 radius quote"
            );
            self.request_quote(
                input_group.token,
                output.token,
                output.chain_id,
                input_group.amount,
                owner,
            )
        });
        let round1_results = try_join_all(round1_futures).await?;

        let output_quotes: Vec<_> = output_groups
            .iter()
            .zip(&round1_results)
            .map(|(group, &quoted)| OutputQuote { group: *group, quoted })
            .collect();
        let splits = compute_output_splits(input_group.amount, &output_quotes)?;

        // Round 2: re-quote at split amounts for accurate pricing.
        let round2_futures = splits.iter().map(|split| {
            debug!(
                from_token = %input_group.token,
                to_token = %split.group.token,
                amount_in = %split.amount,
                to_chain_id = split.group.chain_id,
                "requesting round-2 radius quote"
            );
            self.request_quote(
                input_group.token,
                split.group.token,
                split.group.chain_id,
                split.amount,
                owner,
            )
        });
        let round2_results = try_join_all(round2_futures).await?;

        // Compute effective cost in input-token terms. If a split covers its output, the cost is
        // just the split amount. If it falls short, we extrapolate how much input would actually be
        // needed.
        let mut total_cost = U256::ZERO;
        for (split, &round2_out) in splits.iter().zip(&round2_results) {
            let cost = if round2_out >= split.group.required {
                split.amount
            } else {
                split
                    .amount
                    .checked_mul(split.group.required)
                    .and_then(|product| product.checked_div(round2_out))
                    .ok_or(RadiusPricingError::Overflow)?
            };
            total_cost = total_cost.checked_add(cost).ok_or(RadiusPricingError::Overflow)?;

            trace!(
                to_token = %split.group.token,
                to_chain_id = split.group.chain_id,
                split_amount = %split.amount,
                round2_out = %round2_out,
                required = %split.group.required,
                cost = %cost,
                "per-output cost"
            );
        }

        trace!(
            total_input = %input_group.amount,
            total_cost = %total_cost,
            num_groups = output_groups.len(),
            "aggregated output costs"
        );

        Ok(FillCostEstimate {
            estimated_gas: 0,
            estimated_gas_cost: U256::ZERO,
            total_input_value: input_group.amount,
            total_output_value: total_cost,
        })
    }
}

impl PricingClient for RadiusPricingClient {
    type Error = RadiusPricingError;

    #[instrument(skip_all)]
    async fn estimate_fill_cost(
        &self,
        order: &SignedOrder,
    ) -> Result<FillCostEstimate, Self::Error> {
        let permitted = &order.permit().permit.permitted;
        if permitted.is_empty() {
            return Err(RadiusPricingError::NoInputs);
        }
        let outputs = order.outputs();
        if outputs.is_empty() {
            return Err(RadiusPricingError::NoOutputs);
        }
        let owner = order.permit().owner;

        // Aggregate inputs by token.
        let mut input_groups: Vec<InputGroup> = Vec::new();
        for input in permitted {
            if let Some(group) = input_groups.iter_mut().find(|group| group.token == input.token) {
                group.amount =
                    group.amount.checked_add(input.amount).ok_or(RadiusPricingError::Overflow)?;
            } else {
                input_groups.push(InputGroup { token: input.token, amount: input.amount });
            }
        }

        // Aggregate outputs by (token, chain_id).
        let mut output_groups: Vec<OutputGroup> = Vec::new();
        for output in outputs {
            if let Some(group) = output_groups
                .iter_mut()
                .find(|group| group.token == output.token && group.chain_id == output.chainId)
            {
                group.required = group
                    .required
                    .checked_add(output.amount)
                    .ok_or(RadiusPricingError::Overflow)?;
            } else {
                output_groups.push(OutputGroup {
                    token: output.token,
                    chain_id: output.chainId,
                    required: output.amount,
                });
            }
        }

        if output_groups.len() > MAX_SUPPORTED_OUTPUT_GROUPS {
            return Err(RadiusPricingError::TooManyOutputs(output_groups.len()));
        }
        if input_groups.len() > 1 && output_groups.len() > 1 {
            return Err(RadiusPricingError::MultipleInputsMultipleOutputs);
        }

        if output_groups.len() > 1 {
            return self.quote_outputs(&input_groups[0], &output_groups, owner).await;
        }

        let total_output_value = output_groups[0].required;
        let total_input_value = self.quote_inputs(&input_groups, &output_groups[0], owner).await?;

        trace!(
            total_input_value = %total_input_value,
            total_output_value = %total_output_value,
            "fill cost estimate"
        );

        Ok(FillCostEstimate {
            estimated_gas: 0,
            estimated_gas_cost: U256::ZERO,
            total_input_value,
            total_output_value,
        })
    }

    #[instrument(skip(self, order), fields(order_hash = %order.order_hash()))]
    async fn is_profitable(&self, order: &SignedOrder) -> Result<bool, Self::Error> {
        let estimate = self.estimate_fill_cost(order).await?;
        let profitable = estimate.total_input_value >= estimate.total_output_value;

        trace!(
            quoted_value = %estimate.total_input_value,
            required_output = %estimate.total_output_value,
            profitable,
            "profitability check"
        );

        Ok(profitable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_quote(chain_id: u32, required: u64, quoted: u64) -> OutputQuote {
        OutputQuote {
            group: OutputGroup { token: Address::ZERO, chain_id, required: U256::from(required) },
            quoted: U256::from(quoted),
        }
    }

    #[test]
    fn single_group_split_returns_full_input() {
        let input = U256::from(10_000);
        let quotes = vec![test_quote(1, 5, 10)];
        let splits = compute_output_splits(input, &quotes).unwrap();

        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].amount, input);
        assert_eq!(splits[0].group.chain_id, 1);
    }

    #[test]
    fn equal_rates_produce_proportional_splits() {
        let input = U256::from(10_000);
        // Both groups quote 100 for the full input, but group 1 needs 30 and group 2 needs 70 —
        // splits should be 30/70.
        let quotes = vec![test_quote(1, 30, 100), test_quote(2, 70, 100)];
        let splits = compute_output_splits(input, &quotes).unwrap();

        assert_eq!(splits.len(), 2);
        assert_eq!(splits[0].amount, U256::from(3_000));
        assert_eq!(splits[1].amount, U256::from(7_000));
    }

    #[test]
    fn different_rates_weight_by_cost() {
        let input = U256::from(10_000);
        // Group 1: need 50, quote gives 100 → weight = 50/100 = 0.5
        // Group 2: need 50, quote gives 200 → weight = 50/200 = 0.25
        // Total weight = 0.75
        // Split 1 = 10000 * 0.5 / 0.75 = 6666
        // Split 2 = 10000 * 0.25 / 0.75 = 3333
        let quotes = vec![test_quote(1, 50, 100), test_quote(2, 50, 200)];
        let splits = compute_output_splits(input, &quotes).unwrap();

        assert_eq!(splits.len(), 2);
        assert_eq!(splits[0].amount, U256::from(6666));
        assert_eq!(splits[1].amount, U256::from(3333));
    }

    #[test]
    fn splits_do_not_exceed_input() {
        let input = U256::from(1_000);
        let quotes = vec![test_quote(1, 33, 100), test_quote(2, 33, 100), test_quote(3, 34, 100)];
        let splits = compute_output_splits(input, &quotes).unwrap();

        let total: U256 = splits.iter().map(|split| split.amount).sum();
        assert!(total <= input, "sum of splits ({total}) exceeds input ({input})");
    }

    #[test]
    fn zero_quote_returns_overflow() {
        let input = U256::from(10_000);
        let quotes = vec![test_quote(1, 50, 0)];
        let result = compute_output_splits(input, &quotes);

        assert!(matches!(result, Err(RadiusPricingError::Overflow)));
    }
}
