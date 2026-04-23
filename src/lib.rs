//! Joyride Oracle service and transport layer.
//!
//! The embeddable in-process API lives in `joyride-oracle-core`. This crate
//! contains the WebSocket server and re-exports the core and wire crates for
//! convenience.
//!
//! # Features
//!
//! - **Real-time price streaming** via Pyth Hermes SSE API
//! - **TWAP calculation** for settlement pricing
//! - **Multi-asset support** (SOL, BTC, ETH)
//!
//! # Example
//!
//! ```no_run
//! use joyride_oracle_core::{PythClient, TwapCalculator, Asset, OracleEvent};
//! use tokio::sync::mpsc;
//!
//! #[tokio::main]
//! async fn main() {
//!     let (tx, mut rx) = mpsc::channel(256);
//!     let assets = vec![Asset::Sol, Asset::Btc, Asset::Eth];
//!
//!     let mut client = PythClient::new(tx, assets);
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
    Asset, OracleEvent, PythClient, TwapCalculator, TwapResult, TwapSample,
    DEFAULT_TWAP_WINDOW_SECS, HERMES_URL,
};
pub use joyride_oracle_wire::{BroadcastFrame, PriceUpdate, TwapPreview, WirePayload};
pub use server::run_server;
