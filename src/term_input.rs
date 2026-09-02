//! Mouse, clipboard and drag-and-drop input for the embedded terminals.
//!
//! `egui_term` covers the basics — typing, dragging out a selection, Cmd+C —
//! but a handful of gaps make its terminal feel unlike a real one. This module
//! fills them in *around* the widget: it runs just before `TerminalView` is
//! added and takes the events it acts on off the queue, so the widget's own
//! (weaker) handling of the same input never runs on top of ours.
//!
//! What's missing upstream, and why each gap matters here:
//!
//! * **Scrolling a full-screen program.** `egui_term` always scrolls its own
//!   scrollback. A program that has asked to track the mouse (Claude Code does,
//!   whenever it's on the alternate screen) expects the wheel as mouse reports
//!   so it can scroll its own view; scrolling the scrollback instead just walks
//!   back through the stale frames it already painted over, which is why
//!   scrolling a Claude Code session looked like it moved the history rather
//!   than the content.
//! * **Selecting text while a program owns the mouse.** In that same mode
//!   `egui_term` hands every drag to the program, so there's no way to select
//!   anything. Terminals solve this with a modifier that forces a local
//!   selection; we take Option or Shift, matching iTerm2 and xterm.
//! * **Copying more than one line.** `TerminalBackend::selectable_content()`
//!   concatenates the selected cells with no line breaks at all, so anything
//!   taller than one row pastes back as a single run.
//! * **Pasting.** Pasted text goes to the PTY raw, so a multi-line paste into a
//!   shell runs every line but the last.
//! * **Dropped files.** Not handled at all.
//!
//! Everything here needs the grid, selection and mode types that `egui_term`
//! only passes through from `alacritty_terminal` without re-exporting, hence
//! this crate's direct dependency on it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use eframe::egui;
use egui_term::{BackendCommand, TerminalBackend};

/// Wheel-up / wheel-down button numbers in an xterm mouse report.
const WHEEL_UP: u8 = 64;
const WHEEL_DOWN: u8 = 65;

/// Ceiling on wheel reports emitted from one frame's worth of scrolling. A fast
/// flick can accumulate a lot of lines; past a point the program can't keep up
/// and the scroll just overshoots.
const MAX_WHEEL_REPORTS: i32 = 10;

/// The colour of the drop-target outline.
const DROP_HINT_COLOR: egui::Color32 = egui::Color32::from_rgb(0x58, 0xa6, 0xff);

/// Per-terminal input state carried between frames.
#[derive(Default)]
pub struct MouseState {
    /// Fractional scroll lines accumulated from smooth wheel deltas, so slow
    /// trackpad scrolling isn't rounded away to nothing.
    scroll_accum: f32,
    /// True while a modifier-held drag is extending a selection that the
    /// program running in the terminal would otherwise have received.
    selecting: bool,
}

/// The terminal state one frame of input is interpreted against, copied out of
/// the backend so its borrow ends before we start issuing commands.
struct Snapshot {
    mode: TermMode,
    cell_w: f32,
    cell_h: f32,
}

impl Snapshot {
    fn of(backend: &TerminalBackend) -> Self {
        let content = backend.last_content();
        Self {
            mode: content.terminal_mode,
            // Before the first frame's resize the backend still reports its
            // 1x1 placeholder cell; floor the size so the scroll accumulator
            // can't divide by something absurd.
            cell_w: (content.terminal_size.cell_width as f32).max(2.0),
            cell_h: (content.terminal_size.cell_height as f32).max(2.0),
        }
    }

    /// Whether a program is tracking the mouse, i.e. clicks, drags and the
    /// wheel belong to it rather than to us.
    fn mouse_tracked(&self) -> bool {
        self.mode.intersects(TermMode::MOUSE_MODE)
    }

    /// The zero-based viewport cell under a pointer position, relative to the
    /// terminal's top-left corner.
    fn cell_at(&self, offset: egui::Vec2) -> (usize, usize) {
        (
            (offset.x.max(0.0) / self.cell_w) as usize,
            (offset.y.max(0.0) / self.cell_h) as usize,
        )
    }
}

/// Handle one frame of input for the terminal drawn at `region`.
///
/// Call this immediately *before* adding the `TerminalView`, and
/// [`paint_drop_hint`] immediately after it.
pub fn handle(
    ui: &egui::Ui,
    region: egui::Rect,
    backend: &mut TerminalBackend,
    state: &mut MouseState,
) {
    let term = Snapshot::of(backend);

    // Cmd+C / Cmd+V don't depend on where the pointer is, so they're handled
    // whenever this terminal is the live pane — unlike `egui_term`, which wants
    // the pointer parked over the widget before either does anything.
    if take_copy(ui) {
        let text = selection_text(backend);
        // With nothing selected, leave the clipboard alone rather than blanking
        // it: that's what a terminal does, and it's what the user meant.
        if !text.is_empty() {
            ui.ctx().copy_text(text);
        }
    }
    if let Some(text) = take_paste(ui) {
        backend.process_command(BackendCommand::Write(paste_bytes(&text, term.mode)));
    }

    // A drop anywhere in the window belongs to the terminal on screen: only the
    // selected pane renders, so there's exactly one candidate.
    let dropped = dropped_paths(ui);
    if !dropped.is_empty() {
        let mut text = dropped
            .iter()
            .map(|p| shell_quote(&p.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        text.push(' ');
        backend.process_command(BackendCommand::Write(paste_bytes(&text, term.mode)));
    }

    for cmd in wheel(ui, region, &term, state) {
        backend.process_command(cmd);
    }
    for cmd in drag_select(ui, region, &term, state) {
        backend.process_command(cmd);
    }
}

/// Outline the terminal while files are dragged over the window, so a drop
/// looks like it will land somewhere. Painted on the foreground layer because
/// the terminal itself is drawn after this is called.
pub fn paint_drop_hint(ui: &egui::Ui, region: egui::Rect) {
    if ui.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("cutter_term_drop_hint"),
    ));
    painter.rect_filled(region, 4.0, egui::Color32::from_black_alpha(120));
    painter.rect_stroke(
        region,
        4.0,
        egui::Stroke::new(2.0, DROP_HINT_COLOR),
        egui::StrokeKind::Inside,
    );
    painter.text(
        region.center(),
        egui::Align2::CENTER_CENTER,
        "Drop to paste the file path",
        egui::FontId::proportional(14.0),
        egui::Color32::WHITE,
    );
}

/// The selected text, reconstructed from the visible grid.
///
/// One line break per grid row, except where a row is flagged as soft-wrapped —
/// there the shell broke a long line to fit the window and no newline was ever
/// typed, so joining the rows back up is what the user selected. Trailing
/// blanks, which a terminal pads every row with, are trimmed.
///
/// Only the visible viewport is walked, so a selection scrolled off the top
/// copies the part still on screen. That matches the range the widget is
/// highlighting, which is drawn from the same iterator.
fn selection_text(backend: &TerminalBackend) -> String {
    let content = backend.last_content();
    let Some(range) = content.selectable_range else {
        return String::new();
    };

    // (text, soft-wrapped) per selected grid row, in display order.
    let mut rows: Vec<(String, bool)> = Vec::new();
    let mut line = None;
    for indexed in content.grid.display_iter() {
        if !range.contains(indexed.point) {
            continue;
        }
        // The second cell of a double-width character, and the padding cell
        // before one that didn't fit on a row, hold placeholders rather than
        // text of their own.
        if indexed
            .cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        if line != Some(indexed.point.line) {
            line = Some(indexed.point.line);
            rows.push((String::new(), false));
        }
        let row = rows.last_mut().expect("a row was just pushed");
        row.0.push(indexed.c);
        row.1 |= indexed.cell.flags.contains(Flags::WRAPLINE);
    }

    let mut out = String::new();
    for (i, (text, wrapped)) in rows.iter().enumerate() {
        if *wrapped {
            out.push_str(text);
        } else {
            out.push_str(text.trim_end());
            if i + 1 < rows.len() {
                out.push('\n');
            }
        }
    }
    out
}

/// Turn this frame's wheel input into terminal commands, consuming it so
/// neither `egui_term` nor an enclosing scroll area acts on it too.
fn wheel(
    ui: &egui::Ui,
    region: egui::Rect,
    term: &Snapshot,
    state: &mut MouseState,
) -> Vec<BackendCommand> {
    if !ui.rect_contains_pointer(region) {
        return Vec::new();
    }
    let dy = ui.input(|i| i.smooth_scroll_delta.y);
    if dy == 0.0 {
        return Vec::new();
    }
    ui.input_mut(|i| {
        i.smooth_scroll_delta = egui::Vec2::ZERO;
        i.events
            .retain(|e| !matches!(e, egui::Event::MouseWheel { .. }));
    });

    // Carry the sub-line remainder into the next frame so slow scrolling still
    // moves eventually.
    state.scroll_accum += dy / term.cell_h;
    let lines = state.scroll_accum.trunc();
    state.scroll_accum -= lines;
    let lines = lines as i32;
    if lines == 0 {
        return Vec::new();
    }

    // Holding the override modifier keeps the wheel for this terminal even when
    // a program is tracking the mouse, the same way it keeps a drag.
    if !term.mouse_tracked() || ui.input(|i| i.modifiers.alt || i.modifiers.shift) {
        // Ours to scroll: the scrollback, or — on the alternate screen, which
        // has none — the arrow keys the backend substitutes.
        return vec![BackendCommand::Scroll(lines)];
    }

    // The program is tracking the mouse, so the wheel is its input, not ours.
    let Some(pos) = ui.input(|i| i.pointer.latest_pos()) else {
        return Vec::new();
    };
    let (col, row) = term.cell_at(pos - region.min);
    let button = if lines > 0 { WHEEL_UP } else { WHEEL_DOWN };
    let mut bytes = Vec::new();
    for _ in 0..lines.abs().min(MAX_WHEEL_REPORTS) {
        bytes.extend(mouse_report(term.mode, button, col, row));
    }
    vec![BackendCommand::Write(bytes)]
}

/// Drive a modifier-held selection over a terminal whose program is tracking
/// the mouse, consuming the button events so they aren't also reported to it.
///
/// With no program tracking the mouse there's nothing to override and this does
/// nothing: `egui_term`'s own selection handling is already correct there, and
/// leaving it in charge keeps Cmd+click link opening working.
fn drag_select(
    ui: &egui::Ui,
    region: egui::Rect,
    term: &Snapshot,
    state: &mut MouseState,
) -> Vec<BackendCommand> {
    if !term.mouse_tracked() {
        state.selecting = false;
        return Vec::new();
    }

    let mut cmds = Vec::new();
    let mut consumed = false;
    for event in ui.input(|i| i.events.clone()) {
        let egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers,
        } = event
        else {
            continue;
        };
        if pressed {
            // Option (iTerm2) or Shift (xterm) forces a local selection.
            if region.contains(pos) && (modifiers.alt || modifiers.shift) {
                state.selecting = true;
                consumed = true;
                cmds.push(select_start(SelectionType::Simple, region, pos));
            }
        } else if state.selecting {
            state.selecting = false;
            consumed = true;
            // Pin the far end at the button-up position: pointer motion and the
            // release can land in the same frame, and the release is the edge
            // the user is looking at.
            let at = pos - region.min;
            cmds.push(BackendCommand::SelectUpdate(at.x, at.y));
            // Click counting only resolves on release, so a double- or
            // triple-click restarts the selection as a word or a line — the
            // same order `egui_term` does it in.
            let (double, triple) = ui.input(|i| {
                (
                    i.pointer
                        .button_double_clicked(egui::PointerButton::Primary),
                    i.pointer
                        .button_triple_clicked(egui::PointerButton::Primary),
                )
            });
            if triple {
                cmds.push(select_start(SelectionType::Lines, region, pos));
            } else if double {
                cmds.push(select_start(SelectionType::Semantic, region, pos));
            }
        }
    }

    if state.selecting {
        if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
            let at = pos - region.min;
            cmds.push(BackendCommand::SelectUpdate(at.x, at.y));
        }
    }

    if consumed || state.selecting {
        // Drop every primary-button event for the frame: the press belongs to
        // the selection, and the program must not see the release either.
        ui.input_mut(|i| {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::PointerButton {
                        button: egui::PointerButton::Primary,
                        ..
                    }
                )
            })
        });
    }
    cmds
}

fn select_start(kind: SelectionType, region: egui::Rect, pos: egui::Pos2) -> BackendCommand {
    let at = pos - region.min;
    BackendCommand::SelectStart(kind, at.x, at.y)
}

/// Encode one mouse report in whichever format the program asked for. `col` and
/// `row` are zero-based viewport cells.
fn mouse_report(mode: TermMode, button: u8, col: usize, row: usize) -> Vec<u8> {
    if mode.contains(TermMode::SGR_MOUSE) {
        format!("\x1b[<{};{};{}M", button, col + 1, row + 1).into_bytes()
    } else if col < 223 && row < 223 {
        // The original encoding spends one byte per field, biased by 32, and
        // simply can't address anything past column or row 223 — a report from
        // out there would land somewhere else, so send nothing.
        vec![
            0x1b,
            b'[',
            b'M',
            32 + button,
            32 + 1 + col as u8,
            32 + 1 + row as u8,
        ]
    } else {
        Vec::new()
    }
}

/// The bytes to write for pasted text.
///
/// A program that asked for bracketed paste gets the text fenced in
/// `ESC[200~ … ESC[201~`, which is how a shell tells a pasted newline apart
/// from someone pressing Enter — without the fence, pasting two lines into zsh
/// runs the first one. Outside the fence, newlines have to arrive as the
/// carriage return the Enter key actually sends.
fn paste_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        // Strip escapes so pasted content can't close the fence early and have
        // the rest of itself read as keystrokes.
        let text = text.replace('\x1b', "");
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Take a copy request off the event queue, if one arrived this frame.
fn take_copy(ui: &egui::Ui) -> bool {
    let mut found = false;
    ui.input_mut(|i| {
        i.events.retain(|e| {
            let is_copy = matches!(e, egui::Event::Copy);
            found |= is_copy;
            !is_copy
        });
    });
    found
}

/// Take pasted text off the event queue, if any arrived this frame.
fn take_paste(ui: &egui::Ui) -> Option<String> {
    let mut text = None;
    ui.input_mut(|i| {
        i.events.retain(|e| match e {
            egui::Event::Paste(t) => {
                text = Some(t.clone());
                false
            }
            _ => true,
        });
    });
    text
}

/// The paths of any files dropped this frame.
///
/// Handing over a path is how a terminal gives a program a file — it's the
/// gesture behind dropping an image into a Claude Code session, which then
/// reads it from disk. A drop that carries bytes but no path (dragged straight
/// out of a browser, say) is spilled to a temp file so that there is one.
fn dropped_paths(ui: &egui::Ui) -> Vec<PathBuf> {
    let files = ui.input(|i| i.raw.dropped_files.clone());
    files
        .into_iter()
        .filter_map(|f| match (f.path, f.bytes) {
            (Some(path), _) => Some(path),
            (None, Some(bytes)) => spill(&f.name, &bytes),
            (None, None) => None,
        })
        .collect()
}

/// Write dropped bytes somewhere on disk so there's a path to paste. Kept out
/// of the way in a temp directory, and stamped so two drops of the same
/// `image.png` don't overwrite each other.
fn spill(name: &str, bytes: &[u8]) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cutter-drops");
    std::fs::create_dir_all(&dir).ok()?;
    // The name comes from whatever was dragged, so keep only its last
    // component: it must not be able to point outside the directory.
    let name = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "dropped".to_string());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{stamp}-{name}"));
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

/// Quote a path for a POSIX shell, as terminals do with dropped paths: a
/// filename with a space in it would otherwise arrive as two arguments.
fn shell_quote(path: &str) -> String {
    let safe = |b: u8| {
        b.is_ascii_alphanumeric()
            || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b',' | b':')
    };
    if !path.is_empty() && path.bytes().all(safe) {
        path.to_string()
    } else {
        // Everything inside single quotes is literal, so the only thing to
        // handle is a quote itself: close, escape it, reopen.
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_what_needs_it() {
        assert_eq!(shell_quote("/tmp/a-b_c.png"), "/tmp/a-b_c.png");
        assert_eq!(shell_quote("/tmp/my file.png"), "'/tmp/my file.png'");
        assert_eq!(shell_quote("/tmp/it's.png"), r"'/tmp/it'\''s.png'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn fences_pasted_text_only_when_asked() {
        let plain = TermMode::empty();
        assert_eq!(paste_bytes("a\nb", plain), b"a\rb".to_vec());
        assert_eq!(paste_bytes("a\r\nb", plain), b"a\rb".to_vec());

        let bracketed = TermMode::BRACKETED_PASTE;
        assert_eq!(
            paste_bytes("a\nb", bracketed),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
        // An escape in the pasted text can't be allowed to close the fence.
        assert_eq!(
            paste_bytes("a\x1b[201~b", bracketed),
            b"\x1b[200~a[201~b\x1b[201~".to_vec()
        );
    }

    #[test]
    fn encodes_mouse_reports() {
        assert_eq!(
            mouse_report(TermMode::SGR_MOUSE, WHEEL_UP, 4, 9),
            b"\x1b[<64;5;10M".to_vec()
        );
        assert_eq!(
            mouse_report(TermMode::empty(), WHEEL_DOWN, 4, 9),
            vec![0x1b, b'[', b'M', 32 + 65, 32 + 5, 32 + 10]
        );
        // Past the addressable range the old encoding has nothing to say.
        assert!(mouse_report(TermMode::empty(), WHEEL_UP, 300, 1).is_empty());
    }
}
