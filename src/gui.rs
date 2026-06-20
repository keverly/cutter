use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use eframe::egui;
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};

use crate::cli::ClaudeMode;
use crate::commands;
use crate::config::{config_dir, expand_tilde, Base, Config};
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

/// The target of a pending "are you sure?" confirmation.
#[derive(Clone)]
enum RemoveTarget {
    Workspace(String),
    Base(String),
}

/// A user intent collected during a UI pass, applied after rendering so the
/// borrow of `self` from the panel/window closures has ended.
enum PendingAction {
    CreateBase { name: String, paths: Vec<String> },
    RemoveBase(String),
    CreateWorkspace { name: String, base: String },
    RemoveWorkspace(String),
}

/// A long-running operation (create/remove) executing on a worker thread.
struct RunningJob {
    label: String,
}

/// The result of a worker job, sent back to the UI thread.
struct JobOutcome {
    ok: bool,
    message: String,
}

/// A success/error banner shown after the most recent job finishes.
struct StatusMsg {
    ok: bool,
    text: String,
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

    // New-base form
    show_new_base: bool,
    new_base_name: String,
    new_base_repos: Vec<String>,
    new_base_manual_path: String,

    // New-workspace form
    show_new_workspace: bool,
    new_ws_name: String,
    new_ws_base: Option<String>,

    // Pending "are you sure?" for a destructive action.
    confirm_remove: Option<RemoveTarget>,

    // Background work. Create/remove shell out to git (fetch can be slow), so
    // they run off the UI thread; `job_rx` delivers the outcome back.
    job: Option<RunningJob>,
    job_rx: Option<Receiver<JobOutcome>>,
    status: Option<StatusMsg>,

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
            show_new_base: false,
            new_base_name: String::new(),
            new_base_repos: Vec::new(),
            new_base_manual_path: String::new(),
            show_new_workspace: false,
            new_ws_name: String::new(),
            new_ws_base: None,
            confirm_remove: None,
            job: None,
            job_rx: None,
            status: None,
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

    /// Spawn `op` on a worker thread, wake the UI when it finishes, and surface
    /// the result as a status banner. No-op if a job is already running.
    fn start_job<F>(&mut self, ctx: &egui::Context, label: String, op: F)
    where
        F: FnOnce() -> std::result::Result<String, String> + Send + 'static,
    {
        if self.job.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let outcome = match op() {
                Ok(message) => JobOutcome { ok: true, message },
                Err(message) => JobOutcome { ok: false, message },
            };
            let _ = tx.send(outcome);
            ctx.request_repaint();
        });
        self.job = Some(RunningJob { label });
        self.job_rx = Some(rx);
        self.status = None;
    }

    /// Turn a collected [`PendingAction`] into a background job.
    fn dispatch(&mut self, ctx: &egui::Context, action: PendingAction) {
        match action {
            PendingAction::CreateBase { name, paths } => {
                let label = format!("Creating base '{name}'…");
                let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
                let display = name.clone();
                self.start_job(ctx, label, move || {
                    commands::base::add(&name, &paths).map_err(|e| e.to_string())?;
                    Ok(format!("Base '{display}' created"))
                });
            }
            PendingAction::RemoveBase(name) => {
                let label = format!("Removing base '{name}'…");
                let display = name.clone();
                self.start_job(ctx, label, move || {
                    commands::base::remove(&name).map_err(|e| e.to_string())?;
                    Ok(format!("Base '{display}' removed"))
                });
            }
            PendingAction::CreateWorkspace { name, base } => {
                let label = format!("Creating workspace '{name}'…");
                // Optimistically select it; reload keeps the selection if it landed.
                self.selected = Some(name.clone());
                let display = name.clone();
                self.start_job(ctx, label, move || {
                    commands::create::run(Some(&name), Some(&base), false, ClaudeMode::None)
                        .map_err(|e| e.to_string())?;
                    Ok(format!("Workspace '{display}' created"))
                });
            }
            PendingAction::RemoveWorkspace(name) => {
                let label = format!("Removing workspace '{name}'…");
                let display = name.clone();
                self.start_job(ctx, label, move || {
                    commands::remove::run(&name, false).map_err(|e| e.to_string())?;
                    Ok(format!("Workspace '{display}' removed"))
                });
            }
        }
    }

    fn workspaces_ui(&mut self, ctx: &egui::Context) {
        let job_active = self.job.is_some();
        let mut want_remove_ws: Option<String> = None;

        egui::SidePanel::left("workspace_list")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Workspaces ({})", self.workspaces.len()))
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!job_active, egui::Button::new("➕ New"))
                            .on_hover_text("Create a workspace")
                            .clicked()
                        {
                            self.open_new_workspace();
                        }
                    });
                });
                ui.separator();

                if let Some(err) = &self.workspaces_error {
                    ui.colored_label(egui::Color32::RED, err);
                    return;
                }
                if self.workspaces.is_empty() {
                    ui.add_space(8.0);
                    ui.label("No workspaces yet.");
                    ui.label(egui::RichText::new("Use ➕ New to create one.").weak());
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
            Some(ws) => Self::workspace_details(ui, ws, job_active, &mut want_remove_ws),
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("Select a workspace").weak());
                });
            }
        });

        if let Some(name) = want_remove_ws {
            self.confirm_remove = Some(RemoveTarget::Workspace(name));
        }
    }

    fn workspace_details(
        ui: &mut egui::Ui,
        ws: &WorkspaceConfig,
        job_active: bool,
        want_remove: &mut Option<String>,
    ) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(&ws.workspace.name);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!job_active, egui::Button::new("🗑 Remove"))
                        .on_hover_text("Remove worktrees, branches, and files")
                        .clicked()
                    {
                        *want_remove = Some(ws.workspace.name.clone());
                    }
                });
            });
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
        let job_active = self.job.is_some();
        let mut open_new_base = false;
        let mut want_remove_base: Option<String> = None;

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
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Bases ({})", self.bases.len())).strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!job_active, egui::Button::new("➕ New base"))
                            .clicked()
                        {
                            open_new_base = true;
                        }
                    });
                });
                ui.separator();

                if self.bases.is_empty() {
                    ui.add_space(6.0);
                    ui.label("No bases configured.");
                    ui.label(
                        egui::RichText::new("Use ➕ New base to define one from local repos.")
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
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_enabled(!job_active, egui::Button::new("Remove"))
                                        .on_hover_text("Delete this base definition")
                                        .clicked()
                                    {
                                        want_remove_base = Some(name.clone());
                                    }
                                },
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
                                    ui.label(egui::RichText::new(format!("[from {bf}]")).weak());
                                }
                            });
                        }
                    });
                }
            });
        });

        if open_new_base {
            self.show_new_base = true;
        }
        if let Some(name) = want_remove_base {
            self.confirm_remove = Some(RemoveTarget::Base(name));
        }
    }

    fn open_new_workspace(&mut self) {
        self.show_new_workspace = true;
        if self.new_ws_base.is_none() {
            self.new_ws_base = self.bases.keys().next().cloned();
        }
    }

    /// Modal form for defining a new base from one or more local git repos.
    fn new_base_window(&mut self, ctx: &egui::Context, action: &mut Option<PendingAction>) {
        if !self.show_new_base {
            return;
        }
        let mut close = false;

        egui::Window::new("New base")
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Name").strong());
                ui.text_edit_singleline(&mut self.new_base_name);

                ui.add_space(8.0);
                ui.label(egui::RichText::new("Repositories").strong());
                ui.label(
                    egui::RichText::new("Each must be a local git repository.").weak(),
                );

                ui.horizontal(|ui| {
                    if ui.button("Browse…").clicked() {
                        if let Some(paths) = rfd::FileDialog::new()
                            .set_title("Select repositories")
                            .pick_folders()
                        {
                            for p in paths {
                                let s = p.to_string_lossy().to_string();
                                if !self.new_base_repos.contains(&s) {
                                    self.new_base_repos.push(s);
                                }
                            }
                        }
                    }
                });

                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.new_base_manual_path)
                            .hint_text("…or type a path and press Add")
                            .desired_width(280.0),
                    );
                    let submit = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Add").clicked() || submit)
                        && !self.new_base_manual_path.trim().is_empty()
                    {
                        let p = expand_tilde(self.new_base_manual_path.trim());
                        let s = p.to_string_lossy().to_string();
                        if !self.new_base_repos.contains(&s) {
                            self.new_base_repos.push(s);
                        }
                        self.new_base_manual_path.clear();
                    }
                });

                if !self.new_base_repos.is_empty() {
                    ui.add_space(4.0);
                    let mut remove_idx: Option<usize> = None;
                    for (i, p) in self.new_base_repos.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.small_button("✕").clicked() {
                                remove_idx = Some(i);
                            }
                            ui.label(egui::RichText::new(p.as_str()).monospace());
                        });
                    }
                    if let Some(i) = remove_idx {
                        self.new_base_repos.remove(i);
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    let can_create = !self.new_base_name.trim().is_empty()
                        && !self.new_base_repos.is_empty()
                        && self.job.is_none();
                    if ui
                        .add_enabled(can_create, egui::Button::new("Create base"))
                        .clicked()
                    {
                        *action = Some(PendingAction::CreateBase {
                            name: self.new_base_name.trim().to_string(),
                            paths: self.new_base_repos.clone(),
                        });
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            self.show_new_base = false;
            self.new_base_name.clear();
            self.new_base_repos.clear();
            self.new_base_manual_path.clear();
        }
    }

    /// Modal form for creating a new workspace from an existing base.
    fn new_workspace_window(&mut self, ctx: &egui::Context, action: &mut Option<PendingAction>) {
        if !self.show_new_workspace {
            return;
        }
        let mut close = false;
        // Owned copy so no borrow of `self.new_ws_name` is held across the
        // closures below that also mutate `self`.
        let name = self.new_ws_name.trim().to_string();
        let name_ok = !name.is_empty() && !name.chars().any(|c| c.is_whitespace());

        egui::Window::new("New workspace")
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Name").strong());
                ui.text_edit_singleline(&mut self.new_ws_name);
                if !self.new_ws_name.is_empty() && !name_ok {
                    ui.colored_label(egui::Color32::RED, "Name cannot contain whitespace.");
                }

                ui.add_space(8.0);
                ui.label(egui::RichText::new("Base").strong());
                if self.bases.is_empty() {
                    ui.colored_label(
                        egui::Color32::RED,
                        "No bases configured. Create a base first (Settings ▸ New base).",
                    );
                } else {
                    let current = self
                        .new_ws_base
                        .clone()
                        .unwrap_or_else(|| "Select a base".to_string());
                    let names: Vec<String> = self.bases.keys().cloned().collect();
                    egui::ComboBox::from_id_salt("new_ws_base")
                        .selected_text(current)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for n in names {
                                let selected = self.new_ws_base.as_deref() == Some(n.as_str());
                                if ui.selectable_label(selected, &n).clicked() {
                                    self.new_ws_base = Some(n);
                                }
                            }
                        });
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    let can_create =
                        name_ok && self.new_ws_base.is_some() && self.job.is_none();
                    if ui
                        .add_enabled(can_create, egui::Button::new("Create"))
                        .clicked()
                    {
                        *action = Some(PendingAction::CreateWorkspace {
                            name: name.clone(),
                            base: self.new_ws_base.clone().unwrap(),
                        });
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            self.show_new_workspace = false;
            self.new_ws_name.clear();
            self.new_ws_base = None;
        }
    }

    /// Confirmation dialog for the destructive actions (remove base/workspace).
    fn confirm_window(&mut self, ctx: &egui::Context, action: &mut Option<PendingAction>) {
        let Some(target) = self.confirm_remove.clone() else {
            return;
        };
        let mut close = false;

        let (title, message) = match &target {
            RemoveTarget::Workspace(n) => (
                "Remove workspace?",
                format!(
                    "Remove workspace '{n}'?\n\nThis removes its worktrees, deletes their \
                     branches where possible, and deletes the workspace directory."
                ),
            ),
            RemoveTarget::Base(n) => (
                "Remove base?",
                format!(
                    "Remove base '{n}'?\n\nThis only deletes the base definition. Your repos \
                     and any existing workspaces are left untouched."
                ),
            ),
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    let remove = egui::Button::new(
                        egui::RichText::new("Remove").color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(0xb0, 0x30, 0x30));
                    if ui.add(remove).clicked() {
                        *action = Some(match &target {
                            RemoveTarget::Workspace(n) => {
                                PendingAction::RemoveWorkspace(n.clone())
                            }
                            RemoveTarget::Base(n) => PendingAction::RemoveBase(n.clone()),
                        });
                        close = true;
                    }
                });
            });

        if close {
            self.confirm_remove = None;
        }
    }
}

impl eframe::App for CutterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut do_refresh = false;

        // A finished background job: surface its outcome and re-read from disk.
        if let Some(rx) = &self.job_rx {
            if let Ok(outcome) = rx.try_recv() {
                self.job = None;
                self.job_rx = None;
                self.status = Some(StatusMsg {
                    ok: outcome.ok,
                    text: outcome.message,
                });
                do_refresh = true;
            }
        }

        // Coalesce any filesystem-change ticks since the last frame.
        if let Some(rx) = &self.fs_rx {
            while rx.try_recv().is_ok() {
                do_refresh = true;
            }
        }

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

        // Status bar: spinner while a job runs, else the last result.
        let mut dismiss = false;
        if self.job.is_some() || self.status.is_some() {
            egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if let Some(job) = &self.job {
                        ui.spinner();
                        ui.label(&job.label);
                    } else if let Some(status) = &self.status {
                        let color = if status.ok {
                            egui::Color32::from_rgb(0x2e, 0x7d, 0x32)
                        } else {
                            egui::Color32::from_rgb(0xc6, 0x28, 0x28)
                        };
                        ui.colored_label(color, if status.ok { "✓" } else { "✗" });
                        ui.colored_label(color, &status.text);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("✕").clicked() {
                                    dismiss = true;
                                }
                            },
                        );
                    }
                });
                ui.add_space(4.0);
            });
        }
        if dismiss {
            self.status = None;
        }

        // Reload once, after the top panel's Refresh button has had a chance to
        // request it, so a manual refresh takes effect this frame.
        if do_refresh {
            self.reload();
        }

        match self.tab {
            Tab::Workspaces => self.workspaces_ui(ctx),
            Tab::Settings => self.settings_ui(ctx),
        }

        // Floating forms collect intent into `action`, applied once their
        // borrows of `self` have ended.
        let mut action: Option<PendingAction> = None;
        self.new_base_window(ctx, &mut action);
        self.new_workspace_window(ctx, &mut action);
        self.confirm_window(ctx, &mut action);
        if let Some(action) = action {
            self.dispatch(ctx, action);
        }
    }
}

/// A label/value pair as one row of a two-column [`egui::Grid`].
fn meta_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(egui::RichText::new(label).strong());
    ui.label(value);
    ui.end_row();
}
