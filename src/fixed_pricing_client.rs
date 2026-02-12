use alloy::primitives::{Address, U256};
use init4_bin_base::deps::tracing::{instrument, trace};
use signet_constants::SignetSystemConstants;
use signet_types::SignedOrder;
use std::collections::HashMap;

/// Metadata for a known token used in value normalization.
#[derive(Debug, Clone, Copy)]
struct TokenInfo {
    decimals: u8,
    price_usd: U256,
}

/// Errors from the fixed pricing acceptable loss check.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum FixedPricingError {
    /// The order has no inputs.
    #[error("order has no inputs")]
    NoInputs,
    /// The order has no outputs.
    #[error("order has no outputs")]
    NoOutputs,
    /// A token address in the order is not recognized.
    #[error("unknown token: {0}")]
    UnknownToken(Address),
    /// A calculation overflowed.
    #[error("arithmetic overflow")]
    Overflow,
}

/// Pricing client that normalizes token values using hardcoded exchange rates and checks that the
/// filler's loss does not exceed a configurable percentage.
#[derive(Debug)]
pub(crate) struct FixedPricingClient {
    max_loss_percent: u8,
    token_info: HashMap<Address, TokenInfo>,
}

impl FixedPricingClient {
    /// Returns the wrapped native token address for the given chain name, if one is defined.
    fn wrapped_token_address(chain_name: &str) -> Option<Address> {
        match chain_name {
            "parmigiana" => Some(signet_constants::parmigiana::WRAPPED),
            "mainnet" => Some(signet_constants::mainnet::WRAPPED),
            _ => None,
        }
    }

    /// Create a new [`FixedPricingClient`].
    ///
    /// Builds a lookup table of known token addresses, decimals, and USD prices from the chain
    /// constants. The wrapped token address is resolved from `chain_name`.
    pub(crate) fn new(
        constants: &SignetSystemConstants,
        chain_name: &str,
        max_loss_percent: u8,
    ) -> Self {
        let host_tokens = constants.host().tokens();
        let rollup_tokens = constants.rollup().tokens();

        let mut token_info = HashMap::from([
            // Host chain tokens
            (host_tokens.usdc(), TokenInfo { decimals: 6, price_usd: U256::from(1) }),
            (host_tokens.usdt(), TokenInfo { decimals: 6, price_usd: U256::from(1) }),
            (host_tokens.weth(), TokenInfo { decimals: 18, price_usd: U256::from(3_000) }),
            (host_tokens.wbtc(), TokenInfo { decimals: 8, price_usd: U256::from(60_000) }),
            // Rollup chain tokens
            (
                signet_constants::NATIVE_TOKEN_ADDRESS,
                TokenInfo { decimals: 18, price_usd: U256::from(1) },
            ),
            (rollup_tokens.weth(), TokenInfo { decimals: 18, price_usd: U256::from(3_000) }),
            (rollup_tokens.wbtc(), TokenInfo { decimals: 8, price_usd: U256::from(60_000) }),
        ]);

        if let Some(wrapped) = Self::wrapped_token_address(chain_name) {
            token_info.insert(wrapped, TokenInfo { decimals: 18, price_usd: U256::from(1) });
        }

        Self { max_loss_percent, token_info }
    }

    #[instrument(skip_all, fields(order_hash = %order.order_hash()))]
    pub(crate) fn is_acceptable(&self, order: &SignedOrder) -> Result<bool, FixedPricingError> {
        if order.permit().permit.permitted.is_empty() {
            return Err(FixedPricingError::NoInputs);
        }
        if order.outputs().is_empty() {
            return Err(FixedPricingError::NoOutputs);
        }

        // Normalize a raw token amount to an 18-decimal USD-equivalent value and add it to the
        // running total, returning the new total:
        // `runnning_total + (amount * price_usd * 10^(18 - decimals))`
        let try_sum = |running_total: U256,
                       (token_address, amount): (&Address, U256)|
         -> Result<U256, FixedPricingError> {
            let token_info = self
                .token_info
                .get(token_address)
                .ok_or(FixedPricingError::UnknownToken(*token_address))?;
            18_u8
                .checked_sub(token_info.decimals)
                .map(U256::from)
                .and_then(|exponent| U256::from(10_u64).checked_pow(exponent))
                .and_then(|scale| token_info.price_usd.checked_mul(scale))
                .and_then(|multiplier| amount.checked_mul(multiplier))
                .and_then(|normalized_amount| running_total.checked_add(normalized_amount))
                .ok_or(FixedPricingError::Overflow)
        };

        let normalized_total_input = order
            .permit()
            .permit
            .permitted
            .iter()
            .map(|permitted| (&permitted.token, permitted.amount))
            .try_fold(U256::ZERO, try_sum)?;
        let normalized_total_output = order
            .outputs()
            .iter()
            .map(|output| (&output.token, output.amount))
            .try_fold(U256::ZERO, try_sum)?;

        // acceptable if inputs/outputs >= 90%, i.e. inputs * 100 >= outputs * 90
        let lhs = normalized_total_input
            .checked_mul(U256::from(100))
            .ok_or(FixedPricingError::Overflow)?;
        let rhs = 100_u8
            .checked_sub(self.max_loss_percent)
            .map(U256::from)
            .and_then(|percent| normalized_total_output.checked_mul(percent))
            .ok_or(FixedPricingError::Overflow)?;
        let is_acceptable = lhs >= rhs;

        trace!(
            %normalized_total_input,
            %normalized_total_output,
            max_loss_percent = self.max_loss_percent,
            is_acceptable,
            "acceptable loss check"
        );

        Ok(is_acceptable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Bytes;
    use signet_zenith::RollupOrders::{
        Output, Permit2Batch, PermitBatchTransferFrom, TokenPermissions,
    };

    fn parmigiana_client(max_loss_percent: u8) -> FixedPricingClient {
        FixedPricingClient::new(
            &SignetSystemConstants::parmigiana(),
            "parmigiana",
            max_loss_percent,
        )
    }

    /// Build a signed order with one USDC input and one USDC output at the given amounts (in raw
    /// 6-decimal units).
    fn usdc_order(input_amount: u64, output_amount: u64) -> SignedOrder {
        let usdc = SignetSystemConstants::parmigiana().host().tokens().usdc();
        SignedOrder::new(
            Permit2Batch {
                permit: PermitBatchTransferFrom {
                    permitted: vec![TokenPermissions {
                        token: usdc,
                        amount: U256::from(input_amount),
                    }],
                    nonce: U256::ZERO,
                    deadline: U256::ZERO,
                },
                owner: Address::ZERO,
                signature: Bytes::from([0; 65]),
            },
            vec![Output {
                token: usdc,
                amount: U256::from(output_amount),
                recipient: Address::ZERO,
                chainId: 0,
            }],
        )
    }

    // -- max_loss_percent = 0: input must be >= output --

    #[test]
    fn zero_max_loss_equal_values_is_acceptable() {
        let client = parmigiana_client(0);
        let order = usdc_order(1_000_000, 1_000_000);
        assert!(client.is_acceptable(&order).unwrap());
    }

    #[test]
    fn zero_max_loss_input_less_than_output_is_not_acceptable() {
        let client = parmigiana_client(0);
        let order = usdc_order(999_999, 1_000_000);
        assert!(!client.is_acceptable(&order).unwrap());
    }

    #[test]
    fn zero_max_loss_input_greater_than_output_is_acceptable() {
        let client = parmigiana_client(0);
        let order = usdc_order(1_000_001, 1_000_000);
        assert!(client.is_acceptable(&order).unwrap());
    }

    // -- max_loss_percent = 100: always acceptable --

    #[test]
    fn max_loss_100_always_acceptable() {
        let client = parmigiana_client(100);
        // input * 100 >= output * (100 - 100) → input * 100 >= 0 → always true
        let order = usdc_order(1, 1_000_000);
        assert!(client.is_acceptable(&order).unwrap());
    }

    // -- max_loss_percent = 101: overflows the u8 subtraction --

    #[test]
    fn max_loss_101_overflows() {
        let client = parmigiana_client(101);
        let order = usdc_order(1_000_000, 1_000_000);
        assert!(matches!(client.is_acceptable(&order), Err(FixedPricingError::Overflow)));
    }

    // -- standard threshold (10%) --

    #[test]
    fn standard_threshold_at_boundary_is_acceptable() {
        let client = parmigiana_client(10);
        // input * 100 >= output * 90 → 900_000 * 100 >= 1_000_000 * 90 → 90M >= 90M
        let order = usdc_order(900_000, 1_000_000);
        assert!(client.is_acceptable(&order).unwrap());
    }

    #[test]
    fn standard_threshold_just_below_boundary_is_not_acceptable() {
        let client = parmigiana_client(10);
        // 899_999 * 100 = 89_999_900 < 1_000_000 * 90 = 90_000_000
        let order = usdc_order(899_999, 1_000_000);
        assert!(!client.is_acceptable(&order).unwrap());
    }

    #[test]
    fn standard_threshold_above_boundary_is_acceptable() {
        let client = parmigiana_client(10);
        let order = usdc_order(u64::MAX, 1_000_000);
        assert!(client.is_acceptable(&order).unwrap());
    }

    // -- error cases --

    #[test]
    fn no_inputs_returns_error() {
        let client = parmigiana_client(10);
        let usdc = SignetSystemConstants::parmigiana().host().tokens().usdc();
        let order = SignedOrder::new(
            Permit2Batch {
                permit: PermitBatchTransferFrom {
                    permitted: vec![],
                    nonce: U256::ZERO,
                    deadline: U256::ZERO,
                },
                owner: Address::ZERO,
                signature: Bytes::from([0; 65]),
            },
            vec![Output {
                token: usdc,
                amount: U256::from(1_000_000_u64),
                recipient: Address::ZERO,
                chainId: 0,
            }],
        );
        assert!(matches!(client.is_acceptable(&order), Err(FixedPricingError::NoInputs)));
    }

    #[test]
    fn no_outputs_returns_error() {
        let client = parmigiana_client(10);
        let usdc = SignetSystemConstants::parmigiana().host().tokens().usdc();
        let order = SignedOrder::new(
            Permit2Batch {
                permit: PermitBatchTransferFrom {
                    permitted: vec![TokenPermissions {
                        token: usdc,
                        amount: U256::from(1_000_000_u64),
                    }],
                    nonce: U256::ZERO,
                    deadline: U256::ZERO,
                },
                owner: Address::ZERO,
                signature: Bytes::from([0; 65]),
            },
            vec![],
        );
        assert!(matches!(client.is_acceptable(&order), Err(FixedPricingError::NoOutputs)));
    }

    #[test]
    fn unknown_token_returns_error() {
        let client = parmigiana_client(10);
        let unknown = Address::repeat_byte(0xFF);
        let usdc = SignetSystemConstants::parmigiana().host().tokens().usdc();
        let order = SignedOrder::new(
            Permit2Batch {
                permit: PermitBatchTransferFrom {
                    permitted: vec![TokenPermissions { token: unknown, amount: U256::from(1_u64) }],
                    nonce: U256::ZERO,
                    deadline: U256::ZERO,
                },
                owner: Address::ZERO,
                signature: Bytes::from([0; 65]),
            },
            vec![Output {
                token: usdc,
                amount: U256::from(1_u64),
                recipient: Address::ZERO,
                chainId: 0,
            }],
        );
        assert!(matches!(
            client.is_acceptable(&order),
            Err(FixedPricingError::UnknownToken(addr)) if addr == unknown
        ));
    }

    // -- cross-token normalization --

    #[test]
    fn cross_token_normalization_equal_usd_value_is_acceptable() {
        let client = parmigiana_client(0);
        let constants = SignetSystemConstants::parmigiana();
        let weth = constants.host().tokens().weth();
        let usdc = constants.host().tokens().usdc();
        // 1 WETH ($3000) input, 3000 USDC ($3000) output → equal normalized value
        let order = SignedOrder::new(
            Permit2Batch {
                permit: PermitBatchTransferFrom {
                    permitted: vec![TokenPermissions {
                        token: weth,
                        amount: U256::from(1_000_000_000_000_000_000_u64), // 1 WETH (18 decimals)
                    }],
                    nonce: U256::ZERO,
                    deadline: U256::ZERO,
                },
                owner: Address::ZERO,
                signature: Bytes::from([0; 65]),
            },
            vec![Output {
                token: usdc,
                amount: U256::from(3_000_000_000_u64), // 3000 USDC (6 decimals)
                recipient: Address::ZERO,
                chainId: 0,
            }],
        );
        assert!(client.is_acceptable(&order).unwrap());
    }
}
