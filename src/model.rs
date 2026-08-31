//! What a tunnel *is*, independent of how it is run or drawn.
//!
//! Everything here is plain data with pure methods, so the rules that matter —
//! which URL a tunnel advertises, whether it can be metered, what is safe to
//! print — are testable without spawning ssh or opening a socket.

use serde::{Deserialize, Serialize};

use crate::usage::Caps;

/// The three forwarding modes, matching ssh's own flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TunnelKind {
    /// `ssh -D` — a local SOCKS5 proxy. The one StreamArchiver's proxy pool
    /// consumes, and the default for that reason.
    #[default]
    Socks,
    /// `ssh -L` — a local port forwarded to a host reachable from the server.
    Local,
    /// `ssh -R` — a port on the server forwarded back to a local host.
    Remote,
}

impl TunnelKind {
    pub const ALL: [TunnelKind; 3] = [TunnelKind::Socks, TunnelKind::Local, TunnelKind::Remote];

    pub fn label(self) -> &'static str {
        match self {
            TunnelKind::Socks => "SOCKS",
            TunnelKind::Local => "Local",
            TunnelKind::Remote => "Remote",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            TunnelKind::Socks => {
                "ssh -D: a SOCKS5 proxy on this machine, tunnelled out through the server. \
                 This is the kind StreamArchiver's proxy pool takes."
            }
            TunnelKind::Local => {
                "ssh -L: a port on this machine that reaches a host the server can see. \
                 For getting at something behind the server's firewall."
            }
            TunnelKind::Remote => {
                "ssh -R: a port on the SERVER that reaches back to a host here. \
                 Cannot be metered — the connections are opened at the far end."
            }
        }
    }

    /// Whether TunMan can sit in front of this kind and count bytes.
    ///
    /// A remote forward is dialled from the server, so the listening socket is
    /// on the far end and there is nothing here to front. The UI disables the
    /// Meter checkbox for these rather than silently ignoring it.
    pub fn meterable(self) -> bool {
        matches!(self, TunnelKind::Socks | TunnelKind::Local)
    }
}

/// How ssh should authenticate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Public key or a running ssh-agent. Runs under `BatchMode=yes`, so ssh
    /// fails loudly instead of blocking forever on a prompt nobody can see.
    #[default]
    KeyOrAgent,
    /// A stored password, fed to ssh through TunMan's own askpass helper.
    /// Convenient for a host you cannot key; the password is redacted
    /// everywhere it would otherwise be printed.
    Password,
}

impl AuthMode {
    pub fn label(self) -> &'static str {
        match self {
            AuthMode::KeyOrAgent => "Key / agent",
            AuthMode::Password => "Password",
        }
    }
}

/// A tunnel definition. Serialised verbatim into `TunMan.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tunnel {
    /// Display name and log tag. Also the identity used by the config file, so
    /// renaming a tunnel starts its history fresh.
    pub name: String,
    pub kind: TunnelKind,
    pub enabled: bool,
    /// Start this tunnel when TunMan starts.
    pub auto_start: bool,

    pub user: String,
    pub host: String,
    /// The server's SSH port.
    pub ssh_port: u16,

    /// Address the forward listens on. `127.0.0.1` keeps the proxy private to
    /// this machine, which is almost always what you want — `0.0.0.0` exposes
    /// an open proxy to the whole network.
    pub bind: String,
    /// The advertised port: the SOCKS port for `-D`, the local port for `-L`,
    /// the port on the server for `-R`.
    pub port: u16,

    /// Destination for `-L` (as seen from the server) and `-R` (as seen from
    /// here). Unused by `-D`, which routes per SOCKS request.
    pub dest_host: String,
    pub dest_port: u16,

    pub auth: AuthMode,
    /// Private key path. Empty means "let ssh decide" (agent, or its defaults).
    pub identity_file: String,
    /// Only read when `auth` is [`AuthMode::Password`]. Never logged.
    pub password: String,

    /// Count bytes and destinations by fronting the listener. See
    /// [`TunnelKind::meterable`].
    pub meter: bool,

    /// Bandwidth limits. Only enforceable while [`Tunnel::metering`] is on —
    /// without it there are no byte counts to measure against, and the UI says
    /// so rather than showing a cap that quietly does nothing.
    pub caps: Caps,
    /// Country to display instead of the probed one. For a tunnel that cannot
    /// be probed, or one whose provider geolocates somewhere misleading.
    pub country_override: String,

    pub compression: bool,
    /// `ServerAliveInterval`. Zero disables the keepalive entirely, which is
    /// how a tunnel silently dies behind a NAT that drops idle flows.
    pub keepalive_secs: u32,
    /// Passed to ssh verbatim, after everything TunMan generates.
    pub extra_args: Vec<String>,
}

impl Default for Tunnel {
    fn default() -> Self {
        Tunnel {
            name: String::new(),
            kind: TunnelKind::Socks,
            enabled: true,
            auto_start: false,
            user: String::new(),
            host: String::new(),
            ssh_port: 22,
            bind: "127.0.0.1".to_string(),
            port: 1080,
            dest_host: "127.0.0.1".to_string(),
            dest_port: 0,
            auth: AuthMode::KeyOrAgent,
            identity_file: String::new(),
            password: String::new(),
            meter: false,
            caps: Caps::default(),
            country_override: String::new(),
            compression: false,
            // 30s/3 detects a dead peer in ~90s. Long enough not to churn on a
            // brief hiccup, short enough that a wedged tunnel doesn't sit there
            // looking healthy while nothing flows through it.
            keepalive_secs: 30,
            extra_args: Vec::new(),
        }
    }
}

impl Tunnel {
    /// `user@host`, or just the host when no user is set (ssh then uses the
    /// local username, same as typing it by hand).
    pub fn target(&self) -> String {
        if self.user.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.user, self.host)
        }
    }

    /// The proxy URL to hand to a client, for the kinds that have one.
    ///
    /// `socks5h` rather than `socks5`: the `h` makes the *proxy* resolve DNS,
    /// so the home resolver never sees the hostnames being fetched. Sending
    /// the queries locally would leak exactly what routing the traffic away was
    /// meant to hide.
    pub fn proxy_url(&self) -> Option<String> {
        match self.kind {
            TunnelKind::Socks => Some(format!("socks5h://{}:{}", self.bind, self.port)),
            _ => None,
        }
    }

    /// Whether metering will actually happen: asked for, and possible.
    pub fn metering(&self) -> bool {
        self.meter && self.kind.meterable()
    }

    /// Whether this tunnel's caps can actually be enforced.
    ///
    /// A cap needs byte counts, and byte counts need metering. A cap set on an
    /// unmetered tunnel is not a weak limit, it is *no* limit — so the UI warns
    /// instead of implying protection that does not exist.
    pub fn caps_enforceable(&self) -> bool {
        self.metering()
    }

    /// Whether the tunnel can be probed for exit address, country and latency.
    /// Only a SOCKS proxy can carry the request that answers those.
    pub fn probeable(&self) -> bool {
        self.kind == TunnelKind::Socks
    }

    /// Problems that would stop this tunnel from starting, in the order a user
    /// would want to fix them. Empty means startable.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.name.trim().is_empty() {
            errs.push("Name is required — it identifies the tunnel in logs.".into());
        }
        if self.host.trim().is_empty() {
            errs.push("Host is required.".into());
        }
        if self.port == 0 {
            errs.push("Port must be set.".into());
        }
        if self.ssh_port == 0 {
            errs.push("SSH port must be set (22 unless the server moved it).".into());
        }
        if matches!(self.kind, TunnelKind::Local | TunnelKind::Remote) {
            if self.dest_host.trim().is_empty() {
                errs.push("Destination host is required for a forward.".into());
            }
            if self.dest_port == 0 {
                errs.push("Destination port is required for a forward.".into());
            }
        }
        if self.auth == AuthMode::Password && self.password.is_empty() {
            errs.push("Password auth is selected but no password is set.".into());
        }
        if self.caps.any_set() && !self.caps_enforceable() {
            errs.push(
                "Bandwidth caps need metering, which this tunnel does not have — \
                 they would not be enforced."
                    .into(),
            );
        }
        errs
    }
}

/// Strip a password out of anything about to be displayed or logged.
///
/// Belt and braces: TunMan never deliberately formats a password, but ssh
/// command lines get logged verbatim on failure, and one careless format string
/// is all it takes. Anything that reaches a log goes through here first.
pub fn redact(text: &str, password: &str) -> String {
    if password.is_empty() {
        return text.to_string();
    }
    text.replace(password, "********")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socks() -> Tunnel {
        Tunnel {
            name: "vps-fi".into(),
            host: "fi.example.org".into(),
            user: "blu".into(),
            port: 1080,
            ..Default::default()
        }
    }

    /// `socks5h`, not `socks5`. Resolving locally would send every hostname to
    /// the home resolver — the exact leak that routing the traffic away exists
    /// to prevent — and StreamArchiver's proxy pool documents the same choice.
    #[test]
    fn a_socks_tunnel_advertises_a_dns_at_the_proxy_url() {
        assert_eq!(socks().proxy_url().as_deref(), Some("socks5h://127.0.0.1:1080"));
    }

    #[test]
    fn a_forward_has_no_proxy_url() {
        let t = Tunnel { kind: TunnelKind::Local, ..socks() };
        assert_eq!(t.proxy_url(), None);
        let t = Tunnel { kind: TunnelKind::Remote, ..socks() };
        assert_eq!(t.proxy_url(), None);
    }

    /// A remote forward's listener lives on the server, so there is no local
    /// socket for TunMan to sit in front of. Asking to meter one must resolve
    /// to "no", not to a listener that silently counts nothing.
    #[test]
    fn a_remote_forward_can_never_be_metered() {
        assert!(TunnelKind::Socks.meterable());
        assert!(TunnelKind::Local.meterable());
        assert!(!TunnelKind::Remote.meterable());

        let t = Tunnel { kind: TunnelKind::Remote, meter: true, ..socks() };
        assert!(!t.metering(), "asking to meter a -R must not enable metering");
    }

    #[test]
    fn target_omits_the_user_when_there_isnt_one() {
        assert_eq!(socks().target(), "blu@fi.example.org");
        assert_eq!(Tunnel { user: String::new(), ..socks() }.target(), "fi.example.org");
    }

    #[test]
    fn validate_names_every_missing_piece() {
        assert!(socks().validate().is_empty());

        let t = Tunnel { name: "  ".into(), host: String::new(), ..socks() };
        assert_eq!(t.validate().len(), 2);

        // A forward needs a destination; a SOCKS proxy does not.
        let t = Tunnel { kind: TunnelKind::Local, dest_port: 0, ..socks() };
        assert_eq!(t.validate().len(), 1);

        let t = Tunnel { auth: AuthMode::Password, password: String::new(), ..socks() };
        assert_eq!(t.validate().len(), 1);
    }

    /// A cap without metering is not a loose limit, it is no limit at all —
    /// there are no byte counts to measure against. Saying so is the whole
    /// point; a cap that silently does nothing is worse than no cap.
    #[test]
    fn caps_need_metering_to_mean_anything() {
        let t = Tunnel { meter: true, ..socks() };
        assert!(t.caps_enforceable());

        let t = Tunnel { meter: false, ..socks() };
        assert!(!t.caps_enforceable());

        let capped = Tunnel {
            meter: false,
            caps: crate::usage::Caps { monthly_mib: 100, ..Default::default() },
            ..socks()
        };
        assert_eq!(capped.validate().len(), 1, "and validation says so");
    }

    /// Only a SOCKS tunnel can carry the request that reports its own exit.
    #[test]
    fn only_a_socks_tunnel_can_be_probed() {
        assert!(socks().probeable());
        assert!(!Tunnel { kind: TunnelKind::Local, ..socks() }.probeable());
        assert!(!Tunnel { kind: TunnelKind::Remote, ..socks() }.probeable());
    }

    #[test]
    fn redact_removes_the_password_from_anything_printable() {
        assert_eq!(
            redact("ssh failed: hunter2 rejected", "hunter2"),
            "ssh failed: ******** rejected"
        );
        // No password configured must not turn every empty match into a mask.
        assert_eq!(redact("nothing to hide", ""), "nothing to hide");
    }
}
