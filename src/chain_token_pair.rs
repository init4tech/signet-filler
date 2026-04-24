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

/// Known token types across host and rollup chains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum KnownToken {
    /// USDC on the host chain.
    HostUsdc,
    /// USDT on the host chain.
    HostUsdt,
    /// WETH on the host chain.
    HostWeth,
    /// WBTC on the host chain.
    HostWbtc,
    /// WETH on the rollup chain.
    RollupWeth,
    /// WBTC on the rollup chain.
    RollupWbtc,
    /// Native USD token on the rollup chain.
    RollupUsd,
}

impl KnownToken {
    /// All known tokens.
    const ALL: [Self; 7] = [
        Self::HostUsdc,
        Self::HostUsdt,
        Self::HostWeth,
        Self::HostWbtc,
        Self::RollupWeth,
        Self::RollupWbtc,
        Self::RollupUsd,
    ];

    /// Known ERC20 tokens that need Permit2 allowance checks. Native tokens are excluded because
    /// they don't use ERC20 allowances.
    pub(crate) const ERC20: [Self; 6] = [
        Self::HostUsdc,
        Self::HostUsdt,
        Self::HostWeth,
        Self::HostWbtc,
        Self::RollupWeth,
        Self::RollupWbtc,
    ];

    /// Human-readable name.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::HostUsdc => "host USDC",
            Self::HostUsdt => "host USDT",
            Self::HostWeth => "host WETH",
            Self::HostWbtc => "host WBTC",
            Self::RollupWeth => "rollup WETH",
            Self::RollupWbtc => "rollup WBTC",
            Self::RollupUsd => "rollup USD",
        }
    }

    /// Resolve to the concrete [`ChainTokenPair`] using chain constants.
    pub(crate) fn resolve(&self, constants: &SignetSystemConstants) -> ChainTokenPair {
        let host_chain_id = constants.host_chain_id();
        let ru_chain_id = constants.ru_chain_id();
        let host_tokens = constants.host().tokens();
        let rollup_tokens = constants.rollup().tokens();
        match self {
            Self::HostUsdc => ChainTokenPair::new(host_chain_id, host_tokens.usdc()),
            Self::HostUsdt => ChainTokenPair::new(host_chain_id, host_tokens.usdt()),
            Self::HostWeth => ChainTokenPair::new(host_chain_id, host_tokens.weth()),
            Self::HostWbtc => ChainTokenPair::new(host_chain_id, host_tokens.wbtc()),
            Self::RollupWeth => ChainTokenPair::new(ru_chain_id, rollup_tokens.weth()),
            Self::RollupWbtc => ChainTokenPair::new(ru_chain_id, rollup_tokens.wbtc()),
            Self::RollupUsd => {
                ChainTokenPair::new(ru_chain_id, signet_constants::NATIVE_TOKEN_ADDRESS)
            }
        }
    }
}

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

    /// Populates the global token and chain name lookups from chain constants. Called once during
    /// `FillerContext` initialization.
    pub(crate) fn init_token_names(constants: &SignetSystemConstants) {
        let token_names: HashMap<Self, &'static str> =
            KnownToken::ALL.iter().map(|known| (known.resolve(constants), known.name())).collect();
        let chain_names = HashMap::from([
            (constants.host_chain_id(), "host"),
            (constants.ru_chain_id(), "rollup"),
        ]);

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
