# Joyride Oracle

The Joyride price oracle service streams real-time spot prices from [Pyth Network](https://pyth.network) and calculates TWAP (Time-Weighted Average Price).

## Why TWAP?

Options settlement requires a manipulation-resistant price. A single spot price at expiration could be manipulated with a large trade. TWAP averages prices over 30 minutes, making manipulation expensive and impractical.

In addition to manipulation risk, options experience gamma explosion in the final minutes before expiration. Smoothing the settlement price near expiration reduces this gamma exposure and encourages makers to maintain their liquidity for traders.

## Features

- **Real-time price streaming** from Pyth Hermes SSE API
- **TWAP calculation** for settlement (30-minute window, 1-second samples)
- **WebSocket server** broadcasts prices and settlement countdowns
- **Multi-asset support**: BTC, ETH, SOL

## Quick Start

```bash
cargo run
```

The service starts a WebSocket server on port 8083 and begins streaming prices from Pyth.

## Configuration

| Environment Variable | Default | Description |
|---------------------|---------|-------------|
| `ORACLE_BIND_ADDR` | `0.0.0.0:8083` | WebSocket server bind address |
| `ROUND_DURATION_HOURS` | `24` | Round duration in hours (must match server) |
| `RUST_LOG` | `info` | Log level (debug, info, warn, error) |

## Integration

The Joyride server connects to the oracle WebSocket and uses the rolling 30-minute TWAP as the settlement price at round expiry. Clients discover the oracle URL from the server's `GET /api/v1/config` endpoint and connect directly for real-time display.

Set `ORACLE_WS_URL` in the server's `.env` to point to this service (default `ws://127.0.0.1:8083`). Clients do not need a separate oracle URL.

## Library Usage

```rust
use joyride_oracle::{PythClient, TwapCalculator, Asset, OracleEvent};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let (tx, mut rx) = mpsc::channel(256);
    let assets = vec![Asset::Sol, Asset::Btc, Asset::Eth];

    let mut client = PythClient::new(tx, assets);
    let mut twap = TwapCalculator::new();

    tokio::spawn(async move { client.run().await });

    while let Some(event) = rx.recv().await {
        if let OracleEvent::Price(update) = event {
            twap.record(&update);
            println!("{}: ${:.2}", update.symbol, update.price);
        }
    }
}
```

## WebSocket API

Connect to `ws://<host>:8083` to receive real-time events.

### Event Types

**`price`** - Real-time spot price from Pyth (multiple per second)
```json
{
  "type": "price",
  "symbol": "SOL",
  "price": 123.45,
  "confidence": 0.12,
  "publish_time": 1706198400,
  "feed_id": "0xef0d8b6fda..."
}
```
- `confidence` is from Pyth's publisher network - lower values mean more agreement between data sources.

**`twap_preview`** - Rolling 30-minute TWAP (updated every second)
```json
{
  "type": "twap_preview",
  "symbol": "SOL",
  "twap_price": 123.45,
  "sample_count": 1800,
  "coverage": 1.0,
  "in_settlement_window": false
}
```
- `in_settlement_window` is true during the final 30 minutes before settlement (23:30-00:00 UTC)

**`settlement`** - Countdown timers (updated every second)
```json
{
  "type": "settlement",
  "next_settlement": 1706227200,
  "twap_window_start": 1706225400,
  "seconds_to_twap_window": 3600,
  "seconds_to_settlement": 5400,
  "in_twap_window": false
}
```

**`connected`** / **`disconnected`** / **`error`** - Connection status events

## Settlement Timing

- **Settlement**: Every `ROUND_DURATION_HOURS` hours, anchored to Jan 1, 2025 08:00 UTC (same epoch as the server)
- **TWAP Window**: 30 minutes before each settlement (or full round for rounds shorter than 30 min)
- **Sample Rate**: 1 sample per second
- **Minimum Coverage**: 90% (1,620 of 1,800 samples required for a 30-min window)

## Architecture

```
┌─────────────────┐     SSE      ┌─────────────────┐
│  Pyth Hermes    │─────────────▶│   Pyth Client   │
│  (price feed)   │              │                 │
└─────────────────┘              └────────┬────────┘
                                          │
                                          ▼
                                 ┌─────────────────┐
                                 │ TWAP Calculator │
                                 │  (30m window)   │
                                 └────────┬────────┘
                                          │
                    ┌─────────────────────┼─────────────────────┐
                    │                     │                     │
                    ▼                     ▼                     ▼
           ┌──────────────┐      ┌──────────────┐      ┌──────────────┐
           │  Dashboard   │      │ Risk Engine  │      │   Clients    │
           │  (WebSocket) │      │  (internal)  │      │  (WebSocket) │
           └──────────────┘      └──────────────┘      └──────────────┘
```

**Components:**

- **Pyth Client** (`pyth.rs`) - Connects to Pyth Hermes via SSE, parses price updates
- **TWAP Calculator** (`twap.rs`) - Samples prices every second, maintains 30-minute rolling window
- **WebSocket Server** (`server.rs`) - Broadcasts events to connected clients
- **Settlement Timer** (`main.rs`) - Calculates countdown to next settlement window

## Pyth Feed IDs

| Asset | Feed ID |
|-------|---------|
| SOL/USD | `0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d` |
| BTC/USD | `0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43` |
| ETH/USD | `0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace` |
