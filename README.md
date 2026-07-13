# Cutter

Git worktree manager for multi-repo projects.

When working across multiple repositories on the same feature, you usually need to give an agent access to all repositires (frontend, backend, shared-libs). Cutter automates this with workspaces and worktrees.

## Concepts

- **Base** — A named template that lists local git repositories that belong together. For example, a "platform" base might include your frontend, backend, and shared-libs repos.
- **Workspace** — A directory created from a base. When you create a workspace called `my-feature`, cutter runs `git worktree add` on each repo in the base, creating worktrees all on a branch named `my-feature`. The worktrees are grouped together under `~/cutter/my-feature/`.

## Prerequisites

- [Rust](https://rustup.rs/) — install with `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- [Git](https://git-scm.com/) — cutter shells out to `git` for worktree operations

## Install

```sh
git clone git@github.com:keverly/cutter.git
cd cutter
cargo install --path .
```

## Update

```sh
cd cutter
git pull
cargo install --path .
```

## GUI (Cutter.app)

Cutter ships an optional standalone macOS app that lists your active
workspaces and shows your base configurations. It's built separately from the
CLI so the CLI stays lean.

```sh
# Build a double-clickable dist/Cutter.app
./scripts/build-app.sh

# Run it, or install it to /Applications
open dist/Cutter.app
cp -r dist/Cutter.app /Applications/
```

The window has two tabs:

- **Workspaces** — every active workspace; click one to open a terminal rooted
  at it (see [Terminal per workspace](#terminal-per-workspace)), or switch to
  **Details** for its base, branch, path, and per-repo worktrees. Use **➕ New**
  to create a workspace — either by describing it and letting Claude set it up
  (see [AI-driven creation](#ai-driven-creation)) or by filling in a name and
  base — and **🗑 Remove** (in Details) to tear one down.
- **Settings** — the workspace root, default branch-from, and each base with
  its repos, `branch_from`, and `copy_files`. Use **➕ New base** to define one
  (browse for repo folders or type paths), **Edit** to add/remove its repos and
  change its branch-from and copy files, and **Remove** to delete a base
  definition.

Creating and removing run the same logic as the CLI commands, on a background
thread so the window stays responsive during `git fetch`/worktree work; a
status bar shows progress and the result. Destructive actions ask for
confirmation first. The list auto-refreshes when `~/.config/cutter` changes, or
use **⟳ Refresh** to re-read manually.

### Terminal per workspace

Selecting a workspace opens a terminal rooted at that workspace's directory, so
you land at a shell across your worktrees with no external terminal app. The
pane has a **Terminal** / **Details** toggle at the top:

- **Terminal** — an embedded terminal with tabs. Use **＋** to open another tab
  (each is a fresh `$SHELL` in the workspace directory) and **✕** to close one.
  Each workspace keeps its own independent set of terminal tabs.
- **Details** — the workspace's base, branch, path, per-repo worktrees, linked
  windows, and the **🗑 Remove** action.

The terminal is an [`egui_term`](https://github.com/Harzu/egui_term) widget
(alacritty's VT engine over a PTY), rendered inline in the Cutter window.

> The GUI reads and writes the same `~/.config/cutter` data the CLI uses. To run
> it without bundling, use `cargo run --features gui --bin cutter-gui`.

### Linking macOS windows to a workspace

You can tie real macOS windows to a workspace and bring them forward with one
click — useful when you keep, say, an Xcode window and its Simulator per
workspace (including two windows of the *same* app for different workspaces).

- In a workspace's detail pane, click **⧉ Link windows…**. Pick any open windows
  from the list — including ones on **other Spaces (Mission Control desktops)**,
  multi-select — e.g. Xcode *and* the Simulator) and **Save**. Workspaces with
  links show a `⧉` marker in the list.
- **Click the workspace** to raise its linked windows to the foreground. If a
  linked window lives on another Space, Cutter **switches to that Space** first
  (so clicking a workspace from Space 2 jumps you to Space 1 where its windows
  are). The ✕ next to a link removes it.

Links are stored per-workspace (in its `.toml`) as stable descriptors — the app
name, window title, and the open document path when the app exposes one (e.g.
Xcode) — and re-resolved against live windows each time, so they keep working
after you quit and reopen an app. If a window can't be found, the status bar
says which.

This uses the macOS **Accessibility API**, so the first time you open the link
picker macOS will ask you to allow Cutter under **System Settings ▸ Privacy &
Security ▸ Accessibility** (the picker has a button that opens the pane). The
app is ad-hoc signed by `build-app.sh`; because that isn't a Developer ID
signature, macOS may ask you to re-grant Accessibility after each rebuild.

Enumerating windows across Spaces and switching Spaces use private CGS/SkyLight
symbols (`SLSCopySpacesForWindows`, `SLSManagedDisplaySetCurrentSpace`, and
`_AXUIElementGetWindow`) — the same undocumented APIs AltTab and yabai use.
They need no extra permission, but they're unsupported by Apple and could change
across major macOS releases; the SkyLight framework is linked in `build.rs`. If
the Space layout can't be read, Cutter simply skips the Space switch and raises
on the current Space.

## Quick Start

```sh
# Define a base with your repos
cutter base add platform ~/repos/frontend ~/repos/backend ~/repos/shared-libs

# Create a workspace interactively (prompts for name, base, etc.)
cutter create

# Or pass arguments directly
cutter create my-feature --base platform

# Check status
cutter status my-feature

# Print workspace path (useful for cd $(cutter create ... --print))
cutter locate my-feature

# Or launch claude in the workspace
cutter open-claude my-feature

# Clean up when done
cutter remove my-feature
```

## Commands

| Command | Description |
|---------|-------------|
| `cutter base add <name> <path>...` | Define a base from local git repos |
| `cutter base list` | List all bases |
| `cutter base remove <name>` | Remove a base definition |
| `cutter create [name] [--base <base>]` | Create workspace (interactive if args omitted) |
| `cutter list` | List all workspaces |
| `cutter status <name>` | Show repo status (branch, changes, ahead/behind) |
| `cutter remove <name>` | Remove worktrees, branch, and workspace directory |
| `cutter locate <name>` | Print workspace path (for `cd $(cutter locate <name>)`) |
| `cutter open-claude <name>` | Launch `claude` in a workspace directory |

### Create flags

- `--print` — print workspace path to stdout (for `cd $(cutter create ... --print)`)
- `--open-claude` — launch `claude` in the workspace directory after creation
- `--ai "<prompt>"` — describe the workspace in natural language and let a
  headless Claude session name it, pick a base, and create it (see
  [AI-driven creation](#ai-driven-creation)). Conflicts with a positional name
  and with `--print`/`--open-claude`.

### Remove flags

- `--keep-files` — remove worktrees from git but keep files on disk

## AI-driven creation

Instead of naming the workspace and choosing a base yourself, you can describe
what you're doing and let Claude set it up:

```sh
cutter create --ai "fix the SSO login redirect bug tracked in ENG-4471"
```

Cutter wraps your prompt in fixed instructions, injects the list of your
configured bases, and runs a **headless Claude Code session** (`claude -p`).
Claude does any research the request implies, chooses a short kebab-case name
(prefixing a referenced ticket id, e.g. `eng-4471-sso-login-redirect`), picks
the base that best fits, and runs `cutter create <name> --base <base>` itself.
Its progress streams to your terminal, and cutter reports the workspace it
created.

Pass `--base <base>` alongside `--ai` to pin the base yourself; Claude then only
names it. Without it, Claude chooses from your configured bases.

- **No API key** — the session rides your Claude subscription. (If
  `ANTHROPIC_API_KEY` is set it would override the subscription, so cutter
  removes it from the session's environment.)
- **Scoped, not autonomous** — the session may use read-only tools (Read, Grep,
  Glob, WebFetch, WebSearch) plus `cutter` and `git`; it can't run arbitrary
  shell commands. It's instructed not to commit, push, or remove anything.
- **Requirements** — [Claude Code](https://docs.claude.com/en/docs/claude-code)
  must be installed and on your `PATH`, and you need at least one base
  configured. Set `CUTTER_CLAUDE_BIN` to point at a specific `claude` binary if
  it isn't discoverable.

In the GUI, the **➕ New** dialog has a **🤖 AI** / **Manual** switcher at the
top: **AI** shows a prompt box, a base picker, and a **🤖 Create with AI**
button, while **Manual** shows the name and base fields.

## Branch From

By default, worktrees are created from `origin/main`. You can override this at three levels:

1. **Global default** — set `default_branch_from` in `[settings]`
2. **Per-base** — set `branch_from` on a base to override the global default for all repos in that base
3. **Per-repo** — set `branch_from` on an individual repo entry to override both the base and global defaults

Resolution order: repo `branch_from` > base `branch_from` > `settings.default_branch_from` > `origin/main`

Example config:

```toml
[settings]
workspace_root = "~/cutter"
default_branch_from = "origin/main"

[bases.my-project]
branch_from = "origin/develop"  # all repos in this base branch from develop by default

[[bases.my-project.repos]]
name = "backend"
path = "~/repos/backend"
# inherits origin/develop from the base

[[bases.my-project.repos]]
name = "frontend"
path = "~/repos/frontend"
branch_from = "origin/main"  # this repo overrides the base and uses main
```

## `.claude` assembly

When creating a workspace, cutter assembles a `.claude` directory at the workspace root from each repo's `.claude`, so Claude has unified context across all repos when launched from the workspace.

- **`CLAUDE.md`** — **not** merged. Instead cutter generates a `.claude/CLAUDE.md` that tells Claude to read each project's own `CLAUDE.md` (referencing `<repo>/CLAUDE.md`, or `<repo>/.claude/CLAUDE.md` if that's where it lives). Each project's instructions stay authoritative and in place.
- **`skills/`** and **`agents/`** — namespaced per project: each repo's skills/agents are copied under a project subfolder, so the workspace has `.claude/skills/<repo>/…` and `.claude/agents/<repo>/…` and they never collide.
- **`settings.local.json`** — `permissions.allow` and `permissions.deny` arrays are merged and deduplicated.
- **`mcp.json`** — `mcpServers` are merged; on a name clash between repos, the later one is renamed `<repo>/<server>`.
- **Other files** — copied by relative path; if multiple repos share the same path, each copy is prefixed with its repo name.

The generated `.claude/CLAUDE.md` is always written (even if no repo has a `.claude` directory).

> **Note:** the merge reads each repo's **checked-out worktree**, which only contains git-*tracked* files. A gitignored `.claude` file (commonly `settings.local.json`) won't be present in the worktree and so won't be merged.

### Per-base `.claude` directory

You can customize the merged `.claude` directory on a per-base level by placing files in `~/.config/cutter/bases/<base-name>/.claude/`. This directory is overlaid on top of the repo-merged result, so base files take priority.

Merge behavior:

- **`CLAUDE.md`** — base content is appended after the generated workspace `CLAUDE.md` (with a `# CLAUDE.md (from base)` header)
- **`settings.local.json`** — base allow/deny entries are merged into the repo-merged ones
- **`mcp.json`** — base MCP servers are merged in; base servers override same-named repo servers
- **Other files** — base files overwrite repo-merged files at the same relative path

Example:

```sh
# Create a base .claude directory
mkdir -p ~/.config/cutter/bases/platform/.claude

# Add base-level instructions
echo "Always run tests before committing." > ~/.config/cutter/bases/platform/.claude/CLAUDE.md

# Add base-level MCP servers
cat > ~/.config/cutter/bases/platform/.claude/mcp.json << 'EOF'
{ "mcpServers": { "my-server": { "command": "my-server" } } }
EOF
```

If the base `.claude` directory doesn't exist, the step is skipped.

## Data Layout

```
~/.config/cutter/
├── config.toml              # Base definitions + settings
├── bases/
│   └── platform/            # Per-base overrides (optional)
│       └── .claude/
│           ├── CLAUDE.md
│           └── mcp.json
└── workspaces/
    └── my-feature.toml      # Per-workspace state

~/cutter/
└── my-feature/              # Workspace root
    ├── CLAUDE.md            # (in each worktree) each project's own instructions
    ├── .claude/             # Assembled from all repos
    │   ├── CLAUDE.md        # Generated: points at each project's CLAUDE.md
    │   ├── settings.local.json
    │   ├── skills/
    │   │   ├── frontend/    # Namespaced per project
    │   │   └── backend/
    │   └── agents/
    │       ├── frontend/
    │       └── backend/
    ├── frontend/            # Worktree (branch = my-feature)
    ├── backend/             # Worktree (branch = my-feature)
    └── shared-libs/         # Worktree (branch = my-feature)
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `CUTTER_CONFIG_DIR` | Override config directory (default: `~/.config/cutter`) |
| `CUTTER_WORKSPACE_ROOT` | Override workspace root (default: `~/cutter`) |
