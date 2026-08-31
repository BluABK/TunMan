//! Where a tunnel actually comes out, and how far away that is.
//!
//! The useful address for a proxy is the one the far side *presents*, not the
//! `A` record of the box you ssh into — a provider can NAT egress through a
//! different address, and for a chained or jump-hosted tunnel they are simply
//! different machines. So the exit IP and country are measured by asking
//! through the tunnel itself.
//!
//! The endpoint is Cloudflare's `/cdn-cgi/trace`: no API key, no account, no
//! rate limit worth worrying about, and it returns both the caller's address
//! and its country in a two-line-per-field text format that is trivial to parse.
//! Because the request goes *through* the tunnel, it also leaves from the VPS
//! rather than from home — the lookup itself reveals nothing new about you.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// Where to ask. Any host serving a `key=value` trace works; this one is
/// reachable from essentially everywhere.
const TRACE_URL: &str = "https://www.cloudflare.com/cdn-cgi/trace";

/// A probe's findings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exit {
    /// The address the far side presented.
    pub ip: String,
    /// ISO 3166-1 alpha-2, uppercased. `XX` when Cloudflare cannot place it.
    pub country: String,
    /// Round trip for the whole request, through the tunnel.
    pub latency_ms: u64,
}

/// Pull `ip` and `loc` out of a `cdn-cgi/trace` body.
///
/// Pure, and tolerant: the format is a list of `key=value` lines whose set and
/// order Cloudflare has changed before. Unknown keys are ignored and a missing
/// `loc` yields `XX` rather than failing the whole probe — knowing the exit IP
/// without the country is still worth having.
pub fn parse_trace(body: &str) -> (String, String) {
    let mut ip = String::new();
    let mut loc = String::new();
    for line in body.lines() {
        match line.split_once('=') {
            Some(("ip", v)) => ip = v.trim().to_string(),
            Some(("loc", v)) => loc = v.trim().to_uppercase(),
            _ => {}
        }
    }
    if loc.is_empty() {
        loc = "XX".to_string();
    }
    (ip, loc)
}

/// The regional-indicator flag for an ISO country code.
///
/// Built from the code rather than a lookup table: the two letters map onto
/// U+1F1E6..U+1F1FF by offset, so every valid code works and an invalid one
/// falls back to the letters instead of rendering a wrong flag.
pub fn flag(country: &str) -> String {
    let c = country.trim().to_uppercase();
    let bytes = c.as_bytes();
    if bytes.len() != 2 || !bytes.iter().all(|b| b.is_ascii_uppercase()) || c == "XX" {
        return c;
    }
    bytes.iter().filter_map(|b| char::from_u32(0x1F1E6 + (b - b'A') as u32)).collect()
}

/// Ask through a SOCKS proxy where it comes out.
///
/// `proxy` is a `host:port` — the tunnel's own advertised address. `socks5h`
/// so the *proxy* resolves the hostname; resolving here would send the query
/// through the home resolver, which is the leak the tunnel exists to avoid.
pub async fn probe_exit(proxy: &str, timeout: Duration) -> Result<Exit> {
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("socks5h://{proxy}"))?)
        .timeout(timeout)
        // A fresh connection every time is the point: a pooled one would report
        // the latency of a warm socket, not of using the tunnel.
        .pool_max_idle_per_host(0)
        .build()?;

    let started = Instant::now();
    let response = client.get(TRACE_URL).send().await?;
    let status = response.status();
    let body = response.text().await?;
    let latency_ms = started.elapsed().as_millis() as u64;

    if !status.is_success() {
        bail!("trace returned HTTP {status}");
    }
    let (ip, country) = parse_trace(&body);
    if ip.is_empty() {
        bail!("trace did not include an address");
    }
    Ok(Exit { ip, country, latency_ms })
}

/// Resolve an ssh host to an address locally, for the *server* IP column.
///
/// Deliberately a plain local lookup — it answers "what box am I connecting
/// to", which is a different question from where the tunnel comes out, and it
/// should not cost a request through the tunnel to answer.
pub fn resolve_host(host: &str, port: u16) -> Option<String> {
    use std::net::ToSocketAddrs;
    // Already an address? Return it rather than asking a resolver about it.
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Some(host.to_string());
    }
    (host, port).to_socket_addrs().ok()?.next().map(|a| a.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_TRACE: &str = "fl=123f45\nh=www.cloudflare.com\nip=95.216.1.2\nts=1756654321.123\n\
                              visit_scheme=https\nuag=tunman\ncolo=HEL\nsliver=none\nhttp=http/2\n\
                              loc=FI\ntls=TLSv1.3\nsni=plaintext\nwarp=off\ngateway=off\nrbi=off\n\
                              kex=X25519";

    #[test]
    fn a_real_trace_body_yields_the_address_and_country() {
        assert_eq!(parse_trace(REAL_TRACE), ("95.216.1.2".to_string(), "FI".to_string()));
    }

    /// Cloudflare has changed which keys this endpoint returns before, so an
    /// unknown key must be ignored rather than throwing the parse off.
    #[test]
    fn unknown_keys_are_ignored() {
        let body = "something=new\nip=1.2.3.4\nloc=de\nanother=thing";
        assert_eq!(parse_trace(body), ("1.2.3.4".to_string(), "DE".to_string()));
    }

    /// Knowing the exit address without the country is still worth having, so a
    /// missing `loc` must not fail the whole probe.
    #[test]
    fn a_missing_country_falls_back_to_unknown_not_an_error() {
        assert_eq!(parse_trace("ip=1.2.3.4"), ("1.2.3.4".to_string(), "XX".to_string()));
        assert_eq!(parse_trace(""), (String::new(), "XX".to_string()));
    }

    #[test]
    fn flags_are_built_from_the_code_itself() {
        assert_eq!(flag("FI"), "🇫🇮");
        assert_eq!(flag("de"), "🇩🇪");
        assert_eq!(flag("US"), "🇺🇸");
    }

    /// A wrong flag is worse than no flag — it would confidently mislabel where
    /// traffic is coming from.
    #[test]
    fn an_unplaceable_or_malformed_code_shows_the_letters_instead() {
        assert_eq!(flag("XX"), "XX", "Cloudflare's own 'unknown'");
        assert_eq!(flag("F"), "F");
        assert_eq!(flag("FIN"), "FIN");
        assert_eq!(flag("f1"), "F1");
        assert_eq!(flag(""), "");
    }

    #[test]
    fn resolving_an_address_returns_it_without_a_lookup() {
        assert_eq!(resolve_host("127.0.0.1", 22).as_deref(), Some("127.0.0.1"));
        assert_eq!(resolve_host("::1", 22).as_deref(), Some("::1"));
    }

    #[test]
    fn resolving_localhost_gives_a_loopback_address() {
        let got = resolve_host("localhost", 22).expect("localhost always resolves");
        assert!(got == "127.0.0.1" || got == "::1", "got {got}");
    }
}
