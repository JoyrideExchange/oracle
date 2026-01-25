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

use joyride_oracle::{run_server, Asset, OracleEvent, PythClient, SettlementInfo, TwapCalculator};

/// Assets tracked by the oracle.
const ASSETS: &[Asset] = &[Asset::Sol, Asset::Btc, Asset::Eth];

/// WebSocket server address (0.0.0.0 for Docker/production).
fn server_addr() -> String {
    std::env::var("ORACLE_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8083".to_string())
}

/// TWAP window duration in seconds (30 minutes).
const TWAP_WINDOW_SECS: i64 = 30 * 60;

/// Calculate settlement timing info.
/// Settlement is at 00:00 UTC daily, TWAP window opens at 23:30 UTC.
fn calculate_settlement_info(now_secs: i64) -> SettlementInfo {
    // Seconds per day
    const SECS_PER_DAY: i64 = 24 * 60 * 60;

    // Current time of day in seconds
    let time_of_day = now_secs % SECS_PER_DAY;

    // Next settlement is at midnight UTC
    let seconds_to_settlement = if time_of_day == 0 {
        0 // It's exactly midnight
    } else {
        SECS_PER_DAY - time_of_day
    };

    let next_settlement = now_secs + seconds_to_settlement;

    // TWAP window opens 30 minutes before settlement
    let twap_window_start = next_settlement - TWAP_WINDOW_SECS;
    let seconds_to_twap_window = (twap_window_start - now_secs).max(0);

    // We're in the TWAP window if less than 30 minutes to settlement
    let in_twap_window = seconds_to_settlement <= TWAP_WINDOW_SECS && seconds_to_settlement > 0;

    SettlementInfo {
        next_settlement,
        twap_window_start,
        seconds_to_twap_window,
        seconds_to_settlement,
        in_twap_window,
    }
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

    // Create broadcast channel for oracle events (to WebSocket clients)
    let (broadcast_tx, _) = broadcast::channel::<OracleEvent>(256);
    let broadcast_tx_clone = broadcast_tx.clone();

    // Create channel for Pyth client events
    let (event_tx, mut event_rx) = mpsc::channel::<OracleEvent>(256);

    // Create TWAP calculator
    let twap = Arc::new(RwLock::new(TwapCalculator::new()));
    let twap_clone = twap.clone();

    // Start WebSocket server
    let addr = server_addr();
    let server_rx = broadcast_tx.subscribe();
    let addr_clone = addr.clone();
    tokio::spawn(async move {
        run_server(&addr_clone, server_rx).await;
    });
    info!("WebSocket server listening on {}", addr);

    // Start Pyth client
    let mut pyth_client = PythClient::new(event_tx, ASSETS.to_vec());
    tokio::spawn(async move {
        if let Err(e) = pyth_client.run().await {
            tracing::error!("Pyth client error: {}", e);
        }
    });

    // Start settlement timer task (broadcasts timing and TWAP previews every second)
    let timer_broadcast_tx = broadcast_tx.clone();
    let timer_twap = twap.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut last_in_window = false;

        loop {
            interval.tick().await;

            let now = chrono::Utc::now().timestamp();
            let settlement_info = calculate_settlement_info(now);

            // Log when entering/exiting TWAP window
            if settlement_info.in_twap_window && !last_in_window {
                info!("TWAP settlement window is now ACTIVE");
            } else if !settlement_info.in_twap_window && last_in_window {
                info!("TWAP settlement window has ended");
            }
            last_in_window = settlement_info.in_twap_window;

            // Broadcast settlement timing
            let _ = timer_broadcast_tx.send(OracleEvent::Settlement(settlement_info.clone()));

            // Calculate and broadcast TWAP previews for each asset
            let twap = timer_twap.read().await;
            for asset in ASSETS {
                if let Some(preview) = twap.calculate_preview(
                    asset.symbol(),
                    now,
                    settlement_info.in_twap_window,
                ) {
                    let _ = timer_broadcast_tx.send(OracleEvent::TwapPreview(preview));
                }
            }
        }
    });

    // Process events and broadcast to clients
    let mut last_prices: std::collections::HashMap<String, f64> = std::collections::HashMap::new();

    while let Some(event) = event_rx.recv().await {
        // Broadcast all events to WebSocket clients
        let _ = broadcast_tx_clone.send(event.clone());

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
                    result.twap_price,
                    result.sample_count,
                    result.coverage * 100.0
                );
            }
            OracleEvent::Error { message } => {
                warn!("Oracle error: {}", message);
            }
            // TwapPreview and Settlement are generated by the timer task,
            // not received through event_rx
            OracleEvent::TwapPreview(_) | OracleEvent::Settlement(_) => {}
        }
    }

    Ok(())
}
