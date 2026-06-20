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

- **Workspaces** — every active workspace; click one to select it and view its
  base, branch, path, and per-repo worktrees. Use **➕ New** to create a
  workspace from a base, and **🗑 Remove** to tear one down.
- **Settings** — the workspace root, default branch-from, and each base with
  its repos, `branch_from`, and `copy_files`. Use **➕ New base** to define one
  (browse for repo folders or type paths), and **Remove** to delete a base
  definition.

Creating and removing run the same logic as the CLI commands, on a background
thread so the window stays responsive during `git fetch`/worktree work; a
status bar shows progress and the result. Destructive actions ask for
confirmation first. The list auto-refreshes when `~/.config/cutter` changes, or
use **⟳ Refresh** to re-read manually.

> The GUI reads and writes the same `~/.config/cutter` data the CLI uses. To run
> it without bundling, use `cargo run --features gui --bin cutter-gui`.

### Linking macOS windows to a workspace

You can tie real macOS windows to a workspace and bring them forward with one
click — useful when you keep, say, an Xcode window and its Simulator per
workspace (including two windows of the *same* app for different workspaces).

- In a workspace's detail pane, click **⧉ Link windows…**. Pick any open windows
  from the list (multi-select — e.g. Xcode *and* the Simulator) and **Save**.
  Workspaces with links show a `⧉` marker in the list.
- **Click the workspace** to raise its linked windows to the foreground. The ✕
  next to a link removes it.

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

### Remove flags

- `--keep-files` — remove worktrees from git but keep files on disk

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

## `.claude` merging

When creating a workspace, cutter automatically merges the `.claude` directories from each repo into a single `.claude` directory at the workspace root. This gives Claude unified context across all repos when launched from the workspace.

- **`CLAUDE.md`** — concatenated with headers indicating which repo each section came from
- **`settings.local.json`** — `permissions.allow` and `permissions.deny` arrays are merged and deduplicated
- **Subdirectories** (e.g. `skills/`) — recursively copied, preserving structure
- **Other files** — copied directly; if multiple repos share the same filename, each copy is prefixed with its repo name

If no repos contain a `.claude` directory, the step is skipped.

### Per-base `.claude` directory

You can customize the merged `.claude` directory on a per-base level by placing files in `~/.config/cutter/bases/<base-name>/.claude/`. This directory is overlaid on top of the repo-merged result, so base files take priority.

Merge behavior:

- **`CLAUDE.md`** — base content is appended after the repo-merged content (with a `# CLAUDE.md (from base)` header)
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
    ├── .claude/             # Merged from all repos
    │   ├── CLAUDE.md
    │   ├── settings.local.json
    │   └── skills/
    ├── frontend/            # Worktree (branch = my-feature)
    ├── backend/             # Worktree (branch = my-feature)
    └── shared-libs/         # Worktree (branch = my-feature)
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `CUTTER_CONFIG_DIR` | Override config directory (default: `~/.config/cutter`) |
| `CUTTER_WORKSPACE_ROOT` | Override workspace root (default: `~/cutter`) |
