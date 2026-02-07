//! Price data types for the oracle service.

use serde::{Deserialize, Serialize};

/// A price update from Pyth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceUpdate {
    /// The asset symbol (e.g., "SOL", "BTC", "ETH")
    pub symbol: String,

    /// Price in USD (as f64 for simplicity; production may use fixed-point)
    pub price: f64,

    /// Confidence interval (+/- this amount)
    pub confidence: f64,

    /// Unix timestamp in seconds when this price was published
    pub publish_time: i64,

    /// The Pyth feed ID (hex string)
    pub feed_id: String,
}

/// Supported assets and their Pyth feed IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Asset {
    Sol,
    Btc,
    Eth,
}

impl Asset {
    /// Returns the Pyth price feed ID for this asset.
    pub fn feed_id(&self) -> &'static str {
        match self {
            Asset::Sol => "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
            Asset::Btc => "0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
            Asset::Eth => "0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
        }
    }

    /// Returns the symbol string for this asset.
    pub fn symbol(&self) -> &'static str {
        match self {
            Asset::Sol => "SOL",
            Asset::Btc => "BTC",
            Asset::Eth => "ETH",
        }
    }

    /// Parse an asset from its feed ID.
    pub fn from_feed_id(feed_id: &str) -> Option<Self> {
        match feed_id {
            id if id == Asset::Sol.feed_id() => Some(Asset::Sol),
            id if id == Asset::Btc.feed_id() => Some(Asset::Btc),
            id if id == Asset::Eth.feed_id() => Some(Asset::Eth),
            _ => None,
        }
    }

    /// Returns all supported assets.
    pub fn all() -> &'static [Asset] {
        &[Asset::Sol, Asset::Btc, Asset::Eth]
    }
}

impl std::fmt::Display for Asset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.symbol())
    }
}

/// A TWAP (time-weighted average price) sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwapSample {
    /// The price at this sample time
    pub price: f64,

    /// Unix timestamp in seconds
    pub timestamp: i64,
}

/// A completed TWAP calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwapResult {
    /// The asset this TWAP is for
    pub symbol: String,

    /// The calculated TWAP price
    pub twap_price: f64,

    /// Start of the TWAP window (Unix timestamp in seconds)
    pub window_start: i64,

    /// End of the TWAP window (Unix timestamp in seconds)
    pub window_end: i64,

    /// Number of samples used in calculation
    pub sample_count: usize,

    /// Percentage of expected samples that were collected (0.0 to 1.0)
    pub coverage: f64,
}

/// Rolling TWAP preview (what settlement price would be if it happened now).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwapPreview {
    /// The asset symbol
    pub symbol: String,

    /// Rolling 30-minute TWAP price
    pub twap_price: f64,

    /// Number of samples in the current window
    pub sample_count: usize,

    /// Coverage percentage (0.0 to 1.0)
    pub coverage: f64,

    /// Whether we're in the active settlement window (T-30 to T-0)
    pub in_settlement_window: bool,
}

/// Settlement timing information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementInfo {
    /// Unix timestamp of next settlement (round boundary)
    pub next_settlement: i64,

    /// Unix timestamp when TWAP window opens (30 min before settlement)
    pub twap_window_start: i64,

    /// Seconds until TWAP window opens
    pub seconds_to_twap_window: i64,

    /// Seconds until settlement
    pub seconds_to_settlement: i64,

    /// Whether we're currently in the TWAP window
    pub in_twap_window: bool,
}

/// Events emitted by the oracle service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OracleEvent {
    /// A new price update was received
    Price(PriceUpdate),

    /// A TWAP calculation completed (at settlement)
    Twap(TwapResult),

    /// Rolling TWAP preview (every few seconds)
    TwapPreview(TwapPreview),

    /// Settlement timing update (every second)
    Settlement(SettlementInfo),

    /// Connected to Pyth
    Connected,

    /// Disconnected from Pyth
    Disconnected,

    /// An error occurred
    Error { message: String },
}
