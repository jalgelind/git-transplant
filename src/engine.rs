//! The recipe-replay engine: one in-memory walk that rewrites `base..tip`,
//! injecting per-commit edits. Builds only dangling objects and returns the new
//! tip oid — it never moves a ref, so a caller that only promotes on `Ok` gets
//! atomicity for free (on `Err`, nothing referenced changed).

use std::collections::HashMap;

use git2::{ObjectType, Oid, Repository};

use crate::git;
use crate::{Error, Result};

/// One edit injected at a commit during replay.
#[derive(Debug, Clone)]
pub enum Edit {
    /// Apply a synthetic commit's change (its parent->tree diff) via 3-way merge.
    ApplyChange(Oid),
    /// Apply the inverse of a synthetic commit's change (revert).
    RevertChange(Oid),
    /// Set a path's blob directly (whole-file add/replace), preserving `mode`
    /// (e.g. 0o100644, 0o100755 for an executable, 0o120000 for a symlink).
    SetFile { path: String, blob: Oid, mode: i32 },
    /// Remove a path directly.
    RemoveFile { path: String },
}

/// A map of commit -> edits to inject when that commit is replayed.
#[derive(Debug, Default)]
pub struct Recipe {
    edits: HashMap<Oid, Vec<Edit>>,
}

impl Recipe {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&mut self, commit: Oid, edit: Edit) {
        self.edits.entry(commit).or_default().push(edit);
    }
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }
    fn for_commit(&self, commit: Oid) -> &[Edit] {
        self.edits.get(&commit).map(|v| v.as_slice()).unwrap_or(&[])
    }
    /// Earliest touched commit is used to size the replay range; callers pass an
    /// explicit `base` instead, but this guards against a recipe touching a
    /// commit outside the range.
    pub fn touches(&self, commit: Oid) -> bool {
        self.edits.contains_key(&commit)
    }
}

/// Replay `base..tip` (base exclusive, or None to start from the root),
/// injecting `recipe`. Returns the rewritten tip oid. No refs are moved.
pub fn replay(
    repo: &Repository,
    base: Option<Oid>,
    tip: Oid,
    recipe: &Recipe,
    ignore_ws: bool,
) -> Result<Oid> {
    replay_opts(repo, base, tip, recipe, ignore_ws, false)
}

/// Like [`replay`], but `drop_empty` omits any rewritten commit whose tree ends
/// up identical to its (rewritten) parent — so a commit whose whole change was
/// absorbed elsewhere doesn't linger as an empty commit (op A/D).
pub fn replay_opts(
    repo: &Repository,
    base: Option<Oid>,
    tip: Oid,
    recipe: &Recipe,
    ignore_ws: bool,
    drop_empty: bool,
) -> Result<Oid> {
    let commits = git::linear_commits(repo, base, tip)?;

    let mut parent_commit = match base {
        Some(b) => Some(repo.find_commit(b)?),
        None => None,
    };
    let mut parent_tree = match base {
        Some(b) => repo.find_commit(b)?.tree()?,
        None => git::empty_tree(repo)?,
    };

    for ci in &commits {
        // 3-way replay of ci onto the rewritten parent tree.
        let ci_base_tree = if ci.parent_count() == 0 {
            git::empty_tree(repo)?
        } else {
            repo.find_commit(ci.parent_id(0)?)?.tree()?
        };
        let mo = git::merge_opts(ignore_ws);
        let mut idx = repo.merge_trees(&ci_base_tree, &parent_tree, &ci.tree()?, Some(&mo))?;
        if idx.has_conflicts() {
            return Err(conflict(&idx, ci.id()));
        }
        let mut tree_oid = idx.write_tree_to(repo)?;

        // Inject this commit's edits.
        for edit in recipe.for_commit(ci.id()) {
            tree_oid = apply_edit(repo, tree_oid, edit, ci.id(), ignore_ws)?;
        }

        // Drop a commit that became empty (its change was absorbed elsewhere).
        if drop_empty && tree_oid == parent_tree.id() {
            continue;
        }
        let tree = repo.find_tree(tree_oid)?;
        let parents: Vec<&git2::Commit> = parent_commit.iter().collect();
        let new_oid = git::recommit(repo, ci, &tree, &parents)?;
        parent_commit = Some(repo.find_commit(new_oid)?);
        parent_tree = tree;
    }

    Ok(parent_commit.map(|c| c.id()).unwrap_or(tip))
}

fn apply_edit(
    repo: &Repository,
    tree_oid: Oid,
    edit: &Edit,
    at: Oid,
    ignore_ws: bool,
) -> Result<Oid> {
    match edit {
        Edit::ApplyChange(synth) => {
            let s = repo.find_commit(*synth)?;
            let base = s.parent(0)?.tree()?;
            let ours = repo.find_tree(tree_oid)?;
            let theirs = s.tree()?;
            let mo = git::merge_opts(ignore_ws);
            let mut idx = repo.merge_trees(&base, &ours, &theirs, Some(&mo))?;
            if idx.has_conflicts() {
                return Err(conflict(&idx, at));
            }
            Ok(idx.write_tree_to(repo)?)
        }
        Edit::RevertChange(synth) => {
            // Reverse = swap base and theirs: apply (synth -> synth.parent) onto ours.
            let s = repo.find_commit(*synth)?;
            let base = s.tree()?;
            let ours = repo.find_tree(tree_oid)?;
            let theirs = s.parent(0)?.tree()?;
            let mo = git::merge_opts(ignore_ws);
            let mut idx = repo.merge_trees(&base, &ours, &theirs, Some(&mo))?;
            if idx.has_conflicts() {
                return Err(conflict(&idx, at));
            }
            Ok(idx.write_tree_to(repo)?)
        }
        Edit::SetFile { path, blob, mode } => set_path(repo, tree_oid, path, Some((*blob, *mode))),
        Edit::RemoveFile { path } => set_path(repo, tree_oid, path, None),
    }
}

fn conflict(idx: &git2::Index, commit: Oid) -> Error {
    let path = idx
        .conflicts()
        .ok()
        .and_then(|mut cs| cs.next())
        .and_then(|c| c.ok())
        .and_then(|c| c.our.or(c.their).or(c.ancestor))
        .map(|e| String::from_utf8_lossy(&e.path).into_owned());
    Error::Conflict { commit, path, suggested: None }
}

/// Set (blob + mode) or remove a (possibly nested) path in a tree, returning the
/// new tree oid. `entry = None` removes. Handles nested paths by recursing —
/// `treebuilder.insert` rejects any name containing `/`, so this splitting is
/// mandatory (also reused by `patch::synthetic_for_hunks`).
pub(crate) fn set_path(
    repo: &Repository,
    tree_oid: Oid,
    path: &str,
    entry: Option<(Oid, i32)>,
) -> Result<Oid> {
    let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if comps.is_empty() {
        return Err(Error::PathNotFound { path: path.into() });
    }
    update(repo, Some(tree_oid), &comps, entry)
}

fn update(
    repo: &Repository,
    tree_oid: Option<Oid>,
    comps: &[&str],
    entry: Option<(Oid, i32)>,
) -> Result<Oid> {
    let tree = match tree_oid {
        Some(o) => Some(repo.find_tree(o)?),
        None => None,
    };
    let mut b = repo.treebuilder(tree.as_ref())?;
    if comps.len() == 1 {
        match entry {
            Some((oid, mode)) => {
                b.insert(comps[0], oid, mode)?;
            }
            None => {
                if b.get(comps[0])?.is_some() {
                    b.remove(comps[0])?;
                }
            }
        }
    } else {
        // Refuse to descend through a non-tree — inserting/removing under a file
        // would silently corrupt or drop it.
        if let Some(e) = b.get(comps[0])? {
            if e.kind() != Some(ObjectType::Tree) {
                if entry.is_none() {
                    return Ok(b.write()?); // nothing to remove beneath a file
                }
                return Err(Error::Empty(format!(
                    "path conflict: '{}' is a file, not a directory",
                    comps[0]
                )));
            }
        }
        let sub = b.get(comps[0])?.map(|e| e.id());
        let new_sub = update(repo, sub, &comps[1..], entry)?;
        // Drop empty directories on removal; otherwise link the rebuilt subtree.
        let empty = repo.find_tree(new_sub).map(|t| t.is_empty()).unwrap_or(false);
        if entry.is_none() && empty {
            if b.get(comps[0])?.is_some() {
                b.remove(comps[0])?;
            }
        } else {
            b.insert(comps[0], new_sub, 0o040000)?;
        }
    }
    Ok(b.write()?)
}
