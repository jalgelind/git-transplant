//! High-level operations: build a plan, replay, and (only on full success)
//! promote the branch ref. Kept in the library so tests can drive them against
//! a real repo and assert ref-level atomicity.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use git2::build::CheckoutBuilder;
use git2::{Delta, Oid, Repository};

use crate::engine::{Edit, Recipe};
use crate::{engine, git, inference, patch, recipe};
use crate::{Error, Result};

#[derive(Debug)]
pub struct Outcome {
    pub branch: String,
    pub old_tip: Oid,
    pub new_tip: Oid,
    /// Other refs left pointing into the rewritten (now-orphaned) range.
    pub warnings: Vec<String>,
}

/// Result of an absorb: the replay outcome (None if nothing had a home), how many
/// hunks were folded, and how many were left in the worktree (no home).
#[derive(Debug)]
pub struct Absorbed {
    pub outcome: Option<Outcome>,
    pub folded: usize,
    pub orphans: usize,
}

/// op C — fold the staged change into `target_rev`.
pub fn fix(repo: &Repository, target_rev: &str, ignore_ws: bool) -> Result<Outcome> {
    let branch = head_branch(repo)?;
    require_clean_unstaged(repo)?;

    let head = git::resolve(repo, "HEAD")?;
    let target = git::resolve(repo, target_rev)?;

    let staged_tree = repo.index()?.write_tree()?;
    if staged_tree == repo.find_commit(head)?.tree()?.id() {
        return Err(Error::NothingStaged);
    }

    let plan = recipe::fix(repo, target, head, staged_tree)?;
    let new_tip = match engine::replay(repo, plan.base, plan.tip, &plan.recipe, ignore_ws) {
        Ok(t) => t,
        // On conflict, enrich the error with the commit inference thinks owns the
        // changed lines — the target the fold would have gone to cleanly.
        Err(Error::Conflict { commit, path, .. }) => {
            let suggested = suggest_target(repo, head, target).unwrap_or(None);
            return Err(Error::Conflict { commit, path, suggested });
        }
        Err(e) => return Err(e),
    };
    promote(repo, &branch, new_tip, head, &format!("transplant: fix into {target:.8}"), true)?;
    let warnings = abandoned_warnings(repo, plan.base, head, &branch);
    Ok(Outcome { branch, old_tip: head, new_tip, warnings })
}

/// Names of refs (other than `branch`) left pointing at a now-rewritten commit —
/// they're stranded on orphaned history after the rewrite.
fn abandoned_warnings(repo: &Repository, base: Option<Oid>, old_tip: Oid, branch: &str) -> Vec<String> {
    let rewritten: std::collections::HashSet<Oid> = git::linear_commits(repo, base, old_tip)
        .map(|v| v.iter().map(|c| c.id()).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    if let Ok(refs) = repo.references() {
        for r in refs.flatten() {
            let Some(name) = r.name() else { continue };
            if name == branch || !(r.is_branch() || r.is_tag()) {
                continue;
            }
            if let Ok(c) = r.peel_to_commit() {
                if rewritten.contains(&c.id()) {
                    out.push(format!("{name} still points into the rewritten range (now orphaned)"));
                }
            }
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

/// op B — re-anchor `path` at `target_rev`.
pub fn mv(repo: &Repository, path: &str, target_rev: &str, ignore_ws: bool) -> Result<Outcome> {
    let branch = head_branch(repo)?;
    // `mv` takes no staged input, so require a fully clean tree — a checkout to
    // the rewritten tip would otherwise clobber unrelated staged/worktree edits.
    require_fully_clean(repo)?;

    let head = git::resolve(repo, "HEAD")?;
    let target = git::resolve(repo, target_rev)?;

    let plan = recipe::mv(repo, path, target, head)?;
    let new_tip = engine::replay(repo, plan.base, plan.tip, &plan.recipe, ignore_ws)?;
    promote(repo, &branch, new_tip, head, &format!("transplant: move {path} to {target:.8}"), true)?;
    let warnings = abandoned_warnings(repo, plan.base, head, &branch);
    Ok(Outcome { branch, old_tip: head, new_tip, warnings })
}

/// op D (auto) — distribute the staged change hunk-by-hunk into the commits that
/// own the changed lines (git-absorb style). `base` bounds the stack window;
/// None walks to the root. Hunks with no owner in the window are left staged.
pub fn collapse(repo: &Repository, base: Option<Oid>, ignore_ws: bool) -> Result<Absorbed> {
    let branch = head_branch(repo)?;
    require_clean_unstaged(repo)?;

    let head = git::resolve(repo, "HEAD")?;
    let head_tree = repo.find_commit(head)?.tree()?;
    let staged_tree = repo.find_tree(repo.index()?.write_tree()?)?;
    if staged_tree.id() == head_tree.id() {
        return Err(Error::NothingStaged);
    }

    let window: Vec<Oid> = git::linear_commits(repo, base, head)?
        .iter()
        .map(|c| c.id())
        .collect();
    let pos: HashMap<Oid, usize> = window.iter().enumerate().map(|(i, o)| (*o, i)).collect();

    let mut recipe = Recipe::new();
    let (mut folded, mut orphans) = (0usize, 0usize);
    let mut earliest: Option<usize> = None;

    let diff = repo.diff_tree_to_tree(Some(&head_tree), Some(&staged_tree), None)?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for d in diff.deltas() {
        if d.status() == Delta::Modified {
            if let Some(p) = d.new_file().path() {
                paths.push(p.to_path_buf());
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
                    let synth = patch::synthetic_for_hunks(repo, head, &ps, &old_str, &hs, &sel)?;
                    recipe.add(*t, Edit::ApplyChange(synth));
                    folded += 1;
                    let p = pos[t];
                    earliest = Some(earliest.map_or(p, |e| e.min(p)));
                }
                None => orphans += 1,
            }
        }
    }

    let Some(earliest) = earliest else {
        return Ok(Absorbed { outcome: None, folded, orphans });
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
    let new_tip = engine::replay_opts(repo, base_replay, head, &recipe, ignore_ws, true)?;
    // With no orphans the whole staged change was folded → checkout to a clean
    // tree (sync). With orphans, move the ref only so they stay staged.
    promote(repo, &branch, new_tip, head, "transplant: absorb staged change", orphans == 0)?;
    let warnings = abandoned_warnings(repo, base_replay, head, &branch);
    Ok(Absorbed {
        outcome: Some(Outcome { branch, old_tip: head, new_tip, warnings }),
        folded,
        orphans,
    })
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
    repo.reference(branch, new_tip, true, msg)?;
    Ok(())
}
