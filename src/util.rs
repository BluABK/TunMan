//! Small shared helpers: clocks, and the number formatting the UI leans on.

/// Seconds since the epoch. Saturates at 0 rather than panicking if the system
/// clock is set before 1970 — a clock that wrong is not worth crashing over.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Bytes as a short human string. Precision drops as the unit grows, so a
/// column of these stays the same width and stays scannable.
pub fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let b = bytes as f64;
    if b >= TB {
        format!("{:.2} TB", b / TB)
    } else if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// A byte rate. Zero renders as `—`: a column of `0 B/s` reads as broken, while
/// a dash reads as idle, which is what it means.
pub fn fmt_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec < 1.0 {
        return "—".to_string();
    }
    format!("{}/s", fmt_bytes(bytes_per_sec as u64))
}

/// A duration as uptime: `4d 3h`, `3h 12m`, `12m 05s`, `41s`. Two units at
/// most — the third never changes a decision.
pub fn fmt_uptime(secs: i64) -> String {
    if secs < 0 {
        return "—".to_string();
    }
    let (d, h, m, s) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// How long to wait before retrying a tunnel that has failed `fails` times in a
/// row: 5 s doubling to a 5-minute ceiling.
///
/// Capped rather than given up on. A tunnel manager exists to outlast the
/// outage — a server rebooting, a laptop changing networks — so it keeps
/// trying, just not often enough to spam a dead host. The count is what the UI
/// shows so a permanently broken tunnel is still obvious.
pub fn retry_delay_secs(fails: u32) -> u64 {
    const BASE: u64 = 5;
    const CAP: u64 = 300;
    if fails == 0 {
        return 0;
    }
    BASE.saturating_mul(1u64 << (fails - 1).min(16)).min(CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_shrink_their_precision_as_they_grow() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    /// Idle has to look like idle. A grid full of "0 B/s" reads as a broken
    /// tunnel; a dash reads as a quiet one.
    #[test]
    fn an_idle_rate_is_a_dash() {
        assert_eq!(fmt_rate(0.0), "—");
        assert_eq!(fmt_rate(0.4), "—");
        assert_eq!(fmt_rate(2048.0), "2 KB/s");
    }

    #[test]
    fn uptime_shows_two_units_at_most() {
        assert_eq!(fmt_uptime(41), "41s");
        assert_eq!(fmt_uptime(725), "12m 05s");
        assert_eq!(fmt_uptime(3 * 3600 + 12 * 60), "3h 12m");
        assert_eq!(fmt_uptime(4 * 86400 + 3 * 3600), "4d 3h");
        assert_eq!(fmt_uptime(-1), "—");
    }

    /// Doubling, capped — never giving up. A server that comes back after an
    /// hour must find its tunnel still trying.
    #[test]
    fn retry_backs_off_but_never_gives_up() {
        assert_eq!(retry_delay_secs(0), 0);
        assert_eq!(retry_delay_secs(1), 5);
        assert_eq!(retry_delay_secs(2), 10);
        assert_eq!(retry_delay_secs(3), 20);
        assert_eq!(retry_delay_secs(60), 300, "caps rather than overflowing");
    }
}
