//! The Tunnels tab: totals, actions, the table, and the detail panel.

use egui_extras::{Column, TableBuilder};

use crate::supervisor::{Command, Status};
use crate::ui::{TunmanApp, status_color};
use crate::util::{fmt_bytes, fmt_rate, fmt_uptime, now_unix};

/// Row height. Fixed so the table can virtualise.
const ROW_H: f32 = 22.0;

pub fn show(app: &mut TunmanApp, ui: &mut egui::Ui) {
    totals(app, ui);
    ui.add_space(2.0);
    actions(app, ui);
    ui.separator();

    if app.cfg.tunnels.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.weak("No tunnels yet.");
            ui.add_space(4.0);
            if ui
                .button("➕ Add your first tunnel")
                .on_hover_text(
                    "A SOCKS tunnel gives you a socks5h:// URL you can paste into anything \
                     that takes a proxy.",
                )
                .clicked()
            {
                add_tunnel(app);
            }
        });
        return;
    }

    // The detail panel is docked to the bottom so the table above it can take
    // the remaining height — declared first, as egui allocates panels in call
    // order and a table asked to fill would otherwise eat the whole window.
    if app.selected.is_some() {
        egui::Panel::bottom("tunnel_detail")
            .resizable(true)
            .default_size(220.0)
            .show(ui, |ui| detail(app, ui));
    }
    table(app, ui);
}

fn totals(app: &TunmanApp, ui: &mut egui::Ui) {
    let up = app.rows.iter().filter(|r| r.status == Status::Up).count();
    let down = app.rows.len().saturating_sub(up);
    let (mut rin, mut rout) = (0.0, 0.0);
    for r in &app.rows {
        let (i, o) = crate::sampler::rate_of(&r.name);
        rin += i;
        rout += o;
    }
    let conns: u64 = app.rows.iter().map(|r| r.traffic.live_conns()).sum();

    ui.horizontal_wrapped(|ui| {
        ui.heading("Tunnels");
        ui.separator();
        ui.label(format!("{up} up"))
            .on_hover_text("Tunnels whose forward is accepting connections right now.");
        ui.separator();
        ui.label(format!("{down} down")).on_hover_text(
            "Tunnels stopped, retrying, or failed. A retrying tunnel keeps trying \
             indefinitely with a backoff that tops out at 5 minutes.",
        );
        ui.separator();
        ui.label(format!("{conns} connections")).on_hover_text(
            "Sockets currently open through these tunnels, from the system's own \
             connection table.",
        );
        ui.separator();
        ui.label(format!("↓ {}  ↑ {}", fmt_rate(rin), fmt_rate(rout))).on_hover_text(
            "Combined throughput across METERED tunnels only. Tunnels without metering \
             contribute connection counts but no byte counts — Windows does not expose \
             per-socket byte totals.",
        );
    });
}

fn actions(app: &mut TunmanApp, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        if ui.button("➕ Add").on_hover_text("Define a new tunnel.").clicked() {
            add_tunnel(app);
        }
        if ui
            .button("▶ Start all")
            .on_hover_text("Start every enabled tunnel that is not already up.")
            .clicked()
        {
            app.send(Command::StartAll);
        }
        if ui
            .button("⏹ Stop all")
            .on_hover_text("Stop every running tunnel, including its ssh process.")
            .clicked()
        {
            app.send(Command::StopAll);
        }
        ui.separator();

        let urls = proxy_urls(app);
        if ui
            .add_enabled(!urls.is_empty(), egui::Button::new("📋 Copy all URLs"))
            .on_hover_text(if urls.is_empty() {
                "No SOCKS tunnels — only those have a proxy URL.".to_string()
            } else {
                format!("Copy all {} proxy URLs, one per line.", urls.len())
            })
            .clicked()
        {
            ui.ctx().copy_text(urls.join("\n"));
            app.note(format!("Copied {} URLs", urls.len()));
        }
        if ui
            .add_enabled(!urls.is_empty(), egui::Button::new("📤 Export"))
            .on_hover_text("Write the proxy URLs to a text file, one per line.")
            .clicked()
        {
            export(app, &urls);
        }
        if app.cfg.settings.sa_integration_enabled
            && ui
                .add_enabled(!urls.is_empty(), egui::Button::new("➡ Push to StreamArchiver"))
                .on_hover_text(
                    "Add these URLs to StreamArchiver's proxy pool. Never deletes, never \
                     re-enables anything you benched there — only adds and relabels.",
                )
                .clicked()
        {
            push_to_sa(app);
        }
        ui.separator();
        if ui
            .button("📂 Logs")
            .on_hover_text("Open the folder holding tunman's log files.")
            .clicked()
        {
            let dir = crate::app_paths::logs_dir();
            crate::app_paths::ensure_dir(&dir);
            crate::ui::open_path(&dir);
        }
    });
}

fn proxy_urls(app: &TunmanApp) -> Vec<String> {
    app.cfg.tunnels.iter().filter_map(|t| t.proxy_url()).collect()
}

fn add_tunnel(app: &mut TunmanApp) {
    let mut t = crate::model::Tunnel { name: app.cfg.unique_name("tunnel"), ..Default::default() };
    t.port = app.cfg.free_port(1080);
    app.editor = Some(crate::ui::dialogs::EditState::new(t, None));
}

fn export(app: &mut TunmanApp, urls: &[String]) {
    let Some(path) = rfd::FileDialog::new()
        .set_file_name("tunman-proxies.txt")
        .add_filter("Text", &["txt"])
        .save_file()
    else {
        return;
    };
    match std::fs::write(&path, urls.join("\n")) {
        Ok(()) => app.note(format!("Exported {} URLs", urls.len())),
        Err(e) => app.note(format!("Export failed: {e}")),
    }
}

fn push_to_sa(app: &mut TunmanApp) {
    let db = crate::sa_push::resolve_db_path(&app.cfg.settings.sa_db_path);
    let rows: Vec<(String, String)> = app
        .cfg
        .tunnels
        .iter()
        .filter_map(|t| t.proxy_url().map(|u| (format!("tunman: {}", t.name), u)))
        .collect();
    match crate::sa_push::push(&db, &rows) {
        Ok(r) => app.note(r.summary()),
        Err(e) => app.note(format!("Push failed: {e}")),
    }
}

fn table(app: &mut TunmanApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let mut action: Option<(String, RowAction)> = None;
    let selected = app.selected.clone();

    TableBuilder::new(ui)
        .striped(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(18.0)) // status dot
        .column(Column::auto().at_least(90.0).clip(true)) // name
        .column(Column::auto().at_least(56.0)) // kind
        .column(Column::auto().at_least(120.0).clip(true)) // target
        .column(Column::remainder().at_least(160.0).clip(true)) // advertised
        .column(Column::auto().at_least(64.0)) // uptime
        .column(Column::auto().at_least(44.0)) // conns
        .column(Column::auto().at_least(70.0)) // down rate
        .column(Column::auto().at_least(70.0)) // up rate
        .column(Column::auto().at_least(96.0)) // totals
        .column(Column::auto().at_least(112.0)) // actions
        .header(20.0, |mut h| {
            let cell = |h: &mut egui_extras::TableRow, title: &str, hover: &str| {
                h.col(|ui| {
                    ui.strong(title).on_hover_text(hover);
                });
            };
            cell(
                &mut h,
                "",
                "Status at a glance: ● up, ◐ starting or retrying, ▲ failed, ○ stopped.",
            );
            cell(&mut h, "Name", "Also the tag this tunnel's lines carry in the Log tab.");
            cell(&mut h, "Kind", "SOCKS (-D), Local (-L) or Remote (-R) forward.");
            cell(&mut h, "Server", "The SSH server this tunnel goes through.");
            cell(&mut h, "Address", "What clients should connect to. Click 📋 to copy it.");
            cell(&mut h, "Uptime", "How long the forward has been accepting connections.");
            cell(&mut h, "Conns", "Sockets open through this tunnel right now.");
            cell(&mut h, "↓/s", "Metered tunnels only — bytes arriving through the tunnel.");
            cell(&mut h, "↑/s", "Metered tunnels only — bytes leaving through the tunnel.");
            cell(&mut h, "Total", "Bytes in / out since this tunnel started, metered only.");
            cell(&mut h, "", "Start, stop, edit, copy the URL, or delete.");
        })
        .body(|body| {
            body.rows(ROW_H, app.rows.len(), |mut row| {
                let r = &app.rows[row.index()];
                row.set_selected(selected.as_deref() == Some(r.name.as_str()));

                row.col(|ui| {
                    let color = status_color(ui, r.status);
                    let mut hover = format!("{}.", r.status.label());
                    if !r.last_error.is_empty() {
                        hover.push_str(&format!("\n\nLast message: {}", r.last_error));
                    }
                    if r.status == Status::Retrying && r.next_retry_at > 0 {
                        let left = (r.next_retry_at - now_unix()).max(0);
                        hover.push_str(&format!("\n\nNext attempt in {}.", fmt_uptime(left)));
                    }
                    if let Some(ok) = r.probe_ok {
                        hover.push_str(&format!(
                            "\n\nProbe: {} — {}",
                            if ok { "reachable" } else { "FAILED" },
                            r.probe_note
                        ));
                    }
                    ui.colored_label(color, r.status.dot()).on_hover_text(hover);
                });
                row.col(|ui| {
                    ui.label(&r.name);
                });

                let def = app.cfg.tunnels.iter().find(|t| t.name == r.name).cloned();
                row.col(|ui| {
                    let (label, hover) = match &def {
                        Some(t) => (t.kind.label(), t.kind.hint()),
                        None => ("—", "This tunnel is no longer in the config."),
                    };
                    ui.label(label).on_hover_text(hover);
                });
                row.col(|ui| {
                    let target = def.as_ref().map(|t| t.target()).unwrap_or_default();
                    ui.label(&target).on_hover_text(target.clone());
                });
                row.col(|ui| {
                    let mut text = egui::RichText::new(&r.advertised).monospace();
                    if r.metering {
                        text = text.underline();
                    }
                    ui.label(text).on_hover_text(if r.metering {
                        "Metered: tunman owns this port and counts every byte through it."
                    } else {
                        "Not metered: connections are visible, byte counts are not."
                    });
                });
                row.col(|ui| {
                    let text = match (r.status, r.up_since) {
                        (Status::Up, Some(since)) => fmt_uptime(now_unix() - since),
                        (Status::Retrying, _) if r.next_retry_at > 0 => {
                            format!("in {}", fmt_uptime((r.next_retry_at - now_unix()).max(0)))
                        }
                        _ => "—".to_string(),
                    };
                    ui.label(text);
                });
                row.col(|ui| {
                    let n = r.traffic.live_conns();
                    ui.label(if n == 0 { "—".to_string() } else { n.to_string() });
                });

                let (rin, rout) = crate::sampler::rate_of(&r.name);
                row.col(|ui| {
                    ui.label(if r.metering { fmt_rate(rin) } else { "—".into() });
                });
                row.col(|ui| {
                    ui.label(if r.metering { fmt_rate(rout) } else { "—".into() });
                });
                row.col(|ui| {
                    use std::sync::atomic::Ordering;
                    if r.metering {
                        let i = r.traffic.total_in.load(Ordering::Relaxed);
                        let o = r.traffic.total_out.load(Ordering::Relaxed);
                        ui.label(format!("{} / {}", fmt_bytes(i), fmt_bytes(o)))
                            .on_hover_text("Bytes in / out since this tunnel started.");
                    } else {
                        ui.label("—").on_hover_text(
                            "Turn on Meter for this tunnel to count bytes. Without it \
                             Windows can name the processes but not their traffic.",
                        );
                    }
                });
                row.col(|ui| {
                    ui.horizontal(|ui| {
                        let running =
                            matches!(r.status, Status::Up | Status::Starting | Status::Retrying);
                        if running {
                            if ui.small_button("⏹").on_hover_text("Stop this tunnel.").clicked() {
                                action = Some((r.name.clone(), RowAction::Stop));
                            }
                        } else if ui.small_button("▶").on_hover_text("Start this tunnel.").clicked()
                        {
                            action = Some((r.name.clone(), RowAction::Start));
                        }
                        if ui
                            .add_enabled(
                                def.as_ref().and_then(|t| t.proxy_url()).is_some(),
                                egui::Button::new("📋").small(),
                            )
                            .on_hover_text("Copy this tunnel's proxy URL.")
                            .clicked()
                        {
                            action = Some((r.name.clone(), RowAction::Copy));
                        }
                        if ui.small_button("✏").on_hover_text("Edit this tunnel.").clicked() {
                            action = Some((r.name.clone(), RowAction::Edit));
                        }
                        if ui
                            .small_button("🗑")
                            .on_hover_text("Delete this tunnel from the config.")
                            .clicked()
                        {
                            action = Some((r.name.clone(), RowAction::Delete));
                        }
                    });
                });

                if row.response().clicked() {
                    action = Some((r.name.clone(), RowAction::Select));
                }
            });
        });

    if let Some((name, what)) = action {
        apply(app, &name, what, &ctx);
    }
}

enum RowAction {
    Start,
    Stop,
    Copy,
    Edit,
    Delete,
    Select,
}

fn apply(app: &mut TunmanApp, name: &str, what: RowAction, ctx: &egui::Context) {
    match what {
        RowAction::Start => app.send(Command::Start(name.to_string())),
        RowAction::Stop => app.send(Command::Stop(name.to_string())),
        RowAction::Select => {
            app.selected =
                if app.selected.as_deref() == Some(name) { None } else { Some(name.to_string()) };
        }
        RowAction::Copy => {
            if let Some(url) =
                app.rows.iter().find(|r| r.name == name).map(|r| r.advertised.clone())
            {
                ctx.copy_text(url.clone());
                app.note(format!("Copied {url}"));
            }
        }
        RowAction::Edit => {
            if let Some((idx, t)) = app.cfg.tunnels.iter().enumerate().find(|(_, t)| t.name == name)
            {
                app.editor = Some(crate::ui::dialogs::EditState::new(t.clone(), Some(idx)));
            }
        }
        RowAction::Delete => {
            app.send(Command::Stop(name.to_string()));
            app.cfg.tunnels.retain(|t| t.name != name);
            if app.selected.as_deref() == Some(name) {
                app.selected = None;
            }
            app.save_config();
        }
    }
}

/// The per-tunnel detail: who is connected, and to what.
fn detail(app: &mut TunmanApp, ui: &mut egui::Ui) {
    let Some(name) = app.selected.clone() else { return };
    let Some(state) = app.rows.iter().find(|r| r.name == name).cloned() else {
        app.selected = None;
        return;
    };

    ui.horizontal(|ui| {
        ui.heading(&state.name);
        ui.colored_label(status_color(ui, state.status), state.status.label());
        if !state.metering {
            ui.weak("· not metered").on_hover_text(
                "Processes are named from the system connection table. Byte counts and \
                 destinations need metering, which you can turn on when editing the tunnel.",
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("✖").on_hover_text("Close this panel.").clicked() {
                app.selected = None;
            }
        });
    });

    if !state.last_error.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(ui.visuals().warn_fg_color, "⚠");
            ui.label(&state.last_error);
        });
    }
    if state.restarts > 0 {
        ui.weak(format!("Restarted {} times this session.", state.restarts)).on_hover_text(
            "Each unexpected exit is retried with a growing delay, capped at 5 minutes. \
             A tunnel that keeps restarting is usually an auth or a network problem — the \
             Log tab has ssh's own words for it.",
        );
    }

    let rows = state.traffic.rows();
    if rows.is_empty() {
        ui.add_space(8.0);
        ui.weak("Nothing connected right now.");
        return;
    }

    ui.add_space(4.0);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::auto().at_least(56.0))
            .column(Column::auto().at_least(140.0).clip(true))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::auto().at_least(50.0))
            .column(Column::auto().at_least(80.0))
            .column(Column::auto().at_least(80.0))
            .header(20.0, |mut h| {
                h.col(|ui| {
                    ui.strong("PID").on_hover_text("The process that opened the connection.");
                });
                h.col(|ui| {
                    ui.strong("Process").on_hover_text("Executable name, from the OS.");
                });
                h.col(|ui| {
                    ui.strong("Destination").on_hover_text(
                        "Where the client asked to go, read out of the SOCKS handshake as it \
                         passed. Only available on metered SOCKS tunnels.",
                    );
                });
                h.col(|ui| {
                    ui.strong("Conns").on_hover_text("Open now / opened in total.");
                });
                h.col(|ui| {
                    ui.strong("In").on_hover_text("Bytes received through the tunnel.");
                });
                h.col(|ui| {
                    ui.strong("Out").on_hover_text("Bytes sent through the tunnel.");
                });
            })
            .body(|body| {
                body.rows(ROW_H, rows.len(), |mut row| {
                    let r = &rows[row.index()];
                    row.col(|ui| {
                        ui.label(if r.pid == 0 { "—".to_string() } else { r.pid.to_string() })
                            .on_hover_text(if r.pid == 0 {
                                "The socket closed before its owner could be looked up."
                            } else {
                                "Process id at the time the connection was opened."
                            });
                    });
                    row.col(|ui| {
                        ui.label(if r.process.is_empty() { "—" } else { &r.process });
                    });
                    row.col(|ui| {
                        ui.label(if r.dest.is_empty() { "—" } else { &r.dest });
                    });
                    row.col(|ui| {
                        ui.label(format!("{} / {}", r.live, r.total_conns));
                    });
                    row.col(|ui| {
                        ui.label(if state.metering { fmt_bytes(r.in_bytes) } else { "—".into() });
                    });
                    row.col(|ui| {
                        ui.label(if state.metering {
                            fmt_bytes(r.out_bytes)
                        } else {
                            "—".into()
                        });
                    });
                });
            });
    });
}
