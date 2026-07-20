//! Live status of Claude Code sessions running inside Cutter workspaces.
//!
//! Claude Code fires lifecycle *hooks*, which we configure (per workspace) to
//! shell out to `cutter session-event <event>`. Each invocation resolves the
//! owning workspace and writes a small JSON record to
//! `~/.config/cutter/sessions/<session_id>.json`. The GUI already watches the
//! config dir recursively, so those writes light up the workspace list without
//! polling; `cutter list` reads the same records.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;
use crate::error::{Error, Result};
use crate::workspace::WorkspaceConfig;

/// Whether a tracked Claude Code session is actively working or waiting on the
/// user. The user-facing feature has exactly these two states: a finished turn,
/// a permission prompt, and a fresh prompt all read as "waiting for input".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Running,
    Waiting,
}

impl SessionState {
    /// Human-facing label.
    pub fn label(self) -> &'static str {
        match self {
            SessionState::Running => "running",
            SessionState::Waiting => "waiting for input",
        }
    }
}

/// One Claude Code session's last-known status, persisted as
/// `~/.config/cutter/sessions/<session_id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    /// Name of the owning cutter workspace, resolved when the record is written.
    pub workspace: String,
    /// The directory the session reported (its project dir / cwd).
    pub cwd: String,
    pub state: SessionState,
    pub updated_at: DateTime<Utc>,
    /// Best-effort pid of the Claude process (captured from the hook's `$PPID`),
    /// used to prune sessions that went away without firing `SessionEnd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
}

/// Aggregate Claude status for one workspace.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkspaceStatus {
    pub running: usize,
    pub waiting: usize,
}

impl WorkspaceStatus {
    /// The single state to surface; running takes precedence over waiting.
    pub fn state(self) -> Option<SessionState> {
        if self.running > 0 {
            Some(SessionState::Running)
        } else if self.waiting > 0 {
            Some(SessionState::Waiting)
        } else {
            None
        }
    }
}

/// Records older than this with no update are treated as stale, a backstop for
/// the rare case where neither `SessionEnd` nor pid-pruning cleaned them up.
const MAX_AGE_SECS: i64 = 24 * 60 * 60;

/// The five hook events Cutter installs, paired with the `session-event`
/// argument each maps to.
const HOOKS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("Stop", "stop"),
    ("Notification", "notification"),
    ("SessionEnd", "session-end"),
];

pub fn sessions_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("sessions"))
}

/// Path to a session's record file. `session_id` is sanitized to a bare
/// filename so a hostile value can't escape the sessions dir.
fn record_path(session_id: &str) -> Result<PathBuf> {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(sessions_dir()?.join(format!("{safe}.json")))
}

/// Normalize a path for prefix comparison: canonicalize if it exists, else use
/// it as given.
fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Find the workspace that owns `project_dir` — the one whose path equals it or
/// is an ancestor of it — choosing the longest (most specific) match.
pub fn resolve_workspace(project_dir: &Path, workspaces: &[WorkspaceConfig]) -> Option<String> {
    let target = normalize(project_dir);
    let mut best: Option<(usize, String)> = None;
    for ws in workspaces {
        let ws_path = normalize(Path::new(&ws.workspace.path));
        if target.starts_with(&ws_path) {
            let len = ws_path.as_os_str().len();
            if best.as_ref().is_none_or(|(l, _)| len > *l) {
                best = Some((len, ws.workspace.name.clone()));
            }
        }
    }
    best.map(|(_, name)| name)
}

/// Write (or overwrite) the status record for a session. No-op if `project_dir`
/// is not inside a known cutter workspace (keeps non-cutter sessions out).
pub fn record(
    session_id: &str,
    project_dir: &Path,
    state: SessionState,
    pid: Option<i32>,
) -> Result<()> {
    let workspaces = WorkspaceConfig::list_all()?;
    let Some(workspace) = resolve_workspace(project_dir, &workspaces) else {
        return Ok(());
    };
    let dir = sessions_dir()?;
    std::fs::create_dir_all(&dir)?;
    let rec = SessionRecord {
        session_id: session_id.to_string(),
        workspace,
        cwd: project_dir.to_string_lossy().into_owned(),
        state,
        updated_at: Utc::now(),
        pid,
    };
    let json = serde_json::to_string_pretty(&rec).map_err(|e| Error::Config(e.to_string()))?;
    std::fs::write(record_path(session_id)?, format!("{json}\n"))?;
    Ok(())
}

/// Delete a session's record (called on `SessionEnd`).
pub fn remove(session_id: &str) -> Result<()> {
    let path = record_path(session_id)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Read all session records, pruning ones that are dead (pid gone), stale (older
/// than [`MAX_AGE_SECS`]), or unparseable — deleting those files to self-heal.
/// Records whose workspace is no longer present are skipped but left on disk (a
/// missing workspace may just be a transient load failure).
pub fn load_active(workspaces: &[WorkspaceConfig]) -> Vec<SessionRecord> {
    let Ok(dir) = sessions_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let names: HashSet<&str> = workspaces.iter().map(|w| w.workspace.name.as_str()).collect();
    let now = Utc::now();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(rec) = serde_json::from_str::<SessionRecord>(&contents) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let dead_pid = rec.pid.is_some_and(|p| !pid_alive(p));
        let too_old = (now - rec.updated_at).num_seconds() > MAX_AGE_SECS;
        if dead_pid || too_old {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if names.contains(rec.workspace.as_str()) {
            out.push(rec);
        }
    }
    out
}

/// Aggregate already-loaded records into a per-workspace status.
pub fn aggregate(records: &[SessionRecord]) -> HashMap<String, WorkspaceStatus> {
    let mut map: HashMap<String, WorkspaceStatus> = HashMap::new();
    for rec in records {
        let entry = map.entry(rec.workspace.clone()).or_default();
        match rec.state {
            SessionState::Running => entry.running += 1,
            SessionState::Waiting => entry.waiting += 1,
        }
    }
    map
}

/// Convenience: load and aggregate in one call (used by `cutter list`).
pub fn status_by_workspace(workspaces: &[WorkspaceConfig]) -> HashMap<String, WorkspaceStatus> {
    aggregate(&load_active(workspaces))
}

/// Whether a process is alive. `kill(pid, 0)` sends no signal, just checks: it
/// succeeds when the process exists; `EPERM` means it exists but we can't signal
/// it (still alive); `ESRCH` means it's gone.
#[cfg(unix)]
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: i32) -> bool {
    true
}

/// Best-effort absolute path to the `cutter` CLI, for embedding in hook
/// commands. The GUI runs as `cutter-gui`, so we can't just use the current
/// executable; prefer the documented install location, then `which`, then bare
/// `cutter` (resolved via PATH at hook time).
fn resolve_cutter_cli() -> String {
    if let Some(home) = dirs::home_dir() {
        let cargo_bin = home.join(".cargo").join("bin").join("cutter");
        if cargo_bin.is_file() {
            return cargo_bin.to_string_lossy().into_owned();
        }
    }
    if let Ok(out) = std::process::Command::new("which").arg("cutter").output() {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() {
                return path;
            }
        }
    }
    "cutter".to_string()
}

/// Whether an event's hook array already contains one of our `session-event`
/// commands.
fn has_cutter_hook(event_array: &serde_json::Value) -> bool {
    event_array
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(|h| h.as_array()))
        .flatten()
        .filter_map(|h| h.get("command").and_then(|c| c.as_str()))
        .any(|c| c.contains("session-event"))
}

/// Install the session-status hooks into a workspace's
/// `.claude/settings.local.json`, idempotently and without disturbing existing
/// `permissions` or unrelated hooks. Writes only when a hook is missing, so it
/// is cheap to call on every GUI reload (and retrofits older workspaces).
pub fn ensure_hooks(workspace_dir: &Path) -> Result<()> {
    let claude_dir = workspace_dir.join(".claude");
    let settings_path = claude_dir.join("settings.local.json");

    let mut root: serde_json::Value = if settings_path.is_file() {
        let contents = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }

    let cutter = resolve_cutter_cli();
    let mut changed = false;

    let obj = root.as_object_mut().expect("root is an object");
    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        *hooks = serde_json::json!({});
    }
    let hooks = hooks.as_object_mut().expect("hooks is an object");

    for (event, arg) in HOOKS {
        let entry = hooks.entry(*event).or_insert_with(|| serde_json::json!([]));
        if !entry.is_array() {
            *entry = serde_json::json!([]);
        }
        if has_cutter_hook(entry) {
            continue;
        }
        let command = format!("{cutter} session-event {arg} --ppid $PPID");
        entry.as_array_mut().expect("entry is an array").push(serde_json::json!({
            "hooks": [ { "type": "command", "command": command } ]
        }));
        changed = true;
    }

    if changed {
        std::fs::create_dir_all(&claude_dir)?;
        let json =
            serde_json::to_string_pretty(&root).map_err(|e| Error::Config(e.to_string()))?;
        std::fs::write(&settings_path, format!("{json}\n"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{WorkspaceInfo, WorkspaceConfig};
    use std::sync::Mutex;

    // Serialize tests that mutate the shared CUTTER_CONFIG_DIR / cwd-sensitive
    // process env, so they don't race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn ws(name: &str, path: &str) -> WorkspaceConfig {
        WorkspaceConfig {
            workspace: WorkspaceInfo {
                name: name.to_string(),
                base: "b".to_string(),
                branch: name.to_string(),
                path: path.to_string(),
                created_at: Utc::now(),
            },
            repos: Vec::new(),
            linked_windows: Vec::new(),
        }
    }

    #[test]
    fn resolves_longest_prefix() {
        let workspaces = vec![
            ws("feat", "/tmp/root/feat"),
            ws("feature-two", "/tmp/root/feature-two"),
        ];
        // Exact match.
        assert_eq!(
            resolve_workspace(Path::new("/tmp/root/feat"), &workspaces).as_deref(),
            Some("feat")
        );
        // Session launched in a repo subdir maps to its workspace.
        assert_eq!(
            resolve_workspace(Path::new("/tmp/root/feat/frontend/src"), &workspaces).as_deref(),
            Some("feat")
        );
        // Component-wise prefix: "feat" must not swallow "feature-two".
        assert_eq!(
            resolve_workspace(Path::new("/tmp/root/feature-two/api"), &workspaces).as_deref(),
            Some("feature-two")
        );
        // Outside any workspace.
        assert_eq!(
            resolve_workspace(Path::new("/tmp/elsewhere"), &workspaces),
            None
        );
    }

    #[test]
    fn aggregate_precedence() {
        let recs = vec![
            SessionRecord {
                session_id: "a".into(),
                workspace: "w".into(),
                cwd: "/w".into(),
                state: SessionState::Waiting,
                updated_at: Utc::now(),
                pid: None,
            },
            SessionRecord {
                session_id: "b".into(),
                workspace: "w".into(),
                cwd: "/w".into(),
                state: SessionState::Running,
                updated_at: Utc::now(),
                pid: None,
            },
        ];
        let map = aggregate(&recs);
        let status = map.get("w").copied().unwrap();
        assert_eq!(status.running, 1);
        assert_eq!(status.waiting, 1);
        // Running wins.
        assert_eq!(status.state(), Some(SessionState::Running));
    }

    #[test]
    fn record_and_remove_roundtrip() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("cutter-session-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("CUTTER_CONFIG_DIR", &tmp);

        // A workspace must exist on disk for record() to resolve it.
        let ws_dir = tmp.join("wsroot").join("feat");
        std::fs::create_dir_all(&ws_dir).unwrap();
        ws("feat", ws_dir.to_str().unwrap()).save().unwrap();

        record("sess-1", &ws_dir, SessionState::Running, Some(1)).unwrap();
        let loaded = load_active(&WorkspaceConfig::list_all().unwrap());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].workspace, "feat");
        assert_eq!(loaded[0].state, SessionState::Running);

        remove("sess-1").unwrap();
        assert!(load_active(&WorkspaceConfig::list_all().unwrap()).is_empty());

        std::env::remove_var("CUTTER_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn record_ignores_dir_outside_workspaces() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("cutter-session-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("CUTTER_CONFIG_DIR", &tmp);
        std::fs::create_dir_all(tmp.join("wsroot").join("feat")).unwrap();
        ws("feat", tmp.join("wsroot").join("feat").to_str().unwrap())
            .save()
            .unwrap();

        // A dir not under any workspace writes nothing.
        record("sess-x", Path::new("/definitely/not/a/workspace"), SessionState::Running, None)
            .unwrap();
        assert!(load_active(&WorkspaceConfig::list_all().unwrap()).is_empty());

        std::env::remove_var("CUTTER_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_hooks_is_idempotent_and_preserves_permissions() {
        let tmp = std::env::temp_dir().join(format!("cutter-hooks-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let claude = tmp.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        // Pre-existing settings with permissions we must keep.
        std::fs::write(
            claude.join("settings.local.json"),
            r#"{"permissions":{"allow":["WebSearch"]}}"#,
        )
        .unwrap();

        ensure_hooks(&tmp).unwrap();
        let after1 = std::fs::read_to_string(claude.join("settings.local.json")).unwrap();
        let v1: serde_json::Value = serde_json::from_str(&after1).unwrap();
        // Permissions preserved.
        assert_eq!(v1["permissions"]["allow"][0], "WebSearch");
        // All five events installed.
        let hooks = v1["hooks"].as_object().unwrap();
        for (event, _) in HOOKS {
            assert!(hooks.contains_key(*event), "missing {event}");
        }
        assert!(after1.contains("session-event stop"));

        // Second call changes nothing.
        ensure_hooks(&tmp).unwrap();
        let after2 = std::fs::read_to_string(claude.join("settings.local.json")).unwrap();
        assert_eq!(after1, after2);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
