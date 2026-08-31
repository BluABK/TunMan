//! TunMan — an SSH tunnel manager.
//!
//! Keeps a handful of `ssh` forwards up, shows what is connected to each one
//! and how much is going through it, and lives in the tray.

// A console window would flash up on every launch in release; keep it in debug
// where the log on stderr is the whole point.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_paths;
mod config;
mod geo;
/// The app icon as pixels. Kept dependency-free and free of inner doc
/// comments because `build.rs` includes this same source to render the `.ico`
/// it embeds in the exe — `include!` rejects `//!`.
mod icon_art;
mod jobs;
mod log_capture;
mod logfmt;
mod meter;
mod model;
mod mounts;
mod platform;
mod sa_push;
mod sampler;
mod ssh;
mod stale;
mod supervisor;
mod sync;
mod traffic;
mod ui;
mod usage;
mod util;

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use anyhow::Result;
use tracing::{debug, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use ui::UiCommand;

fn main() -> Result<()> {
    // ssh runs us as its askpass helper. This must come before anything else:
    // it is a different program that happens to share an executable, and it
    // must not touch the log, the config or the single-instance lock.
    if std::env::var_os("TUNMAN_ASKPASS").is_some() || std::env::args().any(|a| a == "--askpass") {
        if let Ok(pw) = std::env::var("TUNMAN_PASSWORD") {
            println!("{pw}");
        }
        return Ok(());
    }

    let _tracing_guard = init_tracing();
    info!(
        "TunMan v{} ({}) build {}",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_HASH"),
        env!("BUILD_NUMBER")
    );

    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiCommand>();

    // Bound to a named variable: `let _ =` would drop it here and release the
    // lock immediately.
    let doorbell = ui_tx.clone();
    let _instance = match platform::acquire_single_instance(move || {
        let _ = doorbell.send(UiCommand::ShowWindow);
    }) {
        Some(g) => g,
        None => {
            info!("another TunMan is already running; showing its window");
            platform::notify_running_instance();
            return Ok(());
        }
    };

    let config_path = app_paths::config_path();
    // A config that has vanished while a backup sits next to it is worth
    // saying out loud rather than silently starting empty — that is exactly
    // what an accidental deletion looks like from in here.
    let restore_offer = (!config_path.exists() && app_paths::config_backup_path().exists())
        .then(app_paths::config_backup_path);
    if let Some(bak) = &restore_offer {
        warn!("no config at {} — but a backup exists at {}", config_path.display(), bak.display());
    }
    let (cfg, load_error) = match config::Config::load(&config_path) {
        Ok(c) => (c, None),
        Err(e) => {
            // Deliberately keep running with defaults rather than exiting, but
            // remember the error so saving is refused — overwriting a file
            // someone is part-way through hand-editing would be the worst
            // possible response to a typo.
            warn!(error = %e, "could not read the config");
            (config::Config::default(), Some(e.to_string()))
        }
    };
    app_paths::ensure_dir(&app_paths::data_dir());
    prune_old_logs(&app_paths::logs_dir(), cfg.settings.log_retention_days);

    // Refreshed on every run rather than created once: TunMan is launched from
    // wherever it was built, and a shortcut left pointing at a moved binary
    // fails silently from the Start Menu while the app itself works fine.
    // Done here, before the GUI, so it cannot race winit's own COM setup.
    if cfg.settings.start_menu_shortcut {
        match platform::create_start_menu_shortcut() {
            Ok(p) => debug!("Start Menu shortcut at {}", p.display()),
            Err(e) => warn!("could not write the Start Menu shortcut: {e}"),
        }
    }

    let start_hidden =
        cfg.settings.start_hidden || std::env::args().any(|a| a == "--hidden" || a == "-Embedding");

    let shared = Arc::new(supervisor::Shared::default());
    let mount_shared = Arc::new(jobs::MountShared::default());
    let sync_shared = Arc::new(jobs::SyncShared::default());
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<supervisor::Command>(64);
    let (mount_tx, mount_rx) = tokio::sync::mpsc::channel::<jobs::MountCommand>(64);
    let (sync_tx, sync_rx) = tokio::sync::mpsc::channel::<jobs::SyncCommand>(64);

    // The tokio runtime lives on its own thread; eframe owns the main one.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    {
        let shared = shared.clone();
        let cfg = cfg.clone();
        rt.spawn(async move { supervisor::run(cfg, shared, cmd_rx).await });
    }
    {
        let shared = mount_shared.clone();
        let cfg = cfg.clone();
        rt.spawn(async move { jobs::run_mounts(cfg, shared, mount_rx).await });
    }
    {
        let shared = sync_shared.clone();
        let cfg = cfg.clone();
        rt.spawn(async move { jobs::run_sync(cfg, shared, sync_rx).await });
    }
    sampler::start(shared.clone(), cmd_tx.clone());

    let opts = eframe::NativeOptions {
        persistence_path: Some(app_paths::data_dir().join("window.ron")),
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("TunMan v{} ({})", env!("CARGO_PKG_VERSION"), env!("GIT_HASH")))
            .with_inner_size([1280.0, 680.0])
            .with_min_inner_size([720.0, 420.0])
            // Always invisible at creation. The window is revealed a few frames
            // in, once there is something painted behind it — otherwise the
            // first thing on screen is an unpainted white rectangle.
            .with_visible(false)
            .with_icon(icon_data()),
        ..Default::default()
    };

    let result = eframe::run_native(
        "TunMan",
        opts,
        Box::new(move |cc| {
            // Before anything is drawn: several of the symbols this UI uses have
            // no glyph in egui's default proportional fonts.
            ui::fonts::install(&cc.egui_ctx);
            let (tray, tray_rx) = build_tray(cc.egui_ctx.clone())?;
            // Fold tray events into the same channel the doorbell uses.
            std::thread::Builder::new()
                .name("tray-forward".into())
                .spawn(move || {
                    while let Ok(cmd) = tray_rx.recv() {
                        if ui_tx.send(cmd).is_err() {
                            break;
                        }
                    }
                })
                .ok();
            Ok(Box::new(ui::TunManApp::new(
                shared,
                mount_shared,
                sync_shared,
                cfg,
                cmd_tx,
                mount_tx,
                sync_tx,
                tray,
                ui_rx,
                start_hidden,
                load_error,
                restore_offer,
            )))
        }),
    );

    // The last minute of usage is only in memory; without this a cap would
    // quietly forget it across every restart.
    sampler::flush_usage();

    // Dropping the runtime drops every child with it — they are spawned
    // kill_on_drop, so nothing is left holding a port.
    drop(rt);
    result.map_err(|e| anyhow::anyhow!("{e}"))
}

fn icon_data() -> egui::IconData {
    let (rgba, width, height) = platform::app_icon_rgba();
    egui::IconData { rgba, width, height }
}

/// Build the tray icon and a channel carrying its menu clicks.
///
/// The forwarding thread ends every event with `request_repaint`: that is what
/// wakes the reactive event loop while the window is hidden, and without it a
/// tray click does nothing until something else happens to cause a frame.
fn build_tray(ctx: egui::Context) -> Result<(TrayIcon, Receiver<UiCommand>)> {
    let menu = Menu::new();
    let open = MenuItem::new("Open TunMan", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&open)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit)?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("TunMan — SSH tunnels")
        .with_icon(platform::tray_icon_image()?)
        .build()?;

    let (tx, rx): (Sender<UiCommand>, Receiver<UiCommand>) = std::sync::mpsc::channel();
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
    let menu_rx = MenuEvent::receiver().clone();
    std::thread::Builder::new().name("tray-events".into()).spawn(move || {
        while let Ok(event) = menu_rx.recv() {
            let cmd = if event.id == open_id {
                UiCommand::ShowWindow
            } else if event.id == quit_id {
                UiCommand::Quit
            } else {
                continue;
            };
            if tx.send(cmd).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    })?;

    // The menu items own the ids the thread above compares against, so they
    // have to outlive this function.
    std::mem::forget(open);
    std::mem::forget(quit);
    Ok((tray, rx))
}

fn init_tracing() -> tracing_appender::non_blocking::WorkerGuard {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,TunMan=debug"));

    let log_dir = app_paths::logs_dir();
    app_paths::ensure_dir(&log_dir);

    let (non_blocking, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(&log_dir, "TunMan.log"));

    // `with_ansi_sanitization(false)` is required: tracing-subscriber otherwise
    // escapes the colour codes in message text and prints them as literal
    // `\x1b[38;2;...` garbage.
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_ansi_sanitization(false)
        .with_target(false)
        .with_writer(logfmt::StripAnsiMake(non_blocking));

    let stderr_layer =
        fmt::layer().with_ansi_sanitization(false).with_target(false).with_writer(std::io::stderr);

    // All three sinks under one filter, so "what the Log tab can show" always
    // equals "what the file holds".
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .with(log_capture::LogCaptureLayer)
        .init();
    guard
}

/// Delete log files older than `keep_days`.
///
/// The name check is `.contains(".log.")`, not an extension test: a daily-rolled
/// file is `TunMan.log.2026-08-31`, so `Path::extension()` returns the *date*
/// and an extension-only match never fires — which is how a log directory grows
/// to gigabytes while looking like it has retention.
fn prune_old_logs(dir: &std::path::Path, keep_days: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let Some(cutoff) = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(keep_days.max(1) * 86400))
    else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name.ends_with(".log") || name.contains(".log.")) {
            continue;
        }
        let old = entry.metadata().and_then(|m| m.modified()).map(|m| m < cutoff).unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The trap this exists to avoid: `TunMan.log.2026-08-31` has an
    /// *extension* of `2026-08-31`, so any extension-based check silently skips
    /// every rolled file and retention never happens.
    #[test]
    fn pruning_matches_rolled_names_and_spares_everything_else() {
        let dir = std::env::temp_dir().join("TunMan-prune-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        for name in ["TunMan.log", "TunMan.log.2020-01-01", "TunMan.toml", "notes.txt"] {
            let mut f = std::fs::File::create(dir.join(name)).unwrap();
            writeln!(f, "x").unwrap();
        }
        // Everything was just written, so nothing is older than the cutoff.
        prune_old_logs(&dir, 7);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 4, "fresh files are kept");

        // Zero days is clamped to one, so this must not reach today's files —
        // and must never touch anything that is not a log.
        prune_old_logs(&dir, 0);
        assert!(dir.join("TunMan.toml").exists(), "pruning must never touch the config");
        assert!(dir.join("notes.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
