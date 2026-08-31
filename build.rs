use std::process::Command;

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
}
