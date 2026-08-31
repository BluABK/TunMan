//! The 1 Hz tick behind the rate graph and the unmetered connection list.
//!
//! One thread, one ring of samples, and free functions that hand the UI an
//! owned copy. Nothing is pushed at the UI and no lock is held across a render
//! — the view reads a snapshot about once a second and draws from that.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::supervisor::{Command, Shared, Status};
use crate::usage::{CapAction, Ledger};

/// 30 minutes at one sample a second — enough to see a tunnel's shape over a
/// session without keeping an unbounded history.
pub const HISTORY_LEN: usize = 1800;

/// One tick.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub at_ms: i64,
    /// Per tunnel: bytes/sec in, bytes/sec out.
    pub rates: HashMap<String, (f64, f64)>,
}

static HISTORY: OnceLock<Mutex<VecDeque<Sample>>> = OnceLock::new();

fn history_ring() -> &'static Mutex<VecDeque<Sample>> {
    HISTORY.get_or_init(|| Mutex::new(VecDeque::with_capacity(HISTORY_LEN)))
}

/// The whole ring. Clone this about once a second and draw from the copy —
/// cloning it every frame is pure waste.
pub fn history() -> Vec<Sample> {
    history_ring().lock().iter().cloned().collect()
}

/// Just the most recent sample. O(1), for callers that only want current rates.
pub fn latest() -> Option<Sample> {
    history_ring().lock().back().cloned()
}

/// Bytes/sec for one tunnel, from the newest sample.
pub fn rate_of(name: &str) -> (f64, f64) {
    latest().and_then(|s| s.rates.get(name).copied()).unwrap_or((0.0, 0.0))
}

/// Rate between two cumulative readings.
///
/// **A newly seen tunnel reports zero, not its whole total.** The counters are
/// cumulative for the life of the tunnel, so treating the first reading as a
/// delta would post an entire session's bytes as one second of throughput —
/// a single spike that then dominates the graph's y-axis forever. A counter
/// that went backwards (a restart reset it) is treated the same way.
pub fn rate(prev: Option<u64>, now: u64, elapsed_secs: f64) -> f64 {
    let Some(prev) = prev else { return 0.0 };
    if now < prev || elapsed_secs <= 0.0 {
        return 0.0;
    }
    (now - prev) as f64 / elapsed_secs
}

/// The live usage ledger, shared by the sampler (which writes it) and the UI
/// (which reads it and forgets deleted tunnels).
static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();

fn ledger() -> &'static Mutex<Ledger> {
    LEDGER.get_or_init(|| {
        Mutex::new(Ledger::load(&crate::app_paths::usage_path()).unwrap_or_default())
    })
}

/// Drop a deleted tunnel's usage history, so the file does not accumulate
/// entries under names nothing refers to any more.
pub fn forget_tunnel(name: &str) {
    ledger().lock().forget(name);
    let _ = ledger().lock().save(&crate::app_paths::usage_path());
}

/// Persist usage now — on the way out, so the last few minutes are not lost.
pub fn flush_usage() {
    let _ = ledger().lock().save(&crate::app_paths::usage_path());
}

/// How often the ledger is written. Every tick would be a file write a second
/// for numbers that only matter to the minute; a minute of loss on a hard kill
/// is an acceptable trade against that.
const SAVE_EVERY_TICKS: u32 = 60;

/// Start the sampler. Idempotent — a second call does nothing.
pub fn start(shared: Arc<Shared>, commands: tokio::sync::mpsc::Sender<Command>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || sampler_loop(shared, commands))
        .expect("spawn sampler");
}

fn sampler_loop(shared: Arc<Shared>, commands: tokio::sync::mpsc::Sender<Command>) {
    let mut prev: HashMap<String, (u64, u64)> = HashMap::new();
    let mut last_tick = std::time::Instant::now();
    let self_pid = std::process::id();
    let mut ticks: u32 = 0;

    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let elapsed = last_tick.elapsed().as_secs_f64();
        last_tick = std::time::Instant::now();

        // One TCP-table read and one process-list read per tick, shared by
        // every tunnel — per-tunnel calls would be the same data N times.
        let rows: Vec<(u16, u32, u16, u32, u32)> = crate::platform::tcp_table()
            .into_iter()
            .map(|r| (r.local_port, r.remote_addr, r.remote_port, r.pid, r.state))
            .collect();
        let names: HashMap<u32, String> =
            crate::platform::process_tree_snapshot().into_iter().map(|p| (p.pid, p.name)).collect();

        let states: Vec<_> = shared.states.lock().values().cloned().collect();
        let mut rates = HashMap::new();
        let now = crate::util::now_unix();
        ticks = ticks.wrapping_add(1);

        for st in states {
            // TunMan holds both ends of the loopback pair when metering, and
            // ssh holds the far end always; neither is a "user" of the tunnel.
            let mut exclude = vec![self_pid];
            if let Some(pid) = st.pid {
                exclude.push(pid);
            }
            let observed: Vec<(u32, String, u64)> =
                crate::traffic::clients_of(&rows, st.port, &exclude)
                    .into_iter()
                    .map(|(pid, n)| {
                        (pid, names.get(&pid).cloned().unwrap_or_else(|| format!("pid {pid}")), n)
                    })
                    .collect();
            *st.traffic.observed.lock() = observed;

            let now_in = st.traffic.total_in.load(Ordering::Relaxed);
            let now_out = st.traffic.total_out.load(Ordering::Relaxed);
            let last = prev.get(&st.name).copied();
            rates.insert(
                st.name.clone(),
                (
                    rate(last.map(|(i, _)| i), now_in, elapsed),
                    rate(last.map(|(_, o)| o), now_out, elapsed),
                ),
            );

            // Bank the delta against the caps. Same first-sight rule as the
            // rates: a tunnel seen for the first time contributes nothing, or
            // a restart would charge its whole previous lifetime to this hour.
            if let Some((pi, po)) = last {
                let mut led = ledger().lock();
                led.add(&st.name, now, now_in.saturating_sub(pi), now_out.saturating_sub(po));
            }
            prev.insert(st.name.clone(), (now_in, now_out));

            let status = crate::usage::status(&ledger().lock(), &st.name, &st.cap_config, now);
            let over = status.exceeded.is_some();
            let action = st.cap_config.action;
            let was_over = st.caps.exceeded.is_some();
            let up = st.status == Status::Up;

            shared.update(&st.name, |s| {
                s.caps = status;
                s.tick_availability(up);
                // Recomputed every tick rather than latched, so a rolling
                // window that has moved on lifts the block on its own.
                s.blocked.store(over && action == CapAction::BlockNew, Ordering::Relaxed);
            });

            if over && !was_over {
                tracing::warn!(
                    tunnel = %st.name,
                    action = action.label(),
                    "bandwidth cap reached"
                );
            }

            if action == CapAction::Stop {
                if over && !st.stopped_by_cap && up {
                    shared.update(&st.name, |s| s.stopped_by_cap = true);
                    let _ = commands.try_send(Command::Stop(st.name.clone()));
                } else if !over && st.stopped_by_cap {
                    // The window rolled over; bring it back rather than
                    // leaving it down until someone notices.
                    tracing::info!(tunnel = %st.name, "back under its cap — restarting");
                    shared.update(&st.name, |s| s.stopped_by_cap = false);
                    let _ = commands.try_send(Command::Start(st.name.clone()));
                }
            }
        }

        if ticks.is_multiple_of(SAVE_EVERY_TICKS) {
            let mut led = ledger().lock();
            led.prune(now);
            let _ = led.save(&crate::app_paths::usage_path());
        }

        let mut ring = history_ring().lock();
        if ring.len() >= HISTORY_LEN {
            ring.pop_front();
        }
        ring.push_back(Sample { at_ms: crate::util::now_unix() * 1000, rates });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first reading of a cumulative counter is not a delta. Reporting it
    /// as one turns a re-attached tunnel's lifetime total into a single
    /// multi-gigabyte-per-second spike that flattens the graph forever after.
    #[test]
    fn a_newly_seen_tunnel_reports_zero_not_its_whole_total() {
        assert_eq!(rate(None, 12_000_000_000, 1.0), 0.0);
    }

    #[test]
    fn a_rate_is_the_delta_over_the_interval() {
        assert_eq!(rate(Some(1000), 3000, 2.0), 1000.0);
        assert_eq!(rate(Some(0), 500, 1.0), 500.0);
    }

    /// A restart resets the counters. Without this the next tick computes a
    /// huge negative delta, which as unsigned arithmetic would underflow.
    #[test]
    fn a_counter_that_went_backwards_reports_zero() {
        assert_eq!(rate(Some(5000), 10, 1.0), 0.0);
    }

    /// A stalled or double-fired tick must not divide by zero.
    #[test]
    fn a_zero_length_interval_reports_zero() {
        assert_eq!(rate(Some(0), 1000, 0.0), 0.0);
    }
}
