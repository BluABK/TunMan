//! The metering front door.
//!
//! With metering on, ssh binds a private loopback port and tunman listens on
//! the one clients were told about. Every connection is accepted here, dialled
//! through to ssh, and copied in both directions by tasks that count bytes as
//! they go. Clients see no difference: same address, same protocol, one extra
//! loopback hop.
//!
//! For a SOCKS tunnel the copier also *reads* the client's opening bytes as
//! they pass, which is how the destination host ends up in the UI. It only
//! ever reads — the bytes are forwarded verbatim, so a protocol quirk this
//! parser does not understand costs the destination label and nothing else.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

use crate::traffic::{ConnEntry, TunnelTraffic};

/// How much of the client's opening data to inspect before giving up on
/// finding a SOCKS request. A greeting plus a request with a 255-byte hostname
/// is under 300 bytes; past this it is not SOCKS and we stop looking.
const SOCKS_SNIFF_CAP: usize = 512;

/// Result of looking for a SOCKS5 destination in the bytes seen so far.
#[derive(Clone, Debug, PartialEq)]
pub enum DestParse {
    /// Not enough bytes yet; ask again after the next read.
    Pending,
    /// The client asked for this `host:port`.
    Done(String),
    /// Definitely not a SOCKS5 exchange. Stop sniffing.
    NotSocks,
}

/// Read the destination out of a SOCKS5 greeting + request.
///
/// Parses from the start of the client's stream every time rather than keeping
/// a state machine: the buffer is a few hundred bytes at most and is dropped as
/// soon as the answer is known, and a restartable pure function is far easier
/// to be sure of than a resumable one.
pub fn parse_socks5_dest(buf: &[u8]) -> DestParse {
    if buf.is_empty() {
        return DestParse::Pending;
    }
    if buf[0] != 0x05 {
        return DestParse::NotSocks;
    }
    if buf.len() < 2 {
        return DestParse::Pending;
    }
    // Greeting: VER NMETHODS METHODS...
    let req_at = 2 + buf[1] as usize;
    if buf.len() < req_at + 4 {
        return if buf.len() >= SOCKS_SNIFF_CAP { DestParse::NotSocks } else { DestParse::Pending };
    }
    let r = &buf[req_at..];
    // Request: VER CMD RSV ATYP ADDR PORT
    if r[0] != 0x05 {
        return DestParse::NotSocks;
    }
    let (host, port_at) = match r[3] {
        0x01 => {
            if r.len() < 4 + 4 + 2 {
                return DestParse::Pending;
            }
            (format!("{}.{}.{}.{}", r[4], r[5], r[6], r[7]), 8)
        }
        0x03 => {
            // The length byte itself may not have arrived yet — reading it
            // before checking is an index panic on a stream we do not control,
            // i.e. one any client could trigger.
            if r.len() < 5 {
                return DestParse::Pending;
            }
            let len = r[4] as usize;
            if r.len() < 5 + len + 2 {
                return DestParse::Pending;
            }
            (String::from_utf8_lossy(&r[5..5 + len]).to_string(), 5 + len)
        }
        0x04 => {
            if r.len() < 4 + 16 + 2 {
                return DestParse::Pending;
            }
            let mut seg = [0u16; 8];
            for (i, s) in seg.iter_mut().enumerate() {
                *s = u16::from_be_bytes([r[4 + i * 2], r[5 + i * 2]]);
            }
            (
                format!(
                    "[{}]",
                    std::net::Ipv6Addr::new(
                        seg[0], seg[1], seg[2], seg[3], seg[4], seg[5], seg[6], seg[7],
                    )
                ),
                20,
            )
        }
        _ => return DestParse::NotSocks,
    };
    let port = u16::from_be_bytes([r[port_at], r[port_at + 1]]);
    DestParse::Done(format!("{host}:{port}"))
}

/// Accept connections on `bind` and pipe each one to `upstream`, counting.
///
/// Runs until the returned task is aborted — the supervisor drops it when the
/// tunnel stops, which closes the listener and every live connection with it.
pub async fn run_listener(
    bind: SocketAddr,
    upstream: SocketAddr,
    traffic: Arc<TunnelTraffic>,
    sniff_socks: bool,
    fixed_dest: String,
    tunnel: String,
) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    debug!(tunnel = %tunnel, %bind, %upstream, "metering listener up");
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(tunnel = %tunnel, error = %e, "accept failed");
                continue;
            }
        };
        let traffic = traffic.clone();
        let dest = fixed_dest.clone();
        let tunnel = tunnel.clone();
        tokio::spawn(async move {
            if let Err(e) =
                handle_conn(client, peer, upstream, traffic, sniff_socks, dest, &tunnel).await
            {
                debug!(tunnel = %tunnel, error = %e, "metered connection ended");
            }
        });
    }
}

/// Pipe one connection, counting both directions.
async fn handle_conn(
    client: TcpStream,
    peer: SocketAddr,
    upstream: SocketAddr,
    traffic: Arc<TunnelTraffic>,
    sniff_socks: bool,
    fixed_dest: String,
    tunnel: &str,
) -> Result<()> {
    // Nagle would add latency to the small SOCKS handshake for no benefit on a
    // loopback hop.
    let _ = client.set_nodelay(true);
    let server = TcpStream::connect(upstream).await?;
    let _ = server.set_nodelay(true);

    let (pid, process) = owner_of(peer.port(), upstream.port());
    let entry = Arc::new(ConnEntry::new(
        pid,
        process,
        if sniff_socks { "(resolving)".to_string() } else { fixed_dest },
    ));
    traffic.active.lock().push(entry.clone());

    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();

    let out_entry = entry.clone();
    let out_traffic = traffic.clone();
    let sniff_tunnel = tunnel.to_string();
    // Client → server. This direction carries the SOCKS handshake, so it is the
    // one that sniffs.
    let up = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        let mut sniff: Vec<u8> = Vec::new();
        let mut sniffing = sniff_socks;
        loop {
            let n = match cr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if sw.write_all(&buf[..n]).await.is_err() {
                break;
            }
            out_entry.out_bytes.fetch_add(n as u64, Ordering::Relaxed);
            out_traffic.total_out.fetch_add(n as u64, Ordering::Relaxed);

            if sniffing {
                sniff.extend_from_slice(&buf[..n]);
                sniff.truncate(SOCKS_SNIFF_CAP);
                match parse_socks5_dest(&sniff) {
                    DestParse::Done(dest) => {
                        debug!(tunnel = %sniff_tunnel, %dest, "socks destination");
                        out_entry.set_dest(dest);
                        sniffing = false;
                        sniff = Vec::new();
                    }
                    DestParse::NotSocks => {
                        out_entry.set_dest("(not SOCKS)".to_string());
                        sniffing = false;
                        sniff = Vec::new();
                    }
                    DestParse::Pending => {}
                }
            }
        }
        let _ = sw.shutdown().await;
    });

    let in_entry = entry.clone();
    let in_traffic = traffic.clone();
    // Server → client.
    let down = tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match sr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if cw.write_all(&buf[..n]).await.is_err() {
                break;
            }
            in_entry.in_bytes.fetch_add(n as u64, Ordering::Relaxed);
            in_traffic.total_in.fetch_add(n as u64, Ordering::Relaxed);
        }
        let _ = cw.shutdown().await;
    });

    let _ = tokio::join!(up, down);
    traffic.retire(&entry);
    Ok(())
}

/// Which process opened the connection arriving from `client_port`.
///
/// Matched through the system TCP table: the client's socket is the row whose
/// local port is the ephemeral port it dialled from and whose remote port is
/// ours. Returns pid 0 and an empty name when the socket has already gone —
/// a very short-lived connection can close before this runs, and an unknown
/// owner is better than blocking the pipe to find out.
fn owner_of(client_port: u16, _our_port: u16) -> (u32, String) {
    let rows = crate::platform::tcp_table();
    let Some(pid) = rows
        .iter()
        .find(|r| r.local_port == client_port && r.state == crate::platform::TCP_ESTABLISHED)
        .map(|r| r.pid)
    else {
        return (0, String::new());
    };
    let name = crate::platform::process_tree_snapshot()
        .into_iter()
        .find(|p| p.pid == pid)
        .map(|p| p.name)
        .unwrap_or_default();
    (pid, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Greeting (VER, 1 method, no-auth) followed by a CONNECT request.
    fn greeting() -> Vec<u8> {
        vec![0x05, 0x01, 0x00]
    }

    #[test]
    fn a_domain_destination_is_read_out_of_the_request() {
        let mut b = greeting();
        b.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, 11]);
        b.extend_from_slice(b"youtube.com");
        b.extend_from_slice(&443u16.to_be_bytes());
        assert_eq!(parse_socks5_dest(&b), DestParse::Done("youtube.com:443".into()));
    }

    #[test]
    fn an_ipv4_destination_is_read_out_of_the_request() {
        let mut b = greeting();
        b.extend_from_slice(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4]);
        b.extend_from_slice(&8080u16.to_be_bytes());
        assert_eq!(parse_socks5_dest(&b), DestParse::Done("1.2.3.4:8080".into()));
    }

    #[test]
    fn an_ipv6_destination_is_read_out_of_the_request() {
        let mut b = greeting();
        b.extend_from_slice(&[0x05, 0x01, 0x00, 0x04]);
        b.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        b.extend_from_slice(&443u16.to_be_bytes());
        assert_eq!(parse_socks5_dest(&b), DestParse::Done("[2001:db8::1]:443".into()));
    }

    /// The sniffer runs on a live stream, so it sees the request a few bytes at
    /// a time. Every prefix must say "not yet" rather than guessing.
    #[test]
    fn every_short_prefix_is_pending_not_a_wrong_answer() {
        let mut full = greeting();
        full.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, 11]);
        full.extend_from_slice(b"youtube.com");
        full.extend_from_slice(&443u16.to_be_bytes());

        for n in 0..full.len() {
            assert_eq!(
                parse_socks5_dest(&full[..n]),
                DestParse::Pending,
                "prefix of {n} bytes should still be pending"
            );
        }
        assert!(matches!(parse_socks5_dest(&full), DestParse::Done(_)));
    }

    /// A metered local forward carries whatever protocol it carries. Sniffing
    /// must give up promptly instead of buffering the whole conversation.
    #[test]
    fn a_non_socks_stream_is_rejected_on_the_first_byte() {
        assert_eq!(parse_socks5_dest(b"GET / HTTP/1.1"), DestParse::NotSocks);
        assert_eq!(parse_socks5_dest(&[0x04, 0x01]), DestParse::NotSocks, "SOCKS4 is not SOCKS5");
    }

    #[test]
    fn an_unknown_address_type_is_rejected() {
        let mut b = greeting();
        b.extend_from_slice(&[0x05, 0x01, 0x00, 0x09, 1, 2]);
        assert_eq!(parse_socks5_dest(&b), DestParse::NotSocks);
    }

    /// A stream that opens with 0x05 but never produces a parseable request
    /// must not keep the sniffer buffering forever.
    #[test]
    fn sniffing_gives_up_at_the_cap() {
        let b = vec![0x05u8; SOCKS_SNIFF_CAP];
        // 0x05 methods, then never enough bytes for the request header.
        assert_eq!(parse_socks5_dest(&b), DestParse::NotSocks);
    }

    /// The whole metering path, with a stand-in for ssh: a client connects to
    /// the listener, the listener dials the fake upstream, bytes flow both
    /// ways, and the counts and destination land where the UI reads them.
    ///
    /// Worth doing for real rather than unit-testing the parser alone — the
    /// parts most likely to break are the ones a pure test cannot reach: that
    /// the sniffed bytes are still *forwarded*, that both directions are
    /// counted separately, and that a connection is retired rather than
    /// abandoned when it closes.
    #[tokio::test]
    async fn a_metered_connection_forwards_everything_and_counts_both_directions() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        // Stand-in for ssh's SOCKS port: echoes back twice what it receives, so
        // the two directions carry different byte counts and cannot be
        // confused for one another.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let seen_by_upstream = seen.clone();
        tokio::spawn(async move {
            // Accept in a loop: anything that only served one connection would
            // make this test depend on nothing else ever dialling the port.
            while let Ok((mut sock, _)) = upstream.accept().await {
                let seen = seen_by_upstream.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        let Ok(n) = sock.read(&mut buf).await else { break };
                        if n == 0 {
                            break;
                        }
                        seen.lock().extend_from_slice(&buf[..n]);
                        // Echo twice, so the two directions carry different
                        // totals and cannot be mistaken for each other.
                        if sock.write_all(&buf[..n]).await.is_err()
                            || sock.write_all(&buf[..n]).await.is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });

        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_addr = front.local_addr().unwrap();
        drop(front); // hand the port to the listener under test

        let traffic = Arc::new(TunnelTraffic::default());
        let t = traffic.clone();
        let listener = tokio::spawn(async move {
            let _ = run_listener(front_addr, upstream_addr, t, true, String::new(), "test".into())
                .await;
        });

        // Retry the real client rather than probing first: a throwaway probe
        // connection is a connection, and the listener would dutifully forward
        // and count it.
        let mut client = None;
        for _ in 0..80 {
            if let Ok(c) = TcpStream::connect(front_addr).await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let mut client = client.expect("listener never came up");
        let mut request = vec![0x05, 0x01, 0x00]; // greeting
        request.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, 11]);
        request.extend_from_slice(b"youtube.com");
        request.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&request).await.unwrap();

        // Read back the echo so we know the round trip completed.
        let mut back = vec![0u8; request.len() * 2];
        client.read_exact(&mut back).await.unwrap();

        // Let the counters settle.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let rows = traffic.rows();
        assert_eq!(rows.len(), 1, "one client, one row");
        assert_eq!(rows[0].dest, "youtube.com:443", "destination read from the handshake");
        assert_eq!(rows[0].live, 1);
        assert_eq!(
            rows[0].out_bytes,
            request.len() as u64,
            "everything the client sent was counted outbound"
        );
        assert_eq!(
            rows[0].in_bytes,
            (request.len() * 2) as u64,
            "and the reply was counted inbound, separately"
        );

        // The sniffed bytes must still have reached the far side verbatim: the
        // parser reads a copy, it does not consume the stream.
        assert_eq!(&*seen.lock(), &request, "the upstream got the bytes unchanged");

        // Closing retires the connection into the aggregate rather than
        // dropping its bytes on the floor.
        drop(client);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let rows = traffic.rows();
        assert_eq!(rows[0].live, 0, "no longer live");
        assert_eq!(rows[0].total_conns, 1);
        assert_eq!(rows[0].out_bytes, request.len() as u64, "its bytes survived the close");

        listener.abort();
    }

    #[test]
    fn a_greeting_offering_several_methods_is_skipped_correctly() {
        let mut b = vec![0x05, 0x03, 0x00, 0x01, 0x02]; // three methods
        b.extend_from_slice(&[0x05, 0x01, 0x00, 0x01, 9, 9, 9, 9]);
        b.extend_from_slice(&53u16.to_be_bytes());
        assert_eq!(parse_socks5_dest(&b), DestParse::Done("9.9.9.9:53".into()));
    }
}
