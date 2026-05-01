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
/// Explicit User-Agent so Hermes/Cloudflare logs identify us, and so we
/// don't rely on whatever default the HTTP client sends (the eventsource
/// crate sets only Accept + Cache-Control).
const USER_AGENT: &str = "joyride-oracle/1.0 (+ops@joyride.exchange)";
const FRESHNESS_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// Frame-level idle: reconnect if no SSE frame of any kind arrives within
/// this window. Comments (heartbeats) and malformed events both reset it,
/// so this alone does NOT protect against "connection stays open but no
/// prices flow" — that's what `DEFAULT_PRICE_STALL_TIMEOUT` is for.
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Payload-level liveness: reconnect if no parseable `PriceUpdate`
/// arrives within this window, even if heartbeats or garbage frames
/// keep resetting the frame-level timer. Anchors "connected" to actual
/// market-data flow instead of wire-level activity.
const DEFAULT_PRICE_STALL_TIMEOUT: Duration = Duration::from_secs(60);
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
    price_stall_timeout: Duration,
}

impl PythClient {
    pub fn new(event_tx: mpsc::Sender<OracleEvent>, assets: Vec<Asset>) -> Self {
        Self {
            event_tx,
            assets,
            hermes_url: HERMES_URL.to_string(),
            price_stall_timeout: DEFAULT_PRICE_STALL_TIMEOUT,
        }
    }

    pub fn with_url(event_tx: mpsc::Sender<OracleEvent>, assets: Vec<Asset>, url: &str) -> Self {
        Self {
            event_tx,
            assets,
            hermes_url: url.to_string(),
            price_stall_timeout: DEFAULT_PRICE_STALL_TIMEOUT,
        }
    }

    /// Override the payload-level liveness deadline. Test-only seam; prod
    /// should use `DEFAULT_PRICE_STALL_TIMEOUT`.
    #[cfg(test)]
    pub(crate) fn with_price_stall_timeout(mut self, timeout: Duration) -> Self {
        self.price_stall_timeout = timeout;
        self
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

        // Same UA + explicit non-success handling as the SSE path, so both
        // code paths get the same triage shape. `reqwest::get` uses a
        // default, unconfigured client, which is what we want to avoid.
        let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
        let response = client.get(&url).send().await?;
        let status = response.status();
        if !status.is_success() {
            error!(
                status = %status,
                "Pyth Hermes returned non-success status for /latest request"
            );
            anyhow::bail!("Pyth Hermes rejected /latest request: status={}", status);
        }
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

        let client = eventsource_client::ClientBuilder::for_url(&url)?
            .header("User-Agent", USER_AGENT)?
            .build();
        let mut stream = client.stream();
        let mut freshness_state: HashMap<String, AssetFreshnessState> = HashMap::new();
        // Two distinct states. The 2026-04-24 outage shape was precisely
        // "HTTP OK, then zero payloads" — conflating the two would make
        // "Connected" log lines lie about whether market data is flowing.
        //   handshake_ok   => SSE::Connected fired (server returned 2xx)
        //   streaming_live => first PriceUpdate parsed and emitted
        // OracleEvent::Connected only fires on streaming_live.
        let mut handshake_ok = false;
        let mut streaming_live = false;
        // Payload-level watchdog. Armed on SSE::Connected and reset on
        // every emitted PriceUpdate. If heartbeats or malformed events
        // keep the frame timer alive but no real prices ever parse out,
        // this deadline fires and forces a reconnect.
        let mut price_deadline: Option<Instant> = None;

        loop {
            if let Some(deadline) = price_deadline {
                if Instant::now() >= deadline {
                    warn!(
                        stall_timeout_secs = self.price_stall_timeout.as_secs(),
                        handshake_ok,
                        streaming_live,
                        "No PriceUpdate received within stall timeout; forcing reconnect"
                    );
                    return Err(anyhow::anyhow!(
                        "Pyth Hermes PriceUpdate stall for {}s",
                        self.price_stall_timeout.as_secs()
                    ));
                }
            }

            let event = match tokio::time::timeout(SSE_IDLE_TIMEOUT, stream.next()).await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    info!("Pyth Hermes SSE stream ended");
                    return Ok(());
                }
                Err(_) => {
                    warn!(
                        idle_timeout_secs = SSE_IDLE_TIMEOUT.as_secs(),
                        handshake_ok,
                        streaming_live,
                        "No SSE events received from Hermes; forcing reconnect"
                    );
                    return Err(anyhow::anyhow!(
                        "Pyth Hermes SSE idle for {}s",
                        SSE_IDLE_TIMEOUT.as_secs()
                    ));
                }
            };

            match event {
                Ok(SSE::Connected(_)) => {
                    // HTTP 2xx received. Do NOT announce "Connected to Pyth
                    // Hermes" here — in the failure mode we care about,
                    // the server 200s and then sends nothing.
                    if !handshake_ok {
                        debug!("Pyth Hermes HTTP handshake accepted; awaiting first payload");
                        handshake_ok = true;
                        price_deadline = Some(Instant::now() + self.price_stall_timeout);
                    }
                }
                Ok(SSE::Event(ev)) if ev.event_type == "message" => {
                    match serde_json::from_str::<StreamUpdate>(&ev.data) {
                        Ok(update) => {
                            for parsed in update.parsed {
                                if let Some(price_update) = self.parse_price_update(parsed) {
                                    if !streaming_live {
                                        // First real payload of this connection.
                                        // "Connected" now means market data is
                                        // actually flowing, not just that the
                                        // HTTP handshake succeeded.
                                        info!("Connected to Pyth Hermes");
                                        let _ =
                                            self.event_tx.send(OracleEvent::Connected).await;
                                        streaming_live = true;
                                    }
                                    // Reset the payload-level watchdog on
                                    // every successful PriceUpdate, so
                                    // heartbeats + garbage frames cannot
                                    // hold the connection open forever.
                                    price_deadline =
                                        Some(Instant::now() + self.price_stall_timeout);

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
                Ok(SSE::Comment(_)) => {}
                Err(eventsource_client::Error::UnexpectedResponse(resp, _)) => {
                    // Explicit non-2xx from Hermes. Log status as a
                    // structured field so 403/429/5xx is grep-able and
                    // distinct from the idle-timeout path.
                    let status = resp.status();
                    error!(
                        status = %status,
                        "Pyth Hermes returned non-success status for SSE request"
                    );
                    return Err(anyhow::anyhow!(
                        "Pyth Hermes rejected SSE request: status={}",
                        status
                    ));
                }
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

    /// Pin the User-Agent we send to Hermes. Originally added after a
    /// 2026-04-24 incident where the oracle stalled for ~90 minutes and
    /// identifying our traffic in upstream logs would have shortened
    /// triage. An explicit UA also insulates us from any bot-detection
    /// heuristic that keys on the absence of one.
    #[tokio::test]
    async fn connect_sends_joyride_user_agent_header() {
        use httpmock::prelude::*;

        let server = MockServer::start_async().await;
        let ua_mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/v2/updates/price/stream")
                    .header_exists("user-agent")
                    .matches(|req| {
                        req.headers
                            .as_ref()
                            .and_then(|hs| {
                                hs.iter()
                                    .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
                                    .map(|(_, v)| v.contains("joyride-oracle"))
                            })
                            .unwrap_or(false)
                    });
                then.status(200)
                    .header("content-type", "text/event-stream")
                    .body("");
            })
            .await;

        let (tx, _rx) = mpsc::channel(16);
        let mut client = PythClient::with_url(tx, vec![Asset::Btc], &server.base_url());

        // run() loops on reconnect forever; cap it so the test can observe
        // at least one request reaching the mock and then return.
        let _ = tokio::time::timeout(Duration::from_secs(2), client.run()).await;

        ua_mock.assert_async().await;
    }

    fn sse_price_event_body(feed_id: &str) -> String {
        // Minimal parseable SSE "message" event with one parsed price.
        // expo=-8 → price field is raw × 10^-8. 10_000_000_000 ⇒ $100.
        let payload = serde_json::json!({
            "parsed": [{
                "id": feed_id,
                "price": {"price": "10000000000", "conf": "1000000", "expo": -8, "publish_time": 1000},
                "ema_price": {"price": "10000000000", "conf": "1000000", "expo": -8, "publish_time": 1000}
            }]
        });
        format!("event: message\ndata: {}\n\n", payload)
    }

    /// Failure-mode pin. The 2026-04-24 outage shape was "HTTP 200, then
    /// zero SSE payloads for 30s." The oracle must NOT emit
    /// OracleEvent::Connected in that case — that signal is now reserved
    /// for "we actually received market data."
    #[tokio::test]
    async fn connected_event_does_not_fire_on_silent_200_from_hermes() {
        use httpmock::prelude::*;

        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/v2/updates/price/stream");
                then.status(200)
                    .header("content-type", "text/event-stream")
                    .body("");
            })
            .await;

        let (tx, mut rx) = mpsc::channel(16);
        let mut client = PythClient::with_url(tx, vec![Asset::Btc], &server.base_url());

        let _ = tokio::time::timeout(Duration::from_millis(500), client.run()).await;

        let mut saw_connected = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, OracleEvent::Connected) {
                saw_connected = true;
            }
        }
        assert!(
            !saw_connected,
            "OracleEvent::Connected must not fire on HTTP-OK + empty body \
             (handshake without any payload)"
        );
    }

    /// Happy-path pin. Once a real price event parses out of the stream,
    /// OracleEvent::Connected should fire, and it should come at or
    /// before the first Price event so downstream consumers can treat
    /// "Connected" as a reliable precondition.
    #[tokio::test]
    async fn connected_event_fires_on_first_parseable_price_payload() {
        use httpmock::prelude::*;

        let feed_id = Asset::Btc.feed_id();
        let body = sse_price_event_body(feed_id);

        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/v2/updates/price/stream");
                then.status(200)
                    .header("content-type", "text/event-stream")
                    .body(&body);
            })
            .await;

        let (tx, mut rx) = mpsc::channel(64);
        let mut client = PythClient::with_url(tx, vec![Asset::Btc], &server.base_url());

        let _ = tokio::time::timeout(Duration::from_secs(1), client.run()).await;

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        let connected_idx = events
            .iter()
            .position(|e| matches!(e, OracleEvent::Connected));
        let first_price_idx = events
            .iter()
            .position(|e| matches!(e, OracleEvent::Price(_)));

        let connected_idx = connected_idx
            .expect("OracleEvent::Connected must fire after the first parseable SSE payload");
        let first_price_idx =
            first_price_idx.expect("OracleEvent::Price must fire for the payload");
        assert!(
            connected_idx <= first_price_idx,
            "Connected ({connected_idx}) must precede or coincide with first Price ({first_price_idx})"
        );
    }

    /// Mirror of the SSE UA pin for the REST `/latest` path. Same crime,
    /// same fix — keep both paths in sync.
    #[tokio::test]
    async fn fetch_latest_sends_joyride_user_agent_header() {
        use httpmock::prelude::*;

        let feed_id = Asset::Btc.feed_id();
        let body = serde_json::json!({
            "parsed": [{
                "id": feed_id,
                "price": {"price": "10000000000", "conf": "1000000", "expo": -8, "publish_time": 1000},
                "ema_price": {"price": "10000000000", "conf": "1000000", "expo": -8, "publish_time": 1000}
            }]
        });

        let server = MockServer::start_async().await;
        let ua_mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/v2/updates/price/latest")
                    .matches(|req| {
                        req.headers
                            .as_ref()
                            .and_then(|hs| {
                                hs.iter()
                                    .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
                                    .map(|(_, v)| v.contains("joyride-oracle"))
                            })
                            .unwrap_or(false)
                    });
                then.status(200)
                    .header("content-type", "application/json")
                    .body(body.to_string());
            })
            .await;

        let (tx, _rx) = mpsc::channel(16);
        let client = PythClient::with_url(tx, vec![Asset::Btc], &server.base_url());

        let updates = client
            .fetch_latest()
            .await
            .expect("fetch_latest should succeed against mock");
        assert_eq!(updates.len(), 1);
        ua_mock.assert_async().await;
    }

    /// On a non-2xx from `/latest`, surface an error whose message includes
    /// the status code, so triage can grep for the exact status.
    #[tokio::test]
    async fn fetch_latest_errors_on_non_success_status() {
        use httpmock::prelude::*;

        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/v2/updates/price/latest");
                then.status(429).body("Too Many Requests");
            })
            .await;

        let (tx, _rx) = mpsc::channel(16);
        let client = PythClient::with_url(tx, vec![Asset::Btc], &server.base_url());

        let err = client
            .fetch_latest()
            .await
            .expect_err("fetch_latest must error on non-2xx");
        let msg = err.to_string();
        assert!(
            msg.contains("429"),
            "error message must include the status code: {msg}"
        );
    }

    /// Heartbeats-forever fake SSE server. httpmock can't hold a streaming
    /// body open, so we do it by hand: accept one connection, write
    /// HTTP/200 headers, drip SSE `: heartbeat` comments every 10 ms until
    /// the client disconnects. Used by the payload-level stall test below
    /// to simulate "connection stays open but no prices flow."
    async fn spawn_heartbeat_only_sse_server() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                tokio::spawn(async move {
                    // Consume the request line + headers (until \r\n\r\n)
                    // so the client side of the request is actually read.
                    let mut buf = [0u8; 1024];
                    use tokio::io::AsyncReadExt;
                    let _ = socket.read(&mut buf).await;

                    let headers = b"HTTP/1.1 200 OK\r\n\
                                    content-type: text/event-stream\r\n\
                                    cache-control: no-cache\r\n\
                                    connection: keep-alive\r\n\
                                    \r\n";
                    if socket.write_all(headers).await.is_err() {
                        return;
                    }
                    loop {
                        if socket.write_all(b": heartbeat\n\n").await.is_err() {
                            return;
                        }
                        let _ = socket.flush().await;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                });
            }
        });
        (base_url, handle)
    }

    /// Pins the gap the frame-level idle timer doesn't cover: heartbeats
    /// reset SSE_IDLE_TIMEOUT, so without a payload-level deadline the
    /// oracle could stay "connected" forever while the order book is
    /// empty.
    #[tokio::test]
    async fn stall_watchdog_forces_reconnect_when_only_heartbeats_arrive() {
        let (url, server_handle) = spawn_heartbeat_only_sse_server().await;

        let (tx, mut rx) = mpsc::channel(64);
        let mut client = PythClient::with_url(tx, vec![Asset::Btc], &url)
            .with_price_stall_timeout(Duration::from_millis(200));

        // 1.5 s covers ~50 ms handshake + 200 ms stall deadline + room
        // for the reconnect path to fire OracleEvent::Error before the
        // outer timeout kicks in. The 5 s reconnect backoff is capped
        // off by our timeout first.
        let _ = tokio::time::timeout(Duration::from_millis(1500), client.run()).await;
        server_handle.abort();

        let mut saw_error_with_stall = false;
        let mut saw_connected = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                OracleEvent::Error { message } if message.contains("PriceUpdate stall") => {
                    saw_error_with_stall = true;
                }
                OracleEvent::Connected => {
                    saw_connected = true;
                }
                _ => {}
            }
        }

        assert!(
            saw_error_with_stall,
            "payload-level stall watchdog must fire when heartbeats \
             keep the frame-level timer reset but no PriceUpdate arrives"
        );
        assert!(
            !saw_connected,
            "OracleEvent::Connected must not fire when only heartbeats arrive"
        );
    }
}
