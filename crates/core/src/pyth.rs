//! Pyth Hermes client for streaming price updates.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eventsource_client::{Client, SSE};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::types::{Asset, OracleEvent};
use joyride_oracle_wire::PriceUpdate;

/// Default Hermes API endpoint.
pub const HERMES_URL: &str = "https://hermes.pyth.network";
const FRESHNESS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const INITIAL_RECONNECT_BACKOFF_SECS: u64 = 5;
const MAX_RECONNECT_BACKOFF_SECS: u64 = 60;
const MAX_RECEIVE_LAG_MS: i64 = 10_000;
const MAX_UNCHANGED_STREAK: u32 = 5;

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

#[derive(Debug, Deserialize)]
struct StreamUpdate {
    parsed: Vec<ParsedPrice>,
}

#[derive(Debug, Default)]
struct AssetFreshnessState {
    prev_publish_time: Option<i64>,
    unchanged_streak: u32,
    last_log_instant: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
struct FreshnessObservation {
    publish_advanced: bool,
    unchanged_streak: u32,
    receive_lag_ms: i64,
    publish_gap_secs: Option<i64>,
}

impl AssetFreshnessState {
    fn observe(&mut self, publish_time: i64, receive_time: i64) -> FreshnessObservation {
        let publish_gap_secs = self
            .prev_publish_time
            .map(|previous_publish_time| publish_time.saturating_sub(previous_publish_time));
        let publish_advanced = self
            .prev_publish_time
            .map(|prev| prev != publish_time)
            .unwrap_or(true);

        if publish_advanced {
            self.unchanged_streak = 0;
        } else {
            self.unchanged_streak = self.unchanged_streak.saturating_add(1);
        }

        self.prev_publish_time = Some(publish_time);

        FreshnessObservation {
            publish_advanced,
            unchanged_streak: self.unchanged_streak,
            receive_lag_ms: receive_time
                .saturating_sub(publish_time)
                .saturating_mul(1000),
            publish_gap_secs,
        }
    }

    fn should_emit_sample(&self, now: Instant) -> bool {
        self.last_log_instant
            .map(|last| now.duration_since(last) >= FRESHNESS_LOG_INTERVAL)
            .unwrap_or(true)
    }

    fn mark_logged(&mut self, now: Instant) {
        self.last_log_instant = Some(now);
    }
}

/// Client for Pyth Hermes API.
pub struct PythClient {
    event_tx: mpsc::Sender<OracleEvent>,
    assets: Vec<Asset>,
    hermes_url: String,
}

impl PythClient {
    pub fn new(event_tx: mpsc::Sender<OracleEvent>, assets: Vec<Asset>) -> Self {
        Self {
            event_tx,
            assets,
            hermes_url: HERMES_URL.to_string(),
        }
    }

    pub fn with_url(event_tx: mpsc::Sender<OracleEvent>, assets: Vec<Asset>, url: &str) -> Self {
        Self {
            event_tx,
            assets,
            hermes_url: url.to_string(),
        }
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        let mut backoff_secs = INITIAL_RECONNECT_BACKOFF_SECS;

        loop {
            let reconnect_reason: String;
            match self.connect_and_stream().await {
                Ok(()) => {
                    reconnect_reason = "stream_closed".to_string();
                    info!("Pyth connection closed gracefully");
                    backoff_secs = INITIAL_RECONNECT_BACKOFF_SECS;
                }
                Err(e) => {
                    reconnect_reason = e.to_string();
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
            info!(
                backoff_secs,
                reconnect_reason = %reconnect_reason,
                "Reconnecting to Pyth after backoff"
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
            backoff_secs = next_backoff_secs(backoff_secs);
        }
    }

    pub async fn fetch_latest(&self) -> anyhow::Result<Vec<PriceUpdate>> {
        let feed_ids: Vec<&str> = self.assets.iter().map(|a| a.feed_id()).collect();
        let query = feed_ids
            .iter()
            .map(|id| format!("ids[]={}", id))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("{}/v2/updates/price/latest?{}", self.hermes_url, query);
        debug!("Fetching latest prices from: {}", url);

        let response = reqwest::get(&url).await?;
        let data: HermesPriceResponse = response.json().await?;

        Ok(data
            .parsed
            .into_iter()
            .filter_map(|p| self.parse_price_update(p))
            .collect())
    }

    async fn connect_and_stream(&mut self) -> anyhow::Result<()> {
        let feed_ids: Vec<&str> = self.assets.iter().map(|a| a.feed_id()).collect();
        let query = feed_ids
            .iter()
            .map(|id| format!("ids[]={}", id))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("{}/v2/updates/price/stream?{}", self.hermes_url, query);
        info!("Connecting to Pyth Hermes SSE stream: {}", url);

        let client = eventsource_client::ClientBuilder::for_url(&url)?.build();
        let mut stream = client.stream();
        let mut freshness_state: HashMap<String, AssetFreshnessState> = HashMap::new();

        let _ = self.event_tx.send(OracleEvent::Connected).await;
        info!("Connected to Pyth Hermes");

        loop {
            let event = match tokio::time::timeout(SSE_IDLE_TIMEOUT, stream.next()).await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    info!("Pyth Hermes SSE stream ended");
                    return Ok(());
                }
                Err(_) => {
                    warn!(
                        idle_timeout_secs = SSE_IDLE_TIMEOUT.as_secs(),
                        "No SSE events received from Hermes; forcing reconnect"
                    );
                    return Err(anyhow::anyhow!(
                        "Pyth Hermes SSE idle for {}s",
                        SSE_IDLE_TIMEOUT.as_secs()
                    ));
                }
            };

            match event {
                Ok(SSE::Event(ev)) if ev.event_type == "message" => {
                    match serde_json::from_str::<StreamUpdate>(&ev.data) {
                        Ok(update) => {
                            for parsed in update.parsed {
                                if let Some(price_update) = self.parse_price_update(parsed) {
                                    let receive_time = unix_now_secs();
                                    let now = Instant::now();
                                    let state = freshness_state
                                        .entry(price_update.symbol.clone())
                                        .or_default();
                                    let observation =
                                        state.observe(price_update.publish_time, receive_time);
                                    let abnormal = observation.receive_lag_ms > MAX_RECEIVE_LAG_MS
                                        || observation.unchanged_streak >= MAX_UNCHANGED_STREAK;

                                    if abnormal {
                                        warn!(
                                            asset = %price_update.symbol,
                                            publish_time = price_update.publish_time,
                                            publish_gap_secs = observation.publish_gap_secs,
                                            receive_time,
                                            receive_lag_ms = observation.receive_lag_ms,
                                            publish_advanced = observation.publish_advanced,
                                            unchanged_streak = observation.unchanged_streak,
                                            "hermes_freshness_abnormal"
                                        );
                                    } else if state.should_emit_sample(now) {
                                        info!(
                                            asset = %price_update.symbol,
                                            publish_time = price_update.publish_time,
                                            publish_gap_secs = observation.publish_gap_secs,
                                            receive_time,
                                            receive_lag_ms = observation.receive_lag_ms,
                                            publish_advanced = observation.publish_advanced,
                                            unchanged_streak = observation.unchanged_streak,
                                            "hermes_freshness_sample"
                                        );
                                        state.mark_logged(now);
                                    }

                                    if let Err(e) =
                                        self.event_tx.send(OracleEvent::Price(price_update)).await
                                    {
                                        error!("Failed to send price update: {}", e);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse SSE update: {}", e);
                        }
                    }
                }
                Ok(SSE::Connected(_)) => {
                    debug!("Hermes SSE connected");
                }
                Ok(SSE::Comment(_)) => {}
                Err(e) => {
                    return Err(anyhow::anyhow!("SSE stream error: {}", e));
                }
                _ => {}
            }
        }
    }

    fn parse_price_update(&self, parsed: ParsedPrice) -> Option<PriceUpdate> {
        // Hermes returns ids as bare lowercase hex; our Asset constants
        // carry a 0x prefix. Normalize before matching.
        let feed_id = if parsed.id.starts_with("0x") {
            parsed.id.to_lowercase()
        } else {
            format!("0x{}", parsed.id.to_lowercase())
        };

        let asset = Asset::from_feed_id(&feed_id)?;
        let expo = parsed.price.expo;
        let raw_price: i64 = parsed.price.price.parse().ok()?;
        let raw_conf: u64 = parsed.price.conf.parse().ok()?;
        let factor = 10f64.powi(expo);

        Some(PriceUpdate {
            symbol: asset.symbol().to_string(),
            price: raw_price as f64 * factor,
            confidence: raw_conf as f64 * factor,
            publish_time: parsed.price.publish_time,
            feed_id,
        })
    }
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn next_backoff_secs(current: u64) -> u64 {
    (current.saturating_mul(2)).min(MAX_RECONNECT_BACKOFF_SECS)
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

    #[test]
    fn parse_price_update_accepts_bare_hex_from_hermes() {
        // Regression: the split dropped the feed-id normalization in
        // parse_price_update, causing every Hermes price update to be
        // silently dropped because Hermes returns bare-hex ids while
        // Asset::feed_id constants carry the 0x prefix.
        let (tx, _rx) = mpsc::channel(1);
        let client = PythClient::new(tx, vec![Asset::Sol]);
        let update = client
            .parse_price_update(ParsedPrice {
                id: "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d"
                    .to_string(),
                price: PriceData {
                    price: "12345".to_string(),
                    conf: "67".to_string(),
                    expo: -2,
                    publish_time: 42,
                },
                ema_price: PriceData {
                    price: "0".to_string(),
                    conf: "0".to_string(),
                    expo: 0,
                    publish_time: 42,
                },
            })
            .expect("bare-hex id from Hermes must resolve to an asset");

        assert_eq!(update.symbol, "SOL");
        assert!(update.feed_id.starts_with("0x"));
    }

    #[test]
    fn freshness_state_marks_publish_time_as_unchanged() {
        let mut state = AssetFreshnessState::default();

        let first = state.observe(100, 101);
        let second = state.observe(100, 102);

        assert!(first.publish_advanced);
        assert_eq!(first.unchanged_streak, 0);
        assert_eq!(first.publish_gap_secs, None);

        assert!(!second.publish_advanced);
        assert_eq!(second.unchanged_streak, 1);
        assert_eq!(second.publish_gap_secs, Some(0));
    }

    #[test]
    fn freshness_state_resets_streak_when_publish_time_advances() {
        let mut state = AssetFreshnessState::default();

        state.observe(100, 101);
        state.observe(100, 102);
        let third = state.observe(101, 103);

        assert!(third.publish_advanced);
        assert_eq!(third.unchanged_streak, 0);
        assert_eq!(third.publish_gap_secs, Some(1));
    }

    #[test]
    fn reconnect_backoff_caps_at_max() {
        assert_eq!(next_backoff_secs(5), 10);
        assert_eq!(next_backoff_secs(10), 20);
        assert_eq!(next_backoff_secs(40), 60);
        assert_eq!(next_backoff_secs(60), 60);
    }

    #[test]
    fn parse_update_scales_price() {
        let (tx, _rx) = mpsc::channel(1);
        let client = PythClient::new(tx, vec![Asset::Sol]);
        let update = client
            .parse_price_update(ParsedPrice {
                id: Asset::Sol.feed_id().to_string(),
                price: PriceData {
                    price: "12345".to_string(),
                    conf: "67".to_string(),
                    expo: -2,
                    publish_time: 42,
                },
                ema_price: PriceData {
                    price: "0".to_string(),
                    conf: "0".to_string(),
                    expo: 0,
                    publish_time: 42,
                },
            })
            .unwrap();

        assert_eq!(update.symbol, "SOL");
        assert!((update.price - 123.45).abs() < f64::EPSILON);
        assert!((update.confidence - 0.67).abs() < f64::EPSILON);
    }
}
