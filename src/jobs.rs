//! Running the things that are not tunnels: mounts, and sync jobs.
//!
//! Two supervisors sharing one shape — spawn a child, pump its output into the
//! log, watch it — but with different definitions of "working":
//!
//! - A **mount** is long-lived, and a live process proves nothing. What counts
//!   is whether the mount point answers, so it is polled while the mount runs;
//!   a mount can go stale under a process that is perfectly happy.
//! - A **sync job** is one-shot. It succeeds or fails, on a schedule or on
//!   demand, and its exit status is the answer.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::mounts::{Mount, MountKind, is_mounted};
use crate::sync::{Progress, SyncJob, parse_progress};
use crate::util::{now_unix, retry_delay_secs};

/// How often a live mount is re-checked. Frequent enough to notice a stale
/// mount quickly, rare enough that it is not itself I/O load on the remote.
const MOUNT_CHECK_SECS: u64 = 5;

/// How long to wait for a mount point to start answering after the process
/// starts. A cold remote and a WinFsp registration take a moment.
const MOUNT_READY_SECS: u64 = 45;

// ---------------------------------------------------------------- mounts ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MountStatus {
    #[default]
    Stopped,
    Starting,
    /// The mount point answers.
    Mounted,
    Retrying,
    /// Cannot run at all — a missing tool, or a bad definition.
    Failed,
}

impl MountStatus {
    pub fn label(self) -> &'static str {
        match self {
            MountStatus::Stopped => "Stopped",
            MountStatus::Starting => "Mounting",
            MountStatus::Mounted => "Mounted",
            MountStatus::Retrying => "Retrying",
            MountStatus::Failed => "Failed",
        }
    }

    pub fn dot(self) -> &'static str {
        match self {
            MountStatus::Mounted => "●",
            MountStatus::Starting | MountStatus::Retrying => "◐",
            MountStatus::Failed => "▲",
            MountStatus::Stopped => "○",
        }
    }
}

#[derive(Clone, Debug)]
pub struct MountState {
    pub name: String,
    pub status: MountStatus,
    pub pid: Option<u32>,
    pub since: Option<i64>,
    pub restarts: u32,
    pub fails: u32,
    pub last_error: String,
    pub next_retry_at: i64,
    pub target: String,
    pub source: String,
    /// Set when retries were abandoned after `max_retries`.
    pub gave_up: bool,
}

impl MountState {
    fn new(m: &Mount) -> MountState {
        MountState {
            name: m.name.clone(),
            status: MountStatus::Stopped,
            pid: None,
            since: None,
            restarts: 0,
            fails: 0,
            last_error: String::new(),
            next_retry_at: 0,
            target: m.target.clone(),
            source: m.source(),
            gave_up: false,
        }
    }
}

#[derive(Default)]
pub struct MountShared {
    pub states: Mutex<HashMap<String, MountState>>,
}

impl MountShared {
    pub fn snapshot(&self, order: &[Mount]) -> Vec<MountState> {
        let states = self.states.lock();
        order
            .iter()
            .map(|m| states.get(&m.name).cloned().unwrap_or_else(|| MountState::new(m)))
            .collect()
    }

    fn update(&self, name: &str, f: impl FnOnce(&mut MountState)) {
        if let Some(s) = self.states.lock().get_mut(name) {
            f(s);
        }
    }
}

#[derive(Clone, Debug)]
pub enum MountCommand {
    Start(String),
    Stop(String),
    StartAll,
    StopAll,
    Reload(Box<Config>),
    Shutdown,
}

struct MountHandle {
    stop: mpsc::Sender<()>,
    def: Mount,
}

pub async fn run_mounts(
    mut cfg: Config,
    shared: Arc<MountShared>,
    mut rx: mpsc::Receiver<MountCommand>,
) {
    let mut running: HashMap<String, MountHandle> = HashMap::new();

    for m in cfg.mounts.iter().filter(|m| m.enabled && m.auto_start) {
        start_mount(m, &cfg, &shared, &mut running).await;
    }

    while let Some(cmd) = rx.recv().await {
        match cmd {
            MountCommand::Start(name) => {
                if let Some(m) = cfg.mounts.iter().find(|m| m.name == name) {
                    start_mount(m, &cfg, &shared, &mut running).await;
                }
            }
            MountCommand::Stop(name) => stop_mount(&name, &shared, &mut running).await,
            MountCommand::StartAll => {
                for m in cfg.mounts.clone().iter().filter(|m| m.enabled) {
                    start_mount(m, &cfg, &shared, &mut running).await;
                }
            }
            MountCommand::StopAll => {
                for name in running.keys().cloned().collect::<Vec<_>>() {
                    stop_mount(&name, &shared, &mut running).await;
                }
            }
            MountCommand::Reload(new) => {
                let new = *new;
                for name in running.keys().cloned().collect::<Vec<_>>() {
                    let old = running.get(&name).map(|h| h.def.clone());
                    let now = new.mounts.iter().find(|m| m.name == name).cloned();
                    let changed = match (&old, &now) {
                        (Some(o), Some(n)) => o != n,
                        _ => true,
                    };
                    if changed {
                        stop_mount(&name, &shared, &mut running).await;
                        if let Some(m) = now.filter(|m| m.enabled) {
                            start_mount(&m, &new, &shared, &mut running).await;
                        }
                    }
                }
                cfg = new;
            }
            MountCommand::Shutdown => {
                for name in running.keys().cloned().collect::<Vec<_>>() {
                    stop_mount(&name, &shared, &mut running).await;
                }
                info!("mount supervisor stopped");
                return;
            }
        }
    }
}

async fn start_mount(
    m: &Mount,
    cfg: &Config,
    shared: &Arc<MountShared>,
    running: &mut HashMap<String, MountHandle>,
) {
    if running.contains_key(&m.name) {
        return;
    }
    shared.states.lock().entry(m.name.clone()).or_insert_with(|| MountState::new(m));

    let errs = m.validate();
    if !errs.is_empty() {
        let msg = errs.join(" ");
        warn!(mount = %m.name, "cannot mount: {msg}");
        shared.update(&m.name, |s| {
            s.status = MountStatus::Failed;
            s.last_error = msg;
        });
        return;
    }

    shared.update(&m.name, |s| {
        s.status = MountStatus::Starting;
        s.target = m.target.clone();
        s.source = m.source();
        s.gave_up = false;
        s.last_error.clear();
    });

    let (stop_tx, stop_rx) = mpsc::channel(1);
    running.insert(m.name.clone(), MountHandle { stop: stop_tx, def: m.clone() });

    let task = MountTask {
        mount: m.clone(),
        rclone: cfg.settings.rclone_path.clone(),
        sshfs: cfg.settings.sshfs_path.clone(),
        shared: shared.clone(),
    };
    tokio::spawn(task.run(stop_rx));
}

async fn stop_mount(
    name: &str,
    shared: &Arc<MountShared>,
    running: &mut HashMap<String, MountHandle>,
) {
    if let Some(h) = running.remove(name) {
        let _ = h.stop.send(()).await;
    }
    shared.update(name, |s| {
        s.status = MountStatus::Stopped;
        s.pid = None;
        s.since = None;
        s.next_retry_at = 0;
    });
}

struct MountTask {
    mount: Mount,
    rclone: String,
    sshfs: String,
    shared: Arc<MountShared>,
}

impl MountTask {
    async fn run(self, mut stop_rx: mpsc::Receiver<()>) {
        let name = self.mount.name.clone();
        loop {
            let started = now_unix();
            let stopped = self.one_run(&mut stop_rx).await;
            if stopped {
                break;
            }
            let ran_for = now_unix() - started;

            let (fails, give_up) = {
                let mut states = self.shared.states.lock();
                let Some(s) = states.get_mut(&name) else { break };
                s.fails = if ran_for >= 60 { 1 } else { s.fails + 1 };
                s.restarts += 1;
                s.pid = None;
                s.since = None;
                let give_up = self.mount.max_retries > 0 && s.fails > self.mount.max_retries;
                s.gave_up = give_up;
                (s.fails, give_up)
            };

            if give_up {
                warn!(
                    mount = %name, fails,
                    "gave up after {} consecutive failures", self.mount.max_retries
                );
                self.shared.update(&name, |s| s.status = MountStatus::Failed);
                return;
            }

            // A fixed delay when one is set, because some servers treat a
            // prompt reconnect as an attack and ban rather than reconnect.
            // Otherwise the same doubling backoff tunnels use.
            let wait = if self.mount.retry_delay_secs > 0 {
                self.mount.retry_delay_secs
            } else {
                retry_delay_secs(fails)
            };
            self.shared.update(&name, |s| {
                s.status = MountStatus::Retrying;
                s.next_retry_at = now_unix() + wait as i64;
            });
            warn!(mount = %name, ran_for, fails, wait, "mount ended; retrying");
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                _ = stop_rx.recv() => break,
            }
        }
        self.shared.update(&name, |s| {
            s.status = MountStatus::Stopped;
            s.pid = None;
            s.since = None;
            s.next_retry_at = 0;
        });
    }

    /// One mount attempt. Returns true when asked to stop.
    async fn one_run(&self, stop_rx: &mut mpsc::Receiver<()>) -> bool {
        let m = &self.mount;
        let name = m.name.clone();
        let (program, args) = crate::mounts::args(m, &self.rclone, &self.sshfs);

        if m.kind == MountKind::Sshfs && !crate::mounts::winfsp_installed() {
            self.fail(&name, "WinFsp is not installed — sshfs and rclone mount both need it.");
            return false;
        }

        debug!(mount = %name, "{program} {}", args.join(" "));
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let hint = if m.kind == MountKind::Sshfs {
                    " — sshfs-win does not appear to be installed. An rclone remote using \
                     the sftp backend does the same job."
                } else {
                    ""
                };
                self.fail(&name, &format!("could not run {program}: {e}{hint}"));
                return false;
            }
        };
        let pid = child.id().unwrap_or(0);
        crate::supervisor::adopt(&child);
        self.shared.update(&name, |s| s.pid = Some(pid));

        let last_line = Arc::new(Mutex::new(String::new()));
        if let Some(out) = child.stdout.take() {
            tokio::spawn(pump(out, name.clone(), "mount", false, last_line.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pump(err, name.clone(), "mount", true, last_line.clone()));
        }

        // Wait for the mount point to answer, watching the child at the same
        // time — a tool that exits immediately must not be waited out.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(MOUNT_READY_SECS);
        let target = m.target.clone();
        let mut ready = false;
        loop {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            let t = target.clone();
            if tokio::task::spawn_blocking(move || is_mounted(&t)).await.unwrap_or(false) {
                ready = true;
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(400)) => {}
                _ = stop_rx.recv() => {
                    self.kill(&mut child, pid).await;
                    return true;
                }
            }
        }

        if ready {
            info!(mount = %name, pid, "mounted at {}", m.target);
            self.shared.update(&name, |s| {
                s.status = MountStatus::Mounted;
                s.since = Some(now_unix());
                s.last_error.clear();
            });
        } else {
            let note = last_line.lock().clone();
            self.fail(
                &name,
                if note.is_empty() { "the mount point never answered" } else { &note },
            );
        }

        // Watch the process AND the mount point. A stale mount leaves the
        // process running and only reading the path finds out.
        loop {
            tokio::select! {
                status = child.wait() => {
                    let note = {
                        let line = last_line.lock().clone();
                        if line.is_empty() {
                            match status {
                                Ok(s) => format!("exited ({s})"),
                                Err(e) => format!("exited: {e}"),
                            }
                        } else { line }
                    };
                    self.shared.update(&name, |s| s.last_error = note);
                    return false;
                }
                _ = stop_rx.recv() => {
                    self.kill(&mut child, pid).await;
                    return true;
                }
                _ = tokio::time::sleep(Duration::from_secs(MOUNT_CHECK_SECS)) => {
                    if !ready {
                        continue;
                    }
                    let t = target.clone();
                    let ok = tokio::task::spawn_blocking(move || is_mounted(&t))
                        .await
                        .unwrap_or(false);
                    if !ok {
                        warn!(mount = %name, "mount point stopped answering; remounting");
                        self.shared.update(&name, |s| {
                            s.last_error = "the mount point stopped answering".into();
                        });
                        self.kill(&mut child, pid).await;
                        return false;
                    }
                }
            }
        }
    }

    async fn kill(&self, child: &mut tokio::process::Child, pid: u32) {
        if pid != 0 {
            crate::platform::kill_process_tree(pid);
        }
        let _ = child.kill().await;
    }

    fn fail(&self, name: &str, msg: &str) {
        error!(mount = %name, "{msg}");
        self.shared.update(name, |s| s.last_error = msg.to_string());
    }
}

// ------------------------------------------------------------------ sync ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum JobStatus {
    #[default]
    Idle,
    Running,
    /// Last run finished cleanly.
    Ok,
    /// Last run failed.
    Failed,
}

impl JobStatus {
    pub fn label(self) -> &'static str {
        match self {
            JobStatus::Idle => "Never run",
            JobStatus::Running => "Running",
            JobStatus::Ok => "OK",
            JobStatus::Failed => "Failed",
        }
    }

    pub fn dot(self) -> &'static str {
        match self {
            JobStatus::Running => "◐",
            JobStatus::Ok => "●",
            JobStatus::Failed => "▲",
            JobStatus::Idle => "○",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct JobState {
    pub name: String,
    pub status: JobStatus,
    /// True while the current run is a dry run.
    pub dry_run: bool,
    pub started_at: i64,
    pub finished_at: i64,
    pub last_ok_at: i64,
    pub last_error: String,
    pub progress: Progress,
    /// Set when the job is scheduled and idle.
    pub next_run_at: i64,
    /// What the last run actually did, for the detail panel.
    pub tail: Vec<String>,
}

#[derive(Default)]
pub struct SyncShared {
    pub states: Mutex<HashMap<String, JobState>>,
}

impl SyncShared {
    pub fn snapshot(&self, order: &[SyncJob]) -> Vec<JobState> {
        let states = self.states.lock();
        order
            .iter()
            .map(|j| {
                states
                    .get(&j.name)
                    .cloned()
                    .unwrap_or_else(|| JobState { name: j.name.clone(), ..Default::default() })
            })
            .collect()
    }

    fn update(&self, name: &str, f: impl FnOnce(&mut JobState)) {
        let mut states = self.states.lock();
        let s = states
            .entry(name.to_string())
            .or_insert_with(|| JobState { name: name.to_string(), ..Default::default() });
        f(s);
    }
}

#[derive(Clone, Debug)]
pub enum SyncCommand {
    /// Run now. `dry_run` reports what would happen and changes nothing.
    Run {
        name: String,
        dry_run: bool,
    },
    Cancel(String),
    Reload(Box<Config>),
    Shutdown,
}

pub async fn run_sync(
    mut cfg: Config,
    shared: Arc<SyncShared>,
    mut rx: mpsc::Receiver<SyncCommand>,
) {
    let mut running: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(20));

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return };
                match cmd {
                    SyncCommand::Run { name, dry_run } => {
                        if let Some(j) = cfg.jobs.iter().find(|j| j.name == name) {
                            spawn_job(j, &cfg, &shared, &mut running, dry_run);
                        }
                    }
                    SyncCommand::Cancel(name) => {
                        if let Some(h) = running.remove(&name) {
                            h.abort();
                            shared.update(&name, |s| {
                                s.status = JobStatus::Failed;
                                s.last_error = "cancelled".into();
                                s.finished_at = now_unix();
                            });
                        }
                    }
                    SyncCommand::Reload(new) => cfg = *new,
                    SyncCommand::Shutdown => {
                        for (_, h) in running.drain() {
                            h.abort();
                        }
                        info!("sync supervisor stopped");
                        return;
                    }
                }
            }
            _ = tick.tick() => {
                running.retain(|_, h| !h.is_finished());
                let now = now_unix();
                for j in cfg.jobs.clone() {
                    if !j.enabled || j.interval_mins == 0 || running.contains_key(&j.name) {
                        continue;
                    }
                    let due = {
                        let states = shared.states.lock();
                        match states.get(&j.name) {
                            // Never run: due now, so a new scheduled job does
                            // something visible rather than waiting a full
                            // interval before its first sign of life.
                            None => true,
                            Some(s) => {
                                let last = s.finished_at.max(s.started_at);
                                last == 0 || now - last >= (j.interval_mins * 60) as i64
                            }
                        }
                    };
                    if due {
                        spawn_job(&j, &cfg, &shared, &mut running, false);
                    }
                }
                // Refresh the countdown shown on idle rows.
                for j in &cfg.jobs {
                    if j.interval_mins == 0 {
                        continue;
                    }
                    shared.update(&j.name, |s| {
                        if s.status != JobStatus::Running {
                            let last = s.finished_at.max(s.started_at);
                            s.next_run_at = last + (j.interval_mins * 60) as i64;
                        }
                    });
                }
            }
        }
    }
}

fn spawn_job(
    job: &SyncJob,
    cfg: &Config,
    shared: &Arc<SyncShared>,
    running: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    dry_run: bool,
) {
    if running.contains_key(&job.name) {
        return;
    }
    let errs = job.validate();
    if !errs.is_empty() {
        let msg = errs.join(" ");
        warn!(job = %job.name, "cannot run: {msg}");
        shared.update(&job.name, |s| {
            s.status = JobStatus::Failed;
            s.last_error = msg;
        });
        return;
    }
    let job = job.clone();
    let rclone = cfg.settings.rclone_path.clone();
    let shared = shared.clone();
    let name = job.name.clone();
    let handle = tokio::spawn(async move { run_one_job(job, rclone, shared, dry_run).await });
    running.insert(name, handle);
}

async fn run_one_job(job: SyncJob, rclone: String, shared: Arc<SyncShared>, dry_run: bool) {
    let name = job.name.clone();
    let args = crate::sync::args(&job, dry_run);
    info!(
        job = %name,
        mode = job.mode.verb(),
        dry_run,
        "{} → {}", job.source, job.dest
    );
    debug!(job = %name, "{rclone} {}", args.join(" "));

    shared.update(&name, |s| {
        s.status = JobStatus::Running;
        s.dry_run = dry_run;
        s.started_at = now_unix();
        s.finished_at = 0;
        s.last_error.clear();
        s.progress = Progress::default();
        s.tail.clear();
    });

    let mut cmd = tokio::process::Command::new(&rclone);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            error!(job = %name, "could not run {rclone}: {e}");
            shared.update(&name, |s| {
                s.status = JobStatus::Failed;
                s.last_error = format!("could not run {rclone}: {e}");
                s.finished_at = now_unix();
            });
            return;
        }
    };

    crate::supervisor::adopt(&child);

    // rclone puts its stats on stderr; both are read so neither pipe can fill.
    let last_line = Arc::new(Mutex::new(String::new()));
    if let Some(out) = child.stdout.take() {
        tokio::spawn(pump_job(out, name.clone(), shared.clone(), false, last_line.clone()));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(pump_job(err, name.clone(), shared.clone(), true, last_line.clone()));
    }

    let status = child.wait().await;
    let ok = matches!(&status, Ok(s) if s.success());
    let note = match &status {
        Ok(s) if s.success() => String::new(),
        Ok(s) => {
            let line = last_line.lock().clone();
            if line.is_empty() { format!("rclone exited ({s})") } else { line }
        }
        Err(e) => format!("rclone failed: {e}"),
    };
    if ok {
        info!(job = %name, dry_run, "finished");
    } else {
        warn!(job = %name, "failed: {note}");
    }

    shared.update(&name, |s| {
        s.status = if ok { JobStatus::Ok } else { JobStatus::Failed };
        s.finished_at = now_unix();
        s.last_error = note.clone();
        if ok && !dry_run {
            s.last_ok_at = now_unix();
        }
    });
}

/// Copy a child's output into the log, one line at a time.
async fn pump<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    name: String,
    field: &'static str,
    is_err: bool,
    last_line: Arc<Mutex<String>>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim_end().to_string();
        if line.is_empty() {
            continue;
        }
        if is_err {
            *last_line.lock() = line.clone();
            warn!(target: "TunMan::tool", tunnel = %format!("{field}:{name}"), "{line}");
        } else {
            info!(target: "TunMan::tool", tunnel = %format!("{field}:{name}"), "{line}");
        }
    }
}

/// Like [`pump`], but also pulls progress out of rclone's stats lines.
async fn pump_job<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    name: String,
    shared: Arc<SyncShared>,
    is_err: bool,
    last_line: Arc<Mutex<String>>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim_end().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(p) = parse_progress(&line) {
            shared.update(&name, |s| s.progress = p);
            // Stats are a once-a-second heartbeat; logging every one would
            // bury everything else the job says.
            continue;
        }
        shared.update(&name, |s| {
            s.tail.push(line.clone());
            // The panel shows the end of the run; an all-night job must not
            // accumulate its whole output in memory.
            if s.tail.len() > 200 {
                s.tail.remove(0);
            }
        });
        if is_err {
            // rclone says a great deal on stderr that is not an error, so the
            // remembered "last line" is only used when the job actually fails.
            *last_line.lock() = line.clone();
            info!(target: "TunMan::tool", tunnel = %format!("sync:{name}"), "{line}");
        } else {
            info!(target: "TunMan::tool", tunnel = %format!("sync:{name}"), "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_all_have_a_label_and_a_dot() {
        for s in [
            MountStatus::Stopped,
            MountStatus::Starting,
            MountStatus::Mounted,
            MountStatus::Retrying,
            MountStatus::Failed,
        ] {
            assert!(!s.label().is_empty());
            assert!(!s.dot().is_empty());
        }
        for s in [JobStatus::Idle, JobStatus::Running, JobStatus::Ok, JobStatus::Failed] {
            assert!(!s.label().is_empty());
            assert!(!s.dot().is_empty());
        }
    }

    /// The table is driven by the config, so a mount that has never run still
    /// gets a row rather than vanishing until something starts it.
    #[test]
    fn a_mount_snapshot_covers_definitions_that_have_never_run() {
        let shared = MountShared::default();
        let order = vec![Mount {
            name: "backups".into(),
            remote: "nas:backups".into(),
            target: "X:".into(),
            ..Default::default()
        }];
        let snap = shared.snapshot(&order);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].status, MountStatus::Stopped);
        assert_eq!(snap[0].source, "nas:backups");
    }

    #[test]
    fn a_job_snapshot_covers_jobs_that_have_never_run() {
        let shared = SyncShared::default();
        let order = vec![SyncJob { name: "photos".into(), ..Default::default() }];
        let snap = shared.snapshot(&order);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].status, JobStatus::Idle);
    }

    /// A fixed retry delay is the whole reason the field exists: some servers
    /// ban a client that reconnects the instant it drops, so the configured
    /// delay must win over the doubling backoff.
    #[test]
    fn a_fixed_retry_delay_overrides_the_backoff() {
        let m = Mount { retry_delay_secs: 120, ..Default::default() };
        let wait = if m.retry_delay_secs > 0 { m.retry_delay_secs } else { retry_delay_secs(1) };
        assert_eq!(wait, 120);

        let m = Mount { retry_delay_secs: 0, ..Default::default() };
        let wait = if m.retry_delay_secs > 0 { m.retry_delay_secs } else { retry_delay_secs(1) };
        assert_eq!(wait, 5, "falls back to the doubling backoff");
    }
}
