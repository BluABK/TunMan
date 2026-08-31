//! Where TunMan keeps its things: one config file and a log directory, both
//! under `%APPDATA%\TunMan`.
//!
//! Deliberately a *file* layout rather than a database. There are a handful of
//! tunnels, they are edited by hand as often as through the UI, and a TOML file
//! can be diffed, backed up and copied to another machine without tooling.

use std::path::{Path, PathBuf};

/// `%APPDATA%\TunMan` (`~/.local/share/TunMan` elsewhere), falling back to a
/// directory beside the executable if the OS won't tell us.
pub fn data_dir() -> PathBuf {
    // Not ProjectDirs::data_dir() on Windows: it appends a further "data"
    // component, burying the config one level deeper than anyone would look
    // for it. For a single hand-editable file that is a cost with no benefit.
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("TunMan");
    }
    directories::ProjectDirs::from("", "", "TunMan")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("TunMan-data"))
}

pub fn config_path() -> PathBuf {
    data_dir().join("TunMan.toml")
}

/// The bandwidth ledger. JSON rather than TOML: it is machine-written,
/// rewritten often, and nobody hand-edits a table of hourly byte counts.
pub fn usage_path() -> PathBuf {
    data_dir().join("usage.json")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// Create `dir` if missing, ignoring the error — every caller has a sensible
/// fallback (the write that follows will report the real problem with a path
/// attached, which is a far better message than one from here).
pub fn ensure_dir(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
}
