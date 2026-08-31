//! Mount points left behind when a mount dies badly.
//!
//! A mount that ends cleanly takes its mount point with it. One that is killed,
//! crashes, or is cut off by a BSOD does not: the drive letter stays registered
//! with nothing behind it, or the directory it was mounted at is left sitting
//! there. Either one makes the *next* mount fail — rclone refuses to mount onto
//! a path that already exists, and a drive letter that is still claimed cannot
//! be claimed again. The mount then retries forever against a mount point that
//! will never come free on its own, which looks exactly like a server problem
//! and is not one.
//!
//! So before every attempt the mount point is examined and, if it is a leftover,
//! cleared. The whole risk here is clearing something that is *not* a leftover,
//! so the rules are deliberately narrow:
//!
//! - A drive letter is only cleared when it does not answer **and** the device
//!   behind it is a WinFsp mount or a network mapping. A letter pointing at a
//!   real volume is never touched, answering or not — a failing disk must not
//!   have its letter taken away by a tunnel manager.
//! - A directory is only ever removed with `remove_dir`, which refuses to
//!   delete anything that has contents. There is no path through this module
//!   that can delete a file.
//!
//! Every probe is time-boxed. A dead mount point does not fail when read — it
//! *hangs*, sometimes for the full network timeout, and a supervisor that waits
//! on one is a supervisor that has stopped supervising.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use parking_lot::Mutex;

/// How long to wait for a mount point to answer before calling it dead and
/// clearing it. Deliberately the more patient of the two: this decision leads
/// to removing something.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// How long to wait when asking whether a mount is still answering. Shorter,
/// because this runs on a timer and only decides whether to remount.
pub const LIVENESS_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether a mount point answers within `timeout`.
///
/// The real test of a mount, and why it is time-boxed: a mount that has gone
/// stale does not report an error when read, it stops answering. An unbounded
/// read against one blocks its caller for as long as the filesystem driver
/// takes to give up — minutes, in the case that matters — and a supervisor
/// waiting on that has stopped watching the thing it supervises.
pub fn responds(target: &str, timeout: Duration) -> bool {
    let path = match classify(target) {
        // A drive letter needs the trailing separator: `X:` alone means "the
        // current directory on X:", which can answer when the drive does not.
        Some(Point::Drive(letter)) => PathBuf::from(format!("{letter}:\\")),
        Some(Point::Dir(p)) => p,
        None => return false,
    };
    matches!(entries_within(path, timeout), Some(Ok(_)))
}

/// Where a mount appears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Point {
    /// A drive letter, e.g. `X:`.
    Drive(char),
    /// A directory.
    Dir(PathBuf),
}

/// What kind of mount point a target string names.
pub fn classify(target: &str) -> Option<Point> {
    let t = target.trim();
    if t.is_empty() {
        return None;
    }
    // `X:`, `X:\` and `X:/` are all the same drive; anything longer is a path.
    let bare = t.trim_end_matches(['/', '\\']);
    let b = bare.as_bytes();
    if bare.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return Some(Point::Drive((b[0] as char).to_ascii_uppercase()));
    }
    Some(Point::Dir(PathBuf::from(t)))
}

/// What is at a mount point right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing there. A mount can be made.
    Free,
    /// In the way, and safe to clear: a registered drive letter with nothing
    /// behind it, or an empty directory left over from a previous mount.
    Stale,
    /// In the way, and **not** ours to remove: a working mount, a real disk, or
    /// a directory with contents.
    Occupied,
}

/// The reading of a mount point, with a sentence explaining it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub state: State,
    pub detail: String,
}

impl Reading {
    fn new(state: State, detail: impl Into<String>) -> Reading {
        Reading { state, detail: detail.into() }
    }
}

/// Whether an NT device path behind a drive letter is one this app may unclaim.
///
/// Pure, and the single safety rule for drive letters: WinFsp devices (what
/// rclone and sshfs mount through) and network redirector mappings are ours to
/// clear; a real volume never is. Anything unrecognised is left alone —
/// guessing wrong here takes a letter away from a disk.
pub fn is_clearable_device(nt_path: &str) -> bool {
    let p = nt_path.to_ascii_lowercase();
    if p.contains("harddiskvolume")
        || p.contains("cdrom")
        || p.contains("floppy")
        || p.contains("harddisk")
    {
        return false;
    }
    p.contains("winfsp") || p.contains("lanmanredirector") || p.contains("mup") || p.contains("unc")
}

/// Whether a tool's own error says the mount point was in the way.
///
/// Used to explain a failure in the terms that matter — "the mount point was
/// still claimed", not "exit code 1" — and to know a clear is worth trying
/// before the next attempt.
pub fn looks_occupied(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    [
        "mountpoint path already exists",
        "mountpoint already exists",
        "directory already exists",
        "cannot create mountpoint",
        "mountpoint is not empty",
        "is already in use",
        "already in use",
        "file exists",
        "object name collision",
        "the directory is not empty",
        "cannot mount: mountpoint",
    ]
    .iter()
    .any(|s| e.contains(s))
}

/// Mount points with a probe still running.
///
/// An abandoned probe is still sitting in `read_dir` on a filesystem that is
/// not answering; starting another one against the same path adds a thread and
/// learns nothing. So a second probe does not start — it takes the outstanding
/// one's silence as the answer, which is what silence means here.
static PROBING: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Holds a path in [`PROBING`] for as long as its probe runs, including after
/// the probe has been given up on — the thread is what has to finish, not the
/// wait for it.
struct ProbeSlot(PathBuf);

impl ProbeSlot {
    /// Claim `path`, or `None` if a probe of it is already running.
    fn claim(path: &Path) -> Option<ProbeSlot> {
        PROBING.lock().insert(path.to_path_buf()).then(|| ProbeSlot(path.to_path_buf()))
    }
}

impl Drop for ProbeSlot {
    fn drop(&mut self) {
        PROBING.lock().remove(&self.0);
    }
}

/// Read a directory, giving up after `timeout`.
///
/// On its own thread because there is no interruptible directory read: a dead
/// mount point blocks the caller until the filesystem driver gives up, which
/// can be minutes. The thread is abandoned rather than joined — nothing waits
/// on it — but it keeps its claim until it actually returns, so a mount point
/// that is wedged collects one stuck thread rather than one per check.
///
/// `None` means "did not answer", whether that is because the read timed out or
/// because an earlier read of the same path has not come back yet.
fn entries_within(path: PathBuf, timeout: Duration) -> Option<std::io::Result<usize>> {
    let slot = ProbeSlot::claim(&path)?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("mountpoint-probe".into())
        .spawn(move || {
            let r = std::fs::read_dir(&path).map(|it| it.take(1).count());
            let _ = tx.send(r);
            // Released here, not when the wait below gives up: until this
            // returns, the path is still being read.
            drop(slot);
        })
        .ok()?;
    rx.recv_timeout(timeout).ok()
}

#[cfg(windows)]
mod win {
    use windows::Win32::Storage::FileSystem::{
        DDD_REMOVE_DEFINITION, DefineDosDeviceW, GetLogicalDrives, QueryDosDeviceW,
    };
    use windows::core::{HSTRING, PCWSTR};

    /// Whether the letter is claimed at all.
    pub fn letter_in_use(letter: char) -> bool {
        let bit = (letter.to_ascii_uppercase() as u32).wrapping_sub('A' as u32);
        bit < 26 && unsafe { GetLogicalDrives() } & (1 << bit) != 0
    }

    /// The NT device path a drive letter points at, e.g.
    /// `\Device\HarddiskVolume4` or `\Device\WinFsp.Disk`.
    pub fn device_target(letter: char) -> String {
        let name = HSTRING::from(format!("{}:", letter.to_ascii_uppercase()));
        let mut buf = vec![0u16; 1024];
        let n = unsafe { QueryDosDeviceW(PCWSTR(name.as_ptr()), Some(&mut buf)) };
        if n == 0 {
            return String::new();
        }
        // The result is a NUL-separated list; the first entry is the current
        // definition and the rest are ones it shadows.
        let end = buf.iter().position(|c| *c == 0).unwrap_or(0);
        String::from_utf16_lossy(&buf[..end])
    }

    /// Drop every DOS device definition for the letter.
    pub fn remove_letter(letter: char) -> Result<(), String> {
        let name = HSTRING::from(format!("{}:", letter.to_ascii_uppercase()));
        unsafe { DefineDosDeviceW(DDD_REMOVE_DEFINITION, PCWSTR(name.as_ptr()), PCWSTR::null()) }
            .map_err(|e| format!("{e}"))
    }

    /// Drop a mapped network drive. WinFsp's network mode registers one, and it
    /// survives the process that made it.
    pub fn disconnect_network_drive(letter: char) -> bool {
        let mut cmd = std::process::Command::new("net");
        cmd.args(["use", &format!("{}:", letter.to_ascii_uppercase()), "/delete", "/y"]);
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.output().map(|o| o.status.success()).unwrap_or(false)
    }
}

#[cfg(not(windows))]
mod win {
    pub fn letter_in_use(_letter: char) -> bool {
        false
    }
    pub fn device_target(_letter: char) -> String {
        String::new()
    }
    pub fn remove_letter(_letter: char) -> Result<(), String> {
        Err("drive letters are Windows-only".into())
    }
    pub fn disconnect_network_drive(_letter: char) -> bool {
        false
    }
}

/// Examine a mount point. Blocking, and bounded by `timeout`.
pub fn probe(point: &Point, timeout: Duration) -> Reading {
    match point {
        Point::Drive(letter) => {
            if !win::letter_in_use(*letter) {
                return Reading::new(State::Free, format!("{letter}: is free"));
            }
            let device = win::device_target(*letter);
            match entries_within(PathBuf::from(format!("{letter}:\\")), timeout) {
                Some(Ok(_)) => Reading::new(
                    State::Occupied,
                    format!("{letter}: is answering — something is mounted there"),
                ),
                other => {
                    // Not answering. Whether it may be unclaimed depends
                    // entirely on what is behind it.
                    let why = match other {
                        None => "did not answer in time",
                        Some(Err(_)) => "could not be read",
                        Some(Ok(_)) => unreachable!(),
                    };
                    if is_clearable_device(&device) {
                        Reading::new(
                            State::Stale,
                            format!("{letter}: {why} and is a leftover {}", describe(&device)),
                        )
                    } else {
                        Reading::new(
                            State::Occupied,
                            format!(
                                "{letter}: {why}, but it points at {} — not ours to unclaim",
                                describe(&device)
                            ),
                        )
                    }
                }
            }
        }
        Point::Dir(path) => {
            // symlink_metadata rather than exists(): it does not follow the
            // reparse point a dead mount leaves behind, so it answers instantly
            // instead of hanging on a filesystem that is gone.
            match std::fs::symlink_metadata(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Reading::new(State::Free, format!("{} does not exist", path.display()))
                }
                _ => match entries_within(path.clone(), timeout) {
                    Some(Ok(0)) => Reading::new(
                        State::Stale,
                        format!("{} is an empty directory in the way", path.display()),
                    ),
                    Some(Ok(_)) => Reading::new(
                        State::Occupied,
                        format!("{} has contents — leaving it alone", path.display()),
                    ),
                    None | Some(Err(_)) => Reading::new(
                        State::Stale,
                        format!("{} does not answer — a dead mount point", path.display()),
                    ),
                },
            }
        }
    }
}

/// A device path in words.
fn describe(device: &str) -> String {
    let d = device.to_ascii_lowercase();
    if d.is_empty() {
        "an unnamed device".into()
    } else if d.contains("winfsp") {
        format!("WinFsp mount ({device})")
    } else if d.contains("lanmanredirector") || d.contains("mup") || d.contains("unc") {
        format!("network mapping ({device})")
    } else if d.contains("harddiskvolume") || d.contains("harddisk") {
        format!("a real volume ({device})")
    } else {
        format!("an unrecognised device ({device})")
    }
}

/// Clear a stale mount point. Returns what was done.
///
/// Only ever called for [`State::Stale`]; it re-checks anyway, because the
/// decision and the action are separated by a probe that takes seconds and the
/// world can change in between.
pub fn clear(point: &Point) -> Result<String, String> {
    match point {
        Point::Drive(letter) => {
            if !win::letter_in_use(*letter) {
                return Ok(format!("{letter}: was already free"));
            }
            let device = win::device_target(*letter);
            if !is_clearable_device(&device) {
                return Err(format!(
                    "{letter}: points at {} — refusing to unclaim it",
                    describe(&device)
                ));
            }
            let d = device.to_ascii_lowercase();
            let mut how = Vec::new();
            if (d.contains("lanmanredirector") || d.contains("mup") || d.contains("unc"))
                && win::disconnect_network_drive(*letter)
            {
                how.push("disconnected the network mapping");
            }
            match win::remove_letter(*letter) {
                Ok(()) => how.push("removed the drive letter"),
                Err(e) if how.is_empty() => return Err(format!("{letter}: {e}")),
                Err(_) => {}
            }
            if win::letter_in_use(*letter) {
                return Err(format!("{letter}: is still claimed after {}", how.join(" and ")));
            }
            Ok(format!("{letter}: {}", how.join(" and ")))
        }
        Point::Dir(path) => {
            // remove_dir, never remove_dir_all: it fails on a directory with
            // contents rather than deleting them. That is the whole safety
            // argument for this branch, and it must stay this way.
            match std::fs::remove_dir(path) {
                Ok(()) => Ok(format!("removed the leftover directory {}", path.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok(format!("{} was already gone", path.display()))
                }
                Err(e) => Err(format!("could not remove {}: {e}", path.display())),
            }
        }
    }
}

/// Probe a mount point and clear it if it is a leftover, in one blocking call.
///
/// Returns a sentence to log when something was actually done.
pub fn clear_if_stale(target: &str) -> Option<String> {
    let point = classify(target)?;
    let reading = probe(&point, PROBE_TIMEOUT);
    if reading.state != State::Stale {
        return None;
    }
    match clear(&point) {
        Ok(done) => Some(format!("{} — {done}", reading.detail)),
        Err(e) => Some(format!("{} — but {e}", reading.detail)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bound on stuck threads. A probe that never returns must not be
    /// joined by a second one: the point of the timeout is to stop waiting, and
    /// if every check spawned another reader, a wedged mount point would
    /// accumulate one thread per check for as long as it stayed wedged.
    #[test]
    fn only_one_probe_of_a_path_runs_at_a_time() {
        let path = std::env::temp_dir().join("TunMan-probe-slot");
        let first = ProbeSlot::claim(&path).expect("first claim");
        assert!(ProbeSlot::claim(&path).is_none(), "a second probe must not start");
        // A different path is unaffected — one stuck mount must not stop the
        // others being checked.
        let other = ProbeSlot::claim(&path.join("elsewhere")).expect("a different path");

        drop(first);
        assert!(ProbeSlot::claim(&path).is_some(), "released once the probe finishes");
        drop(other);
    }

    /// And the reading a caller gets while a probe is outstanding: not
    /// answering. Anything else would report a wedged mount point as healthy.
    #[test]
    fn a_path_already_being_probed_reads_as_no_answer() {
        let dir = std::env::temp_dir().join("TunMan-probe-busy");
        let _ = std::fs::create_dir_all(&dir);
        let held = ProbeSlot::claim(&dir).expect("claim");
        assert!(entries_within(dir.clone(), Duration::from_millis(50)).is_none());
        drop(held);
        // Released, so the same path answers normally again.
        assert!(matches!(entries_within(dir.clone(), PROBE_TIMEOUT), Some(Ok(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drive_letter_is_recognised_however_it_is_written() {
        for t in ["X:", "x:", "X:\\", "X:/", " X: "] {
            assert_eq!(classify(t), Some(Point::Drive('X')), "for {t:?}");
        }
    }

    #[test]
    fn a_path_is_a_directory_not_a_drive() {
        assert_eq!(classify(r"C:\mnt\backups"), Some(Point::Dir(PathBuf::from(r"C:\mnt\backups"))));
        assert_eq!(classify("/mnt/backups"), Some(Point::Dir(PathBuf::from("/mnt/backups"))));
        assert_eq!(classify("   "), None);
        assert_eq!(classify(""), None);
    }

    /// The safety rule. Getting this wrong means unassigning the letter of a
    /// real disk, so it is stated as a test rather than left to the reader.
    #[test]
    fn only_mount_devices_may_be_unclaimed() {
        for ours in [
            r"\Device\WinFsp.Disk",
            r"\Device\WinFsp.Net\rclone\backups",
            r"\Device\LanmanRedirector\;X:0\server\share",
            r"\??\UNC\server\share",
        ] {
            assert!(is_clearable_device(ours), "should be clearable: {ours}");
        }
        for theirs in [
            r"\Device\HarddiskVolume4",
            r"\Device\Harddisk0\Partition1",
            r"\Device\CdRom0",
            r"\Device\Floppy0",
            "",
            r"\Device\SomethingNobodyHasSeen",
        ] {
            assert!(!is_clearable_device(theirs), "must NOT be clearable: {theirs}");
        }
    }

    /// A device that is both — a WinFsp name on a hard disk device — must fall
    /// on the safe side.
    #[test]
    fn an_ambiguous_device_is_left_alone() {
        assert!(!is_clearable_device(r"\Device\HarddiskVolume7\WinFsp"));
    }

    #[test]
    fn the_tools_own_words_for_an_occupied_mount_point_are_recognised() {
        for e in [
            "mount helper error: fusermount: mountpoint path already exists: X:",
            "Fatal error: failed to mount FUSE fs: mountpoint path already exists",
            "cannot create mountpoint: file exists",
            "mountpoint is not empty",
            "Drive letter X: is already in use",
            "The directory is not empty",
        ] {
            assert!(looks_occupied(e), "should be recognised: {e}");
        }
        for e in [
            "Failed to create file system: connection refused",
            "NOTICE: rclone: version 1.72.1",
            "permission denied",
        ] {
            assert!(!looks_occupied(e), "should NOT be recognised: {e}");
        }
    }

    /// An empty directory is in the way of an rclone mount and is safe to
    /// remove; one with anything in it is neither.
    #[test]
    fn a_directory_with_contents_is_never_stale() {
        let base = std::env::temp_dir().join("TunMan-stale-test");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let empty = base.join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(probe(&Point::Dir(empty.clone()), PROBE_TIMEOUT).state, State::Stale);

        let full = base.join("full");
        std::fs::create_dir(&full).unwrap();
        std::fs::write(full.join("a.txt"), b"data").unwrap();
        assert_eq!(probe(&Point::Dir(full.clone()), PROBE_TIMEOUT).state, State::Occupied);

        assert_eq!(probe(&Point::Dir(base.join("missing")), PROBE_TIMEOUT).state, State::Free);

        // And clearing must take the empty one and refuse the other, without
        // touching the file.
        assert!(clear(&Point::Dir(empty.clone())).is_ok());
        assert!(!empty.exists());
        assert!(clear(&Point::Dir(full.clone())).is_err());
        assert!(full.join("a.txt").exists(), "clearing must never delete data");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn clearing_a_mount_point_that_is_not_stale_does_nothing() {
        let base = std::env::temp_dir().join("TunMan-stale-noop");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("keep.txt"), b"x").unwrap();

        assert_eq!(clear_if_stale(&base.to_string_lossy()), None);
        assert!(base.join("keep.txt").exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
