//! Interactive TUI over the engine. One screen, **object–verb**: the focused
//! pane IS the object selector, and there is exactly ONE state axis — where the
//! right pane's rows come from ([`Source`]):
//!
//! - `Staged` (default): the staged diff, one selectable hunk at a time, each
//!   with an inference-prefilled target commit. This IS fix / absorb:
//!     * absorb  = every hunk selected, targets from inference → Enter
//!     * fix     = route all selected hunks to one commit (`f`) → Enter
//!     * by hand = set per-hunk targets with `t`
//! - `Commit(oid)` (`e`): that commit's own hunks — move work OUT of it, into
//!   another commit or into a NEW one (the phantom `+ new commit` row = split).
//! - `Files` (`m`): the tracked-file list — re-anchor a whole file (`move-file`).
//!
//! **Shape edits** (commit pane): `[` / `]` move the selected commit, `d` marks
//! it dropped, `s` squashes it into its parent, `r` rewords it. All go through
//! the same `p` preview and two-step Enter as everything else.
//!
//! Bindings are deliberately arrow-key based — no vim `h/j/k/l`, and **no shift
//! keys at all**; actions use distinct mnemonic letters borrowed from
//! `git rebase -i` where they exist (`e`dit, `s`quash, `d`rop, `r`eword) plus
//! `t`arget, `f`ix-all, `a`bsorb-inference, `m`ove-file, `p`review, `u`ndo.
//!
//! Only ONE line of keymap is permanently on screen; `?` opens a popup with the
//! rest, scoped to the FOCUSED PANE. That is structural, not cosmetic: a flat
//! list of every verb across eleven operations neither fits 80 columns nor stays
//! true, since most keys no-op on any given screen.
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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
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
    /// Inference's original suggestion, for the `a` (accept inference) key.
    inferred: Vec<Option<Oid>>,
    /// Filemode on the CHANGED side (exec bit / symlink must survive the move).
    mode: i32,
}

struct CommitRow {
    oid: Oid,
    summary: String,
    /// The commit's own diff (parent→self) as `(origin, text)` lines, shown when
    /// browsing the commit list so you can see what each commit contains.
    ///
    /// Filled on demand by [`ensure_diff`], never at load. Only the cursor row is
    /// ever rendered, so computing all fifty up front meant fifty full tree diffs
    /// before the first frame — and again after every apply, every Esc out of a
    /// source, and every undo, since `reload` re-runs `load`.
    diff: Option<Vec<(char, String)>>,
    /// Local branches pointing at this commit. A rewrite carries them along
    /// (`ops::restack`), which was invisible until the status line said so
    /// afterwards — on a stacked-PR branch that is the whole point of the run.
    refs: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pane {
    Commits,
    Right,
}

/// Where the right pane's rows come from. The ONE state axis: there is no
/// second `Mode`, because a mode that hijacks the right pane is just a source.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    /// The staged change (HEAD → index) — fold new work into old commits.
    Staged,
    /// The UNSTAGED change (index → worktree) — fold work you never staged.
    /// `git add -p` first was the only way in, which is a whole extra tool for a
    /// step this screen already does better, hunk by hunk.
    Unstaged,
    /// An existing commit's own diff — move hunks OUT of it into another commit.
    Commit(Oid),
    /// The tracked-file list — re-anchor a whole file (`move-file`).
    Files,
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

/// An inline text prompt. It replaces the STATUS line and nothing else — the
/// keymap and context lines above stay exactly where they were, so you can still
/// see what you are naming. Deliberately not a modal popup, and deliberately not
/// `$EDITOR`: `reword -m` refused to spawn one for the CLI (a temp file, a child
/// process and an "aborted, the message was empty" path, for something you can
/// type inline), and contradicting that here would be incoherent.
struct Prompt {
    /// What is being asked, e.g. `message`.
    label: &'static str,
    /// Text typed so far.
    // ponytail: append + backspace only. No cursor movement, no history, no
    // paste; add a cursor index if anyone ever types a paragraph in here.
    text: String,
    what: Ask,
}

/// What a submitted [`Prompt`] does.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ask {
    /// Reword this commit. Only the SUMMARY is edited; the body is re-appended.
    Reword(Oid),
    /// Message for the new commit a split creates, ahead of this one.
    Split(Oid),
    /// Message for a brand-new commit at the tip, built from the picked hunks.
    NewCommit,
    /// Narrow the tracked-file list. Unlike the others this applies as you type
    /// rather than on submit — a filter you cannot see the effect of is useless.
    Filter,
}

/// Target oid standing for the phantom `+ new commit` row: a destination that
/// does not exist yet. `Oid::zero()` is never a real commit, so it can share
/// `targets: Vec<Option<Oid>>` with the real ones and every `t` / `f` / label
/// path works unchanged.
fn phantom() -> Oid {
    Oid::zero()
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
    /// `w` — load the unstaged worktree changes as the hunk source.
    OpenUnstaged,
    /// Return to the staged-hunk source (needs the repo to reload).
    ResetSource,
    /// `u` — walk the last transplant back. No two-step gate: it moves the ref
    /// and only the ref, never the worktree, and pressing it again redoes it.
    Undo,
    /// Enter was pressed in the inline prompt — run what it was asking for.
    Submit,
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
    focus: Pane,
    commit_cursor: usize,
    hunk_cursor: usize,
    move_cursor: usize,
    move_target: Option<Oid>,
    status: String,
    applied: bool,
    /// Enter was pressed once and reported scope; a second Enter applies.
    pending_apply: bool,
    /// `q` was pressed with unapplied routing; a second `q` quits.
    pending_quit: bool,
    /// Substring narrowing the tracked-file list. Empty means everything.
    filter: String,
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
    /// The inline prompt, while one is open. Every key goes to it.
    input: Option<Prompt>,
    /// The commit cursor is parked on the phantom `+ new commit` row.
    ///
    /// The phantom is deliberately **not** a member of `commits`, and never will
    /// be: `commit_cursor` indexes that vector everywhere, three helpers map an
    /// Oid back to a stack position (the rewrite span, the forward/backward move
    /// direction, and the replay base), and `commits.swap()` is what drives
    /// reorder. Inserting a row would silently shift all of them and corrupt
    /// exactly the three things that decide what gets rewritten. So the phantom
    /// is a render-and-cursor concept, held by this one flag.
    phantom_cursor: bool,
    /// The `?` help overlay is up. Any key takes it down again.
    help: bool,
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
    let decorations = branch_decorations(repo);
    let commits: Vec<CommitRow> = stack
        .iter()
        .rev()
        .map(|c| CommitRow {
            oid: c.id(),
            summary: c.summary().unwrap_or("").to_string(),
            diff: None,
            refs: decorations.iter().filter(|(o, _)| *o == c.id()).map(|(_, n)| n.clone()).collect(),
        })
        .collect();

    let head_tree = repo.find_commit(head)?.tree()?;
    let index_tree = repo.find_tree(repo.index()?.write_tree()?)?;
    let diff = repo.diff_tree_to_tree(Some(&head_tree), Some(&index_tree), None)?;

    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for delta in diff.deltas() {
        // An ADD is one whole-file hunk against an empty original — `git::blob_at`
        // already returns empty for an absent path, so it needs no special case
        // beyond not refusing it here. It is also the only way to say "I created
        // this file and it belongs in commit 3": `move-file` cannot, because the
        // file is not in HEAD's tree to be re-anchored. Deletes stay refused —
        // `apply_selected` would produce an empty blob, not a removal.
        if !matches!(delta.status(), git2::Delta::Modified | git2::Delta::Added) {
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
        if let Some(e) = file_entry(repo, path, old, new, file_mode(&delta), &window, &mut skipped) {
            files.push(e);
        }
    }

    let flat = flatten(&files.iter().map(|f| f.hunks.len()).collect::<Vec<_>>());
    let move_files = tracked_files(&head_tree);
    let tree_dirty = ops::require_fully_clean(repo).is_err();

    // Lead with the value proposition, not a duplicate of the keymap below.
    let status = if flat.is_empty() {
        // Staging is only ONE of the two workflows — say so, or this reads as
        // "you must stage something to use this tool".
        match skipped.len() {
            0 => "w: fold your UNSTAGED work · e on a commit: move its hunks out · m: move a file".into(),
            n => format!("{n} staged file(s) can't be hunk-folded (binary or whole-file) — press e on a commit, or m to move a file"),
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
        focus: Pane::Commits, // arrows move the commit list immediately
        commit_cursor: 0,
        hunk_cursor: 0,
        move_cursor: 0,
        move_target: None,
        status,
        applied: false,
        pending_apply: false,
        pending_quit: false,
        filter: String::new(),
        tree_dirty,
        skipped,
        diff_scroll: 0,
        source: Source::Staged,
        source_base: head,
        shape: Shape::None,
        input: None,
        phantom_cursor: false,
        help: false,
    })
}

/// Re-read the repo and keep the user's place in the commit list, then say
/// `status`. This is how the TUI survives its own writes: an apply used to quit
/// the program, so the screen could never show the stack it had just produced.
fn reload(app: &mut App, repo: &Repository, status: String) {
    match load(repo, app.base, app.opts) {
        Ok(fresh) => {
            // A drop/squash shortens the list, so the old index may be past the end.
            let cc = app.commit_cursor.min(fresh.commits.len().saturating_sub(1));
            // Keep your PLACE, not just the commit cursor. Reload runs after every
            // apply, every Esc out of a source and every undo, and it used to
            // dump you back on the commit pane at hunk 0 whatever you were doing.
            // `source` still resets on purpose: the hunks it named are gone.
            let hc = app.hunk_cursor.min(fresh.flat.len().saturating_sub(1));
            let focus = app.focus;
            *app = fresh;
            app.commit_cursor = cc;
            app.hunk_cursor = hc;
            app.focus = focus;
            app.status = status;
        }
        Err(e) => app.status = format!("{e}"),
    }
}

/// Return to the staged-hunk source, keeping the user's place in the commit list.
fn reset_to_staged(app: &mut App, repo: &Repository) {
    reload(app, repo, "back to staged hunks".into());
}

/// `u` — undo the last transplant and show the stack it restored. Deliberately
/// *not* behind the two-step Enter gate: it moves the branch ref and nothing
/// else (`sync = false`), so it cannot touch the worktree, and because the undo
/// writes its own reflog entry, pressing `u` again is the redo.
fn undo(app: &mut App, repo: &Repository) {
    match ops::undo(repo, false) {
        Ok(o) => {
            let msg = format!(
                "undone: {} now at {:.8} (was {:.8}) · u again redoes it",
                o.short_branch(),
                o.new_tip,
                o.old_tip
            );
            reload(app, repo, msg);
        }
        Err(e) => app.status = format!("{e}"),
    }
}

/// `w` — load the UNSTAGED worktree changes (index → worktree) as the hunk
/// source, so folding WIP no longer means running `git add -p` first.
///
/// This needs **no engine change**: everything below [`patch::synthetic_for_hunks`]
/// takes text and produces dangling objects, and `ops::promote(sync=false)` moves
/// only the ref. What it does need is the right merge base. `Edit::ApplyChange`
/// merges against the synthetic's *parent* tree (engine.rs:207-212), so the
/// synthetics are parented at a dangling commit holding the **index tree**. A path
/// that is both staged and unstaged then contributes only its unstaged hunks: the
/// staged part sits in `base` and `theirs` alike and cancels out.
fn open_unstaged_source(app: &mut App, repo: &Repository) {
    if app.source == Source::Unstaged {
        reset_to_staged(app, repo);
        return;
    }
    let Some(root) = repo.workdir().map(|p| p.to_path_buf()) else {
        app.status = "bare repo — there is no worktree to read".into();
        return;
    };
    let index_tree = match repo.index().and_then(|mut i| i.write_tree()).and_then(|o| repo.find_tree(o)) {
        Ok(t) => t,
        Err(e) => {
            app.status = format!("can't read the index: {e}");
            return;
        }
    };
    // Untracked files count as unstaged work — "I created this file and it
    // belongs in commit 3" is the same request as any other hunk, and it is one
    // `move-file` cannot serve. Ignored files stay out: `.gitignore` is already
    // the user saying they are not part of this.
    let mut o = git2::DiffOptions::new();
    o.include_untracked(true).recurse_untracked_dirs(true).include_ignored(false);
    let Ok(diff) = repo.diff_tree_to_workdir(Some(&index_tree), Some(&mut o)) else {
        app.status = "can't diff the worktree".into();
        return;
    };
    let window: Vec<Oid> = app.commits.iter().map(|c| c.oid).collect();
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for delta in diff.deltas() {
        if !matches!(delta.status(), git2::Delta::Modified | git2::Delta::Untracked | git2::Delta::Added) {
            if let Some(p) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                skipped.push(p.to_string_lossy().into_owned());
            }
            continue;
        }
        let Some(path) = delta.new_file().path().map(|p| p.to_string_lossy().into_owned()) else {
            continue;
        };
        let old = git::blob_at(repo, &index_tree, Path::new(&path));
        let Ok(new) = std::fs::read(root.join(&path)) else { continue };
        if let Some(e) = file_entry(repo, path, old, new, file_mode(&delta), &window, &mut skipped) {
            files.push(e);
        }
    }
    if files.is_empty() {
        app.status = match skipped.len() {
            0 => "nothing unstaged to fold".into(),
            n => format!("{n} unstaged file(s) can't be hunk-folded (binary or whole-file add/delete)"),
        };
        return;
    }
    // The index tree as a commit, so it can be a synthetic's parent. Dangling and
    // unreferenced, like everything else this tool builds before it commits to it.
    let sig = git::ident(repo);
    let Ok(head_commit) = repo.find_commit(app.head) else { return };
    let Ok(base) = repo.commit(None, &sig, &sig, "transplant-index", &index_tree, &[&head_commit]) else {
        app.status = "can't snapshot the index".into();
        return;
    };
    app.flat = flatten(&files.iter().map(|f| f.hunks.len()).collect::<Vec<_>>());
    app.files = files;
    app.source = Source::Unstaged;
    app.source_base = base;
    app.hunk_cursor = 0;
    app.focus = Pane::Right;
    app.skipped = skipped;
    app.status = format!(
        "{} unstaged hunk(s), targets from blame — ⏎ absorbs · Space unpicks · w: back",
        app.flat.len()
    );
}

/// Advance the index to the rewritten tip for the paths we just folded, and only
/// those. The worktree is deliberately untouched — that is the TUI's whole story —
/// but the index MUST move: `ops.rs`'s "the rewritten tip's tree is that same
/// index tree" is what makes leaving the worktree alone consistent, and folding an
/// unstaged hunk breaks it. Without this, `git status` reports a phantom *staged
/// reversal* of the change we just wrote into history.
fn refresh_index(repo: &Repository, tip: Oid, paths: &[String]) -> Result<()> {
    let tree = repo.find_commit(tip)?.tree()?;
    let mut idx = repo.index()?;
    for p in paths {
        match tree.get_path(Path::new(p)) {
            Ok(e) => idx.add(&git2::IndexEntry {
                // Zeroed stat fields mark the entry racily-clean, so git re-hashes
                // the file instead of trusting a timestamp we did not observe.
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: e.filemode() as u32,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: e.id(),
                flags: 0,
                flags_extended: 0,
                path: p.clone().into_bytes(),
            })?,
            Err(_) => idx.remove_path(Path::new(p))?,
        }
    }
    idx.write()?;
    Ok(())
}

/// Load the commit under the cursor as the hunk source: its own diff becomes the
/// selectable hunk list, so you can move hunks OUT of it into another commit.
/// Pressing `e` again on the same commit returns to the staged view.
fn open_commit_source(app: &mut App, repo: &Repository) {
    // Only from the commit list — pressing `e` while the hunk pane is focused
    // used to silently discard every pick.
    if app.focus != Pane::Commits || app.on_phantom() {
        app.status = "press e on a commit in the list to use that commit's hunks".into();
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
    app.phantom_cursor = false;
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

/// A commit message's body — everything after the summary's blank line, or ""
/// if it has none.
fn body_of(msg: &str) -> &str {
    msg.split_once("\n\n").map(|(_, b)| b.trim_end()).unwrap_or("")
}

/// Enter in the inline prompt: run what it was asking for, then reload.
fn submit_prompt(app: &mut App, repo: &Repository) {
    let Some(p) = app.input.take() else { return };
    let text = p.text.trim().to_string();
    if text.is_empty() {
        app.status = "empty message — nothing done".into();
        return;
    }
    match p.what {
        Ask::Reword(oid) => {
            // Re-append the body: the prompt edits the SUMMARY, and a reword
            // that ate the paragraphs below it would be data loss.
            let full = repo.find_commit(oid).ok().map(|c| c.message().unwrap_or("").to_string());
            let msg = match body_of(full.as_deref().unwrap_or("")) {
                "" => text,
                body => format!("{text}\n\n{body}"),
            };
            match ops::reword(repo, &oid.to_string(), &msg, &app.opts) {
                Ok(o) => {
                    let s = format!(
                        "reworded {oid:.8}; {} now at {:.8} (was {:.8}) · undo: u",
                        o.short_branch(),
                        o.new_tip,
                        o.old_tip
                    );
                    reload(app, repo, s);
                }
                Err(e) => app.status = format!("not applied: {e}"),
            }
        }
        Ask::Split(src) => {
            let msg = format!("transplant: tui split {src:.8}");
            // sync = false, as everywhere in the TUI: it never writes the worktree.
            match app.split_plan(repo, &text).and_then(|p| {
                let plan = p.ok_or_else(|| anyhow::anyhow!("nothing routed to the new commit"))?;
                Ok(ops::shape(repo, plan, &msg, false, &app.opts)?)
            }) {
                Ok(o) => {
                    let s = format!(
                        "split {src:.8}; {} now at {:.8} (was {:.8}) · undo: u",
                        o.short_branch(),
                        o.new_tip,
                        o.old_tip
                    );
                    reload(app, repo, s);
                }
                Err(e) => app.status = format!("not applied: {e}"),
            }
        }
        // Already applied on every keystroke; Enter just puts the prompt away.
        Ask::Filter => {
            app.status = format!("{} file(s) match \"{}\" — / to change it", app.visible_files().len(), text)
        }
        Ask::NewCommit => {
            // Nothing below HEAD moves, so there is no replay to run — just the
            // commit and the same compare-and-swap promote everything else uses.
            let unstaged = app.source == Source::Unstaged;
            let paths: Vec<String> = app
                .files
                .iter()
                .filter(|f| f.selected.iter().any(|&s| s))
                .map(|f| f.path.clone())
                .collect();
            match app.tip_commit(repo, &text).and_then(|c| {
                let tip = c.ok_or_else(|| anyhow::anyhow!("nothing routed to the new commit"))?;
                ops::promote(repo, &app.branch, tip, app.head, "transplant: tui commit", false)
                    .map_err(anyhow::Error::msg)?;
                Ok(tip)
            }) {
                Ok(tip) => {
                    // Same index invariant as folding unstaged work: the change is
                    // in history now, so the index has to catch up or git reports
                    // a staged reversal of it. See `refresh_index`.
                    let note = match unstaged.then(|| refresh_index(repo, tip, &paths)) {
                        Some(Err(e)) => format!(" · index not synced ({e}) — `git reset` fixes it"),
                        _ => String::new(),
                    };
                    let s = format!(
                        "committed {:.8} on {}{note} · undo: u",
                        tip,
                        app.short_branch()
                    );
                    reload(app, repo, s);
                }
                Err(e) => app.status = format!("not applied: {e}"),
            }
        }
    }
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, repo: &Repository) -> Result<()> {
    loop {
        ensure_diff(app, repo); // the cursor moved; read that commit's diff now
        terminal.draw(|f| ui(f, app))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match on_key(app, key.code) {
            Flow::Quit => break,
            Flow::OpenCommit => open_commit_source(app, repo),
            Flow::OpenUnstaged => open_unstaged_source(app, repo),
            Flow::ResetSource => reset_to_staged(app, repo),
            Flow::Preview => app.preview(repo),
            Flow::Undo => undo(app, repo),
            Flow::Submit => submit_prompt(app, repo),
            Flow::Apply => {
                app.execute(repo);
                if app.applied {
                    // Reload in place rather than quit: the whole point of a TUI
                    // is chaining edits, and `u` needs a screen to undo on.
                    let done = std::mem::take(&mut app.status);
                    reload(app, repo, done);
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
    // While the prompt is open it swallows EVERY key: `q` must not quit and
    // Enter must not apply while you are typing a commit message. Returning from
    // here also keeps the "any key cancels pending_apply" rule below from firing
    // on the letters you type.
    if let Some(p) = &mut app.input {
        let filtering = p.what == Ask::Filter;
        match key {
            KeyCode::Char(c) => p.text.push(c),
            KeyCode::Backspace => {
                p.text.pop();
            }
            KeyCode::Enter => return Flow::Submit,
            KeyCode::Esc => {
                app.input = None;
                // Esc means cancel, so a cancelled filter must not stay applied.
                app.filter.clear();
                app.status = "cancelled".into();
            }
            _ => {}
        }
        // A filter is the one prompt that applies as you TYPE: submitting a
        // narrowing you never saw the effect of is not narrowing, it is guessing.
        if filtering {
            app.filter = app.input.as_ref().map(|p| p.text.clone()).unwrap_or_default();
            app.move_cursor = 0; // the row under the old index is a different file
        }
        return Flow::Continue;
    }
    // The help overlay is transient and swallows the key that closes it, rather
    // than closing AND acting. Dismissing must never be the same keystroke that
    // drops a commit, and `?` reads the screen — it should not also change it.
    if app.help {
        app.help = false;
        return Flow::Continue;
    }
    // Enter is a two-step gate: the first press reports scope, the second
    // applies. Any key that CHANGES what would apply cancels it, so an arm can
    // never fire late against a plan you have since edited.
    //
    // Navigation is exempt. Enter says "rewrite 4 commit(s)" and the natural
    // reply is `↓` to see which four — which used to disarm silently, the only
    // tell being the status text losing its yellow. Moving a cursor cannot
    // change the outcome: the recipe comes from `targets`, the shape from
    // `shape`, the move from `move_target`. None of them read a cursor.
    let was_pending = app.pending_apply;
    let navigation = matches!(
        key,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Left
            | KeyCode::Right
    );
    if key != KeyCode::Enter && !navigation {
        app.pending_apply = false;
    }
    // Same gate for quit, and for the same reason: every rewrite here is
    // byte-identically abortable, but the ROUTING is not — a stray `q` after
    // hand-routing ten hunks loses all of it with nothing to undo.
    let was_quitting = app.pending_quit;
    if key != KeyCode::Char('q') {
        app.pending_quit = false;
    }
    match key {
        // Quit / cancel / help
        KeyCode::Char('q') => match app.unapplied() {
            Some(what) if !was_quitting => {
                app.pending_quit = true;
                app.status = format!("q again to quit and lose {what} — ⏎ applies, p previews");
            }
            _ => return Flow::Quit,
        },
        KeyCode::Char('?') => app.help = true,
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

        // Selection / routing — distinct mnemonic letters, no shift keys at all.
        KeyCode::Char(' ') => app.toggle(),
        KeyCode::Char('t') => app.set_target(),
        KeyCode::Char('f') => app.route_all_to_cursor(), // "fix": all → cursor commit
        KeyCode::Char('a') => app.accept_inference(),    // "absorb": inferred targets
        KeyCode::Char('m') => return app.toggle_files(), // source: the file list
        KeyCode::Char('e') => return Flow::OpenCommit,   // source: this commit's hunks
        KeyCode::Char('w') => return Flow::OpenUnstaged, // source: the worktree
        KeyCode::Char('/') => app.start_filter(),         // narrow the file list

        // Stack shape — commit pane only. `[`/`]` are plain characters every
        // terminal delivers, unlike shift+arrow. The letters are `rebase -i`'s.
        KeyCode::Char('[') => app.move_commit(-1),
        KeyCode::Char(']') => app.move_commit(1),
        KeyCode::Char('d') => app.mark_shape(true),
        KeyCode::Char('s') => app.mark_shape(false),
        KeyCode::Char('r') => app.start_reword(),

        // Act
        KeyCode::Char('p') => return Flow::Preview,
        KeyCode::Char('u') => return Flow::Undo,
        // A merge rule / whitespace mode that changed silently is the dangerous
        // state, so both re-preview at once and both show a sticky badge.
        KeyCode::Char('c') => {
            app.cycle_favor();
            return Flow::Preview;
        }
        KeyCode::Char('i') => {
            app.opts.ignore_ws = !app.opts.ignore_ws;
            app.status = if app.opts.ignore_ws {
                "ignoring whitespace in every merge — i toggles".into()
            } else {
                "whitespace is significant again (the default) — i toggles".into()
            };
            return Flow::Preview;
        }
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

    /// Is the phantom `+ new commit` row on screen? For every hunk source, since
    /// "route these hunks somewhere that doesn't exist yet" reads the same way
    /// whichever pile they came from — only the destination differs:
    ///
    /// - `Commit(_)`: a new commit inserted *before* that one. This is `split`.
    /// - `Staged` / `Unstaged`: a new commit at the **tip**. This is `commit -p`,
    ///   and it needs no replay at all — the synthetic simply becomes the tip.
    ///
    /// Not for `Files`: a whole-file move has a destination, not a selection.
    fn has_phantom(&self) -> bool {
        !matches!(self.source, Source::Files)
    }

    /// Does the phantom row mean "a new commit at the tip" rather than a split?
    fn phantom_is_tip(&self) -> bool {
        !matches!(self.source, Source::Commit(_) | Source::Files)
    }

    /// Is the commit cursor parked on the phantom row?
    fn on_phantom(&self) -> bool {
        self.has_phantom() && self.phantom_cursor
    }

    /// The commit under the cursor — None on the phantom row, which is a
    /// destination rather than a commit, so every commit VERB refuses there.
    fn cursor_commit(&self) -> Option<Oid> {
        if self.on_phantom() {
            return None;
        }
        self.commits.get(self.commit_cursor).map(|c| c.oid)
    }

    /// What `t` / `f` would route to: a real commit, or the phantom.
    fn route_target(&self) -> Option<Oid> {
        if self.on_phantom() {
            return Some(phantom());
        }
        self.cursor_commit()
    }

    /// Does any picked hunk go to the phantom row (i.e. is this a split)?
    fn splits(&self) -> bool {
        self.files
            .iter()
            .flat_map(|f| f.selected.iter().zip(&f.targets))
            .any(|(&s, t)| s && *t == Some(phantom()))
    }

    /// Target commit highlighted in the commit list (hunk's target, or move dest).
    fn active_target(&self) -> Option<Oid> {
        match self.source {
            Source::Files => self.move_target,
            _ => self.flat.get(self.hunk_cursor).and_then(|&(fi, hi)| self.files[fi].targets[hi]),
        }
    }

    /// How many picked hunks land on each destination — the WHOLE routing, not
    /// just the cursor hunk's. `active_target` marks one commit; this is the plan,
    /// and without it a five-hunk selection was only legible by walking the list.
    /// A Vec, not a map: the stack is capped at [`DEFAULT_STACK`] rows.
    fn routing(&self) -> Vec<(Oid, usize)> {
        let mut out: Vec<(Oid, usize)> = Vec::new();
        for t in self
            .files
            .iter()
            .flat_map(|f| f.selected.iter().zip(&f.targets))
            .filter_map(|(&s, t)| if s { *t } else { None })
        {
            match out.iter_mut().find(|(o, _)| *o == t) {
                Some((_, n)) => *n += 1,
                None => out.push((t, 1)),
            }
        }
        out
    }

    /// The routing as one line, newest destination first — what `p` reports next
    /// to the tip, so a preview says WHERE things go and not just that it's clean.
    /// This is the table `absorb -n` prints on the CLI.
    fn routing_summary(&self) -> String {
        let mut v = self.routing();
        v.sort_by_key(|&(o, _)| self.commits.iter().position(|c| c.oid == o).unwrap_or(usize::MAX));
        v.iter()
            .map(|&(o, n)| match o == phantom() {
                true => format!("{n} hunk(s) → + new commit"),
                false => format!("{n} hunk(s) → {o:.8}"),
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// `/` — narrow the tracked-file list. Only there: the file list is every
    /// blob in HEAD's tree, which is unreachable by arrow key on a real repo,
    /// while the commit list is capped at [`DEFAULT_STACK`] rows and is not.
    fn start_filter(&mut self) {
        if self.source != Source::Files {
            self.status = "/ filters the file list — press m first".into();
            return;
        }
        self.input = Some(Prompt { label: "filter", text: self.filter.clone(), what: Ask::Filter });
    }

    /// The file rows actually on screen: `move_files` narrowed by `filter`.
    ///
    /// `move_cursor` indexes THIS, never `move_files` — a cursor that indexed the
    /// unfiltered list would move the wrong file the moment you typed anything,
    /// which is the worst possible failure for a verb that rewrites history.
    fn visible_files(&self) -> Vec<&str> {
        self.move_files
            .iter()
            .map(|s| s.as_str())
            .filter(|p| self.filter.is_empty() || p.contains(&self.filter))
            .collect()
    }

    /// Work that `q` would throw away, described for the confirm. A pending shape
    /// edit, a move destination, or hunks routed somewhere inference did not put
    /// them. A default absorb is deliberately NOT work: opening the TUI, looking,
    /// and pressing `q` must stay instant, or the confirm becomes noise you learn
    /// to type through.
    fn unapplied(&self) -> Option<String> {
        if self.shape != Shape::None {
            return Some(self.shape_summary());
        }
        if self.move_target.is_some() {
            return Some("a file move destination".into());
        }
        // `is_some()` matters: a target of None is never something the user
        // typed — `t` only ever sets Some — so "inference found a home and the
        // target is None" is less routing than we started with, not work to save.
        let n = self
            .files
            .iter()
            .flat_map(|f| f.selected.iter().zip(&f.targets).zip(&f.inferred))
            .filter(|((&s, t), i)| s && t.is_some() && t != i)
            .count();
        (n > 0).then(|| format!("{n} hand-routed hunk(s)"))
    }

    /// Conflict rule and whitespace mode, as a sticky badge for the context line
    /// — empty when both are at their default. A merge rule you cannot see is
    /// the dangerous state: it decides what a conflict silently becomes.
    fn badges(&self) -> String {
        let mut s = String::new();
        if let Some(f) = self.opts.favor {
            s.push_str(&format!(" · rule:{}", favor_name(f)));
        }
        if self.opts.ignore_ws {
            s.push_str(" · ignore-ws");
        }
        s
    }

    /// `c`: cycle the conflict rule abort → ours → theirs → union → abort.
    fn cycle_favor(&mut self) {
        use git2::FileFavor::*;
        self.opts.favor = match self.opts.favor {
            None => Some(Ours),
            Some(Ours) => Some(Theirs),
            Some(Theirs) => Some(Union),
            _ => None,
        };
        self.status = match self.opts.favor {
            None => "conflict rule: abort (the default) — c cycles".into(),
            Some(f) => format!("conflict rule: {} — c cycles", favor_name(f)),
        };
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
            Some(o) if o == phantom() => "+ new commit".into(),
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
            Source::Unstaged => "unstaged".into(),
            Source::Commit(o) => format!("from {o:.8}"),
            Source::Files => "files".into(),
        }
    }

    /// The one line that is ALWAYS on screen: which source, how many picked, and
    /// what the cursor is on. It is the only state visible while you Tab away to
    /// choose a destination, so it carries the source and the count.
    fn context_line(&self) -> String {
        let body = match self.source {
            Source::Files => {
                let file = self.visible_files().get(self.move_cursor).copied().unwrap_or("-");
                format!("move {} → {}", file, self.target_label(self.move_target))
            }
            _ => match self.flat.get(self.hunk_cursor) {
                Some(&(fi, hi)) => {
                    let f = &self.files[fi];
                    format!(
                        "{} · hunk {}/{} · {} picked · {} {} → {}",
                        self.source_label(),
                        self.hunk_cursor + 1,
                        self.flat.len(),
                        self.picked(),
                        if f.selected[hi] { "[x]" } else { "[ ]" },
                        truncate(&f.path, 20),
                        self.target_label(f.targets[hi]),
                    )
                }
                None => format!("{} · no hunks", self.source_label()),
            },
        };
        format!("{body}{}", self.badges())
    }

    /// How many commits a rewrite would touch: from the oldest EDITED commit to
    /// HEAD. That includes the source commit — a forward move also reverts the
    /// hunk there, and the source is older than the target in that direction.
    fn rewrite_span(&self) -> usize {
        self.oldest_touched().map(|i| i + 1).unwrap_or(0)
    }

    /// Index in `commits` (newest-first, so the HIGHEST index is oldest) of the
    /// oldest commit this selection touches, or None when nothing is selected.
    ///
    /// The source commit counts whenever anything is selected: a forward move
    /// also reverts there, and a split rewrites it. The phantom row has no index
    /// of its own — it is inserted immediately before the source — so a
    /// selection that ONLY routes to the phantom still reaches back to it.
    fn oldest_touched(&self) -> Option<usize> {
        let idx = |o: Oid| self.commits.iter().position(|c| c.oid == o);
        let mut oldest = self
            .files
            .iter()
            .flat_map(|f| f.selected.iter().zip(&f.targets))
            .filter_map(|(&s, t)| if s { *t } else { None })
            .filter_map(idx)
            .max();
        if oldest.is_some() || self.splits() {
            if let Source::Commit(src) = self.source {
                oldest = oldest.max(idx(src));
            }
        }
        oldest
    }

    /// Parent of the OLDEST commit the recipe touches — the base to replay from.
    /// Walking to the root instead would rewrite untouched history and, worse,
    /// abort on a merge commit deeper in the stack that the edit never reaches.
    fn replay_base(&self, repo: &Repository) -> Option<Oid> {
        let oldest = self.oldest_touched()?;
        // None when that commit is a root: replay from the beginning.
        repo.find_commit(self.commits.get(oldest)?.oid).ok()?.parent_id(0).ok()
    }

    /// Suffix for the arming line naming the GPG signatures this rewrite drops.
    /// Empty when there are none, so the common case costs nothing on screen.
    fn sign_note(repo: &Repository, oids: impl IntoIterator<Item = Oid>) -> String {
        let n = oids
            .into_iter()
            .filter(|&o| repo.find_commit(o).is_ok_and(|c| git::is_signed(&c)))
            .count();
        match n {
            0 => String::new(),
            n => format!(" · {n} GPG signature(s) will be LOST"),
        }
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

    /// Shape keys act on a real commit in the commit list — say so rather than
    /// doing nothing. The phantom row is a destination, not a commit.
    fn shape_pane(&mut self) -> bool {
        if self.on_phantom() {
            self.status = "the + row is a destination for hunks (t), not a commit".into();
            return false;
        }
        if self.focus == Pane::Commits {
            return true;
        }
        self.status = "shape keys ([ ] d s r) work on the commit list".into();
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

    /// `r`: open the inline prompt on the commit under the cursor, prefilled
    /// with its summary. Only the summary is edited — the body is preserved on
    /// submit, so rewording a headline never silently deletes the paragraphs
    /// under it.
    fn start_reword(&mut self) {
        if !self.shape_pane() {
            return;
        }
        let Some(row) = self.commits.get(self.commit_cursor) else { return };
        self.input = Some(Prompt {
            label: "message",
            text: row.summary.clone(),
            what: Ask::Reword(row.oid),
        });
    }

    /// `d` / `s`: mark the commit under the cursor dropped, or squashed into its
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
                "{} — rewrite {} commit(s) on {}{} · Enter again to apply · Esc: cancel",
                self.shape_summary(),
                plan.order.len(),
                self.short_branch(),
                Self::sign_note(repo, plan.order.iter().copied())
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
    /// armed apply → shape edit → right pane → non-staged source → (home).
    fn go_back(&mut self, was_pending: bool) -> Option<Flow> {
        if was_pending {
            self.status = "apply cancelled".into();
        } else if self.shape != Shape::None {
            self.clear_shape();
            self.status = "shape edit cancelled".into();
        } else if self.focus == Pane::Right {
            self.focus = Pane::Commits;
            self.status = "back to the commit list".into();
        } else if self.source != Source::Staged {
            // leaving a commit / file source needs the repo → driver reloads
            return Some(Flow::ResetSource);
        } else {
            self.status = "already at the top level — q to quit".into();
        }
        None
    }

    /// Home/End: jump the focused list to its first or last entry.
    fn jump(&mut self, to_end: bool) {
        let len = match (self.focus, self.source) {
            (Pane::Commits, _) => self.commits.len(),
            (Pane::Right, Source::Files) => self.visible_files().len(),
            (Pane::Right, _) => self.flat.len(),
        };
        let target = if to_end { len.saturating_sub(1) } else { 0 };
        match (self.focus, self.source) {
            (Pane::Commits, _) => {
                self.commit_cursor = target;
                self.phantom_cursor = !to_end; // the phantom sits above the newest
                self.diff_scroll = 0;
            }
            (Pane::Right, Source::Files) => self.move_cursor = target,
            (Pane::Right, _) => self.hunk_cursor = target,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.focus == Pane::Commits {
            self.diff_scroll = 0; // new commit → start at the top of its diff
        }
        match (self.focus, self.source) {
            // The phantom is one extra stop ABOVE the newest commit. It lives
            // outside `commits`, so walk a virtual index and split it back out.
            (Pane::Commits, _) if self.has_phantom() => {
                let cur = if self.phantom_cursor { 0 } else { self.commit_cursor + 1 };
                let next = step(cur, self.commits.len() + 1, delta);
                self.phantom_cursor = next == 0;
                self.commit_cursor = next.saturating_sub(1);
            }
            (Pane::Commits, _) => {
                self.commit_cursor = step(self.commit_cursor, self.commits.len(), delta)
            }
            (Pane::Right, Source::Files) => {
                self.move_cursor = step(self.move_cursor, self.visible_files().len(), delta)
            }
            (Pane::Right, _) => self.hunk_cursor = step(self.hunk_cursor, self.flat.len(), delta),
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Pane::Commits => Pane::Right,
            Pane::Right => Pane::Commits,
        };
    }

    /// `m`: swap the right pane to the tracked-file list, or back out of it.
    /// `move-file` is a SOURCE like the other two, not a second mode — leaving
    /// it reloads, exactly as leaving a commit source does.
    fn toggle_files(&mut self) -> Flow {
        if self.source == Source::Files {
            return Flow::ResetSource;
        }
        self.source = Source::Files;
        self.focus = Pane::Right;
        self.move_cursor = 0;
        self.status = "move a whole file: pick it, Tab to a commit, t, then Enter".into();
        Flow::Continue
    }

    /// Space: toggle the hunk under the cursor (hunk sources only).
    fn toggle(&mut self) {
        // Space is a HUNK verb, so it needs the hunk pane focused — it used to
        // check only the source, so pressing it from the commit list (where you
        // start) silently flipped whichever hunk the other cursor was on.
        if self.focus != Pane::Right {
            self.status = "Space picks hunks — Tab to the hunk pane first".into();
            return;
        }
        if self.source == Source::Files {
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
        let Some(oid) = self.route_target() else { return };
        if oid == phantom() {
            if let Some(&(fi, hi)) = self.flat.get(self.hunk_cursor) {
                self.files[fi].selected[hi] = true; // routing it IS picking it
                self.files[fi].targets[hi] = Some(oid);
                self.status = match self.phantom_is_tip() {
                    true => "hunk → a NEW commit at the tip — ⏎ names it and commits".into(),
                    false => "hunk → a NEW commit before the source (split) — ⏎ names it".into(),
                };
            }
            return;
        }
        if self.source == Source::Files {
            self.move_target = Some(oid);
            self.status = format!("move destination → {oid:.8}");
            return;
        }
        if let Some(&(fi, hi)) = self.flat.get(self.hunk_cursor) {
            self.files[fi].targets[hi] = Some(oid);
            // Direction matters: backward is absorbed idempotently, forward also
            // reverts at the source and can conflict.
            let note = match self.source {
                Source::Commit(src) if self.is_newer(oid, src) => " (forward — may conflict)",
                Source::Commit(_) => " (backward — clean)",
                _ => "",
            };
            self.status = format!("hunk → {oid:.8}{note}");
        }
    }

    /// `f`: route EVERY selected hunk to the commit under the cursor (= fix).
    fn route_all_to_cursor(&mut self) {
        if self.source == Source::Files {
            return;
        }
        let Some(oid) = self.route_target() else { return };
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
        } else if oid == phantom() {
            match self.phantom_is_tip() {
                true => format!("{n} selected hunk(s) → a NEW commit at the tip"),
                false => format!("{n} selected hunk(s) → a NEW commit before the source (split)"),
            }
        } else {
            format!("{n} selected hunk(s) → {oid:.8} (fix)")
        };
    }

    /// `a`: reset every hunk's target to inference's suggestion (= absorb).
    fn accept_inference(&mut self) {
        // A commit's own hunks have no inferred home; resetting would silently
        // wipe the destinations the user just picked.
        if matches!(self.source, Source::Commit(_)) {
            self.status = "no inference for a commit's hunks — pick destinations with t".into();
            return;
        }
        // Same reason as `toggle`: with the file list open this rewrote every
        // staged hunk's target and reported success, for a pane showing files.
        if self.source == Source::Files {
            self.status = "the file list has no hunks to re-infer — Esc back to staged".into();
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
        if self.source == Source::Files {
            match self.move_plan(repo) {
                Ok(Some((base, tip, rec))) => match engine::replay(repo, base, tip, &rec, self.opts.merge(), false) {
                    Ok(p) => self.status = format!("clean, would move {} to {:.8}", self.short_branch(), p.tip),
                    Err(e) => self.status = format!("conflict: {e}"),
                },
                Ok(None) => self.status = "pick a file and a destination (t) first".into(),
                Err(e) => self.status = format!("{e}"),
            }
            return;
        }
        if self.splits() {
            if let Err(e) = self.check_split_is_the_whole_selection() {
                self.status = e;
                return;
            }
            // A tip commit rewrites nothing, so there is no replay to dry-run and
            // nothing that can conflict — say exactly that rather than pretending
            // to have previewed a merge.
            if self.phantom_is_tip() {
                self.status = match self.tip_commit(repo, "preview") {
                    Ok(Some(_)) => format!(
                        "clean — {} picked hunk(s) would become a NEW commit on top of {:.8}; nothing below it moves",
                        self.picked(),
                        self.head
                    ),
                    Ok(None) => "pick hunks (Space) and route them to + new commit with t".into(),
                    Err(e) => format!("error: {e}"),
                };
                return;
            }
            let msg = self.split_message();
            self.status = match self.split_plan(repo, &msg) {
                Ok(Some(plan)) => {
                    let opts = ops::Opts { dry_run: true, ..self.opts };
                    match ops::shape(repo, plan, "transplant: tui split", false, &opts) {
                        Ok(o) => format!(
                            "clean, would move {} to {:.8} (split into a new commit)",
                            self.short_branch(),
                            o.new_tip
                        ),
                        Err(e) => format!("conflict: {e}"),
                    }
                }
                Ok(None) => "pick hunks (Space) and route them with t first".into(),
                Err(e) => format!("{e}"),
            };
            return;
        }
        match self.build_recipe(repo) {
            Ok(r) if !r.is_empty() => match engine::replay(repo, self.replay_base(repo), self.head, &r, self.opts.merge(), true) {
                Ok(p) if p.tip == self.head => self.status = "no change — targets already hold these hunks".into(),
                Ok(p) => {
                    self.status =
                        format!("clean, would move {} to {:.8} · {}", self.short_branch(), p.tip, self.routing_summary())
                }
                Err(e) => self.status = format!("conflict: {e}"),
            },
            Ok(_) => self.status = "select hunks (Space) and set targets (t) first".into(),
            Err(e) => self.status = format!("preview error: {e}"),
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
        match self.source {
            Source::Files => self.execute_move(repo),
            _ => self.execute_hunks(repo),
        }
    }

    /// A split routes the WHOLE selection into the new commit.
    //
    // ponytail: WONTFIX, decided rather than deferred. Mixing "some hunks split
    // off, others absorbed elsewhere" in one apply is smaller than the earlier
    // note here claimed — `recipe::shaped` already takes an `edited` index for
    // exactly the prefix-trim problem (squash uses it), so `split_at` would take
    // `build_recipe`'s recipe plus the oldest touched index and the rest falls
    // out. It is declined on value, not cost: the OUTCOME is already reachable
    // in two applies, each individually previewable and abortable, and that is
    // asserted by `what_a_mixed_selection_wanted_is_reachable_in_two_applies`.
    // So the refusal costs a keystroke, not a capability — while a one-pass
    // version widens the blast radius of the single riskiest thing here, a
    // rewrite that half-applies. `t` sets one hunk at a time, so a mix is
    // usually a slip anyway. Revisit if anyone actually hits this refusal.
    fn check_split_is_the_whole_selection(&self) -> std::result::Result<(), String> {
        let stray = self
            .files
            .iter()
            .flat_map(|f| f.selected.iter().zip(&f.targets))
            .any(|(&s, t)| s && matches!(t, Some(o) if *o != phantom()));
        if stray {
            return Err("split takes the whole selection — route every picked hunk to + new commit".into());
        }
        Ok(())
    }

    fn execute_hunks(&mut self, repo: &Repository) {
        // A phantom route is a SPLIT: it needs a new commit in the replay ORDER,
        // and a message for it. The prompt IS the confirmation gate here — you
        // cannot apply without naming the commit you are creating.
        if self.splits() {
            self.pending_apply = false;
            if let Err(e) = self.check_split_is_the_whole_selection() {
                self.status = e;
                return;
            }
            let what = match self.source {
                Source::Commit(src) => Ask::Split(src),
                _ => Ask::NewCommit,
            };
            self.input = Some(Prompt { label: "message", text: self.split_message(), what });
            return;
        }
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
            let sign_note = Self::sign_note(repo, self.commits[..n].iter().map(|c| c.oid));
            self.status = format!(
                "rewrite {n} commit(s) on {}{drop_note}{sign_note} — Enter again to apply · Esc: cancel",
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
            Ok(p) => match ops::sibling_refs(repo, &self.branch)
                .map_err(anyhow::Error::msg)
                .and_then(|refs| {
                    // Scan BEFORE the ref moves: an unreadable refdb refuses here,
                    // rather than reporting that nothing was stranded.
                    ops::promote(repo, &self.branch, p.tip, self.head, MSG, false)
                        .map(|()| refs)
                        .map_err(anyhow::Error::msg)
                }) {
                Ok(refs) => {
                    // Unstaged hunks came from the worktree, not the index, so the
                    // index is now BEHIND the history we just wrote. Advance it for
                    // exactly the paths we folded, or `git status` shows a phantom
                    // staged reversal of the change. See `refresh_index`.
                    let mut index_note = String::new();
                    if self.source == Source::Unstaged {
                        let paths: Vec<String> = self
                            .files
                            .iter()
                            .filter(|f| f.selected.iter().any(|&s| s))
                            .map(|f| f.path.clone())
                            .collect();
                        if let Err(e) = refresh_index(repo, p.tip, &paths) {
                            index_note = format!(" · index not synced ({e}) — `git reset` fixes it");
                        }
                    }
                    let (moved, warns) =
                        ops::restack(repo, &p.map, self.head, MSG, &self.opts, &refs);
                    let note = match (moved.len(), warns.len()) {
                        (0, 0) => String::new(),
                        (n, 0) => format!(" · restacked {n} branch(es)"),
                        (0, _) => format!(" · {}", warns.join("; ")),
                        (n, _) => format!(" · restacked {n}, {}", warns.join("; ")),
                    };
                    self.status = format!(
                        "{} now at {:.8} (was {:.8}){note}{index_note} · undo: git-transplant undo",
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
        // Owned: the rest of this function mutates `self`, and a `&str` borrowed
        // out of `visible_files` would keep the whole struct borrowed.
        let path = self.visible_files().get(self.move_cursor).map(|s| s.to_string());
        let (Some(path), Some(target)) = (path, self.move_target) else {
            self.status = "pick a file (↑↓) and a destination (Tab to commits, then t)".into();
            return;
        };
        if !self.pending_apply {
            // Same signature note as the other two gates. The move's span isn't
            // the visible stack — it reaches back to wherever the file is
            // introduced — so it comes from the plan, not from the screen.
            let note = self
                .move_plan(repo)
                .ok()
                .flatten()
                .and_then(|(base, tip, _)| git::linear_commits(repo, base, tip).ok())
                .map(|cs| Self::sign_note(repo, cs.iter().map(|c| c.id())))
                .unwrap_or_default();
            self.pending_apply = true;
            self.status = format!(
                "move {path} → {:.8} on {}{note} — Enter again to apply · any key: cancel",
                target,
                self.short_branch()
            );
            return;
        }
        self.pending_apply = false;
        match ops::mv(repo, &path, &target.to_string(), &self.opts) {
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
        let (Some(path), Some(target)) = (self.visible_files().get(self.move_cursor).copied(), self.move_target) else {
            return Ok(None);
        };
        ops::require_fully_clean(repo).map_err(anyhow::Error::msg)?;
        let plan = recipe::mv(repo, path, target, self.head).map_err(anyhow::Error::msg)?;
        Ok(Some((plan.base, plan.tip, plan.recipe)))
    }

    /// One synthetic commit carrying every hunk routed to the phantom row,
    /// across all files, parented at the source commit's parent — exactly the
    /// shape [`recipe::split_at`] wants. This is
    /// [`patch::synthetic_for_hunks`] with the per-file loop pulled out, because
    /// a split has to be ONE commit however many files it touches.
    fn phantom_synthetic(&self, repo: &Repository, msg: &str) -> Result<Option<Oid>> {
        let base = repo.find_commit(self.source_base)?;
        let mut tree = base.tree()?.id();
        let mut any = false;
        for f in &self.files {
            let mask = mask_for_target(&f.selected, &f.targets, phantom());
            if !mask.iter().any(|&b| b) {
                continue;
            }
            any = true;
            let partial = patch::apply_selected(&f.old_full, &f.hunks, &mask);
            let blob = repo.blob(partial.as_bytes())?;
            tree = engine::set_path(repo, tree, &f.path, Some((blob, f.mode)))?;
        }
        if !any {
            return Ok(None);
        }
        let sig = git::ident(repo);
        Ok(Some(repo.commit(None, &sig, &sig, msg, &repo.find_tree(tree)?, &[&base])?))
    }

    /// Default message for the new commit, matching what `split` prints from the
    /// CLI.
    fn split_message(&self) -> String {
        match self.source {
            Source::Commit(o) => format!("{} (part 1)", self.summary_of(o)),
            // A tip commit is new work, not a piece of something — leave the
            // message empty rather than prefilling a word the user must delete.
            _ => String::new(),
        }
    }

    /// The picked hunks as one new commit **at the tip**, parented at HEAD.
    ///
    /// No replay, no recipe, no engine: nothing below HEAD changes, so this is a
    /// plain commit plus the same compare-and-swap ref move everything else uses.
    ///
    /// The tree starts from HEAD's, not from `source_base`'s — for an unstaged
    /// source those differ by the whole index, and starting there would sweep
    /// unrelated staged work into the commit. Only the paths we actually touch
    /// take their content from `source_base`, which is the minimum that can
    /// possibly work: an unstaged hunk's line numbers assume the staged text
    /// beneath it, so that file's staged part necessarily comes along.
    fn tip_commit(&self, repo: &Repository, msg: &str) -> Result<Option<Oid>> {
        let head = repo.find_commit(self.head)?;
        let mut tree = head.tree()?.id();
        let mut any = false;
        for f in &self.files {
            let mask = mask_for_target(&f.selected, &f.targets, phantom());
            if !mask.iter().any(|&b| b) {
                continue;
            }
            any = true;
            let partial = patch::apply_selected(&f.old_full, &f.hunks, &mask);
            let blob = repo.blob(partial.as_bytes())?;
            tree = engine::set_path(repo, tree, &f.path, Some((blob, f.mode)))?;
        }
        if !any {
            return Ok(None);
        }
        let sig = git::ident(repo);
        Ok(Some(repo.commit(None, &sig, &sig, msg, &repo.find_tree(tree)?, &[&head])?))
    }

    /// The plan for a split: hunks routed to the phantom become a new commit
    /// inserted immediately before the source. None when nothing goes there.
    ///
    /// This is the same primitive `split <rev> <paths>` uses — only how the
    /// split-off commit's tree is built differs (hunks here, whole paths there).
    fn split_plan(&self, repo: &Repository, msg: &str) -> Result<Option<recipe::Shaped>> {
        let Source::Commit(src) = self.source else { return Ok(None) };
        let Some(first) = self.phantom_synthetic(repo, msg)? else { return Ok(None) };
        let plan = recipe::split_at(repo, self.base, self.head, src, first)
            .map_err(anyhow::Error::msg)?;
        Ok(Some(plan))
    }

    /// Assemble the replay recipe from hunk selections: each (file, target) group
    /// of selected hunks becomes one synthetic commit applied at that target.
    fn build_recipe(&self, repo: &Repository) -> Result<engine::Recipe> {
        let mut recipe = engine::Recipe::new();
        for f in &self.files {
            for (t, mask) in recipe_groups(&f.selected, &f.targets) {
                // The phantom's hunks are carried by the replay ORDER (one new
                // commit), not by an edit at an existing one. See `split_plan`.
                if t == phantom() {
                    continue;
                }
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

/// All blob paths in a tree (for the `Source::Files` list).
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
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .split(f.area());
    let (body, status_area) = (rows[0], rows[1]);
    // `Min(32)` on the left, not `Percentage(30)`: at 80 columns 30% left the
    // commit pane 24 cells, which clipped summaries to ~12 characters and cut
    // the title mid-word. 32 is the narrowest that fits `▶ ` + a short oid + a
    // usable summary; the right pane still gets ~48, which the hunk rows are
    // already scaled for (`wide` in `render_hunks`).
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(32), Constraint::Percentage(70)])
        .split(body);
    let (left, right) = (cols[0], cols[1]);
    render_commits(f, app, left);
    // The right column is SPLIT, not swapped. It used to follow focus
    // (lazygit-style), which meant the two things you are relating were never on
    // screen together: Tab-ing to the commit list to choose a destination hid the
    // very hunk you were routing, and `context_line` existed to narrate the one
    // that vanished. Source rows on top, the cursor commit's diff below, always
    // both — the cost is per-hunk previews, which is why only the cursor row
    // expands in `render_hunks`.
    //
    // Except when there is nothing to pick: an empty source pane and the diff
    // never share the column, because one of them has nothing to show. Whichever
    // has focus takes all of it.
    //
    // That is not only tidiness. `List` DROPS an item taller than its viewport
    // instead of clipping it, so halving this pane made the empty state's
    // fifteen-line "here is what this screen does" text render as nothing at all.
    // Anything that must survive here has to fit, or not be in a `List`.
    if app.flat.is_empty() && app.source != Source::Files {
        match app.focus {
            Pane::Commits => render_commit_diff(f, app, right),
            Pane::Right => render_hunks(f, app, right),
        }
    } else {
        let stack = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(right);
        match app.source {
            Source::Files => render_move(f, app, stack[0]),
            _ => render_hunks(f, app, stack[0]),
        }
        render_commit_diff(f, app, stack[1]);
    }
    render_status(f, app, status_area);
    // Last, over everything — including the status line it replaces.
    if app.help {
        render_help(f, app, f.area());
    }
}

/// Filemode on the CHANGED side, straight off the diff. Read from the tree it
/// would silently downgrade a new executable or symlink to 0o100644 — and for an
/// untracked file there is no tree entry to read at all, while libgit2 has
/// already stat'd it.
fn file_mode(delta: &git2::DiffDelta) -> i32 {
    match u32::from(delta.new_file().mode()) {
        0 => 0o100644, // unset (git2 reports Unreadable) — assume a plain file
        m => m as i32,
    }
}

/// One path's before/after bytes as a selectable [`FileEntry`], with targets
/// prefilled from blame. Shared by the staged and unstaged loaders: both diff two
/// snapshots, and both must refuse a binary the same way rather than one of them
/// quietly dropping it. Returns None when there is nothing foldable, pushing to
/// `skipped` only when the reason is worth reporting.
fn file_entry(
    repo: &Repository,
    path: String,
    old: Vec<u8>,
    new: Vec<u8>,
    mode: i32,
    window: &[Oid],
    skipped: &mut Vec<String>,
) -> Option<FileEntry> {
    let hunks = match patch::hunks(&old, &new) {
        Ok(h) if !h.is_empty() => h,
        _ => return None,
    };
    // BOTH sides must be valid UTF-8: patch::hunks reads line text with
    // from_utf8_lossy, so an unchecked `new` would commit U+FFFD in place of the
    // original bytes. (ops::collapse checks both; this path must too.)
    let old_full = match (String::from_utf8(old), std::str::from_utf8(&new)) {
        (Ok(s), Ok(_)) => s,
        _ => {
            // Binary / non-UTF-8: not safely hunk-foldable. Record it so the UI
            // says so — silently dropping the user's work is never acceptable.
            skipped.push(path);
            return None;
        }
    };
    let inferred = inference::infer_targets(repo, &path, &hunks, window)
        .unwrap_or_else(|_| vec![None; hunks.len()]);
    let selected = vec![true; hunks.len()];
    Some(FileEntry { path, old_full, hunks, selected, targets: inferred.clone(), inferred, mode })
}

/// Every local branch tip, as `(oid, short name)`. Collected once per load: the
/// alternative is a ref walk per commit row.
fn branch_decorations(repo: &Repository) -> Vec<(Oid, String)> {
    let mut out = Vec::new();
    let Ok(branches) = repo.branches(Some(git2::BranchType::Local)) else { return out };
    for b in branches.flatten() {
        if let (Some(oid), Ok(Some(name))) = (b.0.get().target(), b.0.name()) {
            out.push((oid, name.to_string()));
        }
    }
    out
}

/// Fill the cursor commit's diff if it hasn't been read yet. Called from the
/// event loop, which is the only place that has both `&mut App` and the repo.
fn ensure_diff(app: &mut App, repo: &Repository) {
    let Some(row) = app.commits.get(app.commit_cursor) else { return };
    if row.diff.is_some() {
        return;
    }
    let oid = row.oid;
    if let Ok(c) = repo.find_commit(oid) {
        let lines = commit_diff_lines(repo, &c);
        app.commits[app.commit_cursor].diff = Some(lines);
    }
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
    // Be honest about what Tab does. It used to REVEAL the rows, so the hint
    // named them; they now sit in the pane directly above this one, and Tab only
    // moves focus there. With nothing staged there is still nothing to focus.
    let hint = match (app.source, app.flat.is_empty()) {
        (Source::Files, _) => "Tab: the file list above",
        (_, true) => "read-only · nothing staged to fold",
        (_, false) => "Tab: pick the hunks above",
    };
    // On the phantom row the commit list's diff would be a lie — that row is a
    // commit that does not exist yet. Show what it WOULD contain instead.
    if app.on_phantom() {
        let mut lines = vec![Line::from(Span::styled(
            match app.phantom_is_tip() {
                true => "A new commit on top of the stack, from the hunks you pick.",
                false => "A new commit, inserted before the one these hunks came from.",
            },
            theme::dim(),
        ))];
        for f in &app.files {
            for (hi, t) in f.targets.iter().enumerate() {
                if f.selected[hi] && *t == Some(phantom()) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{} ", f.path), theme::path()),
                        Span::styled(f.hunks[hi].header.clone(), theme::dim()),
                    ]));
                }
            }
        }
        if lines.len() == 1 {
            lines.push(Line::from("Nothing routed here yet — pick hunks with Space, then press t."));
        }
        let title = match app.phantom_is_tip() {
            true => "[NEW COMMIT] ⏎ names it and commits",
            false => "[NEW COMMIT] ⏎ names it and splits",
        };
        f.render_widget(Paragraph::new(lines).block(list_block(title, true)), area);
        return;
    }
    let (title, diff): (String, &[(char, String)]) = match app.commits.get(app.commit_cursor) {
        Some(c) => (format!("[DIFF] {:.8} {} ({hint})", c.oid, c.summary), c.diff.as_deref().unwrap_or(&[])),
        None => ("[DIFF]".to_string(), &[]),
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
    /// Chrome carries NO foreground colour — it dims whatever the terminal's own
    /// default foreground is. The previous `DarkGray` was ANSI bright-black, which
    /// on a black background is very nearly the background (the reported "dark
    /// grey on black" help text) and on a light one is fine, so it could only ever
    /// be right for half of all users. DIM inherits, so it is legible on both; and
    /// where a terminal ignores DIM the text falls back to full contrast — too
    /// loud, never invisible, which is the failure direction we want.
    pub fn dim() -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }
    pub fn border(focused: bool) -> Style {
        if focused {
            Style::default().fg(PATH).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
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
    // `◀` marks where the CURSOR hunk goes; `←N` is how many go there in total.
    // Different facts: without the counter the accumulated plan was invisible,
    // and the only way to read it was to walk every hunk row.
    let plan = app.routing();
    let routed = |oid: Oid| plan.iter().find(|(o, _)| *o == oid).map_or(0, |&(_, n)| n);
    // In the LEFT GUTTER, beside the mark, for the same reason the mark is there:
    // it survives a truncated summary. Trailing, it cost 4-5 cells of summary at
    // 80 columns — re-introducing exactly the clipping `Min(32)` was widened to
    // fix. A fixed two-cell column costs less and can never be the thing clipped.
    let badge = |n: usize| match n {
        0 => Span::raw("  "),
        n => Span::styled(format!("{n:<2}"), theme::dest()),
    };
    let mut items: Vec<ListItem> = Vec::new();
    // The phantom row: a destination that does not exist yet. `t` on it routes
    // the picked hunks into a NEW commit before the source — split, with no new
    // keys. It is drawn here and nowhere else; `commits` never contains it.
    if app.has_phantom() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                if target == Some(phantom()) { "◀" } else { " " },
                theme::dest(),
            ),
            badge(routed(phantom())),
            Span::styled(
                match app.phantom_is_tip() {
                    true => "+ new commit at the tip",
                    false => "+ new commit here",
                },
                theme::added(),
            ),
        ])));
    }
    items.extend(app.commits.iter().map(|c| {
        // Marker lives in a LEFT gutter so it survives truncated summaries.
        // A pending shape edit wins the gutter: it is the bigger change.
        let (mark, style) = match app.shape {
            Shape::Drop(o) if o == c.oid => ("✗", theme::removed()),
            Shape::Squash(o) if o == c.oid => ("⇣", theme::added()),
            _ if Some(c.oid) == target => ("◀", theme::dest()),
            // The commit the open hunks came from — and what a split splits.
            _ if app.source == Source::Commit(c.oid) => ("⌁", theme::path()),
            _ => (" ", theme::dim()),
        };
        ListItem::new(Line::from(vec![
            Span::styled(mark, style),
            badge(routed(c.oid)),
            Span::styled(format!("{:.8} ", c.oid), theme::oid()),
            Span::raw(c.summary.clone()),
            // Which refs a rewrite is about to carry along (`ops::restack`) — on
            // a stacked-PR branch that is the whole reason for the run, and it
            // used to be reported only AFTER the fact. Last on the line: a
            // clipped decoration is a smaller loss than a clipped summary.
            Span::styled(
                match c.refs.is_empty() {
                    true => String::new(),
                    false => format!(" ({})", c.refs.join(", ")),
                },
                theme::path(),
            ),
        ]))
    }));
    let title = match app.shape {
        // No arm for the phantom row: it labels itself, and announcing it here
        // cost the `--base` hint its only place on screen once every source
        // grew a phantom.
        Shape::None if app.base.is_some() => {
            format!("commits · {} shown (--base widens) · N routed", app.commits.len())
        }
        Shape::None => "commits · ◀ target · N routed".to_string(),
        _ => format!("commits · {} pending — p/Enter", app.shape_summary()),
    };
    let list = List::new(items)
        .block(list_block(&title, app.focus == Pane::Commits))
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    // The phantom occupies row 0 when present, so every real commit shifts down
    // by one HERE only — `commit_cursor` still indexes `commits` directly.
    if app.has_phantom() {
        state.select(Some(if app.phantom_cursor { 0 } else { app.commit_cursor + 1 }));
    } else if !app.commits.is_empty() {
        state.select(Some(app.commit_cursor));
    }
    f.render_stateful_widget(list, area, &mut state);
}

/// Max diff lines previewed for the CURSOR hunk in the selector list. Six, not
/// ten: the list now gets half the right column (the other half is the
/// destination's diff), which is 8 content rows at 80×24 — a ten-line body left
/// no room for the row's own header, the `… more` marker, or any other hunk.
const HUNK_PREVIEW_LINES: usize = 6;

fn render_hunks(f: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    for (idx, &(fi, hi)) in app.flat.iter().enumerate() {
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
        // Only the CURSOR row shows its diff. This pane now shares the right
        // column with the destination's diff, and ten preview lines per hunk left
        // no room for the list itself — a screenful of one hunk's body is the
        // same "can't see both things" problem the split was meant to fix.
        // Cap the preview too: one long hunk must not fill the pane either.
        if idx == app.hunk_cursor {
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
        }
        items.push(ListItem::new(lines));
    }
    if items.is_empty() {
        // This pane folds STAGED work — say what to do, not just what's missing.
        let msg = match app.skipped.len() {
            0 => vec![
                Line::from("Nothing staged. This screen does three things:"),
                Line::from(""),
                Line::from(Span::styled("  Fold UNSTAGED work into an old commit", theme::path())),
                Line::from("    Press w — every unstaged hunk appears here with a blame-inferred"),
                Line::from("    target, and Enter absorbs. No `git add -p` first."),
                Line::from(""),
                Line::from(Span::styled(
                    "  Move hunks BETWEEN commits  (nothing to stage)",
                    theme::path(),
                )),
                Line::from("    Esc → commit list, press e on a commit to load its hunks,"),
                Line::from("    Space to pick, then go to the destination commit and press t."),
                Line::from(""),
                Line::from(Span::styled("  Move a whole file", theme::path())),
                Line::from("    m: pick a file, Tab to a commit, t. · q: quit"),
            ],
            n => vec![
                Line::from(format!("{n} staged file(s) can't be hunk-folded (binary, or a whole-file add/delete).")),
                Line::from(""),
                Line::from("Use `m` (the file list) to re-anchor a file at another commit."),
            ],
        };
        items.push(ListItem::new(msg));
    }
    // Counts live in the TITLE so a cramped status bar can never drop them.
    let total: usize = app.files.iter().map(|f| f.hunks.len()).sum();
    let sel = app.picked();
    let title = match app.source {
        Source::Commit(o) => format!("[HUNKS FROM {o:.8}] {sel}/{total} picked · t: destination"),
        Source::Unstaged => format!("[UNSTAGED HUNKS] {sel}/{total} selected · Enter: absorb"),
        _ => format!("[STAGED HUNKS] {sel}/{total} selected · Enter: absorb"),
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
    let visible = app.visible_files();
    let items: Vec<ListItem> = visible.iter().map(|p| ListItem::new(p.to_string())).collect();
    // `ops::mv` needs a clean tree — say so UP FRONT, not after Enter fails.
    let title = if app.tree_dirty {
        "[MOVE] needs a clean tree — commit or stash your staged changes first".to_string()
    } else {
        format!("[MOVE] {}{} file{} → {dest} · /: filter · t: dest", visible.len(),
            if app.filter.is_empty() { String::new() } else { format!(" of {}", app.move_files.len()) },
            if visible.len() == 1 { "" } else { "s" })
    };
    let list = List::new(items)
        .block(list_block(&title, app.focus == Pane::Right))
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !visible.is_empty() {
        state.select(Some(app.move_cursor.min(visible.len() - 1)));
    }
    f.render_stateful_widget(list, area, &mut state);
}

/// The one line of keymap that is always on screen. Everything else moved into
/// the `?` popup: three permanent keymap lines cost three of the twenty-four rows
/// a small terminal has, and they were the widest thing here — the source of the
/// 80-column clipping this screen shipped twice. A single line of five keys can't
/// clip, and the full reference is one keystroke away.
const HINT: &str = "↑↓ nav · ←→ pane · ⏎ apply · ? help · q quit";

/// Contextual help for the CURRENT screen: a title, one line saying what this
/// screen is for, and only the verbs that actually do something here.
///
/// Scoping to (focus, source) is the same structural argument the old line-2
/// keymap made, with the room to say what a key MEANS rather than just naming it:
/// one flat list of every verb across eleven operations is unreadable, and on this
/// screen most of them would be lies — `d drop` does nothing in the hunk pane.
/// Keys ≤ 5 columns and meanings ≤ 52 keep every row inside the popup unwrapped.
fn help(app: &App) -> (&'static str, &'static str, Vec<(&'static str, &'static str)>) {
    let (title, lead, mut rows) = match (app.focus, app.source) {
        // The phantom row is NOT a commit, so every commit verb refuses there —
        // listing them would be exactly the lie this scoping exists to prevent.
        (Pane::Commits, _) if app.on_phantom() => (
            "+ new commit here",
            "A destination that does not exist yet — split hunks into it.",
            vec![
                ("t", "route the picked hunks into a new commit"),
                ("⏎", "name it and apply the split"),
                ("↑↓", "back down to the real commits"),
            ],
        ),
        (Pane::Commits, _) => (
            "Commits",
            "The stack, newest first — edit one, or send hunks to it.",
            vec![
                ("e", "open this commit's hunks, to take some out"),
                ("w", "open your UNSTAGED work as hunks to route"),
                ("t", "make this commit the destination"),
                ("f", "send every picked hunk here at once"),
                ("[ ]", "move this commit earlier / later"),
                ("d", "drop this commit"),
                ("s", "squash it into the one below"),
                ("r", "reword its message"),
            ],
        ),
        (Pane::Right, Source::Files) => (
            "Files",
            "Whole-file move: pick a file, then a commit to move it to.",
            vec![
                ("↑↓", "pick a file"),
                ("/", "filter the list — applies as you type"),
                ("Tab", "cross to the commit list"),
                ("t", "move the file into the commit there"),
                ("m", "back to hunks"),
            ],
        ),
        (Pane::Right, Source::Staged) => (
            "Staged hunks",
            "Your uncommitted work — route each hunk into the stack.",
            vec![
                ("Spc", "pick / unpick this hunk"),
                ("a", "accept the target git blame inferred"),
                ("Tab", "cross to the commits, then t to send them"),
                ("m", "switch to whole-file move"),
            ],
        ),
        (Pane::Right, Source::Unstaged) => (
            "Unstaged hunks",
            "Work you never staged — fold it straight into the stack.",
            vec![
                ("Spc", "pick / unpick this hunk"),
                ("a", "accept the target git blame inferred"),
                ("Tab", "cross to the commits, then t to send them"),
                ("w", "back to your staged work"),
            ],
        ),
        (Pane::Right, Source::Commit(_)) => (
            "Commit hunks",
            "Hunks taken OUT of this commit — send them elsewhere.",
            vec![
                ("Spc", "pick / unpick this hunk"),
                ("a", "accept the target git blame inferred"),
                ("Tab", "cross to the commits, then t to send them"),
                ("t", "on `+ new commit here` splits them off instead"),
                ("Esc", "close this commit, back to your staged work"),
            ],
        ),
    };
    rows.extend([
        ("", ""),
        ("p", "preview — what would change, nothing written"),
        ("⏎", "apply (press twice; the first press reports scope)"),
        ("u", "undo the last transplant"),
        ("c / i", "conflict rule · ignore whitespace"),
        ("Esc", "step back · q quit"),
    ]);
    (title, lead, rows)
}

/// The `?` overlay. Transient: any key dismisses it, so it can never be the
/// thing that swallowed a keystroke you meant for the stack.
fn render_help(f: &mut Frame, app: &App, area: Rect) {
    let (title, lead, rows) = help(app);
    let mut text = vec![Line::from(Span::styled(lead, theme::dim())), Line::from("")];
    text.extend(rows.iter().map(|(k, m)| {
        Line::from(vec![
            Span::styled(format!("{k:>5}  "), theme::path()),
            Span::raw(*m),
        ])
    }));
    // Centred, and clamped so it still fits when the terminal is smaller than the
    // help is long — the popup must never be the thing that overflows.
    let w = 64.min(area.width.saturating_sub(2));
    let h = (text.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect); // or the panes bleed through
    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} — any key closes "))
                .border_style(theme::border(true)),
        ),
        rect,
    );
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let dim = theme::dim();
    let status_style = theme::status(&app.status, app.pending_apply);
    let mut text = vec![Line::from(Span::styled(HINT, dim))];
    // Always show what the cursor is on, so keys never act on hidden state.
    text.push(Line::from(Span::styled(app.context_line(), dim)));
    // The prompt takes over the STATUS line only — everything above stays put.
    text.push(match &app.input {
        Some(p) => Line::from(vec![
            Span::styled(format!("{}: ", p.label), theme::path()),
            Span::raw(format!("{}▏", p.text)),
            Span::styled("   ⏎ ok · Esc cancel", dim),
        ]),
        None => Line::from(Span::styled(app.status.clone(), status_style)),
    });
    // WRAP, in four rows rather than three. Engine conflict messages are long by
    // design — they name the commit that owns the lines and the command to retry
    // with — and an unwrapped Paragraph clipped that hint off at 80 columns,
    // throwing away the best diagnostic the tool has.
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

/// Display name of a conflict rule, matching the CLI flag that sets it.
fn favor_name(f: git2::FileFavor) -> &'static str {
    match f {
        git2::FileFavor::Ours => "ours",
        git2::FileFavor::Theirs => "theirs",
        git2::FileFavor::Union => "union",
        _ => "normal",
    }
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
            .map(|i| CommitRow {
                oid: oid(i as u8 + 1),
                summary: format!("c{i}"),
                diff: None,
                refs: Vec::new(),
            })
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
            focus: Pane::Commits,
            commit_cursor: 0,
            hunk_cursor: 0,
            move_cursor: 0,
            move_target: None,
            status: String::new(),
            applied: false,
            pending_apply: false,
            pending_quit: false,
            filter: String::new(),
            tree_dirty: false,
            skipped: Vec::new(),
            diff_scroll: 0,
            source: Source::Staged,
            source_base: oid(9),
            shape: Shape::None,
            input: None,
            phantom_cursor: false,
            help: false,
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
            let before = (a.commit_cursor, a.hunk_cursor, a.focus, a.source, a.files[0].selected.clone(), a.files[0].targets.clone());
            assert_eq!(on_key(&mut a, KeyCode::Char(k)), Flow::Continue);
            let after = (a.commit_cursor, a.hunk_cursor, a.focus, a.source, a.files[0].selected.clone(), a.files[0].targets.clone());
            assert_eq!(before, after, "'{k}' must be inert — no nav, no source/selection change");
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
    fn m_switches_the_source_to_the_file_list() {
        let mut a = app(2, 1);
        on_key(&mut a, KeyCode::Char('m'));
        assert_eq!(a.source, Source::Files);
        assert_eq!(a.focus, Pane::Right, "the file list is what you're picking from");
        on_key(&mut a, KeyCode::Down);
        assert_eq!(a.move_cursor, 1);
        // `m` again leaves it — like any other non-staged source, that reloads.
        assert_eq!(on_key(&mut a, KeyCode::Char('m')), Flow::ResetSource);
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
        on_key(&mut a, KeyCode::Char('a')); // a = accept inference (absorb)
        assert_eq!(a.files[0].targets, vec![Some(oid(1)), Some(oid(1))]);
    }

    #[test]
    fn file_source_sets_destination_with_t() {
        let mut a = app(3, 1);
        on_key(&mut a, KeyCode::Char('m'));
        on_key(&mut a, KeyCode::Tab); // back to the commit list
        on_key(&mut a, KeyCode::Down); // commit 1
        on_key(&mut a, KeyCode::Char('t'));
        assert_eq!(a.move_target, Some(oid(2)));
    }

    /// The renames that removed the tool's only shift key. `S` is now nothing,
    /// `s` squashes (like `rebase -i`), `e` opens a commit's hunks, `a` accepts
    /// inference, and `r` is free for reword.
    #[test]
    fn renamed_keys_do_what_their_rebase_i_letters_say() {
        let mut a = app(3, 1);
        on_key(&mut a, KeyCode::Down);
        let oid1 = a.commits[1].oid;
        on_key(&mut a, KeyCode::Char('s'));
        assert_eq!(a.shape, Shape::Squash(oid1), "lowercase s squashes");
        on_key(&mut a, KeyCode::Esc);
        assert_eq!(on_key(&mut a, KeyCode::Char('e')), Flow::OpenCommit, "e opens hunks");
        assert_eq!(on_key(&mut a, KeyCode::Char('u')), Flow::Undo, "u undoes");

        // and the retired bindings are inert
        let mut b = app(3, 1);
        on_key(&mut b, KeyCode::Char('S'));
        assert_eq!(b.shape, Shape::None, "no shift key survives");
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
        // Diffs are read on demand, not at load: only the cursor row is rendered.
        assert!(app.commits.iter().all(|c| c.diff.is_none()), "load reads no diffs");
        ensure_diff(&mut app, &f.repo);
        let cur = app.commits[app.commit_cursor].diff.as_ref().expect("cursor row filled");
        assert!(cur.iter().any(|(o, t)| *o == '+' && t.contains("extra")), "and it is the right diff");
        assert!(app.commits[1..].iter().all(|c| c.diff.is_none()), "the others stay unread");
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
        // deepest: the file source, right pane focused
        on_key(&mut a, KeyCode::Char('m'));
        assert_eq!((a.source, a.focus), (Source::Files, Pane::Right));

        on_key(&mut a, KeyCode::Esc); // right pane -> commit list
        assert_eq!(a.focus, Pane::Commits);
        assert_eq!(a.source, Source::Files, "the source is not skipped");

        // leaving a non-staged source reloads, so it goes through the driver
        assert_eq!(on_key(&mut a, KeyCode::Esc), Flow::ResetSource);
        a.source = Source::Staged; // what reset_to_staged would produce

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
        assert!(text.contains("git add"), "empty pane teaches folding unstaged work");
        assert!(text.contains("Press w"), "and points at the key that does it");
    }

    /// Enter reports "rewrite N commit(s)" and the natural reply is to go and
    /// look at those N. Navigation cannot change what would apply — the recipe
    /// comes from `targets`, the shape from `shape`, the move from `move_target`,
    /// none of which read a cursor — so checking must not silently disarm.
    /// Anything that EDITS the plan still does.
    #[test]
    fn a_pending_apply_survives_looking_but_not_editing() {
        for k in [KeyCode::Down, KeyCode::Up, KeyCode::Tab, KeyCode::Home, KeyCode::End, KeyCode::PageDown] {
            let mut a = app(3, 2);
            a.pending_apply = true;
            on_key(&mut a, k);
            assert!(a.pending_apply, "{k:?} only looks — the arm must survive");
        }
        for k in [KeyCode::Char('t'), KeyCode::Char(' '), KeyCode::Char('d'), KeyCode::Char('a')] {
            let mut a = app(3, 2);
            a.pending_apply = true;
            on_key(&mut a, k);
            assert!(!a.pending_apply, "{k:?} edits the plan — the arm must drop");
        }
    }

    /// The routing is the one thing here with no undo: the engine's aborts are
    /// byte-identical, but a plan lost to a stray `q` is just gone.
    #[test]
    fn quit_confirms_only_when_there_is_routing_to_lose() {
        let mut a = app(3, 2);
        assert_eq!(on_key(&mut a, KeyCode::Char('q')), Flow::Quit, "a fresh view quits instantly");

        let mut a = app(3, 2);
        on_key(&mut a, KeyCode::Down); // commit 1
        on_key(&mut a, KeyCode::Char('t')); // hand-route the cursor hunk there
        assert_eq!(on_key(&mut a, KeyCode::Char('q')), Flow::Continue, "first q warns");
        assert!(a.status.contains("q again"), "and says what would be lost: {}", a.status);
        assert_eq!(on_key(&mut a, KeyCode::Char('q')), Flow::Quit, "second q quits");

        // and the warning does not persist across an unrelated key
        let mut a = app(3, 2);
        on_key(&mut a, KeyCode::Down);
        on_key(&mut a, KeyCode::Char('t'));
        on_key(&mut a, KeyCode::Char('q'));
        on_key(&mut a, KeyCode::Up);
        assert_eq!(on_key(&mut a, KeyCode::Char('q')), Flow::Continue, "the confirm re-arms");
    }

    // ── rendering tests (drive the actual TUI via TestBackend) ──

    #[test]
    fn renders_commit_list_and_selected_commit_diff() {
        let f = staged_fixture("render-browse");
        let mut app = load(&f.repo, None, Default::default()).unwrap(); // default focus = Commits
        ensure_diff(&mut app, &f.repo); // the event loop does this before every draw
        let text = render_to_text(&app);
        assert!(text.contains("commits"), "commit list pane rendered");
        assert!(text.contains("[DIFF]"), "commit-diff pane shown while browsing");
        // the selected (newest) commit c2 introduced `extra` — its diff is visible
        assert!(text.contains("extra"), "browsing a commit shows its diff (regression)");
    }

    /// Rewriting a stack carries every local branch in the range with it
    /// (`ops::restack`). Which ones those are must be visible BEFORE the run,
    /// not reported by the status line once it is done.
    #[test]
    fn commit_rows_show_the_branches_a_rewrite_would_carry() {
        let f = staged_fixture("render-refs");
        let head = f.repo.head().unwrap().peel_to_commit().unwrap();
        f.repo.branch("pr-2", &head.parent(0).unwrap(), false).unwrap();
        let app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.commits[1].refs.contains(&"pr-2".to_string()), "the sibling branch is on its row");
        assert!(render_at(&app, 100, 30).contains("(pr-2)"), "and it is drawn");
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
    fn file_source_warns_up_front_when_tree_is_dirty() {
        let f = staged_fixture("ux-move-dirty"); // fixture has a staged change
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.tree_dirty, "fixture is dirty");
        on_key(&mut app, KeyCode::Char('m'));
        let text = render_at(&app, 100, 30);
        assert!(text.contains("clean tree"), "the file list says so before you press Enter");
    }

    /// `?` help is scoped to the FOCUSED PANE. That is the invariant that keeps it
    /// honest AND short: one flat list of every verb across eleven operations both
    /// overflows the popup and lies, since most of those keys no-op on any given
    /// screen — `d drop` does nothing in the hunk pane.
    #[test]
    fn help_is_focus_aware() {
        let f = staged_fixture("ux-help");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        let (commit_title, _, commits) = help(&app);
        on_key(&mut app, KeyCode::Tab); // focus the hunk pane
        let (hunk_title, _, hunks) = help(&app);
        assert_ne!(commit_title, hunk_title, "the two panes are different screens");
        let has = |rows: &[(&str, &str)], m: &str| rows.iter().any(|(_, v)| v.contains(m));
        assert!(has(&commits, "drop this commit"), "commit pane owns the shape verbs");
        assert!(!has(&hunks, "drop this commit"), "hunk pane drops keys that no-op there");
        assert!(has(&hunks, "pick / unpick"), "hunk pane shows its own keys");
        // and it is what is actually on screen, not just what the helper returns
        app.help = true;
        assert!(render_to_text(&app).contains("pick / unpick"));
    }

    #[test]
    fn help_rows_fit_the_popup_at_80_columns() {
        // Rows aren't wrapped, so an over-long one silently drops its tail. Check
        // the strings, not the render: clipping is what a render would hide. The
        // popup is 64 wide, so 2 border + 5 key + 2 gap leaves 55 for the meaning.
        let f = staged_fixture("ux-help-width");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        assert!(HINT.chars().count() <= 80, "the always-on hint must fit 80 cols");
        for focus in [Pane::Commits, Pane::Right] {
            for source in [Source::Staged, Source::Commit(app.head), Source::Files] {
                for phantom in [false, true] {
                    app.focus = focus;
                    app.source = source;
                    app.phantom_cursor = phantom;
                    let (_, lead, rows) = help(&app);
                    assert!(lead.chars().count() <= 62, "{focus:?}/{source:?} lead too wide: {lead}");
                    for (k, m) in rows {
                        assert!(k.chars().count() <= 5, "key column overflows: {k}");
                        assert!(m.chars().count() <= 55, "{focus:?}/{source:?} row too wide: {m}");
                    }
                }
            }
        }
    }

    /// The phantom row is a destination, not a commit, so every commit verb
    /// refuses there — offering them would be exactly the lie the scoping exists
    /// to prevent, on the one screen where it is easiest to make.
    #[test]
    fn help_on_the_phantom_row_is_not_the_commit_help() {
        let fx = two_hunk_commit_fixture("help-phantom");
        let mut app = load(&fx.repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, &fx.repo);
        on_key(&mut app, KeyCode::Tab); // to the commit list
        on_key(&mut app, KeyCode::Home); // the phantom sits above the newest
        assert!(app.on_phantom(), "fixture precondition");
        let (title, _, rows) = help(&app);
        assert_eq!(title, "+ new commit here");
        assert!(
            !rows.iter().any(|(_, m)| m.contains("drop this commit")),
            "commit verbs must not be offered on a row that is not a commit"
        );
    }

    /// The popup is the one widget that can want more rows than the screen has,
    /// and ratatui CLIPS rather than complains — so a too-long help silently loses
    /// its bottom rows. Assert the last row is really on screen at the sizes we
    /// support; this is what fails the day someone adds a sixth global key.
    #[test]
    fn help_is_not_clipped_at_the_sizes_we_support() {
        let f = staged_fixture("ux-help-small");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        app.help = true;
        for focus in [Pane::Commits, Pane::Right] {
            for source in [Source::Staged, Source::Commit(app.head), Source::Files] {
                app.focus = focus;
                app.source = source;
                let (_, _, rows) = help(&app);
                let last = rows.last().unwrap().1;
                for (w, h) in [(100, 30), (80, 24)] {
                    assert!(
                        render_at(&app, w, h).contains(last),
                        "{focus:?}/{source:?} help clipped at {w}x{h}: lost {last:?}"
                    );
                }
            }
        }
    }

    /// The structural one: the right column used to SWAP between the hunk list
    /// and the commit diff on focus, so Tab-ing over to choose a destination hid
    /// the hunk you were routing. Both must now be on screen in both focuses,
    /// at the smallest size we support.
    #[test]
    fn the_hunk_and_its_destination_are_both_on_screen_in_either_focus() {
        let f = staged_fixture("ux-covisible");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        for focus in ["commits", "right"] {
            let text = render_at(&app, 80, 24);
            assert!(text.contains("[STAGED HUNKS]"), "hunk list visible with {focus} focused: {text}");
            assert!(text.contains("[DIFF]"), "destination diff visible with {focus} focused: {text}");
            on_key(&mut app, KeyCode::Tab);
        }
    }

    /// `◀` marks where the CURSOR hunk goes. The counter says how many go there
    /// in total — without it a multi-hunk routing was only readable by walking
    /// every row, and the plan is the most valuable thing on the screen.
    #[test]
    fn commit_rows_carry_the_whole_routing_not_just_the_cursors() {
        let f = multi_hunk_fixture("ux-plan");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        let dest = app.commits[0].oid;
        on_key(&mut app, KeyCode::Tab); // hunk pane
        on_key(&mut app, KeyCode::Tab); // back to commits, cursor on the newest
        on_key(&mut app, KeyCode::Char('f')); // route EVERY picked hunk there
        let n = app.picked();
        assert!(n > 1, "fixture precondition: more than one hunk picked");
        assert_eq!(app.routing(), vec![(dest, n)], "the whole selection lands on one commit");
        // and it is on the row, at the narrowest width we support
        let text = render_at(&app, 80, 24);
        assert!(text.contains(&format!("{n:<2}{dest:.8}")), "the count sits in the row's gutter: {text}");
        // and `p` reports it rather than just a tip oid
        assert!(app.routing_summary().contains(&format!("{n} hunk(s) → {dest:.8}")), "{}", app.routing_summary());
    }

    /// Space is a HUNK verb. It used to check only the source, so pressing it
    /// from the commit list — where you start — flipped the other pane's cursor
    /// hunk instead of doing nothing.
    #[test]
    fn space_from_the_commit_pane_leaves_the_selection_alone() {
        let mut a = app(2, 2);
        assert_eq!(a.focus, Pane::Commits, "the TUI starts here");
        let before = a.files[0].selected.clone();
        on_key(&mut a, KeyCode::Char(' '));
        assert_eq!(a.files[0].selected, before, "no hidden toggle");
        assert!(a.status.contains("Tab"), "and it says why: {}", a.status);
    }

    /// The engine's conflict messages name the commit that owns the lines AND the
    /// command to retry with. Unwrapped, 80 columns kept the first clause and
    /// threw away the part that says what to do.
    #[test]
    fn a_long_conflict_message_survives_80_columns() {
        let f = staged_fixture("ux-wrap");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        app.status =
            "conflict while rewriting 1ab2bb72 in cfg.txt — 7b7de062 owns those lines; try 'fix 7b7de062' or 'absorb'"
                .into();
        let text = render_at(&app, 80, 24);
        assert!(text.contains("conflict while rewriting"), "the first clause: {text}");
        assert!(text.contains("fix 7b7de062"), "and the retarget hint, which is the actionable half: {text}");
    }

    #[test]
    fn the_hint_is_not_clipped_on_a_narrow_terminal() {
        let f = staged_fixture("ux-hint-narrow");
        let app = load(&f.repo, None, Default::default()).unwrap();
        let text = render_at(&app, 80, 24);
        assert!(text.contains("⏎ apply"), "the apply key must survive 80 cols");
        assert!(text.contains("? help"), "the way to find every other key must survive");
        assert!(text.contains("q quit"), "the quit key must survive 80 cols");
    }

    /// Help is a reader, not an actor: the key that dismisses it must not also do
    /// something to the stack. `d` on a commit would otherwise mark a drop.
    #[test]
    fn any_key_closes_help_and_does_nothing_else() {
        for k in [KeyCode::Char('d'), KeyCode::Char('?'), KeyCode::Esc, KeyCode::Enter, KeyCode::Down] {
            let mut a = app(3, 2);
            assert_eq!(on_key(&mut a, KeyCode::Char('?')), Flow::Continue);
            assert!(a.help, "? opens help");
            let before = (a.commit_cursor, a.focus, a.source, a.shape.clone(), a.pending_apply);
            assert_eq!(on_key(&mut a, k), Flow::Continue, "{k:?} must not act while help is up");
            assert!(!a.help, "{k:?} closes help");
            assert_eq!(before, (a.commit_cursor, a.focus, a.source, a.shape.clone(), a.pending_apply));
        }
    }

    /// The prompt outranks help: `?` is a legal character in a commit message.
    #[test]
    fn question_mark_types_into_an_open_prompt() {
        let mut a = app(3, 2);
        a.input = Some(Prompt { label: "reword", text: String::new(), what: Ask::Reword(oid(1)) });
        on_key(&mut a, KeyCode::Char('?'));
        assert!(!a.help, "no popup while typing");
        assert_eq!(a.input.as_ref().unwrap().text, "?");
    }

    /// The 30/70 split left the commit pane 24 cells at 80 columns, so an
    /// ordinary summary clipped at ~12 characters and the title was cut
    /// mid-word. `Min(32)` fixes the floor without starving the right pane.
    #[test]
    fn the_commit_pane_stays_readable_at_80_columns() {
        let f = staged_fixture("ux-narrow-split");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        for c in app.commits.iter_mut() {
            c.summary = "add the parser module".into(); // 21 chars
        }
        let narrow = render_at(&app, 80, 24);
        assert!(narrow.contains("add the parser modul"), "summary survives 80 cols: {narrow}");
        // the right pane is still usable: its hunk selector renders at 80 too
        on_key(&mut app, KeyCode::Tab);
        let hunks = render_at(&app, 80, 24);
        assert!(hunks.contains("[STAGED HUNKS]"), "right pane still works at 80 cols");
        assert!(render_at(&app, 100, 30).contains("[STAGED HUNKS]"), "and at 100");
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
    fn a_staged_add_folds_as_one_whole_file_hunk() {
        let f = added_file_fixture("ux-added");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        // An add is ONE whole-file hunk against an empty original. It used to be
        // reported and nothing else, which left "I created this file and it
        // belongs in commit 3" with no route at all — `move-file` cannot serve
        // it, because the file is not in HEAD's tree to be re-anchored.
        assert_eq!(app.flat.len(), 1, "the add is offered as a hunk");
        assert_eq!(app.files[0].path, "brand_new.rs");
        assert!(app.skipped.is_empty(), "and not filed away as unfoldable");

        // blame has nothing to say about a path with no history, so it starts
        // unrouted and the user picks — then it folds like anything else.
        assert_eq!(app.files[0].targets[0], None, "no inferred home for a new path");
        on_key(&mut app, KeyCode::Down); // the older commit — the fixture's c1
        on_key(&mut app, KeyCode::Char('t'));
        on_key(&mut app, KeyCode::Enter);
        app.execute(&f.repo);
        on_key(&mut app, KeyCode::Enter);
        app.execute(&f.repo);
        assert!(app.applied, "applied: {}", app.status);

        // The file now exists at the commit we chose, not only at the tip. Read it
        // off the REWRITTEN history: `execute` does not reload, so `app.commits`
        // still holds the pre-rewrite oids.
        let parent = f.repo.head().unwrap().peel_to_commit().unwrap().parent(0).unwrap();
        let blob = git::blob_at(&f.repo, &parent.tree().unwrap(), Path::new("brand_new.rs"));
        assert!(String::from_utf8_lossy(&blob).contains("fn added"), "the add landed at the older commit");
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


    /// Two commits: a 30-line base, then one commit making TWO distant edits to
    /// it — so its own diff is exactly two separately selectable hunks.
    fn two_hunk_commit_fixture(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("gt-tui-{tag}-{}", std::process::id()));
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
            repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &pr).unwrap();
        };
        let base: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        commit("c1 base", &base);
        let mut v: Vec<String> = base.split_inclusive('\n').map(String::from).collect();
        v[1] = "FIRST-EDIT\n".into();
        v[19] = "SECOND-EDIT\n".into();
        commit("c2 two unrelated edits", &v.concat());
        Fixture { dir, repo }
    }

    // ── move hunks OUT of a commit into another commit (op A) ──

    #[test]
    fn e_opens_the_selected_commits_hunks_as_the_source() {
        let f = multi_hunk_fixture("src-open");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        // cursor 0 = newest commit (c2, which added b.rs)
        let c2 = app.commits[0].oid;
        assert_eq!(on_key(&mut app, KeyCode::Char('e')), Flow::OpenCommit);
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
    fn e_again_returns_to_the_staged_view() {
        let f = multi_hunk_fixture("src-toggle");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, &f.repo);
        assert!(matches!(app.source, Source::Commit(_)));
        on_key(&mut app, KeyCode::Tab); // `e` only acts from the commit list
        open_commit_source(&mut app, &f.repo); // same commit again
        assert_eq!(app.source, Source::Staged, "toggles back to staged hunks");
    }

    #[test]
    fn move_one_hunk_of_two_out_of_a_commit_leaving_the_other() {
        // A genuine PARTIAL extraction: c2 edits two distant lines; move only the
        // first back to c1 and prove the second stayed behind.
        let fx = two_hunk_commit_fixture("partial");
        let repo = &fx.repo;
        let mut app = load(repo, None, Default::default()).unwrap();
        // `e` on the newest commit (c2) → its own two hunks
        open_commit_source(&mut app, repo);
        assert_eq!(app.flat.len(), 2, "fixture precondition: c2 has two separate hunks");

        // pick ONLY the first hunk, target the older commit
        on_key(&mut app, KeyCode::Char(' '));
        assert_eq!(app.files[0].selected, vec![true, false], "exactly one hunk picked");
        on_key(&mut app, KeyCode::Tab);
        on_key(&mut app, KeyCode::Down);
        on_key(&mut app, KeyCode::Char('t'));
        app.execute(repo); // arm
        app.execute(repo); // apply
        assert!(app.applied, "applied: {}", app.status);

        let tip = repo.head().unwrap().target().unwrap();
        let read = |c: &git2::Commit| {
            let b = c.tree().unwrap().get_path(Path::new("f.rs")).unwrap()
                .to_object(repo).unwrap().peel_to_blob().unwrap();
            String::from_utf8(b.content().to_vec()).unwrap()
        };
        let tipc = repo.find_commit(tip).unwrap();
        let c1p = tipc.parent(0).unwrap();
        let c1_txt = read(&c1p);
        assert!(c1_txt.contains("FIRST-EDIT"), "the picked hunk moved back to c1");
        assert!(!c1_txt.contains("SECOND-EDIT"), "the UNPICKED hunk did not follow it");
        let tip_txt = read(&tipc);
        assert!(tip_txt.contains("FIRST-EDIT") && tip_txt.contains("SECOND-EDIT"), "tip has both");
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

    /// The headline of this source: unstaged work folds into an old commit with
    /// no `git add` anywhere, and the repo is left in an honest state afterwards.
    ///
    /// The index assertion is the load-bearing one. `ops.rs` leaves the worktree
    /// alone because the rewritten tip's tree equals the index tree — fold an
    /// UNSTAGED hunk and that stops being true, so without `refresh_index` git
    /// reports a staged *reversal* of the change we just wrote into history.
    #[test]
    fn unstaged_work_folds_with_no_git_add_and_leaves_a_clean_status() {
        let f = staged_fixture("unstaged-e2e");
        // start from a clean tree, then edit the file WITHOUT staging it
        {
            let head = f.repo.head().unwrap().peel_to_commit().unwrap();
            f.repo.reset(head.as_object(), git2::ResetType::Hard, None).unwrap();
        }
        let text = std::fs::read_to_string(f.dir.join("f.rs")).unwrap();
        assert!(text.contains("l2\n"), "fixture precondition");
        std::fs::write(f.dir.join("f.rs"), text.replace("l2\n", "l2-FIXED\n")).unwrap();

        let mut app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.flat.is_empty(), "nothing is staged");
        open_unstaged_source(&mut app, &f.repo);
        assert_eq!(app.source, Source::Unstaged);
        assert_eq!(app.flat.len(), 1, "the unstaged hunk is offered");
        assert!(app.files[0].targets[0].is_some(), "and blame prefilled its target");

        let before = f.repo.head().unwrap().target().unwrap();
        on_key(&mut app, KeyCode::Enter); // arm
        app.execute(&f.repo);
        on_key(&mut app, KeyCode::Enter); // apply
        app.execute(&f.repo);
        assert!(app.applied, "applied: {}", app.status);
        assert_ne!(f.repo.head().unwrap().target().unwrap(), before, "the branch moved");

        // the change is in history now
        let tip = f.repo.head().unwrap().peel_to_commit().unwrap();
        let blob = git::blob_at(&f.repo, &tip.tree().unwrap(), Path::new("f.rs"));
        assert!(String::from_utf8_lossy(&blob).contains("l2-FIXED"), "folded into the stack");

        // and the worktree was never written, while the index kept up
        let on_disk = std::fs::read_to_string(f.dir.join("f.rs")).unwrap();
        assert!(on_disk.contains("l2-FIXED"), "worktree untouched");
        let mut o = git2::StatusOptions::new();
        o.include_untracked(false);
        let statuses = f.repo.statuses(Some(&mut o)).unwrap();
        assert!(
            statuses.iter().all(|s| !s.status().is_index_modified() && !s.status().is_index_deleted()),
            "no phantom STAGED reversal: {:?}",
            statuses.iter().map(|s| (s.path().map(String::from), s.status())).collect::<Vec<_>>()
        );
    }

    /// A path with BOTH staged and unstaged edits must contribute only its
    /// unstaged hunks — that is what parenting the synthetics at the index tree
    /// buys, since the staged part then sits in the merge base and cancels.
    #[test]
    fn the_unstaged_source_ignores_what_is_already_staged() {
        let f = staged_fixture("unstaged-mixed"); // f.rs already has a STAGED edit
        let text = std::fs::read_to_string(f.dir.join("f.rs")).unwrap();
        std::fs::write(f.dir.join("f.rs"), format!("{text}TRAILING\n")).unwrap();

        let mut app = load(&f.repo, None, Default::default()).unwrap();
        let staged_hunks = app.flat.len();
        assert!(staged_hunks > 0, "fixture precondition: something is staged");
        open_unstaged_source(&mut app, &f.repo);
        assert_eq!(app.flat.len(), 1, "only the unstaged hunk, not the staged one");
        let body: String =
            app.files[0].hunks[0].lines.iter().map(|(_, t)| t.as_str()).collect();
        assert!(body.contains("TRAILING"), "and it is the unstaged one: {body}");
    }

    /// `git commit -p` without leaving the screen: the phantom row on a staged or
    /// unstaged source means "a new commit at the tip". Nothing below HEAD moves,
    /// so there is no replay — the synthetic simply becomes the new tip.
    #[test]
    fn the_phantom_row_commits_picked_hunks_at_the_tip() {
        let f = staged_fixture("tip-commit");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        let before = f.repo.head().unwrap().target().unwrap();
        let height = app.commits.len();
        assert!(app.has_phantom(), "a staged source offers the new-commit row");
        assert!(app.phantom_is_tip(), "and it means the tip, not a split");

        on_key(&mut app, KeyCode::Home); // the phantom sits above the newest
        assert!(app.on_phantom());
        assert!(render_at(&app, 100, 30).contains("+ new commit at the tip"), "and says so");
        on_key(&mut app, KeyCode::Char('t')); // route the hunks there

        // preview is honest that nothing is rewritten
        app.preview(&f.repo);
        assert!(app.status.contains("nothing below it moves"), "{}", app.status);

        // Enter opens the message prompt — naming the commit IS the gate
        on_key(&mut app, KeyCode::Enter);
        app.execute(&f.repo);
        assert!(app.input.is_some(), "the prompt is the confirmation");
        for c in "add the fix".chars() {
            on_key(&mut app, KeyCode::Char(c));
        }
        assert_eq!(on_key(&mut app, KeyCode::Enter), Flow::Submit);
        submit_prompt(&mut app, &f.repo);

        let head = f.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.parent(0).unwrap().id(), before, "the old tip is its parent — nothing rewritten");
        assert_eq!(head.summary().unwrap(), "add the fix");
        assert_eq!(app.commits.len(), height + 1, "and the stack grew by one");
        let blob = git::blob_at(&f.repo, &head.tree().unwrap(), Path::new("f.rs"));
        assert!(String::from_utf8_lossy(&blob).contains("L2"), "carrying the picked hunk");
    }

    /// The file list is every blob in HEAD's tree. Without a filter `move-file`
    /// is unreachable by arrow key on any real repo. The cursor must follow the
    /// VISIBLE list — indexing the unfiltered one would move a different file
    /// than the one under the cursor, which is the worst failure a
    /// history-rewriting verb can have.
    #[test]
    fn the_file_filter_narrows_the_list_and_the_cursor_follows_it() {
        let mut a = app(3, 1);
        a.move_files = vec!["a.txt".into(), "src/b.rs".into(), "src/c.rs".into()];
        a.source = Source::Files;
        a.focus = Pane::Right;

        on_key(&mut a, KeyCode::Char('/'));
        assert!(a.input.is_some(), "/ opens the filter");
        for c in "src/".chars() {
            on_key(&mut a, KeyCode::Char(c));
        }
        assert_eq!(a.visible_files(), vec!["src/b.rs", "src/c.rs"], "applied as typed");
        on_key(&mut a, KeyCode::Enter); // put the prompt away, keep the filter
        submit_prompt_filter_only(&mut a);

        on_key(&mut a, KeyCode::Down);
        assert_eq!(a.visible_files()[a.move_cursor], "src/c.rs", "the cursor indexes what is shown");
        assert!(render_at(&a, 100, 30).contains("2 of 3 file"), "and the pane says it is narrowed");

        // Esc cancels the narrowing rather than leaving it silently applied
        on_key(&mut a, KeyCode::Char('/'));
        on_key(&mut a, KeyCode::Esc);
        assert_eq!(a.visible_files().len(), 3, "filter cleared");
    }

    /// `submit_prompt` needs a repo; the filter arm does not touch one.
    fn submit_prompt_filter_only(app: &mut App) {
        app.input = None;
    }

    /// "Unstaged files" includes ones git has never seen. Ignored files stay out:
    /// `.gitignore` is already the user saying they are not part of this.
    #[test]
    fn an_untracked_file_is_offered_by_the_unstaged_source() {
        let f = staged_fixture("untracked");
        std::fs::write(f.dir.join(".gitignore"), "junk.log\n").unwrap();
        std::fs::write(f.dir.join("junk.log"), "noise\n").unwrap();
        std::fs::write(f.dir.join("new_mod.rs"), "pub fn hello() {}\n").unwrap();

        let mut app = load(&f.repo, None, Default::default()).unwrap();
        open_unstaged_source(&mut app, &f.repo);
        let paths: Vec<&str> = app.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"new_mod.rs"), "the untracked file is routable: {paths:?}");
        assert!(!paths.contains(&"junk.log"), "ignored files stay out: {paths:?}");
    }

    #[test]
    fn empty_pane_teaches_the_commit_to_commit_flow() {
        let f = stack4("empty-teach");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        assert!(app.status.contains("e on a commit"), "status points at `e`: {}", app.status);
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

    /// The two-step Enter is the last moment anything can be reconsidered, so it
    /// is where "this rewrite destroys N signatures" has to appear.
    #[test]
    fn arming_names_the_signatures_the_rewrite_would_destroy() {
        let f = stack4("gpg-arm");
        // Re-make the tip as a SIGNED commit (fake signature text — nothing here
        // verifies it) so the stack holds exactly one signature.
        let name = {
            let head = f.repo.head().unwrap();
            let name = head.name().unwrap().to_string();
            let c = head.peel_to_commit().unwrap();
            let p = c.parent(0).unwrap();
            let sig = f.repo.signature().unwrap();
            let buf = f
                .repo
                .commit_create_buffer(&sig, &sig, "c4", &c.tree().unwrap(), &[&p])
                .unwrap();
            let signed = f
                .repo
                .commit_signed(
                    std::str::from_utf8(&buf).unwrap(),
                    "-----BEGIN PGP SIGNATURE-----\n\nnope\n-----END PGP SIGNATURE-----",
                    None,
                )
                .unwrap();
            f.repo.reference(&name, signed, true, "sign the tip").unwrap();
            name
        };
        assert!(f.repo.refname_to_id(&name).is_ok());

        let mut app = load(&f.repo, None, Default::default()).unwrap();
        app.focus = Pane::Commits;
        app.commit_cursor = 1; // c3 — dropping it replays the signed tip
        app.mark_shape(true);
        app.execute(&f.repo); // arm
        assert!(app.pending_apply, "{}", app.status);
        assert!(
            app.status.contains("1 GPG signature(s) will be LOST"),
            "the arming line must name the loss, got {}",
            app.status
        );
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
    fn e_is_ignored_outside_the_commit_list() {
        let f = multi_hunk_fixture("s-gate");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        on_key(&mut app, KeyCode::Tab); // focus the hunk pane
        on_key(&mut app, KeyCode::Char(' ')); // change the selection (staged starts all-on)
        let before = app.files[0].selected.clone();
        open_commit_source(&mut app, &f.repo); // must NOT reload and discard state
        assert_eq!(app.source, Source::Staged, "source unchanged from the wrong pane");
        assert_eq!(app.files[0].selected, before, "selection survives a mis-pressed `e`");
    }

    #[test]
    fn accept_inference_does_not_wipe_destinations_in_commit_source() {
        let f = multi_hunk_fixture("r-guard");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, &f.repo);
        let dest = app.commits[1].oid;
        app.files[0].targets[0] = Some(dest);
        on_key(&mut app, KeyCode::Char('a'));
        assert_eq!(app.files[0].targets[0], Some(dest), "a must not clear a picked destination");
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
        on_key(&mut app, KeyCode::Char('m')); // the file source
        let text = render_to_text(&app);
        assert!(text.contains("[MOVE]"), "the file list is shown for Source::Files");
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
        on_key(&mut a, KeyCode::Char('s'));
        assert_eq!(a.shape, Shape::Squash(oid), "a second mark replaces the first");
        on_key(&mut a, KeyCode::Esc);
        assert_eq!(a.shape, Shape::None);
    }

    #[test]
    fn shape_keys_are_inert_outside_the_commit_pane() {
        let mut a = app(3, 2);
        on_key(&mut a, KeyCode::Tab); // focus the hunk pane
        let order: Vec<Oid> = a.commits.iter().map(|c| c.oid).collect();
        for k in ['[', ']', 'd', 's'] {
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
    /// Three commits that rewrite the SAME line, so the middle one cannot be
    /// dropped without a real conflict.
    fn conflicting_fixture(tag: &str) -> Fixture {
        let f = staged_fixture(tag);
        for (text, msg) in [("one\n", "c3"), ("two\n", "c4"), ("three\n", "c5")] {
            std::fs::write(f.dir.join("f.rs"), text).unwrap();
            let mut idx = f.repo.index().unwrap();
            idx.add_path(Path::new("f.rs")).unwrap();
            idx.write().unwrap();
            let tree = f.repo.find_tree(idx.write_tree().unwrap()).unwrap();
            let sig = f.repo.signature().unwrap();
            let head = f.repo.head().unwrap().peel_to_commit().unwrap();
            f.repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&head]).unwrap();
        }
        f
    }

    #[test]
    fn a_conflict_rule_reaches_the_tui() {
        let f = conflicting_fixture("favor");

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

    // ── the TUI stops being a one-shot picker ──

    /// Applying used to `break` the event loop, so you could never see the stack
    /// you had just produced — and `u` would have had no screen to undo on.
    #[test]
    fn applying_reloads_in_place_instead_of_quitting() {
        let f = staged_fixture("reload");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        let before = f.repo.head().unwrap().target().unwrap();
        app.commit_cursor = 1;
        app.execute(&f.repo); // arm
        app.execute(&f.repo); // apply
        assert!(app.applied, "{}", app.status);

        let done = std::mem::take(&mut app.status);
        reload(&mut app, &f.repo, done.clone());
        let after = f.repo.head().unwrap().target().unwrap();
        assert_ne!(after, before);
        assert_eq!(app.head, after, "the reloaded screen shows the NEW tip");
        assert!(!app.applied, "and is ready for the next edit");
        assert_eq!(app.commit_cursor, 1, "your place in the list is kept");
        assert_eq!(app.status, done, "the verdict survives the reload");
    }

    /// `u` is deliberately NOT behind the two-step gate: it moves the ref and
    /// only the ref, so it cannot lose work, and it is its own redo.
    #[test]
    fn u_undoes_the_last_transplant_with_no_gate() {
        let f = staged_fixture("tui-undo");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        let before = f.repo.head().unwrap().target().unwrap();
        app.execute(&f.repo);
        app.execute(&f.repo);
        assert!(app.applied, "{}", app.status);
        let rewritten = f.repo.head().unwrap().target().unwrap();
        reload(&mut app, &f.repo, String::new());

        assert_eq!(on_key(&mut app, KeyCode::Char('u')), Flow::Undo, "one key, no arming");
        undo(&mut app, &f.repo);
        assert_eq!(f.repo.head().unwrap().target().unwrap(), before, "{}", app.status);
        assert!(app.status.contains("undone"), "{}", app.status);
        assert_eq!(app.head, before, "the screen reloaded onto the restored tip");

        // ...and again is the redo, because the undo wrote its own reflog entry.
        undo(&mut app, &f.repo);
        assert_eq!(f.repo.head().unwrap().target().unwrap(), rewritten, "{}", app.status);
    }

    /// `c` cycles abort → ours → theirs → union and re-previews. The badge is
    /// sticky on the context line: a merge rule you cannot see decides silently
    /// what a conflict becomes.
    #[test]
    fn c_cycles_the_conflict_rule_and_changes_a_previews_outcome() {
        let f = conflicting_fixture("tui-cycle");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        app.commit_cursor = 1; // the middle of three commits touching one line
        on_key(&mut app, KeyCode::Char('d'));
        app.preview(&f.repo);
        assert!(app.status.starts_with("conflict"), "abort is the default: {}", app.status);
        assert!(!app.context_line().contains("rule:"), "no badge at the default");

        // c → ours, and the same drop now goes through
        assert_eq!(on_key(&mut app, KeyCode::Char('c')), Flow::Preview, "c re-previews");
        assert_eq!(app.opts.favor, Some(git2::FileFavor::Ours));
        app.preview(&f.repo);
        assert!(app.status.starts_with("clean, would move"), "{}", app.status);
        assert!(app.context_line().contains("rule:ours"), "{}", app.context_line());
        assert!(render_at(&app, 80, 24).contains("rule:ours"), "the badge is on screen");

        for want in ["theirs", "union"] {
            on_key(&mut app, KeyCode::Char('c'));
            assert!(app.context_line().contains(want), "{}", app.context_line());
        }
        on_key(&mut app, KeyCode::Char('c'));
        assert_eq!(app.opts.favor, None, "cycles back to abort");
        assert!(!app.context_line().contains("rule:"), "and the badge goes away");
    }

    // ── the inline prompt (`r` = reword) ──

    /// The prompt must swallow EVERY key. `q` quitting or Enter applying while
    /// you type a commit message is the whole failure mode a modal exists to
    /// prevent — this one is a branch at the top of `on_key` instead.
    #[test]
    fn the_prompt_swallows_every_key() {
        let mut a = app(3, 2);
        a.pending_apply = true;
        on_key(&mut a, KeyCode::Char('r'));
        assert!(a.input.is_some(), "r opens the prompt");
        assert!(!a.pending_apply, "r itself is a stray key, so it does cancel the arming");
        a.pending_apply = true; // but typing INSIDE the prompt must not

        for k in ['q', 'p', 'd', 's', 'm', ' '] {
            assert_eq!(on_key(&mut a, KeyCode::Char(k)), Flow::Continue, "'{k}' must not act");
        }
        assert_eq!(a.input.as_ref().unwrap().text, "c0qpdsm ", "the keys were typed, not obeyed");
        assert_eq!(a.shape, Shape::None, "no shape edit was made while typing");
        assert!(a.pending_apply, "typing must not trip the any-key cancel");

        on_key(&mut a, KeyCode::Backspace);
        assert_eq!(a.input.as_ref().unwrap().text, "c0qpdsm");
        assert_eq!(on_key(&mut a, KeyCode::Enter), Flow::Submit, "Enter submits, not applies");

        // Esc closes it without doing anything
        on_key(&mut a, KeyCode::Char('r'));
        on_key(&mut a, KeyCode::Esc);
        assert!(a.input.is_none());
        assert_eq!(on_key(&mut a, KeyCode::Char('q')), Flow::Quit, "keys work again after");
    }

    #[test]
    fn r_rewords_the_commit_under_the_cursor_and_keeps_its_body() {
        let f = staged_fixture("tui-reword");
        // give the newest commit a body, which the prompt must not eat
        {
            let head = f.repo.head().unwrap().peel_to_commit().unwrap();
            let sig = f.repo.signature().unwrap();
            head.amend(Some("HEAD"), Some(&sig), Some(&sig), None, Some("c2\n\nwhy it exists\n"), None)
                .unwrap();
        }
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        on_key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.input.as_ref().unwrap().text, "c2", "prefilled with the summary");
        assert!(render_at(&app, 80, 24).contains("message: c2▏"), "the prompt is the status line");

        for c in " renamed".chars() {
            on_key(&mut app, KeyCode::Char(c));
        }
        assert_eq!(on_key(&mut app, KeyCode::Enter), Flow::Submit);
        submit_prompt(&mut app, &f.repo);
        assert!(app.status.contains("reworded"), "{}", app.status);

        let head = f.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.summary(), Some("c2 renamed"));
        assert_eq!(head.message(), Some("c2 renamed\n\nwhy it exists\n"), "the body survived");
        assert_eq!(app.commits[0].summary, "c2 renamed", "and the screen reloaded onto it");
    }

    // ── hunk-granular split via the phantom row ──

    /// The phantom is a render-and-cursor concept ONLY. It must never enter
    /// `commits`: `commit_cursor` indexes that vector everywhere, three helpers
    /// map an Oid back to a stack position, and `swap()` drives reorder.
    #[test]
    fn the_phantom_row_is_never_a_member_of_the_commit_list() {
        let f = multi_hunk_fixture("phantom-not-in-list");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        let before: Vec<Oid> = app.commits.iter().map(|c| c.oid).collect();
        open_commit_source(&mut app, &f.repo);
        assert!(app.has_phantom());
        assert_eq!(app.commits.iter().map(|c| c.oid).collect::<Vec<_>>(), before);
        assert!(!app.commits.iter().any(|c| c.oid == phantom()));

        // and the cursor still means the same thing it always did
        on_key(&mut app, KeyCode::Tab); // to the commit list
        on_key(&mut app, KeyCode::Home);
        assert!(app.on_phantom(), "Home lands on the phantom, above the newest");
        assert_eq!(app.commit_cursor, 0);
        on_key(&mut app, KeyCode::Down);
        assert!(!app.on_phantom());
        assert_eq!(app.cursor_commit(), Some(before[0]), "Down lands on the NEWEST commit");
        on_key(&mut app, KeyCode::Down);
        assert_eq!(app.cursor_commit(), Some(before[1]));
        on_key(&mut app, KeyCode::Up);
        on_key(&mut app, KeyCode::Up);
        assert!(app.on_phantom(), "Up walks back onto it");
        on_key(&mut app, KeyCode::Up);
        assert!(app.on_phantom(), "and stops there");
    }

    /// Commit verbs refuse on the phantom row — it is a destination, not a commit.
    #[test]
    fn commit_verbs_are_inert_on_the_phantom_row() {
        let f = multi_hunk_fixture("phantom-inert");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, &f.repo);
        on_key(&mut app, KeyCode::Tab);
        on_key(&mut app, KeyCode::Home);
        assert!(app.on_phantom());
        let order: Vec<Oid> = app.commits.iter().map(|c| c.oid).collect();
        for k in ['d', 's', 'r', '[', ']'] {
            on_key(&mut app, KeyCode::Char(k));
            assert_eq!(app.shape, Shape::None, "'{k}' must not reshape from the phantom row");
            assert!(app.input.is_none(), "'{k}' must not prompt from the phantom row");
        }
        assert_eq!(app.commits.iter().map(|c| c.oid).collect::<Vec<_>>(), order);
        open_commit_source(&mut app, &f.repo);
        assert!(app.status.contains("press e on a commit"), "{}", app.status);
    }

    /// The whole point: pick SOME of a commit's hunks, send them to the phantom,
    /// and get two commits where there was one — with no new keys.
    #[test]
    fn t_on_the_phantom_row_splits_the_commit_by_hunk() {
        let fx = two_hunk_commit_fixture("hsplit");
        let repo = &fx.repo;
        let mut app = load(repo, None, Default::default()).unwrap();
        let c2 = app.commits[0].oid;
        open_commit_source(&mut app, repo);
        assert_eq!(app.flat.len(), 2, "fixture precondition: two separate hunks");
        assert!(render_at(&app, 100, 30).contains("⌁"), "the source commit is marked");

        // pick the FIRST hunk only, then route it to the phantom row
        on_key(&mut app, KeyCode::Char(' '));
        on_key(&mut app, KeyCode::Tab);
        on_key(&mut app, KeyCode::Home); // the phantom sits above the newest
        assert!(app.on_phantom());
        let screen = render_at(&app, 100, 30);
        assert!(screen.contains("+ new commit here"), "the phantom row is drawn: {screen}");
        on_key(&mut app, KeyCode::Char('t'));
        assert_eq!(app.files[0].targets[0], Some(phantom()));
        assert!(app.splits());

        // preview is the same engine call as apply, so it cannot disagree
        app.preview(repo);
        assert!(app.status.starts_with("clean, would move"), "{}", app.status);

        // Enter opens the message prompt, prefilled the way the CLI names it
        app.execute(repo);
        assert_eq!(app.input.as_ref().unwrap().text, "c2 two unrelated edits (part 1)");
        assert!(render_at(&app, 80, 24).contains("message: c2 two"), "the prompt is on screen");
        app.input.as_mut().unwrap().text.clear();
        for c in "extract the first edit".chars() {
            on_key(&mut app, KeyCode::Char(c));
        }
        assert_eq!(on_key(&mut app, KeyCode::Enter), Flow::Submit);
        submit_prompt(&mut app, repo);
        assert!(app.status.contains("split"), "{}", app.status);

        // two commits where there was one, split BY HUNK
        let read = |c: &git2::Commit| {
            let b = c.tree().unwrap().get_path(Path::new("f.rs")).unwrap()
                .to_object(repo).unwrap().peel_to_blob().unwrap();
            String::from_utf8(b.content().to_vec()).unwrap()
        };
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        let mid = tip.parent(0).unwrap();
        assert_eq!(tip.summary(), Some("c2 two unrelated edits"), "the source keeps its message");
        assert_eq!(mid.summary(), Some("extract the first edit"), "the new commit is named");
        assert_eq!(mid.parent(0).unwrap().summary(), Some("c1 base"), "inserted before the source");
        let mid_txt = read(&mid);
        assert!(mid_txt.contains("FIRST-EDIT"), "the routed hunk went into the new commit");
        assert!(!mid_txt.contains("SECOND-EDIT"), "the unrouted hunk did NOT");
        let tip_txt = read(&tip);
        assert!(tip_txt.contains("FIRST-EDIT") && tip_txt.contains("SECOND-EDIT"), "tip has both");
        assert_ne!(tip.id(), c2, "the source was rewritten, not left behind");
    }

    /// A mixed selection (some hunks split off, others absorbed elsewhere) is
    /// refused rather than half-applied. See the `ponytail:` note.
    #[test]
    fn a_split_takes_the_whole_selection_or_says_so() {
        let f = two_hunk_commit_fixture("phantom-mixed");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, &f.repo);
        app.files[0].selected[0] = true;
        app.files[0].targets[0] = Some(phantom());
        app.files[0].selected[1] = true;
        app.files[0].targets[1] = Some(app.commits[1].oid);
        app.execute(&f.repo);
        assert!(app.input.is_none(), "no prompt opens for a mix");
        assert!(app.status.contains("whole selection"), "{}", app.status);
    }

    /// Why the refusal above is allowed to stand: the outcome it declines is
    /// still reachable, in two applies that are each individually previewable and
    /// individually abortable. The refusal costs a keystroke, not a capability —
    /// which is the whole argument for not building the one-pass version.
    #[test]
    fn what_a_mixed_selection_wanted_is_reachable_in_two_applies() {
        let f = two_hunk_commit_fixture("phantom-two-pass");
        let repo = &f.repo;
        let original_tip_tree = repo.head().unwrap().peel_to_commit().unwrap().tree().unwrap().id();

        // Pass 1 — split the FIRST hunk off into a new commit.
        let mut app = load(repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, repo);
        app.files[0].selected[0] = true;
        app.files[0].targets[0] = Some(phantom());
        app.execute(repo); // opens the message prompt
        app.input.as_mut().unwrap().text = "extracted".into();
        submit_prompt(&mut app, repo);
        // The split path reloads in place instead of setting `applied`.
        assert!(app.status.starts_with("split "), "{}", app.status);

        // Pass 2 — move the remaining hunk back into c1.
        let mut app = load(repo, None, Default::default()).unwrap();
        open_commit_source(&mut app, repo); // the (rewritten) tip
        assert_eq!(app.flat.len(), 1, "only the un-split hunk is left in it");
        app.files[0].selected[0] = true;
        app.files[0].targets[0] = Some(app.commits[2].oid); // c1, two rows older
        app.execute(repo); // arm
        app.execute(repo); // apply
        assert!(app.applied, "{}", app.status);

        let read = |c: &git2::Commit| {
            let b = c.tree().unwrap().get_path(Path::new("f.rs")).unwrap()
                .to_object(repo).unwrap().peel_to_blob().unwrap();
            String::from_utf8(b.content().to_vec()).unwrap()
        };
        // Both of c2's hunks have left, so c2 itself empties and is dropped —
        // exactly what a one-pass mixed apply would have produced.
        let tip = repo.head().unwrap().peel_to_commit().unwrap();
        let bottom = tip.parent(0).unwrap();
        assert!(bottom.parent(0).is_err(), "two commits left: c1 and the split-off one");
        assert_eq!(tip.summary(), Some("extracted"), "the split-off commit");
        assert!(read(&tip).contains("FIRST-EDIT"), "carries the hunk routed to the phantom");
        assert!(read(&bottom).contains("SECOND-EDIT"), "and c1 carries the one routed to it");
        assert_eq!(
            tip.tree().unwrap().id(),
            original_tip_tree,
            "and the tip tree is byte-identical: the hunks were redistributed, not changed"
        );
    }

    /// c1 writes a line, c2 reindents it (whitespace only), and a value fix for
    /// that line is staged — so folding it back into c1 conflicts unless
    /// whitespace is ignored. The `--ignore-whitespace` case, in the TUI.
    fn reindent_fixture(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("gt-tui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        {
            let mut c = repo.config().unwrap();
            c.set_str("user.name", "t").unwrap();
            c.set_str("user.email", "t@t").unwrap();
        }
        let write = |body: &str| {
            std::fs::write(dir.join("f.rs"), body).unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(Path::new("f.rs")).unwrap();
            idx.write().unwrap();
        };
        for (msg, body) in [
            ("c1", "fn f() {\n    let x = 1;\n}\n"),
            ("c2 reindent", "fn f() {\n        let x = 1;\n}\n"),
        ] {
            write(body);
            let tree = {
                let mut idx = repo.index().unwrap();
                repo.find_tree(idx.write_tree().unwrap()).unwrap()
            };
            let sig = repo.signature().unwrap();
            let parents: Vec<_> = repo.head().ok().map(|h| h.peel_to_commit().unwrap()).into_iter().collect();
            let pr: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &pr).unwrap();
        }
        write("fn f() {\n        let x = 2;\n}\n");
        Fixture { dir, repo }
    }

    /// `i` is `--ignore-whitespace` live: it re-previews and shows beside the
    /// conflict rule, because a fold that only worked because whitespace was
    /// ignored is something you should be able to see you asked for.
    #[test]
    fn i_toggles_ignore_whitespace_and_shows_it_beside_the_rule() {
        let f = reindent_fixture("tui-ws");
        let mut app = load(&f.repo, None, Default::default()).unwrap();
        // fold it back into c1, past the commit that reindented the line
        on_key(&mut app, KeyCode::Down);
        on_key(&mut app, KeyCode::Char('f'));
        app.preview(&f.repo);
        assert!(app.status.starts_with("conflict"), "the reindent collides: {}", app.status);
        assert!(!app.context_line().contains("ignore-ws"), "no badge at the default");

        assert_eq!(on_key(&mut app, KeyCode::Char('i')), Flow::Preview, "i re-previews");
        assert!(app.opts.ignore_ws);
        app.preview(&f.repo);
        assert!(app.status.starts_with("clean, would move"), "{}", app.status);
        assert!(app.context_line().contains("ignore-ws"), "{}", app.context_line());
        assert!(render_at(&app, 80, 24).contains("ignore-ws"), "the badge is on screen");

        // and both badges sit together
        on_key(&mut app, KeyCode::Char('c'));
        assert!(app.context_line().contains("rule:ours · ignore-ws"), "{}", app.context_line());

        on_key(&mut app, KeyCode::Char('i'));
        assert!(!app.opts.ignore_ws, "it toggles back off");
        assert!(!app.context_line().contains("ignore-ws"));
    }

    #[test]
    fn body_of_takes_everything_after_the_blank_line() {
        assert_eq!(body_of("just a summary\n"), "");
        assert_eq!(body_of("summary\n\nbody line 1\nbody line 2\n"), "body line 1\nbody line 2");
    }




}
