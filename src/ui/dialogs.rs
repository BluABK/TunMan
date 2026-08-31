//! The two windows: editing a tunnel, and the settings.

use crate::model::{AuthMode, Tunnel, TunnelKind};
use crate::supervisor::Command;
use crate::ui::TunManApp;

/// A tunnel being edited. Held as a draft so Cancel really cancels — editing
/// the live config in place would restart the tunnel on every keystroke.
pub struct EditState {
    pub draft: Tunnel,
    /// Index in the config, or `None` when this is a new tunnel.
    pub index: Option<usize>,
    /// The name it had on open, so a rename can be detected.
    pub original_name: String,
    pub show_password: bool,
}

impl EditState {
    pub fn new(draft: Tunnel, index: Option<usize>) -> EditState {
        let original_name = draft.name.clone();
        EditState { draft, index, original_name, show_password: false }
    }
}

pub fn show_editor(app: &mut TunManApp, ctx: &egui::Context) {
    let Some(mut ed) = app.editor.take() else { return };
    let mut open = true;
    let mut save = false;
    let mut cancel = false;

    egui::Window::new(if ed.index.is_some() { "Edit tunnel" } else { "New tunnel" })
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(430.0)
        .show(ctx, |ui| {
            egui::Grid::new("edit_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Name").on_hover_text(
                    "Identifies the tunnel in the table, in the log, and in the config file.",
                );
                ui.text_edit_singleline(&mut ed.draft.name);
                ui.end_row();

                ui.label("Kind").on_hover_text(ed.draft.kind.hint());
                egui::ComboBox::from_id_salt("kind").selected_text(ed.draft.kind.label()).show_ui(
                    ui,
                    |ui| {
                        for k in TunnelKind::ALL {
                            ui.selectable_value(&mut ed.draft.kind, k, k.label())
                                .on_hover_text(k.hint());
                        }
                    },
                );
                ui.end_row();

                ui.label("User").on_hover_text("Leave blank to use your local username.");
                ui.text_edit_singleline(&mut ed.draft.user);
                ui.end_row();

                ui.label("Host").on_hover_text("The SSH server to tunnel through.");
                ui.text_edit_singleline(&mut ed.draft.host);
                ui.end_row();

                ui.label("SSH port").on_hover_text("22 unless the server moved it.");
                ui.add(egui::DragValue::new(&mut ed.draft.ssh_port).range(1..=65535));
                ui.end_row();

                ui.separator();
                ui.end_row();

                let (port_label, port_hover) = match ed.draft.kind {
                    TunnelKind::Socks => (
                        "SOCKS port",
                        "The local port the proxy listens on. This is the port that goes in \
                         the socks5h:// URL.",
                    ),
                    TunnelKind::Local => (
                        "Local port",
                        "The port on this machine that reaches the destination below.",
                    ),
                    TunnelKind::Remote => (
                        "Server port",
                        "The port on the SERVER that reaches back to the destination below.",
                    ),
                };
                ui.label(port_label).on_hover_text(port_hover);
                ui.add(egui::DragValue::new(&mut ed.draft.port).range(1..=65535));
                ui.end_row();

                ui.label("Bind address").on_hover_text(
                    "127.0.0.1 keeps this private to your machine. Binding 0.0.0.0 exposes \
                     an OPEN PROXY to your whole network — only do that deliberately.",
                );
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut ed.draft.bind);
                    if ed.draft.bind.trim() != "127.0.0.1" && ed.draft.bind.trim() != "localhost" {
                        ui.colored_label(ui.visuals().warn_fg_color, "⚠").on_hover_text(
                            "Reachable from outside this machine. Anyone who can reach this \
                             port can use your tunnel.",
                        );
                    }
                });
                ui.end_row();

                if ed.draft.kind != TunnelKind::Socks {
                    ui.label("Destination").on_hover_text(match ed.draft.kind {
                        TunnelKind::Local => "Host and port as seen FROM THE SERVER.",
                        _ => "Host and port as seen FROM THIS MACHINE.",
                    });
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut ed.draft.dest_host);
                        ui.add(egui::DragValue::new(&mut ed.draft.dest_port).range(0..=65535));
                    });
                    ui.end_row();
                }

                ui.separator();
                ui.end_row();

                ui.label("Auth").on_hover_text(
                    "Key or agent runs ssh in batch mode, so it fails loudly instead of \
                     waiting on a prompt you cannot see.",
                );
                ui.horizontal(|ui| {
                    for a in [AuthMode::KeyOrAgent, AuthMode::Password] {
                        ui.selectable_value(&mut ed.draft.auth, a, a.label());
                    }
                });
                ui.end_row();

                ui.label("Key file").on_hover_text(
                    "Optional. Blank lets ssh use your agent and its own defaults. Naming a \
                     key also stops ssh offering every other key first.",
                );
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut ed.draft.identity_file);
                    if ui.button("…").on_hover_text("Browse for a private key.").clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_file()
                    {
                        ed.draft.identity_file = p.display().to_string();
                    }
                });
                ui.end_row();

                if ed.draft.auth == AuthMode::Password {
                    ui.label("Password").on_hover_text(
                        "Stored in TunMan.toml as plain text and passed to ssh through the \
                         environment. It is masked in the log and in the copied command, but \
                         a key is safer wherever you can use one.",
                    );
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.draft.password)
                                .password(!ed.show_password),
                        );
                        ui.checkbox(&mut ed.show_password, "👁").on_hover_text("Show it.");
                    });
                    ui.end_row();
                }

                ui.separator();
                ui.end_row();

                ui.label("Options");
                ui.vertical(|ui| {
                    ui.checkbox(&mut ed.draft.enabled, "Enabled").on_hover_text(
                        "Unchecked, this tunnel is never started, even by Start all.",
                    );
                    ui.checkbox(&mut ed.draft.auto_start, "Start with TunMan")
                        .on_hover_text("Bring this tunnel up when TunMan launches.");

                    let meterable = ed.draft.kind.meterable();
                    ui.add_enabled_ui(meterable, |ui| {
                        ui.checkbox(&mut ed.draft.meter, "Meter traffic").on_hover_text(
                            if meterable {
                                "Count bytes and, for SOCKS, show each destination. TunMan \
                                 takes the port and hands connections on to ssh, which costs \
                                 one loopback hop. Without this you still see which processes \
                                 are connected, just not how much they moved."
                            } else {
                                "A remote forward is dialled from the server, so there is no \
                                 local socket for TunMan to sit in front of."
                            },
                        );
                    });
                    ui.checkbox(&mut ed.draft.compression, "Compression (-C)").on_hover_text(
                        "Helps on a slow link with compressible traffic. Usually a loss on \
                         video or anything already encrypted.",
                    );
                });
                ui.end_row();

                ui.label("Keepalive").on_hover_text(
                    "Seconds between keepalives. Zero turns them off, which is how a tunnel \
                     silently dies behind a NAT that drops idle connections.",
                );
                ui.add(egui::DragValue::new(&mut ed.draft.keepalive_secs).range(0..=600));
                ui.end_row();

                ui.label("Extra ssh args").on_hover_text(
                    "Passed verbatim, space-separated, before the target. For things like \
                     -o ProxyJump=bastion.",
                );
                let mut extra = ed.draft.extra_args.join(" ");
                if ui.text_edit_singleline(&mut extra).changed() {
                    ed.draft.extra_args = extra.split_whitespace().map(|s| s.to_string()).collect();
                }
                ui.end_row();
            });

            ui.separator();

            // The command as it will actually run — the fastest way to see that
            // an option landed where you meant it to.
            let bind = crate::ssh::Bind {
                addr: ed.draft.bind.clone(),
                port: if ed.draft.metering() { 0 } else { ed.draft.port },
            };
            let cmd = crate::ssh::display_command(&app.cfg.settings.ssh_path, &ed.draft, &bind);
            ui.horizontal(|ui| {
                ui.weak("Command");
                if ui.small_button("📋").on_hover_text("Copy this command.").clicked() {
                    ui.ctx().copy_text(cmd.clone());
                }
            });
            ui.label(egui::RichText::new(&cmd).monospace().weak()).on_hover_text(
                if ed.draft.metering() {
                    "With metering on, ssh binds a private port chosen at start-up (shown as \
                     0 here) and TunMan listens on the port you set."
                } else {
                    "Exactly what TunMan will run."
                },
            );

            let problems = ed.draft.validate();
            let name_taken = app
                .cfg
                .tunnels
                .iter()
                .enumerate()
                .any(|(i, t)| t.name == ed.draft.name && Some(i) != ed.index);
            if name_taken {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    "Another tunnel already has that name.",
                );
            }
            for p in &problems {
                ui.colored_label(ui.visuals().error_fg_color, p);
            }

            ui.separator();
            ui.horizontal(|ui| {
                let ok = problems.is_empty() && !name_taken;
                if ui
                    .add_enabled(ok, egui::Button::new("Save"))
                    .on_hover_text(if ok {
                        "Write to TunMan.toml. A running tunnel restarts only if something \
                         it depends on changed."
                    } else {
                        "Fix the problems above first."
                    })
                    .clicked()
                {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });

    if save {
        // A rename is a new identity: stop the old one so its ssh process and
        // metering listener do not outlive the name they were started under.
        if ed.index.is_some() && ed.original_name != ed.draft.name {
            app.send(Command::Stop(ed.original_name.clone()));
            app.shared.states.lock().remove(&ed.original_name);
            if app.selected.as_deref() == Some(ed.original_name.as_str()) {
                app.selected = Some(ed.draft.name.clone());
            }
        }
        match ed.index {
            Some(i) if i < app.cfg.tunnels.len() => app.cfg.tunnels[i] = ed.draft.clone(),
            _ => app.cfg.tunnels.push(ed.draft.clone()),
        }
        app.save_config();
        return; // editor closed
    }
    if !cancel && open {
        app.editor = Some(ed);
    }
}

pub fn show_settings(app: &mut TunManApp, ctx: &egui::Context) {
    if !app.settings_open {
        return;
    }
    let mut open = true;
    let mut save = false;
    let mut cfg = app.cfg.settings.clone();

    egui::Window::new("Settings")
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(460.0)
        .show(ctx, |ui| {
            egui::Grid::new("settings_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("ssh binary").on_hover_text(
                    "Left as \"ssh\" it is found on PATH — on Windows that is normally \
                     C:\\Windows\\System32\\OpenSSH\\ssh.exe.",
                );
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut cfg.ssh_path);
                    if ui.button("…").on_hover_text("Browse for an ssh executable.").clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_file()
                    {
                        cfg.ssh_path = p.display().to_string();
                    }
                });
                ui.end_row();

                ui.label("Start-up");
                ui.vertical(|ui| {
                    ui.checkbox(&mut cfg.start_with_windows, "Start with Windows").on_hover_text(
                        "Adds TunMan to the current user's startup entries, launched hidden.",
                    );
                    ui.checkbox(&mut cfg.start_hidden, "Start minimised to tray")
                        .on_hover_text("Launch straight to the tray with no window.");
                    ui.checkbox(&mut cfg.autostart_tunnels, "Auto-start tunnels").on_hover_text(
                        "Master switch for the per-tunnel \"Start with TunMan\" setting. \
                             Off means nothing comes up on its own.",
                    );
                });
                ui.end_row();

                ui.separator();
                ui.end_row();

                ui.label("Health probe").on_hover_text(
                    "Periodically opens a real connection through each SOCKS tunnel. This is \
                     the difference between \"ssh is running\" and \"the tunnel works\" — a \
                     wedged session keeps the process alive while carrying nothing.",
                );
                ui.vertical(|ui| {
                    ui.checkbox(&mut cfg.probe_enabled, "Probe tunnels");
                    ui.add_enabled_ui(cfg.probe_enabled, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Target");
                            ui.text_edit_singleline(&mut cfg.probe_target);
                        })
                        .response
                        .on_hover_text("host:port to reach through the proxy.");
                        ui.horizontal(|ui| {
                            ui.label("Every");
                            ui.add(
                                egui::DragValue::new(&mut cfg.probe_interval_secs)
                                    .range(30..=3600)
                                    .suffix(" s"),
                            );
                        });
                    });
                });
                ui.end_row();

                ui.label("Log retention").on_hover_text(
                    "Days of log files to keep. The in-app Log tab holds the most recent \
                     50,000 lines regardless.",
                );
                ui.add(egui::DragValue::new(&mut cfg.log_retention_days).range(1..=365));
                ui.end_row();

                ui.separator();
                ui.end_row();

                ui.label("StreamArchiver").on_hover_text(
                    "Entirely optional. TunMan works standalone; this only adds a button \
                     that copies your proxy URLs into StreamArchiver's pool.",
                );
                ui.vertical(|ui| {
                    ui.checkbox(&mut cfg.sa_integration_enabled, "Show the push button");
                    ui.add_enabled_ui(cfg.sa_integration_enabled, |ui| {
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut cfg.sa_db_path);
                            if ui.button("…").clicked()
                                && let Some(p) = rfd::FileDialog::new()
                                    .add_filter("SQLite", &["sqlite3", "db"])
                                    .pick_file()
                            {
                                cfg.sa_db_path = p.display().to_string();
                            }
                        })
                        .response
                        .on_hover_text(format!(
                            "Path to streamarchiver.sqlite3. Blank uses {}.",
                            crate::sa_push::default_db_path().display()
                        ));
                    });
                });
                ui.end_row();
            });

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    save = true;
                }
                if ui
                    .button("📂 Open config folder")
                    .on_hover_text("TunMan.toml and the logs live here.")
                    .clicked()
                {
                    let dir = crate::app_paths::data_dir();
                    crate::app_paths::ensure_dir(&dir);
                    crate::ui::open_path(&dir);
                }
            });
        });

    if save {
        let autostart_changed = cfg.start_with_windows != app.cfg.settings.start_with_windows;
        app.cfg.settings = cfg;
        if autostart_changed {
            apply_autostart(app);
        }
        app.save_config();
        app.settings_open = false;
        return;
    }
    app.settings_open = open;
}

/// Register or remove the "start with Windows" entry.
fn apply_autostart(app: &mut TunManApp) {
    let Ok(exe) = std::env::current_exe() else {
        app.note("Could not find TunMan's own path");
        return;
    };
    let launcher = auto_launch::AutoLaunchBuilder::new()
        .set_app_name("TunMan")
        .set_app_path(&exe.display().to_string())
        // Launched by the OS it should go straight to the tray rather than
        // throwing a window at you every time you log in.
        .set_args(&["--hidden"])
        .build();
    let result = match launcher {
        Ok(l) if app.cfg.settings.start_with_windows => l.enable(),
        Ok(l) => l.disable(),
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => app.note(if app.cfg.settings.start_with_windows {
            "TunMan will start with Windows"
        } else {
            "TunMan will no longer start with Windows"
        }),
        Err(e) => app.note(format!("Could not change the startup entry: {e}")),
    }
}
