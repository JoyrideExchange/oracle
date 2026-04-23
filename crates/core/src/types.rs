//! Oracle-local domain types.
//!
//! Wire-format types (`PriceUpdate`, `TwapPreview`, `TwapResult`, `WirePayload`,
//! `BroadcastFrame`) live in the `joyride-oracle-wire` crate and are re-exported
//! at the `joyride_oracle_core` crate root. This module holds the in-process
//! domain vocabulary: [`OracleEvent`] and [`Asset`].
//!
//! [`OracleEvent`] intentionally does *not* have a `Heartbeat` variant:
//! heartbeats are a WebSocket transport concern and never flow through the
//! in-process event channel. They live only on the wire, as
//! `WirePayload::Heartbeat`.

use serde::{Deserialize, Serialize};

/// Events produced by the oracle's in-process pipeline (Pyth ingestion,
/// TWAP calculator, upstream connection status). Fanned out over the
/// internal broadcast channel to the WebSocket server and to any in-process
/// consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OracleEvent {
    /// A new price update was received.
    Price(joyride_oracle_wire::PriceUpdate),

    /// Rolling TWAP preview (every few seconds).
    TwapPreview(joyride_oracle_wire::TwapPreview),

    /// Upstream Pyth connection established.
    Connected,

    /// Upstream Pyth connection lost.
    Disconnected,

    /// An error occurred on the upstream connection.
    Error { message: String },
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
