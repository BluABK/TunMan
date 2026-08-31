//! Filesystem mounts: sshfs and `rclone mount`.
//!
//! Same supervision shape as a tunnel — a long-lived child, watched, restarted
//! with a backoff — but the readiness test is different. A mount process being
//! alive says nothing; what matters is whether the mount point actually
//! answers. So a mount is only *up* once its path can be listed, and it is
//! re-checked while it runs, because a mount can go stale underneath a process
//! that is still perfectly happy.
//!
//! **The retry delay is per mount, and deliberately configurable.** Some
//! servers respond badly to being reconnected the moment they drop — fail2ban
//! is the usual culprit, and a manager that retries eagerly will get itself
//! banned rather than reconnected.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What provides the mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    /// `rclone mount <remote>: <target>`. Uses any remote already in your
    /// rclone config, including its `sftp` backend — which covers the sshfs
    /// case without needing sshfs installed at all.
    #[default]
    Rclone,
    /// `sshfs user@host:/path <target>`. Needs sshfs-win on Windows.
    Sshfs,
}

impl MountKind {
    pub const ALL: [MountKind; 2] = [MountKind::Rclone, MountKind::Sshfs];

    pub fn label(self) -> &'static str {
        match self {
            MountKind::Rclone => "rclone",
            MountKind::Sshfs => "sshfs",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            MountKind::Rclone => {
                "Mounts any remote from your rclone config. Its sftp backend does the same \
                 job as sshfs, so this covers ssh servers too without a separate install."
            }
            MountKind::Sshfs => {
                "Mounts an ssh server directly. Needs sshfs-win on Windows; if it is not \
                 installed, an rclone remote using the sftp backend does the same job."
            }
        }
    }
}

/// One mount definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mount {
    pub name: String,
    pub kind: MountKind,
    pub enabled: bool,
    pub auto_start: bool,

    /// Where it appears: a drive letter like `X:` on Windows, or a directory.
    pub target: String,

    /// rclone: the remote and path, e.g. `nas:backups`.
    pub remote: String,

    /// sshfs: `user@host` and the remote directory.
    pub user: String,
    pub host: String,
    pub ssh_port: u16,
    pub remote_path: String,
    pub identity_file: String,

    /// Seconds to wait before reconnecting after a drop. Zero uses the same
    /// doubling backoff tunnels use; a fixed value is for servers that dislike
    /// being prodded.
    pub retry_delay_secs: u64,
    /// Stop retrying after this many consecutive failures. Zero means never
    /// stop.
    pub max_retries: u32,

    /// Passed to the tool verbatim, after everything generated here.
    pub extra_args: Vec<String>,
    /// rclone only: keep a local cache so ordinary programs can write.
    pub vfs_cache: bool,
    /// Mount read-only. Worth having on a backup target.
    pub read_only: bool,
}

impl Default for Mount {
    fn default() -> Self {
        Mount {
            name: String::new(),
            kind: MountKind::Rclone,
            enabled: true,
            auto_start: false,
            target: String::new(),
            remote: String::new(),
            user: String::new(),
            host: String::new(),
            ssh_port: 22,
            remote_path: "/".to_string(),
            identity_file: String::new(),
            retry_delay_secs: 0,
            max_retries: 0,
            extra_args: Vec::new(),
            // On by default: without a cache, rclone's mount refuses the
            // read-modify-write that ordinary Windows programs do constantly,
            // and the mount looks broken for anything but streaming reads.
            vfs_cache: true,
            read_only: false,
        }
    }
}

impl Mount {
    /// Problems that would stop this mount from starting.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.name.trim().is_empty() {
            errs.push("Name is required.".into());
        }
        if self.target.trim().is_empty() {
            errs.push("A drive letter or directory to mount at is required.".into());
        }
        match self.kind {
            MountKind::Rclone => {
                if self.remote.trim().is_empty() {
                    errs.push("Pick an rclone remote.".into());
                }
            }
            MountKind::Sshfs => {
                if self.host.trim().is_empty() {
                    errs.push("Host is required.".into());
                }
                if self.remote_path.trim().is_empty() {
                    errs.push("A remote path is required.".into());
                }
            }
        }
        errs
    }

    /// `user@host`, or the bare host when no user is set.
    pub fn target_host(&self) -> String {
        if self.user.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.user, self.host)
        }
    }

    /// What this mount is of, for display.
    pub fn source(&self) -> String {
        match self.kind {
            MountKind::Rclone => self.remote.clone(),
            MountKind::Sshfs => format!("{}:{}", self.target_host(), self.remote_path),
        }
    }
}

/// The program and arguments to run for a mount.
///
/// Pure, so the flags can be asserted rather than discovered by watching a
/// mount fail. `rclone_path` and `sshfs_path` are the configured binaries.
pub fn args(m: &Mount, rclone_path: &str, sshfs_path: &str) -> (String, Vec<String>) {
    let mut a: Vec<String> = Vec::new();
    match m.kind {
        MountKind::Rclone => {
            a.push("mount".into());
            a.push(m.remote.clone());
            a.push(m.target.clone());
            if m.vfs_cache {
                // "writes" rather than "full": ordinary programs can then do a
                // read-modify-write, without rclone also caching every byte
                // read back, which on a big remote fills the disk.
                a.push("--vfs-cache-mode".into());
                a.push("writes".into());
            }
            if m.read_only {
                a.push("--read-only".into());
            }
            // Without this the mount vanishes when the console it was launched
            // from goes away, which for a tray app is immediately.
            a.push("--no-console".into());
        }
        MountKind::Sshfs => {
            a.push(format!("{}:{}", m.target_host(), m.remote_path));
            a.push(m.target.clone());
            // Stay in the foreground: the supervisor watches this process, and
            // a tool that forks away is a tool it can neither watch nor stop.
            a.push("-f".into());
            if m.ssh_port != 22 {
                a.push("-p".into());
                a.push(m.ssh_port.to_string());
            }
            if !m.identity_file.trim().is_empty() {
                a.push("-o".into());
                a.push(format!("IdentityFile={}", m.identity_file.trim()));
            }
            if m.read_only {
                a.push("-o".into());
                a.push("ro".into());
            }
            a.push("-o".into());
            a.push("reconnect".into());
        }
    }
    a.extend(m.extra_args.iter().filter(|s| !s.trim().is_empty()).cloned());

    let program = match m.kind {
        MountKind::Rclone => rclone_path.to_string(),
        MountKind::Sshfs => sshfs_path.to_string(),
    };
    (program, a)
}

/// Whether a mount point is actually answering.
///
/// The real test, and the reason a live process is not enough: a mount can go
/// stale while its process stays perfectly happy, and reading the directory is
/// the only thing that finds out.
pub fn is_mounted(target: &str) -> bool {
    let path = PathBuf::from(target.trim());
    if path.as_os_str().is_empty() {
        return false;
    }
    // A drive letter needs the trailing separator; `X:` alone means "the
    // current directory on X:", which is a different thing and can succeed
    // when the drive is not mounted.
    let path = if target.trim().len() == 2 && target.trim().ends_with(':') {
        PathBuf::from(format!("{}\\", target.trim()))
    } else {
        path
    };
    std::fs::read_dir(&path).is_ok()
}

/// The remotes already configured in rclone.
///
/// Read from rclone itself rather than parsed out of its config file: the file
/// has an ini-ish format with encrypted sections, and rclone is the only thing
/// that can be trusted to say what it will actually accept.
pub fn list_remotes(rclone_path: &str) -> Vec<String> {
    let mut cmd = std::process::Command::new(rclone_path);
    cmd.arg("listremotes");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let Ok(out) = cmd.output() else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Where sshfs might be, when it is not on PATH.
pub fn sshfs_candidates() -> Vec<PathBuf> {
    vec![
        PathBuf::from(r"C:\Program Files\SSHFS-Win\bin\sshfs.exe"),
        PathBuf::from(r"C:\Program Files (x86)\SSHFS-Win\bin\sshfs.exe"),
    ]
}

/// Whether WinFsp is present. Both rclone mount and sshfs need it on Windows,
/// and its absence produces an error that does not obviously say so.
pub fn winfsp_installed() -> bool {
    !cfg!(windows)
        || [r"C:\Program Files (x86)\WinFsp", r"C:\Program Files\WinFsp"]
            .iter()
            .any(|p| PathBuf::from(p).is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rclone_mount() -> Mount {
        Mount {
            name: "backups".into(),
            kind: MountKind::Rclone,
            remote: "nas:backups".into(),
            target: "X:".into(),
            ..Default::default()
        }
    }

    fn sshfs_mount() -> Mount {
        Mount {
            name: "vps".into(),
            kind: MountKind::Sshfs,
            user: "blu".into(),
            host: "fi.example.org".into(),
            remote_path: "/srv/data".into(),
            target: "Y:".into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_rclone_mount_builds_the_expected_line() {
        let (prog, a) = args(&rclone_mount(), "rclone", "sshfs");
        assert_eq!(prog, "rclone");
        assert_eq!(a[0], "mount");
        assert_eq!(a[1], "nas:backups");
        assert_eq!(a[2], "X:");
        assert!(a.contains(&"--vfs-cache-mode".to_string()));
        assert!(a.contains(&"writes".to_string()));
    }

    /// Without a write cache, rclone refuses the read-modify-write that
    /// ordinary Windows programs do constantly, and the mount reads as broken
    /// for anything but streaming. It is on by default for that reason.
    #[test]
    fn the_write_cache_is_on_unless_turned_off() {
        assert!(rclone_mount().vfs_cache, "on by default");
        let m = Mount { vfs_cache: false, ..rclone_mount() };
        let (_, a) = args(&m, "rclone", "sshfs");
        assert!(!a.contains(&"--vfs-cache-mode".to_string()));
    }

    /// A tool that forks into the background is one the supervisor can neither
    /// watch nor stop, so sshfs is pinned to the foreground.
    #[test]
    fn sshfs_stays_in_the_foreground() {
        let (prog, a) = args(&sshfs_mount(), "rclone", "sshfs");
        assert_eq!(prog, "sshfs");
        assert_eq!(a[0], "blu@fi.example.org:/srv/data");
        assert_eq!(a[1], "Y:");
        assert!(a.contains(&"-f".to_string()));
    }

    #[test]
    fn a_moved_ssh_port_and_a_key_are_passed_through() {
        let m = Mount {
            ssh_port: 2222,
            identity_file: "C:/keys/fi.pem".into(),
            read_only: true,
            ..sshfs_mount()
        };
        let (_, a) = args(&m, "rclone", "sshfs");
        assert!(a.contains(&"2222".to_string()));
        assert!(a.contains(&"IdentityFile=C:/keys/fi.pem".to_string()));
        assert!(a.contains(&"ro".to_string()));
    }

    #[test]
    fn read_only_reaches_rclone_too() {
        let m = Mount { read_only: true, ..rclone_mount() };
        let (_, a) = args(&m, "rclone", "sshfs");
        assert!(a.contains(&"--read-only".to_string()));
    }

    #[test]
    fn extra_args_are_appended_and_blanks_skipped() {
        let m = Mount {
            extra_args: vec!["--dir-cache-time".into(), "5m".into(), "  ".into()],
            ..rclone_mount()
        };
        let (_, a) = args(&m, "rclone", "sshfs");
        assert!(a.contains(&"--dir-cache-time".to_string()));
        assert!(!a.iter().any(|s| s.trim().is_empty()));
    }

    #[test]
    fn validation_names_what_is_missing_per_kind() {
        assert!(rclone_mount().validate().is_empty());
        assert!(sshfs_mount().validate().is_empty());

        let m = Mount { remote: String::new(), ..rclone_mount() };
        assert_eq!(m.validate().len(), 1, "an rclone mount needs a remote");

        // ...but an sshfs mount does not — it needs a host instead.
        let m = Mount { remote: String::new(), ..sshfs_mount() };
        assert!(m.validate().is_empty());

        let m = Mount { host: String::new(), ..sshfs_mount() };
        assert_eq!(m.validate().len(), 1);

        let m = Mount { target: String::new(), ..rclone_mount() };
        assert_eq!(m.validate().len(), 1);
    }

    #[test]
    fn source_describes_what_is_being_mounted() {
        assert_eq!(rclone_mount().source(), "nas:backups");
        assert_eq!(sshfs_mount().source(), "blu@fi.example.org:/srv/data");
    }

    /// A directory that exists is mounted enough for our purposes; one that
    /// does not is not. The temp dir stands in for a live mount.
    #[test]
    fn is_mounted_tests_the_path_not_the_process() {
        let real = std::env::temp_dir();
        assert!(is_mounted(&real.display().to_string()));
        assert!(!is_mounted(r"Q:\definitely-not-here-42"));
        assert!(!is_mounted(""), "an empty target is never mounted");
        assert!(!is_mounted("   "));
    }
}
