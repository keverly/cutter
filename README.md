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

## Data Layout

```
~/.config/cutter/
├── config.toml              # Base definitions + settings
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
