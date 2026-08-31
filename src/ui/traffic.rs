//! The Traffic tab: throughput over time, and every client across all tunnels.

use std::collections::HashMap;

use egui_extras::{Column, TableBuilder};
use egui_plot::{Legend, Line, Plot, PlotPoints};

use crate::ui::TunManApp;
use crate::util::{fmt_bytes, fmt_rate};

const ROW_H: f32 = 22.0;

pub fn show(app: &mut TunManApp, ui: &mut egui::Ui) {
    let metered: Vec<String> =
        app.rows.iter().filter(|r| r.metering).map(|r| r.name.clone()).collect();

    ui.horizontal_wrapped(|ui| {
        ui.heading("Traffic");
        ui.separator();
        if metered.is_empty() {
            ui.colored_label(ui.visuals().warn_fg_color, "No metered tunnels").on_hover_text(
                "Byte counts need metering. Edit a SOCKS or local tunnel and tick \"Meter \
                 traffic\" — TunMan then owns the port and counts what passes through it. \
                 Windows has no per-socket byte counter to read instead.",
            );
        } else {
            ui.weak(format!("{} metered", metered.len()));
        }
        ui.separator();
        ui.weak(format!("last {} minutes", crate::sampler::HISTORY_LEN / 60)).on_hover_text(
            "The graph holds 30 minutes of one-second samples. Older data is not kept.",
        );
    });
    ui.separator();

    graph(app, &metered, ui);
    ui.separator();
    combined_table(app, ui);
}

fn graph(app: &TunManApp, metered: &[String], ui: &mut egui::Ui) {
    if app.history.is_empty() || metered.is_empty() {
        ui.add_space(8.0);
        ui.weak("Nothing to plot yet.");
        ui.add_space(8.0);
        return;
    }
    // The x axis is seconds ago, so "now" sits at 0 on the right and the line
    // does not slide sideways as the ring fills.
    let newest = app.history.last().map(|s| s.at_ms).unwrap_or(0);

    Plot::new("rates")
        .height(200.0)
        .legend(Legend::default())
        .include_y(0.0)
        .allow_scroll(false)
        .allow_drag(false)
        .y_axis_formatter(|m, _| fmt_rate(m.value.max(0.0)))
        .x_axis_formatter(|m, _| {
            let secs = -m.value;
            if secs <= 0.5 { "now".to_string() } else { format!("-{:.0}s", secs) }
        })
        .label_formatter(|pos| {
            // 0.37 replaced the (name, point) pair with an enum that also
            // covers hovering away from any line.
            Some(match pos {
                egui_plot::HoverPosition::NearDataPoint { plot_name, position, .. } => format!(
                    "{plot_name}\n{} at -{:.0}s",
                    fmt_rate(position.y.max(0.0)),
                    -position.x
                ),
                egui_plot::HoverPosition::Elsewhere { position } => {
                    format!("{} at -{:.0}s", fmt_rate(position.y.max(0.0)), -position.x)
                }
            })
        })
        .show(ui, |plot| {
            for name in metered {
                for (label, pick) in [("↓ ", true), ("↑ ", false)] {
                    let points: PlotPoints = app
                        .history
                        .iter()
                        .filter_map(|s| {
                            let (i, o) = s.rates.get(name)?;
                            let age = (s.at_ms - newest) as f64 / 1000.0;
                            Some([age, if pick { *i } else { *o }])
                        })
                        .collect();
                    plot.line(Line::new(format!("{label}{name}"), points));
                }
            }
        });
}

/// Every client across every tunnel, folded together.
fn combined_table(app: &mut TunManApp, ui: &mut egui::Ui) {
    // (pid, process, dest, tunnel) → totals. Tunnel is part of the key: the
    // same process on two tunnels is two facts, not one.
    #[derive(Default)]
    struct Row {
        process: String,
        live: u64,
        total: u64,
        in_bytes: u64,
        out_bytes: u64,
        metered: bool,
    }
    let mut by: HashMap<(u32, String, String), Row> = HashMap::new();

    for st in &app.rows {
        for r in st.traffic.rows() {
            let e = by.entry((r.pid, r.dest.clone(), st.name.clone())).or_default();
            e.process = r.process.clone();
            e.live += r.live;
            e.total += r.total_conns;
            e.in_bytes += r.in_bytes;
            e.out_bytes += r.out_bytes;
            e.metered = st.metering;
        }
    }

    let mut rows: Vec<((u32, String, String), Row)> = by.into_iter().collect();
    rows.sort_by(|a, b| {
        (b.1.in_bytes + b.1.out_bytes)
            .cmp(&(a.1.in_bytes + a.1.out_bytes))
            .then(b.1.live.cmp(&a.1.live))
            .then(a.1.process.cmp(&b.1.process))
    });

    if rows.is_empty() {
        ui.add_space(8.0);
        ui.weak("Nothing is connected through any tunnel right now.");
        return;
    }

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::auto().at_least(56.0))
            .column(Column::auto().at_least(140.0).clip(true))
            .column(Column::auto().at_least(100.0).clip(true))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::auto().at_least(56.0))
            .column(Column::auto().at_least(80.0))
            .column(Column::auto().at_least(80.0))
            .header(20.0, |mut h| {
                h.col(|ui| {
                    ui.strong("PID");
                });
                h.col(|ui| {
                    ui.strong("Process").on_hover_text("The program using the tunnel.");
                });
                h.col(|ui| {
                    ui.strong("Tunnel").on_hover_text("Which tunnel it is going through.");
                });
                h.col(|ui| {
                    ui.strong("Destination").on_hover_text(
                        "Read from the SOCKS handshake. Blank on tunnels without metering.",
                    );
                });
                h.col(|ui| {
                    ui.strong("Conns").on_hover_text("Open now / opened in total.");
                });
                h.col(|ui| {
                    ui.strong("In");
                });
                h.col(|ui| {
                    ui.strong("Out");
                });
            })
            .body(|body| {
                body.rows(ROW_H, rows.len(), |mut row| {
                    let ((pid, dest, tunnel), r) = &rows[row.index()];
                    row.col(|ui| {
                        ui.label(if *pid == 0 { "—".to_string() } else { pid.to_string() });
                    });
                    row.col(|ui| {
                        ui.label(if r.process.is_empty() { "—" } else { &r.process });
                    });
                    row.col(|ui| {
                        ui.label(tunnel);
                    });
                    row.col(|ui| {
                        ui.label(if dest.is_empty() { "—" } else { dest });
                    });
                    row.col(|ui| {
                        ui.label(format!("{} / {}", r.live, r.total));
                    });
                    row.col(|ui| {
                        ui.label(if r.metered { fmt_bytes(r.in_bytes) } else { "—".into() });
                    });
                    row.col(|ui| {
                        ui.label(if r.metered { fmt_bytes(r.out_bytes) } else { "—".into() });
                    });
                });
            });
    });
}
