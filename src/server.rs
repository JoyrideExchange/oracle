//! WebSocket server for broadcasting oracle data to dashboard clients.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use futures_util::{Sink, SinkExt, StreamExt};
use serde::Serialize;
use socket2::SockRef;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, RwLock};
use tokio::time::Instant;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{handshake::server::Request, Message},
};
use tracing::{error, info, warn};

use crate::types::{OracleEvent, PriceUpdate, TwapPreview};

#[derive(Serialize)]
struct Envelope<'a, T: Serialize> {
    timestamp: String,
    #[serde(flatten)]
    event: &'a T,
}

fn iso_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn serialize_json<T: Serialize>(event: &T) -> serde_json::Result<String> {
    serde_json::to_string(&Envelope {
        timestamp: iso_timestamp(),
        event,
    })
}

#[derive(Serialize)]
struct Heartbeat {
    #[serde(rename = "type")]
    kind: &'static str,
}

fn heartbeat_json() -> String {
    serialize_json(&Heartbeat { kind: "heartbeat" })
        .expect("heartbeat serialization is infallible")
}
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const FANOUT_HEALTH_INTERVAL: Duration = Duration::from_secs(30);
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(10);
const ORDERED_CLIENT_BUFFER: usize = 4096;
const PREVIEW_CLIENT_BUFFER: usize = 2048;

fn tcp_keepalive() -> socket2::TcpKeepalive {
    socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10))
}

#[derive(Clone, Debug)]
struct ClientOptions {
    client_name: String,
    include_previews: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            client_name: "unknown".to_string(),
            include_previews: true,
        }
    }
}

#[derive(Clone, Default)]
struct ServerState {
    latest_prices: Arc<RwLock<HashMap<String, PriceUpdate>>>,
    latest_previews: Arc<RwLock<HashMap<String, TwapPreview>>>,
    metrics: Arc<DeliveryMetrics>,
}

#[derive(Default)]
struct DeliveryMetrics {
    active_clients: AtomicUsize,
    next_connection_id: AtomicU64,
    ordered_events_broadcast: AtomicU64,
    preview_events_broadcast: AtomicU64,
    ordered_fanout_lagged: AtomicU64,
    preview_fanout_lagged: AtomicU64,
    ordered_client_lagged: AtomicU64,
    preview_client_lagged: AtomicU64,
    send_failures: AtomicU64,
    send_timeouts: AtomicU64,
}

struct ActiveClientGuard {
    metrics: Arc<DeliveryMetrics>,
}

impl ActiveClientGuard {
    fn new(metrics: Arc<DeliveryMetrics>) -> Self {
        metrics.active_clients.fetch_add(1, Ordering::Relaxed);
        Self { metrics }
    }
}

impl Drop for ActiveClientGuard {
    fn drop(&mut self) {
        self.metrics.active_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct ClientStats {
    ordered_events_sent: u64,
    preview_events_sent: u64,
    heartbeats_sent: u64,
    snapshot_prices_sent: u64,
    snapshot_previews_sent: u64,
    messages_sent: u64,
    client_pings_received: u64,
    ordered_lagged: u64,
    preview_lagged: u64,
    last_btc_publish_time: Option<i64>,
    last_eth_publish_time: Option<i64>,
    last_sol_publish_time: Option<i64>,
}

#[derive(Default)]
struct PublishTimeSnapshot {
    btc: Option<i64>,
    eth: Option<i64>,
    sol: Option<i64>,
}

impl ServerState {
    async fn cache_ordered_event(&self, event: &OracleEvent) {
        if let OracleEvent::Price(update) = event {
            self.latest_prices
                .write()
                .await
                .insert(update.symbol.clone(), update.clone());
        }
    }

    async fn cache_preview(&self, preview: &TwapPreview) {
        self.latest_previews
            .write()
            .await
            .insert(preview.symbol.clone(), preview.clone());
    }

    async fn snapshot_prices(&self) -> Vec<PriceUpdate> {
        let prices = self.latest_prices.read().await;
        let mut snapshot: Vec<_> = prices.values().cloned().collect();
        snapshot.sort_by(|left, right| left.symbol.cmp(&right.symbol));
        snapshot
    }

    async fn snapshot_previews(&self) -> Vec<TwapPreview> {
        let previews = self.latest_previews.read().await;
        let mut snapshot: Vec<_> = previews.values().cloned().collect();
        snapshot.sort_by(|left, right| left.symbol.cmp(&right.symbol));
        snapshot
    }

    async fn publish_time_snapshot(&self) -> (PublishTimeSnapshot, usize, usize) {
        // Acquire and release each lock sequentially to avoid holding two
        // RwLock read guards simultaneously (prevents lock-ordering issues).
        let (snapshot, price_count) = {
            let prices = self.latest_prices.read().await;
            let s = PublishTimeSnapshot {
                btc: prices.get("BTC").map(|update| update.publish_time),
                eth: prices.get("ETH").map(|update| update.publish_time),
                sol: prices.get("SOL").map(|update| update.publish_time),
            };
            (s, prices.len())
        };
        let preview_count = self.latest_previews.read().await.len();

        (snapshot, price_count, preview_count)
    }
}

impl ClientStats {
    fn record_ordered_event(&mut self, event: &OracleEvent) {
        self.ordered_events_sent = self.ordered_events_sent.saturating_add(1);

        if let OracleEvent::Price(update) = event {
            self.record_price_update(update);
        }
    }

    fn record_preview(&mut self) {
        self.preview_events_sent = self.preview_events_sent.saturating_add(1);
    }

    fn record_snapshot_price(&mut self, update: &PriceUpdate) {
        self.snapshot_prices_sent = self.snapshot_prices_sent.saturating_add(1);
        self.ordered_events_sent = self.ordered_events_sent.saturating_add(1);
        self.record_price_update(update);
    }

    fn record_snapshot_preview(&mut self) {
        self.snapshot_previews_sent = self.snapshot_previews_sent.saturating_add(1);
        self.preview_events_sent = self.preview_events_sent.saturating_add(1);
    }

    fn record_message_sent(&mut self) {
        self.messages_sent = self.messages_sent.saturating_add(1);
    }

    fn record_client_ping(&mut self) {
        self.client_pings_received = self.client_pings_received.saturating_add(1);
    }

    fn record_price_update(&mut self, update: &PriceUpdate) {
        match update.symbol.as_str() {
            "BTC" => self.last_btc_publish_time = Some(update.publish_time),
            "ETH" => self.last_eth_publish_time = Some(update.publish_time),
            "SOL" => self.last_sol_publish_time = Some(update.publish_time),
            _ => {}
        }
    }
}

/// Run the WebSocket server for broadcasting oracle events.
pub async fn run_server(
    addr: &str,
    mut ordered_rx: broadcast::Receiver<OracleEvent>,
    mut preview_rx: broadcast::Receiver<TwapPreview>,
) {
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind server to {}: {}", addr, e);
            return;
        }
    };

    info!("Oracle WebSocket server listening on {}", addr);

    let state = ServerState::default();
    let (ordered_client_tx, _) = broadcast::channel::<OracleEvent>(ORDERED_CLIENT_BUFFER);
    let ordered_client_tx_clone = ordered_client_tx.clone();
    let ordered_state = state.clone();
    let (preview_client_tx, _) = broadcast::channel::<TwapPreview>(PREVIEW_CLIENT_BUFFER);
    let preview_client_tx_clone = preview_client_tx.clone();
    let preview_state = state.clone();
    let health_state = state.clone();
    let health_ordered_tx = ordered_client_tx.clone();
    let health_preview_tx = preview_client_tx.clone();

    tokio::spawn(async move {
        loop {
            match ordered_rx.recv().await {
                Ok(event) => {
                    ordered_state.cache_ordered_event(&event).await;
                    ordered_state
                        .metrics
                        .ordered_events_broadcast
                        .fetch_add(1, Ordering::Relaxed);
                    let _ = ordered_client_tx_clone.send(event);
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    ordered_state
                        .metrics
                        .ordered_fanout_lagged
                        .fetch_add(n, Ordering::Relaxed);
                    warn!(
                        dropped = n,
                        "oracle ordered fanout lagged; dropped ordered events"
                    );
                }
            }
        }
    });

    tokio::spawn(async move {
        loop {
            match preview_rx.recv().await {
                Ok(preview) => {
                    preview_state.cache_preview(&preview).await;
                    preview_state
                        .metrics
                        .preview_events_broadcast
                        .fetch_add(1, Ordering::Relaxed);
                    let _ = preview_client_tx_clone.send(preview);
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    preview_state
                        .metrics
                        .preview_fanout_lagged
                        .fetch_add(n, Ordering::Relaxed);
                    warn!(
                        dropped = n,
                        "oracle preview fanout lagged; dropped preview updates"
                    );
                }
            }
        }
    });

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(FANOUT_HEALTH_INTERVAL);

        loop {
            interval.tick().await;

            let (publish_times, cached_prices, cached_previews) =
                health_state.publish_time_snapshot().await;

            info!(
                active_clients = health_state.metrics.active_clients.load(Ordering::Relaxed),
                ordered_receivers = health_ordered_tx.receiver_count(),
                preview_receivers = health_preview_tx.receiver_count(),
                ordered_events_broadcast = health_state
                    .metrics
                    .ordered_events_broadcast
                    .swap(0, Ordering::Relaxed),
                preview_events_broadcast = health_state
                    .metrics
                    .preview_events_broadcast
                    .swap(0, Ordering::Relaxed),
                ordered_fanout_lagged_total = health_state
                    .metrics
                    .ordered_fanout_lagged
                    .load(Ordering::Relaxed),
                preview_fanout_lagged_total = health_state
                    .metrics
                    .preview_fanout_lagged
                    .load(Ordering::Relaxed),
                ordered_client_lagged_total = health_state
                    .metrics
                    .ordered_client_lagged
                    .load(Ordering::Relaxed),
                preview_client_lagged_total = health_state
                    .metrics
                    .preview_client_lagged
                    .load(Ordering::Relaxed),
                send_failures_total = health_state.metrics.send_failures.load(Ordering::Relaxed),
                send_timeouts_total = health_state.metrics.send_timeouts.load(Ordering::Relaxed),
                cached_prices,
                cached_previews,
                last_btc_publish_time = publish_times.btc,
                last_eth_publish_time = publish_times.eth,
                last_sol_publish_time = publish_times.sol,
                "oracle_fanout_health"
            );
        }
    });

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to accept connection: {}", e);
                continue;
            }
        };

        // Enable TCP keepalive to detect dead connections faster on Railway
        // private networking, which resets connections after ~100s of silence
        // at the TCP layer.
        if let Err(e) = SockRef::from(&stream).set_tcp_keepalive(&tcp_keepalive()) {
            warn!(client = %peer_addr, error = %e, "Failed to set TCP keepalive");
        }

        let state = state.clone();
        let ordered_client_tx = ordered_client_tx.clone();
        let preview_client_tx = preview_client_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(
                stream,
                peer_addr,
                ordered_client_tx,
                preview_client_tx,
                state,
            )
            .await
            {
                warn!(client = %peer_addr, error = %e, "Oracle WS client failed");
            }
        });
    }
}

async fn handle_client(
    stream: TcpStream,
    peer_addr: SocketAddr,
    ordered_tx: broadcast::Sender<OracleEvent>,
    preview_tx: broadcast::Sender<TwapPreview>,
    state: ServerState,
) -> anyhow::Result<()> {
    let client_options_slot = Arc::new(Mutex::new(ClientOptions::default()));
    let client_options_slot_for_handshake = Arc::clone(&client_options_slot);
    let ws_stream = accept_hdr_async(stream, move |request: &Request, response| {
        *client_options_slot_for_handshake
            .lock()
            .expect("poisoned mutex") = parse_client_options(request.uri().query());
        Ok(response)
    })
    .await?;
    let client_options = client_options_slot.lock().expect("poisoned mutex").clone();
    let client_name = client_options.client_name.clone();
    let include_previews = client_options.include_previews;
    let connection_id = state
        .metrics
        .next_connection_id
        .fetch_add(1, Ordering::Relaxed)
        + 1;
    let _active_client_guard = ActiveClientGuard::new(Arc::clone(&state.metrics));
    let mut ordered_rx = ordered_tx.subscribe();
    let mut preview_rx = include_previews.then(|| preview_tx.subscribe());
    let connected_at = Instant::now();
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
        HEARTBEAT_INTERVAL,
    );
    let mut stats = ClientStats::default();
    info!(
        connection_id,
        client = %peer_addr,
        client_name = %client_name,
        include_previews,
        active_clients = state.metrics.active_clients.load(Ordering::Relaxed),
        ordered_receivers = ordered_tx.receiver_count(),
        preview_receivers = preview_tx.receiver_count(),
        "Oracle WS client connected"
    );

    let snapshot_prices = state.snapshot_prices().await;
    for price in &snapshot_prices {
        let event = OracleEvent::Price(price.clone());
        let json = serialize_json(&event)?;
        if let Err(reason) = send_text(
            &mut ws_sender,
            json,
            &state,
            connection_id,
            peer_addr,
            &client_name,
            "initial_snapshot_price",
        )
        .await
        {
            log_disconnect(
                &state,
                connection_id,
                peer_addr,
                &client_name,
                include_previews,
                connected_at,
                &stats,
                reason,
            );
            return Ok(());
        }
        stats.record_message_sent();
        stats.record_snapshot_price(price);
    }

    if include_previews {
        let snapshot_previews = state.snapshot_previews().await;
        for preview in &snapshot_previews {
            let event = OracleEvent::TwapPreview(preview.clone());
            let json = serialize_json(&event)?;
            if let Err(reason) = send_text(
                &mut ws_sender,
                json,
                &state,
                connection_id,
                peer_addr,
                &client_name,
                "initial_snapshot_preview",
            )
            .await
            {
                log_disconnect(
                    &state,
                    connection_id,
                    peer_addr,
                    &client_name,
                    include_previews,
                    connected_at,
                    &stats,
                    reason,
                );
                return Ok(());
            }
            stats.record_message_sent();
            stats.record_snapshot_preview();
        }
    }

    info!(
        connection_id,
        client = %peer_addr,
        client_name = %client_name,
        include_previews,
        snapshot_prices_sent = stats.snapshot_prices_sent,
        snapshot_previews_sent = stats.snapshot_previews_sent,
        "Oracle WS initial snapshot sent"
    );

    let disconnect_reason = 'client: loop {
        loop {
            match ordered_rx.try_recv() {
                Ok(event) => {
                    let json = serialize_json(&event)?;
                    if let Err(reason) = send_text(
                        &mut ws_sender,
                        json,
                        &state,
                        connection_id,
                        peer_addr,
                        &client_name,
                        "ordered_event",
                    )
                    .await
                    {
                        break 'client reason;
                    }
                    stats.record_message_sent();
                    stats.record_ordered_event(&event);
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => break 'client "broadcast_closed",
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    state
                        .metrics
                        .ordered_client_lagged
                        .fetch_add(n, Ordering::Relaxed);
                    stats.ordered_lagged = stats.ordered_lagged.saturating_add(n);
                    warn!(
                        connection_id,
                        client = %peer_addr,
                        client_name = %client_name,
                        dropped = n,
                        "oracle ordered client lagged; dropped ordered events"
                    );
                }
            }
        }

        tokio::select! {
            biased;

            ordered = ordered_rx.recv() => {
                match ordered {
                    Ok(event) => {
                        let json = serialize_json(&event)?;
                        if let Err(reason) = send_text(
                            &mut ws_sender,
                            json,
                            &state,
                            connection_id,
                            peer_addr,
                            &client_name,
                            "ordered_event",
                        )
                        .await
                        {
                            break 'client reason;
                        }
                        stats.record_message_sent();
                        stats.record_ordered_event(&event);
                    }
                    Err(broadcast::error::RecvError::Closed) => break 'client "broadcast_closed",
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        state
                            .metrics
                            .ordered_client_lagged
                            .fetch_add(n, Ordering::Relaxed);
                        stats.ordered_lagged = stats.ordered_lagged.saturating_add(n);
                        warn!(
                            connection_id,
                            client = %peer_addr,
                            client_name = %client_name,
                            dropped = n,
                            "oracle ordered client lagged; dropped ordered events"
                        );
                        continue;
                    }
                }
            }

            preview = async {
                preview_rx
                    .as_mut()
                    .expect("preview branch guarded")
                    .recv()
                    .await
            }, if preview_rx.is_some() => {
                match preview {
                    Ok(first_preview) => {
                        for preview in drain_latest_previews(
                            first_preview,
                            preview_rx
                                .as_mut()
                                .expect("preview branch guarded"),
                            connection_id,
                            peer_addr,
                            &client_name,
                            &state,
                            &mut stats,
                        ) {
                            let event = OracleEvent::TwapPreview(preview);
                            let json = serialize_json(&event)?;
                            if let Err(reason) = send_text(
                                &mut ws_sender,
                                json,
                                &state,
                                connection_id,
                                peer_addr,
                                &client_name,
                                "preview_event",
                            )
                            .await
                            {
                                break 'client reason;
                            }
                            stats.record_message_sent();
                            stats.record_preview();
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break 'client "broadcast_closed",
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        state
                            .metrics
                            .preview_client_lagged
                            .fetch_add(n, Ordering::Relaxed);
                        stats.preview_lagged = stats.preview_lagged.saturating_add(n);
                        warn!(
                            connection_id,
                            client = %peer_addr,
                            client_name = %client_name,
                            dropped = n,
                            "oracle preview client lagged; dropped preview updates"
                        );
                        continue;
                    }
                }
            }

            _ = heartbeat.tick() => {
                if let Err(reason) = send_text(
                    &mut ws_sender,
                    heartbeat_json(),
                    &state,
                    connection_id,
                    peer_addr,
                    &client_name,
                    "heartbeat",
                )
                .await
                {
                    break 'client reason;
                }
                stats.record_message_sent();
                stats.heartbeats_sent = stats.heartbeats_sent.saturating_add(1);
            }

            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break 'client "client_close",
                    Some(Ok(Message::Ping(data))) => {
                        stats.record_client_ping();
                        // With split streams, tokio-tungstenite does NOT auto-send
                        // Pong. We must respond explicitly per the WS protocol.
                        // Wrap in the same send timeout used for all other sends
                        // to prevent a slow-read client from stalling this task.
                        match tokio::time::timeout(WS_SEND_TIMEOUT, ws_sender.send(Message::Pong(data))).await {
                            Ok(Ok(())) => {}
                            Ok(Err(_)) => break 'client "pong_send_error",
                            Err(_) => {
                                state.metrics.send_timeouts.fetch_add(1, Ordering::Relaxed);
                                break 'client "pong_send_timeout";
                            }
                        }
                    }
                    Some(Err(error)) => {
                        warn!(
                            connection_id,
                            client = %peer_addr,
                            client_name = %client_name,
                            error = %error,
                            "Oracle WS receive error"
                        );
                        break 'client "stream_error";
                    }
                    _ => {}
                }
            }
        }
    };

    log_disconnect(
        &state,
        connection_id,
        peer_addr,
        &client_name,
        include_previews,
        connected_at,
        &stats,
        disconnect_reason,
    );

    Ok(())
}

async fn send_text<S>(
    ws_sender: &mut S,
    text: String,
    state: &ServerState,
    connection_id: u64,
    peer_addr: SocketAddr,
    client_name: &str,
    context: &'static str,
) -> Result<(), &'static str>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    match tokio::time::timeout(WS_SEND_TIMEOUT, ws_sender.send(Message::Text(text))).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            state.metrics.send_failures.fetch_add(1, Ordering::Relaxed);
            warn!(
                connection_id,
                client = %peer_addr,
                client_name = %client_name,
                context,
                error = %error,
                "Oracle WS send failed"
            );
            Err("ws_send_error")
        }
        Err(_) => {
            state.metrics.send_timeouts.fetch_add(1, Ordering::Relaxed);
            warn!(
                connection_id,
                client = %peer_addr,
                client_name = %client_name,
                context,
                send_timeout_secs = WS_SEND_TIMEOUT.as_secs(),
                "Oracle WS send timed out"
            );
            Err("ws_send_timeout")
        }
    }
}

fn log_disconnect(
    state: &ServerState,
    connection_id: u64,
    peer_addr: SocketAddr,
    client_name: &str,
    include_previews: bool,
    connected_at: Instant,
    stats: &ClientStats,
    reason: &str,
) {
    info!(
        connection_id,
        client = %peer_addr,
        client_name = %client_name,
        include_previews,
        active_clients = state.metrics.active_clients.load(Ordering::Relaxed).saturating_sub(1),
        duration_secs = connected_at.elapsed().as_secs(),
        reason,
        ordered_events_sent = stats.ordered_events_sent,
        preview_events_sent = stats.preview_events_sent,
        heartbeats_sent = stats.heartbeats_sent,
        snapshot_prices_sent = stats.snapshot_prices_sent,
        snapshot_previews_sent = stats.snapshot_previews_sent,
        messages_sent = stats.messages_sent,
        client_pings_received = stats.client_pings_received,
        ordered_lagged = stats.ordered_lagged,
        preview_lagged = stats.preview_lagged,
        last_btc_publish_time = stats.last_btc_publish_time,
        last_eth_publish_time = stats.last_eth_publish_time,
        last_sol_publish_time = stats.last_sol_publish_time,
        "Oracle WS client disconnected"
    );
}

fn drain_latest_previews(
    first_preview: TwapPreview,
    preview_rx: &mut broadcast::Receiver<TwapPreview>,
    connection_id: u64,
    peer_addr: SocketAddr,
    client_name: &str,
    state: &ServerState,
    stats: &mut ClientStats,
) -> Vec<TwapPreview> {
    let mut latest = HashMap::new();
    latest.insert(first_preview.symbol.clone(), first_preview);

    loop {
        match preview_rx.try_recv() {
            Ok(preview) => {
                latest.insert(preview.symbol.clone(), preview);
            }
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Closed) => break,
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                state
                    .metrics
                    .preview_client_lagged
                    .fetch_add(n, Ordering::Relaxed);
                stats.preview_lagged = stats.preview_lagged.saturating_add(n);
                warn!(
                    connection_id,
                    client = %peer_addr,
                    client_name = %client_name,
                    dropped = n,
                    "oracle preview client lagged while draining; dropped preview updates"
                );
            }
        }
    }

    let mut previews: Vec<_> = latest.into_values().collect();
    previews.sort_by(|left, right| left.symbol.cmp(&right.symbol));
    previews
}

fn parse_client_options(query: Option<&str>) -> ClientOptions {
    let mut options = ClientOptions::default();

    if let Some(query) = query {
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };

            match key {
                "client" if !value.is_empty() => {
                    // Sanitize: keep only alphanumeric, dash, underscore to
                    // prevent log injection via structured log fields.
                    options.client_name = value
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                        .take(64)
                        .collect();
                    if options.client_name.is_empty() {
                        options.client_name = "unknown".to_string();
                    }
                }
                "previews" => {
                    if matches!(value, "0" | "false" | "no" | "off") {
                        options.include_previews = false;
                    } else if matches!(value, "1" | "true" | "yes" | "on") {
                        options.include_previews = true;
                    }
                }
                _ => {}
            }
        }
    }

    options
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::time::timeout;
    use tokio_tungstenite::connect_async;

    #[tokio::test]
    async fn handle_client_sends_heartbeat_when_idle() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ordered_tx, _) = broadcast::channel::<OracleEvent>(16);
        let (preview_tx, _) = broadcast::channel::<TwapPreview>(16);
        let state = ServerState::default();

        let server = tokio::spawn(async move {
            let (stream, peer_addr) = listener.accept().await.unwrap();
            handle_client(stream, peer_addr, ordered_tx, preview_tx, state)
                .await
                .unwrap();
        });

        let (mut ws, _) = connect_async(format!("ws://{}", addr)).await.unwrap();
        let msg = timeout(Duration::from_secs(12), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let text = msg.into_text().unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "heartbeat");
        let timestamp = value["timestamp"].as_str().expect("timestamp field");
        chrono::DateTime::parse_from_rfc3339(timestamp).expect("RFC 3339 timestamp");

        ws.send(Message::Close(None)).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn handle_client_responds_to_ping_with_pong() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ordered_tx, _) = broadcast::channel::<OracleEvent>(16);
        let (preview_tx, _) = broadcast::channel::<TwapPreview>(16);
        let state = ServerState::default();

        let server = tokio::spawn(async move {
            let (stream, peer_addr) = listener.accept().await.unwrap();
            handle_client(stream, peer_addr, ordered_tx, preview_tx, state)
                .await
                .unwrap();
        });

        let (mut ws, _) = connect_async(format!("ws://{}", addr)).await.unwrap();

        // Send a Ping with a payload
        let ping_payload = b"keepalive".to_vec();
        ws.send(Message::Ping(ping_payload.clone().into()))
            .await
            .unwrap();

        // Expect a Pong back with the same payload (per RFC 6455 Section 5.5.3)
        let msg = timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for Pong")
            .unwrap()
            .unwrap();

        assert!(
            matches!(&msg, Message::Pong(data) if &data[..] == &ping_payload[..]),
            "expected Pong with matching payload, got {:?}",
            msg
        );

        ws.send(Message::Close(None)).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn handle_client_replays_latest_state_on_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ordered_tx, _) = broadcast::channel::<OracleEvent>(16);
        let (preview_tx, _) = broadcast::channel::<TwapPreview>(16);
        let state = ServerState::default();
        state
            .cache_ordered_event(&OracleEvent::Price(PriceUpdate {
                symbol: "SOL".to_string(),
                price: 120.0,
                confidence: 0.1,
                publish_time: 100,
                feed_id: "sol-feed".to_string(),
            }))
            .await;
        state
            .cache_ordered_event(&OracleEvent::Price(PriceUpdate {
                symbol: "BTC".to_string(),
                price: 62_000.0,
                confidence: 1.5,
                publish_time: 101,
                feed_id: "btc-feed".to_string(),
            }))
            .await;
        state
            .cache_preview(&TwapPreview {
                symbol: "ETH".to_string(),
                twap: 3_000.0,
                sample_count: 10,
                coverage: 0.9,
            })
            .await;

        let server = tokio::spawn(async move {
            let (stream, peer_addr) = listener.accept().await.unwrap();
            handle_client(stream, peer_addr, ordered_tx, preview_tx, state)
                .await
                .unwrap();
        });

        let (mut ws, _) = connect_async(format!("ws://{}/?client=market-maker", addr))
            .await
            .unwrap();

        let mut messages = Vec::new();
        for _ in 0..3 {
            let message = timeout(Duration::from_secs(1), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            messages.push(serde_json::from_str::<OracleEvent>(&message).unwrap());
        }

        assert!(matches!(
            &messages[0],
            OracleEvent::Price(PriceUpdate { symbol, publish_time, .. })
                if symbol == "BTC" && *publish_time == 101
        ));
        assert!(matches!(
            &messages[1],
            OracleEvent::Price(PriceUpdate { symbol, publish_time, .. })
                if symbol == "SOL" && *publish_time == 100
        ));
        assert!(matches!(
            &messages[2],
            OracleEvent::TwapPreview(TwapPreview { symbol, sample_count, .. })
                if symbol == "ETH" && *sample_count == 10
        ));

        ws.send(Message::Close(None)).await.unwrap();
        server.await.unwrap();
    }

    #[test]
    fn drain_latest_previews_keeps_only_latest_per_asset() {
        let (preview_tx, _) = broadcast::channel::<TwapPreview>(16);
        let mut preview_rx = preview_tx.subscribe();
        let state = ServerState::default();

        let _ = preview_tx.send(TwapPreview {
            symbol: "BTC".to_string(),
            twap: 100.0,
            sample_count: 1,
            coverage: 1.0,
        });
        let first = preview_rx.try_recv().unwrap();

        let _ = preview_tx.send(TwapPreview {
            symbol: "ETH".to_string(),
            twap: 200.0,
            sample_count: 2,
            coverage: 0.9,
        });
        let _ = preview_tx.send(TwapPreview {
            symbol: "BTC".to_string(),
            twap: 101.0,
            sample_count: 3,
            coverage: 1.0,
        });

        let previews = drain_latest_previews(
            first,
            &mut preview_rx,
            7,
            "127.0.0.1:8083".parse().unwrap(),
            "test-client",
            &state,
            &mut ClientStats::default(),
        );

        assert_eq!(previews.len(), 2);
        assert_eq!(previews[0].symbol, "BTC");
        assert_eq!(previews[0].twap, 101.0);
        assert_eq!(previews[1].symbol, "ETH");
        assert_eq!(previews[1].twap, 200.0);
    }

    #[test]
    fn parse_client_options_reads_query_params() {
        let options = parse_client_options(Some("foo=bar&client=market-maker&previews=0"));
        assert_eq!(options.client_name, "market-maker");
        assert!(!options.include_previews);

        let defaults = parse_client_options(None);
        assert_eq!(defaults.client_name, "unknown");
        assert!(defaults.include_previews);
    }

    #[tokio::test]
    async fn handle_client_without_previews_skips_preview_events() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ordered_tx, _) = broadcast::channel::<OracleEvent>(16);
        let (preview_tx, _) = broadcast::channel::<TwapPreview>(16);
        let state = ServerState::default();
        state
            .cache_preview(&TwapPreview {
                symbol: "BTC".to_string(),
                twap: 100.0,
                sample_count: 1,
                coverage: 1.0,
            })
            .await;

        let server = tokio::spawn(async move {
            let (stream, peer_addr) = listener.accept().await.unwrap();
            handle_client(stream, peer_addr, ordered_tx, preview_tx, state)
                .await
                .unwrap();
        });

        let (mut ws, _) = connect_async(format!("ws://{}/?client=risk-engine&previews=0", addr))
            .await
            .unwrap();

        // The server has a cached preview, but the client opted out of preview
        // delivery, so it should not receive the snapshot preview.
        assert!(
            timeout(Duration::from_millis(100), ws.next())
                .await
                .is_err(),
            "preview-opt-out client should not receive preview snapshot"
        );

        ws.send(Message::Close(None)).await.unwrap();
        server.await.unwrap();
    }
}
