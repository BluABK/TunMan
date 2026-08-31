//! The Sync tab: rclone jobs, scheduled or on demand.

use egui_extras::TableBuilder;

use crate::jobs::{JobState, JobStatus, SyncCommand};
use crate::sync::SyncJob;
use crate::ui::TunManApp;
use crate::ui::table::ColSpec;
use crate::util::{fmt_uptime, now_unix};

const ROW_H: f32 = 22.0;

fn status_color(ui: &egui::Ui, status: JobStatus) -> egui::Color32 {
    match status {
        JobStatus::Ok => egui::Color32::from_rgb(0x57, 0xc7, 0x57),
        JobStatus::Running => ui.visuals().warn_fg_color,
        JobStatus::Failed => ui.visuals().error_fg_color,
        JobStatus::Idle => ui.visuals().weak_text_color(),
    }
}

fn when(at: i64) -> String {
    if at == 0 {
        return "—".to_string();
    }
    let ago = now_unix() - at;
    if ago < 0 { "—".to_string() } else { format!("{} ago", fmt_uptime(ago)) }
}

pub fn show(app: &mut TunManApp, ui: &mut egui::Ui) {
    let running = app.job_rows.iter().filter(|j| j.status == JobStatus::Running).count();

    ui.horizontal_wrapped(|ui| {
        ui.heading("Sync");
        ui.separator();
        ui.label(format!("{running} running"));
        ui.separator();
        ui.label(format!("{} jobs", app.job_rows.len()));
        ui.separator();
        ui.weak("rclone").on_hover_text(
            "Every job is an rclone run. Sources and destinations are rclone paths — a \
             remote like `offsite:photos`, or a local path.",
        );
    });
    ui.add_space(2.0);

    ui.horizontal_wrapped(|ui| {
        if ui.button("➕ Add").on_hover_text("Define a new sync job.").clicked() {
            let j =
                crate::sync::SyncJob { name: app.cfg.unique_job_name("job"), ..Default::default() };
            app.job_editor = Some(crate::ui::dialogs::JobEdit::new(j, None));
        }
        ui.separator();
        ui.weak("New jobs copy — they never delete.").on_hover_text(
            "rclone's sync makes the destination match the source, which means deleting \
             anything at the destination that is not at the source. Copy only ever adds, so \
             a wrong path costs a stray copy rather than data. Change the mode per job, and \
             dry-run it first.",
        );
    });
    ui.separator();

    if app.cfg.jobs.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.weak("No sync jobs yet.");
            ui.add_space(4.0);
            ui.weak("Pair two rclone paths, pick an interval, and TunMan keeps them in step.");
        });
        return;
    }

    // The detail panel is declared before the table so the table takes what is
    // left rather than claiming the whole window first.
    if app.selected_job.is_some() {
        egui::Panel::bottom("job_detail")
            .resizable(true)
            .default_size(200.0)
            .show(ui, |ui| detail(app, ui));
    }

    let mut action: Option<(String, JobAction)> = None;

    let cols = crate::ui::table::fit(COLS, ui.available_width(), ui.spacing().item_spacing.x);
    let mut builder = TableBuilder::new(ui)
        .striped(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
    for c in &cols {
        builder = builder.column(c.column());
    }

    builder
        .header(20.0, |mut h| {
            for c in &cols {
                let key = c.key;
                h.col(|ui| header_cell(ui, key));
            }
        })
        .body(|body| {
            body.rows(ROW_H, app.job_rows.len(), |mut row| {
                let r = &app.job_rows[row.index()];
                row.set_selected(app.selected_job.as_deref() == Some(r.name.as_str()));
                let def = app.cfg.jobs.iter().find(|j| j.name == r.name);

                for c in &cols {
                    let key = c.key;
                    row.col(|ui| cell(ui, key, r, def, &mut action));
                }

                if row.response().clicked() {
                    action = Some((r.name.clone(), JobAction::Select));
                }
            });
        });

    if let Some((name, what)) = action {
        apply(app, &name, what);
    }
}

/// The columns of the job table, in the order they are drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
enum C {
    Dot,
    Name,
    Mode,
    Paths,
    Progress,
    LastRun,
    Next,
    Actions,
}

/// Widths and drop order. The paths are the job, so they keep the remaining
/// width; the schedule columns are the first to go, since a job's own editor
/// says the same thing and the detail panel below repeats it.
const COLS: &[ColSpec<C>] = &[
    ColSpec::keep(C::Dot, 18.0),
    ColSpec::keep(C::Name, 96.0),
    ColSpec::opt(C::Mode, 110.0, 3),
    ColSpec::keep(C::Paths, 200.0).grow(),
    ColSpec::opt(C::Progress, 150.0, 4),
    ColSpec::opt(C::LastRun, 84.0, 2),
    ColSpec::opt(C::Next, 80.0, 1),
    ColSpec::keep(C::Actions, 112.0),
];

fn header_cell(ui: &mut egui::Ui, key: C) {
    let (title, hover) = match key {
        C::Dot => ("", "● last run succeeded, ◐ running, ▲ failed, ○ never run."),
        C::Name => ("Name", "Also the tag this job's lines carry in the Log tab."),
        C::Mode => ("Mode", "What this job does to the destination."),
        C::Paths => ("Source → destination", "rclone paths, exactly as rclone takes them."),
        C::Progress => ("Progress", "Live from rclone while a run is in flight."),
        C::LastRun => ("Last run", "When the last run finished."),
        C::Next => ("Next", "When the schedule will fire again. Manual jobs show —."),
        C::Actions => ("", "Run, dry-run, cancel, edit or delete."),
    };
    ui.strong(title).on_hover_text(hover);
}

fn cell(
    ui: &mut egui::Ui,
    key: C,
    r: &JobState,
    def: Option<&SyncJob>,
    action: &mut Option<(String, JobAction)>,
) {
    match key {
        C::Dot => {
            let mut hover = format!("{}.", r.status.label());
            if r.dry_run && r.status == JobStatus::Running {
                hover.push_str("\n\nDry run — reporting what it would do, changing nothing.");
            }
            if !r.last_error.is_empty() {
                hover.push_str(&format!("\n\n{}", r.last_error));
            }
            if r.last_ok_at > 0 {
                hover.push_str(&format!("\n\nLast clean run: {}", when(r.last_ok_at)));
            }
            ui.colored_label(status_color(ui, r.status), r.status.dot()).on_hover_text(hover);
        }

        // Carries the whole job in its hover: at a narrow width the name may be
        // the only column left that identifies the row.
        C::Name => {
            let mut hover = r.name.clone();
            if let Some(j) = def {
                hover.push_str(&format!("\n\n{}\n{}  →  {}", j.mode.label(), j.source, j.dest));
            }
            ui.label(&r.name).on_hover_text(hover);
        }

        C::Mode => match def {
            Some(j) => {
                let color = if j.mode.destructive() {
                    ui.visuals().warn_fg_color
                } else {
                    ui.visuals().text_color()
                };
                ui.colored_label(color, j.mode.label()).on_hover_text(j.mode.hint());
            }
            None => {
                ui.label("—");
            }
        },

        C::Paths => {
            let text = def.map(|j| format!("{}  →  {}", j.source, j.dest)).unwrap_or_default();
            ui.label(&text).on_hover_text(text.clone());
        }

        C::Progress => {
            if r.status == JobStatus::Running {
                let p = &r.progress;
                let text = if p.total.is_empty() {
                    "starting…".to_string()
                } else {
                    format!("{} / {} · {:.0}%", p.transferred, p.total, p.percent)
                };
                ui.label(text).on_hover_text(format!(
                    "{} at {}\n{}",
                    if r.dry_run { "Dry run" } else { "Transferring" },
                    if p.rate.is_empty() { "—" } else { &p.rate },
                    if p.eta.is_empty() {
                        "ETA unknown".to_string()
                    } else {
                        format!("ETA {}", p.eta)
                    }
                ));
            } else {
                ui.weak("—");
            }
        }

        C::LastRun => {
            ui.label(when(r.finished_at));
        }

        C::Next => {
            let text = match def.map(|j| j.interval_mins) {
                Some(0) | None => "—".to_string(),
                Some(_) if r.status == JobStatus::Running => "now".to_string(),
                Some(_) => {
                    let left = r.next_run_at - now_unix();
                    if left <= 0 { "due".to_string() } else { fmt_uptime(left) }
                }
            };
            ui.label(text).on_hover_text(match def.map(|j| j.interval_mins) {
                Some(0) | None => "This job only runs when you press Run.".to_string(),
                Some(m) => format!("Runs every {m} minutes."),
            });
        }

        C::Actions => {
            ui.horizontal(|ui| {
                let busy = r.status == JobStatus::Running;
                if busy {
                    if ui.small_button("⏹").on_hover_text("Cancel this run.").clicked() {
                        *action = Some((r.name.clone(), JobAction::Cancel));
                    }
                } else if ui.small_button("▶").on_hover_text("Run now.").clicked() {
                    *action = Some((r.name.clone(), JobAction::Run));
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("🔍").small())
                    .on_hover_text(
                        "Dry run — rclone reports every change it would make and \
                         makes none of them. Worth doing before trusting any job \
                         that deletes.",
                    )
                    .clicked()
                {
                    *action = Some((r.name.clone(), JobAction::DryRun));
                }
                if ui.small_button("✏").on_hover_text("Edit this job.").clicked() {
                    *action = Some((r.name.clone(), JobAction::Edit));
                }
                if ui.small_button("🗑").on_hover_text("Delete this job.").clicked() {
                    *action = Some((r.name.clone(), JobAction::Delete));
                }
            });
        }
    }
}

enum JobAction {
    Run,
    DryRun,
    Cancel,
    Edit,
    Delete,
    Select,
}

fn apply(app: &mut TunManApp, name: &str, what: JobAction) {
    match what {
        JobAction::Run => {
            app.send_sync(SyncCommand::Run { name: name.to_string(), dry_run: false })
        }
        JobAction::DryRun => {
            app.send_sync(SyncCommand::Run { name: name.to_string(), dry_run: true });
            app.selected_job = Some(name.to_string());
        }
        JobAction::Cancel => app.send_sync(SyncCommand::Cancel(name.to_string())),
        JobAction::Select => {
            app.selected_job = if app.selected_job.as_deref() == Some(name) {
                None
            } else {
                Some(name.to_string())
            };
        }
        JobAction::Edit => {
            if let Some((i, j)) = app.cfg.jobs.iter().enumerate().find(|(_, j)| j.name == name) {
                app.job_editor = Some(crate::ui::dialogs::JobEdit::new(j.clone(), Some(i)));
            }
        }
        JobAction::Delete => {
            app.send_sync(SyncCommand::Cancel(name.to_string()));
            app.cfg.jobs.retain(|j| j.name != name);
            app.sync_shared.states.lock().remove(name);
            if app.selected_job.as_deref() == Some(name) {
                app.selected_job = None;
            }
            app.save_config();
        }
    }
}

/// What the selected job's last run actually did — the part a dry run exists
/// to show you.
fn detail(app: &mut TunManApp, ui: &mut egui::Ui) {
    let Some(name) = app.selected_job.clone() else { return };
    let Some(state) = app.job_rows.iter().find(|j| j.name == name).cloned() else {
        app.selected_job = None;
        return;
    };

    ui.horizontal(|ui| {
        ui.heading(&state.name);
        ui.colored_label(status_color(ui, state.status), state.status.label());
        if state.dry_run {
            ui.colored_label(ui.visuals().warn_fg_color, "dry run")
                .on_hover_text("Nothing was changed — this is what the job would have done.");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("✖").on_hover_text("Close this panel.").clicked() {
                app.selected_job = None;
            }
            if ui.button("📋").on_hover_text("Copy this output.").clicked() {
                ui.ctx().copy_text(state.tail.join("\n"));
            }
        });
    });

    if !state.last_error.is_empty() && state.status == JobStatus::Failed {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(ui.visuals().error_fg_color, "⚠");
            ui.label(&state.last_error);
        });
    }

    if state.tail.is_empty() {
        ui.add_space(8.0);
        ui.weak("No output from the last run.");
        return;
    }

    egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        for line in &state.tail {
            ui.label(egui::RichText::new(line).monospace().size(11.0));
        }
    });
}
