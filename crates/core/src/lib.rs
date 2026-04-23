//! Joyride Oracle core library.
//!
//! Contains the in-process API for embedders: Pyth ingestion, TWAP
//! calculation, and domain event types. WebSocket transport lives in the
//! top-level `joyride-oracle` crate; wire-format serde types live in
//! `joyride-oracle-wire`.

pub mod pyth;
pub mod twap_calculator;
pub mod types;

// Re-export only the wire payload types that appear inside OracleEvent
// variants — callers receiving events need them. BroadcastFrame and
// WirePayload are transport-layer concerns; consumers that want those
// should depend on `joyride-oracle-wire` directly.
pub use joyride_oracle_wire::{PriceUpdate, TwapPreview};
pub use pyth::{PythClient, HERMES_URL};
pub use twap_calculator::{TwapCalculator, TwapResult, TwapSample, DEFAULT_TWAP_WINDOW_SECS};
pub use types::{Asset, OracleEvent};
