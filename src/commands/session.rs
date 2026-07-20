//! Handler for the internal `cutter session-event` subcommand, invoked by the
//! Claude Code hooks that Cutter installs into each workspace.

use std::io::Read;
use std::path::PathBuf;

use crate::cli::SessionEvent;
use crate::session::{self, SessionState};

/// Record a session lifecycle event. A hook must never block Claude or write to
/// its stdout (which would be injected into the conversation), so this swallows
/// every error and produces no output — it only ever updates a status file.
pub fn run(event: SessionEvent, ppid: Option<i32>) {
    let _ = try_run(event, ppid);
}

fn try_run(event: SessionEvent, ppid: Option<i32>) -> Option<()> {
    // The hook payload arrives as JSON on stdin.
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    let payload: serde_json::Value = serde_json::from_str(&buf).ok()?;

    let session_id = payload.get("session_id")?.as_str()?;

    // Session end: clear the record regardless of where it ran.
    if matches!(event, SessionEvent::SessionEnd) {
        let _ = session::remove(session_id);
        return Some(());
    }

    // Project dir: prefer CLAUDE_PROJECT_DIR (the launch root), else the payload
    // cwd (which reflects any `cd` the session has done).
    let project_dir = std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            payload
                .get("cwd")
                .and_then(|c| c.as_str())
                .map(PathBuf::from)
        })?;

    let state = match event {
        SessionEvent::PromptSubmit => SessionState::Running,
        SessionEvent::Stop | SessionEvent::Notification | SessionEvent::SessionStart => {
            SessionState::Waiting
        }
        // Handled above.
        SessionEvent::SessionEnd => return Some(()),
    };

    let _ = session::record(session_id, &project_dir, state, ppid);
    Some(())
}
