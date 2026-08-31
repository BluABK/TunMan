//! Bandwidth accounting and caps.
//!
//! **This has to survive a restart or it is decoration.** A monthly cap that
//! forgets everything when you reboot cannot protect a box from being billed,
//! so usage is kept in hour-sized buckets on disk and summed on demand. Hours
//! are the right grain: small enough that a rolling hour is accurate to the
//! minute you care about, large enough that a month is ~744 numbers per tunnel.
//!
//! **Only metered tunnels have numbers to count.** Windows exposes no
//! per-socket byte totals, so an unmetered tunnel contributes nothing here and
//! its caps are unenforceable — the UI says so rather than showing a cap that
//! silently does nothing.
//!
//! The three windows are deliberately not all the same shape:
//!
//! - **Hourly** and **weekly** are *rolling* (the last 60 minutes, the last 7
//!   days). That is what protects a box from a burst — a calendar hour would
//!   reset at :00 and let you spend the cap twice across a minute boundary.
//! - **Monthly** is the *calendar* month, because that is how transfer quotas
//!   are actually billed. A rolling 30 days would refuse traffic on the 3rd for
//!   something spent on the 5th of the month before, which is not what the
//!   provider is measuring.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Seconds in an hour bucket.
const HOUR: i64 = 3600;

/// How much history to keep. Enough for a calendar month plus the slack to
/// still answer "last 7 days" on the 1st, and no more — this file is rewritten
/// periodically and there is no reason to grow it forever.
const KEEP_HOURS: i64 = 24 * 40;

/// Which window a cap applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    /// The last 60 minutes.
    Hour,
    /// The last 7 days.
    Week,
    /// This calendar month, as billed.
    Month,
}

impl Window {
    pub const ALL: [Window; 3] = [Window::Hour, Window::Week, Window::Month];

    pub fn label(self) -> &'static str {
        match self {
            Window::Hour => "Hourly",
            Window::Week => "Weekly",
            Window::Month => "Monthly",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Window::Hour => {
                "A rolling 60 minutes, not the clock hour — a clock hour resets at :00 and \
                 would let a burst spend the cap twice either side of the boundary."
            }
            Window::Week => "A rolling 7 days.",
            Window::Month => {
                "The calendar month, resetting on the 1st, because that is how transfer \
                 quotas are billed."
            }
        }
    }
}

/// What to do when a cap is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapAction {
    /// Refuse new connections; let what is in flight finish.
    #[default]
    BlockNew,
    /// Kill the tunnel outright.
    Stop,
    /// Badge and log it, keep passing traffic.
    WarnOnly,
}

impl CapAction {
    pub const ALL: [CapAction; 3] = [CapAction::BlockNew, CapAction::Stop, CapAction::WarnOnly];

    pub fn label(self) -> &'static str {
        match self {
            CapAction::BlockNew => "Block new connections",
            CapAction::Stop => "Stop the tunnel",
            CapAction::WarnOnly => "Warn only",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            CapAction::BlockNew => {
                "Transfers already running finish; new ones are refused. The tunnel stays \
                 up and recovers on its own when the window rolls over."
            }
            CapAction::Stop => {
                "ssh is killed and the tunnel goes down until the window rolls over. \
                 Guarantees nothing more is spent, at the cost of cutting whatever was \
                 in flight."
            }
            CapAction::WarnOnly => "Badge the row and log it, but keep passing traffic.",
        }
    }
}

/// Caps for one tunnel, in **mebibytes**. Zero means no cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Caps {
    pub hourly_mib: u64,
    pub weekly_mib: u64,
    pub monthly_mib: u64,
    pub action: CapAction,
    /// Count both directions against the cap. Off counts only what leaves,
    /// which is what asymmetric providers usually bill.
    pub count_both_directions: bool,
}

impl Caps {
    pub fn limit(&self, w: Window) -> u64 {
        match w {
            Window::Hour => self.hourly_mib,
            Window::Week => self.weekly_mib,
            Window::Month => self.monthly_mib,
        }
    }

    pub fn any_set(&self) -> bool {
        self.hourly_mib > 0 || self.weekly_mib > 0 || self.monthly_mib > 0
    }
}

/// One hour's traffic for one tunnel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub in_bytes: u64,
    pub out_bytes: u64,
}

/// The on-disk ledger: tunnel name → hour-start epoch → bytes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ledger {
    pub tunnels: HashMap<String, HashMap<i64, Bucket>>,
}

/// Start of the hour containing `now`.
pub fn hour_of(now: i64) -> i64 {
    now - now.rem_euclid(HOUR)
}

/// Start of the calendar month containing `now`, in local time.
///
/// Local, not UTC: a provider's month boundary follows a human calendar, and
/// counting UTC would shift the reset by hours in either direction.
pub fn month_start(now: i64) -> i64 {
    use chrono::{Datelike, Local, TimeZone};
    let Some(dt) = chrono::DateTime::from_timestamp(now, 0) else { return now };
    let local = dt.with_timezone(&Local);
    Local
        .with_ymd_and_hms(local.year(), local.month(), 1, 0, 0, 0)
        .single()
        .map(|d| d.timestamp())
        .unwrap_or(now)
}

/// The earliest bucket a window includes, given `now`.
pub fn window_start(w: Window, now: i64) -> i64 {
    match w {
        // Rolling: from the hour that contains "an hour ago". Bucket
        // granularity means this over-counts by up to an hour rather than
        // under-counting, which is the safe direction for a cap.
        Window::Hour => hour_of(now - HOUR + 1),
        Window::Week => hour_of(now - 7 * 24 * HOUR + 1),
        Window::Month => hour_of(month_start(now)),
    }
}

impl Ledger {
    pub fn load(path: &Path) -> Result<Ledger> {
        match std::fs::read_to_string(path) {
            Ok(t) => Ok(serde_json::from_str(&t).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically. Same reasoning as the config: a truncated ledger would
    /// silently reset every cap.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            crate::app_paths::ensure_dir(dir);
        }
        let text = serde_json::to_string(self).context("serialising usage")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Add traffic observed since the last call.
    pub fn add(&mut self, tunnel: &str, now: i64, in_bytes: u64, out_bytes: u64) {
        if in_bytes == 0 && out_bytes == 0 {
            return;
        }
        let bucket =
            self.tunnels.entry(tunnel.to_string()).or_default().entry(hour_of(now)).or_default();
        bucket.in_bytes += in_bytes;
        bucket.out_bytes += out_bytes;
    }

    /// Bytes used by `tunnel` in `window`, as of `now`.
    pub fn used(&self, tunnel: &str, w: Window, now: i64, both_directions: bool) -> u64 {
        let Some(buckets) = self.tunnels.get(tunnel) else { return 0 };
        let from = window_start(w, now);
        buckets
            .iter()
            .filter(|(at, _)| **at >= from)
            .map(|(_, b)| if both_directions { b.in_bytes + b.out_bytes } else { b.out_bytes })
            .sum()
    }

    /// Drop buckets older than [`KEEP_HOURS`], and tunnels left with none.
    pub fn prune(&mut self, now: i64) {
        let cutoff = hour_of(now) - KEEP_HOURS * HOUR;
        for buckets in self.tunnels.values_mut() {
            buckets.retain(|at, _| *at >= cutoff);
        }
        self.tunnels.retain(|_, b| !b.is_empty());
    }

    /// Forget a tunnel entirely — used when one is deleted, so its history does
    /// not sit in the file forever under a name nothing refers to.
    pub fn forget(&mut self, tunnel: &str) {
        self.tunnels.remove(tunnel);
    }
}

/// How one tunnel stands against its caps right now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CapStatus {
    /// Per window: used bytes, the limit in bytes (0 = none), and the fraction.
    pub windows: Vec<(Window, u64, u64, f32)>,
    /// The window that is over, if any — the first one to trip.
    pub exceeded: Option<Window>,
}

impl CapStatus {
    /// The tightest window as a fraction of its limit, for a single readout.
    pub fn worst_fraction(&self) -> f32 {
        self.windows.iter().map(|(_, _, _, f)| *f).fold(0.0, f32::max)
    }
}

/// Compare a tunnel's usage against its caps.
///
/// Pure over the ledger so the enforcement rule is testable without a clock,
/// a file or a running tunnel.
pub fn status(ledger: &Ledger, tunnel: &str, caps: &Caps, now: i64) -> CapStatus {
    let mut out = CapStatus::default();
    for w in Window::ALL {
        let limit_mib = caps.limit(w);
        let used = ledger.used(tunnel, w, now, caps.count_both_directions);
        let limit = limit_mib.saturating_mul(1024 * 1024);
        let frac = if limit == 0 { 0.0 } else { used as f32 / limit as f32 };
        if limit > 0 && used >= limit && out.exceeded.is_none() {
            out.exceeded = Some(w);
        }
        out.windows.push((w, used, limit, frac));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3600;

    fn ledger_with(tunnel: &str, entries: &[(i64, u64, u64)]) -> Ledger {
        let mut l = Ledger::default();
        for (at, i, o) in entries {
            l.tunnels
                .entry(tunnel.to_string())
                .or_default()
                .insert(*at, Bucket { in_bytes: *i, out_bytes: *o });
        }
        l
    }

    #[test]
    fn hour_of_snaps_down_to_the_hour() {
        assert_eq!(hour_of(3600), 3600);
        assert_eq!(hour_of(3601), 3600);
        assert_eq!(hour_of(7199), 3600);
        assert_eq!(hour_of(7200), 7200);
    }

    /// The rolling hour must reach back a full hour. Snapping the window start
    /// to the *current* hour instead would reset the cap at :00 and let a burst
    /// spend it twice across the boundary — the exact failure a rolling window
    /// exists to prevent.
    #[test]
    fn the_hourly_window_rolls_rather_than_resetting_on_the_hour() {
        let now = 10 * H + 1; // one second past 10:00
        let start = window_start(Window::Hour, now);
        assert!(start <= 9 * H, "must still include the 09:00 bucket, got {start}");
    }

    #[test]
    fn usage_sums_only_buckets_inside_the_window() {
        let now = 100 * H;
        let l = ledger_with(
            "t",
            &[
                (now, 10, 10),            // this hour
                (now - H, 20, 20),        // last hour
                (now - 50 * H, 500, 500), // well outside an hour, inside a week
            ],
        );
        assert_eq!(l.used("t", Window::Hour, now, true), 60, "the last two buckets");
        assert_eq!(l.used("t", Window::Week, now, true), 1060, "all three");
    }

    /// Providers bill what leaves. Counting both directions is the opt-in.
    #[test]
    fn direction_counting_is_configurable() {
        let now = 100 * H;
        let l = ledger_with("t", &[(now, 900, 100)]);
        assert_eq!(l.used("t", Window::Hour, now, false), 100, "outbound only by default");
        assert_eq!(l.used("t", Window::Hour, now, true), 1000);
    }

    #[test]
    fn an_unknown_tunnel_has_used_nothing() {
        let l = Ledger::default();
        assert_eq!(l.used("nobody", Window::Month, 0, true), 0);
    }

    /// A cap of zero is "no cap", not "no traffic allowed" — getting this
    /// backwards would block every tunnel that never configured one.
    #[test]
    fn a_zero_cap_means_unlimited_not_blocked() {
        let now = 100 * H;
        let l = ledger_with("t", &[(now, 0, 999_999_999)]);
        let s = status(&l, "t", &Caps::default(), now);
        assert_eq!(s.exceeded, None);
        assert_eq!(s.worst_fraction(), 0.0);
    }

    #[test]
    fn a_cap_trips_when_usage_reaches_it() {
        let now = 100 * H;
        let mib = 1024 * 1024;
        let l = ledger_with("t", &[(now, 0, 5 * mib)]);

        let under = Caps { hourly_mib: 10, ..Default::default() };
        assert_eq!(status(&l, "t", &under, now).exceeded, None);

        let exact = Caps { hourly_mib: 5, ..Default::default() };
        assert_eq!(
            status(&l, "t", &exact, now).exceeded,
            Some(Window::Hour),
            "reaching the cap counts as reaching it"
        );

        let over = Caps { hourly_mib: 1, ..Default::default() };
        let s = status(&l, "t", &over, now);
        assert_eq!(s.exceeded, Some(Window::Hour));
        assert!(s.worst_fraction() > 4.0);
    }

    /// The tightest window wins, whichever it is.
    #[test]
    fn the_first_window_to_trip_is_the_one_reported() {
        let now = 100 * H;
        let mib = 1024 * 1024;
        let l = ledger_with("t", &[(now - 40 * H, 0, 50 * mib)]);
        // Outside the hour, inside the week.
        let caps = Caps { hourly_mib: 1, weekly_mib: 10, ..Default::default() };
        assert_eq!(status(&l, "t", &caps, now).exceeded, Some(Window::Week));
    }

    #[test]
    fn adding_accumulates_into_the_current_hour() {
        let mut l = Ledger::default();
        let now = 100 * H + 30;
        l.add("t", now, 5, 7);
        l.add("t", now + 10, 1, 1);
        assert_eq!(l.tunnels["t"][&(100 * H)], Bucket { in_bytes: 6, out_bytes: 8 });
        // A no-op add must not create an empty bucket.
        l.add("t", now + 2 * H, 0, 0);
        assert_eq!(l.tunnels["t"].len(), 1);
    }

    #[test]
    fn pruning_drops_old_buckets_and_empty_tunnels() {
        let now = 10_000 * H;
        let mut l = ledger_with("t", &[(now, 1, 1), (now - KEEP_HOURS * H - H, 9, 9)]);
        l.prune(now);
        assert_eq!(l.tunnels["t"].len(), 1);

        let mut l = ledger_with("old", &[(now - KEEP_HOURS * H - H, 9, 9)]);
        l.prune(now);
        assert!(l.tunnels.is_empty(), "a tunnel with nothing left is dropped entirely");
    }

    #[test]
    fn forget_removes_a_deleted_tunnels_history() {
        let mut l = ledger_with("t", &[(0, 1, 1)]);
        l.forget("t");
        assert!(l.tunnels.is_empty());
    }

    #[test]
    fn a_ledger_survives_a_round_trip_through_disk() {
        let path = std::env::temp_dir().join("TunMan-usage-test.json");
        let _ = std::fs::remove_file(&path);
        let l = ledger_with("t", &[(100 * H, 5, 6)]);
        l.save(&path).unwrap();
        assert_eq!(Ledger::load(&path).unwrap(), l);
        let _ = std::fs::remove_file(&path);
    }

    /// A corrupt ledger must not take the app down or, worse, stop it starting:
    /// losing usage history is recoverable, refusing to launch is not.
    #[test]
    fn a_corrupt_ledger_reads_as_empty_rather_than_failing() {
        let path = std::env::temp_dir().join("TunMan-usage-corrupt.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Ledger::load(&path).unwrap(), Ledger::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn month_start_lands_on_the_first_at_midnight() {
        use chrono::{Datelike, Local, TimeZone, Timelike};
        let now = Local.with_ymd_and_hms(2026, 8, 31, 17, 45, 0).unwrap().timestamp();
        let start = month_start(now);
        let dt = chrono::DateTime::from_timestamp(start, 0).unwrap().with_timezone(&Local);
        assert_eq!((dt.year(), dt.month(), dt.day()), (2026, 8, 1));
        assert_eq!((dt.hour(), dt.minute(), dt.second()), (0, 0, 0));
        assert!(start <= now);
    }
}
