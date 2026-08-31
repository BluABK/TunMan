//! rclone sync jobs — the DIY cloud half.
//!
//! **`sync` deletes.** rclone's `sync` makes the destination match the source,
//! which means removing anything at the destination that is not at the source.
//! Point it at the wrong path once and it is not a failed transfer, it is data
//! gone. So jobs default to [`SyncMode::Copy`], which only ever adds, the
//! destructive modes are labelled as such, and every job can be dry-run first.

use serde::{Deserialize, Serialize};

/// What a job does to the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Add and update. Never deletes.
    #[default]
    Copy,
    /// Make the destination match the source, deletions included.
    Sync,
    /// Two-way: changes on either side propagate to the other.
    Bisync,
    /// Move: copy, then delete the source.
    Move,
}

impl SyncMode {
    pub const ALL: [SyncMode; 4] =
        [SyncMode::Copy, SyncMode::Sync, SyncMode::Bisync, SyncMode::Move];

    pub fn verb(self) -> &'static str {
        match self {
            SyncMode::Copy => "copy",
            SyncMode::Sync => "sync",
            SyncMode::Bisync => "bisync",
            SyncMode::Move => "move",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SyncMode::Copy => "Copy (safe)",
            SyncMode::Sync => "Sync (deletes)",
            SyncMode::Bisync => "Bisync (two-way)",
            SyncMode::Move => "Move (deletes source)",
        }
    }

    /// Whether this mode can destroy data.
    pub fn destructive(self) -> bool {
        !matches!(self, SyncMode::Copy)
    }

    pub fn hint(self) -> &'static str {
        match self {
            SyncMode::Copy => {
                "Adds new files and updates changed ones. Nothing is ever deleted, so the \
                 worst a wrong path can do is copy things somewhere you did not mean."
            }
            SyncMode::Sync => {
                "Makes the destination match the source — anything at the destination that \
                 is not at the source is DELETED. Dry-run a new job before trusting it."
            }
            SyncMode::Bisync => {
                "Two-way. Changes on either side propagate to the other, deletions included. \
                 rclone needs a first run with --resync to establish a baseline."
            }
            SyncMode::Move => {
                "Copies, then DELETES the source. Useful for draining a staging area; \
                 unforgiving if the destination path is wrong."
            }
        }
    }
}

/// One sync job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncJob {
    pub name: String,
    pub enabled: bool,
    pub mode: SyncMode,
    /// rclone paths: a remote like `nas:backups`, or a local path.
    pub source: String,
    pub dest: String,
    /// Run automatically this often. Zero means manual only.
    pub interval_mins: u64,
    /// Establish a bisync baseline on the next run. rclone refuses to bisync
    /// without one, and it is cleared automatically once it succeeds.
    pub resync: bool,
    /// Skip files newer than this many seconds — avoids copying something
    /// still being written.
    pub min_age_secs: u64,
    /// Passed to rclone verbatim.
    pub extra_args: Vec<String>,
    /// Bandwidth ceiling for this job, e.g. `8M`. Empty means unlimited.
    pub bwlimit: String,
    /// Move deleted and replaced files here instead of destroying them. The
    /// single most useful safety net for a destructive mode.
    pub backup_dir: String,
}

impl Default for SyncJob {
    fn default() -> Self {
        SyncJob {
            name: String::new(),
            enabled: true,
            mode: SyncMode::Copy,
            source: String::new(),
            dest: String::new(),
            interval_mins: 0,
            resync: false,
            min_age_secs: 0,
            extra_args: Vec::new(),
            bwlimit: String::new(),
            backup_dir: String::new(),
        }
    }
}

impl SyncJob {
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.name.trim().is_empty() {
            errs.push("Name is required.".into());
        }
        if self.source.trim().is_empty() {
            errs.push("Source is required.".into());
        }
        if self.dest.trim().is_empty() {
            errs.push("Destination is required.".into());
        }
        if !self.source.trim().is_empty() && self.source.trim() == self.dest.trim() {
            errs.push("Source and destination are the same path.".into());
        }
        errs
    }
}

/// Build the rclone command line for a job.
///
/// `dry_run` adds `--dry-run`, which makes rclone report every action it would
/// take and perform none of them.
pub fn args(job: &SyncJob, dry_run: bool) -> Vec<String> {
    let mut a: Vec<String> = vec![job.mode.verb().to_string()];
    a.push(job.source.trim().to_string());
    a.push(job.dest.trim().to_string());

    if dry_run {
        a.push("--dry-run".into());
    }
    if job.mode == SyncMode::Bisync && job.resync {
        a.push("--resync".into());
    }
    if job.min_age_secs > 0 {
        a.push("--min-age".into());
        a.push(format!("{}s", job.min_age_secs));
    }
    if !job.bwlimit.trim().is_empty() {
        a.push("--bwlimit".into());
        a.push(job.bwlimit.trim().to_string());
    }
    if !job.backup_dir.trim().is_empty() {
        a.push("--backup-dir".into());
        a.push(job.backup_dir.trim().to_string());
    }

    // One progress line per second on stdout. `--progress` draws a live
    // terminal display instead, which is unparseable once stdout is a pipe.
    a.push("--stats".into());
    a.push("1s".into());
    a.push("--stats-one-line".into());
    a.push("--stats-log-level".into());
    a.push("NOTICE".into());

    a.extend(job.extra_args.iter().filter(|s| !s.trim().is_empty()).cloned());
    a
}

/// A parsed progress line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub transferred: String,
    pub total: String,
    pub percent: f32,
    pub rate: String,
    pub eta: String,
}

/// Parse one of rclone's `--stats-one-line` lines.
///
/// The shape is
/// `Transferred:   1.5 GiB / 3 GiB, 50%, 10 MiB/s, ETA 2m30s`, and rclone
/// varies it: the ETA can be `-` while it works the size out, and a job with
/// nothing to do prints `100%` with no ETA at all. Returns `None` for anything
/// that is not a stats line, since the same stream carries ordinary log output.
pub fn parse_progress(line: &str) -> Option<Progress> {
    // Two shapes reach here. `--stats-one-line` drops the "Transferred:" label
    // entirely and arrives as an ordinary log line —
    // `2026/08/31 18:22:58 NOTICE:  30 B / 30 B, 100%, 0 B/s, ETA -` — while
    // the labelled form keeps it. Strip whichever prefix is present.
    let rest = line
        .rsplit_once("Transferred:")
        .or_else(|| line.rsplit_once("NOTICE:"))
        .map(|(_, r)| r)
        .unwrap_or(line)
        .trim();

    // Everything before the first comma is `<done> / <total>`.
    let (sizes, tail) = rest.split_once(',')?;
    let (done, total) = sizes.split_once('/')?;
    if done.trim().is_empty() || total.trim().is_empty() {
        return None;
    }

    let fields: Vec<&str> = tail.split(',').map(|f| f.trim()).collect();
    // The percentage is what makes this a stats line rather than an ordinary
    // log line that happens to contain a slash and a comma — and rclone says a
    // great deal on the same stream, including file paths full of slashes.
    //
    // A job with nothing to transfer prints `-` here rather than a number,
    // which is the *common* case for a scheduled job that is already in step.
    // Rejecting it would leave those runs showing "starting…" forever.
    let percent_field = fields.first()?;
    let percent = if *percent_field == "-" {
        if done.trim() == total.trim() { 100.0 } else { 0.0 }
    } else if let Some(n) = percent_field.strip_suffix('%') {
        n.trim().parse::<f32>().ok()?
    } else {
        return None;
    };
    let rate = fields.get(1).map(|s| s.to_string()).unwrap_or_default();
    let eta = fields
        .get(2)
        .map(|s| s.trim_start_matches("ETA").trim().to_string())
        .filter(|s| !s.is_empty() && s != "-")
        .unwrap_or_default();

    Some(Progress {
        transferred: done.trim().to_string(),
        total: total.trim().to_string(),
        percent,
        rate,
        eta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> SyncJob {
        SyncJob {
            name: "photos".into(),
            source: "local:D:/photos".into(),
            dest: "offsite:photos".into(),
            ..Default::default()
        }
    }

    /// The default must be the one that cannot destroy anything. A wrong path
    /// on a first run should cost a stray copy, not a deletion.
    #[test]
    fn a_new_job_defaults_to_the_safe_mode() {
        assert_eq!(SyncJob::default().mode, SyncMode::Copy);
        assert!(!SyncMode::Copy.destructive());
        assert!(SyncMode::Sync.destructive());
        assert!(SyncMode::Bisync.destructive());
        assert!(SyncMode::Move.destructive());
    }

    #[test]
    fn a_copy_job_builds_the_expected_line() {
        let a = args(&job(), false);
        assert_eq!(a[0], "copy");
        assert_eq!(a[1], "local:D:/photos");
        assert_eq!(a[2], "offsite:photos");
        assert!(a.contains(&"--stats-one-line".to_string()));
        assert!(!a.contains(&"--dry-run".to_string()));
    }

    #[test]
    fn a_dry_run_says_so_to_rclone() {
        assert!(args(&job(), true).contains(&"--dry-run".to_string()));
    }

    /// `--resync` is only meaningful for bisync, and passing it elsewhere is an
    /// rclone error rather than a no-op.
    #[test]
    fn resync_is_only_passed_for_bisync() {
        let j = SyncJob { resync: true, ..job() };
        assert!(!args(&j, false).contains(&"--resync".to_string()));

        let j = SyncJob { mode: SyncMode::Bisync, resync: true, ..job() };
        assert!(args(&j, false).contains(&"--resync".to_string()));
    }

    #[test]
    fn the_safety_and_throttle_options_are_passed_through() {
        let j = SyncJob {
            min_age_secs: 60,
            bwlimit: "8M".into(),
            backup_dir: "offsite:trash".into(),
            ..job()
        };
        let a = args(&j, false);
        assert!(a.contains(&"--min-age".to_string()));
        assert!(a.contains(&"60s".to_string()));
        assert!(a.contains(&"--bwlimit".to_string()));
        assert!(a.contains(&"8M".to_string()));
        assert!(a.contains(&"--backup-dir".to_string()));
    }

    /// Same source and destination is not a transfer, and for a destructive
    /// mode it is a good way to lose the lot.
    #[test]
    fn validation_catches_the_dangerous_shapes() {
        assert!(job().validate().is_empty());
        assert_eq!(SyncJob { dest: String::new(), ..job() }.validate().len(), 1);
        assert_eq!(
            SyncJob { dest: "local:D:/photos".into(), ..job() }.validate().len(),
            1,
            "source and destination the same"
        );
    }

    #[test]
    fn a_normal_progress_line_parses() {
        let p = parse_progress("Transferred:   \t1.500 GiB / 3 GiB, 50%, 10.5 MiB/s, ETA 2m30s")
            .expect("should parse");
        assert_eq!(p.transferred, "1.500 GiB");
        assert_eq!(p.total, "3 GiB");
        assert_eq!(p.percent, 50.0);
        assert_eq!(p.rate, "10.5 MiB/s");
        assert_eq!(p.eta, "2m30s");
    }

    /// rclone prints `-` for the ETA until it knows the size. Showing that
    /// verbatim reads as a broken field, so it is treated as absent.
    #[test]
    fn an_unknown_eta_is_empty_rather_than_a_dash() {
        let p = parse_progress("Transferred:   1 MiB / 1 MiB, 100%, 0 B/s, ETA -").unwrap();
        assert_eq!(p.eta, "");
        assert_eq!(p.percent, 100.0);
    }

    #[test]
    fn a_line_with_no_eta_at_all_still_parses() {
        let p = parse_progress("Transferred:   0 B / 0 B, 100%").unwrap();
        assert_eq!(p.percent, 100.0);
        assert_eq!(p.eta, "");
        assert_eq!(p.rate, "");
    }

    /// The shape rclone 1.72 actually emits under `--stats-one-line`: no
    /// "Transferred:" label at all, just a timestamped NOTICE. Captured from a
    /// real run — the first version of this parser required the label and
    /// therefore never matched a single line in practice, leaving the progress
    /// column permanently blank.
    #[test]
    fn the_real_one_line_stats_format_parses() {
        let p =
            parse_progress("2026/08/31 18:22:58 NOTICE:          30 B / 30 B, 100%, 0 B/s, ETA -")
                .expect("should parse the format rclone actually writes");
        assert_eq!(p.transferred, "30 B");
        assert_eq!(p.total, "30 B");
        assert_eq!(p.percent, 100.0);
        assert_eq!(p.rate, "0 B/s");
        assert_eq!(p.eta, "");
    }

    /// A job with nothing to do prints `-` where the percentage goes. That is
    /// the *common* case for a scheduled job already in step, so rejecting it
    /// would leave those runs reading "starting…" until they finished.
    /// Captured from a real run.
    #[test]
    fn a_job_with_nothing_to_transfer_reads_as_complete() {
        let p = parse_progress("2026/08/31 18:26:19 NOTICE:           0 B / 0 B, -, 0 B/s, ETA -")
            .expect("should parse");
        assert_eq!(p.percent, 100.0, "nothing to do is done, not 0% done");
        assert_eq!(p.transferred, "0 B");

        // Mid-transfer with an unknown percentage is not complete.
        let p = parse_progress("Transferred: 1 MiB / 9 MiB, -, 1 MiB/s, ETA -").unwrap();
        assert_eq!(p.percent, 0.0);
    }

    /// The same stream carries ordinary log output, so anything that is not a
    /// stats line must be left alone rather than half-parsed into a bogus
    /// progress reading. File paths are full of slashes, which is exactly what
    /// a loose parser would trip over.
    #[test]
    fn ordinary_log_lines_are_not_progress() {
        assert_eq!(parse_progress("NOTICE: photos/a.jpg: Copied (new)"), None);
        assert_eq!(
            parse_progress("2026/08/31 18:22:58 NOTICE: a/b.jpg: Copied (new), 2 more"),
            None,
            "a slash and a comma are not enough to make it a stats line"
        );
        assert_eq!(parse_progress(""), None);
        assert_eq!(parse_progress("Transferred: nonsense"), None);
    }
}
