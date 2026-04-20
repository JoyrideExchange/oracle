//! Joyride Oracle Service
//!
//! Streams price data from Pyth Network and calculates TWAPs for settlement.
//!
//! # Usage
//!
//! ```bash
//! cargo run
//! ```

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{info, warn};

use joyride_oracle::{
    run_server, Asset, OracleEvent, PythClient, TwapCalculator, TwapPreview, HERMES_URL,
};

/// Assets tracked by the oracle.
const ASSETS: &[Asset] = &[Asset::Sol, Asset::Btc, Asset::Eth];
const ORDERED_FANOUT_BUFFER: usize = 4096;
const PREVIEW_FANOUT_BUFFER: usize = 2048;

/// WebSocket server address (0.0.0.0 for Docker/production).
fn server_addr() -> String {
    std::env::var("ORACLE_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8083".to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    info!("Starting Joyride Oracle Service");
    info!(
        "Tracking assets: {}",
        ASSETS
            .iter()
            .map(|a| a.symbol())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Split ordered oracle traffic from latest-state preview traffic so
    // preview fanout can never displace price delivery.
    let (ordered_tx, _) = broadcast::channel::<OracleEvent>(ORDERED_FANOUT_BUFFER);
    let ordered_tx_clone = ordered_tx.clone();
    let (preview_tx, _) = broadcast::channel::<TwapPreview>(PREVIEW_FANOUT_BUFFER);
    let preview_tx_clone = preview_tx.clone();

    // Create channel for Pyth client events
    let (event_tx, mut event_rx) = mpsc::channel::<OracleEvent>(256);

    // Create TWAP calculator
    let twap = Arc::new(RwLock::new(TwapCalculator::new()));
    let twap_clone = twap.clone();

    // Start WebSocket server
    let addr = server_addr();
    let ordered_server_rx = ordered_tx.subscribe();
    let preview_server_rx = preview_tx.subscribe();
    let addr_clone = addr.clone();
    tokio::spawn(async move {
        run_server(&addr_clone, ordered_server_rx, preview_server_rx).await;
    });
    info!("WebSocket server listening on {}", addr);

    // Start Pyth client
    info!(hermes_url = %HERMES_URL, "Using Hermes endpoint");
    let mut pyth_client = PythClient::new(event_tx, ASSETS.to_vec());
    tokio::spawn(async move {
        if let Err(e) = pyth_client.run().await {
            tracing::error!("Pyth client error: {}", e);
        }
    });

    // Start TWAP preview timer task (broadcasts rolling TWAP previews every second)
    let timer_twap = twap.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));

        loop {
            interval.tick().await;

            let now = chrono::Utc::now().timestamp();

            // Calculate and broadcast TWAP previews for each asset
            let twap = timer_twap.read().await;
            for asset in ASSETS {
                if let Some(preview) = twap.calculate_preview(asset.symbol(), now) {
                    let _ = preview_tx_clone.send(preview);
                }
            }
        }
    });

    // Process events and broadcast to clients
    let mut last_prices: std::collections::HashMap<String, f64> = std::collections::HashMap::new();

    while let Some(event) = event_rx.recv().await {
        // Broadcast ordered events to WebSocket clients. Previews fan out through
        // a dedicated latest-state channel from the timer task above.
        if !matches!(event, OracleEvent::TwapPreview(_)) {
            let _ = ordered_tx_clone.send(event.clone());
        }

        match &event {
            OracleEvent::Connected => {
                info!("Connected to Pyth Hermes");
            }
            OracleEvent::Disconnected => {
                warn!("Disconnected from Pyth Hermes");
            }
            OracleEvent::Price(update) => {
                // Record for TWAP
                let mut twap = twap_clone.write().await;
                twap.record(update);

                // Log price changes (avoid spamming on every update)
                let should_log = match last_prices.get(&update.symbol) {
                    Some(&last) => {
                        let pct_change = ((update.price - last) / last).abs();
                        pct_change > 0.001 // Log if > 0.1% change
                    }
                    None => true,
                };

                if should_log {
                    info!(
                        "{}: ${:.4} (conf: ${:.4}, samples: {})",
                        update.symbol,
                        update.price,
                        update.confidence,
                        twap.sample_count(&update.symbol)
                    );
                    last_prices.insert(update.symbol.clone(), update.price);
                }
            }
            OracleEvent::Twap(result) => {
                info!(
                    "TWAP for {}: ${:.4} ({} samples, {:.1}% coverage)",
                    result.symbol,
                    result.twap,
                    result.sample_count,
                    result.coverage * 100.0
                );
            }
            OracleEvent::Error { message } => {
                warn!("Oracle error: {}", message);
            }
            // TwapPreview is generated by the timer task, not received through event_rx
            OracleEvent::TwapPreview(_) => {}
        }
    }

    Ok(())
}
