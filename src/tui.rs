//! Interactive TUI over the engine. One screen drives all operations:
//!
//! - **Hunks mode** (default): the staged diff, one selectable hunk at a time,
//!   each with an inference-prefilled target commit. This IS fix / absorb /
//!   manual A-D — they are just selection+routing states of one recipe:
//!     * absorb  = every hunk selected, targets from inference → Enter
//!     * fix     = route all selected hunks to one commit (`a`) → Enter
//!     * A/D     = set per-hunk targets by hand (`t`)
//! - **Move mode** (`m`): pick a tracked file and a destination commit → op B.
//!
//! `preview` (`p`) is a dry-run replay whose oid is discarded — the same engine
//! call as apply, so it can never disagree. Input handling is a pure
//! `on_key(&mut App, KeyCode) -> Flow` so the whole state machine is unit-tested
//! without a terminal (see `mod tests`).

use std::path::Path;

use anyhow::Result;
use git2::{DiffOptions, ObjectType, Oid, Patch, Repository, Tree, TreeWalkMode, TreeWalkResult};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::patch::Hunk;
use crate::{engine, git, inference, ops, patch, recipe};

/// A changed file's staged diff plus per-hunk selection and target state.
struct FileEntry {
    path: String,
    old_full: String,
    hunks: Vec<Hunk>,
    lines: Vec<Vec<(char, String)>>,
    selected: Vec<bool>,
    /// Working (user-editable) target per hunk.
    targets: Vec<Option<Oid>>,
    /// Inference's original suggestion, for the `A` reset key.
    inferred: Vec<Option<Oid>>,
}

struct CommitRow {
    oid: Oid,
    summary: String,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pane {
    Commits,
    Right,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Hunks,
    Move,
}

/// What the pure key handler asks the driver to do next.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Flow {
    Continue,
    Quit,
    Preview,
    Apply,
}

struct App {
    branch: String,
    head: Oid,
    ignore_ws: bool,
    commits: Vec<CommitRow>,
    files: Vec<FileEntry>,
    /// `(file, hunk)` pairs in display order; the hunk cursor indexes this.
    flat: Vec<(usize, usize)>,
    move_files: Vec<String>,
    mode: Mode,
    focus: Pane,
    commit_cursor: usize,
    hunk_cursor: usize,
    move_cursor: usize,
    move_target: Option<Oid>,
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

/// Gather the stack, staged hunks, tracked files, and inferred targets.
fn load(repo: &Repository, ignore_ws: bool) -> Result<App> {
    let branch = ops::head_branch(repo)?;
    let head = git::resolve(repo, "HEAD")?;

    // ponytail: full history window; add a `--base` bound if replay gets slow.
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
        if delta.status() != git2::Delta::Modified {
            continue; // only text-modifications are hunk-foldable
        }
        let path = match delta.new_file().path() {
            Some(p) => p.to_string_lossy().into_owned(),
            None => continue,
        };
        let old = read_blob(repo, &head_tree, &path);
        let new = read_blob(repo, &index_tree, &path);
        let hunks = match patch::hunks(&old, &new) {
            Ok(h) if !h.is_empty() => h,
            _ => continue,
        };
        let old_full = match String::from_utf8(old.clone()) {
            Ok(s) => s,
            Err(_) => continue, // binary/non-UTF-8: skip
        };
        let lines = diff_lines(&old, &new)?;
        let inferred = inference::infer_targets(repo, &path, &hunks, &window)
            .unwrap_or_else(|_| vec![None; hunks.len()]);
        let selected = vec![true; hunks.len()];
        files.push(FileEntry {
            path,
            old_full,
            hunks,
            lines,
            selected,
            targets: inferred.clone(),
            inferred,
        });
    }

    let flat = flatten(&files.iter().map(|f| f.hunks.len()).collect::<Vec<_>>());
    let move_files = tracked_files(&head_tree);
    Ok(App {
        branch,
        head,
        ignore_ws,
        commits,
        files,
        flat,
        move_files,
        mode: Mode::Hunks,
        focus: Pane::Commits, // arrows move the commit list immediately
        commit_cursor: 0,
        hunk_cursor: 0,
        move_cursor: 0,
        move_target: None,
        status: "j/k move · Tab pane · Space select · t target · Enter apply · q quit".into(),
        applied: false,
    })
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, repo: &Repository) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match on_key(app, key.code) {
            Flow::Quit => break,
            Flow::Preview => app.preview(repo),
            Flow::Apply => {
                app.execute(repo);
                if app.applied {
                    break;
                }
            }
            Flow::Continue => {}
        }
    }
    Ok(())
}

/// Pure input handler: mutate `app` and say what the driver should do. No I/O,
/// no `Repository` — this is the whole interaction, unit-tested below.
fn on_key(app: &mut App, key: KeyCode) -> Flow {
    match key {
        KeyCode::Char('q') => return Flow::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_cursor(-1),
        KeyCode::Tab => app.toggle_focus(),
        KeyCode::Char('m') => app.toggle_mode(),
        KeyCode::Char(' ') => app.toggle(),
        KeyCode::Char('t') => app.set_target(),
        KeyCode::Char('a') => app.route_all_to_cursor(),
        KeyCode::Char('A') => app.accept_inference(),
        KeyCode::Char('p') => return Flow::Preview,
        KeyCode::Enter => return Flow::Apply,
        _ => {}
    }
    Flow::Continue
}

impl App {
    fn cursor_commit(&self) -> Option<Oid> {
        self.commits.get(self.commit_cursor).map(|c| c.oid)
    }

    /// Target commit highlighted in the commit list (hunk's target, or move dest).
    fn active_target(&self) -> Option<Oid> {
        match self.mode {
            Mode::Hunks => self.flat.get(self.hunk_cursor).and_then(|&(fi, hi)| self.files[fi].targets[hi]),
            Mode::Move => self.move_target,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            Pane::Commits => self.commit_cursor = step(self.commit_cursor, self.commits.len(), delta),
            Pane::Right => match self.mode {
                Mode::Hunks => self.hunk_cursor = step(self.hunk_cursor, self.flat.len(), delta),
                Mode::Move => self.move_cursor = step(self.move_cursor, self.move_files.len(), delta),
            },
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Pane::Commits => Pane::Right,
            Pane::Right => Pane::Commits,
        };
    }

    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            Mode::Hunks => Mode::Move,
            Mode::Move => Mode::Hunks,
        };
        self.status = match self.mode {
            Mode::Hunks => "hunks mode: fold staged changes".into(),
            Mode::Move => "move mode: pick a file, set a target (t), Enter to move".into(),
        };
    }

    /// Space: toggle the hunk under the cursor (Hunks mode only).
    fn toggle(&mut self) {
        if self.mode != Mode::Hunks {
            return;
        }
        if let Some(&(fi, hi)) = self.flat.get(self.hunk_cursor) {
            let s = &mut self.files[fi].selected[hi];
            *s = !*s;
        }
    }

    /// `t`: set the current hunk's target (Hunks) or the move destination (Move)
    /// to the commit under the commit cursor.
    fn set_target(&mut self) {
        let Some(oid) = self.cursor_commit() else { return };
        match self.mode {
            Mode::Hunks => {
                if let Some(&(fi, hi)) = self.flat.get(self.hunk_cursor) {
                    self.files[fi].targets[hi] = Some(oid);
                    self.status = format!("hunk → {oid:.8}");
                }
            }
            Mode::Move => {
                self.move_target = Some(oid);
                self.status = format!("move destination → {oid:.8}");
            }
        }
    }

    /// `a`: route EVERY selected hunk to the commit under the cursor (= fix).
    fn route_all_to_cursor(&mut self) {
        if self.mode != Mode::Hunks {
            return;
        }
        let Some(oid) = self.cursor_commit() else { return };
        for f in &mut self.files {
            for (hi, sel) in f.selected.iter().enumerate() {
                if *sel {
                    f.targets[hi] = Some(oid);
                }
            }
        }
        self.status = format!("all selected hunks → {oid:.8} (fix)");
    }

    /// `A`: reset every hunk's target to inference's suggestion (= absorb).
    fn accept_inference(&mut self) {
        for f in &mut self.files {
            f.targets = f.inferred.clone();
        }
        self.status = "targets reset to inference (absorb)".into();
    }

    fn preview(&mut self, repo: &Repository) {
        match self.mode {
            Mode::Hunks => match self.build_recipe(repo) {
                Ok(r) if !r.is_empty() => match engine::replay_opts(repo, None, self.head, &r, self.ignore_ws, true) {
                    Ok(oid) if oid == self.head => self.status = "would be a no-op (targets already hold these hunks)".into(),
                    Ok(oid) => self.status = format!("clean, would move {} to {oid:.8}", self.branch),
                    Err(e) => self.status = format!("conflict: {e}"),
                },
                Ok(_) => self.status = "select hunks (Space) and set targets (t) first".into(),
                Err(e) => self.status = format!("preview error: {e}"),
            },
            Mode::Move => match self.move_plan(repo) {
                Ok(Some((base, tip, rec))) => match engine::replay(repo, base, tip, &rec, self.ignore_ws) {
                    Ok(oid) => self.status = format!("clean, would move {} to {oid:.8}", self.branch),
                    Err(e) => self.status = format!("conflict: {e}"),
                },
                Ok(None) => self.status = "pick a file and a destination (t) first".into(),
                Err(e) => self.status = format!("{e}"),
            },
        }
    }

    fn execute(&mut self, repo: &Repository) {
        match self.mode {
            Mode::Hunks => self.execute_hunks(repo),
            Mode::Move => self.execute_move(repo),
        }
    }

    fn execute_hunks(&mut self, repo: &Repository) {
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
        match engine::replay_opts(repo, None, self.head, &recipe, self.ignore_ws, true) {
            Ok(new_tip) if new_tip == self.head => {
                self.status = "no change — hunks already sit in their targets".into();
            }
            // sync = false: deselected / no-home hunks stay staged, not wiped.
            Ok(new_tip) => match ops::promote(repo, &self.branch, new_tip, self.head, "transplant: tui fold", false) {
                Ok(()) => {
                    self.status = format!("{} now at {new_tip:.8}", self.branch);
                    self.applied = true;
                }
                Err(e) => self.status = format!("promote failed: {e}"),
            },
            Err(e) => self.status = format!("conflict, not applied: {e}"),
        }
    }

    fn execute_move(&mut self, repo: &Repository) {
        let (Some(path), Some(target)) = (self.move_files.get(self.move_cursor), self.move_target) else {
            self.status = "pick a file (Space/nav) and a destination (t)".into();
            return;
        };
        match ops::mv(repo, path, &target.to_string(), self.ignore_ws) {
            Ok(o) => {
                self.status = format!("moved {} → {:.8}; {} now at {:.8}", path, target, o.branch, o.new_tip);
                self.applied = true;
            }
            Err(e) => self.status = format!("{e}"),
        }
    }

    /// Build a dry-run plan for the current move selection (for preview). Applies
    /// the same clean-tree guard `ops::mv` enforces, so preview can't promise a
    /// success that execute would refuse.
    fn move_plan(&self, repo: &Repository) -> Result<Option<(Option<Oid>, Oid, engine::Recipe)>> {
        let (Some(path), Some(target)) = (self.move_files.get(self.move_cursor), self.move_target) else {
            return Ok(None);
        };
        ops::require_fully_clean(repo).map_err(anyhow::Error::msg)?;
        let plan = recipe::mv(repo, path, target, self.head).map_err(anyhow::Error::msg)?;
        Ok(Some((plan.base, plan.tip, plan.recipe)))
    }

    /// Assemble the replay recipe from hunk selections: each (file, target) group
    /// of selected hunks becomes one synthetic commit applied at that target.
    fn build_recipe(&self, repo: &Repository) -> Result<engine::Recipe> {
        let mut recipe = engine::Recipe::new();
        for f in &self.files {
            for (t, mask) in recipe_groups(&f.selected, &f.targets) {
                let synth = patch::synthetic_for_hunks(repo, self.head, &f.path, &f.old_full, &f.hunks, &mask)?;
                recipe.add(t, engine::Edit::ApplyChange(synth));
            }
        }
        Ok(recipe)
    }
}

// ── pure helpers (unit-tested) ──────────────────────────────────────────────

fn flatten(counts: &[usize]) -> Vec<(usize, usize)> {
    counts.iter().enumerate().flat_map(|(fi, &n)| (0..n).map(move |hi| (fi, hi))).collect()
}

fn mask_for_target(selected: &[bool], targets: &[Option<Oid>], target: Oid) -> Vec<bool> {
    selected.iter().zip(targets).map(|(&s, t)| s && *t == Some(target)).collect()
}

/// Selected hunks grouped by target commit, each as a per-hunk mask. One entry
/// per distinct target; deselected / no-target hunks are excluded.
fn recipe_groups(selected: &[bool], targets: &[Option<Oid>]) -> Vec<(Oid, Vec<bool>)> {
    let mut ts: Vec<Oid> = selected
        .iter()
        .zip(targets)
        .filter_map(|(&s, t)| if s { *t } else { None })
        .collect();
    ts.sort();
    ts.dedup();
    ts.into_iter().map(|t| (t, mask_for_target(selected, targets, t))).collect()
}

fn step(cur: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (cur as isize + delta).clamp(0, len as isize - 1) as usize
}

fn read_blob(repo: &Repository, tree: &Tree, path: &str) -> Vec<u8> {
    tree.get_path(Path::new(path))
        .and_then(|e| e.to_object(repo))
        .ok()
        .and_then(|o| o.peel_to_blob().ok())
        .map(|b| b.content().to_vec())
        .unwrap_or_default()
}

/// All blob paths in a tree (for move mode's file list).
fn tracked_files(tree: &Tree) -> Vec<String> {
    let mut out = Vec::new();
    let _ = tree.walk(TreeWalkMode::PreOrder, |root, entry| {
        if entry.kind() == Some(ObjectType::Blob) {
            out.push(format!("{root}{}", entry.name().unwrap_or("")));
        }
        TreeWalkResult::Ok
    });
    out
}

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
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .split(cols[1]);
    match app.mode {
        Mode::Hunks => render_hunks(f, app, rows[0]),
        Mode::Move => render_move(f, app, rows[0]),
    }
    render_status(f, app, rows[1]);
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn list_block(title: &str, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title.to_string())
        .border_style(border_style(focused))
}

fn render_commits(f: &mut Frame, app: &App, area: Rect) {
    let target = app.active_target();
    let items: Vec<ListItem> = app
        .commits
        .iter()
        .map(|c| {
            let marker = if Some(c.oid) == target { "◀ " } else { "  " };
            ListItem::new(format!("{:.8} {}{marker}", c.oid, c.summary))
        })
        .collect();
    let list = List::new(items)
        .block(list_block("commits (t: target)", app.focus == Pane::Commits))
        .highlight_symbol("▶ ")
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
            None => "(no home)".into(),
        };
        lines.push(Line::from(format!("{checkbox} {}  → {target}", file.hunks[hi].header)));
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
        items.push(ListItem::new("no staged changes — press `m` for move mode"));
    }
    let list = List::new(items)
        .block(list_block("[HUNKS] staged (Space: toggle · m: move mode)", app.focus == Pane::Right))
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !app.flat.is_empty() {
        state.select(Some(app.hunk_cursor));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn render_move(f: &mut Frame, app: &App, area: Rect) {
    let dest = match app.move_target {
        Some(o) => format!("{o:.8}"),
        None => "(set with t)".into(),
    };
    let items: Vec<ListItem> = app.move_files.iter().map(|p| ListItem::new(p.clone())).collect();
    let list = List::new(items)
        .block(list_block(
            &format!("[MOVE] file → {dest} (t: dest · Enter: move · m: hunks)"),
            app.focus == Pane::Right,
        ))
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !app.move_files.is_empty() {
        state.select(Some(app.move_cursor));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let keys = "j/k move · Tab pane · Space sel · t target · a all→cur · A infer · m mode · p preview · Enter apply · q quit";
    let counts = match app.mode {
        Mode::Hunks => {
            let total: usize = app.files.iter().map(|f| f.hunks.len()).sum();
            let sel: usize = app.files.iter().flat_map(|f| f.selected.iter()).filter(|&&s| s).count();
            format!("{sel}/{total} hunks selected")
        }
        Mode::Move => format!("{} tracked files", app.move_files.len()),
    };
    let text = vec![
        Line::from(Span::styled(keys, dim)),
        Line::from(app.status.clone()),
        Line::from(Span::styled(counts, dim)),
    ];
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(b: u8) -> Oid {
        Oid::from_bytes(&[b; 20]).unwrap()
    }

    /// A headless App with `n_commits` commits and one file of `n_hunks` hunks.
    fn app(n_commits: usize, n_hunks: usize) -> App {
        let commits = (0..n_commits)
            .map(|i| CommitRow { oid: oid(i as u8 + 1), summary: format!("c{i}") })
            .collect();
        let files = if n_hunks > 0 {
            vec![FileEntry {
                path: "f.rs".into(),
                old_full: String::new(),
                hunks: Vec::new(),
                lines: Vec::new(),
                selected: vec![true; n_hunks],
                targets: vec![None; n_hunks],
                inferred: vec![Some(oid(1)); n_hunks],
            }]
        } else {
            Vec::new()
        };
        let flat = flatten(&files.iter().map(|f| f.selected.len()).collect::<Vec<_>>());
        let move_files = vec!["a.txt".into(), "src/b.rs".into()];
        App {
            branch: "refs/heads/main".into(),
            head: oid(9),
            ignore_ws: false,
            commits,
            files,
            flat,
            move_files,
            mode: Mode::Hunks,
            focus: Pane::Commits,
            commit_cursor: 0,
            hunk_cursor: 0,
            move_cursor: 0,
            move_target: None,
            status: String::new(),
            applied: false,
        }
    }

    // ── keybinding regressions (the reported "arrows don't work") ──

    #[test]
    fn arrows_move_commit_list_by_default() {
        let mut a = app(3, 2);
        assert_eq!(a.focus, Pane::Commits);
        on_key(&mut a, KeyCode::Down);
        assert_eq!(a.commit_cursor, 1, "Down moves the commit cursor with default focus");
    }

    #[test]
    fn arrows_equal_jk() {
        let (mut a, mut b) = (app(3, 2), app(3, 2));
        on_key(&mut a, KeyCode::Down);
        on_key(&mut b, KeyCode::Char('j'));
        assert_eq!(a.commit_cursor, b.commit_cursor);
    }

    #[test]
    fn navigable_even_with_no_staged_hunks() {
        let mut a = app(3, 0); // empty hunks — the "felt dead" case
        on_key(&mut a, KeyCode::Down);
        on_key(&mut a, KeyCode::Down);
        assert_eq!(a.commit_cursor, 2, "commit list still navigable with zero hunks");
    }

    #[test]
    fn down_moves_hunk_cursor_when_right_focused() {
        let mut a = app(3, 3);
        on_key(&mut a, KeyCode::Tab); // focus Right
        on_key(&mut a, KeyCode::Down);
        assert_eq!(a.hunk_cursor, 1);
        assert_eq!(a.commit_cursor, 0, "commit cursor untouched while Right-focused");
    }

    // ── focus / mode ──

    #[test]
    fn tab_toggles_focus() {
        let mut a = app(2, 1);
        on_key(&mut a, KeyCode::Tab);
        assert_eq!(a.focus, Pane::Right);
        on_key(&mut a, KeyCode::Tab);
        assert_eq!(a.focus, Pane::Commits);
    }

    #[test]
    fn m_toggles_mode_and_arrows_move_file_list() {
        let mut a = app(2, 1);
        on_key(&mut a, KeyCode::Char('m'));
        assert_eq!(a.mode, Mode::Move);
        on_key(&mut a, KeyCode::Tab); // focus Right (= file list in move mode)
        on_key(&mut a, KeyCode::Down);
        assert_eq!(a.move_cursor, 1);
    }

    // ── selection / targeting ──

    #[test]
    fn space_toggles_selected_hunk() {
        let mut a = app(2, 2);
        on_key(&mut a, KeyCode::Tab);
        on_key(&mut a, KeyCode::Char(' ')); // deselect hunk 0
        assert!(!a.files[0].selected[0]);
    }

    #[test]
    fn t_sets_hunk_target_to_commit_under_cursor() {
        let mut a = app(3, 2);
        on_key(&mut a, KeyCode::Down); // commit_cursor -> 1
        on_key(&mut a, KeyCode::Char('t'));
        assert_eq!(a.files[0].targets[0], Some(oid(2)));
    }

    #[test]
    fn route_all_to_cursor_sets_every_selected_target() {
        let mut a = app(3, 3);
        on_key(&mut a, KeyCode::Down); // -> commit 1
        on_key(&mut a, KeyCode::Char('a'));
        assert!(a.files[0].targets.iter().all(|&t| t == Some(oid(2))));
    }

    #[test]
    fn accept_inference_resets_targets() {
        let mut a = app(3, 2);
        a.files[0].targets = vec![Some(oid(3)), None];
        on_key(&mut a, KeyCode::Char('A'));
        assert_eq!(a.files[0].targets, vec![Some(oid(1)), Some(oid(1))]);
    }

    #[test]
    fn move_mode_sets_destination_with_t() {
        let mut a = app(3, 1);
        on_key(&mut a, KeyCode::Char('m'));
        on_key(&mut a, KeyCode::Down); // commit 1
        on_key(&mut a, KeyCode::Char('t'));
        assert_eq!(a.move_target, Some(oid(2)));
    }

    // ── Flow routing ──

    #[test]
    fn flow_routing() {
        let mut a = app(2, 1);
        assert_eq!(on_key(&mut a, KeyCode::Char('q')), Flow::Quit);
        assert_eq!(on_key(&mut a, KeyCode::Char('p')), Flow::Preview);
        assert_eq!(on_key(&mut a, KeyCode::Enter), Flow::Apply);
        assert_eq!(on_key(&mut a, KeyCode::Char('j')), Flow::Continue);
    }

    // ── recipe assembly ──

    #[test]
    fn recipe_groups_dedup_by_target() {
        let (a, b) = (oid(1), oid(2));
        let selected = [true, true, false, true];
        let targets = [Some(a), Some(b), Some(a), Some(a)];
        let groups = recipe_groups(&selected, &targets);
        assert_eq!(groups.len(), 2, "two distinct targets among selected");
        let ga = groups.iter().find(|(t, _)| *t == a).unwrap();
        assert_eq!(ga.1, vec![true, false, false, true], "hunk 2 deselected, excluded");
        let gb = groups.iter().find(|(t, _)| *t == b).unwrap();
        assert_eq!(gb.1, vec![false, true, false, false]);
    }

    #[test]
    fn recipe_groups_excludes_no_target_hunks() {
        let a = oid(1);
        let groups = recipe_groups(&[true, true], &[Some(a), None]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1, vec![true, false]);
    }

    // ── end-to-end: load → key → apply against a real temp repo ──

    #[test]
    fn end_to_end_absorb_applies_and_moves_branch() {
        let dir = std::env::temp_dir().join(format!("gt-tui-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        {
            let mut c = repo.config().unwrap();
            c.set_str("user.name", "t").unwrap();
            c.set_str("user.email", "t@t").unwrap();
        }
        let commit = |msg: &str, content: &str| {
            std::fs::write(dir.join("f.rs"), content).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(Path::new("f.rs")).unwrap();
            idx.write().unwrap();
            let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
            let sig = repo.signature().unwrap();
            let parents: Vec<_> = repo.head().ok().map(|h| h.peel_to_commit().unwrap()).into_iter().collect();
            let pr: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &pr).unwrap()
        };
        let l8: String = (1..=8).map(|i| format!("l{i}\n")).collect();
        commit("c1", &l8);
        commit("c2", &format!("{l8}extra\n"));
        let before = repo.head().unwrap().target().unwrap();

        // stage a change to line 2 (owned by c1)
        let mut staged: Vec<String> = format!("{l8}extra\n").split_inclusive('\n').map(String::from).collect();
        staged[1] = "L2\n".into();
        std::fs::write(dir.join("f.rs"), staged.concat()).unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("f.rs")).unwrap();
        idx.write().unwrap();

        let mut app = load(&repo, false).unwrap();
        assert!(!app.files.is_empty(), "staged hunk loaded");
        // default state = absorb: all selected, inferred targets -> Enter applies
        assert_eq!(on_key(&mut app, KeyCode::Enter), Flow::Apply);
        app.execute(&repo);
        assert!(app.applied, "applied: {}", app.status);
        assert_ne!(repo.head().unwrap().target().unwrap(), before, "branch moved");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
