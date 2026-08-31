//! Building the `ssh` command line, and nothing else.
//!
//! [`args`] is pure so the flags can be asserted in tests rather than
//! discovered by watching a tunnel fail to come up. It is also the only place
//! that knows the difference between the port a tunnel *advertises* and the
//! port ssh actually binds — when metering is on those differ, and TunMan owns
//! the advertised one.

use crate::model::{AuthMode, Tunnel, TunnelKind};

/// Where ssh should listen, given whether TunMan is fronting the tunnel.
///
/// With metering on, ssh binds a private loopback port and TunMan takes the
/// advertised one, so clients keep the address they were given and gain byte
/// counting without knowing anything changed.
pub struct Bind {
    pub addr: String,
    pub port: u16,
}

/// The full argument list for a tunnel, excluding the program name.
///
/// `bind` is where ssh's own forward should listen — normally the tunnel's
/// advertised address, or the private port when [`Tunnel::metering`] is on.
pub fn args(t: &Tunnel, bind: &Bind) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();

    // -N: no remote command, this is a forward and nothing else.
    // -T: no pty, so ssh cannot decide to be interactive at us.
    a.push("-N".into());
    a.push("-T".into());

    match t.kind {
        TunnelKind::Socks => {
            a.push("-D".into());
            a.push(format!("{}:{}", bind.addr, bind.port));
        }
        TunnelKind::Local => {
            a.push("-L".into());
            a.push(format!("{}:{}:{}:{}", bind.addr, bind.port, t.dest_host, t.dest_port));
        }
        TunnelKind::Remote => {
            // -R's first field is the address to listen on AT THE SERVER, and
            // the destination is resolved from here. Metering never applies, so
            // this always uses the tunnel's own port rather than `bind`.
            a.push("-R".into());
            a.push(format!("{}:{}:{}:{}", t.bind, t.port, t.dest_host, t.dest_port));
        }
    }

    // Without this ssh reports "cannot listen to port" and then sits there
    // connected, forwarding nothing — a tunnel that looks up and is not. This
    // is the single most important option here.
    a.push("-o".into());
    a.push("ExitOnForwardFailure=yes".into());

    match t.auth {
        // Fail fast rather than block forever on a prompt nobody can see:
        // stdio is piped, so an interactive password request would hang the
        // tunnel with no visible reason.
        AuthMode::KeyOrAgent => {
            a.push("-o".into());
            a.push("BatchMode=yes".into());
        }
        // BatchMode would suppress the very prompt the askpass helper answers.
        AuthMode::Password => {
            a.push("-o".into());
            a.push("BatchMode=no".into());
            a.push("-o".into());
            a.push("NumberOfPasswordPrompts=1".into());
        }
    }

    if t.keepalive_secs > 0 {
        a.push("-o".into());
        a.push(format!("ServerAliveInterval={}", t.keepalive_secs));
        a.push("-o".into());
        a.push("ServerAliveCountMax=3".into());
    }

    if t.compression {
        a.push("-C".into());
    }

    if !t.identity_file.trim().is_empty() {
        a.push("-i".into());
        a.push(t.identity_file.trim().to_string());
        // With an explicit key, stop ssh from working through every other
        // identity it can find first — on a host with an agent full of keys
        // that is how you hit MaxAuthTries and get refused with a valid key in
        // hand.
        a.push("-o".into());
        a.push("IdentitiesOnly=yes".into());
    }

    if t.ssh_port != 22 {
        a.push("-p".into());
        a.push(t.ssh_port.to_string());
    }

    a.extend(t.extra_args.iter().filter(|s| !s.trim().is_empty()).cloned());
    a.push(t.target());
    a
}

/// The command line as a user would type it, with the password masked. For
/// logs and for the "copy command" button.
pub fn display_command(program: &str, t: &Tunnel, bind: &Bind) -> String {
    let joined = args(t, bind)
        .into_iter()
        .map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a })
        .collect::<Vec<_>>()
        .join(" ");
    crate::model::redact(&format!("{program} {joined}"), &t.password)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socks() -> Tunnel {
        Tunnel {
            name: "vps-fi".into(),
            user: "blu".into(),
            host: "fi.example.org".into(),
            port: 1080,
            ..Default::default()
        }
    }

    fn bind_of(t: &Tunnel) -> Bind {
        Bind { addr: t.bind.clone(), port: t.port }
    }

    #[test]
    fn a_socks_tunnel_builds_the_expected_line() {
        let t = socks();
        let a = args(&t, &bind_of(&t));
        assert_eq!(
            a,
            vec![
                "-N",
                "-T",
                "-D",
                "127.0.0.1:1080",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=30",
                "-o",
                "ServerAliveCountMax=3",
                "blu@fi.example.org",
            ]
        );
    }

    /// Metering moves ssh onto a private port while the client keeps the
    /// address it was given. If this ever binds the advertised port, TunMan and
    /// ssh race for it and one of them loses.
    #[test]
    fn metering_puts_ssh_on_the_private_port_not_the_advertised_one() {
        let t = Tunnel { meter: true, ..socks() };
        let a = args(&t, &Bind { addr: "127.0.0.1".into(), port: 49999 });
        assert!(a.contains(&"127.0.0.1:49999".to_string()));
        assert!(!a.contains(&"127.0.0.1:1080".to_string()));
    }

    #[test]
    fn a_local_forward_names_its_destination() {
        let t = Tunnel {
            kind: TunnelKind::Local,
            port: 5432,
            dest_host: "db.internal".into(),
            dest_port: 5432,
            ..socks()
        };
        let a = args(&t, &bind_of(&t));
        assert!(a.contains(&"-L".to_string()));
        assert!(a.contains(&"127.0.0.1:5432:db.internal:5432".to_string()));
    }

    /// A remote forward listens at the far end, so it must use the tunnel's own
    /// bind/port even if a stale `meter` flag hands in a private one.
    #[test]
    fn a_remote_forward_ignores_the_metering_bind() {
        let t = Tunnel {
            kind: TunnelKind::Remote,
            bind: "0.0.0.0".into(),
            port: 8080,
            dest_host: "127.0.0.1".into(),
            dest_port: 3000,
            meter: true,
            ..socks()
        };
        let a = args(&t, &Bind { addr: "127.0.0.1".into(), port: 49999 });
        assert!(a.contains(&"-R".to_string()));
        assert!(a.contains(&"0.0.0.0:8080:127.0.0.1:3000".to_string()));
        assert!(!a.iter().any(|s| s.contains("49999")));
    }

    /// BatchMode=yes would suppress the password prompt that the askpass helper
    /// exists to answer, so password auth has to turn it off explicitly.
    #[test]
    fn password_auth_turns_batch_mode_off() {
        let t = Tunnel { auth: AuthMode::Password, password: "hunter2".into(), ..socks() };
        let a = args(&t, &bind_of(&t));
        assert!(a.contains(&"BatchMode=no".to_string()));
        assert!(!a.contains(&"BatchMode=yes".to_string()));
        assert!(a.contains(&"NumberOfPasswordPrompts=1".to_string()));
    }

    /// An explicit key without IdentitiesOnly makes ssh offer every agent key
    /// first and hit MaxAuthTries with a perfectly good key in hand.
    #[test]
    fn an_explicit_identity_is_used_exclusively() {
        let t = Tunnel { identity_file: "C:/keys/fi.pem".into(), ..socks() };
        let a = args(&t, &bind_of(&t));
        assert!(a.contains(&"-i".to_string()));
        assert!(a.contains(&"C:/keys/fi.pem".to_string()));
        assert!(a.contains(&"IdentitiesOnly=yes".to_string()));
    }

    #[test]
    fn a_default_ssh_port_is_left_off_and_a_moved_one_is_passed() {
        let t = socks();
        assert!(!args(&t, &bind_of(&t)).contains(&"-p".to_string()));
        let t = Tunnel { ssh_port: 2222, ..socks() };
        let a = args(&t, &bind_of(&t));
        assert!(a.contains(&"-p".to_string()));
        assert!(a.contains(&"2222".to_string()));
    }

    #[test]
    fn keepalive_can_be_turned_off_entirely() {
        let t = Tunnel { keepalive_secs: 0, ..socks() };
        let a = args(&t, &bind_of(&t));
        assert!(!a.iter().any(|s| s.starts_with("ServerAliveInterval")));
    }

    #[test]
    fn extra_args_land_before_the_target_and_skip_blanks() {
        let t = Tunnel {
            extra_args: vec!["-o".into(), "ProxyJump=bastion".into(), "   ".into()],
            ..socks()
        };
        let a = args(&t, &bind_of(&t));
        let target_at = a.iter().position(|s| s == "blu@fi.example.org").unwrap();
        let jump_at = a.iter().position(|s| s == "ProxyJump=bastion").unwrap();
        assert!(jump_at < target_at);
        assert!(!a.iter().any(|s| s.trim().is_empty()));
        assert_eq!(target_at, a.len() - 1, "the target is always last");
    }

    #[test]
    fn the_display_command_never_shows_the_password() {
        let t = Tunnel {
            auth: AuthMode::Password,
            password: "hunter2".into(),
            extra_args: vec!["-o".into(), "SomeOpt=hunter2".into()],
            ..socks()
        };
        let line = display_command("ssh", &t, &bind_of(&t));
        assert!(!line.contains("hunter2"), "{line}");
        assert!(line.contains("********"));
    }
}
