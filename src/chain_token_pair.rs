use alloy::primitives::Address;
use init4_bin_base::deps::tracing::warn;
use signet_constants::SignetSystemConstants;
use std::{
    collections::HashMap,
    fmt::{self, Display, Formatter},
    sync::OnceLock,
};

/// Human-readable names for known (chain, token) pairs.
static TOKEN_NAMES: OnceLock<HashMap<ChainTokenPair, &'static str>> = OnceLock::new();

/// Human-readable names for known chain IDs.
static CHAIN_NAMES: OnceLock<HashMap<u64, &'static str>> = OnceLock::new();

/// Identifies a token on a specific chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ChainTokenPair {
    chain_id: u64,
    token: Address,
}

impl ChainTokenPair {
    /// Create from a chain ID and token address, as found on order outputs.
    pub(crate) const fn new(chain_id: u64, token: Address) -> Self {
        Self { chain_id, token }
    }

    /// The chain ID.
    pub(crate) const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// The token address.
    pub(crate) const fn token(&self) -> Address {
        self.token
    }

    /// All known ERC20 tokens that need Permit2 allowance checks. Native tokens are excluded
    /// because they don't use ERC20 allowances.
    pub(crate) fn known_erc20_tokens(
        constants: &SignetSystemConstants,
    ) -> [(ChainTokenPair, &'static str); 6] {
        let host_chain_id = constants.host_chain_id();
        let ru_chain_id = constants.ru_chain_id();
        let host_tokens = constants.host().tokens();
        let rollup_tokens = constants.rollup().tokens();

        [
            (Self::new(host_chain_id, host_tokens.usdc()), "host USDC"),
            (Self::new(host_chain_id, host_tokens.usdt()), "host USDT"),
            (Self::new(host_chain_id, host_tokens.weth()), "host WETH"),
            (Self::new(host_chain_id, host_tokens.wbtc()), "host WBTC"),
            (Self::new(ru_chain_id, rollup_tokens.weth()), "rollup WETH"),
            (Self::new(ru_chain_id, rollup_tokens.wbtc()), "rollup WBTC"),
        ]
    }

    /// Populates the global token and chain name lookups from chain constants. Called once during
    /// `FillerContext` initialization.
    pub(crate) fn init_token_names(constants: &SignetSystemConstants) {
        let host_chain_id = constants.host_chain_id();
        let ru_chain_id = constants.ru_chain_id();

        let mut token_names: HashMap<Self, &'static str> =
            Self::known_erc20_tokens(constants).into_iter().collect();
        token_names
            .insert(Self::new(ru_chain_id, signet_constants::NATIVE_TOKEN_ADDRESS), "rollup USD");

        let chain_names = HashMap::from([(host_chain_id, "host"), (ru_chain_id, "rollup")]);

        if TOKEN_NAMES.set(token_names).is_err() {
            warn!("token names already initialized");
        }
        if CHAIN_NAMES.set(chain_names).is_err() {
            warn!("chain names already initialized");
        }
    }
}

impl Display for ChainTokenPair {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if let Some(name) = TOKEN_NAMES.get().and_then(|names| names.get(self)) {
            return write!(formatter, "{name}");
        }
        let chain = CHAIN_NAMES
            .get()
            .and_then(|names| names.get(&self.chain_id).map(ToString::to_string))
            .unwrap_or_else(|| format!("unknown-chain({})", self.chain_id));
        write!(formatter, "{chain}:{}", self.token)
    }
}
