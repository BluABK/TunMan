//! Starting tunnels, keeping them up, and knowing when they are not.
//!
//! One tokio task per tunnel, looping: spawn ssh, watch it, and when it dies
//! unexpectedly, wait out a backoff and go again. The task owns the child; the
//! UI only ever reads a snapshot of [`Shared`] and sends [`Command`]s, so
//! nothing on the render thread can block on a process.
//!
//! **"ssh is running" is not "the tunnel works."** A forward that failed to
//! bind, or a session wedged behind a dead NAT, keeps the process alive while
//! carrying nothing. So a tunnel is only reported up once its port actually
//! accepts a connection, and the optional probe goes further and drives a real
//! request through it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::model::{AuthMode, Tunnel, TunnelKind};
use crate::ssh::Bind;
use crate::traffic::TunnelTraffic;
use crate::util::{now_unix, retry_delay_secs};

/// How long a tunnel must stay up before its failure streak is forgiven. A
/// tunnel that ran for an hour and then dropped should retry immediately, not
/// inherit the backoff from whatever went wrong when it first started.
const STABLE_AFTER_SECS: i64 = 60;

/// How long to wait for ssh's forward to start accepting after the process
/// starts. Key auth and the handshake are usually well under a second; a slow
/// or far-away server can take several.
const LISTEN_TIMEOUT_SECS: u64 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Stopped,
    /// Process spawned, forward not yet accepting.
    Starting,
    /// Forward is accepting connections.
    Up,
    /// Died; waiting out the backoff before trying again.
    Retrying,
    /// Cannot be started at all — a bad definition, or ssh is missing.
    Failed,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Stopped => "Stopped",
            Status::Starting => "Starting",
            Status::Up => "Up",
            Status::Retrying => "Retrying",
            Status::Failed => "Failed",
        }
    }

    /// The status dot. Colour is applied by the caller from the egui theme.
    pub fn dot(self) -> &'static str {
        match self {
            Status::Up => "●",
            Status::Starting | Status::Retrying => "◐",
            Status::Failed => "▲",
            Status::Stopped => "○",
        }
    }
}

/// Everything the UI shows about one tunnel. Cloning is cheap — the traffic
/// counters are behind an `Arc` and stay live in the clone.
#[derive(Clone)]
pub struct TunnelState {
    pub name: String,
    pub status: Status,
    pub pid: Option<u32>,
    /// When the current run started accepting connections.
    pub up_since: Option<i64>,
    /// Times this tunnel has been restarted after an unexpected exit.
    pub restarts: u32,
    /// Consecutive failures, which set the backoff.
    pub fails: u32,
    pub last_error: String,
    /// When the next retry is due, for the countdown on the row.
    pub next_retry_at: i64,
    /// What clients should connect to.
    pub advertised: String,
    /// The advertised port on its own, so the sampler can match TCP-table rows
    /// without parsing `advertised` back apart.
    pub port: u16,
    pub metering: bool,
    /// Last probe result, when probing is enabled.
    pub probe_ok: Option<bool>,
    pub probe_note: String,
    pub traffic: Arc<TunnelTraffic>,
}

impl TunnelState {
    fn new(name: String, advertised: String, port: u16, metering: bool) -> TunnelState {
        TunnelState {
            name,
            status: Status::Stopped,
            pid: None,
            up_since: None,
            restarts: 0,
            fails: 0,
            last_error: String::new(),
            next_retry_at: 0,
            advertised,
            port,
            metering,
            probe_ok: None,
            probe_note: String::new(),
            traffic: Arc::new(TunnelTraffic::default()),
        }
    }
}

/// State shared between the supervisor tasks and the UI.
#[derive(Default)]
pub struct Shared {
    pub states: Mutex<HashMap<String, TunnelState>>,
}

impl Shared {
    /// Every tunnel's state, in `order`. Tunnels in `order` that have never run
    /// appear as `Stopped`, so the table matches the config even before
    /// anything starts.
    pub fn snapshot(&self, order: &[Tunnel]) -> Vec<TunnelState> {
        let states = self.states.lock();
        order
            .iter()
            .map(|t| {
                states.get(&t.name).cloned().unwrap_or_else(|| {
                    TunnelState::new(
                        t.name.clone(),
                        t.proxy_url().unwrap_or_else(|| format!("{}:{}", t.bind, t.port)),
                        t.port,
                        t.metering(),
                    )
                })
            })
            .collect()
    }

    fn update(&self, name: &str, f: impl FnOnce(&mut TunnelState)) {
        if let Some(s) = self.states.lock().get_mut(name) {
            f(s);
        }
    }
}

/// What the UI asks the supervisor to do.
#[derive(Clone, Debug)]
pub enum Command {
    Start(String),
    Stop(String),
    StartAll,
    StopAll,
    /// The config changed: restart anything whose definition moved, leave the
    /// rest alone.
    Reload(Box<Config>),
    /// Stop everything and end the supervisor.
    Shutdown,
}

struct RunHandle {
    stop: mpsc::Sender<()>,
    /// The tunnel definition this run was started from, so a reload can tell
    /// whether anything it cares about actually changed.
    def: Tunnel,
}

/// Run the supervisor until [`Command::Shutdown`]. Owns every child process.
pub async fn run(mut cfg: Config, shared: Arc<Shared>, mut rx: mpsc::Receiver<Command>) {
    let mut running: HashMap<String, RunHandle> = HashMap::new();

    if cfg.settings.autostart_tunnels {
        for t in cfg.tunnels.iter().filter(|t| t.enabled && t.auto_start) {
            start(t, &cfg, &shared, &mut running).await;
        }
    }

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Start(name) => {
                if let Some(t) = cfg.tunnels.iter().find(|t| t.name == name) {
                    start(t, &cfg, &shared, &mut running).await;
                }
            }
            Command::Stop(name) => stop(&name, &shared, &mut running).await,
            Command::StartAll => {
                for t in cfg.tunnels.clone().iter().filter(|t| t.enabled) {
                    start(t, &cfg, &shared, &mut running).await;
                }
            }
            Command::StopAll => {
                for name in running.keys().cloned().collect::<Vec<_>>() {
                    stop(&name, &shared, &mut running).await;
                }
            }
            Command::Reload(new) => {
                let new = *new;
                // Only bounce tunnels whose definition actually changed —
                // editing one tunnel must not drop every other connection.
                for name in running.keys().cloned().collect::<Vec<_>>() {
                    let old = running.get(&name).map(|h| h.def.clone());
                    let now = new.tunnels.iter().find(|t| t.name == name).cloned();
                    let changed = match (&old, &now) {
                        (Some(o), Some(n)) => o != n,
                        _ => true, // deleted, or renamed out from under us
                    };
                    if changed {
                        stop(&name, &shared, &mut running).await;
                        if let Some(t) = now.filter(|t| t.enabled) {
                            start(&t, &new, &shared, &mut running).await;
                        }
                    }
                }
                cfg = new;
            }
            Command::Shutdown => {
                for name in running.keys().cloned().collect::<Vec<_>>() {
                    stop(&name, &shared, &mut running).await;
                }
                info!("supervisor stopped");
                return;
            }
        }
    }
}

async fn start(
    t: &Tunnel,
    cfg: &Config,
    shared: &Arc<Shared>,
    running: &mut HashMap<String, RunHandle>,
) {
    if running.contains_key(&t.name) {
        return; // already up; Start is idempotent so double-clicks are harmless
    }
    let errs = t.validate();
    if !errs.is_empty() {
        let msg = errs.join(" ");
        warn!(tunnel = %t.name, "cannot start: {msg}");
        let mut states = shared.states.lock();
        let s = states.entry(t.name.clone()).or_insert_with(|| {
            TunnelState::new(t.name.clone(), String::new(), t.port, t.metering())
        });
        s.status = Status::Failed;
        s.last_error = msg;
        return;
    }

    let advertised = t.proxy_url().unwrap_or_else(|| format!("{}:{}", t.bind, t.port));
    shared.states.lock().entry(t.name.clone()).or_insert_with(|| {
        TunnelState::new(t.name.clone(), advertised.clone(), t.port, t.metering())
    });
    shared.update(&t.name, |s| {
        s.advertised = advertised;
        s.port = t.port;
        s.metering = t.metering();
        s.status = Status::Starting;
        s.last_error.clear();
    });

    let (stop_tx, stop_rx) = mpsc::channel(1);
    running.insert(t.name.clone(), RunHandle { stop: stop_tx, def: t.clone() });

    let task = TunnelTask {
        tunnel: t.clone(),
        ssh_path: cfg.settings.ssh_path.clone(),
        probe: cfg
            .settings
            .probe_enabled
            .then(|| (cfg.settings.probe_target.clone(), cfg.settings.probe_interval_secs.max(30))),
        shared: shared.clone(),
    };
    tokio::spawn(task.run(stop_rx));
}

async fn stop(name: &str, shared: &Arc<Shared>, running: &mut HashMap<String, RunHandle>) {
    if let Some(h) = running.remove(name) {
        // The task owns the child; asking it to stop lets it kill the process
        // tree and tidy up rather than being cancelled mid-kill.
        let _ = h.stop.send(()).await;
    }
    shared.update(name, |s| {
        s.status = Status::Stopped;
        s.pid = None;
        s.up_since = None;
        s.next_retry_at = 0;
    });
}

struct TunnelTask {
    tunnel: Tunnel,
    ssh_path: String,
    probe: Option<(String, u64)>,
    shared: Arc<Shared>,
}

impl TunnelTask {
    async fn run(self, mut stop_rx: mpsc::Receiver<()>) {
        let name = self.tunnel.name.clone();
        loop {
            let started = now_unix();
            let outcome = self.one_run(&mut stop_rx).await;
            let ran_for = now_unix() - started;

            match outcome {
                RunOutcome::Stopped => break,
                RunOutcome::Exited(note) => {
                    // A run that lasted clears the streak: an hour-old tunnel
                    // dropping is a fresh problem, not a continuation of
                    // whatever happened at startup.
                    let stable = ran_for >= STABLE_AFTER_SECS;
                    let fails = {
                        let mut states = self.shared.states.lock();
                        let Some(s) = states.get_mut(&name) else { break };
                        s.fails = if stable { 1 } else { s.fails + 1 };
                        s.restarts += 1;
                        s.pid = None;
                        s.up_since = None;
                        if !note.is_empty() {
                            s.last_error = note.clone();
                        }
                        s.fails
                    };
                    let wait = retry_delay_secs(fails);
                    self.shared.update(&name, |s| {
                        s.status = Status::Retrying;
                        s.next_retry_at = now_unix() + wait as i64;
                    });
                    warn!(
                        tunnel = %name, ran_for, fails, wait,
                        "tunnel exited; retrying"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                        _ = stop_rx.recv() => break,
                    }
                }
            }
        }
        self.shared.update(&name, |s| {
            s.status = Status::Stopped;
            s.pid = None;
            s.up_since = None;
            s.next_retry_at = 0;
        });
        debug!(tunnel = %name, "supervisor task ended");
    }

    /// One spawn-and-watch cycle.
    async fn one_run(&self, stop_rx: &mut mpsc::Receiver<()>) -> RunOutcome {
        let t = &self.tunnel;
        let name = t.name.clone();

        // With metering on, ssh takes a private port and TunMan takes the
        // advertised one. Port 0 lets the OS pick a free one; we bind, read the
        // number and immediately drop it, which leaves a tiny race that
        // ExitOnForwardFailure turns into a clean retry rather than a silent
        // half-up tunnel.
        let bind = if t.metering() {
            match std::net::TcpListener::bind(("127.0.0.1", 0)).and_then(|l| l.local_addr()) {
                Ok(a) => Bind { addr: "127.0.0.1".into(), port: a.port() },
                Err(e) => return RunOutcome::Exited(format!("no free local port: {e}")),
            }
        } else {
            Bind { addr: t.bind.clone(), port: t.port }
        };

        let args = crate::ssh::args(t, &bind);
        debug!(tunnel = %name, "{}", crate::ssh::display_command(&self.ssh_path, t, &bind));

        let mut cmd = tokio::process::Command::new(&self.ssh_path);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The child dies with us. A tunnel manager that leaves orphan ssh
            // processes holding ports is worse than one that drops them.
            .kill_on_drop(true);
        if t.auth == AuthMode::Password {
            // ssh runs this helper to answer the prompt; the helper is TunMan
            // itself, reading the password from the environment it inherits.
            if let Ok(exe) = std::env::current_exe() {
                cmd.env("SSH_ASKPASS", exe);
                cmd.env("SSH_ASKPASS_REQUIRE", "force");
                cmd.env("TUNMAN_ASKPASS", "1");
                cmd.env("TUNMAN_PASSWORD", &t.password);
            }
        }
        #[cfg(windows)]
        {
            // Without this every tunnel flashes up a console window.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                error!(tunnel = %name, error = %e, "could not run {}", self.ssh_path);
                self.shared.update(&name, |s| {
                    s.status = Status::Failed;
                    s.last_error = format!("could not run {}: {e}", self.ssh_path);
                });
                return RunOutcome::Exited(String::new());
            }
        };
        let pid = child.id().unwrap_or(0);
        self.shared.update(&name, |s| s.pid = Some(pid));

        // Two separate readers: reading one pipe while the other fills is a
        // deadlock, and ssh writes almost everything to stderr.
        let last_line = Arc::new(Mutex::new(String::new()));
        if let Some(out) = child.stdout.take() {
            tokio::spawn(pump(out, name.clone(), false, last_line.clone(), t.password.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pump(err, name.clone(), true, last_line.clone(), t.password.clone()));
        }

        // A remote forward listens at the far end, so there is nothing here to
        // probe; it counts as up once the process is still running.
        let ready = if t.kind == TunnelKind::Remote {
            tokio::time::sleep(Duration::from_millis(500)).await;
            child.try_wait().ok().flatten().is_none()
        } else {
            let addr = format!("{}:{}", bind.addr, bind.port);
            wait_until_ready(&mut child, &addr, LISTEN_TIMEOUT_SECS).await == Ready::Listening
        };

        if ready {
            info!(tunnel = %name, pid, "up on {}", self.shared.states.lock()
                .get(&name).map(|s| s.advertised.clone()).unwrap_or_default());
            self.shared.update(&name, |s| {
                s.status = Status::Up;
                s.up_since = Some(now_unix());
            });
        }

        // Metering listener lives exactly as long as this run: dropping the
        // task closes the listener and every connection through it, so a
        // restart never leaves a stale front door open on the advertised port.
        let meter_task = if ready && t.metering() {
            let advertised: SocketAddr = match format!("{}:{}", t.bind, t.port).parse() {
                Ok(a) => a,
                Err(e) => {
                    warn!(tunnel = %name, error = %e, "bad bind address; metering off");
                    return self.watch(child, stop_rx, pid, last_line).await;
                }
            };
            let upstream: SocketAddr = format!("127.0.0.1:{}", bind.port).parse().expect("literal");
            let traffic =
                self.shared.states.lock().get(&name).map(|s| s.traffic.clone()).unwrap_or_default();
            let fixed = format!("{}:{}", t.dest_host, t.dest_port);
            let sniff = t.kind == TunnelKind::Socks;
            let n = name.clone();
            Some(tokio::spawn(async move {
                if let Err(e) = crate::meter::run_listener(
                    advertised,
                    upstream,
                    traffic,
                    sniff,
                    fixed,
                    n.clone(),
                )
                .await
                {
                    warn!(tunnel = %n, error = %e, "metering listener stopped");
                }
            }))
        } else {
            None
        };

        let probe_task = match (&self.probe, ready) {
            (Some((target, every)), true) if t.kind == TunnelKind::Socks => {
                let (target, every) = (target.clone(), *every);
                let addr = format!("{}:{}", t.bind, t.port);
                let shared = self.shared.clone();
                let n = name.clone();
                Some(tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(every)).await;
                        let (ok, note) = probe_socks(&addr, &target).await;
                        if !ok {
                            warn!(tunnel = %n, "probe failed: {note}");
                        }
                        shared.update(&n, |s| {
                            s.probe_ok = Some(ok);
                            s.probe_note = note.clone();
                        });
                    }
                }))
            }
            _ => None,
        };

        let outcome = self.watch(child, stop_rx, pid, last_line).await;
        if let Some(h) = meter_task {
            h.abort();
        }
        if let Some(h) = probe_task {
            h.abort();
        }
        outcome
    }

    /// Wait for the child to exit, or for a stop request.
    async fn watch(
        &self,
        mut child: tokio::process::Child,
        stop_rx: &mut mpsc::Receiver<()>,
        pid: u32,
        last_line: Arc<Mutex<String>>,
    ) -> RunOutcome {
        tokio::select! {
            status = child.wait() => {
                let note = {
                    let line = last_line.lock().clone();
                    if line.is_empty() {
                        match status {
                            Ok(s) => format!("ssh exited ({s})"),
                            Err(e) => format!("ssh exited: {e}"),
                        }
                    } else {
                        line
                    }
                };
                RunOutcome::Exited(note)
            }
            _ = stop_rx.recv() => {
                // ssh can have helpers of its own (ProxyCommand, askpass); a
                // plain kill leaves those running and holding the port.
                if pid != 0 {
                    crate::platform::kill_process_tree(pid);
                }
                let _ = child.kill().await;
                RunOutcome::Stopped
            }
        }
    }
}

enum RunOutcome {
    /// Asked to stop; do not retry.
    Stopped,
    /// Died on its own, with whatever it last said.
    Exited(String),
}

/// Copy a child's output into the log, one line at a time.
///
/// Emitted at `info`/`warn` rather than `trace` deliberately: under the default
/// filter a `trace` line is invisible, which would make the Log tab useless for
/// the exact thing it exists for.
async fn pump<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    tunnel: String,
    is_err: bool,
    last_line: Arc<Mutex<String>>,
    password: String,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = crate::model::redact(line.trim_end(), &password);
        if line.is_empty() {
            continue;
        }
        if is_err {
            *last_line.lock() = line.clone();
            warn!(target: "TunMan::ssh", tunnel = %tunnel, "{line}");
        } else {
            info!(target: "TunMan::ssh", tunnel = %tunnel, "{line}");
        }
    }
}

/// How waiting for a tunnel to come up ended.
#[derive(Debug, PartialEq, Eq)]
enum Ready {
    /// The forward is accepting connections.
    Listening,
    /// ssh gave up before the forward ever opened.
    Exited,
    /// Still running, still not listening.
    TimedOut,
}

/// Wait for `addr` to accept a connection, for `child` to die, or for the
/// timeout — whichever comes first.
///
/// This is the difference between "ssh is running" and "the forward is up":
/// with `ExitOnForwardFailure=yes` a bind failure kills ssh, while a slow
/// handshake just means the port is not there yet.
///
/// **The child has to be watched alongside the port.** A refused connection or
/// a rejected key kills ssh in about two seconds, and a wait that only polls
/// the port sits out the entire timeout afterwards: the failure is reported
/// ~18 s late, the retry clock starts that much later, and `ran_for` comes out
/// long enough to look like a tunnel that had been up and stable — which would
/// forgive the failure streak and reset the backoff to its shortest delay.
async fn wait_until_ready(
    child: &mut tokio::process::Child,
    addr: &str,
    timeout_secs: u64,
) -> Ready {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ready::Exited;
        }
        if TcpStream::connect(addr).await.is_ok() {
            return Ready::Listening;
        }
        if tokio::time::Instant::now() >= deadline {
            return Ready::TimedOut;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Drive a real SOCKS5 CONNECT through the proxy to `target`.
///
/// Answers the question the process list cannot: not "is ssh alive" but "does
/// traffic actually reach the far side". Returns the failure text for the row's
/// hover.
pub async fn probe_socks(proxy: &str, target: &str) -> (bool, String) {
    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(443)),
        None => (target.to_string(), 443),
    };
    let fut = async {
        let mut s = TcpStream::connect(proxy).await?;
        s.write_all(&[0x05, 0x01, 0x00]).await?; // greeting: no auth
        let mut reply = [0u8; 2];
        s.read_exact(&mut reply).await?;
        if reply[0] != 0x05 || reply[1] == 0xff {
            return Err(std::io::Error::other("proxy refused the no-auth method"));
        }
        let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
        req.extend_from_slice(host.as_bytes());
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).await?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head).await?;
        if head[1] != 0x00 {
            return Err(std::io::Error::other(format!("SOCKS reply code {}", head[1])));
        }
        Ok::<(), std::io::Error>(())
    };
    match tokio::time::timeout(Duration::from_secs(15), fut).await {
        Ok(Ok(())) => (true, format!("reached {target}")),
        Ok(Err(e)) => (false, e.to_string()),
        Err(_) => (false, "timed out".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_has_a_label_and_a_dot() {
        for s in [Status::Stopped, Status::Starting, Status::Up, Status::Retrying, Status::Failed] {
            assert!(!s.label().is_empty());
            assert!(!s.dot().is_empty());
        }
        assert_eq!(Status::Up.label(), "Up");
    }

    /// The table is driven by the config, not by what happens to be running, so
    /// a tunnel that has never started still gets a row.
    #[test]
    fn a_snapshot_covers_tunnels_that_have_never_run() {
        let shared = Shared::default();
        let order = vec![
            Tunnel { name: "a".into(), port: 1080, ..Default::default() },
            Tunnel { name: "b".into(), port: 1081, ..Default::default() },
        ];
        let snap = shared.snapshot(&order);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].status, Status::Stopped);
        assert_eq!(snap[0].advertised, "socks5h://127.0.0.1:1080");
    }

    /// Snapshot order follows the config so rows do not jump around as tunnels
    /// come and go.
    #[test]
    fn a_snapshot_keeps_the_config_order() {
        let shared = Shared::default();
        shared.states.lock().insert(
            "b".into(),
            TunnelState::new("b".into(), "socks5h://127.0.0.1:1081".into(), 1081, false),
        );
        let order = vec![
            Tunnel { name: "a".into(), port: 1080, ..Default::default() },
            Tunnel { name: "b".into(), port: 1081, ..Default::default() },
        ];
        let names: Vec<String> = shared.snapshot(&order).into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    /// A forward has no proxy URL, so the row still needs something to show and
    /// to copy.
    #[test]
    fn a_forward_advertises_its_host_and_port() {
        let shared = Shared::default();
        let order = vec![Tunnel {
            name: "db".into(),
            kind: TunnelKind::Local,
            port: 5432,
            ..Default::default()
        }];
        assert_eq!(shared.snapshot(&order)[0].advertised, "127.0.0.1:5432");
    }

    /// Spawn something harmless that stays alive, so the wait has a child to
    /// watch without needing ssh or a server.
    fn sleeper() -> tokio::process::Child {
        let mut c = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" });
        if cfg!(windows) {
            c.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        } else {
            c.arg("30");
        }
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn a sleeper")
    }

    #[tokio::test]
    async fn readiness_sees_an_open_port_and_times_out_on_a_closed_one() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let mut child = sleeper();
        assert_eq!(wait_until_ready(&mut child, &addr, 5).await, Ready::Listening);

        // A port nothing is listening on: still running, still not up.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = closed.local_addr().unwrap().to_string();
        drop(closed);
        assert_eq!(wait_until_ready(&mut child, &dead, 1).await, Ready::TimedOut);
        let _ = child.kill().await;
    }

    /// The regression this exists for: ssh that dies immediately must be
    /// noticed immediately. Polling only the port sat out the full 20-second
    /// timeout, which delayed the retry and inflated the run's apparent
    /// duration past the "this tunnel was stable" threshold.
    #[tokio::test]
    async fn readiness_gives_up_as_soon_as_the_child_dies() {
        let mut child = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) { vec!["/C", "exit 1"] } else { vec![] })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn");

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = closed.local_addr().unwrap().to_string();
        drop(closed);

        let started = std::time::Instant::now();
        assert_eq!(wait_until_ready(&mut child, &dead, 30).await, Ready::Exited);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned after {:?}; it should not wait out the timeout",
            started.elapsed()
        );
    }
}
