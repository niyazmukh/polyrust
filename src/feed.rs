//! WebSocket feed loops for all three connections.
//!
//! Each function owns one WS connection with its own exponential-backoff
//! reconnect loop. Callbacks are synchronous (`Fn(Bytes)`) — the caller
//! acquires shared state inside the closure. PING/PONG is handled
//! internally via `tokio::select!`.
//!
//! The market feed accepts an optional `outgoing` mpsc channel for
//! mid-connection subscribe commands — the maintenance task sends
//! incremental subscribe frames through it on market rotation.
//!
//! Traces to:
//!   market_ws.py:370-436          (market_feed_loop)
//!   market_ws.py:284-302         (incremental subscribe on rotation)
//!   binance_sbe_listener.py:583-614 (binance_feed_loop, adapted to JSON)
//!   user_channel_ws.py:117-193    (user_feed_loop)

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::logline::{self, Field, Level};
use crate::ws::{self, Backoff};

// ---------------------------------------------------------------------------
// Polymarket Market Feed
// ---------------------------------------------------------------------------

/// Run the Polymarket market-channel WebSocket feed.
///
/// - URL: `wss://ws-subscriptions-clob.polymarket.com/ws/market`
/// - Calls `current_tokens()` on each (re)connect for the initial subscribe.
/// - `outgoing` channel receives JSON text frames to send mid-connection.
///   The maintenance task sends incremental `{"operation":"subscribe",...}`
///   frames on market rotation.
/// - App-level PING every 10 s.
/// - Backoff: 0.5 s initial → 1.8× → 10 s max.
///
/// Traces to: market_ws.py:370-436 (feed loop),
///   market_ws.py:284-302 (incremental subscribe on rotation).
pub async fn market_feed_loop(
    url: &str,
    current_tokens: impl Fn() -> Vec<String> + Send + 'static,
    on_event: impl Fn(Bytes) + Send + 'static,
    on_disconnect: impl Fn() + Send + 'static,
    mut outgoing: mpsc::UnboundedReceiver<String>,
) {
    let mut backoff = Backoff::new(500.0, 1.8, 10_000.0);
    loop {
        match ws::connect(url, None).await {
            Ok(ws) => {
                backoff.reset();
                let (mut write, mut read) = ws.split();

                // Subscribe to current tokens (fresh on each reconnect).
                let assets = current_tokens();
                if !assets.is_empty() {
                    let subscribe = serde_json::json!({
                        "assets_ids": assets,
                        "type": "market",
                        "custom_feature_enabled": true,
                    });
                    if ws::send_text(&mut write, &subscribe.to_string())
                        .await
                        .is_err()
                    {
                        on_disconnect();
                        tokio::time::sleep(backoff.next_delay()).await;
                        continue;
                    }
                }

                let mut ping = tokio::time::interval(std::time::Duration::from_secs_f64(
                    ws::POLY_PING_INTERVAL_S,
                ));
                ping.tick().await; // skip initial burst

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(msg)) if msg.is_text() || msg.is_binary() => {
                                    on_event(msg.into_data());
                                }
                                Some(Ok(_)) => {
                                    // PONG or other control frame — discard.
                                }
                                Some(Err(_)) | None => break,
                            }
                        }
                        _ = ping.tick() => {
                            if write.send(Message::Text("PING".into())).await.is_err() {
                                break;
                            }
                        }
                        cmd = outgoing.recv() => {
                            match cmd {
                                Some(text) => {
                                    // Send incremental subscribe / control frame.
                                    // Traces to: market_ws.py:293-302.
                                    if write.send(Message::Text(text.into())).await.is_err() {
                                        break;
                                    }
                                }
                                None => break, // channel closed — disconnect
                            }
                        }
                    }
                }
            }
            Err(_) => {
                // connect failed — backoff and retry.
            }
        }
        on_disconnect();
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

// ---------------------------------------------------------------------------
// Binance Book-Ticker Feed
// ---------------------------------------------------------------------------

/// Run the Binance `@bookTicker` WebSocket feed (JSON protocol).
///
/// - URL: `wss://stream-sbe.binance.com:9443/ws/{symbol}@bestBidAsk` (SBE binary)
/// - No app-level ping (relies on TCP keepalive).
/// - Backoff: 0.25 s initial → 1.7× → 8 s max.
///
/// Traces to: binance_sbe_listener.py:583-614.
/// NOTE: Python uses SBE binary; Rust uses JSON @bookTicker which
/// `binance.rs::parse_book_ticker` already handles.
pub async fn binance_feed_loop(
    url: &str,
    api_key: Option<&str>,
    on_ticker: impl Fn(Bytes) + Send + 'static,
) {
    let mut backoff = Backoff::new(250.0, 1.7, 8_000.0);
    loop {
        match ws::connect(url, api_key).await {
            Ok(ws) => {
                backoff.reset();
                logline::log_event(Level::Warn, "binance_ws_connected", &[]);
                let (_write, mut read) = ws.split();
                // Binance @bookTicker does not need app-level PING.
                loop {
                    match read.next().await {
                        Some(Ok(msg)) if msg.is_text() || msg.is_binary() => {
                            on_ticker(msg.into_data());
                        }
                        Some(Ok(_)) => {
                            // Control frame — discard.
                        }
                        Some(Err(_)) | None => break,
                    }
                }
            }
            Err(e) => {
                logline::log_event(
                    Level::Error,
                    "binance_ws_connect_failed",
                    &[Field {
                        key: "err",
                        value: &e.as_str(),
                    }],
                );
            }
        }
        logline::log_event(Level::Warn, "binance_ws_disconnected", &[]);
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

// ---------------------------------------------------------------------------
// Polymarket User Feed
// ---------------------------------------------------------------------------

/// Run the Polymarket user-channel WebSocket feed.
///
/// - URL: `wss://ws-subscriptions-clob.polymarket.com/ws/user`
/// - Sends auth frame: `{"auth": {"apiKey":..., "secret":..., "passphrase":...}, "type": "user"}`
/// - `on_auth_frame_sent` fires after the frame is sent — NOT a trust grant.
///   Trust is granted only when the caller receives `UserMessage::AuthSuccess`
///   via `on_event`, and revoked on disconnect via `on_disconnect`.
/// - App-level PING every 10 s.
/// - Backoff: 0.25 s initial → 1.7× → 5 s max.
///
/// Traces to: user_channel_ws.py:117-193.
pub async fn user_feed_loop(
    url: &str,
    api_key: &str,
    api_secret: &str,
    api_passphrase: &str,
    on_event: impl Fn(Bytes) + Send + 'static,
    on_auth_frame_sent: impl Fn() + Send + 'static,
    on_disconnect: impl Fn() + Send + 'static,
) {
    let mut backoff = Backoff::new(250.0, 1.7, 5_000.0);
    // Pre-render the auth frame — credentials don't change across reconnects.
    let auth_frame = serde_json::json!({
        "auth": {
            "apiKey": api_key,
            "secret": api_secret,
            "passphrase": api_passphrase,
        },
        "type": "user",
    })
    .to_string();

    loop {
        match ws::connect(url, None).await {
            Ok(ws) => {
                backoff.reset();
                logline::log_event(Level::Warn, "user_ws_connected", &[]);
                let (mut write, mut read) = ws.split();

                // Send auth frame.
                if ws::send_text(&mut write, &auth_frame).await.is_err() {
                    logline::log_event(Level::Error, "user_ws_auth_send_failed", &[]);
                    tokio::time::sleep(backoff.next_delay()).await;
                    continue;
                }
                logline::log_event(Level::Warn, "user_ws_auth_sent", &[]);
                on_auth_frame_sent();

                let mut ping = tokio::time::interval(std::time::Duration::from_secs_f64(
                    ws::POLY_PING_INTERVAL_S,
                ));
                ping.tick().await; // skip initial burst

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(msg)) if msg.is_text() || msg.is_binary() => {
                                    on_event(msg.into_data());
                                }
                                Some(Ok(_)) => {
                                    // PONG / control — discard.
                                }
                                Some(Err(_)) | None => break,
                            }
                        }
                        _ = ping.tick() => {
                            if write.send(Message::Text("PING".into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // connect failed — backoff and retry.
                logline::log_event(
                    Level::Error,
                    "user_ws_connect_failed",
                    &[Field {
                        key: "err",
                        value: &e.as_str(),
                    }],
                );
            }
        }
        on_disconnect();
        logline::log_event(Level::Warn, "user_ws_disconnected", &[]);
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    async fn binance_feed_receives_frames() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}");

        // Server: accept one connection, send a bookTicker frame, then close.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = accept_async(stream).await.unwrap();
            let (mut write, _read) = ws.split();
            write
                .send(Message::Text(
                    r#"{"e":"bookTicker","u":1,"E":1777000030000000,"b":"100.00","B":"3.0","a":"101.00","A":"1.0"}"#
                        .into(),
                ))
                .await
                .unwrap();
            // Close the connection so the feed's read.next() returns None
            // and triggers reconnect (which we abort before it happens).
            drop(write);
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let feed_url = url.clone();
        let feed = tokio::spawn(async move {
            binance_feed_loop(&feed_url, None, move |data| {
                let _ = tx.send(data);
            })
            .await;
        });

        // Wait for the frame to arrive.
        let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert!(received.to_vec().windows(11).any(|w| w == b"\"bookTicker"));
        feed.abort();
        server.abort();
    }

    #[tokio::test]
    async fn user_feed_sends_auth_frame_on_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}");

        // Single-shot server: accept one connection, read the first frame.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = accept_async(stream).await.unwrap();
            let (_write, mut read) = ws.split();
            read.next().await // first frame should be auth
        });

        // Spawn feed — will send auth then block waiting for echo.
        let feed = tokio::spawn(async move {
            user_feed_loop(&url, "key", "secret", "phrase", |_| {}, || {}, || {}).await;
        });

        // Wait for server to receive auth frame.
        let first_frame = tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();

        match first_frame {
            Some(Ok(Message::Text(t))) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                assert_eq!(v["auth"]["apiKey"], "key");
                assert_eq!(v["auth"]["secret"], "secret");
                assert_eq!(v["auth"]["passphrase"], "phrase");
                assert_eq!(v["type"], "user");
            }
            other => panic!("expected auth text frame, got {other:?}"),
        }

        feed.abort();
    }
}
