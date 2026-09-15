//! Joyride Oracle service and transport layer.
//!
//! The embeddable in-process API lives in `joyride-oracle-core`. This crate
//! contains the WebSocket server and re-exports the core and wire crates for
//! convenience.
//!
//! # Features
//!
//! - **Real-time spot index streaming** via the Block Scholes WebSocket API
//! - **TWAP calculation** for settlement pricing
//! - **Multi-asset support** (SOL, BTC, ETH)
//!
//! # Example
//!
//! ```no_run
//! use joyride_oracle_core::{BlockScholesClient, TwapCalculator, Asset, OracleEvent};
//! use tokio::sync::mpsc;
//!
//! #[tokio::main]
//! async fn main() {
//!     let (tx, mut rx) = mpsc::channel(256);
//!     let assets = vec![Asset::Sol, Asset::Btc, Asset::Eth];
//!
//!     let mut client = BlockScholesClient::new(tx, assets)
//!         .with_api_key(std::env::var("BLOCKSCHOLES_API_KEY").ok());
//!     let mut twap = TwapCalculator::new();
//!
//!     tokio::spawn(async move { client.run().await });
//!
//!     while let Some(event) = rx.recv().await {
//!         if let OracleEvent::Price(update) = event {
//!             twap.record(&update);
//!             println!("{}: ${:.2}", update.symbol, update.price);
//!         }
//!     }
//! }
//! ```

pub mod server;
pub use joyride_oracle_core::{
    Asset, BlockScholesClient, OracleEvent, PythClient, TwapCalculator, TwapResult, TwapSample,
    BLOCKSCHOLES_WS_URL, DEFAULT_FREQUENCY_MS, DEFAULT_TWAP_WINDOW_SECS, HERMES_URL,
};
pub use joyride_oracle_wire::{BroadcastFrame, PriceUpdate, TwapPreview, WirePayload};
pub use server::run_server;
