//! Thin git2 helpers shared by the engine and commands.

use git2::{Commit, MergeOptions, Oid, Repository, Signature, Tree};

use crate::{Error, Result};

/// The well-known empty tree.
pub fn empty_tree(repo: &Repository) -> Result<Tree<'_>> {
    let oid = repo.treebuilder(None)?.write()?;
    Ok(repo.find_tree(oid)?)
}

/// A signature to stamp synthetic commits with; falls back if git identity is unset.
pub fn ident(repo: &Repository) -> Signature<'static> {
    repo.signature()
        .unwrap_or_else(|_| Signature::now("git-transplant", "git-transplant@localhost").unwrap())
}

/// Merge options used for every 3-way merge in the engine.
pub fn merge_opts(ignore_ws: bool) -> MergeOptions {
    let mut mo = MergeOptions::new();
    mo.patience(true); // fewer spurious conflicts than the default (myers) diff
    if ignore_ws {
        mo.ignore_whitespace(true);
    }
    mo
}

/// Resolve a revspec to a commit.
pub fn resolve(repo: &Repository, rev: &str) -> Result<Oid> {
    Ok(repo.revparse_single(rev)?.peel_to_commit()?.id())
}

/// Is `ancestor` an ancestor of (or equal to) `descendant`?
pub fn is_ancestor(repo: &Repository, ancestor: Oid, descendant: Oid) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    Ok(repo.graph_descendant_of(descendant, ancestor)?)
}

/// Commits from `base` (exclusive) up to `tip` (inclusive), oldest first,
/// following first parents. Rejects merge commits and a base that isn't reached.
pub fn linear_commits(repo: &Repository, base: Option<Oid>, tip: Oid) -> Result<Vec<Commit<'_>>> {
    let mut oids = Vec::new();
    let mut cur = tip;
    loop {
        if Some(cur) == base {
            break;
        }
        let c = repo.find_commit(cur)?;
        if c.parent_count() > 1 {
            return Err(Error::MergeInRange { commit: cur });
        }
        oids.push(cur);
        if c.parent_count() == 0 {
            if base.is_some() {
                return Err(Error::BaseNotAncestor);
            }
            break;
        }
        cur = c.parent_id(0)?;
    }
    oids.reverse();
    oids.into_iter()
        .map(|o| repo.find_commit(o).map_err(Error::from))
        .collect()
}

/// The linear run of commits from `tip` back to (but excluding) the first merge
/// commit, or the root. Used to size the *window* the TUI and `absorb` offer:
/// walking to the root with `linear_commits` aborts the whole operation when any
/// merge exists anywhere in the branch's ancestry, even if every commit the user
/// touches is linear.
pub fn linear_window(repo: &Repository, tip: Oid) -> Result<Vec<Commit<'_>>> {
    let mut oids = Vec::new();
    let mut cur = tip;
    loop {
        let c = repo.find_commit(cur)?;
        if c.parent_count() > 1 {
            break; // a merge bounds the rewritable stack
        }
        oids.push(cur);
        if c.parent_count() == 0 {
            break;
        }
        cur = c.parent_id(0)?;
    }
    oids.reverse();
    oids.into_iter()
        .map(|o| repo.find_commit(o).map_err(Error::from))
        .collect()
}

/// Re-create `orig` with a new tree and parents, preserving author and committer.
pub fn recommit(repo: &Repository, orig: &Commit, tree: &Tree, parents: &[&Commit]) -> Result<Oid> {
    Ok(repo.commit(
        None,
        &orig.author(),
        &orig.committer(),
        orig.message_raw().unwrap_or(""),
        tree,
        parents,
    )?)
}
