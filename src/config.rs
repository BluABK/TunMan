//! `TunMan.toml` — the whole of TunMan's persistent state.
//!
//! Two rules shape this module. **A file we could not parse is never
//! overwritten**: a typo in a hand-edit must not cost you the file, so a load
//! failure surfaces as an error the UI shows and saving is refused until it is
//! resolved. And **saves are atomic** (write a sibling temp file, then rename),
//! because the alternative is a truncated config if the machine dies mid-write
//! — the one moment losing it would hurt most.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::Tunnel;
use crate::mounts::Mount;
use crate::sync::SyncJob;

/// App-wide settings. Every field has a default, so an older config file with
/// none of them still loads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The ssh binary. Left as `ssh` it resolves on PATH — on Windows that is
    /// normally `C:\Windows\System32\OpenSSH\ssh.exe`.
    pub ssh_path: String,
    /// Start minimised to the tray. Set automatically when launched with
    /// `--hidden`, which is how the auto-start entry runs it.
    pub start_hidden: bool,
    pub start_with_windows: bool,
    /// Keep a Start Menu shortcut pointing at the running executable,
    /// refreshed on every launch so it survives the binary moving.
    pub start_menu_shortcut: bool,
    /// Master switch for per-tunnel `auto_start`. Off means no tunnel comes up
    /// on its own, whatever the individual flags say — one place to stop
    /// everything without editing every tunnel.
    pub autostart_tunnels: bool,

    /// Periodically check that a SOCKS tunnel can actually reach the internet,
    /// rather than trusting that ssh is still running.
    pub probe_enabled: bool,
    /// `host:port` to connect to through the proxy during a probe.
    pub probe_target: String,
    pub probe_interval_secs: u64,

    /// The rclone binary, for mounts and sync jobs. Left as `rclone` it
    /// resolves on PATH.
    pub rclone_path: String,
    /// The sshfs binary. On Windows this comes from sshfs-win, which is a
    /// separate install; an rclone remote on the sftp backend does the same
    /// job without it.
    pub sshfs_path: String,

    pub log_retention_days: u64,

    /// Optional convenience only. TunMan is standalone; this exists so a
    /// working tunnel can be pushed into StreamArchiver's proxy pool without
    /// copy-paste, and it is off unless asked for.
    pub sa_integration_enabled: bool,
    /// Path to `streamarchiver.sqlite3`. Empty means "work it out from
    /// %APPDATA%".
    pub sa_db_path: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ssh_path: "ssh".to_string(),
            start_hidden: false,
            start_with_windows: false,
            start_menu_shortcut: true,
            autostart_tunnels: true,
            probe_enabled: false,
            probe_target: "example.com:443".to_string(),
            probe_interval_secs: 300,
            rclone_path: "rclone".to_string(),
            sshfs_path: "sshfs".to_string(),
            log_retention_days: 7,
            sa_integration_enabled: false,
            sa_db_path: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub settings: Settings,
    /// `[[tunnel]]` blocks, in the order they are shown.
    #[serde(rename = "tunnel")]
    pub tunnels: Vec<Tunnel>,
    /// `[[mount]]` blocks.
    #[serde(rename = "mount")]
    pub mounts: Vec<Mount>,
    /// `[[job]]` blocks — rclone sync jobs.
    #[serde(rename = "job")]
    pub jobs: Vec<SyncJob>,
}

impl Config {
    /// Load from `path`. A missing file is not an error — it is a first run,
    /// and returns defaults.
    pub fn load(path: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write to `path` atomically: a temp file in the same directory, then a
    /// rename. Same-directory matters — a rename across volumes is a copy, and
    /// stops being atomic.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            crate::app_paths::ensure_dir(dir);
        }
        let text = toml::to_string_pretty(self).context("serialising config")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Index of the tunnel named `name`, if any. Names are the identity used
    /// across the config, the logs and the UI.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.tunnels.iter().position(|t| t.name == name)
    }

    /// A name not already taken, derived from `base` — `vps`, `vps-2`, `vps-3`.
    pub fn unique_name(&self, base: &str) -> String {
        let base = if base.trim().is_empty() { "tunnel" } else { base.trim() };
        if self.index_of(base).is_none() {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|c| self.index_of(c).is_none())
            .unwrap_or_default()
    }

    /// A name not already used by a mount.
    pub fn unique_mount_name(&self, base: &str) -> String {
        let base = if base.trim().is_empty() { "mount" } else { base.trim() };
        let taken = |n: &str| self.mounts.iter().any(|m| m.name == n);
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|n| format!("{base}-{n}")).find(|c| !taken(c)).unwrap_or_default()
    }

    /// A name not already used by a sync job.
    pub fn unique_job_name(&self, base: &str) -> String {
        let base = if base.trim().is_empty() { "job" } else { base.trim() };
        let taken = |n: &str| self.jobs.iter().any(|j| j.name == n);
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|n| format!("{base}-{n}")).find(|c| !taken(c)).unwrap_or_default()
    }

    /// A local port nothing in this config is already using, starting at
    /// `from`. Only catches TunMan's own collisions — a port held by another
    /// program still fails at bind time, which ssh reports through
    /// `ExitOnForwardFailure`.
    pub fn free_port(&self, from: u16) -> u16 {
        let taken: Vec<u16> = self.tunnels.iter().map(|t| t.port).collect();
        (from..=u16::MAX).find(|p| !taken.contains(p)).unwrap_or(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthMode, TunnelKind};

    fn sample() -> Config {
        Config {
            settings: Settings { ssh_path: "ssh".into(), ..Default::default() },
            tunnels: vec![
                Tunnel {
                    name: "vps-fi".into(),
                    user: "blu".into(),
                    host: "fi.example.org".into(),
                    port: 1080,
                    meter: true,
                    ..Default::default()
                },
                Tunnel {
                    name: "db".into(),
                    kind: TunnelKind::Local,
                    host: "bastion".into(),
                    port: 5432,
                    dest_host: "db.internal".into(),
                    dest_port: 5432,
                    auth: AuthMode::Password,
                    password: "hunter2".into(),
                    ..Default::default()
                },
            ],
            mounts: Vec::new(),
            jobs: Vec::new(),
        }
    }

    #[test]
    fn a_config_survives_a_round_trip() {
        let c = sample();
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }

    /// Hand-editing is a supported way to use this file, so an older or
    /// partially-written config must load rather than fail. Everything absent
    /// falls back to the same defaults a fresh install gets.
    #[test]
    fn a_minimal_hand_written_config_loads() {
        let text = r#"
            [[tunnel]]
            name = "vps"
            host = "example.org"
        "#;
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.tunnels.len(), 1);
        assert_eq!(c.tunnels[0].name, "vps");
        assert_eq!(c.tunnels[0].kind, TunnelKind::Socks, "SOCKS is the default kind");
        assert_eq!(c.tunnels[0].ssh_port, 22);
        assert_eq!(c.tunnels[0].bind, "127.0.0.1");
        assert_eq!(c.settings.ssh_path, "ssh");
    }

    /// Mounts and jobs live in the same file as tunnels and must survive the
    /// same round trip — a config that loses half of itself on save is worse
    /// than one that refuses to save at all.
    #[test]
    fn mounts_and_jobs_round_trip_with_the_tunnels() {
        let c = Config {
            settings: Settings::default(),
            tunnels: sample().tunnels,
            mounts: vec![crate::mounts::Mount {
                name: "backups".into(),
                remote: "nas:backups".into(),
                target: "X:".into(),
                retry_delay_secs: 120,
                ..Default::default()
            }],
            jobs: vec![crate::sync::SyncJob {
                name: "photos".into(),
                source: "local:D:/photos".into(),
                dest: "offsite:photos".into(),
                interval_mins: 60,
                ..Default::default()
            }],
        };
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
        assert!(text.contains("[[mount]]"), "serialised under the expected key");
        assert!(text.contains("[[job]]"));
    }

    /// A config written before mounts existed must still load.
    #[test]
    fn a_config_without_mounts_or_jobs_still_loads() {
        let text = r#"
            [[tunnel]]
            name = "vps"
            host = "example.org"
        "#;
        let c: Config = toml::from_str(text).unwrap();
        assert!(c.mounts.is_empty());
        assert!(c.jobs.is_empty());
        assert_eq!(c.settings.rclone_path, "rclone");
    }

    #[test]
    fn unique_names_are_found_per_kind() {
        let mut c = Config::default();
        c.mounts.push(crate::mounts::Mount { name: "mount".into(), ..Default::default() });
        c.jobs.push(crate::sync::SyncJob { name: "job".into(), ..Default::default() });
        assert_eq!(c.unique_mount_name("mount"), "mount-2");
        assert_eq!(c.unique_job_name("job"), "job-2");
        // The namespaces are separate: a mount named "job" does not block one.
        assert_eq!(c.unique_mount_name("job"), "job");
    }

    #[test]
    fn a_missing_file_is_a_first_run_not_an_error() {
        let path = std::env::temp_dir().join("TunMan-does-not-exist-42.toml");
        let _ = std::fs::remove_file(&path);
        assert_eq!(Config::load(&path).unwrap(), Config::default());
    }

    /// A typo in a hand-edit must not silently become an empty config that then
    /// overwrites the original. Load reports the error; the caller keeps the
    /// file untouched.
    #[test]
    fn a_broken_file_is_an_error_not_an_empty_config() {
        let path = std::env::temp_dir().join("TunMan-broken.toml");
        std::fs::write(&path, "[[tunnel]\nname = ").unwrap();
        assert!(Config::load(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_then_load_returns_what_was_saved() {
        let path = std::env::temp_dir().join("TunMan-roundtrip.toml");
        let c = sample();
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unique_name_walks_past_what_is_taken() {
        let c = sample();
        assert_eq!(c.unique_name("new"), "new");
        assert_eq!(c.unique_name("vps-fi"), "vps-fi-2");
        assert_eq!(c.unique_name("   "), "tunnel");
    }

    #[test]
    fn free_port_skips_ports_this_config_already_uses() {
        let c = sample();
        assert_eq!(c.free_port(1080), 1081);
        assert_eq!(c.free_port(9000), 9000);
    }
}
