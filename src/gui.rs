use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use eframe::egui;
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};

use crate::config::{config_dir, Base, Config};
use crate::workspace::WorkspaceConfig;

/// Launch the standalone Cutter GUI window.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Cutter")
            .with_inner_size([820.0, 560.0])
            .with_min_inner_size([560.0, 360.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Cutter",
        options,
        Box::new(|cc| Ok(Box::new(CutterApp::new(&cc.egui_ctx)))),
    )
}

/// Watch `~/.config/cutter` (config.toml + workspaces/*.toml) and signal the UI
/// to reload when anything changes, so the list stays current without a manual
/// refresh. Events are debounced to coalesce the burst of writes that a single
/// `cutter create` produces into one reload.
fn spawn_watcher(ctx: egui::Context) -> Option<(Debouncer<RecommendedWatcher>, Receiver<()>)> {
    let dir = config_dir().ok()?;
    // Watching fails if the directory doesn't exist yet (no workspaces created).
    let _ = std::fs::create_dir_all(&dir);

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(
        Duration::from_millis(250),
        move |_res: DebounceEventResult| {
            // Don't reload on the watcher thread; just wake the UI, which drains
            // the channel and reloads on the main thread.
            let _ = tx.send(());
            ctx.request_repaint();
        },
    )
    .ok()?;

    debouncer
        .watcher()
        .watch(&dir, RecursiveMode::Recursive)
        .ok()?;

    Some((debouncer, rx))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Workspaces,
    Settings,
}

struct CutterApp {
    tab: Tab,

    // Workspaces
    workspaces: Vec<WorkspaceConfig>,
    workspaces_error: Option<String>,
    selected: Option<String>,

    // Settings / config
    workspace_root: String,
    default_branch_from: String,
    bases: BTreeMap<String, Base>,
    config_error: Option<String>,

    // Filesystem watching. The debouncer is kept alive only so it keeps
    // watching; `fs_rx` receives a tick whenever the config dir changes.
    _debouncer: Option<Debouncer<RecommendedWatcher>>,
    fs_rx: Option<Receiver<()>>,
}

impl CutterApp {
    fn new(ctx: &egui::Context) -> Self {
        let mut app = Self {
            tab: Tab::Workspaces,
            workspaces: Vec::new(),
            workspaces_error: None,
            selected: None,
            workspace_root: String::new(),
            default_branch_from: String::new(),
            bases: BTreeMap::new(),
            config_error: None,
            _debouncer: None,
            fs_rx: None,
        };
        app.reload();
        if let Some((debouncer, rx)) = spawn_watcher(ctx.clone()) {
            app._debouncer = Some(debouncer);
            app.fs_rx = Some(rx);
        }
        app
    }

    /// Re-read workspaces and config from disk.
    fn reload(&mut self) {
        match WorkspaceConfig::list_all() {
            Ok(ws) => {
                self.workspaces = ws;
                self.workspaces_error = None;
            }
            Err(e) => {
                self.workspaces.clear();
                self.workspaces_error = Some(e.to_string());
            }
        }

        // Preserve the current selection if it still exists, else select the first.
        let still_present = self
            .selected
            .as_ref()
            .is_some_and(|name| self.workspaces.iter().any(|w| &w.workspace.name == name));
        if !still_present {
            self.selected = self.workspaces.first().map(|w| w.workspace.name.clone());
        }

        match Config::load() {
            Ok(cfg) => {
                self.workspace_root = cfg.settings.workspace_root.clone();
                self.default_branch_from = cfg.settings.default_branch_from.clone();
                self.bases = cfg.bases;
                self.config_error = None;
            }
            Err(e) => self.config_error = Some(e.to_string()),
        }
    }

    fn selected_workspace(&self) -> Option<&WorkspaceConfig> {
        let name = self.selected.as_ref()?;
        self.workspaces.iter().find(|w| &w.workspace.name == name)
    }

    fn workspaces_ui(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("workspace_list")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("Workspaces ({})", self.workspaces.len())).strong(),
                );
                ui.separator();

                if let Some(err) = &self.workspaces_error {
                    ui.colored_label(egui::Color32::RED, err);
                    return;
                }
                if self.workspaces.is_empty() {
                    ui.add_space(8.0);
                    ui.label("No workspaces yet.");
                    ui.label(
                        egui::RichText::new("Create one with `cutter create`.").weak(),
                    );
                    return;
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Snapshot names first so we can mutate `selected` while iterating.
                    let names: Vec<String> =
                        self.workspaces.iter().map(|w| w.workspace.name.clone()).collect();
                    for name in names {
                        let is_selected = self.selected.as_deref() == Some(name.as_str());
                        if ui.selectable_label(is_selected, name.as_str()).clicked() {
                            self.selected = Some(name);
                        }
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| match self.selected_workspace() {
            Some(ws) => Self::workspace_details(ui, ws),
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("Select a workspace").weak());
                });
            }
        });
    }

    fn workspace_details(ui: &mut egui::Ui, ws: &WorkspaceConfig) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(6.0);
            ui.heading(&ws.workspace.name);
            ui.add_space(4.0);

            egui::Grid::new("ws_meta")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    meta_row(ui, "Base", &ws.workspace.base);
                    meta_row(ui, "Branch", &ws.workspace.branch);
                    meta_row(
                        ui,
                        "Created",
                        &ws.workspace.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
                    );
                    ui.label(egui::RichText::new("Path").strong());
                    ui.label(egui::RichText::new(ws.workspace.path.as_str()).monospace());
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.label(egui::RichText::new(format!("Repos ({})", ws.repos.len())).strong());
            ui.separator();

            for repo in &ws.repos {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(repo.name.as_str()).strong());
                egui::Grid::new(format!("repo_{}", repo.name))
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        meta_row(ui, "Branch", &repo.branch);
                        ui.label(egui::RichText::new("Source").weak());
                        ui.label(egui::RichText::new(repo.source.as_str()).monospace());
                        ui.end_row();
                        ui.label(egui::RichText::new("Worktree").weak());
                        ui.label(egui::RichText::new(repo.worktree_path.as_str()).monospace());
                        ui.end_row();
                    });
            }
        });
    }

    fn settings_ui(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(6.0);
                ui.heading("Settings");
                ui.add_space(4.0);

                if let Some(err) = &self.config_error {
                    ui.colored_label(egui::Color32::RED, err);
                    return;
                }

                egui::Grid::new("settings_meta")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Workspace root").strong());
                        ui.label(egui::RichText::new(self.workspace_root.as_str()).monospace());
                        ui.end_row();
                        meta_row(ui, "Default branch from", &self.default_branch_from);
                    });

                ui.add_space(12.0);
                ui.label(egui::RichText::new(format!("Bases ({})", self.bases.len())).strong());
                ui.separator();

                if self.bases.is_empty() {
                    ui.add_space(6.0);
                    ui.label("No bases configured.");
                    ui.label(
                        egui::RichText::new("Add one with `cutter base add <name> <paths...>`.")
                            .weak(),
                    );
                }

                for (name, base) in &self.bases {
                    ui.add_space(8.0);
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(name.as_str()).heading());
                            ui.label(
                                egui::RichText::new(format!("· {} repo(s)", base.repos.len()))
                                    .weak(),
                            );
                        });

                        let branch_from = base.branch_from.clone().unwrap_or_else(|| {
                            format!("{} (inherited)", self.default_branch_from)
                        });
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("branch from:").weak());
                            ui.label(branch_from);
                        });

                        if !base.copy_files.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new("copy files:").weak());
                                ui.label(
                                    egui::RichText::new(base.copy_files.join(", ")).monospace(),
                                );
                            });
                        }

                        ui.add_space(4.0);
                        for repo in &base.repos {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new(format!("{}:", repo.name)).strong());
                                ui.label(egui::RichText::new(repo.path.as_str()).monospace());
                                if let Some(bf) = &repo.branch_from {
                                    ui.label(
                                        egui::RichText::new(format!("[from {bf}]")).weak(),
                                    );
                                }
                            });
                        }
                    });
                }
            });
        });
    }
}

impl eframe::App for CutterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut do_refresh = false;

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Cutter");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Workspaces, "Workspaces");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳ Refresh").clicked() {
                        do_refresh = true;
                    }
                });
            });
            ui.add_space(4.0);
        });

        // Coalesce any filesystem-change ticks since the last frame into a
        // single reload.
        if let Some(rx) = &self.fs_rx {
            while rx.try_recv().is_ok() {
                do_refresh = true;
            }
        }

        if do_refresh {
            self.reload();
        }

        match self.tab {
            Tab::Workspaces => self.workspaces_ui(ctx),
            Tab::Settings => self.settings_ui(ctx),
        }
    }
}

/// A label/value pair as one row of a two-column [`egui::Grid`].
fn meta_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(egui::RichText::new(label).strong());
    ui.label(value);
    ui.end_row();
}
