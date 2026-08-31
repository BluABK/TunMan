//! The window: three tabs, a tray icon, and the rule that closing hides.

pub mod dialogs;
pub mod log_view;
pub mod traffic;
pub mod tunnels;

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc::Sender;
use tracing::{info, warn};
use tray_icon::TrayIcon;

use crate::config::Config;
use crate::supervisor::{Command, Shared, Status, TunnelState};

/// Out-of-band requests reaching the UI from the tray thread or a second
/// launch. Routed through a channel rather than touching the app directly,
/// because they arrive on other threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiCommand {
    ShowWindow,
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Tunnels,
    Traffic,
    Log,
}

pub struct TunManApp {
    pub shared: Arc<Shared>,
    pub cfg: Config,
    pub cmd_tx: Sender<Command>,
    /// Dropping this removes the icon from the tray, so it has to be kept for
    /// the life of the app even though nothing reads it.
    _tray: TrayIcon,
    ui_rx: Receiver<UiCommand>,

    pub tab: Tab,
    /// Set when a real quit is under way, so the close interception below
    /// stands aside instead of cancelling it.
    quitting: bool,
    /// The window is created invisible and revealed a few frames in, once
    /// there is something painted to show.
    startup_revealed: bool,
    frames: u32,
    start_hidden: bool,

    /// Row the detail panel is following.
    pub selected: Option<String>,
    /// Cached snapshot, refreshed about once a second rather than per frame.
    pub rows: Vec<TunnelState>,
    refreshed: Option<Instant>,
    pub history: Vec<crate::sampler::Sample>,

    pub editor: Option<dialogs::EditState>,
    pub settings_open: bool,
    pub log: log_view::LogViewState,
    /// Transient message shown in the action row (copied, exported, saved).
    pub toast: Option<(String, Instant)>,
    /// A config we could not parse. Held so saving is refused rather than
    /// overwriting whatever the user was in the middle of hand-editing.
    pub load_error: Option<String>,
}

impl TunManApp {
    pub fn new(
        shared: Arc<Shared>,
        cfg: Config,
        cmd_tx: Sender<Command>,
        tray: TrayIcon,
        ui_rx: Receiver<UiCommand>,
        start_hidden: bool,
        load_error: Option<String>,
    ) -> TunManApp {
        TunManApp {
            shared,
            cfg,
            cmd_tx,
            _tray: tray,
            ui_rx,
            tab: Tab::Tunnels,
            quitting: false,
            startup_revealed: false,
            frames: 0,
            start_hidden,
            selected: None,
            rows: Vec::new(),
            refreshed: None,
            history: Vec::new(),
            editor: None,
            settings_open: false,
            log: log_view::LogViewState::default(),
            toast: None,
            load_error,
        }
    }

    /// Send a command to the supervisor. Never blocks the render thread: a full
    /// queue means the supervisor is busy, and a dropped duplicate click is a
    /// better outcome than a frozen window.
    pub fn send(&self, cmd: Command) {
        if self.cmd_tx.try_send(cmd).is_err() {
            warn!("supervisor is busy; command dropped");
        }
    }

    pub fn note(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Persist the config and tell the supervisor what changed.
    pub fn save_config(&mut self) {
        if let Some(err) = &self.load_error {
            self.note(format!("Not saving — {err} still needs fixing by hand"));
            return;
        }
        match self.cfg.save(&crate::app_paths::config_path()) {
            Ok(()) => {
                self.send(Command::Reload(Box::new(self.cfg.clone())));
                self.note("Saved");
            }
            Err(e) => {
                warn!(error = %e, "could not save config");
                self.note(format!("Could not save: {e}"));
            }
        }
    }

    /// Refresh the cached snapshot at about 1 Hz, matching the sampler.
    fn refresh(&mut self, ctx: &egui::Context) {
        let stale = self.refreshed.map(|t| t.elapsed().as_millis() >= 900).unwrap_or(true);
        // Swapping the model out from under a selection cancels it, so a live
        // table cannot be copied unless refreshes pause while text is held.
        if stale && !text_selection_hold(ctx) {
            self.rows = self.shared.snapshot(&self.cfg.tunnels);
            self.history = crate::sampler::history();
            self.refreshed = Some(Instant::now());
        }
    }

    fn pump_messages(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.ui_rx.try_recv() {
            match cmd {
                UiCommand::ShowWindow => raise_window(ctx),
                UiCommand::Quit => self.request_quit(ctx),
            }
        }
    }

    fn request_quit(&mut self, ctx: &egui::Context) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        info!("shutting down; stopping tunnels");
        self.send(Command::Shutdown);
        // Give the supervisor a moment to kill its children. The processes are
        // spawned kill_on_drop, so worst case they die with us anyway.
        std::thread::sleep(Duration::from_millis(250));
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

/// Bring the window back from the tray.
///
/// All three commands, in this order. `Visible` alone leaves a genuinely
/// minimised window minimised — iconic and hidden are different states — and
/// without `Focus` it comes back behind whatever the user is looking at.
fn raise_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

/// Whether live refreshes should pause because the user is selecting text.
///
/// egui drops a label selection whenever the model behind it changes, so a
/// table that reloads every second cannot be copied from. The hold is **capped**
/// rather than open-ended: a selection can outlive the user's interest, and an
/// abandoned one must not freeze every readout in the app. The cap restarts
/// while the selection is actively being dragged, so a slow deliberate
/// selection is never cut short.
pub fn text_selection_hold(ctx: &egui::Context) -> bool {
    const HOLD: Duration = Duration::from_secs(45);
    static HELD_SINCE: Mutex<Option<Instant>> = Mutex::new(None);

    let has_selection = ctx
        .plugin_opt::<egui::text_selection::LabelSelectionState>()
        .map(|h| h.lock().has_selection())
        .unwrap_or(false);
    let dragging = has_selection && ctx.input(|i| i.pointer.primary_down());
    selection_hold_decision(has_selection, dragging, Instant::now(), &mut HELD_SINCE.lock(), HOLD)
}

/// The [`text_selection_hold`] state machine, pure so the cap is testable
/// without a running egui context.
fn selection_hold_decision(
    has_selection: bool,
    dragging: bool,
    now: Instant,
    held: &mut Option<Instant>,
    cap: Duration,
) -> bool {
    match (has_selection, *held) {
        (false, _) => {
            *held = None;
            false
        }
        (true, None) => {
            *held = Some(now);
            true
        }
        (true, Some(since)) => {
            if dragging {
                *held = Some(now);
                true
            } else {
                now.duration_since(since) < cap
            }
        }
    }
}

impl eframe::App for TunManApp {
    /// Runs even while the window is hidden — which is exactly why the close
    /// interception and the tray pump live here rather than in `ui`. eframe
    /// skips `ui` entirely for a hidden viewport, so a tray click handled there
    /// would never arrive.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_messages(ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            info!("closed to tray — tunnels keep running");
        }

        // Reveal once there are painted frames behind it: showing at frame zero
        // puts an unpainted white rectangle on screen through startup.
        self.frames = self.frames.saturating_add(1);
        if !self.startup_revealed && self.frames >= 3 {
            self.startup_revealed = true;
            if !self.start_hidden {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        // A hidden window still ticks, so the tray menu stays responsive.
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.refresh(ui.ctx());

        if let Some((_, at)) = &self.toast
            && at.elapsed() > Duration::from_secs(4)
        {
            self.toast = None;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Tunnels, "Tunnels")
                    .on_hover_text("Every tunnel, its state, and what is connected to it.");
                ui.selectable_value(&mut self.tab, Tab::Traffic, "Traffic").on_hover_text(
                    "Throughput over the last 30 minutes, and every process and \
                         destination across all tunnels.",
                );
                ui.selectable_value(&mut self.tab, Tab::Log, "Log").on_hover_text(
                    "TunMan's log, including everything ssh itself printed — where the \
                     reason a tunnel dropped will be.",
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("⚙")
                        .on_hover_text("Settings — ssh binary, start-up behaviour, health probe.")
                        .clicked()
                    {
                        self.settings_open = true;
                    }
                    if let Some((msg, _)) = &self.toast {
                        ui.weak(msg.clone());
                    }
                });
            });
            ui.separator();

            if let Some(err) = self.load_error.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, "⚠");
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("{err} — fix it by hand; TunMan will not overwrite it."),
                    );
                    if ui
                        .button("📂 Open config")
                        .on_hover_text("Open the folder holding TunMan.toml.")
                        .clicked()
                    {
                        open_path(&crate::app_paths::data_dir());
                    }
                });
                ui.separator();
            }

            match self.tab {
                Tab::Tunnels => tunnels::show(self, ui),
                Tab::Traffic => traffic::show(self, ui),
                Tab::Log => log_view::show(self, ui),
            }
        });

        dialogs::show_editor(self, ui.ctx());
        dialogs::show_settings(self, ui.ctx());
    }

    /// egui's own persistence writes the window geometry; 5 minutes is plenty
    /// and avoids rewriting the file every half minute for a window that has
    /// not moved.
    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(300)
    }
}

/// Open a folder or file in the OS file manager.
pub fn open_path(path: &std::path::Path) {
    #[cfg(windows)]
    {
        // Not `explorer.exe <path>`: it mangles anything with a query-like
        // segment. ShellExecute-equivalent via `cmd /c start` keeps it literal.
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &path.display().to_string()])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

/// Colour for a status, from the theme rather than hardcoded so it stays
/// legible in both light and dark.
pub fn status_color(ui: &egui::Ui, status: Status) -> egui::Color32 {
    match status {
        Status::Up => egui::Color32::from_rgb(0x57, 0xc7, 0x57),
        Status::Starting => ui.visuals().warn_fg_color,
        Status::Retrying => ui.visuals().warn_fg_color,
        Status::Failed => ui.visuals().error_fg_color,
        Status::Stopped => ui.visuals().weak_text_color(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An abandoned selection must not freeze the app's readouts for good.
    #[test]
    fn a_settled_selection_holds_refreshes_only_until_the_cap() {
        let cap = Duration::from_secs(45);
        let t0 = Instant::now();
        let mut held = None;

        assert!(selection_hold_decision(true, false, t0, &mut held, cap), "hold starts");
        assert!(selection_hold_decision(true, false, t0 + Duration::from_secs(10), &mut held, cap));
        assert!(
            !selection_hold_decision(true, false, t0 + Duration::from_secs(46), &mut held, cap),
            "an abandoned selection stops holding once the cap passes"
        );
    }

    /// Dragging restarts the clock, so a slow deliberate selection is never cut
    /// short mid-gesture.
    #[test]
    fn dragging_restarts_the_cap() {
        let cap = Duration::from_secs(45);
        let t0 = Instant::now();
        let mut held = None;
        selection_hold_decision(true, false, t0, &mut held, cap);
        assert!(selection_hold_decision(true, true, t0 + Duration::from_secs(44), &mut held, cap));
        // The clock restarted, so a moment later it is still holding.
        assert!(selection_hold_decision(true, false, t0 + Duration::from_secs(60), &mut held, cap));
    }

    #[test]
    fn no_selection_never_holds() {
        let mut held = Some(Instant::now());
        assert!(!selection_hold_decision(false, false, Instant::now(), &mut held, Duration::MAX));
        assert!(held.is_none(), "the hold is cleared so the next one starts fresh");
    }
}
