use std::process::Command;

// The same drawing the app uses for its window and tray icon. Included rather
// than imported: a build script is its own crate and cannot reach into the one
// it is building, and drawing the icon twice would guarantee the exe's icon
// and the window's icon eventually disagreed.
#[cfg(windows)]
mod icon_art {
    include!("src/icon_art.rs");
}

fn main() {
    // Short commit hash, shown in the window title so a bug report identifies
    // the exact build. "unknown" when built outside a checkout.
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_HASH={hash}");

    // Commit count as a build number: monotonic, needs no state file, and
    // survives a fresh clone.
    let builds = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0".to_string());
    println!("cargo:rustc-env=BUILD_NUMBER={builds}");

    // `.git/HEAD` alone is not enough: on a normal commit it keeps the same
    // contents ("ref: refs/heads/master") and only the ref file underneath it
    // moves, so watching HEAD by itself leaves the version string stuck at
    // whatever it was when the crate was first built. Watching the refs
    // directory catches commits; HEAD still catches branch switches.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    embed_icon();
}

/// Put the icon *inside the executable*.
///
/// Windows reads a program's icon out of its resources for everything that is
/// not the running window: the Start Menu entry, an Explorer listing, and —
/// when a program is launched from a shortcut — the taskbar button too. An exe
/// with no icon resource gets the generic one, which is why TunMan and
/// StreamArchiver appeared in the Start Menu as identical blank pages while
/// their windows showed the right icons.
///
/// A missing resource compiler is a warning rather than an error: it costs the
/// icon, not the build.
#[cfg(windows)]
fn embed_icon() {
    println!("cargo:rerun-if-changed=src/icon_art.rs");

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico = out.join("TunMan.ico");
    if let Err(e) = std::fs::write(&ico, icon_art::ico(icon_art::SIZES)) {
        println!("cargo:warning=could not write {}: {e}", ico.display());
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon(&ico.to_string_lossy());
    res.set("FileDescription", "TunMan - SSH tunnel manager");
    res.set("ProductName", "TunMan");
    if let Err(e) = res.compile() {
        println!("cargo:warning=icon not embedded (no resource compiler?): {e}");
    }
}
