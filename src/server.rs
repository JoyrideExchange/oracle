//! WebSocket server for broadcasting oracle data to dashboard clients.

use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{debug, error, info};

use crate::types::OracleEvent;

/// Run the WebSocket server for broadcasting oracle events.
pub async fn run_server(addr: &str, mut event_rx: broadcast::Receiver<OracleEvent>) {
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind server to {}: {}", addr, e);
            return;
        }
    };

    info!("Oracle WebSocket server listening on {}", addr);

    // Broadcast channel for clients
    let (client_tx, _) = broadcast::channel::<String>(256);
    let client_tx_clone = client_tx.clone();

    // Forward oracle events to client broadcast
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    if let Ok(json) = serde_json::to_string(&event) {
                        let _ = client_tx_clone.send(json);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to accept connection: {}", e);
                continue;
            }
        };

        let mut client_rx = client_tx.subscribe();

        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, peer_addr, &mut client_rx).await {
                debug!("Client {} error: {}", peer_addr, e);
            }
        });
    }
}

async fn handle_client(
    stream: TcpStream,
    peer_addr: SocketAddr,
    data_rx: &mut broadcast::Receiver<String>,
) -> anyhow::Result<()> {
    let ws_stream = accept_async(stream).await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    debug!("Client {} connected", peer_addr);

    loop {
        tokio::select! {
            // Forward data to client
            data = data_rx.recv() => {
                match data {
                    Ok(json) => {
                        if ws_sender.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }

            // Handle incoming messages (ping/pong and close)
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }

    debug!("Client {} disconnected", peer_addr);
    Ok(())
}
