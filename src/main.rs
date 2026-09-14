//! Joyride Oracle Service
//!
//! Streams spot index prices from Block Scholes and calculates TWAPs for
//! settlement.
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
    run_server, Asset, BlockScholesClient, OracleEvent, TwapCalculator, TwapPreview,
    BLOCKSCHOLES_WS_URL, DEFAULT_FREQUENCY_MS,
};

/// Assets tracked by the oracle.
const ASSETS: &[Asset] = &[Asset::Sol, Asset::Btc, Asset::Eth];
const ORDERED_FANOUT_BUFFER: usize = 4096;
const PREVIEW_FANOUT_BUFFER: usize = 2048;

/// WebSocket server address (0.0.0.0 for Docker/production).
fn server_addr() -> String {
    std::env::var("ORACLE_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8083".to_string())
}

/// Slowest Block Scholes subscription frequency the service accepts. The TWAP
/// samples once per second, so anything slower leaves seconds unsampled.
/// Faster rates are fine: the calculator keeps one sample per second.
const MAX_FREQUENCY_MS: u64 = 1_000;

fn blockscholes_frequency_ms() -> anyhow::Result<u64> {
    match std::env::var("BLOCKSCHOLES_FREQUENCY_MS") {
        Ok(raw) => parse_frequency_ms(Some(&raw)),
        Err(std::env::VarError::NotPresent) => parse_frequency_ms(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("BLOCKSCHOLES_FREQUENCY_MS is not valid UTF-8")
        }
    }
}

/// Parse `BLOCKSCHOLES_FREQUENCY_MS`. Unset or blank means the default; a
/// malformed, zero, or slower-than-one-second value fails startup. Whether a
/// faster value is allowed is the account plan's call, made at subscribe time.
fn parse_frequency_ms(raw: Option<&str>) -> anyhow::Result<u64> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(DEFAULT_FREQUENCY_MS);
    };
    let frequency_ms: u64 = raw.parse().map_err(|_| {
        anyhow::anyhow!(
            "BLOCKSCHOLES_FREQUENCY_MS must be a whole number of milliseconds (got {raw:?})"
        )
    })?;
    if frequency_ms == 0 || frequency_ms > MAX_FREQUENCY_MS {
        anyhow::bail!(
            "BLOCKSCHOLES_FREQUENCY_MS must be between 1 and {MAX_FREQUENCY_MS} for one-second \
             TWAP sampling (got {frequency_ms})"
        );
    }
    Ok(frequency_ms)
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
    let frequency_ms = blockscholes_frequency_ms()?;

    // Split ordered oracle traffic from latest-state preview traffic so
    // preview fanout can never displace price delivery.
    let (ordered_tx, _) = broadcast::channel::<OracleEvent>(ORDERED_FANOUT_BUFFER);
    let ordered_tx_clone = ordered_tx.clone();
    let (preview_tx, _) = broadcast::channel::<TwapPreview>(PREVIEW_FANOUT_BUFFER);
    let preview_tx_clone = preview_tx.clone();

    // Create channel for upstream price-feed events
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

    // Start Block Scholes client. The Pyth client is retained in
    // joyride-oracle-core but not wired up: its data plan lapsed.
    let api_key = std::env::var("BLOCKSCHOLES_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty());
    if api_key.is_none() {
        warn!(
            "BLOCKSCHOLES_API_KEY is not set. Block Scholes rejects \
             unauthenticated connections; prices will freeze at their last \
             cached values."
        );
    }
    info!(
        ws_url = %BLOCKSCHOLES_WS_URL,
        authenticated = api_key.is_some(),
        frequency_ms,
        "Using Block Scholes index.px feed"
    );
    let mut price_client = BlockScholesClient::new(event_tx, ASSETS.to_vec())
        .with_api_key(api_key)
        .with_frequency_ms(frequency_ms);
    tokio::spawn(async move {
        if let Err(e) = price_client.run().await {
            tracing::error!("Block Scholes client error: {}", e);
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
                info!("Upstream price feed live");
            }
            OracleEvent::Disconnected => {
                warn!("Disconnected from upstream price feed");
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
            OracleEvent::Error { message } => {
                warn!("Oracle error: {}", message);
            }
            // TwapPreview is generated by the timer task, not received through event_rx
            OracleEvent::TwapPreview(_) => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_defaults_when_unset_or_blank() {
        assert_eq!(parse_frequency_ms(None).unwrap(), DEFAULT_FREQUENCY_MS);
        assert_eq!(parse_frequency_ms(Some("")).unwrap(), DEFAULT_FREQUENCY_MS);
        assert_eq!(
            parse_frequency_ms(Some("  ")).unwrap(),
            DEFAULT_FREQUENCY_MS
        );
    }

    #[test]
    fn frequency_accepts_one_second_or_faster() {
        assert_eq!(parse_frequency_ms(Some("1000")).unwrap(), 1_000);
        assert_eq!(parse_frequency_ms(Some(" 1000 ")).unwrap(), 1_000);
        assert_eq!(parse_frequency_ms(Some("500")).unwrap(), 500);
        assert_eq!(parse_frequency_ms(Some("1")).unwrap(), 1);
    }

    #[test]
    fn frequency_rejects_slower_than_one_second_and_zero() {
        for raw in ["1001", "20000", "60000", "0"] {
            let err = parse_frequency_ms(Some(raw)).unwrap_err().to_string();
            assert!(err.contains("between 1 and 1000"), "{raw}: {err}");
        }
    }

    #[test]
    fn frequency_rejects_malformed_values_instead_of_falling_back() {
        for raw in ["1000ms", "1s", "1_000", "-1", "1e3", "abc"] {
            let err = parse_frequency_ms(Some(raw)).unwrap_err().to_string();
            assert!(err.contains("whole number of milliseconds"), "{raw}: {err}");
        }
    }
}
