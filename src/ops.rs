//! High-level operations: build a plan, replay, and (only on full success)
//! promote the branch ref. Kept in the library so tests can drive them against
//! a real repo and assert ref-level atomicity.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use git2::build::CheckoutBuilder;
use git2::{BranchType, Delta, Oid, Repository};

use crate::engine::{Edit, Recipe};
use crate::{engine, git, inference, patch, recipe};
use crate::{Error, Result};

/// Flags every rewriting operation takes. `Default` *is* the shipped default:
/// whitespace significant, not a dry run, siblings restacked.
#[derive(Debug, Default, Clone, Copy)]
pub struct Opts {
    /// Ignore whitespace in the replay's 3-way merges.
    pub ignore_ws: bool,
    /// Do everything except move refs.
    pub dry_run: bool,
    /// Leave sibling branches stranded on the orphaned range (warn, don't move).
    pub no_restack: bool,
}

#[derive(Debug)]
pub struct Outcome {
    pub branch: String,
    pub old_tip: Oid,
    pub new_tip: Oid,
    /// Sibling branches carried across the rewrite, as `name old -> new`.
    pub restacked: Vec<String>,
    /// Refs left pointing into the rewritten (now-orphaned) range.
    pub warnings: Vec<String>,
}

impl Outcome {
    /// The branch name to *print* (`main`, not `refs/heads/main`).
    pub fn short_branch(&self) -> &str {
        short_branch(&self.branch)
    }
}

/// Display form of a refname: everything after the last `/`. Shared by the CLI
/// and the TUI so both print the same thing.
pub fn short_branch(refname: &str) -> &str {
    refname.rsplit('/').next().unwrap_or(refname)
}

/// Result of an absorb: the replay outcome (None if nothing had a home), how many
/// hunks were folded, and how many were left in the worktree (no home).
#[derive(Debug)]
pub struct Absorbed {
    pub outcome: Option<Outcome>,
    pub folded: usize,
    pub orphans: usize,
    /// Where each folded hunk went: (path, `@@` header, target commit). The
    /// routing table `--dry-run` prints, in `hg absorb -n` shape.
    pub routes: Vec<(String, String, Oid)>,
}

/// op C — fold the staged change into `target_rev`. `opts.dry_run` does everything
/// but the ref moves, so the returned `Outcome` is what a real run would produce.
pub fn fix(repo: &Repository, target_rev: &str, opts: &Opts) -> Result<Outcome> {
    let branch = head_branch(repo)?;
    require_clean_unstaged(repo)?;

    let head = git::resolve(repo, "HEAD")?;
    let target = git::resolve(repo, target_rev)?;

    let staged_tree = repo.index()?.write_tree()?;
    if staged_tree == repo.find_commit(head)?.tree()?.id() {
        return Err(Error::NothingStaged);
    }

    let plan = recipe::fix(repo, target, head, staged_tree)?;
    let r = match engine::replay(repo, plan.base, plan.tip, &plan.recipe, opts.ignore_ws, false) {
        Ok(t) => t,
        // On conflict, enrich the error with the commit inference thinks owns the
        // changed lines — the target the fold would have gone to cleanly.
        Err(Error::Conflict { commit, path, .. }) => {
            let suggested = suggest_target(repo, head, target).unwrap_or(None);
            return Err(Error::Conflict { commit, path, suggested });
        }
        Err(e) => return Err(e),
    };
    let msg = format!("transplant: fix into {target:.8}");
    if !opts.dry_run {
        promote(repo, &branch, r.tip, head, &msg, true)?;
    }
    Ok(outcome(repo, branch, head, r, &msg, opts))
}

/// Promote the siblings and package the result. Called after the branch itself
/// has moved (or, on a dry run, hasn't).
fn outcome(
    repo: &Repository,
    branch: String,
    old_tip: Oid,
    r: engine::Replay,
    msg: &str,
    opts: &Opts,
) -> Outcome {
    let (restacked, warnings) = restack(repo, &r.map, &branch, msg, opts);
    Outcome { branch, old_tip, new_tip: r.tip, restacked, warnings }
}

/// Reflog message a restack writes, derived from the operation that caused it.
/// `undo` matches on this string to walk the sibling moves back too.
fn restack_msg(op: &str) -> String {
    format!("transplant: restack ({op})")
}

/// Carry every OTHER local branch whose tip is inside the rewritten range over to
/// its rewritten counterpart, through the same compare-and-swap [`promote`].
/// Returns `(restacked, warnings)`.
///
/// Three things are deliberately *not* moved:
/// - **Tags.** A tag names a specific historical commit — moving `v1.0` because
///   an unrelated branch was rewritten would silently redefine a release.
/// - **Branches checked out in a linked worktree.** `repo.reference()` would move
///   them happily, leaving that worktree's HEAD pointing somewhere its index and
///   files don't match.
/// - **Anything, if `opts.no_restack`** — then this is the old warn-only behaviour.
pub fn restack(
    repo: &Repository,
    map: &HashMap<Oid, Oid>,
    branch: &str,
    op: &str,
    opts: &Opts,
) -> (Vec<String>, Vec<String>) {
    let (mut moved, mut warnings) = (Vec::new(), Vec::new());
    let Ok(refs) = repo.references() else {
        return (moved, vec!["could not enumerate refs; siblings not checked".into()]);
    };
    // Collect first: promoting while the ref iterator is live would mutate the
    // refdb underneath it.
    let mut todo: Vec<(String, Oid, Oid, bool)> = Vec::new();
    for r in refs.flatten() {
        let Some(name) = r.name() else { continue };
        if name == branch || !(r.is_branch() || r.is_tag()) {
            continue;
        }
        let Ok(old) = r.peel_to_commit() else { continue };
        // Not in the map = not rewritten (or dropped with nothing to land on).
        let Some(&new) = map.get(&old.id()) else { continue };
        if old.id() != new {
            todo.push((name.to_string(), old.id(), new, r.is_tag()));
        }
    }
    let held = if todo.is_empty() { HashSet::new() } else { worktree_branches(repo) };
    for (name, old, new, is_tag) in todo {
        let short = short_branch(&name);
        if is_tag {
            warnings.push(format!("tag {short} still points at {old:.8} (kept; a tag names a commit)"));
        } else if opts.no_restack {
            warnings.push(format!("{short} still points into the rewritten range (now orphaned)"));
        } else if held.contains(&name) {
            warnings.push(format!("{short} is checked out in another worktree — left at {old:.8}"));
        } else if opts.dry_run {
            moved.push(format!("{short} {old:.8} -> {new:.8}"));
        } else {
            // sync = false: a sibling's worktree is not ours to write.
            match promote(repo, &name, new, old, &restack_msg(op), false) {
                Ok(()) => moved.push(format!("{short} {old:.8} -> {new:.8}")),
                Err(e) => warnings.push(format!("{short} not restacked: {e}")),
            }
        }
    }
    (moved, warnings)
}

/// Refnames checked out in a *linked* worktree (`git worktree add`).
fn worktree_branches(repo: &Repository) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(names) = repo.worktrees() else { return out };
    for n in names.iter().flatten() {
        let Ok(wt) = repo.find_worktree(n) else { continue };
        let Ok(r) = Repository::open_from_worktree(&wt) else { continue };
        if let Some(name) = r.head().ok().and_then(|h| h.name().map(String::from)) {
            out.insert(name);
        }
    }
    out
}

/// The newest commit that owns any staged-change line (excluding `requested`),
/// used to hint a better `fix` target after a conflict.
fn suggest_target(repo: &Repository, head: Oid, requested: Oid) -> Result<Option<Oid>> {
    let head_tree = repo.find_commit(head)?.tree()?;
    let staged_tree = repo.find_tree(repo.index()?.write_tree()?)?;
    let window: Vec<Oid> = git::linear_commits(repo, None, head)?.iter().map(|c| c.id()).collect();
    let pos: HashMap<Oid, usize> = window.iter().enumerate().map(|(i, o)| (*o, i)).collect();

    let diff = repo.diff_tree_to_tree(Some(&head_tree), Some(&staged_tree), None)?;
    let mut best: Option<(usize, Oid)> = None;
    for d in diff.deltas() {
        if d.status() != Delta::Modified {
            continue;
        }
        let Some(p) = d.new_file().path() else { continue };
        let old = blob_at(repo, &head_tree, p)?;
        let new = blob_at(repo, &staged_tree, p)?;
        if std::str::from_utf8(&old).is_err() || std::str::from_utf8(&new).is_err() {
            continue;
        }
        let hs = patch::hunks(&old, &new)?;
        for t in inference::infer_targets(repo, &p.to_string_lossy(), &hs, &window)?
            .into_iter()
            .flatten()
        {
            let rank = pos[&t];
            if best.is_none_or(|(r, _)| rank > r) {
                best = Some((rank, t));
            }
        }
    }
    Ok(best.map(|(_, t)| t).filter(|&t| t != requested))
}

/// op B — re-anchor `path` so it first appears at `target_rev` (which may be
/// earlier *or* later than where the file is introduced today). `opts.dry_run`
/// skips the ref moves only.
pub fn mv(repo: &Repository, path: &str, target_rev: &str, opts: &Opts) -> Result<Outcome> {
    let branch = head_branch(repo)?;
    // `mv` takes no staged input, so require a fully clean tree — a checkout to
    // the rewritten tip would otherwise clobber unrelated staged/worktree edits.
    require_fully_clean(repo)?;

    let head = git::resolve(repo, "HEAD")?;
    let target = git::resolve(repo, target_rev)?;

    let plan = recipe::mv(repo, path, target, head)?;
    let r = engine::replay(repo, plan.base, plan.tip, &plan.recipe, opts.ignore_ws, false)?;
    let msg = format!("transplant: move {path} to {target:.8}");
    if !opts.dry_run {
        promote(repo, &branch, r.tip, head, &msg, true)?;
    }
    Ok(outcome(repo, branch, head, r, &msg, opts))
}

/// op D (auto) — distribute the staged change hunk-by-hunk into the commits that
/// own the changed lines (git-absorb style). `base` bounds the stack window;
/// None walks to the root. Hunks with no owner in the window are left staged.
/// `opts.dry_run` skips the ref moves (and the checkout), nothing else.
pub fn collapse(repo: &Repository, base: Option<Oid>, opts: &Opts) -> Result<Absorbed> {
    let branch = head_branch(repo)?;
    require_clean_unstaged(repo)?;

    let head = git::resolve(repo, "HEAD")?;
    let head_tree = repo.find_commit(head)?.tree()?;
    let staged_tree = repo.find_tree(repo.index()?.write_tree()?)?;
    if staged_tree.id() == head_tree.id() {
        return Err(Error::NothingStaged);
    }

    // No explicit --base: stop at the first merge rather than aborting because
    // one exists somewhere in the ancestry.
    let stack = match base {
        Some(_) => git::linear_commits(repo, base, head)?,
        None => git::linear_window(repo, head)?,
    };
    let window: Vec<Oid> = stack.iter().map(|c| c.id()).collect();
    let pos: HashMap<Oid, usize> = window.iter().enumerate().map(|(i, o)| (*o, i)).collect();

    let mut recipe = Recipe::new();
    let (mut folded, mut orphans) = (0usize, 0usize);
    let mut earliest: Option<usize> = None;
    let mut routes: Vec<(String, String, Oid)> = Vec::new();

    let diff = repo.diff_tree_to_tree(Some(&head_tree), Some(&staged_tree), None)?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for d in diff.deltas() {
        if d.status() == Delta::Modified {
            match d.new_file().path() {
                Some(p) => paths.push(p.to_path_buf()),
                // Unnameable path: not folded, so it MUST count as an orphan —
                // `orphans == 0` is what gates the force checkout.
                None => orphans += 1,
            }
        } else {
            // whole-file add/delete isn't a hunk we can attribute — leave it.
            orphans += 1;
        }
    }

    for path in &paths {
        let ps = path.to_string_lossy().into_owned();
        let old = blob_at(repo, &head_tree, path)?;
        let new = blob_at(repo, &staged_tree, path)?;
        // Only text (valid UTF-8) files can be safely hunk-absorbed; leave the
        // rest staged rather than risk corrupting bytes we can't diff line-wise.
        if std::str::from_utf8(&old).is_err() || std::str::from_utf8(&new).is_err() {
            orphans += 1;
            continue;
        }
        let hs = patch::hunks(&old, &new)?;
        if hs.is_empty() {
            // binary or otherwise unrepresentable as hunks — leave it staged.
            orphans += 1;
            continue;
        }
        let targets = inference::infer_targets(repo, &ps, &hs, &window)?;
        let old_str = String::from_utf8_lossy(&old).into_owned();
        for (i, tgt) in targets.iter().enumerate() {
            match tgt {
                Some(t) => {
                    let mut sel = vec![false; hs.len()];
                    sel[i] = true;
                    let mode = staged_tree
                        .get_path(path)
                        .map(|e| e.filemode())
                        .unwrap_or(0o100644);
                    let synth =
                        patch::synthetic_for_hunks(repo, head, &ps, &old_str, &hs, &sel, mode)?;
                    recipe.add(*t, Edit::ApplyChange(synth));
                    routes.push((ps.clone(), hs[i].header.clone(), *t));
                    folded += 1;
                    let p = pos[t];
                    earliest = Some(earliest.map_or(p, |e| e.min(p)));
                }
                None => orphans += 1,
            }
        }
    }

    let Some(earliest) = earliest else {
        return Ok(Absorbed { outcome: None, folded, orphans, routes });
    };
    let earliest_oid = window[earliest];
    let base_replay = {
        let c = repo.find_commit(earliest_oid)?;
        if c.parent_count() == 0 {
            None
        } else {
            Some(c.parent_id(0)?)
        }
    };
    // drop_empty: a commit fully absorbed elsewhere shouldn't linger empty.
    let r = engine::replay(repo, base_replay, head, &recipe, opts.ignore_ws, true)?;
    let msg = "transplant: absorb staged change";
    // With no orphans the whole staged change was folded → checkout to a clean
    // tree (sync). With orphans, move the ref only so they stay staged.
    if !opts.dry_run {
        promote(repo, &branch, r.tip, head, msg, orphans == 0)?;
    }
    Ok(Absorbed {
        outcome: Some(outcome(repo, branch, head, r, msg, opts)),
        folded,
        orphans,
        routes,
    })
}

/// Move the branch back to where the newest `transplant:` reflog entry found it.
///
/// The reflog is enough here. git-branchless rejected it for its own undo because
/// a reflog cannot recover branch *creation* or *deletion* — but this tool only
/// ever moves one existing branch, and every move writes a `transplant: …` entry,
/// so that branch's reflog is a complete record of everything we did.
///
/// The ref move goes through the same compare-and-swap `promote`, so an undo
/// refuses if the branch moved since. The undo is itself recorded as
/// `transplant: undo …`, which makes a second `undo` a redo.
pub fn undo(repo: &Repository, dry_run: bool) -> Result<Outcome> {
    let branch = head_branch(repo)?;
    let reflog = repo.reflog(&branch)?;
    // Entry 0 is the newest.
    let entry = reflog
        .iter()
        .find(|e| e.message().is_some_and(|m| m.starts_with("transplant: ")))
        .ok_or_else(|| {
            Error::Empty(format!(
                "no git-transplant entry in {}'s reflog — nothing to undo",
                short_branch(&branch)
            ))
        })?;
    let (from, to) = (entry.id_new(), entry.id_old());
    let msg = entry.message().unwrap_or_default().to_string();
    // Same guarantee `promote` gives, said better: name the operation being undone.
    let current = repo.refname_to_id(&branch)?;
    if current != from {
        return Err(Error::Empty(format!(
            "{} has moved since `{msg}` (now {current:.8}, expected {from:.8}); refusing to undo",
            short_branch(&branch)
        )));
    }
    let mut restacked = Vec::new();
    if !dry_run {
        // sync = false: undo must never write the worktree. A force checkout would
        // discard whatever is on disk; moving the ref alone cannot lose work — the
        // undone change simply resurfaces as an uncommitted edit.
        promote(repo, &branch, to, from, &format!("transplant: undo ({msg})"), false)?;
        restacked = unrestack(repo, &msg);
    }
    Ok(Outcome { branch, old_tip: from, new_tip: to, restacked, warnings: vec![] })
}

/// Put back the siblings that `msg`'s restack moved, so undo restores the whole
/// stack and not just the branch that was rewritten.
///
/// A branch qualifies only if its *newest* reflog entry is exactly this
/// operation's restack and it still sits where that entry left it — which is the
/// same compare-and-swap discipline `undo` applies to the branch itself.
fn unrestack(repo: &Repository, msg: &str) -> Vec<String> {
    let want = restack_msg(msg);
    let names: Vec<String> = repo
        .branches(Some(BranchType::Local))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|(b, _)| b.get().name().map(String::from))
        .collect();
    let mut back = Vec::new();
    for name in names {
        let Ok(log) = repo.reflog(&name) else { continue };
        let Some(e) = log.iter().next().filter(|e| e.message() == Some(want.as_str())) else {
            continue;
        };
        let (from, to) = (e.id_new(), e.id_old());
        if repo.refname_to_id(&name) != Ok(from) {
            continue; // moved since; leave it alone rather than clobber
        }
        if promote(repo, &name, to, from, &format!("transplant: undo ({want})"), false).is_ok() {
            back.push(format!("{} {from:.8} -> {to:.8}", short_branch(&name)));
        }
    }
    back
}

fn blob_at(repo: &Repository, tree: &git2::Tree, path: &Path) -> Result<Vec<u8>> {
    let entry = tree
        .get_path(path)
        .map_err(|_| Error::PathNotFound { path: path.to_string_lossy().into_owned() })?;
    Ok(entry.to_object(repo)?.as_blob().map(|b| b.content().to_vec()).unwrap_or_default())
}

/// Full ref name HEAD points at (e.g. `refs/heads/main`). Public so the TUI can
/// reuse it.
pub fn head_branch(repo: &Repository) -> Result<String> {
    let head = repo.head()?;
    if !head.is_branch() {
        return Err(Error::DetachedHead);
    }
    Ok(head.name().unwrap().to_string())
}

/// Reject unstaged tracked changes. Staged changes are `fix`/`absorb`'s input,
/// so only working-tree churn is rejected. Public so the TUI shares the guard.
pub fn require_clean_unstaged(repo: &Repository) -> Result<()> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(false).include_ignored(false);
    for e in repo.statuses(Some(&mut opts))?.iter() {
        let s = e.status();
        if s.is_wt_modified() || s.is_wt_deleted() || s.is_wt_renamed() || s.is_wt_typechange() {
            return Err(Error::DirtyWorktree);
        }
    }
    Ok(())
}

/// Reject any tracked change, staged or unstaged. Public so the TUI's move
/// preview mirrors the guard `mv` applies on execute.
pub fn require_fully_clean(repo: &Repository) -> Result<()> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(false).include_ignored(false);
    if !repo.statuses(Some(&mut opts))?.is_empty() {
        return Err(Error::DirtyWorktree);
    }
    Ok(())
}

/// Move the branch ref to the rewritten tip (with a reflog entry). The ref move
/// is LAST, so any failure leaves the ref unmoved.
///
/// `sync` decides the worktree: `fix`/`move` fully absorb their input, so they
/// force-checkout the new tip (clean tree). `absorb` and the TUI may leave hunks
/// *un-folded* (no home, or deselected) — a force-checkout would wipe that staged
/// work — so they move the ref only; the worktree/index already equal `new_tip`
/// plus the still-staged remainder. Public so the TUI shares this path.
pub fn promote(
    repo: &Repository,
    branch: &str,
    new_tip: Oid,
    old_tip: Oid,
    msg: &str,
    sync: bool,
) -> Result<()> {
    if new_tip == old_tip {
        return Ok(());
    }
    if sync {
        let tree = repo.find_commit(new_tip)?.tree()?;
        let mut co = CheckoutBuilder::new();
        co.force();
        repo.checkout_tree(tree.as_object(), Some(&mut co))?;
    }
    // Compare-and-swap: only move the ref if it still points where we started.
    // A plain force-update would silently discard commits made on this branch
    // (another terminal, a long-lived TUI session) since `old_tip` was captured.
    if let Err(e) = repo.reference_matching(branch, new_tip, true, old_tip, msg) {
        let current = repo.refname_to_id(branch).ok();
        if current != Some(old_tip) {
            return Err(Error::Empty(format!(
                "{} moved since this operation started (now {}) — refusing to \
                 overwrite; re-run to pick up the new commits",
                short_branch(branch),
                current.map(|o| format!("{o:.8}")).unwrap_or_else(|| "gone".into())
            )));
        }
        return Err(Error::Git(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_branch_strips_the_ref_prefix() {
        assert_eq!(short_branch("refs/heads/main"), "main");
        assert_eq!(short_branch("refs/heads/feature/x"), "x");
        assert_eq!(short_branch("refs/tags/v1"), "v1");
        assert_eq!(short_branch("main"), "main");
    }

    #[test]
    fn outcome_prints_the_short_branch() {
        let o = Outcome {
            branch: "refs/heads/main".into(),
            old_tip: Oid::zero(),
            new_tip: Oid::zero(),
            restacked: vec![],
            warnings: vec![],
        };
        assert_eq!(o.short_branch(), "main");
    }
}
