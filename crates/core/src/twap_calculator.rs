//! TWAP (Time-Weighted Average Price) calculator.
//!
//! Accumulates price samples over a configurable window and computes
//! the time-weighted average for settlement pricing.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use tracing::{debug, info, warn};

use joyride_oracle_wire::{PriceUpdate, TwapPreview};

/// A single recorded TWAP sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwapSample {
    /// The price at this sample time.
    pub price: f64,
    /// Unix timestamp in seconds.
    pub timestamp: i64,
}

/// A completed TWAP calculation over a closed window.
///
/// Produced by [`TwapCalculator::calculate`] for callers that want to
/// settle or persist a window value. Not on the oracle wire contract —
/// the oracle service only broadcasts rolling previews — but available
/// for embedders implementing their own settlement logic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwapResult {
    /// The asset this TWAP is for.
    pub symbol: String,

    /// The calculated TWAP price.
    pub twap: f64,

    /// Start of the TWAP window (Unix timestamp in seconds).
    pub window_start: i64,

    /// End of the TWAP window (Unix timestamp in seconds).
    pub window_end: i64,

    /// Number of samples used in calculation.
    pub sample_count: usize,

    /// Percentage of expected samples that were collected (0.0 to 1.0).
    pub coverage: f64,
}

/// Default TWAP window duration in seconds (30 minutes).
pub const DEFAULT_TWAP_WINDOW_SECS: i64 = 30 * 60;

/// Default retention horizon, in windows. Kept at two so `calculate()` can
/// still be called with a `window_end` up to one window in the past.
pub const DEFAULT_RETENTION_WINDOWS: u32 = 2;

/// Sampling interval in seconds (1 second). Not configurable — `with_window`
/// locks this in — and thus not part of the public API.
const DEFAULT_SAMPLE_INTERVAL_SECS: i64 = 1;

/// TWAP calculator that accumulates samples and computes averages.
pub struct TwapCalculator {
    samples: HashMap<String, VecDeque<TwapSample>>,
    window_secs: i64,
    /// How many windows of history to retain. Caps how far in the past
    /// `calculate()` returns full coverage and bounds memory.
    retention_windows: u32,
    sample_interval_secs: i64,
    last_sample_time: HashMap<String, i64>,
}

impl TwapCalculator {
    pub fn new() -> Self {
        Self::with_retention(DEFAULT_TWAP_WINDOW_SECS, DEFAULT_RETENTION_WINDOWS)
    }

    pub fn with_window(window_secs: i64) -> Self {
        Self::with_retention(window_secs, DEFAULT_RETENTION_WINDOWS)
    }

    /// Construct with an explicit retention horizon. `retention_windows`
    /// must be `>= 1`; a larger value lets `calculate()` look further into
    /// the past at the cost of memory.
    pub fn with_retention(window_secs: i64, retention_windows: u32) -> Self {
        assert!(
            retention_windows >= 1,
            "retention_windows must be >= 1 (got {retention_windows})"
        );
        Self {
            samples: HashMap::new(),
            window_secs,
            retention_windows,
            sample_interval_secs: DEFAULT_SAMPLE_INTERVAL_SECS,
            last_sample_time: HashMap::new(),
        }
    }

    pub fn record(&mut self, update: &PriceUpdate) -> bool {
        let symbol = &update.symbol;
        let timestamp = update.publish_time;

        if let Some(&last_time) = self.last_sample_time.get(symbol) {
            if timestamp - last_time < self.sample_interval_secs {
                return false;
            }
        }

        let sample = TwapSample {
            price: update.price,
            timestamp,
        };

        let deque = self.samples.entry(symbol.clone()).or_default();
        deque.push_back(sample);

        // The interval check above keeps timestamps strictly increasing, so
        // stale data always sits at the head and front-pop is enough.
        let retention_secs = self
            .window_secs
            .saturating_mul(i64::from(self.retention_windows));
        let cutoff = timestamp.saturating_sub(retention_secs);
        while let Some(front) = deque.front() {
            if front.timestamp < cutoff {
                deque.pop_front();
            } else {
                break;
            }
        }

        self.last_sample_time.insert(symbol.clone(), timestamp);

        debug!(
            "TWAP sample recorded for {}: ${:.4} at {}",
            symbol, update.price, timestamp
        );

        true
    }

    pub fn sample_count(&self, symbol: &str) -> usize {
        self.samples.get(symbol).map(|s| s.len()).unwrap_or(0)
    }

    pub fn expected_samples(&self) -> usize {
        (self.window_secs / self.sample_interval_secs) as usize
    }

    pub fn calculate(&self, symbol: &str, window_end: i64) -> Option<TwapResult> {
        let samples = self.samples.get(symbol)?;
        let window_start = window_end - self.window_secs;
        let window_samples: Vec<&TwapSample> = samples
            .iter()
            .filter(|s| s.timestamp >= window_start && s.timestamp <= window_end)
            .collect();

        if window_samples.is_empty() {
            warn!("No samples found for {} in TWAP window", symbol);
            return None;
        }

        let sum: f64 = window_samples.iter().map(|s| s.price).sum();
        let twap = sum / window_samples.len() as f64;
        let expected = self.expected_samples();
        let coverage = window_samples.len() as f64 / expected as f64;

        info!(
            "TWAP calculated for {}: ${:.4} ({} samples, {:.1}% coverage)",
            symbol,
            twap,
            window_samples.len(),
            coverage * 100.0
        );

        Some(TwapResult {
            symbol: symbol.to_string(),
            twap,
            window_start,
            window_end,
            sample_count: window_samples.len(),
            coverage,
        })
    }

    pub fn calculate_preview(&self, symbol: &str, current_time: i64) -> Option<TwapPreview> {
        let samples = self.samples.get(symbol)?;
        let window_start = current_time - self.window_secs;
        let window_samples: Vec<&TwapSample> = samples
            .iter()
            .filter(|s| s.timestamp >= window_start && s.timestamp <= current_time)
            .collect();

        if window_samples.is_empty() {
            return Some(TwapPreview {
                symbol: symbol.to_string(),
                twap: 0.0,
                sample_count: 0,
                coverage: 0.0,
            });
        }

        let sum: f64 = window_samples.iter().map(|s| s.price).sum();
        let twap = sum / window_samples.len() as f64;
        let expected = self.expected_samples();
        let coverage = (window_samples.len() as f64 / expected as f64).min(1.0);

        Some(TwapPreview {
            symbol: symbol.to_string(),
            twap,
            sample_count: window_samples.len(),
            coverage,
        })
    }

    pub fn clear(&mut self, symbol: &str) {
        self.samples.remove(symbol);
        self.last_sample_time.remove(symbol);
        info!("Cleared TWAP samples for {}", symbol);
    }

    pub fn prune(&mut self, before_timestamp: i64) {
        for (symbol, samples) in &mut self.samples {
            let original_len = samples.len();
            while let Some(front) = samples.front() {
                if front.timestamp < before_timestamp {
                    samples.pop_front();
                } else {
                    break;
                }
            }
            let pruned = original_len - samples.len();
            if pruned > 0 {
                debug!("Pruned {} old samples for {}", pruned, symbol);
            }
        }
    }

    pub fn get_samples(&self, symbol: &str) -> Option<Vec<TwapSample>> {
        self.samples
            .get(symbol)
            .map(|d| d.iter().cloned().collect())
    }
}

impl Default for TwapCalculator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_update(symbol: &str, price: f64, timestamp: i64) -> PriceUpdate {
        PriceUpdate {
            symbol: symbol.to_string(),
            price,
            confidence: 0.01,
            publish_time: timestamp,
            feed_id: "0x123".to_string(),
        }
    }

    #[test]
    fn test_record_samples() {
        let mut calc = TwapCalculator::new();
        assert!(calc.record(&make_update("SOL", 200.0, 1000)));
        assert!(calc.record(&make_update("SOL", 201.0, 1001)));
        assert!(calc.record(&make_update("SOL", 202.0, 1002)));
        assert_eq!(calc.sample_count("SOL"), 3);
    }

    #[test]
    fn test_sample_interval() {
        let mut calc = TwapCalculator::new();
        assert!(calc.record(&make_update("SOL", 200.0, 1000)));
        assert!(!calc.record(&make_update("SOL", 200.5, 1000)));
        assert_eq!(calc.sample_count("SOL"), 1);
    }

    #[test]
    fn test_calculate_twap() {
        let mut calc = TwapCalculator::with_window(10);
        for i in 0..10 {
            calc.record(&make_update("SOL", 200.0 + i as f64, 1000 + i));
        }
        let result = calc.calculate("SOL", 1009).unwrap();
        assert_eq!(result.sample_count, 10);
        assert!((result.twap - 204.5).abs() < 0.0001);
    }

    #[test]
    fn test_prune_old_samples() {
        let mut calc = TwapCalculator::new();

        for i in 0..100 {
            calc.record(&make_update("SOL", 200.0, 1000 + i));
        }

        assert_eq!(calc.sample_count("SOL"), 100);

        calc.prune(1050);

        assert_eq!(calc.sample_count("SOL"), 50);
    }

    #[test]
    fn test_record_trims_to_retention_horizon() {
        // Default retention is two windows wide, so a 10s window keeps 20s.
        let mut calc = TwapCalculator::with_window(10);

        for i in 0..100 {
            calc.record(&make_update("SOL", 200.0 + i as f64, 1000 + i));
        }

        assert_eq!(calc.sample_count("SOL"), 21);

        let result = calc.calculate("SOL", 1099).unwrap();
        assert_eq!(result.sample_count, 11);

        // window_end one window in the past still sits inside retention.
        let past = calc.calculate("SOL", 1089).unwrap();
        assert_eq!(past.sample_count, 11);
    }

    #[test]
    fn test_with_retention_custom_horizon() {
        let mut calc = TwapCalculator::with_retention(10, 5);

        for i in 0..100 {
            calc.record(&make_update("SOL", 200.0 + i as f64, 1000 + i));
        }

        assert_eq!(calc.sample_count("SOL"), 51);

        let past = calc.calculate("SOL", 1059).unwrap();
        assert_eq!(past.sample_count, 11);
    }

    #[test]
    #[should_panic(expected = "retention_windows")]
    fn test_with_retention_rejects_zero() {
        let _ = TwapCalculator::with_retention(100, 0);
    }
}
