//! The Tunnels tab: totals, actions, the table, and the detail panel.

use egui_extras::{Column, TableBuilder};

use crate::model::Tunnel;
use crate::supervisor::{Command, Status, TunnelState};
use crate::ui::table::ColSpec;
use crate::ui::{TunManApp, status_color};
use crate::util::{fmt_bytes, fmt_rate, fmt_uptime, now_unix};

/// Row height. Fixed so the table can virtualise.
const ROW_H: f32 = 22.0;

/// How much observed time an availability figure needs before it is worth
/// showing. Below this the number is dominated by the second or two a tunnel
/// spends coming up — 20 seconds up out of 21 observed is 95%, which reads as
/// a fault rather than as arithmetic.
const AVAIL_SETTLE_SECS: u64 = 120;

pub fn show(app: &mut TunManApp, ui: &mut egui::Ui) {
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

fn totals(app: &TunManApp, ui: &mut egui::Ui) {
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

fn actions(app: &mut TunManApp, ui: &mut egui::Ui) {
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
            .on_hover_text("Open the folder holding TunMan's log files.")
            .clicked()
        {
            let dir = crate::app_paths::logs_dir();
            crate::app_paths::ensure_dir(&dir);
            crate::ui::open_path(&dir);
        }
    });
}

fn proxy_urls(app: &TunManApp) -> Vec<String> {
    app.cfg.tunnels.iter().filter_map(|t| t.proxy_url()).collect()
}

fn add_tunnel(app: &mut TunManApp) {
    let mut t = crate::model::Tunnel { name: app.cfg.unique_name("tunnel"), ..Default::default() };
    t.port = app.cfg.free_port(1080);
    app.editor = Some(crate::ui::dialogs::EditState::new(t, None));
}

fn export(app: &mut TunManApp, urls: &[String]) {
    let Some(path) = rfd::FileDialog::new()
        .set_file_name("TunMan-proxies.txt")
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

fn push_to_sa(app: &mut TunManApp) {
    let db = crate::sa_push::resolve_db_path(&app.cfg.settings.sa_db_path);
    let rows: Vec<(String, String)> = app
        .cfg
        .tunnels
        .iter()
        .filter_map(|t| t.proxy_url().map(|u| (format!("TunMan: {}", t.name), u)))
        .collect();
    match crate::sa_push::push(&db, &rows) {
        Ok(r) => app.note(r.summary()),
        Err(e) => app.note(format!("Push failed: {e}")),
    }
}

/// Colour ramp for a fraction of a cap: quiet until it matters, loud at the top.
fn cap_color(ui: &egui::Ui, frac: f32) -> egui::Color32 {
    if frac >= 1.0 {
        ui.visuals().error_fg_color
    } else if frac >= 0.8 {
        ui.visuals().warn_fg_color
    } else {
        ui.visuals().text_color()
    }
}

/// Everything a cap readout needs to say, in one hover.
fn cap_hover(st: &crate::supervisor::TunnelState, metered: bool) -> String {
    if !st.cap_config.any_set() {
        return "No bandwidth caps set for this tunnel.".to_string();
    }
    if !metered {
        return "Caps are set, but this tunnel is not metered — there are no byte counts \
                to measure against, so nothing is enforced. Turn on Meter traffic when \
                editing the tunnel."
            .to_string();
    }
    let mut out = String::new();
    for (w, used, limit, frac) in &st.caps.windows {
        if *limit == 0 {
            continue;
        }
        out.push_str(&format!(
            "{}: {} of {} ({:.0}%)\n{}\n\n",
            w.label(),
            fmt_bytes(*used),
            fmt_bytes(*limit),
            frac * 100.0,
            w.hint()
        ));
    }
    out.push_str(&format!("At the cap: {}.", st.cap_config.action.label()));
    out
}

/// The columns of the tunnel table, in the order they are drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
enum C {
    Dot,
    Name,
    Kind,
    Geo,
    Server,
    Exit,
    Address,
    Uptime,
    Avail,
    Latency,
    Conns,
    Down,
    Up,
    Total,
    Cap,
    Actions,
}

/// Widths and drop order.
///
/// The four kept columns are what a row needs to be worth showing at all:
/// which tunnel it is, where to point a client, and the buttons to start or
/// stop it. Everything else gives way as the window narrows, least useful
/// first — the totals go before the caps, the caps before the exit address,
/// and uptime is the last to leave. Nothing is lost by dropping a column:
/// select the row and the detail panel below spells all of it out.
const COLS: &[ColSpec<C>] = &[
    ColSpec::keep(C::Dot, 18.0),
    ColSpec::keep(C::Name, 96.0),
    ColSpec::opt(C::Kind, 52.0, 8),
    ColSpec::opt(C::Geo, 36.0, 6),
    ColSpec::opt(C::Server, 150.0, 5),
    ColSpec::opt(C::Exit, 104.0, 3),
    ColSpec::keep(C::Address, 140.0).grow(),
    ColSpec::opt(C::Uptime, 62.0, 12),
    ColSpec::opt(C::Avail, 56.0, 4),
    ColSpec::opt(C::Latency, 60.0, 7),
    ColSpec::opt(C::Conns, 46.0, 11),
    ColSpec::opt(C::Down, 66.0, 10),
    ColSpec::opt(C::Up, 66.0, 9),
    ColSpec::opt(C::Total, 108.0, 1),
    ColSpec::opt(C::Cap, 52.0, 2),
    ColSpec::keep(C::Actions, 112.0),
];

fn header_cell(ui: &mut egui::Ui, key: C) {
    let (title, hover) = match key {
        C::Dot => ("", "Status at a glance: ● up, ◐ starting or retrying, ▲ failed, ○ stopped."),
        C::Name => ("Name", "Also the tag this tunnel's lines carry in the Log tab."),
        C::Kind => ("Kind", "SOCKS (-D), Local (-L) or Remote (-R) forward."),
        C::Geo => (
            "Geo",
            "Country of the tunnel's EXIT, measured by asking through the tunnel itself \
             rather than by looking up the server's address. Set a manual override when \
             editing a tunnel if the provider geolocates somewhere misleading.\n\nThe flag \
             image is fetched once per country and cached; a country whose flag could \
             not be fetched shows its two-letter code instead.",
        ),
        C::Server => (
            "Server",
            "The SSH server, with its address from a local DNS lookup so you never have \
             to resolve the hostname yourself.",
        ),
        C::Exit => (
            "Exit IP",
            "The address the far side actually presents. Not always the server's own \
             address — a provider can route egress through a different one, and a \
             jump-hosted tunnel comes out somewhere else entirely.",
        ),
        C::Address => ("Address", "What clients should connect to. Click 📋 to copy it."),
        C::Uptime => ("Uptime", "How long the forward has been accepting connections."),
        C::Avail => (
            "Avail",
            "Share of the time since this tunnel first started that it has been up. \
             Counted only while TunMan is running, and reset when TunMan restarts.",
        ),
        C::Latency => (
            "Latency",
            "Round trip of the last probe, through the tunnel. Measured once when the \
             tunnel comes up, and repeatedly if the health probe is on. The hover shows \
             the average of the last few, which is the number worth trusting — one slow \
             probe on a busy link is noise.",
        ),
        C::Conns => ("Conns", "Sockets open through this tunnel right now."),
        C::Down => ("↓/s", "Metered tunnels only — bytes arriving through the tunnel."),
        C::Up => ("↑/s", "Metered tunnels only — bytes leaving through the tunnel."),
        C::Total => ("Total", "Bytes in / out since this tunnel started, metered only."),
        C::Cap => (
            "Cap",
            "How close this tunnel is to its tightest bandwidth cap. Caps need metering \
             to be enforceable.",
        ),
        C::Actions => ("", "Start, stop, edit, copy the URL, or delete."),
    };
    ui.strong(title).on_hover_text(hover);
}

/// Flag images the table may draw, by country code. Looked up rather than
/// fetched here: a cell is drawn every frame, and a table is not the place
/// to start network requests from.
struct FlagLookup<'a>(&'a std::collections::HashMap<String, egui::TextureHandle>);

impl FlagLookup<'_> {
    fn get(&self, country: &str) -> Option<&egui::TextureHandle> {
        self.0.get(&crate::flags::normalise(country)?)
    }
}

fn cell(
    ui: &mut egui::Ui,
    key: C,
    r: &TunnelState,
    def: Option<&Tunnel>,
    flags: &FlagLookup,
    action: &mut Option<(String, RowAction)>,
) {
    match key {
        C::Dot => {
            let color = status_color(ui, r.status);
            let mut hover = format!("{}.", r.status.label());
            if !r.last_error.is_empty() {
                hover.push_str(&format!("\n\nLast message: {}", r.last_error));
            }
            if r.status == Status::Retrying && r.next_retry_at > 0 {
                let left = (r.next_retry_at - now_unix()).max(0);
                hover.push_str(&format!("\n\nNext attempt in {}.", fmt_uptime(left)));
            }
            if r.stopped_by_cap {
                hover.push_str(
                    "\n\nStopped because it hit a bandwidth cap. It will start again \
                     on its own when the window rolls over.",
                );
            }
            if let Some(ok) = r.probe_ok {
                hover.push_str(&format!(
                    "\n\nProbe: {} — {}",
                    if ok { "reachable" } else { "FAILED" },
                    r.probe_note
                ));
            }
            ui.colored_label(color, r.status.dot()).on_hover_text(hover);
        }

        // The name carries a summary, because in a narrow window it may be the
        // only thing left identifying the row.
        C::Name => {
            let mut hover = r.name.clone();
            if let Some(t) = def {
                hover.push_str(&format!("\n\n{} to {}", t.kind.label(), t.target()));
            }
            if !r.exit_ip.is_empty() {
                hover.push_str(&format!("\nComes out at {}", r.exit_ip));
            }
            hover.push_str("\n\nClick the row for the full picture.");
            ui.label(&r.name).on_hover_text(hover);
        }

        C::Kind => {
            let (label, hover) = match def {
                Some(t) => (t.kind.label(), t.kind.hint()),
                None => ("—", "This tunnel is no longer in the config."),
            };
            ui.label(label).on_hover_text(hover);
        }

        // Country of the exit.
        C::Geo => {
            let overridden = def.is_some_and(|t| !t.country_override.trim().is_empty());
            if r.country.is_empty() {
                let hover = match def {
                    Some(t) if !t.probeable() => {
                        "Only a SOCKS tunnel can be asked where it comes out."
                    }
                    _ => {
                        "Not measured yet. Every SOCKS tunnel is asked where it comes out \
                         once it is up; until then, or if that request cannot get through, \
                         this stays empty. A country can also be set by hand when editing \
                         the tunnel."
                    }
                };
                ui.weak("—").on_hover_text(hover);
            } else {
                let hover = if overridden {
                    format!("{} — set manually for this tunnel.", r.country)
                } else {
                    format!("{} — measured through the tunnel.", r.country)
                };
                match flags.get(&r.country) {
                    // The flag is 4:3 at a height that matches the text
                    // beside it, so a row of them lines up whatever shape
                    // the individual flags are.
                    Some(tex) => {
                        ui.add(
                            egui::Image::new(tex)
                                .fit_to_exact_size(egui::vec2(18.0, 13.5))
                                .corner_radius(1.0),
                        )
                        .on_hover_text(hover);
                    }
                    // Not fetched yet, or not fetchable: the code says the
                    // same thing, less prettily.
                    None => {
                        ui.label(r.country.to_uppercase()).on_hover_text(hover);
                    }
                }
            }
        }

        // Server, with its resolved address right beside the hostname.
        // The resolved address moved into the hover rather than sitting beside
        // the hostname. A nested horizontal layout inside a table cell is what
        // made this column render as a 4px sliver: the column sized itself from
        // its own measured content, the label inside truncated itself to the
        // width it was offered, and the two fed each other down to nothing. One
        // label in a fixed-width column cannot collapse that way.
        // Server, with its resolved address right beside the hostname.
        C::Server => {
            let target = def.map(|t| t.target()).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label(&target);
                if r.server_ip.is_empty() {
                    ui.weak("·").on_hover_text("Not resolved yet.");
                } else {
                    ui.weak(&r.server_ip);
                }
            })
            .response
            .on_hover_text(if r.server_ip.is_empty() {
                format!("{target}\n\nAddress not resolved yet.")
            } else {
                format!("{target}\nResolves to {} (local DNS lookup).", r.server_ip)
            });
        }

        // Exit address.
        C::Exit => {
            if r.exit_ip.is_empty() {
                ui.weak("—").on_hover_text(
                    "Where this tunnel comes out is measured through the tunnel itself, \
                     once it is up. Empty means that request has not completed: the \
                     tunnel may still be starting, or whatever it exits through blocks \
                     the lookup. The Log tab has the reason.",
                );
            } else {
                let same = r.exit_ip == r.server_ip;
                ui.label(egui::RichText::new(&r.exit_ip).monospace()).on_hover_text(if same {
                    format!(
                        "{}\n\nSame as the server's own address — traffic comes \
                         out where it went in.",
                        r.exit_ip
                    )
                } else {
                    format!(
                        "{}\n\nDIFFERENT from the server address ({}). The \
                         provider routes egress elsewhere, or this tunnel is \
                         jump-hosted.",
                        r.exit_ip, r.server_ip
                    )
                });
            }
        }

        C::Address => {
            let mut text = egui::RichText::new(&r.advertised).monospace();
            if r.metering {
                text = text.underline();
            }
            ui.label(text).on_hover_text(if r.metering {
                "Metered: TunMan owns this port and counts every byte through it."
            } else {
                "Not metered: connections are visible, byte counts are not."
            });
        }

        C::Uptime => {
            let text = match (r.status, r.up_since) {
                (Status::Up, Some(since)) => fmt_uptime(now_unix() - since),
                (Status::Retrying, _) if r.next_retry_at > 0 => {
                    format!("in {}", fmt_uptime((r.next_retry_at - now_unix()).max(0)))
                }
                _ => "—".to_string(),
            };
            ui.label(text);
        }

        // Availability and error history.
        C::Avail => match r.availability().filter(|_| r.tracked_secs >= AVAIL_SETTLE_SECS) {
            None => {
                ui.weak("—").on_hover_text(format!(
                    "Not enough observed yet — {} of the first {}.

A tunnel that has just                      started is always a second or two short of perfect, and showing that as                      95% would read as a fault rather than as arithmetic.",
                    fmt_uptime(r.tracked_secs as i64),
                    fmt_uptime(AVAIL_SETTLE_SECS as i64)
                ));
            }
            Some(frac) => {
                let pct = frac * 100.0;
                let color = if pct >= 99.0 {
                    ui.visuals().text_color()
                } else if pct >= 90.0 {
                    ui.visuals().warn_fg_color
                } else {
                    ui.visuals().error_fg_color
                };
                ui.colored_label(color, format!("{pct:.1}%")).on_hover_text(format!(
                    "Up for {} of the {} observed since this tunnel first \
                     started.\n\nRestarts: {}\nConsecutive failures: {}\n\n\
                     Counted only while TunMan runs, and reset on restart.",
                    fmt_uptime(r.up_secs as i64),
                    fmt_uptime(r.tracked_secs as i64),
                    r.restarts,
                    r.fails
                ));
            }
        },

        C::Latency => {
            if r.latency_ms == 0 {
                ui.weak("—").on_hover_text(
                    "No probe has completed yet. Latency comes from the same request \
                     that measures the exit, made once when the tunnel comes up; turn \
                     the health probe on in Settings to keep re-measuring it.",
                );
            } else {
                let color = if r.latency_ms < 150 {
                    ui.visuals().text_color()
                } else if r.latency_ms < 400 {
                    ui.visuals().warn_fg_color
                } else {
                    ui.visuals().error_fg_color
                };
                ui.colored_label(color, format!("{} ms", r.latency_ms)).on_hover_text(format!(
                    "Last probe: {} ms\nAverage of the last few: {} ms\n\n\
                     A full request through the tunnel and back, so it includes \
                     the far side's own latency, not just the hop to the server.",
                    r.latency_ms, r.latency_avg_ms
                ));
            }
        }

        C::Conns => {
            let n = r.traffic.live_conns();
            ui.label(if n == 0 { "—".to_string() } else { n.to_string() });
        }

        C::Down => {
            let (rin, _) = crate::sampler::rate_of(&r.name);
            ui.label(if r.metering { fmt_rate(rin) } else { "—".into() });
        }
        C::Up => {
            let (_, rout) = crate::sampler::rate_of(&r.name);
            ui.label(if r.metering { fmt_rate(rout) } else { "—".into() });
        }

        C::Total => {
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
        }

        // Cap headroom.
        C::Cap => {
            let hover = cap_hover(r, r.metering);
            if !r.cap_config.any_set() {
                ui.weak("—").on_hover_text(hover);
            } else if !r.metering {
                ui.colored_label(ui.visuals().error_fg_color, "⚠").on_hover_text(hover);
            } else {
                let frac = r.caps_worst();
                ui.colored_label(cap_color(ui, frac), format!("{:.0}%", (frac * 100.0).min(999.0)))
                    .on_hover_text(hover);
            }
        }

        C::Actions => {
            ui.horizontal(|ui| {
                let running = matches!(r.status, Status::Up | Status::Starting | Status::Retrying);
                if running {
                    if ui.small_button("⏹").on_hover_text("Stop this tunnel.").clicked() {
                        *action = Some((r.name.clone(), RowAction::Stop));
                    }
                } else if ui.small_button("▶").on_hover_text("Start this tunnel.").clicked() {
                    *action = Some((r.name.clone(), RowAction::Start));
                }
                if ui
                    .add_enabled(
                        def.and_then(|t| t.proxy_url()).is_some(),
                        egui::Button::new("📋").small(),
                    )
                    .on_hover_text("Copy this tunnel's proxy URL.")
                    .clicked()
                {
                    *action = Some((r.name.clone(), RowAction::Copy));
                }
                if ui.small_button("✏").on_hover_text("Edit this tunnel.").clicked() {
                    *action = Some((r.name.clone(), RowAction::Edit));
                }
                if ui
                    .small_button("🗑")
                    .on_hover_text("Delete this tunnel from the config.")
                    .clicked()
                {
                    *action = Some((r.name.clone(), RowAction::Delete));
                }
            });
        }
    }
}

/// Make sure every country currently on screen has its flag fetched and
/// uploaded, exactly once each.
///
/// Called from the table because that is what knows which countries are on
/// screen, but it does no work in the common case: a code already in the
/// texture map is skipped, and a code with no file yet starts one background
/// fetch and is skipped on every frame until that lands.
fn load_flags(app: &mut TunManApp, ctx: &egui::Context) {
    let wanted: Vec<String> = app
        .rows
        .iter()
        .filter_map(|r| crate::flags::normalise(&r.country))
        .filter(|cc| !app.flag_textures.contains_key(cc))
        .collect();
    for cc in wanted {
        match crate::flags::cached(&cc) {
            Some(png) => {
                if let Some(img) = crate::flags::decode(&png) {
                    let tex =
                        ctx.load_texture(format!("flag:{cc}"), img, egui::TextureOptions::LINEAR);
                    app.flag_textures.insert(cc, tex);
                }
                // A file that will not decode is left alone rather than
                // retried every frame; the row shows the code instead.
            }
            None => crate::flags::ensure(&cc, &app.rt),
        }
    }
}

fn table(app: &mut TunManApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let mut action: Option<(String, RowAction)> = None;
    let selected = app.selected.clone();
    // Every country on screen needs its flag fetched once and uploaded
    // once; both are no-ops after the first time.
    load_flags(app, ui.ctx());
    let flags = FlagLookup(&app.flag_textures);

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
            body.rows(ROW_H, app.rows.len(), |mut row| {
                let r = &app.rows[row.index()];
                row.set_selected(selected.as_deref() == Some(r.name.as_str()));
                let def = app.cfg.tunnels.iter().find(|t| t.name == r.name);

                for c in &cols {
                    let key = c.key;
                    row.col(|ui| cell(ui, key, r, def, &flags, &mut action));
                }

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

fn apply(app: &mut TunManApp, name: &str, what: RowAction, ctx: &egui::Context) {
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
            // Otherwise the ledger keeps hourly buckets under a name nothing
            // refers to any more, and a tunnel later given the same name would
            // inherit a stranger's usage against its caps.
            crate::sampler::forget_tunnel(name);
            app.shared.states.lock().remove(name);
            if app.selected.as_deref() == Some(name) {
                app.selected = None;
            }
            app.save_config();
        }
    }
}

/// Everything the table can drop when the window is narrow, spelled out for
/// the selected tunnel. It wraps, so it survives any width — which is the
/// point: no fact in this app is reachable only through a wide window.
fn facts(app: &TunManApp, ui: &mut egui::Ui, state: &crate::supervisor::TunnelState) {
    let def = app.cfg.tunnels.iter().find(|t| t.name == state.name);
    let dash = |s: &str| if s.is_empty() { "—".to_string() } else { s.to_string() };

    let mut items: Vec<(&str, String, &str)> = Vec::new();
    if let Some(t) = def {
        items.push(("Kind", t.kind.label().to_string(), t.kind.hint()));
        items.push(("Server", t.target(), "The SSH server this tunnel connects to."));
    }
    items.push((
        "Address",
        state.advertised.clone(),
        "What clients connect to. Underlined in the table when metered.",
    ));
    items.push((
        "Server IP",
        dash(&state.server_ip),
        "The server's address from a local DNS lookup.",
    ));
    items.push((
        "Exit IP",
        dash(&state.exit_ip),
        "Where the far side actually comes out, measured through the tunnel.",
    ));
    items.push((
        "Geo",
        if state.country.is_empty() { "—".to_string() } else { state.country.to_uppercase() },
        "Country of the exit. Needs the health probe, or a manual override.",
    ));
    items.push((
        "Uptime",
        match (state.status, state.up_since) {
            (Status::Up, Some(since)) => fmt_uptime(now_unix() - since),
            _ => "—".to_string(),
        },
        "How long the forward has been accepting connections.",
    ));
    items.push((
        "Avail",
        match state.availability().filter(|_| state.tracked_secs >= AVAIL_SETTLE_SECS) {
            Some(f) => format!("{:.1}%", f * 100.0),
            None => "—".to_string(),
        },
        "Share of the observed time this tunnel has been up, since TunMan started.          Held back until a couple of minutes have been observed.",
    ));
    items.push((
        "Latency",
        if state.latency_ms == 0 {
            "—".to_string()
        } else {
            format!("{} ms (avg {})", state.latency_ms, state.latency_avg_ms)
        },
        "Round trip of the last probe, through the tunnel.",
    ));
    if state.metering {
        use std::sync::atomic::Ordering;
        let (rin, rout) = crate::sampler::rate_of(&state.name);
        items.push((
            "Rate",
            format!("↓ {}  ↑ {}", fmt_rate(rin), fmt_rate(rout)),
            "Current throughput, counted by TunMan's own listener.",
        ));
        items.push((
            "Total",
            format!(
                "{} / {}",
                fmt_bytes(state.traffic.total_in.load(Ordering::Relaxed)),
                fmt_bytes(state.traffic.total_out.load(Ordering::Relaxed))
            ),
            "Bytes in / out since this tunnel started.",
        ));
    }
    if state.cap_config.any_set() {
        items.push((
            "Cap",
            format!("{:.0}%", (state.caps_worst() * 100.0).min(999.0)),
            "How close this tunnel is to its tightest bandwidth cap.",
        ));
    }

    ui.horizontal_wrapped(|ui| {
        for (i, (label, value, hover)) in items.iter().enumerate() {
            if i > 0 {
                ui.separator();
            }
            ui.weak(*label);
            ui.label(value).on_hover_text(*hover);
        }
    });
}

/// The per-tunnel detail: who is connected, and to what.
fn detail(app: &mut TunManApp, ui: &mut egui::Ui) {
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

    facts(app, ui, &state);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(width: f32) -> Vec<C> {
        crate::ui::table::fit(COLS, width, 8.0).iter().map(|c| c.key).collect()
    }

    /// The report that prompted this: a window about 1030px wide, where the
    /// last five columns — including every button on the row — had run off the
    /// right-hand edge, so a tunnel could not be stopped or edited without
    /// resizing the window first.
    #[test]
    fn a_row_stays_operable_at_every_width_the_window_allows() {
        // 720 is the minimum inner width the viewport permits.
        for w in [720.0_f32, 900.0, 1030.0, 1280.0, 1920.0] {
            let keys = at(w);
            assert!(keys.contains(&C::Actions), "no buttons at {w}px: {keys:?}");
            assert!(keys.contains(&C::Name), "unidentifiable row at {w}px");
            assert!(keys.contains(&C::Address), "no address at {w}px");
            assert!(keys.contains(&C::Dot), "no status at {w}px");
        }
    }

    /// What survives at the narrowest supported width. Not arbitrary: these
    /// are the columns worth the space when there is almost none — is it up,
    /// for how long, and is anything actually going through it.
    #[test]
    fn the_narrowest_window_keeps_the_columns_worth_keeping() {
        assert_eq!(
            at(720.0),
            vec![C::Dot, C::Name, C::Address, C::Uptime, C::Conns, C::Down, C::Up, C::Actions]
        );
    }

    #[test]
    fn a_wide_window_still_shows_everything() {
        assert_eq!(at(1920.0).len(), COLS.len());
    }

    /// Sanity on the ranks themselves: every optional column has a distinct
    /// one, or two columns would trade places from frame to frame as the
    /// window is dragged.
    #[test]
    fn the_drop_order_is_unambiguous() {
        let mut ranks: Vec<u8> = COLS.iter().map(|c| c.rank).filter(|r| *r > 0).collect();
        let before = ranks.len();
        ranks.sort_unstable();
        ranks.dedup();
        assert_eq!(ranks.len(), before, "duplicate drop ranks in COLS");
    }
}
