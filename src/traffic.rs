//! Who is using a tunnel, and how much is going through it.
//!
//! Two tiers, because Windows gives one of them away and charges for the other.
//!
//! **Always on** — the system TCP table ([`platform::tcp_table`]) names the
//! process behind every socket, so "which programs are on this tunnel right
//! now" costs one call a second and no interference. It cannot tell you how
//! many bytes moved: that is simply not in the table.
//!
//! **Opt-in metering** — for byte counts, something has to be *in* the stream.
//! When a tunnel is metered, ssh binds a private port and TunMan owns the
//! advertised one, so every byte passes through a task that can count it (and,
//! for SOCKS, read the destination out of the handshake on the way past).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// A live metered connection. The counters are atomics so the copying tasks
/// update them without ever taking the tunnel's lock.
pub struct ConnEntry {
    pub pid: u32,
    pub process: String,
    /// Destination as the client asked for it — for SOCKS, read out of the
    /// handshake as it passes; for a local forward, the fixed far end.
    ///
    /// Behind a lock because it is filled in *after* the connection opens, once
    /// enough of the handshake has arrived, and both copy tasks share this one
    /// entry. An earlier version swapped in a replacement entry instead, which
    /// silently orphaned the counter the download task was still writing to —
    /// every SOCKS row showed zero bytes in.
    pub dest: Mutex<String>,
    /// Bytes travelling out through the tunnel (client → server).
    pub out_bytes: AtomicU64,
    /// Bytes coming back (server → client).
    pub in_bytes: AtomicU64,
}

impl ConnEntry {
    pub fn new(pid: u32, process: String, dest: String) -> ConnEntry {
        ConnEntry {
            pid,
            process,
            dest: Mutex::new(dest),
            out_bytes: AtomicU64::new(0),
            in_bytes: AtomicU64::new(0),
        }
    }

    /// Label the destination once the handshake reveals it.
    pub fn set_dest(&self, dest: String) {
        *self.dest.lock() = dest;
    }

    pub fn dest(&self) -> String {
        self.dest.lock().clone()
    }
}

/// Cumulative totals for one `(process, destination)` pair.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agg {
    pub conns: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
}

/// One row of the per-tunnel detail table.
#[derive(Clone, Debug, PartialEq)]
pub struct TrafficRow {
    pub pid: u32,
    pub process: String,
    pub dest: String,
    /// Connections currently open.
    pub live: u64,
    /// Connections opened in total, including those since closed.
    pub total_conns: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
}

/// Everything known about one tunnel's traffic.
#[derive(Default)]
pub struct TunnelTraffic {
    /// Metered connections currently open.
    pub active: Mutex<Vec<Arc<ConnEntry>>>,
    /// Metered connections that have closed, folded down by (pid, dest) so a
    /// busy tunnel does not accumulate an unbounded list.
    pub closed: Mutex<HashMap<(u32, String), Agg>>,
    /// Unmetered view: process id → live connection count, from the TCP table.
    pub observed: Mutex<Vec<(u32, String, u64)>>,
    /// Lifetime totals, including closed connections.
    pub total_in: AtomicU64,
    pub total_out: AtomicU64,
}

impl TunnelTraffic {
    /// Fold a finished connection into the closed aggregate and drop it from
    /// the live list.
    pub fn retire(&self, entry: &Arc<ConnEntry>) {
        let inb = entry.in_bytes.load(Ordering::Relaxed);
        let outb = entry.out_bytes.load(Ordering::Relaxed);
        {
            let mut closed = self.closed.lock();
            let agg = closed.entry((entry.pid, entry.dest())).or_default();
            agg.conns += 1;
            agg.in_bytes += inb;
            agg.out_bytes += outb;
        }
        self.active.lock().retain(|e| !Arc::ptr_eq(e, entry));
    }

    /// The detail table: live connections folded together with closed ones.
    ///
    /// Sorted by traffic so the loudest client is at the top — with metering
    /// off there are no bytes to sort by, so it falls back to connection count.
    pub fn rows(&self) -> Vec<TrafficRow> {
        let mut by_key: HashMap<(u32, String), TrafficRow> = HashMap::new();

        for (key, agg) in self.closed.lock().iter() {
            let row = by_key.entry(key.clone()).or_insert_with(|| TrafficRow {
                pid: key.0,
                process: String::new(),
                dest: key.1.clone(),
                live: 0,
                total_conns: 0,
                in_bytes: 0,
                out_bytes: 0,
            });
            row.total_conns += agg.conns;
            row.in_bytes += agg.in_bytes;
            row.out_bytes += agg.out_bytes;
        }

        for e in self.active.lock().iter() {
            let dest = e.dest();
            let row = by_key.entry((e.pid, dest.clone())).or_insert_with(|| TrafficRow {
                pid: e.pid,
                process: e.process.clone(),
                dest,
                live: 0,
                total_conns: 0,
                in_bytes: 0,
                out_bytes: 0,
            });
            row.process = e.process.clone();
            row.live += 1;
            row.total_conns += 1;
            row.in_bytes += e.in_bytes.load(Ordering::Relaxed);
            row.out_bytes += e.out_bytes.load(Ordering::Relaxed);
        }

        // Unmetered tunnels have no per-connection data at all, so the observed
        // table is the whole answer. Only used when metering produced nothing,
        // so a tunnel that was metered mid-session keeps its real numbers.
        if by_key.is_empty() {
            for (pid, process, conns) in self.observed.lock().iter() {
                by_key.insert(
                    (*pid, String::new()),
                    TrafficRow {
                        pid: *pid,
                        process: process.clone(),
                        dest: String::new(),
                        live: *conns,
                        total_conns: *conns,
                        in_bytes: 0,
                        out_bytes: 0,
                    },
                );
            }
        }

        let mut rows: Vec<TrafficRow> = by_key.into_values().collect();
        rows.sort_by(|a, b| {
            (b.in_bytes + b.out_bytes)
                .cmp(&(a.in_bytes + a.out_bytes))
                .then(b.live.cmp(&a.live))
                .then(a.process.cmp(&b.process))
                .then(a.dest.cmp(&b.dest))
        });
        rows
    }

    /// Live connection count, from whichever tier has data.
    pub fn live_conns(&self) -> u64 {
        let metered = self.active.lock().len() as u64;
        if metered > 0 {
            return metered;
        }
        self.observed.lock().iter().map(|(_, _, n)| n).sum()
    }
}

/// The processes connected to a local port right now, from the system TCP
/// table — the unmetered tier.
///
/// A loopback connection appears **twice**: once from the client's side
/// (`remote_port` = the tunnel's port) and once from the listener's
/// (`local_port` = the tunnel's port). Only the client side is wanted, or every
/// connection is counted twice and the listener is reported as its own client.
///
/// `exclude` drops TunMan's own pid and ssh's: with metering on, TunMan holds
/// both ends of the loopback pair, and counting itself as a user of its own
/// tunnel is noise.
pub fn clients_of(
    rows: &[(u16, u32, u16, u32, u32)],
    port: u16,
    exclude: &[u32],
) -> Vec<(u32, u64)> {
    const LOOPBACK_LE: u32 = 0x0100_007f; // 127.0.0.1 as stored in the table
    let mut counts: HashMap<u32, u64> = HashMap::new();
    for &(_local_port, remote_addr, remote_port, pid, state) in rows {
        if state != crate::platform::TCP_ESTABLISHED {
            continue;
        }
        if remote_port != port || remote_addr != LOOPBACK_LE {
            continue;
        }
        if exclude.contains(&pid) {
            continue;
        }
        *counts.entry(pid).or_default() += 1;
    }
    let mut out: Vec<(u32, u64)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESTAB: u32 = crate::platform::TCP_ESTABLISHED;
    const LOOPBACK: u32 = 0x0100_007f;

    /// (local_port, remote_addr, remote_port, pid, state)
    fn row(local: u16, remote: u16, pid: u32, state: u32) -> (u16, u32, u16, u32, u32) {
        (local, LOOPBACK, remote, pid, state)
    }

    /// A loopback connection shows up from both ends. Counting both would
    /// double every number and list the tunnel's own listener as a client of
    /// itself.
    #[test]
    fn only_the_client_side_of_a_loopback_pair_counts() {
        let rows = vec![
            row(51000, 1080, 4242, ESTAB), // the client
            row(1080, 51000, 999, ESTAB),  // ssh's listener, same connection
        ];
        assert_eq!(clients_of(&rows, 1080, &[]), vec![(4242, 1)]);
    }

    #[test]
    fn connections_are_counted_per_process() {
        let rows = vec![
            row(51000, 1080, 4242, ESTAB),
            row(51001, 1080, 4242, ESTAB),
            row(51002, 1080, 777, ESTAB),
        ];
        assert_eq!(clients_of(&rows, 1080, &[]), vec![(4242, 2), (777, 1)]);
    }

    #[test]
    fn a_connection_to_another_port_is_not_ours() {
        let rows = vec![row(51000, 9999, 4242, ESTAB)];
        assert!(clients_of(&rows, 1080, &[]).is_empty());
    }

    /// Only established connections carry traffic; a socket in TIME_WAIT would
    /// otherwise keep a departed client on the list for minutes.
    #[test]
    fn a_socket_that_is_not_established_does_not_count() {
        let rows = vec![row(51000, 1080, 4242, 11 /* TIME_WAIT */)];
        assert!(clients_of(&rows, 1080, &[]).is_empty());
    }

    /// With metering on, TunMan holds both ends of the loopback pair. Listing
    /// itself as a user of its own tunnel is pure noise.
    #[test]
    fn our_own_processes_are_excluded() {
        let rows = vec![row(51000, 1080, 4242, ESTAB), row(51001, 1080, 100, ESTAB)];
        assert_eq!(clients_of(&rows, 1080, &[100]), vec![(4242, 1)]);
    }

    #[test]
    fn rows_fold_live_and_closed_connections_together() {
        let t = TunnelTraffic::default();
        t.closed
            .lock()
            .insert((7, "youtube.com:443".into()), Agg { conns: 2, in_bytes: 100, out_bytes: 10 });

        let live = Arc::new(ConnEntry::new(7, "sa.exe".into(), "youtube.com:443".into()));
        live.in_bytes.store(50, Ordering::Relaxed);
        t.active.lock().push(live);

        let rows = t.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].live, 1);
        assert_eq!(rows[0].total_conns, 3, "two closed plus the live one");
        assert_eq!(rows[0].in_bytes, 150);
        assert_eq!(rows[0].process, "sa.exe");
    }

    /// Retiring must move the bytes, not lose them — a tunnel's totals should
    /// not drop when a connection closes.
    #[test]
    fn retiring_a_connection_keeps_its_bytes() {
        let t = TunnelTraffic::default();
        let e = Arc::new(ConnEntry::new(7, "sa.exe".into(), "a:443".into()));
        e.in_bytes.store(500, Ordering::Relaxed);
        e.out_bytes.store(20, Ordering::Relaxed);
        t.active.lock().push(e.clone());

        t.retire(&e);
        assert!(t.active.lock().is_empty());
        let rows = t.rows();
        assert_eq!(rows[0].in_bytes, 500);
        assert_eq!(rows[0].out_bytes, 20);
        assert_eq!(rows[0].live, 0);
    }

    /// An unmetered tunnel has no byte data, so the observed table is all there
    /// is — but it must not overwrite real numbers on a tunnel that has been
    /// metered at some point this session.
    #[test]
    fn observed_rows_only_fill_in_when_metering_produced_nothing() {
        let t = TunnelTraffic::default();
        t.observed.lock().push((4242, "firefox.exe".into(), 3));
        let rows = t.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].live, 3);
        assert_eq!(rows[0].in_bytes, 0);

        t.closed.lock().insert((7, "a:443".into()), Agg { conns: 1, in_bytes: 9, out_bytes: 1 });
        let rows = t.rows();
        assert_eq!(rows.len(), 1, "metered data wins outright");
        assert_eq!(rows[0].pid, 7);
    }

    #[test]
    fn live_conns_prefers_metered_and_falls_back_to_observed() {
        let t = TunnelTraffic::default();
        t.observed.lock().push((4242, "firefox.exe".into(), 3));
        assert_eq!(t.live_conns(), 3);

        t.active.lock().push(Arc::new(ConnEntry::new(7, "sa.exe".into(), "a:443".into())));
        assert_eq!(t.live_conns(), 1);
    }
}
