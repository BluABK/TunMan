//! The Mounts tab: sshfs and rclone mounts, kept up.

use egui_extras::{Column, TableBuilder};

use crate::jobs::{MountCommand, MountStatus};
use crate::mounts::MountKind;
use crate::ui::TunManApp;
use crate::util::{fmt_uptime, now_unix};

const ROW_H: f32 = 22.0;

fn status_color(ui: &egui::Ui, status: MountStatus) -> egui::Color32 {
    match status {
        MountStatus::Mounted => egui::Color32::from_rgb(0x57, 0xc7, 0x57),
        MountStatus::Starting | MountStatus::Retrying => ui.visuals().warn_fg_color,
        MountStatus::Failed => ui.visuals().error_fg_color,
        MountStatus::Stopped => ui.visuals().weak_text_color(),
    }
}

pub fn show(app: &mut TunManApp, ui: &mut egui::Ui) {
    let mounted = app.mount_rows.iter().filter(|m| m.status == MountStatus::Mounted).count();

    ui.horizontal_wrapped(|ui| {
        ui.heading("Mounts");
        ui.separator();
        ui.label(format!("{mounted} mounted"));
        ui.separator();
        ui.label(format!("{} defined", app.mount_rows.len()));
        if !crate::mounts::winfsp_installed() {
            ui.separator();
            ui.colored_label(ui.visuals().error_fg_color, "⚠ WinFsp missing").on_hover_text(
                "Both rclone mount and sshfs need WinFsp on Windows. Without it a mount \
                 fails with an error that does not obviously say so.",
            );
        }
    });
    ui.add_space(2.0);

    ui.horizontal_wrapped(|ui| {
        if ui.button("➕ Add").on_hover_text("Define a new mount.").clicked() {
            let m = crate::mounts::Mount {
                name: app.cfg.unique_mount_name("mount"),
                ..Default::default()
            };
            app.mount_editor = Some(crate::ui::dialogs::MountEdit::new(m, None));
        }
        if ui
            .button("▶ Mount all")
            .on_hover_text("Mount everything enabled that is not already up.")
            .clicked()
        {
            app.send_mount(MountCommand::StartAll);
        }
        if ui
            .button("⏹ Unmount all")
            .on_hover_text("Stop every mount, releasing its drive letter.")
            .clicked()
        {
            app.send_mount(MountCommand::StopAll);
        }
        ui.separator();
        if ui
            .button("🔄 Refresh remotes")
            .on_hover_text("Re-read the remote list from rclone, after editing its config.")
            .clicked()
        {
            app.rclone_remotes = crate::mounts::list_remotes(&app.cfg.settings.rclone_path);
            let n = app.rclone_remotes.len();
            app.note(format!("Found {n} rclone remotes"));
        }
        ui.weak(format!("{} rclone remotes", app.rclone_remotes.len())).on_hover_text(
            if app.rclone_remotes.is_empty() {
                "No remotes found. Check the rclone path in Settings, or run \
                 `rclone config` to set one up."
            } else {
                "Remotes already configured in rclone, offered when you pick a source."
            },
        );
    });
    ui.separator();

    if app.cfg.mounts.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.weak("No mounts yet.");
            ui.add_space(4.0);
            ui.weak(
                "An rclone mount can use any remote you already have — including its sftp \
                 backend, which does the same job as sshfs.",
            );
        });
        return;
    }

    let mut action: Option<(String, MountAction)> = None;

    TableBuilder::new(ui)
        .striped(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(18.0))
        .column(Column::auto().at_least(100.0).clip(true))
        .column(Column::auto().at_least(58.0))
        .column(Column::remainder().at_least(180.0).clip(true))
        .column(Column::auto().at_least(70.0))
        .column(Column::auto().at_least(72.0))
        .column(Column::auto().at_least(70.0))
        .column(Column::auto().at_least(96.0))
        .header(20.0, |mut h| {
            let cell = |h: &mut egui_extras::TableRow, title: &str, hover: &str| {
                h.col(|ui| {
                    ui.strong(title).on_hover_text(hover);
                });
            };
            cell(&mut h, "", "● mounted, ◐ mounting or retrying, ▲ failed, ○ not mounted.");
            cell(&mut h, "Name", "Also the tag this mount's lines carry in the Log tab.");
            cell(&mut h, "Via", "rclone or sshfs.");
            cell(&mut h, "Source", "What is being mounted.");
            cell(&mut h, "At", "Drive letter or directory it appears at.");
            cell(
                &mut h,
                "Uptime",
                "How long it has been answering. A mount is only counted as up once its \
                 path can actually be listed — a live process is not enough, because a \
                 mount can go stale while its process stays perfectly happy.",
            );
            cell(&mut h, "Retries", "Times it has been remounted after a drop.");
            cell(&mut h, "", "Mount, unmount, open, edit or delete.");
        })
        .body(|body| {
            body.rows(ROW_H, app.mount_rows.len(), |mut row| {
                let r = &app.mount_rows[row.index()];
                let def = app.cfg.mounts.iter().find(|m| m.name == r.name).cloned();

                row.col(|ui| {
                    let mut hover = format!("{}.", r.status.label());
                    if !r.last_error.is_empty() {
                        hover.push_str(&format!("\n\nLast message: {}", r.last_error));
                    }
                    if r.status == MountStatus::Retrying && r.next_retry_at > 0 {
                        hover.push_str(&format!(
                            "\n\nNext attempt in {}.",
                            fmt_uptime((r.next_retry_at - now_unix()).max(0))
                        ));
                    }
                    if r.gave_up {
                        hover.push_str(
                            "\n\nStopped retrying after reaching this mount's retry limit.",
                        );
                    }
                    ui.colored_label(status_color(ui, r.status), r.status.dot())
                        .on_hover_text(hover);
                });
                row.col(|ui| {
                    ui.label(&r.name);
                });
                row.col(|ui| {
                    let (label, hover) = match &def {
                        Some(m) => (m.kind.label(), m.kind.hint()),
                        None => ("—", "No longer in the config."),
                    };
                    ui.label(label).on_hover_text(hover);
                });
                row.col(|ui| {
                    ui.label(&r.source).on_hover_text(&r.source);
                });
                row.col(|ui| {
                    ui.label(egui::RichText::new(&r.target).monospace());
                });
                row.col(|ui| {
                    let text = match (r.status, r.since) {
                        (MountStatus::Mounted, Some(s)) => fmt_uptime(now_unix() - s),
                        (MountStatus::Retrying, _) if r.next_retry_at > 0 => {
                            format!("in {}", fmt_uptime((r.next_retry_at - now_unix()).max(0)))
                        }
                        _ => "—".to_string(),
                    };
                    ui.label(text);
                });
                row.col(|ui| {
                    let hover = match def.as_ref().map(|m| m.retry_delay_secs) {
                        Some(0) | None => {
                            "Reconnects with a doubling backoff from 5 seconds, capped at 5 \
                             minutes."
                                .to_string()
                        }
                        Some(d) => format!(
                            "Waits a fixed {d}s before reconnecting — set for servers that \
                             react badly to being prodded straight away."
                        ),
                    };
                    ui.label(if r.restarts == 0 { "—".into() } else { r.restarts.to_string() })
                        .on_hover_text(hover);
                });
                row.col(|ui| {
                    ui.horizontal(|ui| {
                        let running = matches!(
                            r.status,
                            MountStatus::Mounted | MountStatus::Starting | MountStatus::Retrying
                        );
                        if running {
                            if ui.small_button("⏹").on_hover_text("Unmount.").clicked() {
                                action = Some((r.name.clone(), MountAction::Stop));
                            }
                        } else if ui.small_button("▶").on_hover_text("Mount.").clicked() {
                            action = Some((r.name.clone(), MountAction::Start));
                        }
                        if ui
                            .add_enabled(
                                r.status == MountStatus::Mounted,
                                egui::Button::new("📂").small(),
                            )
                            .on_hover_text("Open the mount in Explorer.")
                            .clicked()
                        {
                            action = Some((r.name.clone(), MountAction::Open));
                        }
                        if ui.small_button("✏").on_hover_text("Edit this mount.").clicked() {
                            action = Some((r.name.clone(), MountAction::Edit));
                        }
                        if ui.small_button("🗑").on_hover_text("Delete this mount.").clicked() {
                            action = Some((r.name.clone(), MountAction::Delete));
                        }
                    });
                });
            });
        });

    if let Some((name, what)) = action {
        apply(app, &name, what);
    }
}

enum MountAction {
    Start,
    Stop,
    Open,
    Edit,
    Delete,
}

fn apply(app: &mut TunManApp, name: &str, what: MountAction) {
    match what {
        MountAction::Start => app.send_mount(MountCommand::Start(name.to_string())),
        MountAction::Stop => app.send_mount(MountCommand::Stop(name.to_string())),
        MountAction::Open => {
            if let Some(t) =
                app.mount_rows.iter().find(|m| m.name == name).map(|m| m.target.clone())
            {
                crate::ui::open_path(std::path::Path::new(&t));
            }
        }
        MountAction::Edit => {
            if let Some((i, m)) = app.cfg.mounts.iter().enumerate().find(|(_, m)| m.name == name) {
                app.mount_editor = Some(crate::ui::dialogs::MountEdit::new(m.clone(), Some(i)));
            }
        }
        MountAction::Delete => {
            app.send_mount(MountCommand::Stop(name.to_string()));
            app.cfg.mounts.retain(|m| m.name != name);
            app.mount_shared.states.lock().remove(name);
            app.save_config();
        }
    }
}

/// The kind picker, shared with the editor so the sshfs caveat is stated in one
/// place rather than drifting between them.
pub fn kind_hint(kind: MountKind) -> String {
    let mut hint = kind.hint().to_string();
    if kind == MountKind::Sshfs && !crate::mounts::sshfs_candidates().iter().any(|p| p.exists()) {
        hint.push_str(
            "\n\nsshfs-win does not appear to be installed here. Either install it, or use \
             an rclone remote on the sftp backend, which mounts an ssh server the same way \
             with the tooling you already have.",
        );
    }
    hint
}
