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

                ui.separator();
                ui.end_row();

                let enforceable = ed.draft.caps_enforceable();
                ui.label("Bandwidth caps").on_hover_text(
                    "Limits in MiB, to keep a box off its provider's bad side. Zero means \
                     no cap. Caps need metering to be enforceable — without it there are no \
                     byte counts to measure against.",
                );
                ui.vertical(|ui| {
                    if ed.draft.caps.any_set() && !enforceable {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            "⚠ Not enforced — this tunnel is not metered.",
                        )
                        .on_hover_text(
                            "Windows has no per-socket byte counter, so a cap is only \
                             possible when TunMan owns the port and counts what passes \
                             through it. Tick Meter traffic above.",
                        );
                    }
                    for w in crate::usage::Window::ALL {
                        ui.horizontal(|ui| {
                            let field = match w {
                                crate::usage::Window::Hour => &mut ed.draft.caps.hourly_mib,
                                crate::usage::Window::Week => &mut ed.draft.caps.weekly_mib,
                                crate::usage::Window::Month => &mut ed.draft.caps.monthly_mib,
                            };
                            ui.add(
                                egui::DragValue::new(field)
                                    .speed(64.0)
                                    .range(0..=u64::MAX)
                                    .suffix(" MiB"),
                            );
                            ui.label(w.label()).on_hover_text(w.hint());
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label("At the cap");
                        egui::ComboBox::from_id_salt("cap_action")
                            .selected_text(ed.draft.caps.action.label())
                            .show_ui(ui, |ui| {
                                for a in crate::usage::CapAction::ALL {
                                    ui.selectable_value(&mut ed.draft.caps.action, a, a.label())
                                        .on_hover_text(a.hint());
                                }
                            })
                            .response
                            .on_hover_text(ed.draft.caps.action.hint());
                    });
                    ui.checkbox(&mut ed.draft.caps.count_both_directions, "Count both directions")
                        .on_hover_text(
                            "Off counts only what leaves, which is what most providers bill. \
                             On counts everything through the tunnel.",
                        );
                });
                ui.end_row();

                ui.label("Country").on_hover_text(
                    "Normally measured by asking through the tunnel where it comes out. Set \
                     a two-letter code here to override that — for a tunnel that cannot be \
                     probed, or one whose provider geolocates somewhere misleading.",
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.draft.country_override)
                            .hint_text("auto")
                            .desired_width(60.0),
                    );
                    if !ed.draft.country_override.trim().is_empty() {
                        ui.label(crate::geo::flag(&ed.draft.country_override));
                    }
                });
                ui.end_row();

                ui.separator();
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

// ----------------------------------------------------------------- mounts ---

/// A mount being edited. A draft, for the same reason tunnels use one: editing
/// the live config in place would remount on every keystroke.
pub struct MountEdit {
    pub draft: crate::mounts::Mount,
    pub index: Option<usize>,
    pub original_name: String,
}

impl MountEdit {
    pub fn new(draft: crate::mounts::Mount, index: Option<usize>) -> MountEdit {
        let original_name = draft.name.clone();
        MountEdit { draft, index, original_name }
    }
}

pub fn show_mount_editor(app: &mut TunManApp, ctx: &egui::Context) {
    use crate::mounts::MountKind;

    let Some(mut ed) = app.mount_editor.take() else { return };
    let mut open = true;
    let mut save = false;
    let mut cancel = false;

    egui::Window::new(if ed.index.is_some() { "Edit mount" } else { "New mount" })
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(440.0)
        .show(ctx, |ui| {
            egui::Grid::new("mount_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Name").on_hover_text("Identifies the mount in the table and the log.");
                ui.text_edit_singleline(&mut ed.draft.name);
                ui.end_row();

                ui.label("Via").on_hover_text(crate::ui::mounts::kind_hint(ed.draft.kind));
                egui::ComboBox::from_id_salt("mount_kind")
                    .selected_text(ed.draft.kind.label())
                    .show_ui(ui, |ui| {
                        for k in MountKind::ALL {
                            ui.selectable_value(&mut ed.draft.kind, k, k.label())
                                .on_hover_text(crate::ui::mounts::kind_hint(k));
                        }
                    });
                ui.end_row();

                if ed.draft.kind == MountKind::Rclone {
                    ui.label("Remote").on_hover_text(
                        "An rclone remote and optional path, like `nas:backups`. The list \
                         comes from your own rclone config.",
                    );
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.draft.remote)
                                .desired_width(200.0)
                                .hint_text("remote:path"),
                        );
                        egui::ComboBox::from_id_salt("remote_pick")
                            .selected_text("Pick")
                            .width(80.0)
                            .show_ui(ui, |ui| {
                                if app.rclone_remotes.is_empty() {
                                    ui.weak("No remotes configured");
                                }
                                for r in &app.rclone_remotes {
                                    if ui.selectable_label(false, r).clicked() {
                                        ed.draft.remote = r.clone();
                                    }
                                }
                            });
                    });
                    ui.end_row();
                } else {
                    ui.label("User").on_hover_text("Leave blank to use your local username.");
                    ui.text_edit_singleline(&mut ed.draft.user);
                    ui.end_row();

                    ui.label("Host").on_hover_text("The ssh server to mount from.");
                    ui.text_edit_singleline(&mut ed.draft.host);
                    ui.end_row();

                    ui.label("SSH port");
                    ui.add(egui::DragValue::new(&mut ed.draft.ssh_port).range(1..=65535));
                    ui.end_row();

                    ui.label("Remote path").on_hover_text("The directory on the server.");
                    ui.text_edit_singleline(&mut ed.draft.remote_path);
                    ui.end_row();

                    ui.label("Key file").on_hover_text("Optional; blank uses your agent.");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut ed.draft.identity_file);
                        if ui.button("…").clicked()
                            && let Some(p) = rfd::FileDialog::new().pick_file()
                        {
                            ed.draft.identity_file = p.display().to_string();
                        }
                    });
                    ui.end_row();
                }

                ui.label("Mount at").on_hover_text(
                    "A free drive letter like `X:`, or an empty directory. The letter must \
                     not already be in use.",
                );
                ui.text_edit_singleline(&mut ed.draft.target);
                ui.end_row();

                ui.separator();
                ui.end_row();

                ui.label("Retry delay").on_hover_text(
                    "Seconds to wait before reconnecting after a drop. Zero uses the same \
                     doubling backoff tunnels use (5s up to 5 minutes). Set a fixed value \
                     for a server that reacts badly to being prodded — some will ban a \
                     client that reconnects the instant it drops rather than let it back in.",
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut ed.draft.retry_delay_secs)
                            .range(0..=3600)
                            .suffix(" s"),
                    );
                    if ed.draft.retry_delay_secs == 0 {
                        ui.weak("(backoff)");
                    }
                });
                ui.end_row();

                ui.label("Give up after").on_hover_text(
                    "Stop retrying after this many consecutive failures. Zero keeps trying \
                     indefinitely, which is usually what you want for a mount you rely on.",
                );
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut ed.draft.max_retries).range(0..=1000));
                    if ed.draft.max_retries == 0 {
                        ui.weak("(never)");
                    }
                });
                ui.end_row();

                ui.label("Options");
                ui.vertical(|ui| {
                    ui.checkbox(&mut ed.draft.enabled, "Enabled");
                    ui.checkbox(&mut ed.draft.auto_start, "Mount when TunMan starts");
                    ui.checkbox(&mut ed.draft.read_only, "Read-only")
                        .on_hover_text("Worth having on anything you only ever read from.");
                    if ed.draft.kind == MountKind::Rclone {
                        ui.checkbox(&mut ed.draft.vfs_cache, "Write cache").on_hover_text(
                            "Lets ordinary programs modify files. Without it rclone refuses \
                             the read-modify-write that most Windows software does \
                             constantly, and the mount reads as broken for anything but \
                             streaming.",
                        );
                    }
                });
                ui.end_row();

                ui.label("Extra args").on_hover_text("Passed to the tool verbatim.");
                let mut extra = ed.draft.extra_args.join(" ");
                if ui.text_edit_singleline(&mut extra).changed() {
                    ed.draft.extra_args = extra.split_whitespace().map(|s| s.to_string()).collect();
                }
                ui.end_row();
            });

            ui.separator();
            let (prog, args) = crate::mounts::args(
                &ed.draft,
                &app.cfg.settings.rclone_path,
                &app.cfg.settings.sshfs_path,
            );
            ui.weak("Command");
            ui.label(egui::RichText::new(format!("{prog} {}", args.join(" "))).monospace().weak());

            let problems = ed.draft.validate();
            let taken = app
                .cfg
                .mounts
                .iter()
                .enumerate()
                .any(|(i, m)| m.name == ed.draft.name && Some(i) != ed.index);
            if taken {
                ui.colored_label(ui.visuals().error_fg_color, "Another mount has that name.");
            }
            for p in &problems {
                ui.colored_label(ui.visuals().error_fg_color, p);
            }

            ui.separator();
            ui.horizontal(|ui| {
                let ok = problems.is_empty() && !taken;
                if ui.add_enabled(ok, egui::Button::new("Save")).clicked() {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });

    if save {
        if ed.index.is_some() && ed.original_name != ed.draft.name {
            app.send_mount(crate::jobs::MountCommand::Stop(ed.original_name.clone()));
            app.mount_shared.states.lock().remove(&ed.original_name);
        }
        match ed.index {
            Some(i) if i < app.cfg.mounts.len() => app.cfg.mounts[i] = ed.draft.clone(),
            _ => app.cfg.mounts.push(ed.draft.clone()),
        }
        app.save_config();
        return;
    }
    if !cancel && open {
        app.mount_editor = Some(ed);
    }
}

// ------------------------------------------------------------------ sync ----

/// A sync job being edited.
pub struct JobEdit {
    pub draft: crate::sync::SyncJob,
    pub index: Option<usize>,
    pub original_name: String,
}

impl JobEdit {
    pub fn new(draft: crate::sync::SyncJob, index: Option<usize>) -> JobEdit {
        let original_name = draft.name.clone();
        JobEdit { draft, index, original_name }
    }
}

pub fn show_job_editor(app: &mut TunManApp, ctx: &egui::Context) {
    use crate::sync::SyncMode;

    let Some(mut ed) = app.job_editor.take() else { return };
    let mut open = true;
    let mut save = false;
    let mut cancel = false;

    egui::Window::new(if ed.index.is_some() { "Edit sync job" } else { "New sync job" })
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(460.0)
        .show(ctx, |ui| {
            egui::Grid::new("job_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Name").on_hover_text("Identifies the job in the table and the log.");
                ui.text_edit_singleline(&mut ed.draft.name);
                ui.end_row();

                ui.label("Mode").on_hover_text(ed.draft.mode.hint());
                egui::ComboBox::from_id_salt("job_mode")
                    .selected_text(ed.draft.mode.label())
                    .show_ui(ui, |ui| {
                        for m in SyncMode::ALL {
                            ui.selectable_value(&mut ed.draft.mode, m, m.label())
                                .on_hover_text(m.hint());
                        }
                    });
                ui.end_row();

                let pick =
                    |ui: &mut egui::Ui, field: &mut String, remotes: &[String], salt: &str| {
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(field).desired_width(210.0));
                            egui::ComboBox::from_id_salt(salt)
                                .selected_text("Pick")
                                .width(76.0)
                                .show_ui(ui, |ui| {
                                    if remotes.is_empty() {
                                        ui.weak("No remotes configured");
                                    }
                                    for r in remotes {
                                        if ui.selectable_label(false, r).clicked() {
                                            *field = r.clone();
                                        }
                                    }
                                });
                        });
                    };

                ui.label("Source").on_hover_text(
                    "An rclone path: a remote like `nas:photos`, or a local path.",
                );
                pick(ui, &mut ed.draft.source, &app.rclone_remotes, "src_pick");
                ui.end_row();

                ui.label("Destination").on_hover_text("Where it goes. Also an rclone path.");
                pick(ui, &mut ed.draft.dest, &app.rclone_remotes, "dst_pick");
                ui.end_row();

                ui.label("Every").on_hover_text(
                    "Run automatically this often. Zero means the job only runs when you \
                     press Run.",
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut ed.draft.interval_mins)
                            .range(0..=10080)
                            .suffix(" min"),
                    );
                    if ed.draft.interval_mins == 0 {
                        ui.weak("(manual)");
                    }
                });
                ui.end_row();

                ui.separator();
                ui.end_row();

                ui.label("Safety");
                ui.vertical(|ui| {
                    ui.checkbox(&mut ed.draft.enabled, "Enabled");
                    if ed.draft.mode == SyncMode::Bisync {
                        ui.checkbox(&mut ed.draft.resync, "Establish baseline (--resync)")
                            .on_hover_text(
                                "rclone refuses to bisync without a baseline. Tick this for \
                                 the first run of a new pair; it is cleared once it works.",
                            );
                    }
                    ui.horizontal(|ui| {
                        ui.label("Skip files newer than");
                        ui.add(
                            egui::DragValue::new(&mut ed.draft.min_age_secs)
                                .range(0..=86400)
                                .suffix(" s"),
                        );
                    })
                    .response
                    .on_hover_text(
                        "Avoids copying a file that is still being written. Zero copies \
                         everything.",
                    );
                });
                ui.end_row();

                ui.label("Backup dir").on_hover_text(
                    "Files that would be deleted or replaced are moved here instead. The \
                     single most useful safety net for a mode that deletes — set it and a \
                     mistake is recoverable.",
                );
                ui.text_edit_singleline(&mut ed.draft.backup_dir);
                ui.end_row();

                ui.label("Bandwidth limit")
                    .on_hover_text("An rclone bandwidth ceiling like `8M`. Empty means unlimited.");
                ui.add(
                    egui::TextEdit::singleline(&mut ed.draft.bwlimit)
                        .hint_text("unlimited")
                        .desired_width(90.0),
                );
                ui.end_row();

                ui.label("Extra args").on_hover_text("Passed to rclone verbatim.");
                let mut extra = ed.draft.extra_args.join(" ");
                if ui.text_edit_singleline(&mut extra).changed() {
                    ed.draft.extra_args = extra.split_whitespace().map(|s| s.to_string()).collect();
                }
                ui.end_row();
            });

            if ed.draft.mode.destructive() {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().warn_fg_color, "⚠");
                    ui.colored_label(ui.visuals().warn_fg_color, ed.draft.mode.hint());
                });
                if ed.draft.backup_dir.trim().is_empty() {
                    ui.weak(
                        "Consider setting a backup dir, so anything this deletes is moved \
                         aside rather than destroyed.",
                    );
                }
            }

            ui.separator();
            ui.weak("Command");
            ui.label(
                egui::RichText::new(format!(
                    "{} {}",
                    app.cfg.settings.rclone_path,
                    crate::sync::args(&ed.draft, false).join(" ")
                ))
                .monospace()
                .weak(),
            );

            let problems = ed.draft.validate();
            let taken = app
                .cfg
                .jobs
                .iter()
                .enumerate()
                .any(|(i, j)| j.name == ed.draft.name && Some(i) != ed.index);
            if taken {
                ui.colored_label(ui.visuals().error_fg_color, "Another job has that name.");
            }
            for p in &problems {
                ui.colored_label(ui.visuals().error_fg_color, p);
            }

            ui.separator();
            ui.horizontal(|ui| {
                let ok = problems.is_empty() && !taken;
                if ui.add_enabled(ok, egui::Button::new("Save")).clicked() {
                    save = true;
                }
                if ui
                    .add_enabled(ok, egui::Button::new("Save & dry run"))
                    .on_hover_text(
                        "Save, then immediately show what this job would do without doing \
                         any of it.",
                    )
                    .clicked()
                {
                    save = true;
                    app.pending_dry_run = Some(ed.draft.name.clone());
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });

    if save {
        if ed.index.is_some() && ed.original_name != ed.draft.name {
            app.sync_shared.states.lock().remove(&ed.original_name);
        }
        match ed.index {
            Some(i) if i < app.cfg.jobs.len() => app.cfg.jobs[i] = ed.draft.clone(),
            _ => app.cfg.jobs.push(ed.draft.clone()),
        }
        app.save_config();
        if let Some(name) = app.pending_dry_run.take() {
            app.send_sync(crate::jobs::SyncCommand::Run { name: name.clone(), dry_run: true });
            app.selected_job = Some(name);
            app.tab = crate::ui::Tab::Sync;
        }
        return;
    }
    if !cancel && open {
        app.mount_editor = app.mount_editor.take();
        app.job_editor = Some(ed);
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

                ui.label("rclone").on_hover_text(
                    "Used by mounts and every sync job. Left as \"rclone\" it resolves on                      PATH.",
                );
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut cfg.rclone_path);
                    if ui.button("…").clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_file()
                    {
                        cfg.rclone_path = p.display().to_string();
                    }
                });
                ui.end_row();

                ui.label("sshfs").on_hover_text(
                    "Only needed for sshfs mounts. On Windows this comes from sshfs-win, a                      separate install — an rclone remote on the sftp backend mounts an ssh                      server the same way without it.",
                );
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut cfg.sshfs_path);
                    if ui.button("…").clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_file()
                    {
                        cfg.sshfs_path = p.display().to_string();
                    }
                    if !crate::mounts::sshfs_candidates().iter().any(|p| p.exists())
                        && cfg.sshfs_path == "sshfs"
                    {
                        ui.colored_label(ui.visuals().warn_fg_color, "?")
                            .on_hover_text("sshfs-win does not appear to be installed here.");
                    }
                });
                ui.end_row();

                ui.separator();
                ui.end_row();

                ui.label("Start-up");
                ui.vertical(|ui| {
                    ui.checkbox(&mut cfg.start_with_windows, "Start with Windows").on_hover_text(
                        "Adds TunMan to the current user's startup entries, launched hidden.",
                    );
                    ui.checkbox(&mut cfg.start_hidden, "Start minimised to tray")
                        .on_hover_text("Launch straight to the tray with no window.");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut cfg.start_menu_shortcut, "Start Menu shortcut")
                            .on_hover_text(
                                "Keep a shortcut in your Start Menu, rewritten on every \
                                 launch to point at whatever binary is actually running. \
                                 That is the useful part: TunMan runs from wherever it was \
                                 built, and a shortcut left pointing at a moved exe fails \
                                 silently from the Start Menu while the app works fine \
                                 launched directly.",
                            );
                        if crate::platform::start_menu_shortcut_exists() {
                            ui.weak("✔").on_hover_text(
                                crate::platform::start_menu_shortcut_path()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_default(),
                            );
                        }
                    });
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
        let shortcut_changed = cfg.start_menu_shortcut != app.cfg.settings.start_menu_shortcut;
        app.cfg.settings = cfg;
        if autostart_changed {
            apply_autostart(app);
        }
        // Unticking has to actually delete it. Merely stopping the refresh
        // would leave the shortcut sitting in the Start Menu forever, which is
        // not what unticking a box called "Start Menu shortcut" means.
        if shortcut_changed {
            if app.cfg.settings.start_menu_shortcut {
                match crate::platform::create_start_menu_shortcut() {
                    Ok(p) => app.note(format!("Start Menu shortcut created at {}", p.display())),
                    Err(e) => app.note(format!("Could not create the shortcut: {e}")),
                }
            } else {
                match crate::platform::remove_start_menu_shortcut() {
                    Ok(()) => app.note("Start Menu shortcut removed"),
                    Err(e) => app.note(format!("Could not remove the shortcut: {e}")),
                }
            }
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
