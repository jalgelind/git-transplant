//! High-level operations: build a plan, replay, and (only on full success)
//! promote the branch ref. Kept in the library so tests can drive them against
//! a real repo and assert ref-level atomicity.

use git2::{Oid, Repository, ResetType};

use crate::{engine, git, recipe};
use crate::{Error, Result};

#[derive(Debug)]
pub struct Outcome {
    pub branch: String,
    pub old_tip: Oid,
    pub new_tip: Oid,
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
    let new_tip = engine::replay(repo, plan.base, plan.tip, &plan.recipe, ignore_ws)?;
    promote(repo, &branch, new_tip, head, &format!("transplant: fix into {target:.8}"))?;
    Ok(Outcome { branch, old_tip: head, new_tip })
}

/// op B — re-anchor `path` at `target_rev`.
pub fn mv(repo: &Repository, path: &str, target_rev: &str, ignore_ws: bool) -> Result<Outcome> {
    let branch = head_branch(repo)?;
    require_clean_unstaged(repo)?;

    let head = git::resolve(repo, "HEAD")?;
    let target = git::resolve(repo, target_rev)?;

    let plan = recipe::mv(repo, path, target, head)?;
    let new_tip = engine::replay(repo, plan.base, plan.tip, &plan.recipe, ignore_ws)?;
    promote(repo, &branch, new_tip, head, &format!("transplant: move {path} to {target:.8}"))?;
    Ok(Outcome { branch, old_tip: head, new_tip })
}

/// Full ref name HEAD points at (e.g. `refs/heads/main`).
fn head_branch(repo: &Repository) -> Result<String> {
    let head = repo.head()?;
    if !head.is_branch() {
        return Err(Error::DetachedHead);
    }
    Ok(head.name().unwrap().to_string())
}

/// Reject unstaged tracked changes. Staged changes are `fix`'s input, so only
/// working-tree churn is rejected.
fn require_clean_unstaged(repo: &Repository) -> Result<()> {
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

/// Move the branch ref to the rewritten tip (with a reflog entry) and reset the
/// index. The worktree already matches the new tip (the folded change was in the
/// worktree; the moved file still exists at HEAD), so files are never touched.
fn promote(repo: &Repository, branch: &str, new_tip: Oid, old_tip: Oid, msg: &str) -> Result<()> {
    if new_tip == old_tip {
        return Ok(());
    }
    repo.reference(branch, new_tip, true, msg)?;
    let obj = repo.find_object(new_tip, None)?;
    repo.reset(&obj, ResetType::Mixed, None)?;
    Ok(())
}
