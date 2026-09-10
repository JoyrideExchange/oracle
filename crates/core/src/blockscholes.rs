//! Block Scholes WebSocket client for streaming spot index prices.
//!
//! Endpoint: `wss://prod-websocket-api.blockscholes.com/` (JSON-RPC 2.0).
//! After the WebSocket upgrade the client sends an `authenticate` call
//! carrying the API key, then a single `subscribe` holding one `index.px`
//! spot item per asset. The server pushes `subscription` notifications at
//! the requested `frequency` (a maximum rate; `1000ms` is the fastest the
//! current plan accepts). Subscriptions do not survive a dropped
//! connection, so every reconnect re-authenticates and re-subscribes.
//!
//! Liveness is checked at two levels, mirroring [`crate::pyth`]:
//!
//! - Frame level: outbound pings every [`PING_INTERVAL`] elicit pongs, and
//!   a connection that delivers no frame of any kind within
//!   [`IDLE_TIMEOUT`] is torn down. Railway drops idle-looking flows without
//!   FIN/RST, so without this a half-open socket wedges the client forever
//!   on `stream.next()`.
//! - Payload level: a connection that stays up (pongs keep flowing) but
//!   delivers no parseable price within the stall timeout is torn down.
//!   `OracleEvent::Connected` only fires once a price has actually arrived.
//!
//! Subscribe calls are rate-limited per API key, and the key is shared with
//! other Joyride services. A rejected `subscribe` is treated like any other
//! connection failure: back off (with jitter, per Block Scholes' guidance)
//! and reconnect.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::{debug, error, info, warn};

use crate::types::{Asset, OracleEvent};
use joyride_oracle_wire::PriceUpdate;

/// Production Block Scholes WebSocket endpoint.
pub const BLOCKSCHOLES_WS_URL: &str = "wss://prod-websocket-api.blockscholes.com/";
/// Default subscription `frequency`, in milliseconds. The server accepts
/// only an enumerated set of values per plan (currently 1000, 20000 and
/// 60000); anything else is rejected at subscribe time.
pub const DEFAULT_FREQUENCY_MS: u64 = 1_000;
/// Same identifying User-Agent the Pyth client sends, so upstream logs can
/// attribute our traffic during triage.
const USER_AGENT: &str = "joyride-oracle/1.0 (+ops@joyride.exchange)";
/// Frame-level idle: reconnect if no frame of any kind (data, pong, ping)
/// arrives within this window. Sized above `PING_INTERVAL` so pong replies
/// keep a healthy connection alive even if prices pause.
const IDLE_TIMEOUT: Duration = Duration::from_secs(12);
const PING_INTERVAL: Duration = Duration::from_secs(10);
/// Payload-level liveness: reconnect if no parseable price arrives within
/// this window. Prices normally arrive every second, so 20s bounds a stall
/// to ~1% of a 30-minute TWAP window.
const DEFAULT_PRICE_STALL_TIMEOUT: Duration = Duration::from_secs(20);
const INITIAL_RECONNECT_BACKOFF_SECS: u64 = 5;
const MAX_RECONNECT_BACKOFF_SECS: u64 = 60;
const FRESHNESS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const MAX_RECEIVE_LAG_MS: i64 = 5_000;
const MAX_STALE_STREAK: u32 = 5;
/// Stable batch handle. Re-subscribing with the same `client_id` replaces
/// the batch in place server-side.
const CLIENT_ID: &str = "joyride-oracle-index";
const AUTH_REQUEST_ID: u64 = 1;
const SUBSCRIBE_REQUEST_ID: u64 = 2;
const PRICE_DECIMALS: u32 = 8;

#[derive(Debug, Deserialize)]
struct RpcMessage {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct NotificationParams {
    data: NotificationData,
}

#[derive(Debug, Deserialize)]
struct NotificationData {
    values: Vec<IndexValue>,
    /// Milliseconds, because the subscription requests `"timestamp": "ms"`.
    timestamp: i64,
}

#[derive(Debug, Deserialize)]
struct IndexValue {
    sid: String,
    v: f64,
    /// Index bid/ask spread in quote currency, present because the
    /// subscription sets `index_spread: true`.
    #[serde(default)]
    s: Option<f64>,
}

#[derive(Debug, Default)]
struct AssetFreshnessState {
    prev_timestamp_ms: Option<i64>,
    stale_streak: u32,
    last_log_instant: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
struct FreshnessObservation {
    timestamp_advanced: bool,
    stale_streak: u32,
    receive_lag_ms: i64,
    timestamp_gap_ms: Option<i64>,
}

impl AssetFreshnessState {
    fn observe(&mut self, timestamp_ms: i64, receive_time_ms: i64) -> FreshnessObservation {
        let timestamp_gap_ms = self
            .prev_timestamp_ms
            .map(|prev| timestamp_ms.saturating_sub(prev));
        let timestamp_advanced = self
            .prev_timestamp_ms
            .map(|prev| timestamp_ms > prev)
            .unwrap_or(true);

        if timestamp_advanced {
            self.stale_streak = 0;
        } else {
            self.stale_streak = self.stale_streak.saturating_add(1);
        }
        self.prev_timestamp_ms = Some(timestamp_ms);

        FreshnessObservation {
            timestamp_advanced,
            stale_streak: self.stale_streak,
            receive_lag_ms: receive_time_ms.saturating_sub(timestamp_ms),
            timestamp_gap_ms,
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

/// Client for the Block Scholes `index.px` spot feed.
pub struct BlockScholesClient {
    event_tx: mpsc::Sender<OracleEvent>,
    assets: Vec<Asset>,
    ws_url: String,
    api_key: Option<String>,
    frequency_ms: u64,
    price_stall_timeout: Duration,
    idle_timeout: Duration,
    ping_interval: Duration,
}

impl BlockScholesClient {
    pub fn new(event_tx: mpsc::Sender<OracleEvent>, assets: Vec<Asset>) -> Self {
        Self::with_url(event_tx, assets, BLOCKSCHOLES_WS_URL)
    }

    pub fn with_url(event_tx: mpsc::Sender<OracleEvent>, assets: Vec<Asset>, url: &str) -> Self {
        Self {
            event_tx,
            assets,
            ws_url: url.to_string(),
            api_key: None,
            frequency_ms: DEFAULT_FREQUENCY_MS,
            price_stall_timeout: DEFAULT_PRICE_STALL_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            ping_interval: PING_INTERVAL,
        }
    }

    pub fn with_api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key.filter(|k| !k.trim().is_empty());
        self
    }

    /// Override the subscription `frequency`. Must be a value the account's
    /// plan accepts, or every subscribe is rejected.
    pub fn with_frequency_ms(mut self, frequency_ms: u64) -> Self {
        self.frequency_ms = frequency_ms;
        self
    }

    /// Override the liveness deadlines. Test-only seam; prod should use the
    /// module defaults.
    #[cfg(test)]
    pub(crate) fn with_timeouts(
        mut self,
        price_stall_timeout: Duration,
        idle_timeout: Duration,
        ping_interval: Duration,
    ) -> Self {
        self.price_stall_timeout = price_stall_timeout;
        self.idle_timeout = idle_timeout;
        self.ping_interval = ping_interval;
        self
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        let mut backoff_secs = INITIAL_RECONNECT_BACKOFF_SECS;

        loop {
            let reconnect_reason: String;
            match self.connect_and_stream().await {
                Ok(()) => {
                    reconnect_reason = "stream_closed".to_string();
                    info!("Block Scholes connection closed gracefully");
                    backoff_secs = INITIAL_RECONNECT_BACKOFF_SECS;
                }
                Err(e) => {
                    reconnect_reason = e.to_string();
                    error!("Block Scholes connection error: {}", e);
                    let _ = self
                        .event_tx
                        .send(OracleEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                }
            }

            let _ = self.event_tx.send(OracleEvent::Disconnected).await;
            let delay = jittered_backoff(backoff_secs, jitter_seed());
            info!(
                backoff_ms = delay.as_millis() as u64,
                reconnect_reason = %reconnect_reason,
                "Reconnecting to Block Scholes after backoff"
            );
            tokio::time::sleep(delay).await;
            backoff_secs = next_backoff_secs(backoff_secs);
        }
    }

    async fn connect_and_stream(&mut self) -> anyhow::Result<()> {
        info!(url = %self.ws_url, "Connecting to Block Scholes WebSocket");

        let mut request = self.ws_url.as_str().into_client_request()?;
        request
            .headers_mut()
            .insert("User-Agent", HeaderValue::from_static(USER_AGENT));
        let (ws, _response) = match tokio::time::timeout(
            self.idle_timeout,
            tokio_tungstenite::connect_async(request),
        )
        .await
        {
            Err(_) => anyhow::bail!(
                "Block Scholes WebSocket connect timed out after {}ms",
                self.idle_timeout.as_millis()
            ),
            Ok(Err(tungstenite::Error::Http(response))) => {
                let status = response.status();
                error!(
                    status = %status,
                    "Block Scholes returned non-success status for WebSocket upgrade"
                );
                anyhow::bail!(
                    "Block Scholes rejected WebSocket upgrade: status={}",
                    status
                );
            }
            Ok(Err(e)) => anyhow::bail!("Block Scholes WebSocket connect failed: {}", e),
            Ok(Ok(pair)) => pair,
        };
        let (mut sink, mut stream) = ws.split();

        // ---- 1. Authenticate ----
        sink.send(Message::Text(build_auth_frame(
            self.api_key.as_deref().unwrap_or_default(),
        )))
        .await?;

        // Deadline covers a server (or half-open socket) that accepts the
        // upgrade and then goes silent.
        let auth_deadline = Instant::now() + self.idle_timeout;
        loop {
            let msg = match tokio::time::timeout_at(auth_deadline, stream.next()).await {
                Err(_) => anyhow::bail!(
                    "Block Scholes auth timeout: no response within {}ms",
                    self.idle_timeout.as_millis()
                ),
                Ok(None) => anyhow::bail!("Block Scholes closed the stream before auth response"),
                Ok(Some(msg)) => msg?,
            };
            match msg {
                Message::Text(text) => {
                    let Ok(rpc) = serde_json::from_str::<RpcMessage>(&text) else {
                        continue;
                    };
                    if let Some(err) = rpc.error {
                        anyhow::bail!(
                            "Block Scholes rejected authenticate: code={} {}",
                            err.code,
                            err.message
                        );
                    }
                    if rpc.result.as_ref().and_then(Value::as_str) == Some("ok") {
                        break;
                    }
                }
                Message::Ping(payload) => sink.send(Message::Pong(payload)).await?,
                Message::Close(_) => {
                    anyhow::bail!("Block Scholes closed the connection during auth")
                }
                _ => {}
            }
        }
        debug!("Block Scholes authenticated; subscribing");

        // ---- 2. Subscribe ----
        let symbols: Vec<&str> = self.assets.iter().map(|a| a.symbol()).collect();
        sink.send(Message::Text(build_subscribe_frame(
            &symbols,
            self.frequency_ms,
        )))
        .await?;

        // ---- 3. Stream ----
        // Mirrors the Pyth client's two states: `subscribed` (server acked
        // the subscribe) vs `streaming_live` (first price parsed and
        // emitted). OracleEvent::Connected only fires on the latter.
        let mut subscribed = false;
        let mut streaming_live = false;
        let mut price_deadline = Instant::now() + self.price_stall_timeout;
        let mut last_frame_at = Instant::now();
        let mut freshness_state: HashMap<String, AssetFreshnessState> = HashMap::new();
        let mut ping_ticks = tokio::time::interval(self.ping_interval);
        // Consume the immediate first tick so pings start one interval in.
        ping_ticks.tick().await;

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(last_frame_at + self.idle_timeout) => {
                    warn!(
                        idle_timeout_ms = self.idle_timeout.as_millis() as u64,
                        subscribed,
                        streaming_live,
                        "No frames received from Block Scholes; forcing reconnect"
                    );
                    anyhow::bail!(
                        "Block Scholes WebSocket idle for {}ms",
                        self.idle_timeout.as_millis()
                    );
                }
                _ = tokio::time::sleep_until(price_deadline) => {
                    warn!(
                        stall_timeout_ms = self.price_stall_timeout.as_millis() as u64,
                        subscribed,
                        streaming_live,
                        "No price received within stall timeout; forcing reconnect"
                    );
                    anyhow::bail!(
                        "Block Scholes price stall for {}ms",
                        self.price_stall_timeout.as_millis()
                    );
                }
                _ = ping_ticks.tick() => {
                    sink.send(Message::Ping(Vec::new())).await?;
                }
                frame = stream.next() => {
                    let msg = match frame {
                        None => {
                            info!("Block Scholes WebSocket stream ended");
                            return Ok(());
                        }
                        Some(Err(e)) => anyhow::bail!("Block Scholes WebSocket read error: {}", e),
                        Some(Ok(msg)) => msg,
                    };
                    last_frame_at = Instant::now();

                    let text = match msg {
                        Message::Text(text) => text,
                        Message::Ping(payload) => {
                            sink.send(Message::Pong(payload)).await?;
                            continue;
                        }
                        Message::Close(frame) => {
                            info!(?frame, "Block Scholes closed the connection");
                            return Ok(());
                        }
                        _ => continue,
                    };

                    let rpc = match serde_json::from_str::<RpcMessage>(&text) {
                        Ok(rpc) => rpc,
                        Err(e) => {
                            warn!("Failed to parse Block Scholes frame: {}", e);
                            continue;
                        }
                    };

                    if let Some(err) = rpc.error {
                        // Covers subscribe rejections, including the
                        // per-key rate limit. Reconnecting re-sends the
                        // same three-item subscribe after backoff, which
                        // is what Block Scholes recommends for retries.
                        error!(
                            id = ?rpc.id,
                            code = err.code,
                            message = %err.message,
                            "Block Scholes returned a JSON-RPC error"
                        );
                        anyhow::bail!(
                            "Block Scholes rejected request: code={} {}",
                            err.code,
                            err.message
                        );
                    }

                    if rpc.method.as_deref() != Some("subscription") {
                        if rpc.result.is_some() && !subscribed {
                            debug!("Block Scholes subscription acknowledged; awaiting first price");
                            subscribed = true;
                        }
                        continue;
                    }

                    let Some(params) = rpc.params else {
                        continue;
                    };
                    let notifications: Vec<NotificationParams> = match serde_json::from_value(params) {
                        Ok(n) => n,
                        Err(e) => {
                            warn!("Failed to parse Block Scholes subscription update: {}", e);
                            continue;
                        }
                    };

                    for notification in notifications {
                        let timestamp_ms = notification.data.timestamp;
                        for value in notification.data.values {
                            let Some(price_update) = parse_index_value(&value, timestamp_ms) else {
                                continue;
                            };
                            if !streaming_live {
                                info!("Connected to Block Scholes");
                                let _ = self.event_tx.send(OracleEvent::Connected).await;
                                streaming_live = true;
                            }
                            price_deadline = Instant::now() + self.price_stall_timeout;

                            let receive_time_ms = unix_now_ms();
                            let now = Instant::now();
                            let state = freshness_state
                                .entry(price_update.symbol.clone())
                                .or_default();
                            let observation = state.observe(timestamp_ms, receive_time_ms);
                            let abnormal = observation.receive_lag_ms > MAX_RECEIVE_LAG_MS
                                || observation.stale_streak >= MAX_STALE_STREAK;

                            if abnormal {
                                warn!(
                                    asset = %price_update.symbol,
                                    timestamp_ms,
                                    timestamp_gap_ms = observation.timestamp_gap_ms,
                                    receive_time_ms,
                                    receive_lag_ms = observation.receive_lag_ms,
                                    timestamp_advanced = observation.timestamp_advanced,
                                    stale_streak = observation.stale_streak,
                                    "blockscholes_freshness_abnormal"
                                );
                            } else if state.should_emit_sample(now) {
                                info!(
                                    asset = %price_update.symbol,
                                    timestamp_ms,
                                    timestamp_gap_ms = observation.timestamp_gap_ms,
                                    receive_time_ms,
                                    receive_lag_ms = observation.receive_lag_ms,
                                    timestamp_advanced = observation.timestamp_advanced,
                                    stale_streak = observation.stale_streak,
                                    "blockscholes_freshness_sample"
                                );
                                state.mark_logged(now);
                            }

                            if let Err(e) = self.event_tx.send(OracleEvent::Price(price_update)).await {
                                error!("Failed to send price update: {}", e);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn build_auth_frame(api_key: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": AUTH_REQUEST_ID,
        "method": "authenticate",
        "params": { "api_key": api_key },
    })
    .to_string()
}

fn build_subscribe_frame(symbols: &[&str], frequency_ms: u64) -> String {
    let batch: Vec<Value> = symbols
        .iter()
        .map(|symbol| {
            json!({
                "sid": symbol,
                "feed": "index.px",
                "asset": "spot",
                "base_asset": symbol,
                "quote_asset": "USD",
                "index_spread": true,
            })
        })
        .collect();
    json!({
        "jsonrpc": "2.0",
        "id": SUBSCRIBE_REQUEST_ID,
        "method": "subscribe",
        "params": [{
            "frequency": format!("{frequency_ms}ms"),
            "client_id": CLIENT_ID,
            "batch": batch,
            "options": {
                "format": { "timestamp": "ms", "hexify": false, "decimals": PRICE_DECIMALS },
            },
        }],
    })
    .to_string()
}

/// Map one `index.px` value onto the wire `PriceUpdate`. The sid is the
/// asset symbol (see `build_subscribe_frame`). Confidence is half the index
/// bid/ask spread, in quote currency — far tighter than Pyth's publisher
/// dispersion band, and not comparable to it.
fn parse_index_value(value: &IndexValue, timestamp_ms: i64) -> Option<PriceUpdate> {
    let asset = Asset::from_symbol(&value.sid)?;
    if !value.v.is_finite() || value.v <= 0.0 {
        return None;
    }
    Some(PriceUpdate {
        symbol: asset.symbol().to_string(),
        price: value.v,
        confidence: value
            .s
            .filter(|s| s.is_finite() && *s >= 0.0)
            .unwrap_or(0.0)
            / 2.0,
        publish_time: timestamp_ms.div_euclid(1000),
        feed_id: format!("blockscholes:index.px:{}", asset.symbol()),
    })
}

fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn next_backoff_secs(current: u64) -> u64 {
    (current.saturating_mul(2)).min(MAX_RECONNECT_BACKOFF_SECS)
}

/// Clock-derived jitter source. Only needs to de-synchronise our reconnects
/// from other clients sharing the API key, not be unpredictable.
fn jitter_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64
}

/// `backoff_secs` plus up to 25% random jitter.
fn jittered_backoff(backoff_secs: u64, seed: u64) -> Duration {
    let max_jitter_ms = backoff_secs.saturating_mul(250);
    Duration::from_secs(backoff_secs) + Duration::from_millis(seed % (max_jitter_ms + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;

    const TEST_KEY: &str = "test-key";

    #[test]
    fn asset_resolves_from_symbol() {
        assert_eq!(Asset::from_symbol("BTC"), Some(Asset::Btc));
        assert_eq!(Asset::from_symbol("ETH"), Some(Asset::Eth));
        assert_eq!(Asset::from_symbol("SOL"), Some(Asset::Sol));
        assert_eq!(Asset::from_symbol("btc"), None);
    }

    #[test]
    fn parse_index_value_maps_to_price_update() {
        let update = parse_index_value(
            &IndexValue {
                sid: "BTC".to_string(),
                v: 77237.20105,
                s: Some(0.049),
            },
            1_789_074_000_500,
        )
        .expect("BTC sid must resolve");

        assert_eq!(update.symbol, "BTC");
        assert!((update.price - 77237.20105).abs() < 1e-9);
        assert!((update.confidence - 0.0245).abs() < 1e-12);
        assert_eq!(update.publish_time, 1_789_074_000);
        assert_eq!(update.feed_id, "blockscholes:index.px:BTC");
    }

    #[test]
    fn parse_index_value_defaults_confidence_without_spread() {
        let update = parse_index_value(
            &IndexValue {
                sid: "SOL".to_string(),
                v: 100.05,
                s: None,
            },
            1_000,
        )
        .unwrap();
        assert_eq!(update.confidence, 0.0);
    }

    #[test]
    fn parse_index_value_rejects_unknown_sid_and_bad_prices() {
        let value = |sid: &str, v: f64| IndexValue {
            sid: sid.to_string(),
            v,
            s: None,
        };
        assert!(parse_index_value(&value("DOGE", 1.0), 1_000).is_none());
        assert!(parse_index_value(&value("BTC", 0.0), 1_000).is_none());
        assert!(parse_index_value(&value("BTC", f64::NAN), 1_000).is_none());
    }

    #[test]
    fn subscribe_frame_requests_spot_index_with_spread_for_each_asset() {
        let frame: Value =
            serde_json::from_str(&build_subscribe_frame(&["SOL", "BTC", "ETH"], 1_000)).unwrap();
        let params = &frame["params"][0];
        assert_eq!(frame["method"], "subscribe");
        assert_eq!(params["frequency"], "1000ms");
        assert_eq!(params["client_id"], CLIENT_ID);
        assert_eq!(params["options"]["format"]["timestamp"], "ms");
        let batch = params["batch"].as_array().unwrap();
        assert_eq!(batch.len(), 3);
        for item in batch {
            assert_eq!(item["feed"], "index.px");
            assert_eq!(item["asset"], "spot");
            assert_eq!(item["index_spread"], true);
            assert_eq!(item["sid"], item["base_asset"]);
        }
    }

    #[test]
    fn freshness_state_tracks_stale_timestamps_in_ms() {
        let mut state = AssetFreshnessState::default();

        let first = state.observe(1_000, 1_015);
        let second = state.observe(1_000, 2_015);
        let third = state.observe(2_000, 2_020);

        assert!(first.timestamp_advanced);
        assert_eq!(first.receive_lag_ms, 15);
        assert_eq!(first.timestamp_gap_ms, None);

        assert!(!second.timestamp_advanced);
        assert_eq!(second.stale_streak, 1);
        assert_eq!(second.timestamp_gap_ms, Some(0));

        assert!(third.timestamp_advanced);
        assert_eq!(third.stale_streak, 0);
        assert_eq!(third.timestamp_gap_ms, Some(1_000));
    }

    #[test]
    fn reconnect_backoff_caps_at_max() {
        assert_eq!(next_backoff_secs(5), 10);
        assert_eq!(next_backoff_secs(40), 60);
        assert_eq!(next_backoff_secs(60), 60);
    }

    #[test]
    fn jittered_backoff_stays_within_25_percent() {
        assert_eq!(jittered_backoff(5, 0), Duration::from_secs(5));
        assert_eq!(jittered_backoff(5, 1_250), Duration::from_millis(6_250));
        for seed in [1, 999, 123_456_789, u32::MAX as u64] {
            let d = jittered_backoff(60, seed);
            assert!(
                d >= Duration::from_secs(60) && d <= Duration::from_secs(75),
                "{d:?}"
            );
        }
    }

    // ---- Mock-server tests ----

    type ServerWs = WebSocketStream<TcpStream>;

    /// Serve exactly one WebSocket connection with `script`, returning the
    /// `ws://` URL to point the client at.
    async fn spawn_mock<F, Fut>(script: F) -> String
    where
        F: FnOnce(ServerWs) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            script(ws).await;
        });
        format!("ws://{addr}/")
    }

    async fn recv_json(ws: &mut ServerWs) -> Value {
        loop {
            match ws.next().await.expect("client hung up").unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                _ => continue,
            }
        }
    }

    async fn send_json(ws: &mut ServerWs, value: Value) {
        ws.send(Message::Text(value.to_string())).await.unwrap();
    }

    /// Accept auth and subscribe, asserting the client sent our key.
    async fn handshake(ws: &mut ServerWs) {
        let auth = recv_json(ws).await;
        assert_eq!(auth["method"], "authenticate");
        assert_eq!(auth["params"]["api_key"], TEST_KEY);
        send_json(
            ws,
            json!({"jsonrpc": "2.0", "result": "ok", "id": auth["id"]}),
        )
        .await;

        let sub = recv_json(ws).await;
        assert_eq!(sub["method"], "subscribe");
        send_json(
            ws,
            json!({"jsonrpc": "2.0", "result": [{"batch": sub["params"][0]}], "id": sub["id"]}),
        )
        .await;
    }

    fn client(url: &str) -> (BlockScholesClient, mpsc::Receiver<OracleEvent>) {
        let (tx, rx) = mpsc::channel(32);
        let client = BlockScholesClient::with_url(tx, vec![Asset::Btc, Asset::Eth], url)
            .with_api_key(Some(TEST_KEY.to_string()));
        (client, rx)
    }

    fn drain(rx: &mut mpsc::Receiver<OracleEvent>) -> Vec<OracleEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn streams_prices_after_auth_and_subscribe() {
        let url = spawn_mock(|mut ws| async move {
            handshake(&mut ws).await;
            send_json(
                &mut ws,
                json!({
                    "jsonrpc": "2.0",
                    "method": "subscription",
                    "params": [{
                        "data": {
                            "values": [
                                {"sid": "BTC", "v": 77237.2, "s": 0.049},
                                {"sid": "ETH", "v": 2461.177, "s": 0.0275},
                            ],
                            "timestamp": 1_789_074_000_000_i64,
                        },
                        "client_id": CLIENT_ID,
                    }],
                }),
            )
            .await;
            ws.close(None).await.unwrap();
        })
        .await;

        let (mut client, mut rx) = client(&url);
        client.connect_and_stream().await.expect("graceful close");

        let events = drain(&mut rx);
        assert!(matches!(events[0], OracleEvent::Connected), "{events:?}");
        let prices: Vec<&PriceUpdate> = events
            .iter()
            .filter_map(|e| match e {
                OracleEvent::Price(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(prices.len(), 2);
        assert_eq!(prices[0].symbol, "BTC");
        assert_eq!(prices[0].publish_time, 1_789_074_000);
        assert!((prices[0].confidence - 0.0245).abs() < 1e-12);
        assert_eq!(prices[1].symbol, "ETH");
    }

    #[tokio::test]
    async fn authenticate_rejection_is_a_connection_error() {
        let url = spawn_mock(|mut ws| async move {
            let auth = recv_json(&mut ws).await;
            send_json(
                &mut ws,
                json!({"jsonrpc": "2.0", "error": {"message": "Invalid API Key", "code": -2610}, "id": auth["id"]}),
            )
            .await;
        })
        .await;

        let (mut client, mut rx) = client(&url);
        let err = client.connect_and_stream().await.unwrap_err().to_string();
        assert!(err.contains("Invalid API Key"), "{err}");
        assert!(drain(&mut rx).is_empty());
    }

    /// A rejected subscribe — e.g. the per-key rate limit, or a frequency
    /// the plan doesn't allow — must fail the connection so `run` backs off
    /// and retries, rather than sitting on a live socket with no prices.
    #[tokio::test]
    async fn subscribe_rejection_is_a_connection_error() {
        let url = spawn_mock(|mut ws| async move {
            let auth = recv_json(&mut ws).await;
            send_json(&mut ws, json!({"jsonrpc": "2.0", "result": "ok", "id": auth["id"]})).await;
            let sub = recv_json(&mut ws).await;
            send_json(
                &mut ws,
                json!({"jsonrpc": "2.0", "error": {"message": "Invalid parameters: 'frequency': Value error, invalid frequency: 500ms", "code": -2610}, "id": sub["id"]}),
            )
            .await;
            // Hold the socket open: the client must fail on the error
            // frame, not on a close.
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .await;

        let (mut client, mut rx) = client(&url);
        let err = tokio::time::timeout(Duration::from_secs(2), client.connect_and_stream())
            .await
            .expect("must fail promptly on the error frame")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid frequency"), "{err}");
        assert!(drain(&mut rx).is_empty());
    }

    /// The 2026-04-24 Hermes outage shape: the connection is healthy at
    /// the transport level but no prices arrive. Connected must not fire
    /// and the stall watchdog must force a reconnect.
    #[tokio::test]
    async fn price_stall_forces_reconnect_without_connected_event() {
        let url = spawn_mock(|mut ws| async move {
            handshake(&mut ws).await;
            // Keep reading so pings are answered and the frame-level idle
            // timer stays satisfied; send no prices.
            while ws.next().await.is_some() {}
        })
        .await;

        let (client, mut rx) = client(&url);
        let mut client = client.with_timeouts(
            Duration::from_millis(300),
            Duration::from_secs(5),
            Duration::from_millis(100),
        );
        let err = client.connect_and_stream().await.unwrap_err().to_string();
        assert!(err.contains("price stall"), "{err}");
        assert!(drain(&mut rx).is_empty());
    }

    /// Half-open connection: after the handshake the peer stops reading and
    /// never answers pings. The frame-level idle timeout must fire even
    /// though the price stall deadline is far away.
    #[tokio::test]
    async fn idle_timeout_detects_silent_peer() {
        let url = spawn_mock(|mut ws| async move {
            handshake(&mut ws).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(ws);
        })
        .await;

        let (client, _rx) = client(&url);
        let mut client = client.with_timeouts(
            Duration::from_secs(5),
            Duration::from_millis(300),
            Duration::from_millis(100),
        );
        let err = tokio::time::timeout(Duration::from_secs(2), client.connect_and_stream())
            .await
            .expect("idle timeout must fire")
            .unwrap_err()
            .to_string();
        assert!(err.contains("idle"), "{err}");
    }

    /// Pin the User-Agent on the WebSocket upgrade, matching the Pyth
    /// client, so upstream logs can identify our traffic.
    #[tokio::test]
    // The handshake callback's error type is fixed by tungstenite.
    #[allow(clippy::result_large_err)]
    async fn upgrade_sends_joyride_user_agent_header() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ua_tx, ua_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ua_tx = Some(ua_tx);
            let _ws = tokio_tungstenite::accept_hdr_async(
                stream,
                |req: &tungstenite::handshake::server::Request,
                 resp: tungstenite::handshake::server::Response| {
                    let ua = req
                        .headers()
                        .get("User-Agent")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let _ = ua_tx.take().unwrap().send(ua);
                    Ok(resp)
                },
            )
            .await;
        });

        let (client, _rx) = client(&format!("ws://{addr}/"));
        let mut client = client.with_timeouts(
            Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_millis(100),
        );
        let _ = client.connect_and_stream().await;
        assert_eq!(ua_rx.await.unwrap().as_deref(), Some(USER_AGENT));
    }
}
