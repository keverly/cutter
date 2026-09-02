use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use eframe::egui;
use egui_term::{PtyEvent, TerminalBackend, TerminalView};
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};

use crate::ai_link;
use crate::cli::ClaudeMode;
use crate::commands;
use crate::config::{config_dir, expand_tilde, Base, Config, RepoRef};
use crate::git;
use crate::pr;
use crate::session::{self, SessionRecord, SessionState, WorkspaceStatus};
use crate::term_input::{self, MouseState};
use crate::window_manager::{self, WindowInfo};
use crate::workspace::{LinkedWindow, WorkspaceConfig};

/// Claude "running" amber and "waiting for input" green, shared by the list
/// icons, header chip, and details section.
const RUNNING_COLOR: egui::Color32 = egui::Color32::from_rgb(0xff, 0xb0, 0x2e);
const WAITING_COLOR: egui::Color32 = egui::Color32::from_rgb(0x3f, 0xb9, 0x50);

/// Phosphor icon glyphs for each Claude status: a spinner ring for "running",
/// a chat bubble with dots ("your turn to reply") for "waiting for input".
const RUNNING_ICON: &str = egui_phosphor::regular::SPINNER_GAP;
const WAITING_ICON: &str = egui_phosphor::regular::CHAT_CIRCLE_DOTS;

/// Diff line colours: added green, removed red, hunk-header blue. Context and
/// file headers use the default text colour (headers bolded).
const DIFF_ADD_COLOR: egui::Color32 = egui::Color32::from_rgb(0x3f, 0xb9, 0x50);
const DIFF_DEL_COLOR: egui::Color32 = egui::Color32::from_rgb(0xf8, 0x51, 0x49);
const DIFF_HUNK_COLOR: egui::Color32 = egui::Color32::from_rgb(0x58, 0xa6, 0xff);

/// Icons for the collapse toggle: point the carets the way the window moves.
const COLLAPSE_ICON: &str = egui_phosphor::regular::CARET_DOUBLE_LEFT;
const EXPAND_ICON: &str = egui_phosphor::regular::CARET_DOUBLE_RIGHT;

/// Window width, in points, of the collapsed list-only view.
const COLLAPSED_WIDTH: f32 = 300.0;

/// PR-status chip colours: draft grey, open green, merged purple.
const PR_DRAFT_COLOR: egui::Color32 = egui::Color32::from_rgb(0x8b, 0x94, 0x9e);
const PR_OPEN_COLOR: egui::Color32 = egui::Color32::from_rgb(0x3f, 0xb9, 0x50);
const PR_MERGED_COLOR: egui::Color32 = egui::Color32::from_rgb(0x8b, 0x5c, 0xf6);

/// Launch the standalone Cutter GUI window.
pub fn run() -> eframe::Result<()> {
    // Embedded terminals inherit this process's environment. Launched from
    // Finder, Cutter has no TERM, so shell programs (e.g. Claude Code) assume a
    // color-less terminal and emit no color. Advertise the emulator's real
    // capabilities — it's an xterm-class, truecolor VT (alacritty's engine, and
    // the widget renders per-cell ANSI colors). alacritty/egui_term don't set
    // these themselves. `xterm-256color` is chosen over `alacritty` because its
    // terminfo is present on virtually every system.
    std::env::set_var("TERM", "xterm-256color");
    std::env::set_var("COLORTERM", "truecolor");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Cutter")
            .with_inner_size([820.0, 560.0])
            // Low enough to let the collapsed list-only view shrink to
            // `COLLAPSED_WIDTH`; the expanded view is never this small in
            // practice, and nothing breaks if the user drags it there.
            .with_min_inner_size([260.0, 320.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Cutter",
        options,
        Box::new(|cc| {
            install_icon_font(&cc.egui_ctx);
            Ok(Box::new(CutterApp::new(&cc.egui_ctx)))
        }),
    )
}

/// Merge the Phosphor icon font into egui's fonts so the status glyphs render.
/// It's added only as a fallback in the Proportional family, so the default
/// text font and the embedded terminal's monospace rendering are untouched.
fn install_icon_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
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

/// Which pane the selected workspace shows: an embedded terminal (the default,
/// so clicking a workspace lands on a live shell), its details (repos, linked
/// windows, remove), or a git diff of one of its repos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceView {
    Terminal,
    Details,
    Diff,
}

/// How one line of a unified diff should be coloured. Classified once when a
/// fetched diff arrives so per-frame rendering is just a table lookup.
#[derive(Clone, Copy)]
enum DiffLineKind {
    /// `diff --git`, `+++`/`---`, `index`, `new file`, … — bolded default text.
    FileHeader,
    /// A `@@ … @@` hunk header.
    Hunk,
    /// An added line (`+`).
    Add,
    /// A removed line (`-`).
    Del,
    /// An unchanged context line.
    Context,
}

/// A fetched, pre-classified diff ready to render: the ref it was diffed against
/// (for the header), a change summary, and its lines. Built on the UI thread from
/// a [`git::RepoDiff`] so repaints don't re-split/re-classify a potentially large
/// diff each frame.
struct DiffView {
    base_label: String,
    lines: Vec<(DiffLineKind, String)>,
    /// Summary counts for the header: files touched, lines added, lines removed.
    files: usize,
    additions: usize,
    deletions: usize,
}

impl DiffView {
    fn from_raw(raw: git::RepoDiff) -> Self {
        let lines: Vec<(DiffLineKind, String)> = raw
            .text
            .lines()
            .map(|l| (classify_diff_line(l), l.to_string()))
            .collect();

        // Tally the summary from the classified lines: one file per `diff --git`
        // header, and add/remove counts (the `+++`/`---` file headers classify as
        // FileHeader, so they're never miscounted here).
        let mut files = 0;
        let mut additions = 0;
        let mut deletions = 0;
        for (kind, text) in &lines {
            match kind {
                DiffLineKind::Add => additions += 1,
                DiffLineKind::Del => deletions += 1,
                DiffLineKind::FileHeader if text.starts_with("diff --git") => files += 1,
                _ => {}
            }
        }

        DiffView {
            base_label: raw.base_label,
            lines,
            files,
            additions,
            deletions,
        }
    }
}

/// Classify a unified-diff line by its leading marker. Order matters: `+++`/`---`
/// file headers must be caught before the single-char `+`/`-` add/remove tests.
fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with("diff --git")
        || line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("index ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("rename ")
        || line.starts_with("similarity ")
        || line.starts_with("old mode")
        || line.starts_with("new mode")
        || line.starts_with("Binary files")
    {
        DiffLineKind::FileHeader
    } else if line.starts_with("@@") {
        DiffLineKind::Hunk
    } else if line.starts_with('+') {
        DiffLineKind::Add
    } else if line.starts_with('-') {
        DiffLineKind::Del
    } else {
        DiffLineKind::Context
    }
}

/// The full-row background tint for a diff line, or `None` for context lines.
/// Kept faint (low alpha) so the coloured foreground text stays readable and the
/// tint reads the same over light or dark panels.
fn diff_line_bg(kind: DiffLineKind) -> Option<egui::Color32> {
    match kind {
        DiffLineKind::Add => Some(egui::Color32::from_rgba_unmultiplied(0x3f, 0xb9, 0x50, 26)),
        DiffLineKind::Del => Some(egui::Color32::from_rgba_unmultiplied(0xf8, 0x51, 0x49, 26)),
        DiffLineKind::Hunk => Some(egui::Color32::from_rgba_unmultiplied(0x58, 0xa6, 0xff, 22)),
        // A neutral band behind each file's header block visually separates files.
        DiffLineKind::FileHeader => {
            Some(egui::Color32::from_rgba_unmultiplied(0x88, 0x88, 0x88, 22))
        }
        DiffLineKind::Context => None,
    }
}

/// Render one diff line: a full-width background tint (fixed to the viewport, so
/// it stays put during horizontal scroll) with non-wrapping monospace text on
/// top. Long lines extend past the viewport rather than wrapping, so alignment
/// and hunk structure stay readable.
fn diff_row(ui: &mut egui::Ui, kind: DiffLineKind, text: &str, row_h: f32) {
    // Paint the row background first (behind the text) across the visible width.
    if let Some(bg) = diff_line_bg(kind) {
        let clip = ui.clip_rect();
        let rect = egui::Rect::from_min_size(
            egui::pos2(clip.left(), ui.cursor().top()),
            egui::vec2(clip.width(), row_h),
        );
        ui.painter().rect_filled(rect, 0.0, bg);
    }

    // A blank line still needs to occupy a row so the diff's spacing is kept.
    let shown = if text.is_empty() { " " } else { text };
    let rt = egui::RichText::new(shown).monospace();
    let rt = match kind {
        DiffLineKind::Add => rt.color(DIFF_ADD_COLOR),
        DiffLineKind::Del => rt.color(DIFF_DEL_COLOR),
        DiffLineKind::Hunk => rt.color(DIFF_HUNK_COLOR),
        DiffLineKind::FileHeader => rt.strong(),
        DiffLineKind::Context => rt,
    };
    ui.add(egui::Label::new(rt).wrap_mode(egui::TextWrapMode::Extend));
}

/// One terminal tab within a workspace: a live PTY-backed terminal, its stable
/// global id (used to route PTY events), the shell-provided title (updated via
/// OSC sequences), and an optional user-set name that overrides it.
struct TermTab {
    id: u64,
    backend: TerminalBackend,
    title: String,
    /// A name the user typed via rename; when set, it wins over the shell title.
    custom_name: Option<String>,
    /// Mouse/scroll state for the extra input handling in [`term_input`].
    mouse: MouseState,
}

impl TermTab {
    /// The label to show: the user's name if they set one, else the shell title.
    fn display_name(&self) -> String {
        self.custom_name.clone().unwrap_or_else(|| self.title.clone())
    }
}

/// An in-progress inline rename of a terminal (a workspace tab or a standalone
/// terminal), keyed by the terminal's global PTY id.
struct Renaming {
    id: u64,
    buffer: String,
    /// Request keyboard focus for the edit field on its next render.
    focus: bool,
}

/// The terminals belonging to a single workspace: the open tabs and the index
/// of the active one.
struct WorkspaceTerminals {
    tabs: Vec<TermTab>,
    active: usize,
}

/// A standalone terminal not tied to any workspace — a plain shell rooted at the
/// home directory, listed under "Terminals" in the left panel.
struct ScratchTerminal {
    /// Display name, e.g. "Terminal 1".
    name: String,
    /// The live PTY. Its `tab.id` doubles as the terminal's selection/routing id.
    tab: TermTab,
}

/// What the central pane is showing: a workspace (by name) or a standalone
/// terminal (by its PTY id).
#[derive(Clone, PartialEq)]
enum Selection {
    Workspace(String),
    Scratch(u64),
}

/// The target of a pending "are you sure?" confirmation.
#[derive(Clone)]
enum RemoveTarget {
    Workspace(String),
    Base(String),
}

/// Which half of the New-workspace dialog is active: describe-it-with-AI or the
/// manual name/base form.
#[derive(Clone, Copy, PartialEq)]
enum NewWsMode {
    Ai,
    Manual,
}

/// Intents collected while the workspace list renders, applied once the panel
/// closure's borrow of `self` has ended.
#[derive(Default)]
struct ListActions {
    /// A workspace was clicked → raise its linked windows.
    raise: Option<String>,
    /// Open a new standalone terminal.
    new_scratch: bool,
    /// Close the standalone terminal with this PTY id.
    close_scratch: Option<u64>,
    /// Start renaming a standalone terminal, seeded with its current name.
    start_rename: Option<(u64, String)>,
    /// Commit the in-progress rename of this standalone terminal.
    commit_rename: Option<u64>,
    /// Abandon the in-progress rename.
    cancel_rename: bool,
}

/// A user intent collected during a UI pass, applied after rendering so the
/// borrow of `self` from the panel/window closures has ended.
enum PendingAction {
    CreateBase { name: String, paths: Vec<String> },
    RemoveBase(String),
    CreateWorkspace { name: String, base: String },
    CreateWorkspaceAi { prompt: String, base: Option<String> },
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

/// Intents collected while rendering a workspace's details pane, applied after
/// the pane's borrow of `self` has ended.
#[derive(Default)]
struct DetailActions {
    remove: Option<String>,
    open_link: bool,
    auto_link: bool,
    unlink_idx: Option<usize>,
}

struct CutterApp {
    tab: Tab,

    // Workspaces
    workspaces: Vec<WorkspaceConfig>,
    workspaces_error: Option<String>,
    selected: Option<Selection>,

    // Live Claude Code session status, refreshed from ~/.config/cutter/sessions
    // on every reload (the config-dir watcher wakes us when a hook writes one).
    // `sessions` is the raw per-session list (for the details pane); `session_status`
    // is the per-workspace aggregate (for the list dots and header chip).
    sessions: Vec<SessionRecord>,
    session_status: HashMap<String, WorkspaceStatus>,

    // GitHub PR status per workspace, fetched lazily off-thread via `gh` and
    // cached. `pr_fetching` guards against duplicate in-flight fetches; results
    // arrive on `pr_rx`.
    pr_status: HashMap<String, Vec<pr::PrInfo>>,
    pr_fetching: HashSet<String>,
    pr_tx: Sender<(String, Vec<pr::PrInfo>)>,
    pr_rx: Receiver<(String, Vec<pr::PrInfo>)>,

    // Git diffs, keyed by (workspace name, repo name) and fetched lazily
    // off-thread. `diff_view_repo` remembers which repo the Diff pane shows per
    // workspace; `diff_fetching` guards duplicate in-flight fetches; results
    // (Ok(diff) or Err(message)) arrive on `diff_rx`.
    diff_cache: HashMap<(String, String), std::result::Result<DiffView, String>>,
    diff_view_repo: HashMap<String, usize>,
    diff_fetching: HashSet<(String, String)>,
    diff_tx: Sender<(String, String, std::result::Result<git::RepoDiff, String>)>,
    diff_rx: Receiver<(String, String, std::result::Result<git::RepoDiff, String>)>,

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

    // Edit-base form. `edit_base_name` is the (immutable) base being edited.
    show_edit_base: bool,
    edit_base_name: String,
    edit_repos: Vec<RepoRef>,
    edit_branch_from: String,
    edit_copy_files: Vec<String>,
    edit_new_copy_file: String,
    edit_error: Option<String>,

    // New-workspace form
    show_new_workspace: bool,
    new_ws_name: String,
    new_ws_base: Option<String>,
    // Natural-language prompt for AI-driven creation, which mode the dialog's top
    // switcher is on, and an optional base hint for AI mode (`None` = let Claude
    // choose).
    new_ws_ai: String,
    new_ws_mode: NewWsMode,
    new_ws_ai_base: Option<String>,

    // Pending "are you sure?" for a destructive action.
    confirm_remove: Option<RemoveTarget>,

    // "Link windows" modal: which workspace, the enumerated candidates, and
    // which are checked. `ax_trusted` is snapshotted when the modal opens.
    show_link_windows: bool,
    link_for: Option<String>,
    link_candidates: Vec<WindowInfo>,
    link_checked: Vec<bool>,
    ax_trusted: bool,

    // Background work. Create/remove shell out to git (fetch can be slow), so
    // they run off the UI thread; `job_rx` delivers the outcome back.
    job: Option<RunningJob>,
    job_rx: Option<Receiver<JobOutcome>>,
    status: Option<StatusMsg>,

    // Filesystem watching. The debouncer is kept alive only so it keeps
    // watching; `fs_rx` receives a tick whenever the config dir changes.
    _debouncer: Option<Debouncer<RecommendedWatcher>>,
    fs_rx: Option<Receiver<()>>,

    // Embedded terminals. `ws_view` selects the terminal or details pane for the
    // selected workspace. `terminals` holds the live terminals per workspace
    // (keyed by name), created lazily on first view. Every backend reports PTY
    // events as `(tab id, event)` on the shared channel; ids are globally unique
    // (handed out by `next_term_id`) so each event routes to exactly one tab.
    ws_view: WorkspaceView,
    terminals: HashMap<String, WorkspaceTerminals>,
    // Standalone terminals not tied to any workspace, shown under "Terminals" in
    // the left list. `next_scratch_num` names them "Terminal 1", "Terminal 2", …
    scratch_terminals: Vec<ScratchTerminal>,
    next_scratch_num: u32,
    // In-progress inline rename of a terminal, if any (see `Renaming`).
    renaming: Option<Renaming>,

    // Collapsed view: just the workspace list, in a window narrowed to fit it.
    // `expanded_size` remembers the window to restore on the way back out.
    collapsed: bool,
    expanded_size: Option<egui::Vec2>,
    term_tx: Sender<(u64, PtyEvent)>,
    term_rx: Receiver<(u64, PtyEvent)>,
    next_term_id: u64,
}

impl CutterApp {
    fn new(ctx: &egui::Context) -> Self {
        let (term_tx, term_rx) = std::sync::mpsc::channel();
        let (pr_tx, pr_rx) = std::sync::mpsc::channel();
        let (diff_tx, diff_rx) = std::sync::mpsc::channel();
        let mut app = Self {
            tab: Tab::Workspaces,
            workspaces: Vec::new(),
            workspaces_error: None,
            selected: None,
            sessions: Vec::new(),
            session_status: HashMap::new(),
            pr_status: HashMap::new(),
            pr_fetching: HashSet::new(),
            pr_tx,
            pr_rx,
            diff_cache: HashMap::new(),
            diff_view_repo: HashMap::new(),
            diff_fetching: HashSet::new(),
            diff_tx,
            diff_rx,
            workspace_root: String::new(),
            default_branch_from: String::new(),
            bases: BTreeMap::new(),
            config_error: None,
            show_new_base: false,
            new_base_name: String::new(),
            new_base_repos: Vec::new(),
            new_base_manual_path: String::new(),
            show_edit_base: false,
            edit_base_name: String::new(),
            edit_repos: Vec::new(),
            edit_branch_from: String::new(),
            edit_copy_files: Vec::new(),
            edit_new_copy_file: String::new(),
            edit_error: None,
            show_new_workspace: false,
            new_ws_name: String::new(),
            new_ws_base: None,
            new_ws_ai: String::new(),
            new_ws_mode: NewWsMode::Ai,
            new_ws_ai_base: None,
            confirm_remove: None,
            show_link_windows: false,
            link_for: None,
            link_candidates: Vec::new(),
            link_checked: Vec::new(),
            ax_trusted: false,
            job: None,
            job_rx: None,
            status: None,
            _debouncer: None,
            fs_rx: None,
            ws_view: WorkspaceView::Terminal,
            terminals: HashMap::new(),
            scratch_terminals: Vec::new(),
            next_scratch_num: 1,
            renaming: None,
            collapsed: false,
            expanded_size: None,
            term_tx,
            term_rx,
            next_term_id: 0,
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

        // Preserve the current selection if it still exists, else select the
        // first workspace. A selected standalone terminal is kept as long as it's
        // still open (reload is driven by workspace/config changes, not terminals).
        let still_present = match &self.selected {
            Some(Selection::Workspace(name)) => {
                self.workspaces.iter().any(|w| &w.workspace.name == name)
            }
            Some(Selection::Scratch(id)) => {
                self.scratch_terminals.iter().any(|s| s.tab.id == *id)
            }
            None => false,
        };
        if !still_present {
            self.selected = self
                .workspaces
                .first()
                .map(|w| Selection::Workspace(w.workspace.name.clone()));
        }

        // Keep the session-status hooks installed in every workspace. Idempotent
        // and cheap (writes only when a hook is missing), so this also retrofits
        // workspaces created before this feature existed.
        for ws in &self.workspaces {
            let _ = session::ensure_hooks(std::path::Path::new(&ws.workspace.path));
        }
        self.sessions = session::load_active(&self.workspaces);
        self.session_status = session::aggregate(&self.sessions);

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
        match &self.selected {
            Some(Selection::Workspace(name)) => {
                self.workspaces.iter().find(|w| &w.workspace.name == name)
            }
            _ => None,
        }
    }

    /// Spawn one PTY-backed terminal rooted at `path`, taking the next global
    /// id. Returns `None` if the backend fails to start (e.g. bad shell), which
    /// the caller surfaces as no new tab.
    fn spawn_terminal(
        next_id: &mut u64,
        ctx: &egui::Context,
        tx: &Sender<(u64, PtyEvent)>,
        path: &str,
    ) -> Option<TermTab> {
        let id = *next_id;
        *next_id += 1;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        // Default tab label is the shell name (e.g. "zsh"); the shell usually
        // replaces it via an OSC title sequence once it starts.
        let title = std::path::Path::new(&shell)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shell".to_string());
        let settings = egui_term::BackendSettings {
            shell,
            working_directory: Some(PathBuf::from(path)),
            ..Default::default()
        };
        match TerminalBackend::new(id, ctx.clone(), tx.clone(), settings) {
            Ok(backend) => Some(TermTab {
                id,
                backend,
                title,
                custom_name: None,
                mouse: MouseState::default(),
            }),
            Err(_) => None,
        }
    }

    /// Route a terminal PTY event to its tab by global id: a finished shell
    /// closes its tab; a title sequence renames it.
    fn on_term_event(&mut self, id: u64, event: PtyEvent) {
        match event {
            PtyEvent::Exit => self.remove_term_by_id(id),
            PtyEvent::Title(title) => {
                for term in self.terminals.values_mut() {
                    if let Some(t) = term.tabs.iter_mut().find(|t| t.id == id) {
                        t.title = title;
                        return;
                    }
                }
                if let Some(s) = self.scratch_terminals.iter_mut().find(|s| s.tab.id == id) {
                    s.tab.title = title;
                }
            }
            _ => {}
        }
    }

    /// Remove the terminal tab with `id` from whichever workspace or standalone
    /// terminal owns it, keeping any active index / selection in range.
    fn remove_term_by_id(&mut self, id: u64) {
        // Drop any pending rename targeting the terminal being removed.
        if self.renaming.as_ref().is_some_and(|r| r.id == id) {
            self.renaming = None;
        }
        for term in self.terminals.values_mut() {
            if let Some(pos) = term.tabs.iter().position(|t| t.id == id) {
                term.tabs.remove(pos);
                if !term.tabs.is_empty() && term.active >= term.tabs.len() {
                    term.active = term.tabs.len() - 1;
                }
                return;
            }
        }
        if let Some(pos) = self.scratch_terminals.iter().position(|s| s.tab.id == id) {
            self.scratch_terminals.remove(pos);
            // If the closed terminal was selected, drop the selection.
            if self.selected == Some(Selection::Scratch(id)) {
                self.selected = None;
            }
        }
    }

    /// Render the terminal pane for `ws_name`: a tab strip (＋ to add, ✕ to
    /// close) above the active terminal, which fills the rest of the pane. The
    /// first tab is created lazily so clicking a workspace lands on a shell.
    fn terminal_pane(&mut self, ui: &mut egui::Ui, ws_name: &str) {
        use std::collections::hash_map::Entry;
        let ctx = ui.ctx().clone();

        // While a modal is open or a terminal is being renamed, the terminal must
        // not claim keyboard focus, or the modal/rename text field can't be typed
        // into (the terminal would grab focus back every frame).
        let allow_focus = !self.any_modal_open() && self.renaming.is_none();

        // The shell is rooted at the workspace directory.
        let Some(path) = self
            .workspaces
            .iter()
            .find(|w| w.workspace.name == ws_name)
            .map(|w| w.workspace.path.clone())
        else {
            return;
        };

        // First time this workspace's terminal is shown, open one tab. Closing
        // every tab afterwards leaves it empty (＋ reopens one) rather than
        // respawning against the user's intent.
        if let Entry::Vacant(v) = self.terminals.entry(ws_name.to_string()) {
            match Self::spawn_terminal(&mut self.next_term_id, &ctx, &self.term_tx, &path) {
                Some(tab) => {
                    v.insert(WorkspaceTerminals {
                        tabs: vec![tab],
                        active: 0,
                    });
                }
                None => {
                    ui.colored_label(egui::Color32::RED, "Failed to start a terminal.");
                    return;
                }
            }
        }

        // Take any in-progress rename out of `self` so the edit field can be
        // rendered without aliasing the `term` borrow below; written back at the
        // end of the function.
        let mut rename = self.renaming.take();
        let term = self.terminals.get_mut(ws_name).expect("just inserted");

        // --- tab strip ---
        let mut want_activate: Option<usize> = None;
        let mut want_close: Option<usize> = None;
        let mut want_add = false;
        let mut want_start_rename: Option<u64> = None;
        let mut commit_rename = false;
        let mut cancel_rename = false;
        ui.horizontal(|ui| {
            for i in 0..term.tabs.len() {
                let tab_id = term.tabs[i].id;
                let editing = rename.as_ref().is_some_and(|r| r.id == tab_id);
                if editing {
                    match rename_field(ui, rename.as_mut().unwrap(), 90.0) {
                        Some(true) => commit_rename = true,
                        Some(false) => cancel_rename = true,
                        None => {}
                    }
                } else {
                    let selected = i == term.active;
                    let resp = ui
                        .selectable_label(selected, term.tabs[i].display_name())
                        .on_hover_text("Double-click to rename");
                    if resp.clicked() {
                        want_activate = Some(i);
                    }
                    if resp.double_clicked() {
                        want_start_rename = Some(tab_id);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Rename").clicked() {
                            want_start_rename = Some(tab_id);
                            ui.close();
                        }
                    });
                }
                // U+2716 (✖) and U+2795 (➕) below are glyphs egui's bundled
                // fonts actually ship; plain "x"/"＋" (U+FF0B) render as tofu.
                if ui.small_button("✖").on_hover_text("Close terminal").clicked() {
                    want_close = Some(i);
                }
                ui.separator();
            }
            if ui.button("➕").on_hover_text("New terminal").clicked() {
                want_add = true;
            }
        });
        // Apply rename intents against the tab list.
        if commit_rename {
            if let Some(r) = rename.take() {
                if let Some(t) = term.tabs.iter_mut().find(|t| t.id == r.id) {
                    let n = r.buffer.trim();
                    t.custom_name = (!n.is_empty()).then(|| n.to_string());
                }
            }
        }
        if cancel_rename {
            rename = None;
        }
        if let Some(id) = want_start_rename {
            if let Some(t) = term.tabs.iter().find(|t| t.id == id) {
                rename = Some(Renaming {
                    id,
                    buffer: t.display_name(),
                    focus: true,
                });
            }
        }
        if let Some(i) = want_activate {
            term.active = i;
        }
        ui.separator();

        // --- active terminal fills the rest of the pane ---
        if term.tabs.is_empty() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("No terminals open. Use ＋ to start one.").weak());
        } else {
            if term.active >= term.tabs.len() {
                term.active = term.tabs.len() - 1;
            }
            let region = ui.available_rect_before_wrap();
            let tab = &mut term.tabs[term.active];
            if allow_focus {
                term_input::handle(ui, region, &mut tab.backend, &mut tab.mouse);
            }
            let terminal = TerminalView::new(ui, &mut tab.backend)
                .set_focus(allow_focus)
                .set_size(ui.available_size());
            ui.add(terminal);
            if allow_focus {
                term_input::paint_drop_hint(ui, region);
            }
        }

        // --- apply tab intents after rendering ---
        if let Some(i) = want_close {
            if i < term.tabs.len() {
                // Drop a pending rename if it targets the tab being closed.
                if rename.as_ref().is_some_and(|r| r.id == term.tabs[i].id) {
                    rename = None;
                }
                term.tabs.remove(i);
                if !term.tabs.is_empty() && term.active >= term.tabs.len() {
                    term.active = term.tabs.len() - 1;
                }
            }
        }
        if want_add {
            if let Some(tab) =
                Self::spawn_terminal(&mut self.next_term_id, &ctx, &self.term_tx, &path)
            {
                term.tabs.push(tab);
                term.active = term.tabs.len() - 1;
            }
        }

        // Carry the (possibly updated) rename state back into `self`.
        self.renaming = rename;
    }

    /// Open a new standalone terminal rooted at the user's home directory and
    /// select it.
    fn new_scratch_terminal(&mut self, ctx: &egui::Context) {
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());
        if let Some(tab) = Self::spawn_terminal(&mut self.next_term_id, ctx, &self.term_tx, &home) {
            let id = tab.id;
            let name = format!("Terminal {}", self.next_scratch_num);
            self.next_scratch_num += 1;
            self.scratch_terminals.push(ScratchTerminal { name, tab });
            self.selected = Some(Selection::Scratch(id));
        }
    }

    /// Render the selected standalone terminal filling the pane, with a header
    /// showing its name and a Close button.
    fn scratch_terminal_pane(&mut self, ui: &mut egui::Ui, id: u64) {
        // Don't let this terminal grab focus while a modal is open or a rename is
        // in progress (the rename field lives in the left panel).
        let allow_focus = !self.any_modal_open() && self.renaming.is_none();
        let Some(idx) = self.scratch_terminals.iter().position(|s| s.tab.id == id) else {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("Terminal closed").weak());
            });
            return;
        };
        let name = self.scratch_terminals[idx].name.clone();

        ui.add_space(6.0);
        let mut want_close = false;
        ui.horizontal(|ui| {
            ui.heading(&name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("✖ Close")
                    .on_hover_text("Close this terminal")
                    .clicked()
                {
                    want_close = true;
                }
            });
        });
        ui.separator();

        let region = ui.available_rect_before_wrap();
        let tab = &mut self.scratch_terminals[idx].tab;
        if allow_focus {
            term_input::handle(ui, region, &mut tab.backend, &mut tab.mouse);
        }
        let terminal = TerminalView::new(ui, &mut tab.backend)
            .set_focus(allow_focus)
            .set_size(ui.available_size());
        ui.add(terminal);
        if allow_focus {
            term_input::paint_drop_hint(ui, region);
        }

        if want_close {
            self.remove_term_by_id(id);
        }
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
                self.selected = Some(Selection::Workspace(name.clone()));
                let display = name.clone();
                self.start_job(ctx, label, move || {
                    commands::create::run(Some(&name), Some(&base), false, ClaudeMode::None)
                        .map_err(|e| e.to_string())?;
                    Ok(format!("Workspace '{display}' created"))
                });
            }
            PendingAction::CreateWorkspaceAi { prompt, base } => {
                let label = "Creating workspace with AI…".to_string();
                self.start_job(ctx, label, move || {
                    let name = commands::ai::run(&prompt, base.as_deref())
                        .map_err(|e| e.to_string())?;
                    Ok(format!("Workspace '{name}' created"))
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

    /// Collapse to just the workspace list, or restore the full view.
    ///
    /// Collapsed, Cutter is a launcher: the list of workspaces and their Claude
    /// status, narrow enough to sit beside whatever you're actually working in.
    /// The window follows, giving up the width the details pane was using and
    /// getting it back on the way out. Terminals keep running either way —
    /// they're only hidden, not closed.
    fn toggle_collapsed(&mut self, ctx: &egui::Context) {
        let current = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
        self.collapsed = !self.collapsed;
        if self.collapsed {
            // Settings is a form; it has nothing to say at list width.
            self.tab = Tab::Workspaces;
            self.expanded_size = current;
            let height = current.map_or(560.0, |s| s.y);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                COLLAPSED_WIDTH,
                height,
            )));
        } else if let Some(size) = self.expanded_size.take() {
            // Keep whatever height the user settled on while collapsed.
            let size = egui::vec2(size.x, current.map_or(size.y, |s| s.y));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
    }

    /// Render the left-hand list: every workspace, then the standalone
    /// terminals. Collected intents go into `out` because the list is drawn
    /// from inside a panel closure that borrows `self`.
    ///
    /// This is the whole window when the view is collapsed, so it has to stand
    /// on its own rather than assume a details pane beside it.
    fn workspace_list(&mut self, ui: &mut egui::Ui, out: &mut ListActions) {
        let job_active = self.job.is_some();
        // A fixed id keeps the scroll position across a collapse/expand, which
        // reparents the list from the side panel to the central one.
        let scroll = egui::ScrollArea::vertical().id_salt("workspace_list_scroll");
        scroll.show(ui, |ui| {
            // --- Workspaces ---
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
            } else if self.workspaces.is_empty() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("No workspaces yet. Use ➕ New.").weak());
            } else {
                // Snapshot names first so we can mutate `selected` while iterating.
                let names: Vec<String> =
                    self.workspaces.iter().map(|w| w.workspace.name.clone()).collect();
                for name in names {
                    let is_selected = matches!(
                        &self.selected,
                        Some(Selection::Workspace(n)) if n == &name
                    );
                    // Show a small ⧉ marker on workspaces that have links.
                    let has_links = self
                        .workspaces
                        .iter()
                        .find(|w| w.workspace.name == name)
                        .is_some_and(|w| !w.linked_windows.is_empty());
                    let label = if has_links {
                        format!("⧉  {name}")
                    } else {
                        name.clone()
                    };
                    let status =
                        self.session_status.get(&name).copied().unwrap_or_default();
                    let clicked = ui
                        .horizontal(|ui| {
                            // A leading icon shows Claude's status in a
                            // fixed-width slot so names stay aligned; idle
                            // workspaces leave the slot empty.
                            let slot = egui::vec2(18.0, 16.0);
                            match status.state() {
                                Some(state) => {
                                    let (icon, color) = state_icon(state);
                                    ui.add_sized(
                                        slot,
                                        egui::Label::new(
                                            egui::RichText::new(icon).color(color),
                                        ),
                                    )
                                    .on_hover_text(status_hover(state, status));
                                }
                                None => {
                                    ui.add_sized(slot, egui::Label::new(""));
                                }
                            }
                            ui.selectable_label(is_selected, label).clicked()
                        })
                        .inner;
                    if clicked {
                        self.selected = Some(Selection::Workspace(name.clone()));
                        // Clicking activates the workspace's linked windows.
                        out.raise = Some(name);
                    }
                }
            }

            // --- Standalone terminals ---
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Terminals ({})",
                        self.scratch_terminals.len()
                    ))
                    .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("➕ New")
                        .on_hover_text("Open a standalone terminal (not tied to a workspace)")
                        .clicked()
                    {
                        out.new_scratch = true;
                    }
                });
            });
            ui.separator();

            if self.scratch_terminals.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("A plain shell rooted at your home directory.")
                        .weak(),
                );
            } else {
                // Snapshot ids/names so we can mutate `selected` while iterating.
                let entries: Vec<(u64, String)> = self
                    .scratch_terminals
                    .iter()
                    .map(|s| (s.tab.id, s.name.clone()))
                    .collect();
                for (id, tname) in entries {
                    let is_selected = self.selected == Some(Selection::Scratch(id));
                    let editing = self.renaming.as_ref().is_some_and(|r| r.id == id);
                    let clicked = ui
                        .horizontal(|ui| {
                            if ui
                                .small_button("✖")
                                .on_hover_text("Close terminal")
                                .clicked()
                            {
                                out.close_scratch = Some(id);
                            }
                            if editing {
                                match rename_field(
                                    ui,
                                    self.renaming.as_mut().unwrap(),
                                    130.0,
                                ) {
                                    Some(true) => out.commit_rename = Some(id),
                                    Some(false) => out.cancel_rename = true,
                                    None => {}
                                }
                                false
                            } else {
                                let resp = ui
                                    .selectable_label(is_selected, tname.clone())
                                    .on_hover_text("Double-click to rename");
                                if resp.double_clicked() {
                                    out.start_rename = Some((id, tname.clone()));
                                }
                                resp.clicked()
                            }
                        })
                        .inner;
                    if clicked {
                        self.selected = Some(Selection::Scratch(id));
                    }
                }
            }
        });
    }

    fn workspaces_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let job_active = self.job.is_some();
        // List intents (selection, standalone terminals), applied after the
        // panels render and their borrows of `self` have ended.
        let mut list = ListActions::default();
        // Detail-pane intents for the selected workspace.
        let mut actions = DetailActions::default();
        // A workspace whose PR status needs fetching (lazy, once per workspace).
        let mut pr_fetch_request: Option<String> = None;

        // Collapsed, the list is the whole window; expanded, it's the left
        // panel with the selected workspace's pane beside it.
        if self.collapsed {
            egui::CentralPanel::default().show_inside(ui, |ui| self.workspace_list(ui, &mut list));
        } else {
            egui::Panel::left("workspace_list")
                .resizable(true)
                .default_size(220.0)
                .show_inside(ui, |ui| self.workspace_list(ui, &mut list));

            egui::CentralPanel::default().show_inside(ui, |ui| match self.selected.clone() {
                Some(Selection::Workspace(name)) => {
                    // Header: workspace name + Claude status chip + Terminal/Details toggle.
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.heading(&name);
                        let state = self.session_status.get(&name).and_then(|s| s.state());
                        if let Some(state) = state {
                            match state {
                                SessionState::Running => {
                                    // A live spinner reads as activity; the Spinner
                                    // widget requests its own repaints while visible.
                                    ui.add(egui::Spinner::new().size(15.0).color(RUNNING_COLOR));
                                    ui.colored_label(RUNNING_COLOR, "running");
                                }
                                SessionState::Waiting => {
                                    ui.colored_label(
                                        WAITING_COLOR,
                                        format!("{WAITING_ICON} waiting for input"),
                                    );
                                }
                            }
                        }
                        // Anchor the view toggle to the right so it stays put as the
                        // status chip's text changes width. In a right-to-left layout
                        // the first widget lands rightmost, so add Details before Terminal.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let view = &mut self.ws_view;
                            ui.selectable_value(view, WorkspaceView::Diff, "Diff");
                            ui.selectable_value(view, WorkspaceView::Details, "Details");
                            ui.selectable_value(view, WorkspaceView::Terminal, "Terminal");
                        });
                    });

                    // PR chips under the workspace name (one per in-flight PR across
                    // the repos), coloured by status and linking to GitHub. Fetched
                    // lazily the first time the workspace is shown.
                    match self.pr_status.get(&name) {
                        Some(prs) if !prs.is_empty() => {
                            ui.horizontal(|ui| {
                                for p in prs {
                                    ui.hyperlink_to(
                                        egui::RichText::new(format!("#{}", p.number))
                                            .color(pr_color(p.state))
                                            .strong(),
                                        &p.url,
                                    )
                                    .on_hover_text(format!(
                                        "{} · {} ({})",
                                        p.repo,
                                        p.title,
                                        p.state.label()
                                    ));
                                }
                            });
                        }
                        Some(_) => {} // fetched, none in flight → show nothing
                        None => {
                            if !self.pr_fetching.contains(&name) {
                                pr_fetch_request = Some(name.clone());
                            }
                        }
                    }

                    ui.separator();

                    match self.ws_view {
                        WorkspaceView::Terminal => self.terminal_pane(ui, &name),
                        WorkspaceView::Diff => self.diff_pane(ui, &name),
                        WorkspaceView::Details => match self.selected_workspace() {
                            Some(ws) => Self::workspace_details(
                                ui,
                                ws,
                                &self.sessions,
                                job_active,
                                &mut actions,
                            ),
                            None => {
                                ui.centered_and_justified(|ui| {
                                    ui.label(egui::RichText::new("Select a workspace").weak());
                                });
                            }
                        },
                    }
                }
                Some(Selection::Scratch(id)) => self.scratch_terminal_pane(ui, id),
                None => {
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new("Select a workspace or terminal").weak());
                    });
                }
            });
        }

        if list.new_scratch {
            self.new_scratch_terminal(&ctx);
        }
        if let Some(id) = list.close_scratch {
            self.remove_term_by_id(id);
        }
        // Standalone-terminal rename: commit/cancel first, then start a new one.
        if let Some(id) = list.commit_rename {
            if let Some(r) = self.renaming.take() {
                if let Some(s) = self.scratch_terminals.iter_mut().find(|s| s.tab.id == id) {
                    let n = r.buffer.trim();
                    if !n.is_empty() {
                        s.name = n.to_string();
                    }
                }
            }
        }
        if list.cancel_rename {
            self.renaming = None;
        }
        if let Some((id, current)) = list.start_rename {
            self.renaming = Some(Renaming {
                id,
                buffer: current,
                focus: true,
            });
        }
        if let Some(name) = actions.remove {
            self.confirm_remove = Some(RemoveTarget::Workspace(name));
        }
        if actions.open_link {
            if let Some(Selection::Workspace(name)) = self.selected.clone() {
                self.open_link_windows(name);
            }
        }
        if actions.auto_link {
            if let Some(Selection::Workspace(name)) = self.selected.clone() {
                self.start_ai_link(&ctx, name);
            }
        }
        if let Some(name) = pr_fetch_request {
            self.start_pr_fetch(&ctx, name);
        }
        if let Some(idx) = actions.unlink_idx {
            if let Some(Selection::Workspace(name)) = self.selected.clone() {
                self.unlink_window(&name, idx);
            }
        }
        if let Some(name) = list.raise {
            self.activate_workspace(&name);
        }
    }

    fn workspace_details(
        ui: &mut egui::Ui,
        ws: &WorkspaceConfig,
        sessions: &[SessionRecord],
        job_active: bool,
        actions: &mut DetailActions,
    ) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(6.0);
            // The workspace name is shown in the pane header; here we just offer
            // the (contextual) Remove action, right-aligned.
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!job_active, egui::Button::new("🗑 Remove"))
                        .on_hover_text("Remove worktrees, branches, and files")
                        .clicked()
                    {
                        actions.remove = Some(ws.workspace.name.clone());
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

            // Live Claude Code sessions running in this workspace.
            let ws_sessions: Vec<&SessionRecord> = sessions
                .iter()
                .filter(|s| s.workspace == ws.workspace.name)
                .collect();
            if !ws_sessions.is_empty() {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(format!("Claude sessions ({})", ws_sessions.len()))
                        .strong(),
                );
                ui.separator();
                for s in ws_sessions {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        let (icon, color) = state_icon(s.state);
                        ui.colored_label(color, icon);
                        ui.label(s.state.label());
                        ui.label(
                            egui::RichText::new(format!(
                                "· updated {}",
                                s.updated_at.format("%H:%M:%S UTC")
                            ))
                            .weak(),
                        );
                    });
                    ui.label(egui::RichText::new(s.cwd.as_str()).monospace().weak().small());
                }
            }

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

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("Linked windows ({})", ws.linked_windows.len()))
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("⧉ Link windows…")
                        .on_hover_text("Manually tie macOS windows to this workspace")
                        .clicked()
                    {
                        actions.open_link = true;
                    }
                    if ui
                        .add_enabled(!job_active, egui::Button::new("🤖 Auto-link"))
                        .on_hover_text(
                            "Let Claude pick windows related to this workspace and link them",
                        )
                        .clicked()
                    {
                        actions.auto_link = true;
                    }
                });
            });
            ui.separator();

            if ws.linked_windows.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "None. Click this workspace to bring its linked windows forward.",
                    )
                    .weak(),
                );
            } else {
                for (i, link) in ws.linked_windows.iter().enumerate() {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        if ui.small_button("✕").on_hover_text("Unlink").clicked() {
                            actions.unlink_idx = Some(i);
                        }
                        ui.label(egui::RichText::new(link.app_name.as_str()).strong());
                        let title = if link.title.is_empty() {
                            "(untitled)".to_string()
                        } else {
                            link.title.clone()
                        };
                        ui.label(egui::RichText::new(title).weak());
                    });
                }
            }
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let job_active = self.job.is_some();
        let mut open_new_base = false;
        let mut want_remove_base: Option<String> = None;
        let mut want_edit_base: Option<String> = None;

        egui::CentralPanel::default().show_inside(ui, |ui| {
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
                                    if ui
                                        .add_enabled(!job_active, egui::Button::new("Edit"))
                                        .on_hover_text("Edit repos, branch-from, and copy files")
                                        .clicked()
                                    {
                                        want_edit_base = Some(name.clone());
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
        if let Some(name) = want_edit_base {
            self.open_edit_base(name);
        }
    }

    /// Load a base into the edit form and open the modal.
    fn open_edit_base(&mut self, base_name: String) {
        if let Some(base) = self.bases.get(&base_name).cloned() {
            self.edit_branch_from = base.branch_from.clone().unwrap_or_default();
            self.edit_copy_files = base.copy_files.clone();
            self.edit_repos = base.repos.clone();
            self.edit_base_name = base_name;
            self.edit_new_copy_file.clear();
            self.edit_error = None;
            self.show_edit_base = true;
        }
    }

    /// Whether any modal window/dialog is currently open. Used to keep the
    /// embedded terminal from stealing keyboard focus from a modal's fields.
    fn any_modal_open(&self) -> bool {
        self.show_new_workspace
            || self.show_new_base
            || self.show_edit_base
            || self.show_link_windows
            || self.confirm_remove.is_some()
    }

    fn open_new_workspace(&mut self) {
        self.show_new_workspace = true;
        let first_base = self.bases.keys().next().cloned();
        if self.new_ws_base.is_none() {
            self.new_ws_base = first_base.clone();
        }
        if self.new_ws_ai_base.is_none() {
            self.new_ws_ai_base = first_base;
        }
    }

    /// Fetch GitHub PR status for a workspace's repos off the UI thread (a `gh`
    /// call per repo), caching the result. No-op if already cached or in flight.
    /// The Diff pane for `ws_name`: a repo dropdown + refresh above a coloured,
    /// scrollable unified diff of the selected repo. Diffs are fetched lazily and
    /// cached; switching repos or hitting Refresh triggers a fetch.
    fn diff_pane(&mut self, ui: &mut egui::Ui, ws_name: &str) {
        let ctx = ui.ctx().clone();

        // The repos in this workspace (name + worktree path) and its base name.
        let Some(ws) = self.workspaces.iter().find(|w| w.workspace.name == ws_name) else {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("Workspace not found").weak());
            });
            return;
        };
        let repos: Vec<(String, String)> = ws
            .repos
            .iter()
            .map(|r| (r.name.clone(), r.worktree_path.clone()))
            .collect();
        let base_name = ws.workspace.base.clone();

        if repos.is_empty() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("This workspace has no repos.").weak());
            return;
        }

        // Which repo is shown, clamped in case the workspace's repos changed.
        let mut sel = self
            .diff_view_repo
            .get(ws_name)
            .copied()
            .unwrap_or(0)
            .min(repos.len() - 1);

        // --- header: repo dropdown + refresh + "vs <base>" label ---
        let mut refresh = false;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("diff_repo", ws_name))
                .selected_text(repos[sel].0.clone())
                .show_ui(ui, |ui| {
                    for (i, (name, _)) in repos.iter().enumerate() {
                        ui.selectable_value(&mut sel, i, name);
                    }
                });
            if ui
                .button("⟳ Refresh")
                .on_hover_text("Re-run git diff for this repo")
                .clicked()
            {
                refresh = true;
            }
            let key = (ws_name.to_string(), repos[sel].0.clone());
            if let Some(Ok(dv)) = self.diff_cache.get(&key) {
                // Change summary: "N files  +A  −D". Skipped when there's nothing
                // to show (the body prints "No changes." instead).
                if !dv.lines.is_empty() {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!(
                            "{} file{}",
                            dv.files,
                            if dv.files == 1 { "" } else { "s" }
                        ))
                        .weak(),
                    );
                    ui.colored_label(DIFF_ADD_COLOR, format!("+{}", dv.additions));
                    ui.colored_label(DIFF_DEL_COLOR, format!("−{}", dv.deletions));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("vs {}", dv.base_label))
                            .weak()
                            .small(),
                    )
                    .on_hover_text(
                        "Changes on this branch — committed and uncommitted — \
                         since it forked from this ref.",
                    );
                });
            }
        });
        self.diff_view_repo.insert(ws_name.to_string(), sel);

        let (repo_name, worktree) = repos[sel].clone();
        let key = (ws_name.to_string(), repo_name.clone());

        if refresh {
            self.diff_cache.remove(&key);
        }

        // Kick off a fetch if we have neither a cached result nor one in flight.
        if !self.diff_cache.contains_key(&key) && !self.diff_fetching.contains(&key) {
            let base_hint = self.base_ref_for(&base_name, &repo_name);
            self.start_diff_fetch(&ctx, ws_name.to_string(), repo_name, worktree, base_hint);
        }

        ui.separator();

        // --- body ---
        match self.diff_cache.get(&key) {
            Some(Ok(dv)) if dv.lines.is_empty() => {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("No changes.").weak());
            }
            Some(Ok(dv)) => {
                // Virtualise on line count: only the visible rows are laid out,
                // so even a huge diff stays cheap to repaint.
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, dv.lines.len(), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for i in range {
                            let (kind, text) = &dv.lines[i];
                            diff_row(ui, *kind, text, row_h);
                        }
                    });
            }
            Some(Err(e)) => {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::RED, e);
            }
            None => {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Loading diff…").weak());
                });
            }
        }
    }

    /// Fetch `repo_name`'s diff for `ws_name` on a worker thread, delivering the
    /// result on `diff_rx`. `base_hint` is the ref the worktree forked from.
    fn start_diff_fetch(
        &mut self,
        ctx: &egui::Context,
        ws_name: String,
        repo_name: String,
        worktree: String,
        base_hint: Option<String>,
    ) {
        let key = (ws_name.clone(), repo_name.clone());
        if self.diff_fetching.contains(&key) {
            return;
        }
        self.diff_fetching.insert(key);
        let tx = self.diff_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = git::diff(std::path::Path::new(&worktree), base_hint.as_deref())
                .map_err(|e| e.to_string());
            let _ = tx.send((ws_name, repo_name, result));
            ctx.request_repaint();
        });
    }

    /// The ref the given repo's worktree was branched from, following the same
    /// precedence as workspace creation: the repo's per-repo override, else the
    /// base's override, else the global default. Returns `None` if the base is
    /// no longer in the config (the diff then auto-detects a default branch).
    fn base_ref_for(&self, base_name: &str, repo_name: &str) -> Option<String> {
        let base = self.bases.get(base_name)?;
        let per_repo = base
            .repos
            .iter()
            .find(|r| r.name == repo_name)
            .and_then(|r| r.branch_from.clone());
        per_repo
            .or_else(|| base.branch_from.clone())
            .or_else(|| Some(self.default_branch_from.clone()))
    }

    fn start_pr_fetch(&mut self, ctx: &egui::Context, ws_name: String) {
        if self.pr_fetching.contains(&ws_name) || self.pr_status.contains_key(&ws_name) {
            return;
        }
        let Some(ws) = self.workspaces.iter().find(|w| w.workspace.name == ws_name) else {
            return;
        };
        let repos: Vec<(String, String)> = ws
            .repos
            .iter()
            .map(|r| (r.name.clone(), r.worktree_path.clone()))
            .collect();
        if repos.is_empty() {
            // Nothing to query; record an empty result so we don't retry.
            self.pr_status.insert(ws_name, Vec::new());
            return;
        }
        let branch = ws.workspace.branch.clone();
        self.pr_fetching.insert(ws_name.clone());
        let tx = self.pr_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let prs = pr::fetch(&repos, &branch);
            let _ = tx.send((ws_name, prs));
            ctx.request_repaint();
        });
    }

    /// Auto-link windows to `ws_name`: enumerate the live windows on the UI
    /// thread, then off-thread ask Claude which relate to the workspace (its
    /// directory matches are always included) and persist the result. Runs as a
    /// background job so the (slow) Claude call doesn't block the UI.
    fn start_ai_link(&mut self, ctx: &egui::Context, ws_name: String) {
        if self.job.is_some() {
            return;
        }
        // Enumerating other apps' windows needs Accessibility permission.
        self.ax_trusted = window_manager::accessibility_trusted(false);
        if !self.ax_trusted {
            window_manager::accessibility_trusted(true);
            window_manager::open_accessibility_settings();
            self.status = Some(StatusMsg {
                ok: false,
                text: "Cutter needs Accessibility access to see windows. Grant it under \
                       System Settings ▸ Privacy & Security ▸ Accessibility, then try again."
                    .into(),
            });
            return;
        }

        let windows = window_manager::list_windows();
        let Some(ws) = self.workspaces.iter().find(|w| w.workspace.name == ws_name) else {
            return;
        };
        let existing = ws.linked_windows.clone();
        let ws_ctx = ai_link::WsContext {
            name: ws.workspace.name.clone(),
            path: ws.workspace.path.clone(),
            repo_paths: ws
                .repos
                .iter()
                .flat_map(|r| [r.worktree_path.clone(), r.source.clone()])
                .collect(),
        };

        let label = format!("🤖 Auto-linking windows for '{ws_name}'…");
        let name = ws_name;
        self.start_job(ctx, label, move || {
            let suggestion = ai_link::suggest(&ws_ctx, &windows, &existing);
            let n = suggestion.links.len();
            let mut ws = WorkspaceConfig::load(&name).map_err(|e| e.to_string())?;
            ws.linked_windows = suggestion.links;
            ws.save().map_err(|e| e.to_string())?;
            if suggestion.claude_ok {
                Ok(format!("Linked {n} window(s) to '{name}'"))
            } else {
                Ok(format!(
                    "Linked {n} window(s) to '{name}' (Claude unavailable; matched by directory)"
                ))
            }
        });
    }

    /// Open the "Link windows" modal for `ws_name`, snapshotting the live
    /// windows and pre-checking the ones already linked.
    fn open_link_windows(&mut self, ws_name: String) {
        self.ax_trusted = window_manager::accessibility_trusted(false);
        self.link_candidates = if self.ax_trusted {
            window_manager::list_windows()
        } else {
            Vec::new()
        };
        let existing: Vec<LinkedWindow> = self
            .workspaces
            .iter()
            .find(|w| w.workspace.name == ws_name)
            .map(|w| w.linked_windows.clone())
            .unwrap_or_default();
        self.link_checked = self
            .link_candidates
            .iter()
            .map(|c| existing.iter().any(|l| c.matches(l)))
            .collect();
        self.link_for = Some(ws_name);
        self.show_link_windows = true;
    }

    /// Re-enumerate windows for the open modal (e.g. after granting permission).
    fn refresh_link_candidates(&mut self) {
        self.ax_trusted = window_manager::accessibility_trusted(false);
        let existing: Vec<LinkedWindow> = self
            .link_for
            .as_ref()
            .and_then(|name| self.workspaces.iter().find(|w| &w.workspace.name == name))
            .map(|w| w.linked_windows.clone())
            .unwrap_or_default();
        self.link_candidates = if self.ax_trusted {
            window_manager::list_windows()
        } else {
            Vec::new()
        };
        self.link_checked = self
            .link_candidates
            .iter()
            .map(|c| existing.iter().any(|l| c.matches(l)))
            .collect();
    }

    /// Persist a workspace's full set of linked windows.
    fn save_links(&mut self, ws_name: &str, links: Vec<LinkedWindow>) {
        let result = WorkspaceConfig::load(ws_name).and_then(|mut ws| {
            ws.linked_windows = links;
            ws.save()
        });
        match result {
            Ok(()) => {
                self.status = Some(StatusMsg {
                    ok: true,
                    text: format!("Updated linked windows for '{ws_name}'"),
                });
                self.reload();
            }
            Err(e) => {
                self.status = Some(StatusMsg {
                    ok: false,
                    text: e.to_string(),
                });
            }
        }
    }

    /// Remove the link at `idx` from a workspace.
    fn unlink_window(&mut self, ws_name: &str, idx: usize) {
        let Some(ws) = self.workspaces.iter().find(|w| w.workspace.name == ws_name) else {
            return;
        };
        let mut links = ws.linked_windows.clone();
        if idx < links.len() {
            links.remove(idx);
            self.save_links(ws_name, links);
        }
    }

    /// Raise the linked windows of `ws_name`, reporting any that didn't resolve.
    fn activate_workspace(&mut self, ws_name: &str) {
        let Some(ws) = self.workspaces.iter().find(|w| w.workspace.name == ws_name) else {
            return;
        };
        if ws.linked_windows.is_empty() {
            return;
        }
        let links = ws.linked_windows.clone();
        let report = window_manager::raise_windows(&links);
        if !report.unresolved.is_empty() {
            self.status = Some(StatusMsg {
                ok: false,
                text: format!(
                    "Raised {} window(s); couldn't find: {}",
                    report.raised,
                    report.unresolved.join(", ")
                ),
            });
        }
    }

    /// Modal listing open windows so the user can tie a multi-selection to the
    /// workspace. Shows a permission gate when Accessibility access is missing.
    fn link_windows_window(&mut self, ctx: &egui::Context) {
        if !self.show_link_windows {
            return;
        }
        let ws_name = self.link_for.clone().unwrap_or_default();
        let mut close = false;
        let mut do_save = false;
        let mut do_refresh = false;
        let mut request_permission = false;

        egui::Window::new(format!("Link windows · {ws_name}"))
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .default_height(420.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if !self.ax_trusted {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Cutter needs Accessibility access").strong(),
                    );
                    ui.label(
                        "To see and raise other apps' windows, allow Cutter under \
                         System Settings ▸ Privacy & Security ▸ Accessibility.",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Open Accessibility Settings").clicked() {
                            request_permission = true;
                        }
                        if ui.button("Re-check").clicked() {
                            do_refresh = true;
                        }
                    });
                    ui.add_space(8.0);
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                    return;
                }

                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Select windows to tie to this workspace").weak(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("⟳ Rescan").clicked() {
                            do_refresh = true;
                        }
                    });
                });
                ui.separator();

                if self.link_candidates.is_empty() {
                    ui.add_space(6.0);
                    ui.label("No linkable windows found. Open the apps you want, then Rescan.");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show(ui, |ui| {
                            for (i, win) in self.link_candidates.iter().enumerate() {
                                let checked = &mut self.link_checked[i];
                                let title = if win.title.is_empty() {
                                    "(untitled)".to_string()
                                } else {
                                    win.title.clone()
                                };
                                ui.horizontal(|ui| {
                                    ui.checkbox(checked, "");
                                    ui.label(egui::RichText::new(win.app_name.as_str()).strong());
                                    ui.label(egui::RichText::new(title).weak());
                                });
                                if let Some(doc) = &win.document_path {
                                    ui.label(
                                        egui::RichText::new(format!("    {doc}"))
                                            .monospace()
                                            .weak()
                                            .small(),
                                    );
                                }
                            }
                        });
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    let n = self.link_checked.iter().filter(|c| **c).count();
                    if ui
                        .button(format!("Save ({n})"))
                        .on_hover_text("Replace this workspace's linked windows")
                        .clicked()
                    {
                        do_save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if request_permission {
            // Triggers the system prompt; user grants in System Settings.
            window_manager::accessibility_trusted(true);
            window_manager::open_accessibility_settings();
        }
        if do_refresh {
            self.refresh_link_candidates();
        }
        if do_save {
            let links: Vec<LinkedWindow> = self
                .link_candidates
                .iter()
                .zip(&self.link_checked)
                .filter(|(_, checked)| **checked)
                .map(|(win, _)| win.to_link())
                .collect();
            self.save_links(&ws_name, links);
            close = true;
        }
        if close {
            self.show_link_windows = false;
            self.link_for = None;
            self.link_candidates.clear();
            self.link_checked.clear();
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

    /// Modal form for editing an existing base: its repos, base-level
    /// branch-from override, and copy-files list.
    fn edit_base_window(&mut self, ctx: &egui::Context) {
        if !self.show_edit_base {
            return;
        }
        let name = self.edit_base_name.clone();
        let mut close = false;
        let mut do_save = false;

        egui::Window::new(format!("Edit base · {name}"))
            .collapsible(false)
            .resizable(true)
            .default_width(480.0)
            .default_height(440.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if let Some(err) = &self.edit_error {
                    ui.colored_label(egui::Color32::RED, err);
                    ui.add_space(4.0);
                }

                // --- Repos ---
                ui.label(egui::RichText::new("Repositories").strong());
                let mut remove_repo: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .id_salt("edit_repos")
                    .max_height(150.0)
                    .show(ui, |ui| {
                        for (i, repo) in self.edit_repos.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.small_button("✕").on_hover_text("Remove repo").clicked() {
                                    remove_repo = Some(i);
                                }
                                ui.label(egui::RichText::new(repo.name.as_str()).strong());
                                ui.label(egui::RichText::new(repo.path.as_str()).monospace().weak());
                            });
                        }
                    });
                if let Some(i) = remove_repo {
                    self.edit_repos.remove(i);
                }
                if ui.button("Add repo…").clicked() {
                    self.edit_error = None;
                    if let Some(paths) = rfd::FileDialog::new()
                        .set_title("Add repositories to base")
                        .pick_folders()
                    {
                        for p in paths {
                            match commands::base::make_repo_ref(&p) {
                                Ok(repo) => {
                                    if !self.edit_repos.iter().any(|r| r.path == repo.path) {
                                        self.edit_repos.push(repo);
                                    }
                                }
                                Err(e) => self.edit_error = Some(e.to_string()),
                            }
                        }
                    }
                }

                ui.add_space(10.0);
                // --- Base-level branch-from ---
                ui.label(egui::RichText::new("Branch from").strong());
                ui.add(
                    egui::TextEdit::singleline(&mut self.edit_branch_from)
                        .hint_text(format!("empty = inherit ({})", self.default_branch_from))
                        .desired_width(320.0),
                );

                ui.add_space(10.0);
                // --- Copy files ---
                ui.label(egui::RichText::new("Copy files").strong());
                ui.label(
                    egui::RichText::new("Gitignored files copied into each worktree (e.g. .env).")
                        .weak(),
                );
                let mut remove_copy: Option<usize> = None;
                for (i, file) in self.edit_copy_files.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✕").clicked() {
                            remove_copy = Some(i);
                        }
                        ui.label(egui::RichText::new(file.as_str()).monospace());
                    });
                }
                if let Some(i) = remove_copy {
                    self.edit_copy_files.remove(i);
                }
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.edit_new_copy_file)
                            .hint_text(".env")
                            .desired_width(240.0),
                    );
                    let submit =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Add").clicked() || submit)
                        && !self.edit_new_copy_file.trim().is_empty()
                    {
                        let f = self.edit_new_copy_file.trim().to_string();
                        if !self.edit_copy_files.contains(&f) {
                            self.edit_copy_files.push(f);
                        }
                        self.edit_new_copy_file.clear();
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    let can_save = !self.edit_repos.is_empty() && self.job.is_none();
                    if ui
                        .add_enabled(can_save, egui::Button::new("Save"))
                        .on_hover_text("A base needs at least one repo")
                        .clicked()
                    {
                        do_save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if do_save {
            let branch_from = {
                let t = self.edit_branch_from.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            };
            let base = Base {
                repos: self.edit_repos.clone(),
                branch_from,
                copy_files: self.edit_copy_files.clone(),
            };
            match commands::base::update(&name, base) {
                Ok(()) => {
                    self.status = Some(StatusMsg {
                        ok: true,
                        text: format!("Base '{name}' updated"),
                    });
                    self.reload();
                    close = true;
                }
                Err(e) => self.edit_error = Some(e.to_string()),
            }
        }

        if close {
            self.show_edit_base = false;
            self.edit_base_name.clear();
            self.edit_repos.clear();
            self.edit_copy_files.clear();
            self.edit_new_copy_file.clear();
            self.edit_error = None;
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
                // Mode switcher at the top: AI vs. manual.
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.new_ws_mode, NewWsMode::Ai, "🤖 AI");
                    ui.selectable_value(&mut self.new_ws_mode, NewWsMode::Manual, "Manual");
                });
                ui.separator();
                ui.add_space(6.0);

                match self.new_ws_mode {
                    NewWsMode::Ai => {
                        ui.label(
                            egui::RichText::new(
                                "Describe the workspace; a headless Claude session names it, \
                                 picks a base, and creates it.",
                            )
                            .weak(),
                        );
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::multiline(&mut self.new_ws_ai)
                                .hint_text("e.g. fix the SSO redirect bug in ENG-1234")
                                .desired_rows(3)
                                .desired_width(f32::INFINITY),
                        );

                        ui.add_space(8.0);
                        ui.label(egui::RichText::new("Base").strong());
                        if self.bases.is_empty() {
                            ui.colored_label(
                                egui::Color32::RED,
                                "No bases configured. Create a base first (Settings ▸ New base).",
                            );
                        } else {
                            let current = self
                                .new_ws_ai_base
                                .clone()
                                .unwrap_or_else(|| "Select a base".to_string());
                            let names: Vec<String> = self.bases.keys().cloned().collect();
                            egui::ComboBox::from_id_salt("new_ws_ai_base")
                                .selected_text(current)
                                .width(260.0)
                                .show_ui(ui, |ui| {
                                    for n in names {
                                        let selected =
                                            self.new_ws_ai_base.as_deref() == Some(n.as_str());
                                        if ui.selectable_label(selected, &n).clicked() {
                                            self.new_ws_ai_base = Some(n);
                                        }
                                    }
                                });
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.horizontal(|ui| {
                            let ai_ok = !self.new_ws_ai.trim().is_empty()
                                && self.new_ws_ai_base.is_some()
                                && self.job.is_none();
                            if ui
                                .add_enabled(ai_ok, egui::Button::new("🤖 Create with AI"))
                                .clicked()
                            {
                                *action = Some(PendingAction::CreateWorkspaceAi {
                                    prompt: self.new_ws_ai.trim().to_string(),
                                    base: self.new_ws_ai_base.clone(),
                                });
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    }
                    NewWsMode::Manual => {
                        ui.label(egui::RichText::new("Name").strong());
                        ui.text_edit_singleline(&mut self.new_ws_name);
                        if !self.new_ws_name.is_empty() && !name_ok {
                            ui.colored_label(
                                egui::Color32::RED,
                                "Name cannot contain whitespace.",
                            );
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
                                        let selected =
                                            self.new_ws_base.as_deref() == Some(n.as_str());
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
                    }
                }
            });

        if close {
            self.show_new_workspace = false;
            self.new_ws_name.clear();
            self.new_ws_base = None;
            self.new_ws_ai.clear();
            self.new_ws_ai_base = None;
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut do_refresh = false;

        // Closing the window: drop all terminals so their shells terminate.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.terminals.clear();
            self.scratch_terminals.clear();
        }

        // Route any terminal PTY events (title changes, shell exits) collected
        // since the last frame. Backends request a repaint on new output, so
        // this runs promptly without polling.
        while let Ok((id, event)) = self.term_rx.try_recv() {
            self.on_term_event(id, event);
        }

        // Collect any finished PR-status fetches.
        while let Ok((name, prs)) = self.pr_rx.try_recv() {
            self.pr_fetching.remove(&name);
            self.pr_status.insert(name, prs);
        }

        // Collect any finished diff fetches, classifying each into a DiffView
        // (splitting/colour-tagging its lines once) before caching it.
        while let Ok((ws, repo, result)) = self.diff_rx.try_recv() {
            let key = (ws, repo);
            self.diff_fetching.remove(&key);
            self.diff_cache
                .insert(key, result.map(DiffView::from_raw));
        }

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

        let mut toggle_collapsed = false;
        egui::Panel::top("tabs").show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let (icon, hint) = if self.collapsed {
                    (EXPAND_ICON, "Show the terminal and details pane")
                } else {
                    (COLLAPSE_ICON, "Collapse to just the workspace list")
                };
                if ui.button(icon).on_hover_text(hint).clicked() {
                    toggle_collapsed = true;
                }
                // At list width there's no room for the title and tabs, and
                // nothing to switch to: the collapsed view is the list.
                if !self.collapsed {
                    ui.heading("Cutter");
                    ui.separator();
                    ui.selectable_value(&mut self.tab, Tab::Workspaces, "Workspaces");
                    ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let refresh = if self.collapsed { "⟳" } else { "⟳ Refresh" };
                    if ui.button(refresh).on_hover_text("Reload from disk").clicked() {
                        do_refresh = true;
                        // Drop cached PR status and diffs so the selected
                        // workspace refetches both.
                        self.pr_status.clear();
                        self.pr_fetching.clear();
                        self.diff_cache.clear();
                    }
                });
            });
            ui.add_space(4.0);
        });

        // Status bar: spinner while a job runs, else the last result.
        let mut dismiss = false;
        if self.job.is_some() || self.status.is_some() {
            egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
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
        if toggle_collapsed {
            self.toggle_collapsed(&ctx);
        }

        // Reload once, after the top panel's Refresh button has had a chance to
        // request it, so a manual refresh takes effect this frame.
        if do_refresh {
            self.reload();
        }

        match self.tab {
            Tab::Workspaces => self.workspaces_ui(ui),
            Tab::Settings => self.settings_ui(ui),
        }

        // Floating forms collect intent into `action`, applied once their
        // borrows of `self` have ended.
        let mut action: Option<PendingAction> = None;
        self.new_base_window(&ctx, &mut action);
        self.edit_base_window(&ctx);
        self.new_workspace_window(&ctx, &mut action);
        self.confirm_window(&ctx, &mut action);
        self.link_windows_window(&ctx);
        if let Some(action) = action {
            self.dispatch(&ctx, action);
        }
    }
}

/// A label/value pair as one row of a two-column [`egui::Grid`].
fn meta_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(egui::RichText::new(label).strong());
    ui.label(value);
    ui.end_row();
}

/// The Phosphor icon glyph and colour used to render a Claude session state.
fn state_icon(state: SessionState) -> (&'static str, egui::Color32) {
    match state {
        SessionState::Running => (RUNNING_ICON, RUNNING_COLOR),
        SessionState::Waiting => (WAITING_ICON, WAITING_COLOR),
    }
}

/// Draw the inline rename text field for a terminal. Requests focus on the first
/// render, then returns `Some(true)` to commit (Enter or click-away),
/// `Some(false)` to cancel (Escape), or `None` while still editing.
fn rename_field(ui: &mut egui::Ui, r: &mut Renaming, width: f32) -> Option<bool> {
    let resp = ui.add(egui::TextEdit::singleline(&mut r.buffer).desired_width(width));
    if r.focus {
        resp.request_focus();
        r.focus = false;
    }
    if resp.lost_focus() {
        // Escape cancels; Enter or clicking elsewhere commits.
        return Some(!ui.input(|i| i.key_pressed(egui::Key::Escape)));
    }
    None
}

/// The chip colour for a PR status.
fn pr_color(state: pr::PrState) -> egui::Color32 {
    match state {
        pr::PrState::Draft => PR_DRAFT_COLOR,
        pr::PrState::Open => PR_OPEN_COLOR,
        pr::PrState::Merged => PR_MERGED_COLOR,
    }
}

/// Hover text for a workspace's status icon, e.g. "Claude: running (2)".
fn status_hover(state: SessionState, status: WorkspaceStatus) -> String {
    let n = match state {
        SessionState::Running => status.running,
        SessionState::Waiting => status.waiting,
    };
    format!("Claude: {} ({})", state.label(), n)
}
