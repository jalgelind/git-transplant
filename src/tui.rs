//! Interactive TUI over the engine. One screen drives all operations:
//!
//! - **Hunks mode** (default): the staged diff, one selectable hunk at a time,
//!   each with an inference-prefilled target commit. This IS fix / absorb /
//!   manual A-D — they are just selection+routing states of one recipe:
//!     * absorb  = every hunk selected, targets from inference → Enter
//!     * fix     = route all selected hunks to one commit (`f`) → Enter
//!     * A/D     = set per-hunk targets by hand (`t`)
//! - **Move mode** (`m`): pick a tracked file and a destination commit → op B.
//!
//! Bindings are deliberately arrow-key based — no vim `h/j/k/l`, and no
//! shift-variant pairs (`a`/`A`); actions use distinct mnemonic letters
//! (`t`arget, `f`ix-all, `r`eset, `m`ove, `p`review).
//!
//! `preview` (`p`) is a dry-run replay whose oid is discarded — the same engine
//! call as apply, so it can never disagree. Input handling is a pure
//! `on_key(&mut App, KeyCode) -> Flow` so the whole state machine is unit-tested
//! without a terminal (see `mod tests`).

use std::path::Path;

use anyhow::Result;
use git2::{DiffFormat, DiffOptions, ObjectType, Oid, Patch, Repository, Tree, TreeWalkMode, TreeWalkResult};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
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
    /// Inference's original suggestion, for the `r` (reset) key.
    inferred: Vec<Option<Oid>>,
}

struct CommitRow {
    oid: Oid,
    summary: String,
    /// The commit's own diff (parent→self) as `(origin, text)` lines, shown when
    /// browsing the commit list so you can see what each commit contains.
    diff: Vec<(char, String)>,
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
    /// Enter was pressed once and reported scope; a second Enter applies.
    pending_apply: bool,
    /// Worktree/index has changes — `move` (which needs a clean tree) can't run.
    tree_dirty: bool,
    /// Staged paths that can't be hunk-folded (adds/deletes), for an honest note.
    skipped: Vec<String>,
    /// Scroll offset for the commit-diff pane.
    diff_scroll: u16,
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
        .map(|c| CommitRow {
            oid: c.id(),
            summary: c.summary().unwrap_or("").to_string(),
            diff: commit_diff_lines(repo, c),
        })
        .collect();

    let head_tree = repo.find_commit(head)?.tree()?;
    let index_tree = repo.find_tree(repo.index()?.write_tree()?)?;
    let diff = repo.diff_tree_to_tree(Some(&head_tree), Some(&index_tree), None)?;

    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for delta in diff.deltas() {
        if delta.status() != git2::Delta::Modified {
            // Adds/deletes are staged but not hunk-foldable — record them so the
            // UI can say so instead of claiming "no staged changes".
            if let Some(p) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                skipped.push(p.to_string_lossy().into_owned());
            }
            continue;
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
    let tree_dirty = ops::require_fully_clean(repo).is_err();

    // Lead with the value proposition, not a duplicate of the keymap below.
    let status = if flat.is_empty() {
        match skipped.len() {
            0 => "nothing staged to fold — stage a fix, or press m to move a file".into(),
            n => format!("{n} staged add/delete can't be hunk-folded — press m to move a file"),
        }
    } else {
        let mut s = format!(
            "Enter: absorb {} staged hunk(s) into inferred commits · p: preview first",
            flat.len()
        );
        if !skipped.is_empty() {
            s.push_str(&format!(" · {} add/delete skipped", skipped.len()));
        }
        s
    };

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
        status,
        applied: false,
        pending_apply: false,
        tree_dirty,
        skipped,
        diff_scroll: 0,
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
    // Enter is a two-step gate: the first press reports scope, the second
    // applies. ANY other key cancels the pending apply, so it can't fire late.
    if key != KeyCode::Enter {
        app.pending_apply = false;
    }
    match key {
        // Quit / cancel
        KeyCode::Char('q') => return Flow::Quit,
        KeyCode::Esc => app.status = "cancelled".into(),

        // Navigation — arrow keys, not vim h/j/k/l.
        KeyCode::Down => app.move_cursor(1),
        KeyCode::Up => app.move_cursor(-1),
        KeyCode::Home => app.jump(false),
        KeyCode::End => app.jump(true),
        KeyCode::PageDown => app.diff_scroll = app.diff_scroll.saturating_add(10),
        KeyCode::PageUp => app.diff_scroll = app.diff_scroll.saturating_sub(10),
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Right | KeyCode::Left => app.toggle_focus(),

        // Selection / routing — distinct mnemonic letters, no shift-pairs.
        KeyCode::Char(' ') => app.toggle(),
        KeyCode::Char('t') => app.set_target(),
        KeyCode::Char('f') => app.route_all_to_cursor(), // "fix": all → cursor commit
        KeyCode::Char('r') => app.accept_inference(),    // "reset" to inferred targets
        KeyCode::Char('m') => app.toggle_mode(),

        // Act
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

    /// Short branch name for display (`master`, not `refs/heads/master`).
    fn short_branch(&self) -> &str {
        self.branch.rsplit('/').next().unwrap_or(&self.branch)
    }

    /// Summary of a commit by oid, for target labels.
    fn summary_of(&self, oid: Oid) -> &str {
        self.commits
            .iter()
            .find(|c| c.oid == oid)
            .map(|c| c.summary.as_str())
            .unwrap_or("")
    }

    /// A one-line label for a target commit: `a1b2c3d4 add parser`.
    fn target_label(&self, target: Option<Oid>) -> String {
        match target {
            Some(o) => format!("{o:.8} {}", truncate(self.summary_of(o), 28)),
            None => "(no home)".into(),
        }
    }

    /// Always-visible description of the hunk under the cursor, so selection keys
    /// never act on something the user can't see — even while browsing commits.
    fn context_line(&self) -> String {
        match self.mode {
            Mode::Hunks => match self.flat.get(self.hunk_cursor) {
                Some(&(fi, hi)) => {
                    let f = &self.files[fi];
                    format!(
                        "hunk {}/{} · {} {} {} → {}",
                        self.hunk_cursor + 1,
                        self.flat.len(),
                        if f.selected[hi] { "[x]" } else { "[ ]" },
                        f.path,
                        truncate(&f.hunks[hi].header, 24),
                        self.target_label(f.targets[hi]),
                    )
                }
                None => "no staged hunks".into(),
            },
            Mode::Move => {
                let file = self.move_files.get(self.move_cursor).map(|s| s.as_str()).unwrap_or("-");
                format!("move {} → {}", file, self.target_label(self.move_target))
            }
        }
    }

    /// How many commits a rewrite would touch: from the oldest target to HEAD.
    fn rewrite_span(&self) -> usize {
        let oldest = self
            .files
            .iter()
            .flat_map(|f| f.selected.iter().zip(&f.targets))
            .filter_map(|(&s, t)| if s { *t } else { None })
            .filter_map(|t| self.commits.iter().position(|c| c.oid == t))
            .max(); // commits are newest-first, so max index = oldest commit
        oldest.map(|i| i + 1).unwrap_or(0)
    }

    /// Home/End: jump the focused list to its first or last entry.
    fn jump(&mut self, to_end: bool) {
        let len = match (self.focus, self.mode) {
            (Pane::Commits, _) => self.commits.len(),
            (Pane::Right, Mode::Hunks) => self.flat.len(),
            (Pane::Right, Mode::Move) => self.move_files.len(),
        };
        let target = if to_end { len.saturating_sub(1) } else { 0 };
        match (self.focus, self.mode) {
            (Pane::Commits, _) => {
                self.commit_cursor = target;
                self.diff_scroll = 0;
            }
            (Pane::Right, Mode::Hunks) => self.hunk_cursor = target,
            (Pane::Right, Mode::Move) => self.move_cursor = target,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.focus == Pane::Commits {
            self.diff_scroll = 0; // new commit → start at the top of its diff
        }
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
                    Ok(oid) if oid == self.head => self.status = "no change — targets already hold these hunks".into(),
                    Ok(oid) => self.status = format!("clean, would move {} to {oid:.8}", self.short_branch()),
                    Err(e) => self.status = format!("conflict: {e}"),
                },
                Ok(_) => self.status = "select hunks (Space) and set targets (t) first".into(),
                Err(e) => self.status = format!("preview error: {e}"),
            },
            Mode::Move => match self.move_plan(repo) {
                Ok(Some((base, tip, rec))) => match engine::replay(repo, base, tip, &rec, self.ignore_ws) {
                    Ok(oid) => self.status = format!("clean, would move {} to {oid:.8}", self.short_branch()),
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
        // First Enter states the scope and arms; second Enter actually rewrites.
        if !self.pending_apply {
            let n = self.rewrite_span();
            if n == 0 {
                self.status = "nothing selected to apply".into();
                return;
            }
            self.pending_apply = true;
            self.status = format!(
                "rewrite {n} commit(s) on {} — Enter again to apply · p: preview · any key: cancel",
                self.short_branch()
            );
            return;
        }
        self.pending_apply = false;
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
                self.status = "no change — targets already hold these hunks".into();
            }
            // sync = false: deselected / no-home hunks stay staged, not wiped.
            Ok(new_tip) => match ops::promote(repo, &self.branch, new_tip, self.head, "transplant: tui fold", false) {
                Ok(()) => {
                    self.status = format!("{} now at {new_tip:.8}", self.short_branch());
                    self.applied = true;
                }
                Err(e) => self.status = format!("promote failed: {e}"),
            },
            Err(e) => self.status = format!("conflict, not applied: {e}"),
        }
    }

    fn execute_move(&mut self, repo: &Repository) {
        let (Some(path), Some(target)) = (self.move_files.get(self.move_cursor), self.move_target) else {
            self.status = "pick a file (j/k) and a destination (Tab to commits, then t)".into();
            return;
        };
        if !self.pending_apply {
            self.pending_apply = true;
            self.status = format!(
                "move {path} → {:.8} on {} — Enter again to apply · any key: cancel",
                target,
                self.short_branch()
            );
            return;
        }
        self.pending_apply = false;
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
    // Status spans the FULL width — inside the right column the keymap clipped
    // and lost `p preview · Enter apply · q quit`.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .split(f.area());
    let (body, status_area) = (rows[0], rows[1]);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(body);
    let (left, right) = (cols[0], cols[1]);
    render_commits(f, app, left);
    // Diff-follows-focus (lazygit-style): browsing commits shows the selected
    // commit's own diff; focusing the right pane shows the staged-hunk selector
    // (or the move file list).
    if app.focus == Pane::Commits {
        render_commit_diff(f, app, right);
    } else {
        match app.mode {
            Mode::Hunks => render_hunks(f, app, right),
            Mode::Move => render_move(f, app, right),
        }
    }
    render_status(f, app, status_area);
}

/// A commit's own diff (parent → self) as styled `(origin, text)` lines.
fn commit_diff_lines(repo: &Repository, commit: &git2::Commit) -> Vec<(char, String)> {
    let new_tree = commit.tree().ok();
    let old_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let mut out = Vec::new();
    if let Ok(diff) = repo.diff_tree_to_tree(old_tree.as_ref(), new_tree.as_ref(), None) {
        let _ = diff.print(DiffFormat::Patch, |_d, _h, line| {
            // git's file header arrives as ONE multi-line chunk — split it, or it
            // renders as a garbled run-on line.
            let raw = String::from_utf8_lossy(line.content()).into_owned();
            for part in raw.trim_end_matches('\n').split('\n') {
                out.push((line.origin(), part.to_string()));
            }
            true
        });
    }
    out
}

fn render_commit_diff(f: &mut Frame, app: &App, area: Rect) {
    let empty: Vec<(char, String)> = Vec::new();
    let (title, diff) = match app.commits.get(app.commit_cursor) {
        Some(c) => (format!("[DIFF] {:.8} {} (Tab: select hunks)", c.oid, c.summary), &c.diff),
        None => ("[DIFF]".to_string(), &empty),
    };
    let lines: Vec<Line> = if diff.is_empty() {
        vec![Line::from(Span::styled("(empty commit)", Style::default().fg(Color::DarkGray)))]
    } else {
        diff.iter()
            .map(|(origin, text)| {
                let (prefix, style) = match origin {
                    '+' => ("+", Style::default().fg(Color::Green)),
                    '-' => ("-", Style::default().fg(Color::Red)),
                    'F' | 'H' => ("", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    _ => (" ", Style::default()),
                };
                Line::from(Span::styled(format!("{prefix}{text}"), style))
            })
            .collect()
    };
    f.render_widget(
        Paragraph::new(lines)
            .scroll((app.diff_scroll, 0))
            .block(list_block(&title, app.focus == Pane::Commits)),
        area,
    );
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
            // Marker lives in a LEFT gutter so it survives truncated summaries.
            let marker = if Some(c.oid) == target { "◀" } else { " " };
            ListItem::new(format!("{marker}{:.8} {}", c.oid, c.summary))
        })
        .collect();
    let list = List::new(items)
        .block(list_block("commits · ◀ = target (t sets)", app.focus == Pane::Commits))
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !app.commits.is_empty() {
        state.select(Some(app.commit_cursor));
    }
    f.render_stateful_widget(list, area, &mut state);
}

/// Max diff lines previewed per hunk in the selector list.
const HUNK_PREVIEW_LINES: usize = 10;

fn render_hunks(f: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    for &(fi, hi) in &app.flat {
        let file = &app.files[fi];
        let mut lines: Vec<Line> = Vec::new();
        // Every row is self-describing (file + hunk + target). No separate file
        // header: that made the ▶ cursor land on a header for first-in-file
        // hunks but on the hunk row otherwise, and lost the filename once the
        // list scrolled past the header.
        let checkbox = if file.selected[hi] { "[x]" } else { "[ ]" };
        lines.push(Line::from(vec![
            Span::raw(format!("{checkbox} ")),
            Span::styled(
                format!("{} ", truncate(&file.path, 22)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate(&file.hunks[hi].header, 20),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(format!("  → {}", app.target_label(file.targets[hi]))),
        ]));
        // Cap the preview: one long hunk must not fill the pane and hide the rest.
        let body = file.lines.get(hi).map(|v| v.as_slice()).unwrap_or(&[]);
        for (origin, text) in body.iter().take(HUNK_PREVIEW_LINES) {
            let (prefix, style) = match origin {
                '+' => ('+', Style::default().fg(Color::Green)),
                '-' => ('-', Style::default().fg(Color::Red)),
                _ => (' ', Style::default()),
            };
            lines.push(Line::from(Span::styled(format!("{prefix}{text}"), style)));
        }
        if body.len() > HUNK_PREVIEW_LINES {
            lines.push(Line::from(Span::styled(
                format!("  … +{} more line(s)", body.len() - HUNK_PREVIEW_LINES),
                Style::default().fg(Color::DarkGray),
            )));
        }
        items.push(ListItem::new(lines));
    }
    if items.is_empty() {
        let msg = match app.skipped.len() {
            0 => "no staged changes — press `m` for move mode".to_string(),
            n => format!("{n} staged add/delete — not hunk-foldable; press `m` for move mode"),
        };
        items.push(ListItem::new(msg));
    }
    // Counts live in the TITLE so a cramped status bar can never drop them.
    let total: usize = app.files.iter().map(|f| f.hunks.len()).sum();
    let sel: usize = app.files.iter().flat_map(|f| f.selected.iter()).filter(|&&s| s).count();
    let title = format!("[HUNKS] {sel}/{total} selected · Enter: absorb · Space: toggle");
    let list = List::new(items)
        .block(list_block(&title, app.focus == Pane::Right))
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
        Some(o) => app.target_label(Some(o)),
        None => "(set with t)".into(),
    };
    let items: Vec<ListItem> = app.move_files.iter().map(|p| ListItem::new(p.clone())).collect();
    // `move` needs a clean tree — say so UP FRONT, not after Enter fails.
    let title = if app.tree_dirty {
        "[MOVE] needs a clean tree — commit or stash your staged changes first".to_string()
    } else {
        format!("[MOVE] {} file{} → {dest} · t: dest · Enter: move", app.move_files.len(),
            if app.move_files.len() == 1 { "" } else { "s" })
    };
    let list = List::new(items)
        .block(list_block(&title, app.focus == Pane::Right))
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
    // Mode-aware keymap: only keys that actually do something here. Rendered
    // WITHOUT wrap so the lines below can never be pushed out of the box.
    // Two short lines beat one clipped line: nav on top, actions below.
    let (nav, act) = match app.mode {
        // Keep each line <= 80 chars: they are NOT wrapped, so anything longer
        // silently loses the trailing (most important) keys on a narrow terminal.
        Mode::Hunks => (
            "↑↓ move · ←→/Tab pane · Home/End first/last · PgUp/PgDn scroll · q quit",
            "Space sel · t target · f fix-all · r reset · m move · p preview · Enter apply",
        ),
        Mode::Move => (
            "↑↓ move · ←→/Tab pane · Home/End first/last · q quit",
            "t destination · m hunks-mode · p preview · Enter move",
        ),
    };
    let status_style = if app.pending_apply {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let text = vec![
        Line::from(Span::styled(nav, dim)),
        Line::from(Span::styled(act, dim)),
        // Always show what the cursor is on, so keys never act on hidden state.
        Line::from(Span::styled(app.context_line(), dim)),
        Line::from(Span::styled(app.status.clone(), status_style)),
    ];
    f.render_widget(Paragraph::new(text), area);
}

/// Trim `s` to `max` chars with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
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
            .map(|i| CommitRow { oid: oid(i as u8 + 1), summary: format!("c{i}"), diff: Vec::new() })
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
            pending_apply: false,
            tree_dirty: false,
            skipped: Vec::new(),
            diff_scroll: 0,
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
    fn vim_keys_are_deliberately_not_bound() {
        // Navigation is arrows-only; j/k/h/l must do nothing.
        for k in ['j', 'k', 'h', 'l'] {
            let mut a = app(3, 2);
            assert_eq!(on_key(&mut a, KeyCode::Char(k)), Flow::Continue);
            assert_eq!(a.commit_cursor, 0, "'{k}' must not navigate");
            assert_eq!(a.hunk_cursor, 0, "'{k}' must not navigate");
        }
    }

    #[test]
    fn home_and_end_jump_the_focused_list() {
        let mut a = app(4, 3);
        on_key(&mut a, KeyCode::End);
        assert_eq!(a.commit_cursor, 3, "End → last commit");
        on_key(&mut a, KeyCode::Home);
        assert_eq!(a.commit_cursor, 0, "Home → first commit");
        on_key(&mut a, KeyCode::Tab); // focus hunks
        on_key(&mut a, KeyCode::End);
        assert_eq!(a.hunk_cursor, 2, "End → last hunk");
    }

    #[test]
    fn left_right_also_switch_panes() {
        let mut a = app(3, 2);
        on_key(&mut a, KeyCode::Right);
        assert_eq!(a.focus, Pane::Right);
        on_key(&mut a, KeyCode::Left);
        assert_eq!(a.focus, Pane::Commits);
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
        on_key(&mut a, KeyCode::Char('f')); // f = fix-all→cursor
        assert!(a.files[0].targets.iter().all(|&t| t == Some(oid(2))));
    }

    #[test]
    fn accept_inference_resets_targets() {
        let mut a = app(3, 2);
        a.files[0].targets = vec![Some(oid(3)), None];
        on_key(&mut a, KeyCode::Char('r')); // r = reset to inference
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
        assert_eq!(on_key(&mut a, KeyCode::Down), Flow::Continue);
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

    // ── repo-backed helpers ──

    struct Fixture {
        dir: std::path::PathBuf,
        repo: Repository,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A 2-commit repo (c1: l1..l8, c2: +extra) with a staged edit to line 2.
    fn staged_fixture(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("gt-tui-{tag}-{}", std::process::id()));
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
            repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &pr).unwrap();
        };
        let l8: String = (1..=8).map(|i| format!("l{i}\n")).collect();
        commit("c1", &l8);
        commit("c2", &format!("{l8}extra\n"));
        let mut staged: Vec<String> = format!("{l8}extra\n").split_inclusive('\n').map(String::from).collect();
        staged[1] = "L2\n".into();
        std::fs::write(dir.join("f.rs"), staged.concat()).unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("f.rs")).unwrap();
        idx.write().unwrap();
        Fixture { dir, repo }
    }

    /// Render `app` at a given terminal size and return the visible text.
    fn render_at(app: &App, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    fn render_to_text(app: &App) -> String {
        render_at(app, 120, 30)
    }

    /// A repo whose only staged change is a brand-new file.
    fn added_file_fixture(tag: &str) -> Fixture {
        let f = staged_fixture(tag);
        {
            // reset the staged edit, then stage a NEW file instead
            let head = f.repo.head().unwrap().peel_to_commit().unwrap();
            f.repo.reset(head.as_object(), git2::ResetType::Hard, None).unwrap();
        }
        std::fs::write(f.dir.join("brand_new.rs"), "fn added() {}\n").unwrap();
        let mut idx = f.repo.index().unwrap();
        idx.add_path(Path::new("brand_new.rs")).unwrap();
        idx.write().unwrap();
        f
    }

    // ── end-to-end: load → key → apply against a real temp repo ──

    #[test]
    fn end_to_end_absorb_applies_and_moves_branch() {
        let f = staged_fixture("e2e");
        let before = f.repo.head().unwrap().target().unwrap();

        let mut app = load(&f.repo, false).unwrap();
        assert!(!app.files.is_empty(), "staged hunk loaded");
        assert!(
            app.commits.iter().any(|c| c.diff.iter().any(|(o, t)| *o == '+' && t.contains("extra"))),
            "commit rows carry their own diff for browsing"
        );
        // default state = absorb: all selected with inferred targets.
        // Enter is a two-step gate: the first press only arms + reports scope.
        assert_eq!(on_key(&mut app, KeyCode::Enter), Flow::Apply);
        app.execute(&f.repo);
        assert!(!app.applied, "first Enter must NOT apply");
        assert!(app.pending_apply, "first Enter arms the confirm");
        assert!(app.status.contains("Enter again"), "scope reported: {}", app.status);
        assert_eq!(f.repo.head().unwrap().target().unwrap(), before, "ref unmoved after arming");

        // second Enter applies for real
        assert_eq!(on_key(&mut app, KeyCode::Enter), Flow::Apply);
        app.execute(&f.repo);
        assert!(app.applied, "applied: {}", app.status);
        assert_ne!(f.repo.head().unwrap().target().unwrap(), before, "branch moved");
    }

    #[test]
    fn any_key_cancels_a_pending_apply() {
        let mut a = app(3, 2);
        a.pending_apply = true;
        on_key(&mut a, KeyCode::Down); // any other key
        assert!(!a.pending_apply, "a stray key must cancel the armed apply");
    }

    // ── rendering tests (drive the actual TUI via TestBackend) ──

    #[test]
    fn renders_commit_list_and_selected_commit_diff() {
        let f = staged_fixture("render-browse");
        let app = load(&f.repo, false).unwrap(); // default focus = Commits
        let text = render_to_text(&app);
        assert!(text.contains("commits"), "commit list pane rendered");
        assert!(text.contains("[DIFF]"), "commit-diff pane shown while browsing");
        // the selected (newest) commit c2 introduced `extra` — its diff is visible
        assert!(text.contains("extra"), "browsing a commit shows its diff (regression)");
    }

    #[test]
    fn renders_hunk_selector_when_right_focused() {
        let f = staged_fixture("render-hunks");
        let mut app = load(&f.repo, false).unwrap();
        on_key(&mut app, KeyCode::Tab); // focus the right pane
        let text = render_to_text(&app);
        assert!(text.contains("[HUNKS]"), "staged-hunk selector shown when right-focused");
        assert!(text.contains("[x]"), "hunk checkboxes rendered");
    }

    // ── UX regressions (tasks #11–#19) ──

    #[test]
    fn launch_status_advertises_absorb_not_a_second_keymap() {
        let f = staged_fixture("ux-launch");
        let app = load(&f.repo, false).unwrap();
        assert!(app.status.contains("absorb"), "sells the primary action: {}", app.status);
        assert!(!app.status.contains("Tab pane"), "must not duplicate the keymap: {}", app.status);
    }

    #[test]
    fn target_marker_survives_long_commit_summaries() {
        let f = staged_fixture("ux-marker");
        let mut app = load(&f.repo, false).unwrap();
        for c in app.commits.iter_mut() {
            c.summary = "a very long commit summary that will certainly be truncated".into();
        }
        assert!(app.active_target().is_some(), "inference set a target");
        let text = render_at(&app, 100, 30);
        assert!(text.contains('◀'), "target marker visible in the left gutter");
    }

    #[test]
    fn context_line_always_names_the_current_hunk() {
        let f = staged_fixture("ux-ctx");
        let app = load(&f.repo, false).unwrap();
        let ctx = app.context_line();
        assert!(ctx.starts_with("hunk 1/"), "{ctx}");
        assert!(ctx.contains("f.rs"), "{ctx}");
        // and it's on screen even though focus is the commit list
        assert!(render_to_text(&app).contains("hunk 1/"), "context line rendered while browsing");
    }

    #[test]
    fn selection_counts_survive_a_cramped_terminal() {
        let f = staged_fixture("ux-narrow");
        let mut app = load(&f.repo, false).unwrap();
        on_key(&mut app, KeyCode::Tab); // show the hunk pane
        let text = render_at(&app, 80, 24);
        assert!(text.contains("selected"), "counts live in the title, not the overflowing status");
    }

    #[test]
    fn move_mode_warns_up_front_when_tree_is_dirty() {
        let f = staged_fixture("ux-move-dirty"); // fixture has a staged change
        let mut app = load(&f.repo, false).unwrap();
        assert!(app.tree_dirty, "fixture is dirty");
        on_key(&mut app, KeyCode::Char('m'));
        on_key(&mut app, KeyCode::Tab);
        let text = render_at(&app, 100, 30);
        assert!(text.contains("clean tree"), "move mode says so before you press Enter");
    }

    #[test]
    fn keymap_is_mode_aware() {
        let f = staged_fixture("ux-keymap");
        let mut app = load(&f.repo, false).unwrap();
        let hunks = render_to_text(&app);
        on_key(&mut app, KeyCode::Char('m'));
        let moves = render_at(&app, 120, 30);
        assert!(hunks.contains("f fix-all"), "hunks keymap shows selection keys");
        assert!(!moves.contains("f fix-all"), "move keymap drops keys that no-op there");
        assert!(moves.contains("t destination"), "move keymap shows its own keys");
    }

    #[test]
    fn keymap_is_not_clipped_on_a_narrow_terminal() {
        // The keymap lines aren't wrapped, so an over-long line silently drops
        // the trailing keys — which are the most important ones.
        let f = staged_fixture("ux-keymap-narrow");
        let app = load(&f.repo, false).unwrap();
        let text = render_at(&app, 80, 24);
        assert!(text.contains("Enter apply"), "the apply key must survive 80 cols");
        assert!(text.contains("q quit"), "the quit key must survive 80 cols");
    }

    #[test]
    fn staged_added_file_is_reported_not_hidden() {
        let f = added_file_fixture("ux-added");
        let app = load(&f.repo, false).unwrap();
        assert!(app.flat.is_empty(), "an add has no foldable hunks");
        assert!(!app.skipped.is_empty(), "but it IS recorded");
        let text = render_at(&app, 100, 30);
        assert!(text.contains("add/delete"), "user is told, not shown 'no staged changes'");
    }

    #[test]
    fn commit_diff_header_is_not_a_run_on_line() {
        let f = staged_fixture("ux-header");
        let app = load(&f.repo, false).unwrap();
        let text = render_to_text(&app);
        assert!(!text.contains("b/f.rsindex"), "git's multi-line header must be split");
    }






    /// Two files, two separated hunks each → 4 hunks with different owners.
    fn multi_hunk_fixture(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("gt-tui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        {
            let mut c = repo.config().unwrap();
            c.set_str("user.name", "t").unwrap();
            c.set_str("user.email", "t@t").unwrap();
        }
        let long = |p: &str| (1..=30).map(|i| format!("{p}{i}\n")).collect::<String>();
        let commit = |msg: &str, files: &[(&str, String)]| {
            let mut idx = repo.index().unwrap();
            for (n, c) in files {
                std::fs::write(dir.join(n), c).unwrap();
                idx.add_path(Path::new(n)).unwrap();
            }
            idx.write().unwrap();
            let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
            let sig = repo.signature().unwrap();
            let parents: Vec<_> = repo.head().ok().map(|h| h.peel_to_commit().unwrap()).into_iter().collect();
            let pr: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &pr).unwrap();
        };
        commit("c1 adds a.rs", &[("a.rs", long("a"))]);
        commit("c2 adds b.rs", &[("b.rs", long("b"))]);
        // stage 2 separated hunks in each file
        let edit = |base: &str, idxs: &[usize], pre: &str| {
            let mut v: Vec<String> = base.split_inclusive('\n').map(String::from).collect();
            for &i in idxs { v[i] = format!("{pre}{}\n", i + 1); }
            v.concat()
        };
        let mut idx = repo.index().unwrap();
        std::fs::write(dir.join("a.rs"), edit(&long("a"), &[1, 20], "EDITED-a")).unwrap();
        std::fs::write(dir.join("b.rs"), edit(&long("b"), &[3, 25], "EDITED-b")).unwrap();
        idx.add_path(Path::new("a.rs")).unwrap();
        idx.add_path(Path::new("b.rs")).unwrap();
        idx.write().unwrap();
        Fixture { dir, repo }
    }


    // ── hunk browser / selector ──

    #[test]
    fn hunk_browser_lists_every_hunk_across_files_with_owner_targets() {
        let f = multi_hunk_fixture("hb-list");
        let app = load(&f.repo, false).unwrap();
        assert_eq!(app.files.len(), 2, "two changed files");
        assert_eq!(app.flat, vec![(0, 0), (0, 1), (1, 0), (1, 1)], "4 hunks in order");
        // hunks of a.rs belong to c1, hunks of b.rs to c2 (different owners)
        let a_t = app.files[0].targets.clone();
        let b_t = app.files[1].targets.clone();
        assert!(a_t.iter().all(|t| t.is_some()) && b_t.iter().all(|t| t.is_some()));
        assert_ne!(a_t[0], b_t[0], "different files routed to their own owners");
    }

    #[test]
    fn every_hunk_row_names_its_file_so_context_survives_scrolling() {
        let f = multi_hunk_fixture("hb-ctx");
        let mut app = load(&f.repo, false).unwrap();
        on_key(&mut app, KeyCode::Tab);
        // scroll to the LAST hunk, where the first file's header is long gone
        on_key(&mut app, KeyCode::End);
        assert_eq!(app.hunk_cursor, 3);
        let text = render_at(&app, 100, 30);
        assert!(text.contains("b.rs"), "the visible hunk rows still name their file");
    }

    #[test]
    fn cursor_walks_hunks_one_by_one_in_order() {
        let f = multi_hunk_fixture("hb-nav");
        let mut app = load(&f.repo, false).unwrap();
        on_key(&mut app, KeyCode::Tab);
        for expected in 0..4 {
            assert_eq!(app.hunk_cursor, expected);
            assert_eq!(app.flat[app.hunk_cursor], app.flat[expected]);
            on_key(&mut app, KeyCode::Down);
        }
        assert_eq!(app.hunk_cursor, 3, "clamps at the last hunk");
    }

    #[test]
    fn space_toggles_only_the_hunk_under_the_cursor() {
        let f = multi_hunk_fixture("hb-toggle");
        let mut app = load(&f.repo, false).unwrap();
        on_key(&mut app, KeyCode::Tab);
        on_key(&mut app, KeyCode::Down); // hunk 1 = a.rs second hunk
        on_key(&mut app, KeyCode::Char(' '));
        assert!(!app.files[0].selected[1], "cursor hunk deselected");
        assert!(app.files[0].selected[0], "sibling hunk untouched");
        assert!(app.files[1].selected.iter().all(|&s| s), "other file untouched");
        // and the title count reflects it
        assert!(render_at(&app, 100, 30).contains("3/4 selected"));
    }

    #[test]
    fn per_hunk_targets_are_independent() {
        let f = multi_hunk_fixture("hb-target");
        let mut app = load(&f.repo, false).unwrap();
        on_key(&mut app, KeyCode::Tab);
        on_key(&mut app, KeyCode::Down); // hunk 1 (a.rs, inferred → c1)
        on_key(&mut app, KeyCode::Tab); // back to commits; cursor 0 = newest (c2)
        let picked = app.commits[app.commit_cursor].oid;
        assert_ne!(Some(picked), app.files[0].targets[0], "picking a DIFFERENT commit");
        on_key(&mut app, KeyCode::Char('t'));
        assert_eq!(app.files[0].targets[1], Some(picked), "only hunk 1 retargeted");
        assert_ne!(app.files[0].targets[0], Some(picked), "hunk 0 keeps its inferred target");
    }

    #[test]
    fn a_long_hunk_preview_is_capped_so_others_stay_visible() {
        let f = multi_hunk_fixture("hb-cap");
        let mut app = load(&f.repo, false).unwrap();
        // inflate one hunk's rendered body well past the cap
        app.files[0].lines[0] = (0..40).map(|i| (' ', format!("ctx{i}"))).collect();
        on_key(&mut app, KeyCode::Tab);
        let text = render_at(&app, 100, 30);
        assert!(text.contains("more line(s)"), "long hunk is truncated with a marker");
        assert!(text.contains("a.rs"), "and the list still shows other rows");
    }



    #[test]
    fn renders_move_file_list_in_move_mode() {
        let f = staged_fixture("render-move");
        let mut app = load(&f.repo, false).unwrap();
        on_key(&mut app, KeyCode::Char('m')); // move mode
        on_key(&mut app, KeyCode::Tab); // focus right pane (file list)
        let text = render_to_text(&app);
        assert!(text.contains("[MOVE]"), "move file list shown in move mode");
        assert!(text.contains("f.rs"), "tracked file listed");
    }
}
