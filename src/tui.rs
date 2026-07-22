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
//! - **Shape edits** (commit pane): `[` / `]` move the selected commit, `d`
//!   marks it dropped, `S` squashes it into its parent. All three go through the
//!   same `p` preview and two-step Enter as everything else.
//!
//! Bindings are deliberately arrow-key based — no vim `h/j/k/l`, and no
//! shift-variant pairs (`a`/`A`); actions use distinct mnemonic letters
//! (`t`arget, `f`ix-all, `r`eset, `m`ove, `p`review). `S` (squash) is the one
//! shift key: it acts on the commit list, like lowercase `s`, and the two are
//! the only pair where that reads as a family rather than an on/off switch.
//!
//! `preview` (`p`) is a dry-run replay whose oid is discarded — the same engine
//! call as apply, so it can never disagree. Input handling is a pure
//! `on_key(&mut App, KeyCode) -> Flow` so the whole state machine is unit-tested
//! without a terminal (see `mod tests`).

use std::path::Path;

use anyhow::Result;
use git2::{DiffFormat, ObjectType, Oid, Repository, Tree, TreeWalkMode, TreeWalkResult};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
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
    selected: Vec<bool>,
    /// Working (user-editable) target per hunk.
    targets: Vec<Option<Oid>>,
    /// Inference's original suggestion, for the `r` (reset) key.
    inferred: Vec<Option<Oid>>,
    /// Filemode on the CHANGED side (exec bit / symlink must survive the move).
    mode: i32,
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

/// Where the hunks in the right pane come from.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    /// The staged change (HEAD → index) — fold new work into old commits.
    Staged,
    /// An existing commit's own diff — move hunks OUT of it into another commit.
    Commit(Oid),
}

/// A pending stack-SHAPE edit made in the commit pane. Previewed with `p` and
/// applied through the same two-step Enter as every other operation.
#[derive(Clone, PartialEq, Debug)]
enum Shape {
    None,
    /// A reorder is in progress; the payload is the ORIGINAL commit order
    /// (newest-first), so Esc can put the list back.
    Reorder(Vec<Oid>),
    Drop(Oid),
    Squash(Oid),
}

/// What the pure key handler asks the driver to do next.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Flow {
    Continue,
    Quit,
    Preview,
    Apply,
    /// Load the commit under the cursor as the hunk source (needs the repo).
    OpenCommit,
    /// Return to the staged-hunk source (needs the repo to reload).
    ResetSource,
}

struct App {
    branch: String,
    head: Oid,
    /// The flags the CLI was given (minus `--dry-run`: `p` IS the dry run).
    opts: ops::Opts,
    /// Oldest commit the view is bounded to, exclusive — `--base`, or the
    /// default cap. Every shape plan uses it, so a plan can only ever reorder
    /// commits that were actually on screen.
    base: Option<Oid>,
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
    /// Where the right pane's hunks come from.
    source: Source,
    /// Commit the synthetics are parented at: HEAD for staged work, the source
    /// commit's PARENT when moving hunks out of a commit.
    source_base: Oid,
    /// Pending drop / reorder / squash from the commit pane.
    shape: Shape,
}

/// Launch the TUI, run it to completion, and print the final verdict.
pub fn run(repo: &Repository, base: Option<Oid>, opts: ops::Opts) -> Result<()> {
    // `--dry-run` is meaningless here: `p` previews and Enter is a two-step gate.
    let mut app = load(repo, base, ops::Opts { dry_run: false, ..opts })?;
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, &mut app, repo);
    ratatui::restore();
    res?;
    println!("{}", app.status);
    Ok(())
}

/// How many commits the TUI offers without `--base`. Every row costs a full
/// tree diff at load and widens the blame window, and nobody reorders the commit
/// 400 back. `hg absorb` caps at 50, `git absorb` at 10; 50 is the generous one.
const DEFAULT_STACK: usize = 50;

/// Gather the stack, staged hunks, tracked files, and inferred targets.
/// `base` bounds the stack shown (exclusive); None applies [`DEFAULT_STACK`].
fn load(repo: &Repository, base: Option<Oid>, opts: ops::Opts) -> Result<App> {
    let branch = ops::head_branch(repo)?;
    let head = git::resolve(repo, "HEAD")?;

    // Bounded by the first merge commit: a merge anywhere in the ancestry must
    // not stop you rewriting the linear stack on top of it.
    let mut stack = match base {
        Some(b) => git::linear_commits(repo, Some(b), head)?,
        None => git::linear_window(repo, head)?,
    };
    let mut base = base;
    if base.is_none() && stack.len() > DEFAULT_STACK {
        stack.drain(..stack.len() - DEFAULT_STACK);
        base = stack[0].parent_id(0).ok();
    }
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
        let old = git::blob_at(repo, &head_tree, Path::new(&path));
        let new = git::blob_at(repo, &index_tree, Path::new(&path));
        let hunks = match patch::hunks(&old, &new) {
            Ok(h) if !h.is_empty() => h,
            _ => continue,
        };
        // BOTH sides must be valid UTF-8: patch::hunks reads line text with
        // from_utf8_lossy, so an unchecked `new` would commit U+FFFD in place of
        // the original bytes. (ops::collapse checks both; this path must too.)
        let old_full = match (String::from_utf8(old.clone()), std::str::from_utf8(&new)) {
            (Ok(s), Ok(_)) => s,
            _ => {
                // Binary / non-UTF-8: not safely hunk-foldable. Record it so the
                // UI says so — silently dropping staged work is never acceptable.
                skipped.push(path);
                continue;
            }
        };
        let inferred = inference::infer_targets(repo, &path, &hunks, &window)
            .unwrap_or_else(|_| vec![None; hunks.len()]);
        let selected = vec![true; hunks.len()];
        let mode = index_tree
            .get_path(Path::new(&path))
            .map(|e| e.filemode())
            .unwrap_or(0o100644);
        files.push(FileEntry {
            path,
            old_full,
            hunks,
            selected,
            targets: inferred.clone(),
            inferred,
            mode,
        });
    }

    let flat = flatten(&files.iter().map(|f| f.hunks.len()).collect::<Vec<_>>());
    let move_files = tracked_files(&head_tree);
    let tree_dirty = ops::require_fully_clean(repo).is_err();

    // Lead with the value proposition, not a duplicate of the keymap below.
    let status = if flat.is_empty() {
        // Staging is only ONE of the two workflows — say so, or this reads as
        // "you must stage something to use this tool".
        match skipped.len() {
            0 => "press s on a commit to move its hunks · or `git add` a fix and reopen".into(),
            n => format!("{n} staged file(s) can't be hunk-folded (binary or whole-file) — press s on a commit, or m to move a file"),
        }
    } else {
        let mut s = format!(
            "Enter: absorb {} staged hunk(s) into inferred commits · p: preview first",
            flat.len()
        );
        if !skipped.is_empty() {
            s.push_str(&format!(" · {} file(s) not hunk-foldable", skipped.len()));
        }
        s
    };

    Ok(App {
        branch,
        head,
        opts,
        base,
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
        source: Source::Staged,
        source_base: head,
        shape: Shape::None,
    })
}

/// Return to the staged-hunk source, keeping the user's place in the commit list.
fn reset_to_staged(app: &mut App, repo: &Repository) {
    match load(repo, app.base, app.opts) {
        Ok(fresh) => {
            let cc = app.commit_cursor;
            *app = fresh;
            app.commit_cursor = cc;
            app.status = "back to staged hunks".into();
        }
        Err(e) => app.status = format!("{e}"),
    }
}

/// Load the commit under the cursor as the hunk source: its own diff becomes the
/// selectable hunk list, so you can move hunks OUT of it into another commit.
/// Pressing `s` again on the same commit returns to the staged view.
fn open_commit_source(app: &mut App, repo: &Repository) {
    // Only from the commit list, and only in hunks mode — pressing `s` while the
    // hunk pane is focused used to silently discard every pick.
    if app.focus != Pane::Commits || app.mode != Mode::Hunks {
        app.status = "press s on the commit list (hunks mode) to use its hunks".into();
        return;
    }
    let Some(row) = app.commits.get(app.commit_cursor) else { return };
    let oid = row.oid;
    if app.source == Source::Commit(oid) {
        reset_to_staged(app, repo);
        return;
    }
    let picked = app.picked();
    let Ok(commit) = repo.find_commit(oid) else { return };
    let Ok(parent) = commit.parent(0) else {
        app.status = "root commit has no parent — can't move hunks out of it".into();
        return;
    };
    let (Ok(new_tree), Ok(old_tree)) = (commit.tree(), parent.tree()) else { return };
    let Ok(diff) = repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None) else { return };

    let mut files = Vec::new();
    for delta in diff.deltas() {
        // A DELETED path can't be modelled as a movable hunk: `apply_selected`
        // would produce an empty blob, not a removal, so it always conflicts.
        // Modified and Added both work (an added file is one whole-file hunk).
        if !matches!(delta.status(), git2::Delta::Modified | git2::Delta::Added) {
            continue;
        }
        let Some(p) = delta.new_file().path().or_else(|| delta.old_file().path()) else { continue };
        let path = p.to_string_lossy().into_owned();
        let old = git::blob_at(repo, &old_tree, Path::new(&path));
        let new = git::blob_at(repo, &new_tree, Path::new(&path));
        let Ok(hunks) = patch::hunks(&old, &new) else { continue };
        if hunks.is_empty() {
            continue;
        }
        // Both sides must be UTF-8 — see the same guard in `load`.
        let (Ok(old_full), Ok(_)) = (String::from_utf8(old.clone()), std::str::from_utf8(&new)) else {
            continue;
        };
        let n = hunks.len();
        let mode = new_tree
            .get_path(Path::new(&path))
            .map(|e| e.filemode())
            .unwrap_or(0o100644);
        files.push(FileEntry {
            path,
            old_full,
            hunks,
            // Nothing pre-selected: moving a commit's hunks is deliberate.
            selected: vec![false; n],
            targets: vec![None; n],
            inferred: vec![None; n],
            mode,
        });
    }

    if files.is_empty() {
        app.status = format!("{oid:.8} has no text hunks to move");
        return;
    }
    app.flat = flatten(&files.iter().map(|f| f.hunks.len()).collect::<Vec<_>>());
    app.files = files;
    app.source = Source::Commit(oid);
    app.source_base = parent.id();
    app.hunk_cursor = 0;
    app.focus = Pane::Right;
    let lost = if picked > 0 {
        format!(" ({picked} earlier pick(s) discarded)")
    } else {
        String::new()
    };
    app.status = format!(
        "{} hunk(s) from {oid:.8}{lost} — Space to pick, then Tab to a destination and press t",
        app.flat.len()
    );
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
            Flow::OpenCommit => open_commit_source(app, repo),
            Flow::ResetSource => reset_to_staged(app, repo),
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
    let was_pending = app.pending_apply;
    if key != KeyCode::Enter {
        app.pending_apply = false;
    }
    match key {
        // Quit / cancel
        KeyCode::Char('q') => return Flow::Quit,
        KeyCode::Esc => {
            if let Some(flow) = app.go_back(was_pending) {
                return flow;
            }
        }

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
        KeyCode::Char('s') => return Flow::OpenCommit, // source: this commit's hunks

        // Stack shape — commit pane only. `[`/`]` are plain characters every
        // terminal delivers, unlike shift+arrow.
        KeyCode::Char('[') => app.move_commit(-1),
        KeyCode::Char(']') => app.move_commit(1),
        KeyCode::Char('d') => app.mark_shape(true),
        KeyCode::Char('S') => app.mark_shape(false),

        // Act
        KeyCode::Char('p') => return Flow::Preview,
        KeyCode::Enter => return Flow::Apply,
        _ => {}
    }
    Flow::Continue
}

impl App {
    /// How many hunks are selected, across every file.
    fn picked(&self) -> usize {
        self.files.iter().flat_map(|f| f.selected.iter()).filter(|&&s| s).count()
    }

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
        ops::short_branch(&self.branch)
    }

    /// Is `a` newer than `b`? The list is newest-first, so a lower index is newer.
    fn is_newer(&self, a: Oid, b: Oid) -> bool {
        let idx = |o: Oid| self.commits.iter().position(|c| c.oid == o);
        matches!((idx(a), idx(b)), (Some(ia), Some(ib)) if ia < ib)
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
            // Moving a commit's hunks has no inferred home — it's an instruction.
            None if matches!(self.source, Source::Commit(_)) => "(pick a destination)".into(),
            None => "(no home)".into(),
        }
    }

    /// Always-visible description of the hunk under the cursor, so selection keys
    /// never act on something the user can't see — even while browsing commits.
    /// Short name of the current hunk source, e.g. `staged` or `from a1b2c3d4`.
    fn source_label(&self) -> String {
        match self.source {
            Source::Staged => "staged".into(),
            Source::Commit(o) => format!("from {o:.8}"),
        }
    }

    /// The one line that is ALWAYS on screen: which source, how many picked, and
    /// what the cursor is on. It is the only state visible while you Tab away to
    /// choose a destination, so it carries the source and the count.
    fn context_line(&self) -> String {
        match self.mode {
            Mode::Hunks => {
                let picked = self.picked();
                match self.flat.get(self.hunk_cursor) {
                    Some(&(fi, hi)) => {
                        let f = &self.files[fi];
                        format!(
                            "{} · hunk {}/{} · {picked} picked · {} {} → {}",
                            self.source_label(),
                            self.hunk_cursor + 1,
                            self.flat.len(),
                            if f.selected[hi] { "[x]" } else { "[ ]" },
                            truncate(&f.path, 20),
                            self.target_label(f.targets[hi]),
                        )
                    }
                    None => format!("{} · no hunks", self.source_label()),
                }
            }
            Mode::Move => {
                let file = self.move_files.get(self.move_cursor).map(|s| s.as_str()).unwrap_or("-");
                format!("move {} → {}", file, self.target_label(self.move_target))
            }
        }
    }

    /// How many commits a rewrite would touch: from the oldest EDITED commit to
    /// HEAD. That includes the source commit — a forward move also reverts the
    /// hunk there, and the source is older than the target in that direction.
    fn rewrite_span(&self) -> usize {
        let idx = |o: Oid| self.commits.iter().position(|c| c.oid == o);
        let mut oldest = self
            .files
            .iter()
            .flat_map(|f| f.selected.iter().zip(&f.targets))
            .filter_map(|(&s, t)| if s { *t } else { None })
            .filter_map(idx)
            .max(); // commits are newest-first, so max index = oldest commit
        if oldest.is_some() {
            if let Source::Commit(src) = self.source {
                oldest = oldest.max(idx(src));
            }
        }
        oldest.map(|i| i + 1).unwrap_or(0)
    }

    /// Parent of the OLDEST commit the recipe touches — the base to replay from.
    /// Walking to the root instead would rewrite untouched history and, worse,
    /// abort on a merge commit deeper in the stack that the edit never reaches.
    fn replay_base(&self, repo: &Repository) -> Option<Oid> {
        let idx = |o: Oid| self.commits.iter().position(|c| c.oid == o);
        let mut oldest = self
            .files
            .iter()
            .flat_map(|f| f.selected.iter().zip(&f.targets))
            .filter_map(|(&s, t)| if s { *t } else { None })
            .filter_map(idx)
            .max();
        if oldest.is_some() {
            if let Source::Commit(src) = self.source {
                oldest = oldest.max(idx(src));
            }
        }
        // None when that commit is a root: replay from the beginning.
        repo.find_commit(self.commits.get(oldest?)?.oid).ok()?.parent_id(0).ok()
    }

    /// Commits this apply would DELETE, straight from the engine.
    ///
    /// Guessing from the selection ("every hunk moved away, so the source
    /// empties") was wrong in both directions: it ignored the files `load`
    /// skipped — binaries, deletions — which keep the commit alive, and with no
    /// files at all it vacuously promised a drop. The replay is the same call
    /// `p` already makes, and it is the thing that actually decides.
    fn dropped_by_replay(&self, repo: &Repository) -> Vec<Oid> {
        let Ok(recipe) = self.build_recipe(repo) else { return Vec::new() };
        engine::replay(repo, self.replay_base(repo), self.head, &recipe, self.opts.merge(), true)
            .map(|p| p.dropped)
            .unwrap_or_default()
    }

    // ── stack shape (commit pane) ──────────────────────────────────────────

    /// The order the commit list is currently SHOWING, oldest-first — which is
    /// exactly the plan a reorder replays.
    fn commit_order(&self) -> Vec<Oid> {
        self.commits.iter().rev().map(|c| c.oid).collect()
    }

    /// Undo a pending reorder's effect on the displayed list, and clear the mark.
    fn clear_shape(&mut self) {
        if let Shape::Reorder(orig) = std::mem::replace(&mut self.shape, Shape::None) {
            // ponytail: O(n²) over the visible stack; a rank map if it ever grows.
            self.commits.sort_by_key(|c| orig.iter().position(|&o| o == c.oid).unwrap_or(usize::MAX));
        }
        self.shape = Shape::None;
    }

    /// Shape keys act on the commit list only — say so rather than doing nothing.
    fn shape_pane(&mut self) -> bool {
        if self.focus == Pane::Commits && self.mode == Mode::Hunks {
            return true;
        }
        self.status = "shape keys ([ ] d S) work on the commit list".into();
        false
    }

    /// `[` / `]`: move the selected commit one step newer / older in the stack.
    fn move_commit(&mut self, delta: isize) {
        if !self.shape_pane() {
            return;
        }
        let to = self.commit_cursor as isize + delta;
        if to < 0 || to as usize >= self.commits.len() {
            return;
        }
        if !matches!(self.shape, Shape::Reorder(_)) {
            self.clear_shape();
            self.shape = Shape::Reorder(self.commits.iter().map(|c| c.oid).collect());
        }
        self.commits.swap(self.commit_cursor, to as usize);
        self.commit_cursor = to as usize;
        self.diff_scroll = 0;
        self.status = "reordered — p: preview · Enter: apply · Esc: cancel".into();
    }

    /// `d` / `S`: mark the commit under the cursor dropped, or squashed into its
    /// parent. Either replaces whatever shape edit was pending.
    fn mark_shape(&mut self, drop: bool) {
        if !self.shape_pane() {
            return;
        }
        let Some(oid) = self.cursor_commit() else { return };
        self.clear_shape();
        self.shape = if drop { Shape::Drop(oid) } else { Shape::Squash(oid) };
        let verb = if drop { "drop" } else { "squash into its parent" };
        self.status = format!("{verb} {oid:.8} — p: preview · Enter: apply · Esc: cancel");
    }

    /// The plan for the pending shape edit, or None if there isn't one.
    fn shape_plan(&self, repo: &Repository) -> Result<Option<recipe::Shaped>> {
        let plan = match &self.shape {
            Shape::None => return Ok(None),
            Shape::Reorder(_) => recipe::reshape(repo, self.base, self.head, self.commit_order()),
            Shape::Drop(o) => recipe::drop_commit(repo, self.base, self.head, *o),
            Shape::Squash(o) => recipe::squash(repo, self.base, self.head, *o, None),
        };
        Ok(Some(plan.map_err(anyhow::Error::msg)?))
    }

    /// Reflog message for the pending shape edit.
    fn shape_msg(&self) -> String {
        match &self.shape {
            Shape::None => String::new(),
            Shape::Reorder(_) => "transplant: tui reorder".into(),
            Shape::Drop(o) => format!("transplant: drop {o:.8}"),
            Shape::Squash(o) => format!("transplant: squash {o:.8}"),
        }
    }

    /// Enter, with a shape edit pending: first press states the scope, second
    /// applies — the same gate the hunk flow uses.
    fn execute_shape(&mut self, repo: &Repository, plan: recipe::Shaped) {
        if !self.pending_apply {
            self.pending_apply = true;
            self.status = format!(
                "{} — rewrite {} commit(s) on {} · Enter again to apply · Esc: cancel",
                self.shape_summary(),
                plan.order.len(),
                self.short_branch()
            );
            return;
        }
        self.pending_apply = false;
        // sync = false: the TUI never writes the worktree, which is what lets you
        // reshape the stack with work in progress present.
        match ops::shape(repo, plan, &self.shape_msg(), false, &self.opts) {
            Ok(o) => {
                self.status = format!(
                    "{} now at {:.8} (was {:.8}) · undo: git-transplant undo",
                    o.short_branch(),
                    o.new_tip,
                    o.old_tip
                );
                self.applied = true;
            }
            Err(e) => self.status = format!("not applied: {e}"),
        }
    }

    /// Human description of the pending shape edit, for the arm/preview line.
    fn shape_summary(&self) -> String {
        match &self.shape {
            Shape::None => String::new(),
            Shape::Reorder(_) => "reorder".into(),
            Shape::Drop(o) => format!("drop {o:.8}"),
            Shape::Squash(o) => format!("squash {o:.8}"),
        }
    }

    /// Esc — step back out of wherever you are, one layer at a time:
    /// armed apply → shape edit → right pane → move mode → (already home).
    fn go_back(&mut self, was_pending: bool) -> Option<Flow> {
        if was_pending {
            self.status = "apply cancelled".into();
        } else if self.shape != Shape::None {
            self.clear_shape();
            self.status = "shape edit cancelled".into();
        } else if self.focus == Pane::Right {
            self.focus = Pane::Commits;
            self.status = "back to the commit list".into();
        } else if matches!(self.source, Source::Commit(_)) {
            // leaving commit-source needs the repo → let the driver reload
            return Some(Flow::ResetSource);
        } else if self.mode == Mode::Move {
            self.mode = Mode::Hunks;
            self.status = "back to hunks mode".into();
        } else {
            self.status = "already at the top level — q to quit".into();
        }
        None
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
                    // Direction matters: backward is absorbed idempotently,
                    // forward also reverts at the source and can conflict.
                    let note = match self.source {
                        Source::Commit(src) if self.is_newer(oid, src) => " (forward — may conflict)",
                        Source::Commit(_) => " (backward — clean)",
                        Source::Staged => "",
                    };
                    self.status = format!("hunk → {oid:.8}{note}");
                }
            }
            Mode::Move => {
                self.move_target = Some(oid);
                self.status = format!("move destination → {oid:.8}");
            }
        }
    }

    /// `f`: route EVERY selected hunk to the commit under the cursor (= fix).
    fn route_all_to_cursor(&mut self) {
        if self.mode != Mode::Hunks {
            return;
        }
        let Some(oid) = self.cursor_commit() else { return };
        let mut n = 0;
        for f in &mut self.files {
            for (hi, sel) in f.selected.iter().enumerate() {
                if *sel {
                    f.targets[hi] = Some(oid);
                    n += 1;
                }
            }
        }
        self.status = if n == 0 {
            "nothing selected — pick hunks with Space first".into()
        } else {
            format!("{n} selected hunk(s) → {oid:.8} (fix)")
        };
    }

    /// `r`: reset every hunk's target to inference's suggestion (= absorb).
    fn accept_inference(&mut self) {
        // A commit's own hunks have no inferred home; resetting would silently
        // wipe the destinations the user just picked.
        if matches!(self.source, Source::Commit(_)) {
            self.status = "no inference for a commit's hunks — pick destinations with t".into();
            return;
        }
        for f in &mut self.files {
            f.targets = f.inferred.clone();
        }
        self.status = "targets reset to inference (absorb)".into();
    }

    fn preview(&mut self, repo: &Repository) {
        // A pending shape edit outranks the hunk/move selection: it is what the
        // commit pane is currently showing.
        match self.shape_plan(repo) {
            Ok(Some(plan)) => {
                let opts = ops::Opts { dry_run: true, ..self.opts };
                self.status = match ops::shape(repo, plan, &self.shape_msg(), false, &opts) {
                    Ok(o) => format!(
                        "clean, would move {} to {:.8} ({})",
                        self.short_branch(),
                        o.new_tip,
                        self.shape_summary()
                    ),
                    Err(e) => format!("conflict: {e}"),
                };
                return;
            }
            Ok(None) => {}
            Err(e) => {
                self.status = format!("{e}");
                return;
            }
        }
        match self.mode {
            Mode::Hunks => match self.build_recipe(repo) {
                Ok(r) if !r.is_empty() => match engine::replay(repo, self.replay_base(repo), self.head, &r, self.opts.merge(), true) {
                    Ok(p) if p.tip == self.head => self.status = "no change — targets already hold these hunks".into(),
                    Ok(p) => self.status = format!("clean, would move {} to {:.8}", self.short_branch(), p.tip),
                    Err(e) => self.status = format!("conflict: {e}"),
                },
                Ok(_) => self.status = "select hunks (Space) and set targets (t) first".into(),
                Err(e) => self.status = format!("preview error: {e}"),
            },
            Mode::Move => match self.move_plan(repo) {
                Ok(Some((base, tip, rec))) => match engine::replay(repo, base, tip, &rec, self.opts.merge(), false) {
                    Ok(p) => self.status = format!("clean, would move {} to {:.8}", self.short_branch(), p.tip),
                    Err(e) => self.status = format!("conflict: {e}"),
                },
                Ok(None) => self.status = "pick a file and a destination (t) first".into(),
                Err(e) => self.status = format!("{e}"),
            },
        }
    }

    fn execute(&mut self, repo: &Repository) {
        match self.shape_plan(repo) {
            Ok(Some(plan)) => return self.execute_shape(repo, plan),
            Ok(None) => {}
            Err(e) => {
                self.pending_apply = false;
                self.status = format!("{e}");
                return;
            }
        }
        match self.mode {
            Mode::Hunks => self.execute_hunks(repo),
            Mode::Move => self.execute_move(repo),
        }
    }

    fn execute_hunks(&mut self, repo: &Repository) {
        // No clean-worktree guard here. The TUI promotes with sync=false: it only
        // moves the branch ref — it never checks out or rewrites the worktree or
        // index, and the commit-source flow doesn't even read them. Requiring a
        // clean tree blocked the ordinary case of moving hunks between commits
        // while you have unrelated work in progress.
        // First Enter states the scope and arms; second Enter actually rewrites.
        if !self.pending_apply {
            let n = self.rewrite_span();
            if n == 0 {
                self.status = "nothing selected to apply".into();
                return;
            }
            self.pending_apply = true;
            let dropped = self.dropped_by_replay(repo);
            let drop_note = match dropped.len() {
                0 => String::new(),
                n => format!(
                    " · {n} commit(s) become empty and will be DROPPED ({})",
                    dropped.iter().map(|o| format!("{o:.8}")).collect::<Vec<_>>().join(", ")
                ),
            };
            self.status = format!(
                "rewrite {n} commit(s) on {}{drop_note} — Enter again to apply · Esc: cancel",
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
        let base = self.replay_base(repo);
        const MSG: &str = "transplant: tui fold";
        match engine::replay(repo, base, self.head, &recipe, self.opts.merge(), true) {
            Ok(p) if p.tip == self.head => {
                self.status = "no change — targets already hold these hunks".into();
            }
            // sync = false: deselected / no-home hunks stay staged, not wiped.
            Ok(p) => match ops::promote(repo, &self.branch, p.tip, self.head, MSG, false) {
                Ok(()) => {
                    let (moved, warns) = ops::restack(repo, &p.map, &self.branch, MSG, &self.opts);
                    let note = match (moved.len(), warns.len()) {
                        (0, 0) => String::new(),
                        (n, 0) => format!(" · restacked {n} branch(es)"),
                        (0, _) => format!(" · {}", warns.join("; ")),
                        (n, _) => format!(" · restacked {n}, {}", warns.join("; ")),
                    };
                    self.status = format!(
                        "{} now at {:.8} (was {:.8}){note} · undo: git-transplant undo",
                        self.short_branch(),
                        p.tip,
                        self.head
                    );
                    self.applied = true;
                }
                Err(e) => self.status = format!("promote failed: {e}"),
            },
            Err(e) => self.status = format!("conflict, not applied: {e}"),
        }
    }

    fn execute_move(&mut self, repo: &Repository) {
        let (Some(path), Some(target)) = (self.move_files.get(self.move_cursor), self.move_target) else {
            self.status = "pick a file (↑↓) and a destination (Tab to commits, then t)".into();
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
        match ops::mv(repo, path, &target.to_string(), &self.opts) {
            Ok(o) => {
                self.status = format!(
                    "moved {path} → {target:.8}; {} now at {:.8} (was {:.8}) · undo: git-transplant undo",
                    o.short_branch(),
                    o.new_tip,
                    o.old_tip
                );
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
                // Synthetics are parented at the SOURCE's base: HEAD for staged
                // work, the source commit's parent when moving hunks out of it.
                let synth =
                    patch::synthetic_for_hunks(repo, self.source_base, &f.path, &f.old_full, &f.hunks, &mask, f.mode)?;
                recipe.add(t, engine::Edit::ApplyChange(synth));
                // Moving a commit's hunk BACKWARD (to an older commit) needs no
                // removal — replaying the source onto a chain that already has
                // the change absorbs it idempotently. Moving FORWARD does.
                if let Source::Commit(src) = self.source {
                    if self.is_newer(t, src) {
                        recipe.add(src, engine::Edit::RevertChange(synth));
                    }
                }
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

// ── rendering ───────────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &App) {
    // Status spans the FULL width — inside the right column the keymap clipped
    // and lost `p preview · Enter apply · q quit`.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(5)])
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
    // Be honest about what Tab leads to: with nothing staged there are no hunks.
    // The hint must reflect the actual source, not just "staged".
    let hint = match (app.source, app.flat.is_empty()) {
        (Source::Commit(o), _) => format!("Tab: hunks from {o:.8}"),
        (Source::Staged, true) => "read-only · nothing staged to fold".to_string(),
        (Source::Staged, false) => "Tab: pick staged hunks".to_string(),
    };
    let (title, diff) = match app.commits.get(app.commit_cursor) {
        Some(c) => (format!("[DIFF] {:.8} {} ({hint})", c.oid, c.summary), &c.diff),
        None => ("[DIFF]".to_string(), &empty),
    };
    let lines: Vec<Line> = if diff.is_empty() {
        vec![Line::from(Span::styled("(empty commit)", theme::dim()))]
    } else {
        diff.iter()
            .map(|(origin, text)| {
                let (prefix, style) = match origin {
                    '+' => ("+", theme::added()),
                    '-' => ("-", theme::removed()),
                    'F' | 'H' => ("", theme::path()),
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

/// One palette, used everywhere, so the same kind of thing always looks the same:
/// oids amber (like `git log`), paths cyan, destinations magenta, +/- green/red,
/// chrome grey. Only the focused pane is coloured, so the eye lands on it.
mod theme {
    use ratatui::style::{Color, Modifier, Style};

    pub const OID: Color = Color::Yellow;
    pub const PATH: Color = Color::Cyan;
    pub const DEST: Color = Color::Magenta;
    pub const ADD: Color = Color::Green;
    pub const DEL: Color = Color::Red;
    pub const CHROME: Color = Color::DarkGray;

    pub fn oid() -> Style {
        Style::default().fg(OID)
    }
    pub fn path() -> Style {
        Style::default().fg(PATH).add_modifier(Modifier::BOLD)
    }
    pub fn dest() -> Style {
        Style::default().fg(DEST)
    }
    pub fn added() -> Style {
        Style::default().fg(ADD)
    }
    pub fn removed() -> Style {
        Style::default().fg(DEL)
    }
    pub fn dim() -> Style {
        Style::default().fg(CHROME)
    }
    pub fn border(focused: bool) -> Style {
        if focused {
            Style::default().fg(PATH).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(CHROME)
        }
    }
    /// Status colouring by kind, so outcomes read at a glance.
    pub fn status(text: &str, pending: bool) -> Style {
        let low = text.to_ascii_lowercase();
        if pending {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else if low.contains("conflict") || low.contains("error") || low.contains("can't")
            || low.contains("cannot") || low.contains("needs") || low.contains("failed")
        {
            Style::default().fg(DEL).add_modifier(Modifier::BOLD)
        } else if low.contains("now at") || low.contains("clean, would") {
            Style::default().fg(ADD).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    }
}

fn list_block(title: &str, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title.to_string())
        .border_style(theme::border(focused))
}

fn render_commits(f: &mut Frame, app: &App, area: Rect) {
    let target = app.active_target();
    let items: Vec<ListItem> = app
        .commits
        .iter()
        .map(|c| {
            // Marker lives in a LEFT gutter so it survives truncated summaries.
            // A pending shape edit wins the gutter: it is the bigger change.
            let (mark, style) = match app.shape {
                Shape::Drop(o) if o == c.oid => ("✗", theme::removed()),
                Shape::Squash(o) if o == c.oid => ("⇣", theme::added()),
                _ if Some(c.oid) == target => ("◀", theme::dest()),
                _ => (" ", theme::dim()),
            };
            ListItem::new(Line::from(vec![
                Span::styled(mark, style),
                Span::styled(format!("{:.8} ", c.oid), theme::oid()),
                Span::raw(c.summary.clone()),
            ]))
        })
        .collect();
    let title = match app.shape {
        Shape::None if app.base.is_some() => {
            format!("commits · {} shown (--base widens) · ◀ = target", app.commits.len())
        }
        Shape::None => "commits · ◀ = target (t sets)".to_string(),
        _ => format!("commits · {} pending — p/Enter", app.shape_summary()),
    };
    let list = List::new(items)
        .block(list_block(&title, app.focus == Pane::Commits))
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
        // Scale the caps to the pane so the destination isn't clipped at 80 cols.
        let wide = area.width >= 80;
        let (path_cap, head_cap) = if wide { (22, 20) } else { (14, 12) };
        let on = file.selected[hi];
        lines.push(Line::from(vec![
            Span::styled(
                if on { "[x] " } else { "[ ] " },
                if on { theme::added() } else { theme::dim() },
            ),
            Span::styled(format!("{} ", truncate(&file.path, path_cap)), theme::path()),
            Span::styled(truncate(&file.hunks[hi].header, head_cap), theme::dim()),
            Span::styled(
                format!("  → {}", app.target_label(file.targets[hi])),
                theme::dest(),
            ),
        ]));
        // Cap the preview: one long hunk must not fill the pane and hide the rest.
        let body = file.hunks[hi].lines.as_slice();
        for (origin, text) in body.iter().take(HUNK_PREVIEW_LINES) {
            let (prefix, style) = match origin {
                '+' => ('+', theme::added()),
                '-' => ('-', theme::removed()),
                _ => (' ', Style::default()),
            };
            lines.push(Line::from(Span::styled(format!("{prefix}{text}"), style)));
        }
        if body.len() > HUNK_PREVIEW_LINES {
            lines.push(Line::from(Span::styled(
                format!("  … +{} more line(s)", body.len() - HUNK_PREVIEW_LINES),
                theme::dim(),
            )));
        }
        items.push(ListItem::new(lines));
    }
    if items.is_empty() {
        // This pane folds STAGED work — say what to do, not just what's missing.
        let msg = match app.skipped.len() {
            0 => vec![
                Line::from("Nothing staged. This screen does two things:"),
                Line::from(""),
                Line::from(Span::styled(
                    "  Move hunks BETWEEN commits  (no staging needed)",
                    theme::path(),
                )),
                Line::from("    Esc → commit list, press s on a commit to load its hunks,"),
                Line::from("    Space to pick, then go to the destination commit and press t."),
                Line::from(""),
                Line::from(Span::styled("  Fold NEW work into an old commit", theme::path())),
                Line::from("    `git add` your fix, reopen — each hunk appears here, Enter absorbs."),
                Line::from(""),
                Line::from("m: move a whole file · q: quit"),
            ],
            n => vec![
                Line::from(format!("{n} staged file(s) can't be hunk-folded (binary, or a whole-file add/delete).")),
                Line::from(""),
                Line::from("Use `m` (move mode) to re-anchor a file at another commit."),
            ],
        };
        items.push(ListItem::new(msg));
    }
    // Counts live in the TITLE so a cramped status bar can never drop them.
    let total: usize = app.files.iter().map(|f| f.hunks.len()).sum();
    let sel = app.picked();
    let title = match app.source {
        Source::Staged => format!("[STAGED HUNKS] {sel}/{total} selected · Enter: absorb"),
        Source::Commit(o) => format!("[HUNKS FROM {o:.8}] {sel}/{total} picked · t: destination"),
    };
    let list = List::new(items)
        .block(list_block(&title, app.focus == Pane::Right))
        .highlight_symbol("▶ ")
        // Hunk items are multi-line: REVERSED would paint a heavy block. The
        // ▶ gutter plus bold is enough to mark the cursor.
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
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
    let dim = theme::dim();
    // Mode-aware keymap: only keys that actually do something here. Rendered
    // WITHOUT wrap so the lines below can never be pushed out of the box.
    // Two short lines beat one clipped line: nav on top, actions below.
    let (nav, act, shape) = match app.mode {
        // Keep each line <= 80 chars: they are NOT wrapped, so anything longer
        // silently loses the trailing (most important) keys on a narrow terminal.
        Mode::Hunks => (
            "↑↓ nav · ←→/Tab pane · Home/End ends · PgUp/PgDn scroll · Esc back · q quit",
            "Spc sel · t dest · s cmt-hunks · f fix-all · r reset · m move · p prev · ⏎ apply",
            "shape (commits): [ ] move commit · d drop · S squash into parent",
        ),
        Mode::Move => (
            "↑↓ nav · ←→/Tab pane · Home/End ends · Esc back · q quit",
            "t destination · m hunks-mode · p preview · ⏎ move",
            "",
        ),
    };
    let status_style = theme::status(&app.status, app.pending_apply);
    let text = vec![
        Line::from(Span::styled(nav, dim)),
        Line::from(Span::styled(act, dim)),
        Line::from(Span::styled(shape, dim)),
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
                selected: vec![true; n_hunks],
                targets: vec![None; n_hunks],
                inferred: vec![Some(oid(1)); n_hunks],
                mode: 0o100644,
            }]
        } else {
            Vec::new()
        };
        let flat = flatten(&files.iter().map(|f| f.selected.len()).collect::<Vec<_>>());
        let move_files = vec!["a.txt".into(), "src/b.rs".into()];
        App {
            branch: "refs/heads/main".into(),
            head: oid(9),
            opts: ops::Opts::default(),
            base: None,
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
            source: Source::Staged,
            source_base: oid(9),
            shape: Shape::None,
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
            let before = (a.commit_cursor, a.hunk_cursor, a.focus, a.mode, a.files[0].selected.clone(), a.files[0].targets.clone());
            assert_eq!(on_key(&mut a, KeyCode::Char(k)), Flow::Continue);
            let after = (a.commit_cursor, a.hunk_cursor, a.focus, a.mode, a.files[0].selected.clone(), a.files[0].targets.clone());
            assert_eq!(before, after, "'{k}' must be inert — no nav, no mode/selection change");
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

        let mut app = load(&f.repo, None, Default::default()).unwrap();
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
        let after = f.repo.head().unwrap().target().unwrap();
        assert_ne!(after, before, "branch moved");
        // The edit is to line 2, which c1 owns — assert it landed THERE, not just
        // that the branch moved (which passes even for a wrong-commit fold).
        let c1p = f.repo.find_commit(after).unwrap().parent(0).unwrap();
        let blob = c1p.tree().unwrap().get_path(Path::new("f.rs")).unwrap()
            .to_object(&f.repo).unwrap().peel_to_blob().unwrap();
        let c1_txt = String::from_utf8(blob.content().to_vec()).unwrap();
        assert!(c1_txt.contains("L2"), "hunk folded into its OWNER c1, not elsewhere");
        assert!(!c1_txt.contains("extra"), "c1 did not absorb c2's content");
    }

    #[test]
    fn esc_steps_back_one_layer_at_a_time() {
        let mut a = app(3, 2);
        // deepest: move mode + right pane focused
        on_key(&mut a, KeyCode::Char('m'));
        on_key(&mut a, KeyCode::Tab);
        assert_eq!((a.mode, a.focus), (Mode::Move, Pane::Right));

        on_key(&mut a, KeyCode::Esc); // right pane -> commit list
        assert_eq!(a.focus, Pane::Commits);
        assert_eq!(a.mode, Mode::Move, "mode not skipped");

        on_key(&mut a, KeyCode::Esc); // move mode -> hunks mode
        assert_eq!(a.mode, Mode::Hunks);

        on_key(&mut a, KeyCode::Esc); // already home
        assert!(a.status.contains("q to quit"), "tells you how to exit: {}", a.status);
    }

    #[test]
    fn esc_cancels_an_armed_apply_first() {
        let mut a = app(3, 2);
        a.pending_apply = true;
        on_key(&mut a, KeyCode::Esc);
        assert!(!a.pending_apply);
        assert!(a.status.contains("cancelled"), "{}", a.status);
        assert_eq!(a.focus, Pane::Commits, "cancelling doesn't also navigate");
    }

    #[test]
    fn empty_hunk_pane_explains_the_workflow() {
        let f = staged_fixture("ux-empty");
        {
            let head = f.repo.head().unwrap().peel_to_commit().unwrap();
            f.repo.reset(head.as_object(), git2::ResetType::Hard, None).unwrap();
        }
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.flat.is_empty());
        // the commit-diff title must not promise hunks that aren't there
        let browse = render_at(&app, 100, 30);
        assert!(browse.contains("nothing staged"), "diff title is honest: no false 'select hunks'");
        on_key(&mut app, KeyCode::Tab);
        let text = render_at(&app, 100, 30);
        assert!(text.contains("git add"), "empty pane teaches the workflow");
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
        let app = load(&f.repo, None, Default::default()).unwrap(); // default focus = Commits
        let text = render_to_text(&app);
        assert!(text.contains("commits"), "commit list pane rendered");
        assert!(text.contains("[DIFF]"), "commit-diff pane shown while browsing");
        // the selected (newest) commit c2 introduced `extra` — its diff is visible
        assert!(text.contains("extra"), "browsing a commit shows its diff (regression)");
    }

    #[test]
    fn renders_hunk_selector_when_right_focused() {
        let f = staged_fixture("render-hunks");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        on_key(&mut app, KeyCode::Tab); // focus the right pane
        let text = render_to_text(&app);
        assert!(text.contains("[STAGED HUNKS]"), "staged-hunk selector shown when right-focused");
        assert!(text.contains("[x]"), "hunk checkboxes rendered");
    }

    // ── UX regressions (tasks #11–#19) ──

    #[test]
    fn launch_status_advertises_absorb_not_a_second_keymap() {
        let f = staged_fixture("ux-launch");
        let app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.status.contains("absorb"), "sells the primary action: {}", app.status);
        assert!(!app.status.contains("Tab pane"), "must not duplicate the keymap: {}", app.status);
    }

    #[test]
    fn target_marker_survives_long_commit_summaries() {
        let f = staged_fixture("ux-marker");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
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
        let app = load(&f.repo, None, Default::default()).unwrap();
        let ctx = app.context_line();
        assert!(ctx.starts_with("staged · hunk 1/"), "names the source: {ctx}");
        assert!(ctx.contains("f.rs"), "{ctx}");
        // and it's on screen even though focus is the commit list
        assert!(render_to_text(&app).contains("hunk 1/"), "context line rendered while browsing");
    }

    #[test]
    fn selection_counts_survive_a_cramped_terminal() {
        let f = staged_fixture("ux-narrow");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        on_key(&mut app, KeyCode::Tab); // show the hunk pane
        let text = render_at(&app, 80, 24);
        assert!(text.contains("selected"), "counts live in the title, not the overflowing status");
    }

    #[test]
    fn move_mode_warns_up_front_when_tree_is_dirty() {
        let f = staged_fixture("ux-move-dirty"); // fixture has a staged change
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.tree_dirty, "fixture is dirty");
        on_key(&mut app, KeyCode::Char('m'));
        on_key(&mut app, KeyCode::Tab);
        let text = render_at(&app, 100, 30);
        assert!(text.contains("clean tree"), "move mode says so before you press Enter");
    }

    #[test]
    fn keymap_is_mode_aware() {
        let f = staged_fixture("ux-keymap");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
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
        let app = load(&f.repo, None, Default::default()).unwrap();
        let text = render_at(&app, 80, 24);
        assert!(text.contains("⏎ apply"), "the apply key must survive 80 cols");
        assert!(text.contains("q quit"), "the quit key must survive 80 cols");
    }

    #[test]
    fn staged_binary_file_is_reported_not_silently_dropped() {
        // A modified binary file can't be hunk-folded, but silently dropping
        // staged work is never acceptable — it must be surfaced.
        let f = staged_fixture("ux-binary");
        std::fs::write(f.dir.join("blob.bin"), [0x63, 0x61, 0x66, 0xE9, 0x0A]).unwrap();
        {
            let mut idx = f.repo.index().unwrap();
            idx.add_path(Path::new("blob.bin")).unwrap();
            idx.write().unwrap();
        }
        // commit it, then stage a non-UTF-8 MODIFICATION so it is a Modified delta
        {
            let tree = {
                let mut idx = f.repo.index().unwrap();
                f.repo.find_tree(idx.write_tree().unwrap()).unwrap()
            };
            let sig = f.repo.signature().unwrap();
            let head = f.repo.head().unwrap().peel_to_commit().unwrap();
            f.repo.commit(Some("HEAD"), &sig, &sig, "add binary", &tree, &[&head]).unwrap();
        }
        std::fs::write(f.dir.join("blob.bin"), [0x63, 0x61, 0x66, 0xEE, 0x0A]).unwrap();
        let mut idx = f.repo.index().unwrap();
        idx.add_path(Path::new("blob.bin")).unwrap();
        idx.write().unwrap();

        let app = load(&f.repo, None, Default::default()).unwrap();
        assert!(
            app.skipped.iter().any(|p| p == "blob.bin"),
            "non-UTF-8 modification must be recorded, not dropped: {:?}",
            app.skipped
        );
    }

    #[test]
    fn staged_added_file_is_reported_not_hidden() {
        let f = added_file_fixture("ux-added");
        let app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.flat.is_empty(), "an add has no foldable hunks");
        assert!(!app.skipped.is_empty(), "but it IS recorded");
        let text = render_at(&app, 100, 30);
        assert!(text.contains("can't be hunk-folded"), "user is told, not shown 'no staged changes'");
    }

    #[test]
    fn commit_diff_header_is_not_a_run_on_line() {
        let f = staged_fixture("ux-header");
        let app = load(&f.repo, None, Default::default()).unwrap();
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
        // Several tests assert exact hunk counts/indices against this fixture.
        // Pin the shape here so a diff-algorithm or context change fails once,
        // loudly and explanatorily, instead of silently testing something else.
        for (name, base) in [("a.rs", long("a")), ("b.rs", long("b"))] {
            let new = std::fs::read(dir.join(name)).unwrap();
            let n = crate::patch::hunks(base.as_bytes(), &new).unwrap().len();
            assert_eq!(
                n, 2,
                "fixture precondition: {name} must diff into exactly 2 hunks (got {n}) — \
                 libgit2 hunk splitting changed; dependent assertions are now meaningless"
            );
        }
        Fixture { dir, repo }
    }


    // ── move hunks OUT of a commit into another commit (op A) ──

    #[test]
    fn s_opens_the_selected_commits_hunks_as_the_source() {
        let f = multi_hunk_fixture("src-open");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        // cursor 0 = newest commit (c2, which added b.rs)
        let c2 = app.commits[0].oid;
        assert_eq!(on_key(&mut app, KeyCode::Char('s')), Flow::OpenCommit);
        open_commit_source(&mut app, &f.repo);

        assert_eq!(app.source, Source::Commit(c2), "source is now that commit");
        assert!(!app.flat.is_empty(), "its hunks are listed");
        assert!(
            app.files.iter().all(|fe| fe.selected.iter().all(|&s| !s)),
            "nothing pre-selected — moving a commit's hunks is deliberate"
        );
        assert_eq!(app.focus, Pane::Right, "focus jumps to the hunk list");
        assert!(render_at(&app, 100, 30).contains("HUNKS FROM"), "title names the source");
    }

    #[test]
    fn s_again_returns_to_the_staged_view() {
        let f = multi_hunk_fixture("src-toggle");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, &f.repo);
        assert!(matches!(app.source, Source::Commit(_)));
        on_key(&mut app, KeyCode::Tab); // `s` only acts from the commit list
        open_commit_source(&mut app, &f.repo); // same commit again
        assert_eq!(app.source, Source::Staged, "toggles back to staged hunks");
    }

    #[test]
    fn move_one_hunk_of_two_out_of_a_commit_leaving_the_other() {
        // A genuine PARTIAL extraction: c2 edits two distant lines; move only the
        // first back to c1 and prove the second stayed behind.
        let dir = std::env::temp_dir().join(format!("gt-tui-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        {
            let mut c = repo.config().unwrap();
            c.set_str("user.name", "t").unwrap();
            c.set_str("user.email", "t@t").unwrap();
        }
        let commit = |msg: &str, body: &str| {
            std::fs::write(dir.join("f.rs"), body).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(Path::new("f.rs")).unwrap();
            idx.write().unwrap();
            let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
            let sig = repo.signature().unwrap();
            let parents: Vec<_> = repo.head().ok().map(|h| h.peel_to_commit().unwrap()).into_iter().collect();
            let pr: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &pr).unwrap()
        };
        let base: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        commit("c1 base", &base);
        let mut v: Vec<String> = base.split_inclusive('\n').map(String::from).collect();
        v[1] = "FIRST-EDIT\n".into();
        v[19] = "SECOND-EDIT\n".into();
        commit("c2 two edits", &v.concat());

        let mut app = load(&repo, None, Default::default()).unwrap();
        // `s` on the newest commit (c2) → its own two hunks
        open_commit_source(&mut app, &repo);
        assert_eq!(app.flat.len(), 2, "fixture precondition: c2 has two separate hunks");

        // pick ONLY the first hunk, target the older commit
        on_key(&mut app, KeyCode::Char(' '));
        assert_eq!(app.files[0].selected, vec![true, false], "exactly one hunk picked");
        on_key(&mut app, KeyCode::Tab);
        on_key(&mut app, KeyCode::Down);
        on_key(&mut app, KeyCode::Char('t'));
        app.execute(&repo); // arm
        app.execute(&repo); // apply
        assert!(app.applied, "applied: {}", app.status);

        let tip = repo.head().unwrap().target().unwrap();
        let read = |c: &git2::Commit| {
            let b = c.tree().unwrap().get_path(Path::new("f.rs")).unwrap()
                .to_object(&repo).unwrap().peel_to_blob().unwrap();
            String::from_utf8(b.content().to_vec()).unwrap()
        };
        let tipc = repo.find_commit(tip).unwrap();
        let c1p = tipc.parent(0).unwrap();
        let c1_txt = read(&c1p);
        assert!(c1_txt.contains("FIRST-EDIT"), "the picked hunk moved back to c1");
        assert!(!c1_txt.contains("SECOND-EDIT"), "the UNPICKED hunk did not follow it");
        let tip_txt = read(&tipc);
        assert!(tip_txt.contains("FIRST-EDIT") && tip_txt.contains("SECOND-EDIT"), "tip has both");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Four commits, each adding its own file. No staged changes.
    fn stack4(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("gt-tui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        {
            let mut c = repo.config().unwrap();
            c.set_str("user.name", "t").unwrap();
            c.set_str("user.email", "t@t").unwrap();
        }
        for i in 1..=4 {
            let name = format!("f{i}.rs");
            let body: String = (1..=12).map(|n| format!("f{i}line{n}\n")).collect();
            std::fs::write(dir.join(&name), body).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(Path::new(&name)).unwrap();
            idx.write().unwrap();
            let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
            let sig = repo.signature().unwrap();
            let parents: Vec<_> = repo.head().ok().map(|h| h.peel_to_commit().unwrap()).into_iter().collect();
            let pr: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, &format!("c{i}"), &tree, &pr).unwrap();
        }
        Fixture { dir, repo }
    }

    #[test]
    fn moves_a_hunk_to_a_commit_two_back_with_a_dirty_worktree() {
        // Regression: the TUI required a clean worktree, so moving hunks between
        // commits failed with "unstaged changes; commit, stash, or clean first"
        // whenever you had ordinary work in progress.
        let f = stack4("n-minus-2");
        std::fs::write(f.dir.join("f1.rs"), "unrelated work in progress\n").unwrap();

        let mut app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.flat.is_empty(), "nothing staged");
        // s on the newest commit → its hunks
        open_commit_source(&mut app, &f.repo);
        assert!(matches!(app.source, Source::Commit(_)));
        on_key(&mut app, KeyCode::Char(' ')); // pick hunk 0

        // destination = commit n-2 (two older in the newest-first list)
        on_key(&mut app, KeyCode::Tab);
        on_key(&mut app, KeyCode::Down);
        on_key(&mut app, KeyCode::Down);
        let dest = app.commits[app.commit_cursor].oid;
        on_key(&mut app, KeyCode::Char('t'));
        assert_eq!(app.files[0].targets[0], Some(dest));

        let before = f.repo.head().unwrap().target().unwrap();
        app.execute(&f.repo); // arm
        assert!(app.pending_apply, "must arm, not refuse: {}", app.status);
        app.execute(&f.repo); // apply
        assert!(app.applied, "applied despite a dirty worktree: {}", app.status);
        assert_ne!(f.repo.head().unwrap().target().unwrap(), before, "branch moved");

        // the unrelated work in progress is untouched
        let wip = std::fs::read_to_string(f.dir.join("f1.rs")).unwrap();
        assert_eq!(wip, "unrelated work in progress\n", "worktree never written");
    }

    #[test]
    fn empty_pane_teaches_the_commit_to_commit_flow() {
        let f = stack4("empty-teach");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.status.contains("press s"), "status points at `s`: {}", app.status);
        on_key(&mut app, KeyCode::Tab);
        let text = render_at(&app, 100, 30);
        assert!(text.contains("BETWEEN commits"), "empty pane teaches both workflows");
    }

    #[test]
    fn forward_move_reports_the_full_rewrite_span() {
        // A forward move also reverts at the SOURCE, which is older than the
        // target — the span must count from there, not just from the target.
        let mut a = app(4, 1);
        let (newest, oldest) = (a.commits[0].oid, a.commits[3].oid);
        a.source = Source::Commit(oldest); // source is the OLDEST commit
        a.files[0].selected[0] = true;
        a.files[0].targets[0] = Some(newest); // target is newer → forward move
        assert!(a.is_newer(newest, oldest));
        assert_eq!(a.rewrite_span(), 4, "span reaches back to the source commit");
    }

    #[test]
    fn arming_warns_when_the_source_commit_will_be_dropped() {
        let f = multi_hunk_fixture("warn-drop");
        {
            let head = f.repo.head().unwrap().peel_to_commit().unwrap();
            f.repo.reset(head.as_object(), git2::ResetType::Hard, None).unwrap();
        }
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, &f.repo);
        let dest = app.commits[1].oid;
        for fe in app.files.iter_mut() {
            for i in 0..fe.selected.len() {
                fe.selected[i] = true;
                fe.targets[i] = Some(dest);
            }
        }
        app.execute(&f.repo); // arms
        assert!(app.pending_apply);
        assert!(app.status.contains("DROPPED"), "warns about the empty commit: {}", app.status);
    }

    #[test]
    fn esc_leaves_commit_source_back_to_staged() {
        let f = multi_hunk_fixture("esc-src");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, &f.repo);
        assert!(matches!(app.source, Source::Commit(_)));
        on_key(&mut app, KeyCode::Esc); // right pane -> commits
        assert_eq!(app.focus, Pane::Commits);
        assert_eq!(on_key(&mut app, KeyCode::Esc), Flow::ResetSource, "next Esc leaves the source");
        reset_to_staged(&mut app, &f.repo);
        assert_eq!(app.source, Source::Staged);
    }

    #[test]
    fn s_is_ignored_outside_the_commit_list() {
        let f = multi_hunk_fixture("s-gate");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        on_key(&mut app, KeyCode::Tab); // focus the hunk pane
        on_key(&mut app, KeyCode::Char(' ')); // change the selection (staged starts all-on)
        let before = app.files[0].selected.clone();
        open_commit_source(&mut app, &f.repo); // must NOT reload and discard state
        assert_eq!(app.source, Source::Staged, "source unchanged from the wrong pane");
        assert_eq!(app.files[0].selected, before, "selection survives a mis-pressed `s`");
    }

    #[test]
    fn reset_key_does_not_wipe_destinations_in_commit_source() {
        let f = multi_hunk_fixture("r-guard");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, &f.repo);
        let dest = app.commits[1].oid;
        app.files[0].targets[0] = Some(dest);
        on_key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.files[0].targets[0], Some(dest), "r must not clear a picked destination");
    }

    // ── hunk browser / selector ──

    #[test]
    fn hunk_browser_lists_every_hunk_across_files_with_owner_targets() {
        let f = multi_hunk_fixture("hb-list");
        let app = load(&f.repo, None, Default::default()).unwrap();
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
        let mut app = load(&f.repo, None, Default::default()).unwrap();
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
        let mut app = load(&f.repo, None, Default::default()).unwrap();
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
        let mut app = load(&f.repo, None, Default::default()).unwrap();
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
        let mut app = load(&f.repo, None, Default::default()).unwrap();
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
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        // inflate one hunk's rendered body well past the cap
        app.files[0].hunks[0].lines = (0..40).map(|i| (' ', format!("ctx{i}"))).collect();
        on_key(&mut app, KeyCode::Tab);
        let text = render_at(&app, 100, 30);
        assert!(text.contains("more line(s)"), "long hunk is truncated with a marker");
        assert!(text.contains("a.rs"), "and the list still shows other rows");
    }










    /// A merge deeper in history must not block work on the linear stack above
    /// it: the window stops at the merge AND the replay is bounded to the edit.
    #[test]
    fn applies_with_a_merge_deeper_in_history() {
        let dir = std::env::temp_dir().join(format!("gt-tui-mergeapply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        {
            let mut c = repo.config().unwrap();
            c.set_str("user.name", "t").unwrap();
            c.set_str("user.email", "t@t").unwrap();
        }
        let commit = |msg: &str, body: &str| {
            std::fs::write(dir.join("f.rs"), body).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(Path::new("f.rs")).unwrap();
            idx.write().unwrap();
            let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
            let sig = repo.signature().unwrap();
            let parents: Vec<_> = repo.head().ok().map(|h| h.peel_to_commit().unwrap()).into_iter().collect();
            let pr: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &pr).unwrap()
        };
        let base_body: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        let base = commit("base", &base_body);
        let side = commit("side", &format!("{base_body}side\n"));
        let sig = repo.signature().unwrap();
        let basec = repo.find_commit(base).unwrap();
        let sib = repo.commit(None, &sig, &sig, "sib", &basec.tree().unwrap(), &[&basec]).unwrap();
        let sidec = repo.find_commit(side).unwrap();
        let sibc = repo.find_commit(sib).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "merge", &sidec.tree().unwrap(), &[&sidec, &sibc])
            .unwrap();
        let top_body = format!("{base_body}side\ntopline\n");
        commit("top", &top_body);
        let staged = top_body.replace("topline", "TOPLINE-EDITED");
        std::fs::write(dir.join("f.rs"), &staged).unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("f.rs")).unwrap();
        idx.write().unwrap();

        let before = repo.head().unwrap().target().unwrap();
        let mut app = load(&repo, None, Default::default()).unwrap();
        assert_eq!(app.commits.len(), 1, "window stops at the merge");
        assert!(!app.flat.is_empty(), "the staged hunk loaded");
        app.execute(&repo); // arm
        app.execute(&repo); // apply
        assert!(app.applied, "apply must not trip over the merge below: {}", app.status);
        assert_ne!(repo.head().unwrap().target().unwrap(), before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_move_file_list_in_move_mode() {
        let f = staged_fixture("render-move");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        on_key(&mut app, KeyCode::Char('m')); // move mode
        on_key(&mut app, KeyCode::Tab); // focus right pane (file list)
        let text = render_to_text(&app);
        assert!(text.contains("[MOVE]"), "move file list shown in move mode");
        assert!(text.contains("f.rs"), "tracked file listed");
    }

    // ── stack shape (M4) ──

    #[test]
    fn bracket_keys_reorder_the_commit_list_and_esc_puts_it_back() {
        let mut a = app(3, 1);
        let before: Vec<Oid> = a.commits.iter().map(|c| c.oid).collect();
        on_key(&mut a, KeyCode::Char(']'));
        assert_eq!(a.commit_cursor, 1, "the cursor follows the commit it moved");
        assert_eq!((a.commits[0].oid, a.commits[1].oid), (before[1], before[0]));
        assert!(matches!(a.shape, Shape::Reorder(_)));
        on_key(&mut a, KeyCode::Esc);
        assert_eq!(a.commits.iter().map(|c| c.oid).collect::<Vec<_>>(), before);
        assert_eq!(a.shape, Shape::None);
    }

    #[test]
    fn reorder_stops_at_the_ends_of_the_stack() {
        let mut a = app(3, 1);
        on_key(&mut a, KeyCode::Char('[')); // cursor is already on the newest
        assert_eq!(a.commit_cursor, 0);
        assert_eq!(a.shape, Shape::None, "a no-op move arms nothing");
    }

    #[test]
    fn d_and_s_mark_the_commit_under_the_cursor() {
        let mut a = app(3, 1);
        on_key(&mut a, KeyCode::Down);
        let oid = a.commits[1].oid;
        on_key(&mut a, KeyCode::Char('d'));
        assert_eq!(a.shape, Shape::Drop(oid));
        on_key(&mut a, KeyCode::Char('S'));
        assert_eq!(a.shape, Shape::Squash(oid), "a second mark replaces the first");
        on_key(&mut a, KeyCode::Esc);
        assert_eq!(a.shape, Shape::None);
    }

    #[test]
    fn shape_keys_are_inert_outside_the_commit_pane() {
        let mut a = app(3, 2);
        on_key(&mut a, KeyCode::Tab); // focus the hunk pane
        let order: Vec<Oid> = a.commits.iter().map(|c| c.oid).collect();
        for k in ['[', ']', 'd', 'S'] {
            on_key(&mut a, KeyCode::Char(k));
            assert_eq!(a.shape, Shape::None, "'{k}' must not reshape from the hunk pane");
        }
        assert_eq!(a.commits.iter().map(|c| c.oid).collect::<Vec<_>>(), order);
    }

    #[test]
    fn dropping_a_commit_from_the_tui_rewrites_the_branch() {
        let f = staged_fixture("shape-drop");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        let tip = app.commits[0].oid; // newest-first
        on_key(&mut app, KeyCode::Char('d'));
        assert_eq!(app.shape, Shape::Drop(tip));

        app.preview(&f.repo);
        assert!(app.status.starts_with("clean, would move"), "{}", app.status);
        let before = f.repo.head().unwrap().target().unwrap();
        assert_eq!(before, tip, "preview moved nothing");

        app.execute(&f.repo); // arm
        assert!(app.pending_apply, "{}", app.status);
        assert_eq!(f.repo.head().unwrap().target().unwrap(), before, "arming moves nothing");
        app.execute(&f.repo); // apply
        assert!(app.applied, "{}", app.status);
        let head = f.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.summary(), Some("c1"), "the dropped commit is gone");
    }

    #[test]
    fn a_pending_shape_edit_shows_in_the_commit_pane() {
        let f = staged_fixture("shape-render");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        on_key(&mut app, KeyCode::Char('d'));
        let text = render_to_text(&app);
        assert!(text.contains("pending"), "the title names the pending edit: {text}");
        assert!(text.contains('✗'), "and the row is marked");
    }

    /// `n` commits, each adding its own file, so every order commutes.
    fn tall_fixture(tag: &str, n: usize) -> Fixture {
        let dir = std::env::temp_dir().join(format!("gt-tui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        {
            let mut c = repo.config().unwrap();
            c.set_str("user.name", "t").unwrap();
            c.set_str("user.email", "t@t").unwrap();
        }
        for i in 0..n {
            let name = format!("f{i}.txt");
            std::fs::write(dir.join(&name), format!("{i}\n")).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(Path::new(&name)).unwrap();
            idx.write().unwrap();
            let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
            let sig = repo.signature().unwrap();
            let parents: Vec<_> =
                repo.head().ok().map(|h| h.peel_to_commit().unwrap()).into_iter().collect();
            let pr: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, &format!("c{i}"), &tree, &pr).unwrap();
        }
        Fixture { dir, repo }
    }

    #[test]
    fn the_stack_is_bounded_by_default_and_widened_by_base() {
        let f = tall_fixture("cap", DEFAULT_STACK + 5);

        let app = load(&f.repo, None, Default::default()).unwrap();
        assert_eq!(app.commits.len(), DEFAULT_STACK, "the default cap applies");
        assert!(app.base.is_some(), "and it is recorded as the replay bound");
        assert!(render_to_text(&app).contains("--base"), "the pane says how to widen it");

        // --base is the override, in both directions: wider than the cap...
        let head = git::resolve(&f.repo, "HEAD").unwrap();
        let root = git::linear_window(&f.repo, head).unwrap()[0].id();
        let wide = load(&f.repo, Some(root), Default::default()).unwrap();
        assert_eq!(wide.commits.len(), DEFAULT_STACK + 4, "everything but the base itself");
        // ...and narrower.
        let near = f.repo.head().unwrap().peel_to_commit().unwrap().parent(0).unwrap().id();
        let narrow = load(&f.repo, Some(near), Default::default()).unwrap();
        assert_eq!(narrow.commits.len(), 1);
        assert_eq!(narrow.base, Some(near));
    }

    /// The bound is not just a display filter: a shape edit made in a bounded
    /// view must plan against the list it showed. Planning against the full
    /// history instead would read the visible order as "delete everything older".
    #[test]
    fn a_reorder_in_a_bounded_view_leaves_the_history_below_it_alone() {
        let f = tall_fixture("cap-reorder", DEFAULT_STACK + 5);
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        let (newest, second) = (app.commits[0].oid, app.commits[1].oid);

        on_key(&mut app, KeyCode::Char(']')); // swap the two newest
        assert_eq!(app.commits[1].oid, newest);
        app.execute(&f.repo); // arm
        app.execute(&f.repo); // apply
        assert!(app.applied, "{}", app.status);

        let head = f.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.summary(), f.repo.find_commit(second).unwrap().summary());
        let mut n = 1;
        let mut c = head;
        while let Ok(p) = c.parent(0) {
            n += 1;
            c = p;
        }
        assert_eq!(n, DEFAULT_STACK + 5, "not one commit below the bound was lost");
    }

    /// The conflict rules are the CLI's flags, but they reach the TUI through the
    /// same `ops::Opts` — including the shape verbs, which is where they matter
    /// most (a drop in the commit pane is the classic real conflict).
    #[test]
    fn a_conflict_rule_reaches_the_tui() {
        let f = staged_fixture("favor");
        // Re-commit the same line twice more, so the middle commit cannot be
        // dropped without a conflict.
        let write = |text: &str, msg: &str| {
            std::fs::write(f.dir.join("f.rs"), text).unwrap();
            let mut idx = f.repo.index().unwrap();
            idx.add_path(Path::new("f.rs")).unwrap();
            idx.write().unwrap();
            let tree = f.repo.find_tree(idx.write_tree().unwrap()).unwrap();
            let sig = f.repo.signature().unwrap();
            let head = f.repo.head().unwrap().peel_to_commit().unwrap();
            f.repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&head]).unwrap();
        };
        write("one\n", "c3");
        write("two\n", "c4");
        write("three\n", "c5");

        // Without a rule the preview reports the conflict...
        let mut plain = load(&f.repo, None, Default::default()).unwrap();
        plain.commit_cursor = 1; // c4, the middle of the three
        on_key(&mut plain, KeyCode::Char('d'));
        plain.preview(&f.repo);
        assert!(plain.status.starts_with("conflict"), "{}", plain.status);

        // ...with one, the same edit goes through.
        let opts = ops::Opts { favor: Some(git2::FileFavor::Theirs), ..Default::default() };
        let mut app = load(&f.repo, None, opts).unwrap();
        app.commit_cursor = 1;
        on_key(&mut app, KeyCode::Char('d'));
        app.preview(&f.repo);
        assert!(app.status.starts_with("clean, would move"), "{}", app.status);
        app.execute(&f.repo); // arm
        app.execute(&f.repo); // apply
        assert!(app.applied, "{}", app.status);
        let head = f.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.summary(), Some("c5"));
        assert_eq!(head.parent(0).unwrap().summary(), Some("c3"), "c4 was dropped");
    }
}
