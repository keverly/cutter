//! The Cutter menu bar app (`Cutter Menu.app`, the `cutter-menubar` binary): a
//! status item whose native menu lists every workspace with its Claude status
//! and pull requests, and creates and removes workspaces through small AppKit
//! dialogs. It stands alone: no window, no Dock icon, and it never opens
//! Cutter.app.
//!
//! Everything AppKit happens on the main thread, inside `NSApplication::run`.
//! Slow work (creating, removing, opening in Claude Desktop, the `gh` PR
//! checks) runs on worker threads and reports back over channels that a timer
//! drains. The menu itself is rebuilt from disk each time it opens.

use std::cell::{Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication,
    NSApplicationActivationPolicy, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSImage,
    NSMenu, NSMenuDelegate, NSMenuItem, NSPopUpButton, NSStatusBar, NSStatusItem, NSTextField,
    NSView, NSVariableStatusItemLength,
};
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSMutableAttributedString,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer,
};

use crate::cli::ClaudeMode;
use crate::commands;
use crate::config::Config;
use crate::pr::{self, PrInfo, PrState};
use crate::session::{self, SessionState, WorkspaceStatus};
use crate::workspace::WorkspaceConfig;

/// The status item's icon, and the one it shows while a job runs: SF Symbols
/// drawn as template images, so they follow the menu bar's appearance.
const ICON: &str = "arrow.triangle.branch";
const BUSY_ICON: &str = "arrow.triangle.2.circlepath";

/// How often the timer collects finished work, in seconds.
const TICK: f64 = 1.0;
/// How often workspaces are re-read between menu opens, so ones created from
/// the CLI or Cutter.app get their PRs checked too.
const RELOAD_EVERY: Duration = Duration::from_secs(30);
/// How often every workspace's PR status is checked again, so a PR merged
/// while the app sat in the menu bar reads as merged.
const PR_RECHECK: Duration = Duration::from_secs(5 * 60);
/// How long the last job's outcome stays at the top of the menu.
const OUTCOME_SHOWN_FOR: Duration = Duration::from_secs(2 * 60);
/// The longest PR title shown in a workspace's submenu; the rest is in its
/// tooltip.
const PR_TITLE_MAX: usize = 48;
/// Width of the create dialogs' fields.
const FORM_WIDTH: f64 = 300.0;

const NO_BASES: &str = "Add one with `cutter base add`, or under Settings in Cutter.app.";

/// Run the menu bar app until it's quit.
pub fn run() {
    let mtm = MainThreadMarker::new().expect("the menu bar app runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon or app menu: the status item is the whole UI.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let _controller = Controller::new(mtm);
    app.run();
}

/// What a menu item does when picked.
#[derive(Clone)]
enum Action {
    NewWorkspace,
    NewWorkspaceWithAi,
    OpenInClaudeDesktop(String),
    OpenInTerminal(String),
    ShowInFinder(String),
    OpenUrl(String),
    RemoveWorkspace(String),
    CheckPullRequests,
    Quit,
}

/// A worker job's result.
struct JobOutcome {
    ok: bool,
    message: String,
}

/// One workspace's inputs for a PR-status fetch: its name, its branch, and its
/// repos as `(repo name, worktree dir)` pairs.
type PrFetchJob = (String, String, Vec<(String, String)>);

#[derive(Default)]
struct State {
    workspaces: Vec<WorkspaceConfig>,
    workspaces_error: Option<String>,
    bases: Vec<String>,
    claude: HashMap<String, WorkspaceStatus>,
    reloaded_at: Option<Instant>,

    prs: HashMap<String, Vec<PrInfo>>,
    pr_fetching: HashSet<String>,
    pr_swept_at: Option<Instant>,

    /// The running job's label; one job at a time.
    job: Option<String>,
    /// The last job's outcome, and when it landed.
    last: Option<(JobOutcome, Instant)>,
    /// Whether the status item shows the busy icon (`None` before the first).
    shows_busy: Option<bool>,

    /// The action behind each item of the menu as last built, by item tag.
    actions: Vec<Action>,

    /// What the create dialogs remember between uses.
    last_base: Option<String>,
    open_in_desktop: bool,
}

struct Ivars {
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    state: RefCell<State>,
    pr_tx: Sender<(String, Vec<PrInfo>)>,
    pr_rx: Receiver<(String, Vec<PrInfo>)>,
    job_tx: Sender<JobOutcome>,
    job_rx: Receiver<JobOutcome>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Controller
    // doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CutterMenuBarController"]
    #[ivars = Ivars]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSMenuDelegate for Controller {
        /// The menu is about to open: re-read workspaces and rebuild it.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.reload();
            self.rebuild(menu);
            self.sweep_prs();
        }
    }

    impl Controller {
        /// Every actionable menu item's action.
        #[unsafe(method(pick:))]
        fn pick(&self, sender: &NSMenuItem) {
            let action = usize::try_from(sender.tag())
                .ok()
                .and_then(|i| self.state().actions.get(i).cloned());
            if let Some(action) = action {
                self.perform(action);
            }
        }

        /// The timer: collect finished work.
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.on_tick();
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let status_item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let menu = NSMenu::new(mtm);
        status_item.setMenu(Some(&menu));
        let (pr_tx, pr_rx) = std::sync::mpsc::channel();
        let (job_tx, job_rx) = std::sync::mpsc::channel();
        let this = Self::alloc(mtm).set_ivars(Ivars {
            status_item,
            menu,
            state: RefCell::new(State::default()),
            pr_tx,
            pr_rx,
            job_tx,
            job_rx,
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };

        this.ivars().menu.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        this.set_icon(false);
        this.reload();
        this.sweep_prs();
        // The run loop keeps the timer, and the timer keeps its target.
        unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                TICK,
                &this,
                sel!(tick:),
                None,
                true,
            );
        }
        this
    }

    fn state(&self) -> Ref<'_, State> {
        self.ivars().state.borrow()
    }

    fn state_mut(&self) -> RefMut<'_, State> {
        self.ivars().state.borrow_mut()
    }

    /// Re-read workspaces, their Claude status and the bases from disk.
    fn reload(&self) {
        let (workspaces, error) = match WorkspaceConfig::list_all() {
            Ok(ws) => (ws, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        let claude = session::aggregate(&session::load_active(&workspaces));
        let bases = Config::load()
            .map(|c| c.bases.keys().cloned().collect())
            .unwrap_or_default();

        let mut st = self.state_mut();
        // Forget the PR status of workspaces that are gone.
        st.prs
            .retain(|name, _| workspaces.iter().any(|w| &w.workspace.name == name));
        st.workspaces = workspaces;
        st.workspaces_error = error;
        st.claude = claude;
        st.bases = bases;
        st.reloaded_at = Some(Instant::now());
    }

    fn on_tick(&self) {
        let mut failed = None;
        let mut reload = false;
        {
            let ivars = self.ivars();
            let mut st = self.state_mut();
            while let Ok((name, prs)) = ivars.pr_rx.try_recv() {
                st.pr_fetching.remove(&name);
                st.prs.insert(name, prs);
            }
            if let Ok(outcome) = ivars.job_rx.try_recv() {
                st.job = None;
                if !outcome.ok {
                    failed = Some(outcome.message.clone());
                }
                st.last = Some((outcome, Instant::now()));
                reload = true;
            }
            reload |= st.reloaded_at.is_none_or(|t| t.elapsed() >= RELOAD_EVERY);
        }
        if reload {
            self.reload();
        }
        self.sweep_prs();
        let busy = self.state().job.is_some();
        self.set_icon(busy);
        // After every borrow is released: the alert runs a modal loop.
        if let Some(message) = failed {
            show_error(self.mtm(), "Cutter", &message);
        }
    }

    /// Check PR status for every workspace that has none yet, and for all of
    /// them every `PR_RECHECK`. One background thread runs the (slow) `gh`
    /// calls in turn; results land on `pr_rx`.
    fn sweep_prs(&self) {
        let mut st = self.state_mut();
        if !st.pr_fetching.is_empty() {
            return;
        }
        let recheck = st.pr_swept_at.is_some_and(|t| t.elapsed() >= PR_RECHECK);
        let pending: Vec<PrFetchJob> = st
            .workspaces
            .iter()
            .filter(|ws| recheck || !st.prs.contains_key(&ws.workspace.name))
            .map(|ws| {
                let repos = ws
                    .repos
                    .iter()
                    .map(|r| (r.name.clone(), r.worktree_path.clone()))
                    .collect();
                (ws.workspace.name.clone(), ws.workspace.branch.clone(), repos)
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        st.pr_fetching
            .extend(pending.iter().map(|(name, _, _)| name.clone()));
        st.pr_swept_at = Some(Instant::now());
        let tx = self.ivars().pr_tx.clone();
        std::thread::spawn(move || {
            for (name, branch, repos) in pending {
                let prs = if repos.is_empty() {
                    Vec::new()
                } else {
                    pr::fetch(&repos, &branch)
                };
                let _ = tx.send((name, prs));
            }
        });
    }

    /// Run `op` on a worker thread as the one job; the timer collects its
    /// outcome. Ignored while another job runs (the menu disables what would
    /// start one).
    fn start_job<F>(&self, label: String, op: F)
    where
        F: FnOnce() -> Result<String, String> + Send + 'static,
    {
        {
            let mut st = self.state_mut();
            if st.job.is_some() {
                return;
            }
            st.job = Some(label);
            st.last = None;
        }
        self.set_icon(true);
        let tx = self.ivars().job_tx.clone();
        std::thread::spawn(move || {
            let outcome = match op() {
                Ok(message) => JobOutcome { ok: true, message },
                Err(message) => JobOutcome { ok: false, message },
            };
            let _ = tx.send(outcome);
        });
    }

    fn perform(&self, action: Action) {
        let mtm = self.mtm();
        match action {
            Action::NewWorkspace => {
                let (bases, last_base, desktop) = self.dialog_defaults();
                if bases.is_empty() {
                    show_error(mtm, "No bases configured", NO_BASES);
                    return;
                }
                let Some(req) = ask_new_workspace(mtm, &bases, last_base.as_deref(), desktop)
                else {
                    return;
                };
                self.remember(Some(req.base.clone()), req.desktop);
                let label = format!("Creating '{}'…", req.name);
                self.start_job(label, move || {
                    commands::create::run(Some(&req.name), Some(&req.base), false, ClaudeMode::None)
                        .map_err(|e| e.to_string())?;
                    if req.desktop {
                        commands::open::run(&req.name, ClaudeMode::Desktop)
                            .map_err(|e| format!("Created '{}', but {e}", req.name))?;
                    }
                    Ok(format!("Created '{}'", req.name))
                });
            }
            Action::NewWorkspaceWithAi => {
                let (bases, last_base, desktop) = self.dialog_defaults();
                if bases.is_empty() {
                    show_error(mtm, "No bases configured", NO_BASES);
                    return;
                }
                let Some(req) = ask_new_workspace_ai(mtm, &bases, last_base.as_deref(), desktop)
                else {
                    return;
                };
                self.remember(req.base.clone(), req.desktop);
                self.start_job("Creating a workspace with AI…".into(), move || {
                    let name = commands::ai::run(&req.prompt, req.base.as_deref())
                        .map_err(|e| e.to_string())?;
                    if req.desktop {
                        commands::open::run(&name, ClaudeMode::Desktop)
                            .map_err(|e| format!("Created '{name}', but {e}"))?;
                    }
                    Ok(format!("Created '{name}'"))
                });
            }
            Action::OpenInClaudeDesktop(name) => {
                let label = format!("Opening '{name}' in Claude Desktop…");
                self.start_job(label, move || {
                    commands::open::run(&name, ClaudeMode::Desktop).map_err(|e| e.to_string())?;
                    Ok(format!("Opened '{name}' in Claude Desktop"))
                });
            }
            Action::OpenInTerminal(path) => open(mtm, &["-a", "Terminal", &path]),
            Action::ShowInFinder(path) => open(mtm, &[&path]),
            Action::OpenUrl(url) => open(mtm, &[&url]),
            Action::RemoveWorkspace(name) => {
                let live = self
                    .state()
                    .claude
                    .get(&name)
                    .is_some_and(|s| s.state().is_some());
                if !confirm_remove(mtm, &name, live) {
                    return;
                }
                let label = format!("Removing '{name}'…");
                self.start_job(label, move || {
                    commands::remove::run(&name, false).map_err(|e| e.to_string())?;
                    Ok(format!("Removed '{name}'"))
                });
            }
            Action::CheckPullRequests => {
                let mut st = self.state_mut();
                if st.pr_fetching.is_empty() {
                    // Make the next sweep due now: it re-checks every workspace.
                    st.pr_swept_at = Instant::now().checked_sub(PR_RECHECK);
                }
                drop(st);
                self.sweep_prs();
            }
            Action::Quit => NSApplication::sharedApplication(mtm).terminate(None),
        }
    }

    /// The bases, and the base and "Open in Claude Desktop" choice the create
    /// dialogs start from.
    fn dialog_defaults(&self) -> (Vec<String>, Option<String>, bool) {
        let st = self.state();
        (st.bases.clone(), st.last_base.clone(), st.open_in_desktop)
    }

    fn remember(&self, base: Option<String>, desktop: bool) {
        let mut st = self.state_mut();
        if base.is_some() {
            st.last_base = base;
        }
        st.open_in_desktop = desktop;
    }

    /// Show the busy icon while a job runs, with the job as the tooltip.
    fn set_icon(&self, busy: bool) {
        {
            let mut st = self.state_mut();
            if st.shows_busy == Some(busy) {
                return;
            }
            st.shows_busy = Some(busy);
        }
        let Some(button) = self.ivars().status_item.button(self.mtm()) else {
            return;
        };
        let symbol = if busy { BUSY_ICON } else { ICON };
        match NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol),
            Some(&NSString::from_str("Cutter")),
        ) {
            Some(image) => {
                image.setTemplate(true);
                button.setImage(Some(&image));
            }
            None => button.setTitle(&NSString::from_str("Cutter")),
        }
        let tip = self.state().job.clone().unwrap_or_else(|| "Cutter".into());
        button.setToolTip(Some(&NSString::from_str(&tip)));
    }

    /// Fill `menu` from the current state.
    fn rebuild(&self, menu: &NSMenu) {
        menu.removeAllItems();
        let mut b = Builder {
            mtm: self.mtm(),
            target: self,
            actions: Vec::new(),
        };
        {
            let st = self.state();
            let busy = st.job.is_some();

            if let Some(job) = &st.job {
                b.label(menu, job);
                b.separator(menu);
            } else if let Some((outcome, at)) = &st.last {
                if at.elapsed() < OUTCOME_SHOWN_FOR {
                    let mark = if outcome.ok { "✓" } else { "✗" };
                    b.label(menu, &format!("{mark} {}", outcome.message));
                    b.separator(menu);
                }
            }

            if let Some(error) = &st.workspaces_error {
                b.label(menu, &format!("Couldn't read workspaces: {error}"));
            } else if st.workspaces.is_empty() {
                b.label(menu, "No workspaces");
            } else {
                b.label(menu, "Workspaces");
                for ws in &st.workspaces {
                    let name = &ws.workspace.name;
                    let claude = st.claude.get(name).and_then(|s| s.state());
                    let prs = st.prs.get(name).map(Vec::as_slice);
                    b.workspace(menu, ws, claude, prs, busy);
                }
            }

            b.separator(menu);
            b.maybe(menu, !busy, "New Workspace…", Action::NewWorkspace);
            b.maybe(menu, !busy, "New Workspace with AI…", Action::NewWorkspaceWithAi);
            b.separator(menu);
            b.maybe(
                menu,
                st.pr_fetching.is_empty(),
                "Check Pull Requests Now",
                Action::CheckPullRequests,
            );
            b.action(menu, "Quit Cutter Menu", Action::Quit);
        }
        self.state_mut().actions = b.actions;
    }
}

/// Builds the menu, recording the action behind each item it adds.
struct Builder<'a> {
    mtm: MainThreadMarker,
    target: &'a Controller,
    actions: Vec<Action>,
}

impl Builder<'_> {
    /// A workspace's row (Claude dot, name, PR states), opening a submenu of
    /// what can be done with it.
    fn workspace(
        &mut self,
        menu: &NSMenu,
        ws: &WorkspaceConfig,
        claude: Option<SessionState>,
        prs: Option<&[PrInfo]>,
        busy: bool,
    ) {
        let name = ws.workspace.name.clone();
        let path = ws.workspace.path.clone();
        let row = self.label(menu, &name);
        row.setAttributedTitle(Some(&workspace_title(&name, claude, prs)));

        let sub = NSMenu::new(self.mtm);
        self.maybe(
            &sub,
            !busy,
            "Open in Claude Desktop",
            Action::OpenInClaudeDesktop(name.clone()),
        );
        self.action(&sub, "Open in Terminal", Action::OpenInTerminal(path.clone()));
        self.action(&sub, "Show in Finder", Action::ShowInFinder(path));
        self.separator(&sub);
        match prs {
            None => {
                self.label(&sub, "Checking pull requests…");
            }
            Some([]) => {
                self.label(&sub, "No pull requests");
            }
            Some(prs) => {
                for pr in prs {
                    let item = self.action(&sub, &pr.title, Action::OpenUrl(pr.url.clone()));
                    item.setAttributedTitle(Some(&pr_title(pr)));
                    let tip = format!("{}\n{}", pr.title, pr.url);
                    item.setToolTip(Some(&NSString::from_str(&tip)));
                }
            }
        }
        self.separator(&sub);
        self.maybe(&sub, !busy, "Remove…", Action::RemoveWorkspace(name));
        // An item with a submenu is enabled whatever its action.
        row.setSubmenu(Some(&sub));
    }

    /// An item that performs `action` when picked.
    fn action(&mut self, menu: &NSMenu, title: &str, action: Action) -> Retained<NSMenuItem> {
        let item = menu_item(self.mtm, title, true);
        let target: &AnyObject = self.target;
        unsafe { item.setTarget(Some(target)) };
        item.setTag(self.actions.len() as isize);
        self.actions.push(action);
        menu.addItem(&item);
        item
    }

    /// `action`'s item while `enabled`, a disabled one otherwise (used while a
    /// job runs, for what would start another).
    fn maybe(&mut self, menu: &NSMenu, enabled: bool, title: &str, action: Action) {
        if enabled {
            self.action(menu, title, action);
        } else {
            self.label(menu, title);
        }
    }

    /// An item with no action, which the menu draws disabled.
    fn label(&self, menu: &NSMenu, title: &str) -> Retained<NSMenuItem> {
        let item = menu_item(self.mtm, title, false);
        menu.addItem(&item);
        item
    }

    fn separator(&self, menu: &NSMenu) {
        menu.addItem(&NSMenuItem::separatorItem(self.mtm));
    }
}

fn menu_item(mtm: MainThreadMarker, title: &str, actionable: bool) -> Retained<NSMenuItem> {
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            actionable.then_some(sel!(pick:)),
            &NSString::from_str(""),
        )
    }
}

/// `● name    #412 open, #88 merged`: the dot in the Claude status colour,
/// each PR in its state's colour.
fn workspace_title(
    name: &str,
    claude: Option<SessionState>,
    prs: Option<&[PrInfo]>,
) -> Retained<NSAttributedString> {
    let (dot, dot_color) = match claude {
        Some(SessionState::Running) => ("●", NSColor::systemOrangeColor()),
        Some(SessionState::Waiting) => ("●", NSColor::systemGreenColor()),
        None => ("○", NSColor::tertiaryLabelColor()),
    };
    let mut parts = vec![(format!("{dot}  "), Some(dot_color)), (name.to_string(), None)];
    for (i, pr) in prs.unwrap_or_default().iter().enumerate() {
        let sep = if i == 0 { "    " } else { ", " };
        parts.push((sep.to_string(), Some(NSColor::secondaryLabelColor())));
        parts.push((
            format!("#{} {}", pr.number, pr.state.label()),
            Some(pr_color(pr.state)),
        ));
    }
    styled(&parts)
}

/// `backend #412 open — Fix the SSO redirect`, the state in its colour.
fn pr_title(pr: &PrInfo) -> Retained<NSAttributedString> {
    let mut title: String = pr.title.chars().take(PR_TITLE_MAX).collect();
    if pr.title.chars().count() > PR_TITLE_MAX {
        title.push('…');
    }
    styled(&[
        (format!("{} #{} ", pr.repo, pr.number), None),
        (pr.state.label().to_string(), Some(pr_color(pr.state))),
        (format!(" — {title}"), Some(NSColor::secondaryLabelColor())),
    ])
}

fn pr_color(state: PrState) -> Retained<NSColor> {
    match state {
        PrState::Draft => NSColor::secondaryLabelColor(),
        PrState::Open => NSColor::systemGreenColor(),
        PrState::Merged => NSColor::systemPurpleColor(),
    }
}

/// Join `parts` into one string, each in its colour (the menu's text colour
/// when `None`), all in the menu font: an attributed title without a font
/// falls back to 12 pt Helvetica.
fn styled(parts: &[(String, Option<Retained<NSColor>>)]) -> Retained<NSAttributedString> {
    let out = NSMutableAttributedString::new();
    let font = NSFont::menuFontOfSize(0.0);
    for (text, color) in parts {
        let mut keys: Vec<&NSAttributedStringKey> = vec![unsafe { NSFontAttributeName }];
        let mut values: Vec<&AnyObject> = vec![&font];
        if let Some(color) = color {
            keys.push(unsafe { NSForegroundColorAttributeName });
            values.push(color);
        }
        let attributes = NSDictionary::from_slices(&keys, &values);
        let part = unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &NSString::from_str(text),
                Some(&attributes),
            )
        };
        out.appendAttributedString(&part);
    }
    Retained::into_super(out)
}

// Dialogs ---------------------------------------------------------------------

struct NewWorkspace {
    name: String,
    base: String,
    desktop: bool,
}

struct NewWorkspaceAi {
    prompt: String,
    /// `None` lets Claude pick the base.
    base: Option<String>,
    desktop: bool,
}

/// Ask for a new workspace's name and base. Re-asks, saying why, while the
/// name isn't one cutter would take.
fn ask_new_workspace(
    mtm: MainThreadMarker,
    bases: &[String],
    last_base: Option<&str>,
    desktop: bool,
) -> Option<NewWorkspace> {
    let name_label = NSTextField::labelWithString(&NSString::from_str("Name"), mtm);
    let name = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
    name.setPlaceholderString(Some(&NSString::from_str("short-hyphenated-name")));
    let base_label = NSTextField::labelWithString(&NSString::from_str("Base"), mtm);
    let base = popup(mtm, bases.iter().map(String::as_str), last_base);
    let open = checkbox(mtm, "Open in Claude Desktop", desktop);
    let form = form(
        mtm,
        &[
            (&name_label, 17.0),
            (&name, 22.0),
            (&base_label, 17.0),
            (&base, 26.0),
            (&open, 18.0),
        ],
    );

    // No subtext until there's a problem with the name to explain.
    let alert = alert(mtm, "New Workspace", "");
    alert.addButtonWithTitle(&NSString::from_str("Create"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.setAccessoryView(Some(&form));
    alert.layout();
    alert.window().setInitialFirstResponder(Some(&name));

    loop {
        if run_modal(mtm, &alert) != NSAlertFirstButtonReturn {
            return None;
        }
        let value = name.stringValue().to_string().trim().to_string();
        match check_name(&value) {
            Ok(()) => {
                return Some(NewWorkspace {
                    name: value,
                    base: selected(&base)?,
                    desktop: open.state() == NSControlStateValueOn,
                });
            }
            Err(why) => alert.setInformativeText(&NSString::from_str(&why)),
        }
    }
}

/// Ask for a description for AI-driven creation.
fn ask_new_workspace_ai(
    mtm: MainThreadMarker,
    bases: &[String],
    last_base: Option<&str>,
    desktop: bool,
) -> Option<NewWorkspaceAi> {
    const ANY_BASE: &str = "Let Claude choose";
    // A wrapping label made editable: a text field that wraps its lines.
    let prompt = NSTextField::wrappingLabelWithString(&NSString::from_str(""), mtm);
    prompt.setEditable(true);
    prompt.setSelectable(true);
    prompt.setBezeled(true);
    prompt.setDrawsBackground(true);
    prompt.setPlaceholderString(Some(&NSString::from_str(
        "e.g. fix the SSO redirect bug in ENG-1234",
    )));
    let base_label = NSTextField::labelWithString(&NSString::from_str("Base"), mtm);
    let choices = std::iter::once(ANY_BASE).chain(bases.iter().map(String::as_str));
    let base = popup(mtm, choices, last_base);
    let open = checkbox(mtm, "Open in Claude Desktop", desktop);
    let form = form(
        mtm,
        &[(&prompt, 64.0), (&base_label, 17.0), (&base, 26.0), (&open, 18.0)],
    );

    let alert = alert(
        mtm,
        "New Workspace with AI",
        "Describe the work; a headless Claude session names the workspace, picks a base, \
         and creates it.",
    );
    alert.addButtonWithTitle(&NSString::from_str("Create"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.setAccessoryView(Some(&form));
    alert.layout();
    alert.window().setInitialFirstResponder(Some(&prompt));

    loop {
        if run_modal(mtm, &alert) != NSAlertFirstButtonReturn {
            return None;
        }
        let text = prompt.stringValue().to_string().trim().to_string();
        if text.is_empty() {
            alert.setInformativeText(&NSString::from_str("Describe the work first."));
            continue;
        }
        return Some(NewWorkspaceAi {
            prompt: text,
            base: selected(&base).filter(|b| b != ANY_BASE),
            desktop: open.state() == NSControlStateValueOn,
        });
    }
}

/// Ask before removing; `live` says a Claude session is running in it.
fn confirm_remove(mtm: MainThreadMarker, name: &str, live: bool) -> bool {
    let mut info = "This removes its worktrees (and any uncommitted changes in them), deletes \
                    their branches where possible, and deletes the workspace directory."
        .to_string();
    if live {
        info.push_str("\n\nA Claude session is still running in it.");
    }
    let alert = alert(mtm, &format!("Remove workspace '{name}'?"), &info);
    alert.setAlertStyle(NSAlertStyle::Critical);
    alert.addButtonWithTitle(&NSString::from_str("Remove"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    run_modal(mtm, &alert) == NSAlertFirstButtonReturn
}

fn show_error(mtm: MainThreadMarker, title: &str, message: &str) {
    let alert = alert(mtm, title, message);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    run_modal(mtm, &alert);
}

/// Open a file, folder or URL with `open(1)`, saying so when that fails.
fn open(mtm: MainThreadMarker, args: &[&str]) {
    if let Err(e) = std::process::Command::new("/usr/bin/open").args(args).spawn() {
        show_error(mtm, "Couldn't open it", &e.to_string());
    }
}

/// A name cutter accepts for a workspace (and its branch) that isn't taken.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Enter a name for the workspace.".into());
    }
    if name.chars().any(char::is_whitespace) {
        return Err("A workspace name can't contain spaces.".into());
    }
    if WorkspaceConfig::exists(name).unwrap_or(false) {
        return Err(format!("A workspace named '{name}' already exists."));
    }
    Ok(())
}

fn alert(mtm: MainThreadMarker, message: &str, info: &str) -> Retained<NSAlert> {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(info));
    alert
}

/// Run `alert` in front of whatever app is active: an accessory app has to
/// activate itself first, or its alert opens behind other windows.
fn run_modal(mtm: MainThreadMarker, alert: &NSAlert) -> isize {
    // `activate` replaces this on macOS 14+, but Cutter supports 11.
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    alert.runModal()
}

fn popup<'a>(
    mtm: MainThreadMarker,
    titles: impl IntoIterator<Item = &'a str>,
    selected: Option<&str>,
) -> Retained<NSPopUpButton> {
    let popup = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        rect(0.0, 0.0, FORM_WIDTH, 26.0),
        false,
    );
    for title in titles {
        popup.addItemWithTitle(&NSString::from_str(title));
    }
    if let Some(title) = selected {
        popup.selectItemWithTitle(&NSString::from_str(title));
    }
    popup
}

fn selected(popup: &NSPopUpButton) -> Option<String> {
    popup.titleOfSelectedItem().map(|t| t.to_string())
}

fn checkbox(mtm: MainThreadMarker, title: &str, on: bool) -> Retained<NSButton> {
    let button = unsafe {
        NSButton::checkboxWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
    };
    button.setState(if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    button
}

/// Stack `rows` top to bottom, each at its height and the form's width.
fn form(mtm: MainThreadMarker, rows: &[(&NSView, f64)]) -> Retained<NSView> {
    const GAP: f64 = 6.0;
    let height =
        rows.iter().map(|(_, h)| h).sum::<f64>() + GAP * rows.len().saturating_sub(1) as f64;
    let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, FORM_WIDTH, height));
    // AppKit's origin is bottom-left: lay out from the top down.
    let mut top = height;
    for (row, h) in rows {
        top -= h;
        row.setFrame(rect(0.0, top, FORM_WIDTH, *h));
        view.addSubview(row);
        top -= GAP;
    }
    view
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}
