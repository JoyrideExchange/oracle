# Joyride Oracle

The Joyride oracle workspace consumes real-time spot prices from [Pyth Network](https://pyth.network), calculates TWAP (Time-Weighted Average Price), and optionally broadcasts the resulting feed over WebSocket.

This repo is the reference implementation of the TWAP used for Joyride options settlement. It is public so that anyone can verify the math, inspect the feed mapping, and replicate the calculation.

> **The oracle is not a public websocket endpoint.** Public clients, including market makers, trading frontends, and third-party integrations, should use the unified surface at `wss://joyride.exchange/api/v1` and the REST endpoints under `https://joyride.exchange/api/v1/oracle/*` instead.

## Why TWAP?

Options settlement requires a manipulation-resistant price. A single spot price at expiration could be manipulated with a large trade. TWAP averages prices over 30 minutes, making manipulation expensive and impractical.

In addition to manipulation risk, options experience gamma explosion in the final minutes before expiration. Smoothing the settlement price near expiration reduces this gamma exposure and encourages makers to maintain their liquidity for traders.

## Features

- **Real-time price streaming** from Pyth Hermes SSE API
- **TWAP calculation** with rolling 30-minute window, 1-second samples
- **Embeddable core crate** for in-process TWAP consumption
- **WebSocket server** for broadcast/distributed deployments
- **Wire-format crate** for typed Rust consumers of the JSON feed
- **Multi-asset support**: BTC, ETH, SOL

## Workspace Layout

This repo is a Cargo workspace with three packages:

- `joyride-oracle-core` — the in-process API for embedders: Pyth ingestion, TWAP calculation, `Asset`, and `OracleEvent`
- `joyride-oracle-types` — the wire contract for the WebSocket feed: `BroadcastFrame` and `WirePayload`
- `joyride-oracle` — the service/transport crate: WebSocket server plus convenience re-exports of core and wire types

The workspace supports embedded/in-process usage, service/WebSocket usage, and typed wire-format consumption.

## Quick Start

To run the standalone WebSocket service locally, see [Service Usage](#service-usage).

## Configuration

| Environment Variable | Default | Description |
|---------------------|---------|-------------|
| `ORACLE_BIND_ADDR` | `0.0.0.0:8083` | WebSocket server bind address |

## Integration

An embedder can run `joyride-oracle-core` in process and use `TwapCalculator` directly. Distributed consumers can connect to the `joyride-oracle` service over WebSocket.

Internal configuration:

- set `ORACLE_WS_URL` on downstream services to point here
- local default: `ws://127.0.0.1:8083`
- optional: append `?client=<name>` for clearer server-side logs, for example `ws://127.0.0.1:8083?client=risk-engine`

## Embedded Usage

`joyride-oracle-core` provides Pyth ingestion and TWAP logic for in-process usage:

```rust
use joyride_oracle_core::{PythClient, TwapCalculator, Asset, OracleEvent};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let (tx, mut rx) = mpsc::channel(256);
    let assets = vec![Asset::Sol, Asset::Btc, Asset::Eth];

    let mut client = PythClient::new(tx, assets);
    let mut twap = TwapCalculator::new();

    tokio::spawn(async move {
        if let Err(e) = client.run().await {
            eprintln!("pyth client error: {e}");
        }
    });

    while let Some(event) = rx.recv().await {
        if let OracleEvent::Price(update) = event {
            twap.record(&update);
            println!("{}: ${:.2}", update.symbol, update.price);
        }
    }
}
```

You can also inspect raw samples via `TwapCalculator::get_samples()` if you want to validate or persist the calculation inputs.

## Service Usage

`joyride-oracle` is the binary form of this repo: it wires `PythClient` → `TwapCalculator` → a WebSocket fanout server and runs as a standalone process so multiple consumers can share one Pyth connection.

```bash
cargo run -p joyride-oracle
```

Binds `0.0.0.0:8083` by default (override with `ORACLE_BIND_ADDR`) and starts broadcasting price and TWAP data as soon as the first Pyth update arrives.

Connect to `ws://<host>:8083` to receive real-time events from the `joyride-oracle` service.

New websocket clients receive the latest cached spot prices and TWAP previews immediately after connect, before live ticks resume.

Note: The service owns its own event ingestion; running it alongside an embedded `joyride-oracle-core` in another process means two independent Hermes connections.

## Wire Usage

`joyride-oracle-types` provides the typed JSON contract for frames emitted by the server. Depend on it directly (not on `joyride-oracle`) if you only need to parse the WebSocket feed — it pulls in `serde` and `chrono` and nothing else.

```rust
use joyride_oracle_types::{BroadcastFrame, WirePayload};
use tokio_tungstenite::tungstenite::Message;

while let Some(Ok(Message::Text(text))) = ws.next().await {
    let frame: BroadcastFrame = serde_json::from_str(&text)?;
    match frame.payload {
        WirePayload::Price(update) => println!("{}: {}", update.symbol, update.price),
        WirePayload::TwapPreview(p) => println!("{} TWAP: {} ({:.1}% cov)", p.symbol, p.twap, p.coverage * 100.0),
        WirePayload::Heartbeat => {}
        _ => {}
    }
}
```

`frame.timestamp` is an RFC 3339 `DateTime<Utc>`, parsed automatically on deserialization. Compare against `PriceUpdate::publish_time` (Pyth's own timestamp, in seconds) to measure end-to-end latency.

### Event Types

Every broadcast message includes a top-level `timestamp` field: an RFC 3339 UTC timestamp (millisecond precision) recorded at the moment the server serialized the payload. Use it to measure end-to-end freshness — compare against `publish_time` on `price` events for Pyth-to-client latency.

**`price`** - Real-time spot price from Pyth (multiple per second)
```json
{
  "timestamp": "2026-04-20T12:34:56.789Z",
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
  "timestamp": "2026-04-20T12:34:56.789Z",
  "type": "twap_preview",
  "symbol": "SOL",
  "twap": 123.45,
  "sample_count": 1800,
  "coverage": 1.0
}
```

**`heartbeat`** - Text frame sent every 10 seconds indicating a healthy connection
```json
{
  "timestamp": "2026-04-20T12:34:56.789Z",
  "type": "heartbeat"
}
```

**`connected`** / **`disconnected`** / **`error`** - Status of the oracle's upstream connection to Pyth Hermes, not the consumer's connection to this server. Emitted on Pyth state transitions (edge-triggered, not replayed to new subscribers). The `error` payload carries a `message` field with the upstream error string. All three carry `timestamp`.

## TWAP Details

- **Window**: Rolling 30 minutes
- **Sample Rate**: 1 sample per second (1,800 samples fill the window)
- **Coverage**: `actual_samples / 1800`, included in every `twap_preview` payload. For example, a consumer could gate on `coverage >= 0.9` (1,620 samples) before using the TWAP.

## Architecture

Two deployment shapes, both built from the same core components. You can pick one or run both — nothing prevents an embedded process and the service binary from coexisting. Just note that each process maintains its own Pyth connection and its own TWAP state; there's no shared memory between them.

**Embedded:** your process owns a `PythClient` and a `TwapCalculator`. The client delivers `OracleEvent`s over an mpsc channel; your code reads them, feeds `Price` events into the calculator, and queries the calculator when it needs a TWAP. The calculator is a side-store you drive — not a pipeline stage events pass through.

```
Pyth Hermes ─SSE─▶ PythClient ─OracleEvent─▶ your code
                                                │  ▲
                                    .record(..) │  │ .calculate / .calculate_preview
                                                ▼  │
                                          TwapCalculator
```

**Service:** the `joyride-oracle` binary runs those components and fans the feed out over WebSocket. Two independent broadcast streams reach the server:

- an **ordered stream** carrying `Price`, `Connected`, `Disconnected`, and `Error` events in receive order;
- a **preview stream** driven by a 1 Hz timer that calls `TwapCalculator::calculate_preview` for each asset.

Price events do double duty: they're forwarded to the ordered stream *and* recorded into the calculator. The calculator itself is never on the wire path — only its sampled output (via the timer) is.

```
                          ┌──▶ ordered broadcast ─────┐
Pyth Hermes ─▶ PythClient ┤                            ├──▶ WS server ──▶ Gateway / Risk Engine / Dashboard
                          └──▶ TwapCalculator ──▶ preview broadcast ─┘
                                   ▲
                               1s timer
```

**Components:**

- **`joyride-oracle-core`** (`crates/core/`) - Pyth client, TWAP calculator, in-process domain types
- **`joyride-oracle-types`** (`crates/types/`) - typed wire contract for the WebSocket feed
- **`joyride-oracle`** (`src/server.rs`, `src/main.rs`) - WebSocket server and service binary

## Pyth Feed IDs

| Asset | Feed ID |
|-------|---------|
| SOL/USD | `0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d` |
| BTC/USD | `0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43` |
| ETH/USD | `0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace` |
