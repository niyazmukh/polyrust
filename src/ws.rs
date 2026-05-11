//! Shared WebSocket primitives — connect, backoff, ping.
//!
//! Traces to:
//!   market_ws.py:376-436     (connect, backoff: 0.5s→1.8x→10s)
//!   binance_sbe_listener.py:590-614  (connect, backoff: 0.25s→1.7x→8s)
//!   user_channel_ws.py:131-193       (connect, backoff: 0.25s→1.7x→5s)

use std::time::Duration;

use futures_util::SinkExt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    Message,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

// ---------------------------------------------------------------------------
// Connect
// ---------------------------------------------------------------------------

/// Open a WebSocket connection with an optional Binance API key header.
///
/// Traces to: `websockets.connect(url, extra_headers=headers)` in
///   binance_sbe_listener.py:593-602.
pub async fn connect(url: &str, api_key: Option<&str>) -> Result<WsStream, String> {
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("invalid ws url: {e}"))?;
    if let Some(key) = api_key {
        request.headers_mut().insert(
            "X-MBX-APIKEY",
            key.parse().map_err(|e| format!("invalid api key: {e}"))?,
        );
    }
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("ws connect failed: {e}"))?;
    Ok(ws)
}

// ---------------------------------------------------------------------------
// Exponential backoff
// ---------------------------------------------------------------------------

/// Exponential backoff with a configured ceiling.
///
/// Traces to:
///   market_ws.py:376       (0.5s initial, 1.8x factor, 10s max)
///   binance_sbe_listener.py:590  (0.25s initial, 1.7x factor, 8s max)
///   user_channel_ws.py:131       (0.25s initial, 1.7x factor, 5s max)
pub struct Backoff {
    initial: Duration,
    current: Duration,
    factor: f64,
    max: Duration,
}

impl Backoff {
    pub fn new(initial_ms: f64, factor: f64, max_ms: f64) -> Self {
        let initial = Duration::from_secs_f64(initial_ms / 1000.0);
        Self {
            initial,
            current: initial,
            factor,
            max: Duration::from_secs_f64(max_ms / 1000.0),
        }
    }

    /// Return the current delay and advance for the next call.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.current;
        let next = Duration::from_secs_f64(self.current.as_secs_f64() * self.factor);
        self.current = if next > self.max { self.max } else { next };
        d
    }

    /// Reset to the initial delay after a successful connection.
    pub fn reset(&mut self) {
        self.current = self.initial;
    }
}

// ---------------------------------------------------------------------------
// PING presets (matches Python constants exactly)
// ---------------------------------------------------------------------------

/// Polymarket market + user WS both use 10 s ping interval.
/// Traces to: market_ws.py:23 (PING_INTERVAL_S = 10.0),
///   user_channel_ws.py:17 (APP_PING_INTERVAL_S = 10.0).
pub const POLY_PING_INTERVAL_S: f64 = 10.0;

// Binance book-ticker feed does not use app-level ping
// (binance_sbe_listener.py: ping_interval=None at line 597).

// ---------------------------------------------------------------------------
// Helpers for feed loops
// ---------------------------------------------------------------------------

/// Send a JSON text frame through the write half. Used for initial subscribe
/// and auth frames.
pub async fn send_text(
    write: &mut futures_util::stream::SplitSink<WsStream, Message>,
    text: &str,
) -> Result<(), String> {
    write
        .send(Message::Text(text.to_owned().into()))
        .await
        .map_err(|e| format!("ws send failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_converges_to_max() {
        let mut b = Backoff::new(500.0, 1.8, 10_000.0);
        assert_eq!(b.next_delay(), Duration::from_millis(500));
        assert_eq!(b.next_delay(), Duration::from_millis(900)); // 500 * 1.8
        assert_eq!(b.next_delay(), Duration::from_millis(1620)); // 900 * 1.8

        // Advance many times and verify it caps.
        for _ in 0..50 {
            b.next_delay();
        }
        assert_eq!(b.current, Duration::from_secs(10));
    }

    #[test]
    fn backoff_reset_restores_initial() {
        let mut b = Backoff::new(250.0, 1.7, 5_000.0);
        b.next_delay(); // 250
        b.next_delay(); // 425
        b.next_delay(); // 722
        b.reset();
        assert_eq!(b.current, Duration::from_millis(250));
    }

    #[test]
    fn backoff_binance_preset() {
        // binance_sbe_listener.py:590-591: reconnect_min_s=0.25, factor=1.7, max=8.0
        let mut b = Backoff::new(250.0, 1.7, 8_000.0);
        assert_eq!(b.next_delay(), Duration::from_millis(250));
        assert_eq!(b.next_delay(), Duration::from_millis(425));
        // Advance to cap
        for _ in 0..50 {
            b.next_delay();
        }
        assert_eq!(b.current, Duration::from_secs(8));
    }

    #[test]
    fn backoff_market_preset() {
        // market_ws.py:376: 0.5s initial, 1.8x, 10s max
        let mut b = Backoff::new(500.0, 1.8, 10_000.0);
        assert_eq!(b.next_delay(), Duration::from_millis(500));
        assert_eq!(b.next_delay(), Duration::from_millis(900));
    }

    #[test]
    fn backoff_user_preset() {
        // user_channel_ws.py:131: 0.25s initial, 1.7x, 5s max
        let mut b = Backoff::new(250.0, 1.7, 5_000.0);
        assert_eq!(b.next_delay(), Duration::from_millis(250));
        // Advance to cap
        for _ in 0..50 {
            b.next_delay();
        }
        assert_eq!(b.current, Duration::from_secs(5));
    }
}
