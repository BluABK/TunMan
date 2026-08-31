//! The Log tab: everything TunMan logged, including ssh's own output.
//!
//! The rows are refreshed incrementally — the filter only re-scans the whole
//! ring when it changes, and otherwise pulls just what arrived since the last
//! sequence number it saw.

use std::sync::Arc;
use std::time::Duration;

use crate::log_capture::{self, LogRecord, level_rank};
use crate::ui::TunManApp;

const ROW_H: f32 = 18.0;

/// Minimum severity to show.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LevelFilter(pub Option<tracing::Level>);

impl LevelFilter {
    fn label(self) -> &'static str {
        match self.0 {
            None => "All levels",
            Some(tracing::Level::ERROR) => "Error",
            Some(tracing::Level::WARN) => "Warn+",
            Some(tracing::Level::INFO) => "Info+",
            Some(tracing::Level::DEBUG) => "Debug+",
            Some(tracing::Level::TRACE) => "Trace+",
        }
    }
}

pub struct Row {
    pub record: Arc<LogRecord>,
    /// Message and fields joined once, since every filter and the copy button
    /// want the same string.
    pub text: String,
}

pub struct LogViewState {
    pub search: String,
    pub regex: bool,
    pub level: LevelFilter,
    /// Show only lines carrying this tunnel's name.
    pub tunnel: Option<String>,
    pub follow: bool,
    pub rows: Vec<Row>,
    last_seq: u64,
    last_filter: (String, bool, Option<tracing::Level>, Option<String>),
    pub mute_draft: String,
}

impl Default for LogViewState {
    fn default() -> Self {
        LogViewState {
            search: String::new(),
            regex: false,
            level: LevelFilter(None),
            tunnel: None,
            follow: true,
            rows: Vec::new(),
            last_seq: 0,
            last_filter: (String::new(), false, None, None),
            mute_draft: String::new(),
        }
    }
}

impl LogViewState {
    fn filter_key(&self) -> (String, bool, Option<tracing::Level>, Option<String>) {
        (self.search.clone(), self.regex, self.level.0, self.tunnel.clone())
    }

    /// Pull in new records, or rebuild if the filter changed.
    ///
    /// `hold` pauses the live tail while the user has text selected — appending
    /// rows cancels an egui selection, so without this the log cannot be copied
    /// from while anything is happening.
    fn refresh(&mut self, hold: bool) {
        if self.filter_key() != self.last_filter {
            self.rebuild();
            return;
        }
        if hold {
            return;
        }
        let fresh = log_capture::since(self.last_seq);
        for r in fresh {
            self.last_seq = self.last_seq.max(r.seq);
            if self.matches(&r) {
                self.rows.push(Row { text: text_of(&r), record: r });
            }
        }
    }

    fn rebuild(&mut self) {
        self.last_filter = self.filter_key();
        self.rows.clear();
        for r in log_capture::snapshot() {
            self.last_seq = self.last_seq.max(r.seq);
            if self.matches(&r) {
                self.rows.push(Row { text: text_of(&r), record: r });
            }
        }
    }

    fn matches(&self, r: &LogRecord) -> bool {
        record_matches(r, self.level.0, self.tunnel.as_deref(), &self.search, self.regex)
    }
}

fn text_of(r: &LogRecord) -> String {
    if r.fields.is_empty() { r.message.clone() } else { format!("{} {}", r.message, r.fields) }
}

/// Whether one record passes the filters. Pure, so the matching rules can be
/// tested without a UI or a live subscriber.
pub fn record_matches(
    r: &LogRecord,
    min_level: Option<tracing::Level>,
    tunnel: Option<&str>,
    query: &str,
    regex: bool,
) -> bool {
    if let Some(min) = min_level
        && level_rank(r.level) > level_rank(min)
    {
        return false;
    }
    if let Some(t) = tunnel
        && r.tunnel.as_deref() != Some(t)
    {
        return false;
    }
    if query.trim().is_empty() {
        return true;
    }
    let hay = format!("{} {} {}", r.message, r.fields, r.target);
    if regex {
        // An invalid pattern matches nothing rather than everything: a
        // half-typed regex silently showing the whole log reads as "the filter
        // is broken", which is worse than an empty list plus the warning icon.
        match regex_lite::Regex::new(&format!("(?i){query}")) {
            Ok(re) => re.is_match(&hay),
            Err(_) => false,
        }
    } else {
        hay.to_lowercase().contains(&query.to_lowercase())
    }
}

/// The error from an invalid regex, for the warning icon's hover.
pub fn regex_pattern_error(query: &str) -> Option<String> {
    regex_lite::Regex::new(&format!("(?i){query}")).err().map(|e| e.to_string())
}

fn level_color(ui: &egui::Ui, level: tracing::Level) -> egui::Color32 {
    match level {
        tracing::Level::ERROR => ui.visuals().error_fg_color,
        tracing::Level::WARN => ui.visuals().warn_fg_color,
        tracing::Level::INFO => ui.visuals().text_color(),
        _ => ui.visuals().weak_text_color(),
    }
}

pub fn show(app: &mut TunManApp, ui: &mut egui::Ui) {
    ui.ctx().request_repaint_after(Duration::from_millis(250));
    let hold = crate::ui::text_selection_hold(ui.ctx());
    let tunnels = log_capture::tunnels_seen();
    let mut mute_requested: Option<String> = None;

    ui.horizontal_wrapped(|ui| {
        egui::ComboBox::from_id_salt("log_level")
            .selected_text(app.log.level.label())
            .show_ui(ui, |ui| {
                for f in [
                    LevelFilter(None),
                    LevelFilter(Some(tracing::Level::ERROR)),
                    LevelFilter(Some(tracing::Level::WARN)),
                    LevelFilter(Some(tracing::Level::INFO)),
                    LevelFilter(Some(tracing::Level::DEBUG)),
                    LevelFilter(Some(tracing::Level::TRACE)),
                ] {
                    ui.selectable_value(&mut app.log.level, f, f.label());
                }
            })
            .response
            .on_hover_text("Show this severity and everything above it.");

        egui::ComboBox::from_id_salt("log_tunnel")
            .selected_text(app.log.tunnel.clone().unwrap_or_else(|| "All tunnels".into()))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut app.log.tunnel, None, "All tunnels");
                for t in &tunnels {
                    ui.selectable_value(&mut app.log.tunnel, Some(t.clone()), t);
                }
            })
            .response
            .on_hover_text(
                "Show only lines from one tunnel, including everything its ssh process \
                 printed. This is where the reason a tunnel keeps dropping will be.",
            );

        ui.add(
            egui::TextEdit::singleline(&mut app.log.search)
                .hint_text("Search")
                .desired_width(180.0),
        )
        .on_hover_text("Matches the message, its fields and its source.");
        if !app.log.search.is_empty() && ui.small_button("✖").on_hover_text("Clear.").clicked() {
            app.log.search.clear();
        }
        ui.checkbox(&mut app.log.regex, "Regex")
            .on_hover_text("Treat the search as a case-insensitive regular expression.");
        if app.log.regex
            && !app.log.search.is_empty()
            && let Some(err) = regex_pattern_error(&app.log.search)
        {
            ui.colored_label(ui.visuals().warn_fg_color, "⚠")
                .on_hover_text(format!("{err}\n\nNothing matches while the pattern is invalid."));
        }
        ui.checkbox(&mut app.log.follow, "Follow").on_hover_text("Stay pinned to the newest line.");

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(format!("{} / {} captured", app.log.rows.len(), log_capture::len()))
                .on_hover_text(format!(
                    "Shown after filtering, out of the {} most recent lines kept in memory. \
                     The files on disk go further back.",
                    log_capture::CAPACITY
                ));
            if ui
                .button("🗑")
                .on_hover_text("Clear the in-memory log. Files are untouched.")
                .clicked()
            {
                log_capture::clear();
                app.log.rebuild();
            }
            if ui.button("📋").on_hover_text("Copy every line currently shown.").clicked() {
                let text = app
                    .log
                    .rows
                    .iter()
                    .map(|r| format!("{} {} {}", stamp(r.record.time_ms), r.record.level, r.text))
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.ctx().copy_text(text);
                app.toast = Some(("Copied the log".into(), std::time::Instant::now()));
            }
            let mutes = log_capture::mute_list();
            ui.menu_button(format!("🔇 {}", mutes.len()), |ui| {
                ui.label("Muted patterns");
                ui.weak("Lines matching these are never captured. Cleared on restart.");
                ui.separator();
                for (i, m) in mutes.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✖").clicked() {
                            log_capture::remove_mute(i);
                        }
                        ui.label(m);
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut app.log.mute_draft);
                    if ui.button("Mute").clicked() && !app.log.mute_draft.trim().is_empty() {
                        mute_requested = Some(app.log.mute_draft.clone());
                        app.log.mute_draft.clear();
                    }
                });
            })
            .response
            .on_hover_text("Silence a runaway log source for this session.");
        });
    });
    ui.separator();

    app.log.refresh(hold);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(app.log.follow)
        .show_rows(ui, ROW_H, app.log.rows.len(), |ui, range| {
            for i in range {
                let row = &app.log.rows[i];
                let r = &row.record;
                let resp = ui
                    .horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(stamp(r.time_ms)).monospace().weak().size(11.0),
                        );
                        ui.label(
                            egui::RichText::new(format!("{:5}", r.level.as_str()))
                                .monospace()
                                .size(11.0)
                                .color(level_color(ui, r.level)),
                        );
                        if let Some(t) = &r.tunnel {
                            let (cr, cg, cb) = crate::logfmt::tag_rgb(t);
                            ui.label(
                                egui::RichText::new(format!("[{t}]"))
                                    .monospace()
                                    .size(11.0)
                                    .color(egui::Color32::from_rgb(cr, cg, cb)),
                            );
                        }
                        ui.add(
                            egui::Label::new(egui::RichText::new(&row.text).size(12.0))
                                .wrap_mode(egui::TextWrapMode::Truncate),
                        );
                    })
                    .response;

                // A stable salt is required: a row's position shifts every time
                // a line arrives, so a position-derived id would move the
                // context menu onto a different line mid-interaction.
                let id = ui.make_persistent_id(("log_row", r.seq));
                let resp = ui.interact(resp.rect, id, egui::Sense::click());
                resp.context_menu(|ui| {
                    if ui.button("🔇 Mute lines like this").clicked() {
                        mute_requested = Some(log_capture::suggested_mute_pattern(&row.text));
                        ui.close();
                    }
                    if ui.button("📋 Copy line").clicked() {
                        ui.ctx().copy_text(row.text.clone());
                        ui.close();
                    }
                });
            }
        });

    // Applied after the loop: `add_mute` purges the buffer, and doing that
    // while iterating `app.log.rows` would be a borrow conflict.
    if let Some(p) = mute_requested {
        log_capture::add_mute(&p);
        app.log.rebuild();
    }
}

fn stamp(time_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(time_ms)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S%.3f").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(level: tracing::Level, msg: &str, tunnel: Option<&str>) -> LogRecord {
        LogRecord {
            seq: 1,
            time_ms: 0,
            level,
            target: "TunMan::ssh",
            message: msg.to_string(),
            tunnel: tunnel.map(|t| t.to_string()),
            fields: String::new(),
        }
    }

    #[test]
    fn the_level_filter_keeps_everything_at_least_as_severe() {
        let warn = rec(tracing::Level::WARN, "x", None);
        let debug = rec(tracing::Level::DEBUG, "x", None);
        assert!(record_matches(&warn, Some(tracing::Level::WARN), None, "", false));
        assert!(!record_matches(&debug, Some(tracing::Level::WARN), None, "", false));
        assert!(record_matches(&debug, None, None, "", false), "no filter keeps everything");
    }

    /// The tunnel filter matches the field, not the text. A message that merely
    /// mentions another tunnel's name belongs to whoever logged it.
    #[test]
    fn the_tunnel_filter_uses_the_field_not_the_message() {
        let mine = rec(tracing::Level::INFO, "all good", Some("vps-fi"));
        let theirs = rec(tracing::Level::INFO, "could not reach vps-fi", Some("vps-de"));
        assert!(record_matches(&mine, None, Some("vps-fi"), "", false));
        assert!(!record_matches(&theirs, None, Some("vps-fi"), "", false));
    }

    #[test]
    fn search_is_case_insensitive_and_covers_the_target() {
        let r = rec(tracing::Level::INFO, "Connection Refused", None);
        assert!(record_matches(&r, None, None, "refused", false));
        assert!(record_matches(&r, None, None, "TunMan::ssh", false));
        assert!(!record_matches(&r, None, None, "timeout", false));
    }

    #[test]
    fn a_regex_search_matches_case_insensitively() {
        let r = rec(tracing::Level::INFO, "port 1080 bound", None);
        assert!(record_matches(&r, None, None, r"port \d+", true));
        assert!(!record_matches(&r, None, None, r"port [a-z]+", true));
    }

    /// A half-typed pattern showing the entire log reads as a broken filter.
    /// Matching nothing, next to the warning icon, is the honest answer.
    #[test]
    fn an_invalid_regex_matches_nothing_rather_than_everything() {
        let r = rec(tracing::Level::INFO, "anything", None);
        assert!(!record_matches(&r, None, None, "((", true));
        assert!(regex_pattern_error("((").is_some());
        assert!(regex_pattern_error("valid").is_none());
    }
}
