//! Joyride Oracle Service
//!
//! This crate provides price data from Pyth Network for the Joyride exchange.
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
//! use joyride_oracle::{PythClient, TwapCalculator, Asset, OracleEvent};
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

pub mod pyth;
pub mod server;
pub mod twap;
pub mod types;

pub use pyth::{PythClient, HERMES_URL};
pub use server::run_server;
pub use twap::{TwapCalculator, TwapError, DEFAULT_TWAP_WINDOW_SECS, MIN_COVERAGE};
pub use types::{Asset, OracleEvent, PriceUpdate, SettlementInfo, TwapPreview, TwapResult, TwapSample};
