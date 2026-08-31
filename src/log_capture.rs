//! In-memory ring buffer of tunman's own tracing events, feeding the Log tab —
//! a live, filterable equivalent of the file log that needs no tailing.
//!
//! [`LogCaptureLayer`] is a third `tracing_subscriber::Layer` alongside the
//! file and stderr layers, so it sees exactly the same events under the same
//! `EnvFilter`: what the Log tab can show always equals what the file log
//! holds. Every event is copied into a bounded [`VecDeque`] behind one global
//! `Mutex`, oldest dropped at [`CAPACITY`].
//!
//! An SSH tunnel's own stdout/stderr arrives here too — `supervisor` re-emits
//! each child line as a tracing event carrying a `tunnel` field — which is
//! what makes "why did that tunnel drop?" answerable inside the app.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Ring capacity — a few hundred bytes per record, so tens of MB worst case
/// rather than unbounded growth across a session that runs for weeks. The
/// file log (7-day retention) is the durable record.
pub const CAPACITY: usize = 50_000;

/// One captured tracing event. `target` is `&'static str` from the event's
/// compile-time metadata, so storing it costs nothing.
pub struct LogRecord {
    pub seq: u64,
    pub time_ms: i64,
    pub level: tracing::Level,
    pub target: &'static str,
    pub message: String,
    /// The `tunnel` field, pulled out of the field set so the Log tab can
    /// filter on it without substring-matching the message.
    pub tunnel: Option<String>,
    /// Remaining fields, pre-joined as `"key=value key2=value2"` — cheaper
    /// than a map for a view that only displays and substring-searches them.
    pub fields: String,
}

/// Severity rank, most severe first. Deliberately independent of
/// `tracing::Level`'s own `Ord`, so "minimum level" filtering is obviously
/// correct here rather than relying on an externally defined direction.
pub fn level_rank(level: tracing::Level) -> u8 {
    match level {
        tracing::Level::ERROR => 0,
        tracing::Level::WARN => 1,
        tracing::Level::INFO => 2,
        tracing::Level::DEBUG => 3,
        tracing::Level::TRACE => 4,
    }
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

static SEQ: AtomicU64 = AtomicU64::new(0);
static BUFFER: OnceLock<Mutex<VecDeque<Arc<LogRecord>>>> = OnceLock::new();

fn buffer() -> &'static Mutex<VecDeque<Arc<LogRecord>>> {
    BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(1024)))
}

fn push(record: LogRecord) {
    let mut buf = buffer().lock().unwrap_or_else(|e| e.into_inner());
    if buf.len() >= CAPACITY {
        buf.pop_front();
    }
    buf.push_back(Arc::new(record));
}

/// Session-only mute list: substrings that stop matching events from being
/// captured at all — checked *before* `push`, so a runaway source stops
/// costing buffer churn and stops evicting everything else, not merely stops
/// being displayed. Not persisted: a mute rides out one noisy episode, and a
/// restart is a clean slate.
static MUTES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn mutes() -> &'static Mutex<Vec<String>> {
    MUTES.get_or_init(|| Mutex::new(Vec::new()))
}

fn matches_any_mute(list: &[String], haystack: &str) -> bool {
    if list.is_empty() {
        return false;
    }
    let lower = haystack.to_lowercase();
    list.iter().any(|p| lower.contains(&p.to_lowercase()))
}

pub fn is_muted(haystack: &str) -> bool {
    matches_any_mute(&mutes().lock().unwrap_or_else(|e| e.into_inner()), haystack)
}

/// Add a mute pattern and immediately purge everything already captured that
/// matches it — muting a runaway source should recover the view at once, not
/// merely stop it getting worse.
pub fn add_mute(pattern: &str) {
    let pattern = pattern.trim().to_string();
    if pattern.is_empty() {
        return;
    }
    {
        let mut list = mutes().lock().unwrap_or_else(|e| e.into_inner());
        if list.iter().any(|p| p.eq_ignore_ascii_case(&pattern)) {
            return;
        }
        list.push(pattern);
    }
    let list = mutes().lock().unwrap_or_else(|e| e.into_inner()).clone();
    buffer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|r| !matches_any_mute(&list, &format!("{} {} {}", r.message, r.fields, r.target)));
}

/// Remove the mute at `index`. Out-of-range is ignored: the list may have
/// changed under a stale UI snapshot, and the next frame is already current.
pub fn remove_mute(index: usize) {
    let mut list = mutes().lock().unwrap_or_else(|e| e.into_inner());
    if index < list.len() {
        list.remove(index);
    }
}

pub fn mute_list() -> Vec<String> {
    mutes().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A starting mute pattern for "lines like this one": `text` truncated at its
/// first `[` or digit — the point at which most messages stop being a fixed
/// description and start being per-event data. Editable afterwards; this only
/// has to be a good first guess.
pub fn suggested_mute_pattern(text: &str) -> String {
    let cut = text
        .char_indices()
        .find(|&(_, c)| c == '[' || c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    text[..cut].trim_end_matches([':', ',', '-', ' ']).to_string()
}

/// Every record held, oldest first. Cheap — clones `Arc` pointers, not records.
pub fn snapshot() -> Vec<Arc<LogRecord>> {
    buffer().lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
}

/// Records with `seq > since`, oldest first, for extending an already-filtered
/// view instead of rescanning the whole buffer every frame.
pub fn since(since: u64) -> Vec<Arc<LogRecord>> {
    let buf = buffer().lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<Arc<LogRecord>> =
        buf.iter().rev().take_while(|r| r.seq > since).cloned().collect();
    out.reverse();
    out
}

pub fn len() -> usize {
    buffer().lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// Drop everything captured so far. The file log is untouched.
pub fn clear() {
    buffer().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Every distinct tunnel name currently present in the buffer, sorted — the
/// Log tab's filter dropdown. Derived from the records rather than the config
/// so a tunnel that has since been deleted still lets you read back why.
pub fn tunnels_seen() -> Vec<String> {
    let buf = buffer().lock().unwrap_or_else(|e| e.into_inner());
    let mut names: Vec<String> = buf.iter().filter_map(|r| r.tunnel.clone()).collect();
    names.sort();
    names.dedup();
    names
}

/// Collects an event's `message` and `tunnel` fields into their own slots and
/// space-joins the rest as `name=value`.
#[derive(Default)]
struct FieldCollector {
    message: String,
    tunnel: Option<String>,
    fields: String,
}

impl FieldCollector {
    fn push(&mut self, name: &str, value: String) {
        match name {
            "message" => self.message = value,
            "tunnel" => self.tunnel = Some(value),
            _ => {
                if !self.fields.is_empty() {
                    self.fields.push(' ');
                }
                self.fields.push_str(name);
                self.fields.push('=');
                self.fields.push_str(&value);
            }
        }
    }
}

impl tracing::field::Visit for FieldCollector {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.push(field.name(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.push(field.name(), value.to_string());
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.push(field.name(), value.to_string());
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.push(field.name(), value.to_string());
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.push(field.name(), value.to_string());
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.push(field.name(), value.to_string());
    }
}

/// The third layer in `init_tracing`'s registry. Stateless — all state is in
/// the module statics above — so a unit struct rather than a handle.
pub struct LogCaptureLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogCaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut collector = FieldCollector::default();
        event.record(&mut collector);
        let target = event.metadata().target();
        let message = crate::logfmt::strip_ansi(&collector.message);
        let fields = crate::logfmt::strip_ansi(&collector.fields);
        if is_muted(&format!("{message} {fields} {target}")) {
            return;
        }
        push(LogRecord {
            seq: SEQ.fetch_add(1, Ordering::Relaxed),
            time_ms: now_unix_ms(),
            level: *event.metadata().level(),
            target,
            message,
            tunnel: collector.tunnel.map(|t| crate::logfmt::strip_ansi(&t)),
            fields,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_rank_orders_error_as_most_severe() {
        assert!(level_rank(tracing::Level::ERROR) < level_rank(tracing::Level::WARN));
        assert!(level_rank(tracing::Level::WARN) < level_rank(tracing::Level::INFO));
        assert!(level_rank(tracing::Level::INFO) < level_rank(tracing::Level::DEBUG));
        assert!(level_rank(tracing::Level::DEBUG) < level_rank(tracing::Level::TRACE));
    }

    /// The tunnel name has to leave the field soup, or the Log tab's per-tunnel
    /// filter would be a substring match on the message and would happily match
    /// a tunnel whose name appears in some other tunnel's error text.
    #[test]
    fn the_tunnel_field_gets_its_own_slot() {
        let mut c = FieldCollector::default();
        c.push("message", "channel 3: open failed".to_string());
        c.push("tunnel", "vps-fi".to_string());
        c.push("pid", "4180".to_string());
        assert_eq!(c.message, "channel 3: open failed");
        assert_eq!(c.tunnel.as_deref(), Some("vps-fi"));
        assert_eq!(c.fields, "pid=4180");
    }

    #[test]
    fn suggested_mute_stops_at_the_first_variable_part() {
        assert_eq!(
            suggested_mute_pattern("connect to host example.org port 22: timed out"),
            "connect to host example.org port"
        );
        assert_eq!(suggested_mute_pattern("no digits here"), "no digits here");
    }
}
