# Joyride Oracle

The Joyride price oracle service streams real-time spot prices from [Pyth Network](https://pyth.network) and calculates TWAP (Time-Weighted Average Price).

## Why TWAP?

Options settlement requires a manipulation-resistant price. A single spot price at expiration could be manipulated with a large trade. TWAP averages prices over 30 minutes, making manipulation expensive and impractical.

In addition to manipulation risk, options experience gamma explosion in the final minutes before expiration. Smoothing the settlement price near expiration reduces this gamma exposure and encourages makers to maintain their liquidity for traders.

## Features

- **Real-time price streaming** from Pyth Hermes SSE API
- **TWAP calculation** with rolling 30-minute window, 1-second samples
- **WebSocket server** broadcasts prices and TWAP previews
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
| `RUST_LOG` | `info` | Log level (debug, info, warn, error) |

## Integration

The oracle is a pure price feed — it has no knowledge of rounds, settlement timing, or instruments. Settlement timing is owned by the settlement service; the gateway computes client-facing countdowns.

Downstream consumers connect via WebSocket:

- **Order Gateway** — forwards spot prices to clients, caches TWAP for settlement
- **Settlement Service** — reads TWAP coverage to decide when to settle
- **Risk Engine** — uses spot prices for Greeks calculation
- **Market Maker** — uses spot + TWAP for quoting
- **Dashboard** — displays live prices and TWAP previews

Internal configuration:

- set `ORACLE_WS_URL` on downstream services to point here
- local default: `ws://127.0.0.1:8083`
- optional: append `?client=<name>` for clearer server-side logs, for example `ws://127.0.0.1:8083?client=market-maker`

Public clients should not connect to the oracle directly in production. They should use the unified API surface on `joyride.exchange/api` instead:

- REST: `https://joyride.exchange/api/v1/oracle/prices`, `https://joyride.exchange/api/v1/oracle/twap-previews`, `https://joyride.exchange/api/v1/oracle/settlement`
- WebSocket: `wss://joyride.exchange/api/v1` with `spot.{asset}` and `settlement` subscriptions

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

New websocket clients receive the latest cached spot prices and TWAP previews immediately after connect, before live ticks resume.

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
  "coverage": 1.0
}
```

**`connected`** / **`disconnected`** / **`error`** - Connection status events

## TWAP Details

- **Window**: Rolling 30 minutes
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
                    ┌──────────────┬───────┼───────┬──────────────┐
                    │              │       │       │              │
                    ▼              ▼       ▼       ▼              ▼
             ┌──────────┐  ┌──────────┐  ┌───┐  ┌──────────┐  ┌──────────┐
             │ Gateway  │  │Settlement│  │ RE│  │  Market  │  │Dashboard │
             │          │  │ Service  │  │   │  │  Maker   │  │          │
             └──────────┘  └──────────┘  └───┘  └──────────┘  └──────────┘
```

**Components:**

- **Pyth Client** (`pyth.rs`) - Connects to Pyth Hermes via SSE, parses price updates
- **TWAP Calculator** (`twap.rs`) - Samples prices every second, maintains 30-minute rolling window
- **WebSocket Server** (`server.rs`) - Broadcasts events to connected clients

## Pyth Feed IDs

| Asset | Feed ID |
|-------|---------|
| SOL/USD | `0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d` |
| BTC/USD | `0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43` |
| ETH/USD | `0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace` |
