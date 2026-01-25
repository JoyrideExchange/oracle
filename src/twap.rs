//! TWAP (Time-Weighted Average Price) calculator.
//!
//! Accumulates price samples over a configurable window and computes
//! the time-weighted average for settlement pricing.

use std::collections::HashMap;
use tracing::{debug, info, warn};

use crate::types::{PriceUpdate, TwapPreview, TwapResult, TwapSample};

/// Default TWAP window duration in seconds (30 minutes).
pub const DEFAULT_TWAP_WINDOW_SECS: i64 = 30 * 60;

/// Default sampling interval in seconds (1 second).
pub const DEFAULT_SAMPLE_INTERVAL_SECS: i64 = 1;

/// Minimum coverage required for a valid TWAP (90%).
pub const MIN_COVERAGE: f64 = 0.90;

/// TWAP calculator that accumulates samples and computes averages.
pub struct TwapCalculator {
    /// Samples per asset, keyed by symbol.
    samples: HashMap<String, Vec<TwapSample>>,

    /// TWAP window duration in seconds.
    window_secs: i64,

    /// Expected sampling interval in seconds.
    sample_interval_secs: i64,

    /// Last sampled timestamp per asset (to avoid duplicate samples).
    last_sample_time: HashMap<String, i64>,
}

impl TwapCalculator {
    /// Create a new TWAP calculator with default settings.
    pub fn new() -> Self {
        Self {
            samples: HashMap::new(),
            window_secs: DEFAULT_TWAP_WINDOW_SECS,
            sample_interval_secs: DEFAULT_SAMPLE_INTERVAL_SECS,
            last_sample_time: HashMap::new(),
        }
    }

    /// Create a new TWAP calculator with custom window duration.
    pub fn with_window(window_secs: i64) -> Self {
        Self {
            samples: HashMap::new(),
            window_secs,
            sample_interval_secs: DEFAULT_SAMPLE_INTERVAL_SECS,
            last_sample_time: HashMap::new(),
        }
    }

    /// Record a price update as a TWAP sample.
    /// Returns true if a new sample was recorded (based on sample interval).
    pub fn record(&mut self, update: &PriceUpdate) -> bool {
        let symbol = &update.symbol;
        let timestamp = update.publish_time;

        // Check if we should sample (based on interval)
        if let Some(&last_time) = self.last_sample_time.get(symbol) {
            if timestamp - last_time < self.sample_interval_secs {
                return false;
            }
        }

        // Record the sample
        let sample = TwapSample {
            price: update.price,
            timestamp,
        };

        self.samples
            .entry(symbol.clone())
            .or_insert_with(Vec::new)
            .push(sample);

        self.last_sample_time.insert(symbol.clone(), timestamp);

        debug!(
            "TWAP sample recorded for {}: ${:.4} at {}",
            symbol, update.price, timestamp
        );

        true
    }

    /// Get the current number of samples for an asset.
    pub fn sample_count(&self, symbol: &str) -> usize {
        self.samples.get(symbol).map(|s| s.len()).unwrap_or(0)
    }

    /// Get the expected number of samples for the TWAP window.
    pub fn expected_samples(&self) -> usize {
        (self.window_secs / self.sample_interval_secs) as usize
    }

    /// Calculate the TWAP for an asset over the specified window.
    /// `window_end` is the Unix timestamp when the window ends (e.g., expiration time).
    pub fn calculate(&self, symbol: &str, window_end: i64) -> Option<TwapResult> {
        let samples = self.samples.get(symbol)?;
        let window_start = window_end - self.window_secs;

        // Filter samples within the window
        let window_samples: Vec<&TwapSample> = samples
            .iter()
            .filter(|s| s.timestamp >= window_start && s.timestamp <= window_end)
            .collect();

        if window_samples.is_empty() {
            warn!("No samples found for {} in TWAP window", symbol);
            return None;
        }

        // Calculate simple average (all samples equally weighted since we sample at regular intervals)
        let sum: f64 = window_samples.iter().map(|s| s.price).sum();
        let twap_price = sum / window_samples.len() as f64;

        let expected = self.expected_samples();
        let coverage = window_samples.len() as f64 / expected as f64;

        info!(
            "TWAP calculated for {}: ${:.4} ({} samples, {:.1}% coverage)",
            symbol,
            twap_price,
            window_samples.len(),
            coverage * 100.0
        );

        Some(TwapResult {
            symbol: symbol.to_string(),
            twap_price,
            window_start,
            window_end,
            sample_count: window_samples.len(),
            coverage,
        })
    }

    /// Calculate TWAP and validate that coverage meets minimum requirements.
    pub fn calculate_validated(&self, symbol: &str, window_end: i64) -> Result<TwapResult, TwapError> {
        let result = self.calculate(symbol, window_end).ok_or(TwapError::NoSamples)?;

        if result.coverage < MIN_COVERAGE {
            return Err(TwapError::InsufficientCoverage {
                actual: result.coverage,
                required: MIN_COVERAGE,
            });
        }

        Ok(result)
    }

    /// Calculate a rolling TWAP preview (what settlement price would be if it happened now).
    /// This uses the current time as the window end.
    pub fn calculate_preview(&self, symbol: &str, current_time: i64, in_settlement_window: bool) -> Option<TwapPreview> {
        let samples = self.samples.get(symbol)?;
        let window_start = current_time - self.window_secs;

        // Filter samples within the rolling window
        let window_samples: Vec<&TwapSample> = samples
            .iter()
            .filter(|s| s.timestamp >= window_start && s.timestamp <= current_time)
            .collect();

        if window_samples.is_empty() {
            return Some(TwapPreview {
                symbol: symbol.to_string(),
                twap_price: 0.0,
                sample_count: 0,
                coverage: 0.0,
                in_settlement_window,
            });
        }

        let sum: f64 = window_samples.iter().map(|s| s.price).sum();
        let twap_price = sum / window_samples.len() as f64;

        let expected = self.expected_samples();
        let coverage = (window_samples.len() as f64 / expected as f64).min(1.0);

        Some(TwapPreview {
            symbol: symbol.to_string(),
            twap_price,
            sample_count: window_samples.len(),
            coverage,
            in_settlement_window,
        })
    }

    /// Clear all samples for an asset (call after settlement).
    pub fn clear(&mut self, symbol: &str) {
        self.samples.remove(symbol);
        self.last_sample_time.remove(symbol);
        info!("Cleared TWAP samples for {}", symbol);
    }

    /// Clear all samples older than the given timestamp.
    pub fn prune(&mut self, before_timestamp: i64) {
        for (symbol, samples) in self.samples.iter_mut() {
            let original_len = samples.len();
            samples.retain(|s| s.timestamp >= before_timestamp);
            let pruned = original_len - samples.len();
            if pruned > 0 {
                debug!("Pruned {} old samples for {}", pruned, symbol);
            }
        }
    }

    /// Get a snapshot of current samples for an asset (for debugging/verification).
    pub fn get_samples(&self, symbol: &str) -> Option<&Vec<TwapSample>> {
        self.samples.get(symbol)
    }
}

impl Default for TwapCalculator {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors that can occur during TWAP calculation.
#[derive(Debug, thiserror::Error)]
pub enum TwapError {
    #[error("No samples available for TWAP calculation")]
    NoSamples,

    #[error("Insufficient coverage: {actual:.1}% < {required:.1}% required")]
    InsufficientCoverage { actual: f64, required: f64 },
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

        let update1 = make_update("SOL", 200.0, 1000);
        let update2 = make_update("SOL", 201.0, 1001);
        let update3 = make_update("SOL", 202.0, 1002);

        assert!(calc.record(&update1));
        assert!(calc.record(&update2));
        assert!(calc.record(&update3));

        assert_eq!(calc.sample_count("SOL"), 3);
    }

    #[test]
    fn test_sample_interval() {
        let mut calc = TwapCalculator::new();

        // Same second should not record twice
        let update1 = make_update("SOL", 200.0, 1000);
        let update2 = make_update("SOL", 200.5, 1000); // Same timestamp

        assert!(calc.record(&update1));
        assert!(!calc.record(&update2)); // Should not record

        assert_eq!(calc.sample_count("SOL"), 1);
    }

    #[test]
    fn test_calculate_twap() {
        let mut calc = TwapCalculator::with_window(10); // 10 second window for testing

        // Record 10 samples over 10 seconds
        for i in 0..10 {
            let update = make_update("SOL", 200.0 + i as f64, 1000 + i);
            calc.record(&update);
        }

        let result = calc.calculate("SOL", 1009).unwrap();

        // Average of 200, 201, ..., 209 = 204.5
        assert!((result.twap_price - 204.5).abs() < 0.01);
        assert_eq!(result.sample_count, 10);
        assert!((result.coverage - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_validated_coverage() {
        let mut calc = TwapCalculator::with_window(100); // 100 second window

        // Only record 50 samples (50% coverage)
        for i in 0..50 {
            let update = make_update("SOL", 200.0, 1000 + i * 2); // Every 2 seconds
            calc.record(&update);
        }

        // Should fail validation (50% < 90%)
        let result = calc.calculate_validated("SOL", 1099);
        assert!(matches!(result, Err(TwapError::InsufficientCoverage { .. })));
    }

    #[test]
    fn test_prune_old_samples() {
        let mut calc = TwapCalculator::new();

        for i in 0..100 {
            let update = make_update("SOL", 200.0, 1000 + i);
            calc.record(&update);
        }

        assert_eq!(calc.sample_count("SOL"), 100);

        calc.prune(1050);

        assert_eq!(calc.sample_count("SOL"), 50);
    }
}
