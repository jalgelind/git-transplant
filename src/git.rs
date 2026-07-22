//! Thin git2 helpers shared by the engine and commands.

use std::path::Path;

use git2::{Commit, FileFavor, MergeOptions, Oid, Repository, Signature, Tree};

use crate::{Error, Result};

/// The well-known empty tree.
pub fn empty_tree(repo: &Repository) -> Result<Tree<'_>> {
    let oid = repo.treebuilder(None)?.write()?;
    Ok(repo.find_tree(oid)?)
}

/// A path's blob content in `tree`, or empty if it isn't there. Every caller
/// diffs two trees first, so "absent" only ever means "one side of an add or
/// delete" — an error there would be noise, not information.
pub fn blob_at(repo: &Repository, tree: &Tree, path: &Path) -> Vec<u8> {
    tree.get_path(path)
        .and_then(|e| e.to_object(repo))
        .ok()
        .and_then(|o| o.peel_to_blob().ok())
        .map(|b| b.content().to_vec())
        .unwrap_or_default()
}

/// A signature to stamp synthetic commits with; falls back if git identity is unset.
pub fn ident(repo: &Repository) -> Signature<'static> {
    repo.signature()
        .unwrap_or_else(|_| Signature::now("git-transplant", "git-transplant@localhost").unwrap())
}

/// How the engine merges: whitespace sensitivity, plus an optional fixed rule
/// for resolving conflicting regions instead of aborting.
///
/// In every merge the engine does, **ours** is the stack being replayed onto
/// (the rewritten tree so far) and **theirs** is the change being applied (the
/// commit being replayed, or the staged hunk being injected) — `git rebase`'s
/// sense of the words, not `git merge`'s. Ours is never your working copy.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Merge {
    pub ignore_ws: bool,
    pub favor: Option<FileFavor>,
}

/// `false` is still "whitespace significant, conflicts abort" at every call site.
impl From<bool> for Merge {
    fn from(ignore_ws: bool) -> Self {
        Merge { ignore_ws, favor: None }
    }
}

impl Merge {
    /// git2 options for one 3-way merge.
    pub fn opts(&self) -> MergeOptions {
        let mut mo = MergeOptions::new();
        mo.patience(true); // fewer spurious conflicts than the default (myers) diff
        if self.ignore_ws {
            mo.ignore_whitespace(true);
        }
        if let Some(f) = self.favor {
            mo.file_favor(f);
        }
        mo
    }
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
/// `msg` overrides the message (squash/reword); `None` keeps the original.
pub fn recommit(
    repo: &Repository,
    orig: &Commit,
    tree: &Tree,
    parents: &[&Commit],
    msg: Option<&str>,
) -> Result<Oid> {
    Ok(repo.commit(
        None,
        &orig.author(),
        &orig.committer(),
        msg.unwrap_or_else(|| orig.message_raw().unwrap_or("")),
        tree,
        parents,
    )?)
}
