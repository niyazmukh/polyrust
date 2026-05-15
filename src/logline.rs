//! Compact key=value structured line logger for the hot path.
//!
//! Mirrors the Python `log_utils.log_event` shape so analyzers that already
//! parse the Python live logs can ingest Rust output unchanged:
//!
//! ```text
//! ts_us=1778137224437507 level=INFO event=binance_signal_decision \
//!   action=BUY reason=edge_ok side=YES token_id=...
//! ```
//!
//! Phase 1 implementation: write to stderr, level filter from env. Hot path
//! callers must avoid allocating per call beyond the line itself; we use
//! `std::io::Write` directly with a stack-buffered formatter for typical
//! log sizes.

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 40,
    Warn = 30,
    Info = 20,
    Debug = 10,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARNING",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }
}

// Threshold below which logs are dropped. Stored as a simple atomic so the
// hot path doesn't lock to read it.
static LEVEL_THRESHOLD: AtomicU8 = AtomicU8::new(Level::Warn as u8);

static LOG_SENDER: OnceLock<SyncSender<Vec<u8>>> = OnceLock::new();

/// Starts the non-blocking background logger thread to take stderr I/O off the hot path.
pub fn init_background_logger() {
    let (tx, rx) = sync_channel(8192);
    if LOG_SENDER.set(tx).is_ok() {
        std::thread::spawn(move || {
            let mut stderr = std::io::stderr();
            for msg in rx {
                let _ = stderr.write_all(&msg);
            }
        });
    }
}

pub fn set_level(level: Level) {
    LEVEL_THRESHOLD.store(level as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    (level as u8) >= LEVEL_THRESHOLD.load(Ordering::Relaxed)
}

/// Single key/value entry for a log line.
pub struct Field<'a> {
    pub key: &'a str,
    pub value: &'a dyn FieldValue,
}

/// Renderable field values. Concrete impls below cover the types we need
/// without dragging in `serde`.
pub trait FieldValue {
    fn write(&self, w: &mut dyn Write) -> std::io::Result<()>;
}

impl FieldValue for &str {
    fn write(&self, w: &mut dyn Write) -> std::io::Result<()> {
        write_quoted(w, self)
    }
}

impl FieldValue for String {
    fn write(&self, w: &mut dyn Write) -> std::io::Result<()> {
        write_quoted(w, self.as_str())
    }
}

impl FieldValue for bool {
    fn write(&self, w: &mut dyn Write) -> std::io::Result<()> {
        w.write_all(if *self { b"true" } else { b"false" })
    }
}

macro_rules! impl_field_value_int {
    ($($t:ty),*) => {
        $(impl FieldValue for $t {
            fn write(&self, w: &mut dyn Write) -> std::io::Result<()> {
                write!(w, "{}", self)
            }
        })*
    };
}
impl_field_value_int!(i32, i64, u32, u64, usize, isize);

impl FieldValue for f64 {
    fn write(&self, w: &mut dyn Write) -> std::io::Result<()> {
        // Use a fixed-precision render so analyzers can parse without surprises.
        write!(w, "{:.4}", self)
    }
}

/// Write a value with shell-safe quoting — only quote if it contains
/// whitespace or our delimiters. Common case of ASCII identifiers stays
/// allocation-free.
fn write_quoted(w: &mut dyn Write, s: &str) -> std::io::Result<()> {
    if s.is_empty() {
        return w.write_all(b"-");
    }
    let needs_quote = s
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b == b'"' || b == b'=' || b == b'\\');
    if !needs_quote {
        return w.write_all(s.as_bytes());
    }
    w.write_all(b"\"")?;
    for c in s.chars() {
        match c {
            '\\' => w.write_all(b"\\\\")?,
            '"' => w.write_all(b"\\\"")?,
            '\n' => w.write_all(b"\\n")?,
            _ => write!(w, "{c}")?,
        }
    }
    w.write_all(b"\"")?;
    Ok(())
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Emit a structured log line if its level is at or above the threshold.
pub fn log_event(level: Level, event: &str, fields: &[Field<'_>]) {
    if !enabled(level) {
        return;
    }
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let _ = write!(
        &mut buf,
        "ts_us={} level={} event={}",
        now_us(),
        level.as_str(),
        event
    );
    for Field { key, value } in fields {
        let _ = write!(&mut buf, " {key}=");
        let _ = value.write(&mut buf);
    }
    buf.push(b'\n');
    if let Some(tx) = LOG_SENDER.get() {
        let _ = tx.try_send(buf);
    } else {
        let _ = std::io::stderr().write_all(&buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_only_when_needed() {
        let mut buf = Vec::new();
        write_quoted(&mut buf, "yes").unwrap();
        assert_eq!(&buf, b"yes");

        let mut buf = Vec::new();
        write_quoted(&mut buf, "hello world").unwrap();
        assert_eq!(&buf, b"\"hello world\"");

        let mut buf = Vec::new();
        write_quoted(&mut buf, "").unwrap();
        assert_eq!(&buf, b"-");
    }

    #[test]
    fn level_round_trip() {
        set_level(Level::Info);
        assert_eq!(LEVEL_THRESHOLD.load(Ordering::Relaxed), Level::Info as u8);
        set_level(Level::Warn);
        assert_eq!(LEVEL_THRESHOLD.load(Ordering::Relaxed), Level::Warn as u8);
    }
}
