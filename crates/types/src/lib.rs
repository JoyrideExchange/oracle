//! Wire-format types for the Joyride Oracle broadcast feed.
//!
//! This crate is *wire only*. It describes the exact JSON the oracle
//! broadcasts over WebSocket and nothing else — no Pyth ingestion, no
//! TWAP calculator, no in-process event type. Consumers parsing the
//! feed in Rust should deserialize into [`BroadcastFrame`]; every frame
//! the oracle emits matches that shape.
//!
//! Domain types (the in-process [`OracleEvent`] enum, [`Asset`], and the
//! ingestion/calculator code) live in the `joyride-oracle-core` crate.
//! Embedders who want the in-process API without the WebSocket server
//! should depend on `joyride-oracle-core` directly.
//!
//! [`OracleEvent`]: https://docs.rs/joyride-oracle-core
//! [`Asset`]: https://docs.rs/joyride-oracle-core

use chrono::{DateTime, Utc};
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

/// Rolling TWAP preview (what settlement price would be if it happened now).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwapPreview {
    /// The asset symbol
    pub symbol: String,

    /// Rolling 30-minute TWAP price
    pub twap: f64,

    /// Number of samples in the current window
    pub sample_count: usize,

    /// Coverage percentage (0.0 to 1.0)
    pub coverage: f64,
}

/// The `type`-tagged payload carried by every [`BroadcastFrame`].
///
/// Includes both domain events (price updates, rolling TWAP previews, upstream
/// connection status) and transport-only frames (heartbeats). This is
/// deliberately separate from the in-process `OracleEvent` enum in the
/// `joyride-oracle-core` crate: heartbeats are a WebSocket keepalive and
/// never flow through the in-process event channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WirePayload {
    /// A new price update was received.
    Price(PriceUpdate),

    /// Rolling TWAP preview (every few seconds).
    TwapPreview(TwapPreview),

    /// Upstream Pyth connection established.
    Connected,

    /// Upstream Pyth connection lost.
    Disconnected,

    /// An error occurred on the upstream connection.
    Error { message: String },

    /// WebSocket keepalive; emitted by the server on a fixed interval.
    Heartbeat,
}

/// A single broadcast frame as seen on the wire: an RFC 3339 `timestamp`
/// recorded when the oracle serialized the payload, flattened together
/// with a [`WirePayload`] body (`type` + variant fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroadcastFrame {
    /// Envelope timestamp, stamped by the oracle at serialization time.
    /// Use this to measure end-to-end freshness or detect clock skew;
    /// compare against `PriceUpdate::publish_time` for Pyth-to-client latency.
    pub timestamp: DateTime<Utc>,

    /// The payload carried by this frame.
    #[serde(flatten)]
    pub payload: WirePayload,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_frame_round_trips_price() {
        let json = r#"{"timestamp":"2026-04-22T12:34:56.789Z","type":"price","symbol":"SOL","price":123.45,"confidence":0.12,"publish_time":1776947696,"feed_id":"0xef"}"#;
        let frame: BroadcastFrame = serde_json::from_str(json).unwrap();
        match &frame.payload {
            WirePayload::Price(p) => {
                assert_eq!(p.symbol, "SOL");
                assert_eq!(p.publish_time, 1776947696);
            }
            other => panic!("expected Price, got {other:?}"),
        }
        assert_eq!(
            frame.timestamp.to_rfc3339(),
            "2026-04-22T12:34:56.789+00:00"
        );

        let reserialized = serde_json::to_string(&frame).unwrap();
        let reparsed: BroadcastFrame = serde_json::from_str(&reserialized).unwrap();
        assert!(matches!(reparsed.payload, WirePayload::Price(_)));
    }

    #[test]
    fn broadcast_frame_round_trips_heartbeat_and_status() {
        for (json, expected) in [
            (
                r#"{"timestamp":"2026-04-22T12:34:56.789Z","type":"heartbeat"}"#,
                "heartbeat",
            ),
            (
                r#"{"timestamp":"2026-04-22T12:34:56.789Z","type":"connected"}"#,
                "connected",
            ),
            (
                r#"{"timestamp":"2026-04-22T12:34:56.789Z","type":"disconnected"}"#,
                "disconnected",
            ),
            (
                r#"{"timestamp":"2026-04-22T12:34:56.789Z","type":"error","message":"pyth down"}"#,
                "error",
            ),
        ] {
            let frame: BroadcastFrame = serde_json::from_str(json).unwrap();
            let reserialized = serde_json::to_string(&frame).unwrap();
            let reparsed: BroadcastFrame = serde_json::from_str(&reserialized).unwrap();
            let variant = match reparsed.payload {
                WirePayload::Heartbeat => "heartbeat",
                WirePayload::Connected => "connected",
                WirePayload::Disconnected => "disconnected",
                WirePayload::Error { .. } => "error",
                _ => "unexpected",
            };
            assert_eq!(variant, expected);
        }
    }
}
