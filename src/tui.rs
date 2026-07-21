//! Interactive per-hunk fold (Phase 3 TUI).
//!
//! An immediate-mode ratatui app over the frozen engine: it shows the stack and
//! the staged diff, lets you assign each hunk a target commit (inference
//! pre-fills one), and previews or applies by building a `Recipe` and handing it
//! to `engine::replay` — the same engine `ops::fix` uses, so the preview (a
//! dry-run replay whose oid is discarded) can never disagree with the result.

use std::path::Path;

use anyhow::Result;
use git2::{DiffOptions, Oid, Patch, Repository, Tree};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::patch::Hunk;
use crate::{engine, git, inference, ops, patch};

/// A changed file's staged diff plus per-hunk selection and target state.
struct FileEntry {
    path: String,
    /// File content at HEAD — the synthetic source text for `synthetic_for_hunks`.
    old_full: String,
    hunks: Vec<Hunk>,
    /// Display diff lines per hunk `(origin, text)`, parallel to `hunks`.
    lines: Vec<Vec<(char, String)>>,
    selected: Vec<bool>,
    /// Inferred (user-overridable) target commit per hunk; `None` = no home.
    targets: Vec<Option<Oid>>,
}

/// One commit in the stack window, as displayed (newest-first).
struct CommitRow {
    oid: Oid,
    summary: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Commits,
    Hunks,
}

/// All UI state; the screen is redrawn purely from this each frame.
struct App {
    branch: String,
    head: Oid,
    ignore_ws: bool,
    /// Newest-first, for display.
    commits: Vec<CommitRow>,
    files: Vec<FileEntry>,
    /// `(file, hunk)` pairs in display order; the hunk cursor indexes this.
    flat: Vec<(usize, usize)>,
    focus: Pane,
    commit_cursor: usize,
    hunk_cursor: usize,
    status: String,
    applied: bool,
}

/// Launch the TUI, run it to completion, and print the final verdict.
pub fn run(repo: &Repository, ignore_ws: bool) -> Result<()> {
    let mut app = load(repo, ignore_ws)?;
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, &mut app, repo);
    ratatui::restore();
    res?;
    println!("{}", app.status);
    Ok(())
}

/// Gather the stack, staged hunks, and inferred targets into the `App`.
fn load(repo: &Repository, ignore_ws: bool) -> Result<App> {
    let branch = ops::head_branch(repo)?;
    let head = git::resolve(repo, "HEAD")?;

    // Window = the whole linear history (base-exclusive `None`).
    // ponytail: full history; add a `--base` to bound it if replay gets slow.
    let stack = git::linear_commits(repo, None, head)?;
    let window: Vec<Oid> = stack.iter().map(|c| c.id()).collect();
    let commits: Vec<CommitRow> = stack
        .iter()
        .rev()
        .map(|c| CommitRow { oid: c.id(), summary: c.summary().unwrap_or("").to_string() })
        .collect();

    let head_tree = repo.find_commit(head)?.tree()?;
    let index_tree = repo.find_tree(repo.index()?.write_tree()?)?;
    let diff = repo.diff_tree_to_tree(Some(&head_tree), Some(&index_tree), None)?;

    let mut files = Vec::new();
    for delta in diff.deltas() {
        let path = match delta.new_file().path().or_else(|| delta.old_file().path()) {
            Some(p) => p.to_string_lossy().into_owned(),
            None => continue,
        };
        let old = read_blob(repo, &head_tree, &path);
        let new = read_blob(repo, &index_tree, &path);
        let hunks = match patch::hunks(&old, &new) {
            Ok(h) if !h.is_empty() => h,
            _ => continue,
        };
        // ponytail: text files only — hunk surgery is meaningless on binaries.
        let old_full = match String::from_utf8(old.clone()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let lines = diff_lines(&old, &new)?;
        // Newly-added files can't be blamed; fall back to "no home".
        let targets =
            inference::infer_targets(repo, &path, &hunks, &window).unwrap_or_else(|_| vec![None; hunks.len()]);
        let selected = vec![true; hunks.len()]; // absorb-all by default
        files.push(FileEntry { path, old_full, hunks, lines, selected, targets });
    }

    let flat = flatten(&files.iter().map(|f| f.hunks.len()).collect::<Vec<_>>());
    Ok(App {
        branch,
        head,
        ignore_ws,
        commits,
        files,
        flat,
        focus: Pane::Hunks,
        commit_cursor: 0,
        hunk_cursor: 0,
        status: "Space toggle · t set target · p preview · Enter apply · q quit".into(),
        applied: false,
    })
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, repo: &Repository) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue; // ignore key-release/-repeat (Windows sends both)
        }
        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('j') | KeyCode::Down => app.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => app.move_cursor(-1),
            KeyCode::Tab => {
                app.focus = match app.focus {
                    Pane::Commits => Pane::Hunks,
                    Pane::Hunks => Pane::Commits,
                }
            }
            KeyCode::Char(' ') => app.toggle(),
            KeyCode::Char('t') => app.set_target(),
            KeyCode::Char('p') => app.preview(repo),
            KeyCode::Enter => {
                app.execute(repo);
                if app.applied {
                    break;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

impl App {
    /// Target commit of the hunk under the hunk cursor (marks it in the list).
    fn current_target(&self) -> Option<Oid> {
        self.flat.get(self.hunk_cursor).and_then(|&(fi, hi)| self.files[fi].targets[hi])
    }

    fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            Pane::Commits => self.commit_cursor = step(self.commit_cursor, self.commits.len(), delta),
            Pane::Hunks => self.hunk_cursor = step(self.hunk_cursor, self.flat.len(), delta),
        }
    }

    fn toggle(&mut self) {
        let Some(&(fi, hi)) = self.flat.get(self.hunk_cursor) else { return };
        let s = &mut self.files[fi].selected[hi];
        *s = !*s;
    }

    fn set_target(&mut self) {
        let Some(&(fi, hi)) = self.flat.get(self.hunk_cursor) else { return };
        let Some(oid) = self.commits.get(self.commit_cursor).map(|c| c.oid) else { return };
        self.files[fi].targets[hi] = Some(oid);
        self.status = format!("hunk target -> {oid:.8}");
    }

    /// Dry-run: build the recipe and replay without promoting; discard the oid.
    fn preview(&mut self, repo: &Repository) {
        let recipe = match self.build_recipe(repo) {
            Ok(r) if !r.is_empty() => r,
            Ok(_) => {
                self.status = "nothing to preview — select hunks (Space) and set targets (t)".into();
                return;
            }
            Err(e) => {
                self.status = format!("preview error: {e}");
                return;
            }
        };
        match engine::replay(repo, None, self.head, &recipe, self.ignore_ws) {
            Ok(oid) => self.status = format!("clean, would move {} to {oid:.8}", self.branch),
            Err(e) => self.status = format!("conflict: {e}"),
        }
    }

    /// Apply for real: replay, then move the branch ref like `ops::fix` does.
    fn execute(&mut self, repo: &Repository) {
        // Same guard as ops::fix — promote force-checkouts, so unstaged changes
        // to other files must not be silently clobbered.
        if let Err(e) = ops::require_clean_unstaged(repo) {
            self.status = format!("{e}");
            return;
        }
        let recipe = match self.build_recipe(repo) {
            Ok(r) if !r.is_empty() => r,
            Ok(_) => {
                self.status = "nothing selected to apply".into();
                return;
            }
            Err(e) => {
                self.status = format!("error: {e}");
                return;
            }
        };
        match engine::replay(repo, None, self.head, &recipe, self.ignore_ws) {
            Ok(new_tip) => match ops::promote(repo, &self.branch, new_tip, self.head, "transplant: tui fold") {
                Ok(()) => {
                    self.status = format!("{} now at {new_tip:.8}", self.branch);
                    self.applied = true;
                }
                Err(e) => self.status = format!("promote failed: {e}"),
            },
            Err(e) => self.status = format!("conflict, not applied: {e}"),
        }
    }

    /// Build the replay recipe from the selections. Each (file, target) group of
    /// selected hunks becomes one synthetic commit applied at that target.
    fn build_recipe(&self, repo: &Repository) -> Result<engine::Recipe> {
        let mut recipe = engine::Recipe::new();
        for f in &self.files {
            let mut targets: Vec<Oid> = f
                .selected
                .iter()
                .zip(&f.targets)
                .filter_map(|(&s, t)| if s { *t } else { None })
                .collect();
            targets.sort();
            targets.dedup();
            for t in targets {
                let mask = mask_for_target(&f.selected, &f.targets, t);
                let synth = patch::synthetic_for_hunks(repo, self.head, &f.path, &f.old_full, &f.hunks, &mask)?;
                recipe.add(t, engine::Edit::ApplyChange(synth));
            }
        }
        Ok(recipe)
    }
}

// ── pure helpers (unit-tested) ──────────────────────────────────────────────

/// Flatten per-file hunk counts into `(file, hunk)` pairs in display order.
fn flatten(counts: &[usize]) -> Vec<(usize, usize)> {
    counts.iter().enumerate().flat_map(|(fi, &n)| (0..n).map(move |hi| (fi, hi))).collect()
}

/// Mask of a file's hunks that are both selected and routed to `target`.
fn mask_for_target(selected: &[bool], targets: &[Option<Oid>], target: Oid) -> Vec<bool> {
    selected.iter().zip(targets).map(|(&s, t)| s && *t == Some(target)).collect()
}

/// Clamp `cur + delta` into `[0, len)` (empty list stays at 0).
fn step(cur: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (cur as isize + delta).clamp(0, len as isize - 1) as usize
}

/// Read a path's blob bytes from a tree, or empty if the path is absent.
fn read_blob(repo: &Repository, tree: &Tree, path: &str) -> Vec<u8> {
    tree.get_path(Path::new(path))
        .and_then(|e| e.to_object(repo))
        .ok()
        .and_then(|o| o.peel_to_blob().ok())
        .map(|b| b.content().to_vec())
        .unwrap_or_default()
}

/// Per-hunk display lines `(origin, text)`, parallel to `patch::hunks` (same
/// diff options, so hunk count and order match one-to-one).
fn diff_lines(old: &[u8], new: &[u8]) -> Result<Vec<Vec<(char, String)>>> {
    let mut opts = DiffOptions::new();
    opts.context_lines(3);
    let patch = Patch::from_buffers(old, None, new, None, Some(&mut opts))?;
    let mut out = Vec::new();
    for i in 0..patch.num_hunks() {
        let mut lines = Vec::new();
        for j in 0..patch.num_lines_in_hunk(i)? {
            let l = patch.line_in_hunk(i, j)?;
            let text = String::from_utf8_lossy(l.content()).trim_end_matches('\n').to_string();
            lines.push((l.origin(), text));
        }
        out.push(lines);
    }
    Ok(out)
}

// ── rendering ───────────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(f.area());
    render_commits(f, app, cols[0]);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(cols[1]);
    render_hunks(f, app, rows[0]);
    render_status(f, app, rows[1]);
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_commits(f: &mut Frame, app: &App, area: Rect) {
    let target = app.current_target();
    let items: Vec<ListItem> = app
        .commits
        .iter()
        .map(|c| {
            let marker = if Some(c.oid) == target { "> " } else { "  " };
            ListItem::new(format!("{marker}{:.8} {}", c.oid, c.summary))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("commits (t: set target)")
                .border_style(border_style(app.focus == Pane::Commits)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !app.commits.is_empty() {
        state.select(Some(app.commit_cursor));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn render_hunks(f: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    for &(fi, hi) in &app.flat {
        let file = &app.files[fi];
        let mut lines: Vec<Line> = Vec::new();
        if hi == 0 {
            lines.push(Line::from(Span::styled(
                format!("-- {} --", file.path),
                Style::default().add_modifier(Modifier::BOLD),
            )));
        }
        let checkbox = if file.selected[hi] { "[x]" } else { "[ ]" };
        let target = match file.targets[hi] {
            Some(o) => format!("{o:.8}"),
            None => "-- (no home)".into(),
        };
        lines.push(Line::from(format!("{checkbox} {}  -> {target}", file.hunks[hi].header)));
        for (origin, text) in &file.lines[hi] {
            let (prefix, style) = match origin {
                '+' => ('+', Style::default().fg(Color::Green)),
                '-' => ('-', Style::default().fg(Color::Red)),
                _ => (' ', Style::default()),
            };
            lines.push(Line::from(Span::styled(format!("{prefix}{text}"), style)));
        }
        items.push(ListItem::new(lines));
    }
    if items.is_empty() {
        items.push(ListItem::new("no staged changes to fold"));
    }
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("staged hunks (Space: toggle)")
                .border_style(border_style(app.focus == Pane::Hunks)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !app.flat.is_empty() {
        state.select(Some(app.hunk_cursor));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let total: usize = app.files.iter().map(|f| f.hunks.len()).sum();
    let selected: usize = app.files.iter().flat_map(|f| f.selected.iter()).filter(|&&s| s).count();
    let dim = Style::default().fg(Color::DarkGray);
    let text = vec![
        Line::from(Span::styled(
            "j/k move · Tab pane · Space toggle · t target · p preview · Enter apply · q quit",
            dim,
        )),
        Line::from(app.status.clone()),
        Line::from(Span::styled(format!("{selected}/{total} hunks selected"), dim)),
    ];
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(b: u8) -> Oid {
        Oid::from_bytes(&[b; 20]).unwrap()
    }

    #[test]
    fn mask_selects_only_hunks_homed_to_target() {
        let (a, b) = (oid(1), oid(2));
        let selected = [true, false, true];
        let targets = [Some(a), Some(a), Some(b)];
        // hunk 1 is routed to a but deselected; hunk 2 is a different target.
        assert_eq!(mask_for_target(&selected, &targets, a), vec![true, false, false]);
        assert_eq!(mask_for_target(&selected, &targets, b), vec![false, false, true]);
    }

    #[test]
    fn flatten_pairs_files_and_hunks_in_order() {
        assert_eq!(flatten(&[2, 0, 1]), vec![(0, 0), (0, 1), (2, 0)]);
    }

    #[test]
    fn step_clamps_at_bounds() {
        assert_eq!(step(0, 3, -1), 0);
        assert_eq!(step(2, 3, 1), 2);
        assert_eq!(step(1, 3, 1), 2);
        assert_eq!(step(0, 0, 1), 0);
    }
}
