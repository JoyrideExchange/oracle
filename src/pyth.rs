//! Pyth Hermes client for streaming price updates.
//!
//! Connects to Pyth's Hermes API via Server-Sent Events (SSE) to receive
//! real-time price updates for configured assets.

use eventsource_client::{Client, SSE};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::types::{Asset, OracleEvent, PriceUpdate};

/// Default Hermes API endpoint.
pub const HERMES_URL: &str = "https://hermes.pyth.network";

/// Pyth Hermes API response for price updates.
#[derive(Debug, Deserialize)]
struct HermesPriceResponse {
    parsed: Vec<ParsedPrice>,
}

#[derive(Debug, Deserialize)]
struct ParsedPrice {
    id: String,
    price: PriceData,
    #[allow(dead_code)]
    ema_price: PriceData,
}

#[derive(Debug, Deserialize)]
struct PriceData {
    price: String,
    conf: String,
    expo: i32,
    publish_time: i64,
}

/// SSE event data from Hermes streaming endpoint.
#[derive(Debug, Deserialize)]
struct StreamUpdate {
    parsed: Vec<ParsedPrice>,
}

/// Client for Pyth Hermes API.
pub struct PythClient {
    event_tx: mpsc::Sender<OracleEvent>,
    assets: Vec<Asset>,
    hermes_url: String,
}

impl PythClient {
    /// Create a new Pyth client.
    pub fn new(event_tx: mpsc::Sender<OracleEvent>, assets: Vec<Asset>) -> Self {
        Self {
            event_tx,
            assets,
            hermes_url: HERMES_URL.to_string(),
        }
    }

    /// Create a new Pyth client with a custom Hermes URL.
    pub fn with_url(event_tx: mpsc::Sender<OracleEvent>, assets: Vec<Asset>, url: &str) -> Self {
        Self {
            event_tx,
            assets,
            hermes_url: url.to_string(),
        }
    }

    /// Run the client, streaming price updates indefinitely.
    /// Automatically reconnects on disconnect.
    pub async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            match self.connect_and_stream().await {
                Ok(()) => {
                    info!("Pyth connection closed gracefully");
                }
                Err(e) => {
                    error!("Pyth connection error: {}", e);
                    let _ = self
                        .event_tx
                        .send(OracleEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                }
            }

            let _ = self.event_tx.send(OracleEvent::Disconnected).await;

            info!("Reconnecting to Pyth in 5 seconds...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    }

    /// Fetch the latest price for all configured assets (one-shot).
    pub async fn fetch_latest(&self) -> anyhow::Result<Vec<PriceUpdate>> {
        let feed_ids: Vec<&str> = self.assets.iter().map(|a| a.feed_id()).collect();
        let query: String = feed_ids
            .iter()
            .map(|id| format!("ids[]={}", id))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("{}/v2/updates/price/latest?{}", self.hermes_url, query);
        debug!("Fetching latest prices from: {}", url);

        let response = reqwest::get(&url).await?;
        let data: HermesPriceResponse = response.json().await?;

        let updates: Vec<PriceUpdate> = data
            .parsed
            .into_iter()
            .filter_map(|p| self.parse_price_update(p))
            .collect();

        Ok(updates)
    }

    async fn connect_and_stream(&mut self) -> anyhow::Result<()> {
        let feed_ids: Vec<&str> = self.assets.iter().map(|a| a.feed_id()).collect();
        let query: String = feed_ids
            .iter()
            .map(|id| format!("ids[]={}", id))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("{}/v2/updates/price/stream?{}", self.hermes_url, query);
        info!("Connecting to Pyth Hermes SSE stream: {}", url);

        let client = eventsource_client::ClientBuilder::for_url(&url)?.build();
        let mut stream = client.stream();

        let _ = self.event_tx.send(OracleEvent::Connected).await;
        info!("Connected to Pyth Hermes");

        while let Some(event) = stream.next().await {
            match event {
                Ok(SSE::Event(ev)) => {
                    if ev.event_type == "message" {
                        match serde_json::from_str::<StreamUpdate>(&ev.data) {
                            Ok(update) => {
                                for parsed in update.parsed {
                                    if let Some(price_update) = self.parse_price_update(parsed) {
                                        debug!(
                                            "{}: ${:.4} (conf: ${:.4})",
                                            price_update.symbol,
                                            price_update.price,
                                            price_update.confidence
                                        );
                                        let _ = self
                                            .event_tx
                                            .send(OracleEvent::Price(price_update))
                                            .await;
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to parse SSE data: {} - {}", e, ev.data);
                            }
                        }
                    }
                }
                Ok(SSE::Comment(_)) | Ok(SSE::Connected(_)) => {
                    // Heartbeat or connection confirmation, ignore
                }
                Err(e) => {
                    error!("SSE stream error: {}", e);
                    return Err(anyhow::anyhow!("SSE stream error: {}", e));
                }
            }
        }

        Ok(())
    }

    fn parse_price_update(&self, parsed: ParsedPrice) -> Option<PriceUpdate> {
        // Normalize the feed ID (ensure it has 0x prefix and is lowercase)
        let feed_id = if parsed.id.starts_with("0x") {
            parsed.id.to_lowercase()
        } else {
            format!("0x{}", parsed.id.to_lowercase())
        };

        let asset = Asset::from_feed_id(&feed_id)?;

        // Convert price from string with exponent
        let price_raw: i64 = parsed.price.price.parse().ok()?;
        let conf_raw: i64 = parsed.price.conf.parse().ok()?;
        let expo = parsed.price.expo;

        // expo is negative (e.g., -8), so price = raw * 10^expo
        let multiplier = 10f64.powi(expo);
        let price = (price_raw as f64) * multiplier;
        let confidence = (conf_raw as f64) * multiplier;

        Some(PriceUpdate {
            symbol: asset.symbol().to_string(),
            price,
            confidence,
            publish_time: parsed.price.publish_time,
            feed_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_asset_feed_ids() {
        assert!(Asset::Sol.feed_id().starts_with("0x"));
        assert!(Asset::Btc.feed_id().starts_with("0x"));
        assert!(Asset::Eth.feed_id().starts_with("0x"));
    }

    #[test]
    fn test_asset_from_feed_id() {
        assert_eq!(Asset::from_feed_id(Asset::Sol.feed_id()), Some(Asset::Sol));
        assert_eq!(Asset::from_feed_id(Asset::Btc.feed_id()), Some(Asset::Btc));
        assert_eq!(Asset::from_feed_id(Asset::Eth.feed_id()), Some(Asset::Eth));
        assert_eq!(Asset::from_feed_id("unknown"), None);
    }
}
