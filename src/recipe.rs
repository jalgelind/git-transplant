//! Build a `Recipe` (+ replay range) for each operation from git state.
//! Selection lives here; execution lives in `engine::replay`.

use std::path::Path;

use git2::{Oid, Repository};

use crate::engine::{Edit, Recipe};
use crate::git;
use crate::{Error, Result};

/// A recipe plus the range to replay.
pub struct Plan {
    pub recipe: Recipe,
    pub base: Option<Oid>,
    pub tip: Oid,
}

fn parent_of(repo: &Repository, c: Oid) -> Result<Option<Oid>> {
    let c = repo.find_commit(c)?;
    Ok(if c.parent_count() == 0 {
        None
    } else {
        Some(c.parent_id(0)?)
    })
}

/// op C — fold a staged change (captured as `staged_tree`) into `target`.
pub fn fix(repo: &Repository, target: Oid, head: Oid, staged_tree: Oid) -> Result<Plan> {
    if !git::is_ancestor(repo, target, head)? {
        return Err(Error::TargetNotAncestor);
    }
    // Synthetic commit carrying the staged delta, parented at HEAD.
    let sig = git::ident(repo);
    let head_commit = repo.find_commit(head)?;
    let tree = repo.find_tree(staged_tree)?;
    let synth = repo.commit(None, &sig, &sig, "fixup", &tree, &[&head_commit])?;

    let mut recipe = Recipe::new();
    recipe.add(target, Edit::ApplyChange(synth));
    Ok(Plan {
        recipe,
        base: parent_of(repo, target)?,
        tip: head,
    })
}

/// op B — re-anchor `path` so it first appears at `target`, in either direction:
///
/// * `target` already carries the file (it was introduced *earlier*) → strip it
///   from every ancestor of `target` that carries it, keeping `target`'s copy.
/// * `target` doesn't carry it yet (it's introduced *later*) → plant it at
///   `target` and let the replay carry it forward.
pub fn mv(repo: &Repository, path: &str, target: Oid, head: Oid) -> Result<Plan> {
    if !git::is_ancestor(repo, target, head)? {
        return Err(Error::TargetNotAncestor);
    }
    let target_tree = repo.find_commit(target)?.tree()?;
    match target_tree.get_path(Path::new(path)) {
        Ok(e) => strip_from_ancestors(repo, path, target, head, e.id(), e.filemode()),
        Err(_) => plant_at_target(repo, path, target, head),
    }
}

/// `target` already has the file: remove it from the ancestors that carry it
/// (requires the content be unchanged across that span) and re-add it here.
fn strip_from_ancestors(
    repo: &Repository,
    path: &str,
    target: Oid,
    head: Oid,
    blob_target: Oid,
    mode_target: i32,
) -> Result<Plan> {
    let mut recipe = Recipe::new();
    let mut intro: Option<Oid> = None;

    // Walk ancestors of target that contain `path`.
    let mut cur = parent_of(repo, target)?;
    while let Some(oid) = cur {
        let c = repo.find_commit(oid)?;
        let entry = c.tree()?.get_path(Path::new(path)).ok();
        let Some(entry) = entry else { break };
        if entry.id() != blob_target {
            return Err(Error::FileModified {
                path: path.into(),
                commit: oid,
            });
        }
        recipe.add(oid, Edit::RemoveFile { path: path.into() });
        intro = Some(oid);
        cur = parent_of(repo, oid)?;
    }

    let Some(intro) = intro else {
        return Err(Error::Empty(format!(
            "{path} is not present before {target:.8}; nothing to move"
        )));
    };

    // Re-add target's copy (blob + original mode) after the merge strips it.
    recipe.add(
        target,
        Edit::SetFile {
            path: path.into(),
            blob: blob_target,
            mode: mode_target,
        },
    );

    Ok(Plan {
        recipe,
        base: parent_of(repo, intro)?,
        tip: head,
    })
}

/// `target` doesn't have the file yet — a *descendant* introduces it. Plant the
/// introduced blob at `target`; the replay carries it forward from there, and
/// the commit that used to introduce it re-adds byte-identical content, which
/// the 3-way merge resolves to a no-op. Nothing needs removing, and no commit in
/// between can have modified a file that didn't exist yet — so unlike the other
/// direction there is no "modified across the span" case to reject.
fn plant_at_target(repo: &Repository, path: &str, target: Oid, head: Oid) -> Result<Plan> {
    // ponytail: first descendant carrying the path wins. A file deleted *at*
    // `target` and re-added later is resurrected rather than rejected; add a
    // guard if anyone ever hits it.
    let found = git::linear_commits(repo, Some(target), head)?
        .iter()
        .find_map(|c| {
            let e = c.tree().ok()?.get_path(Path::new(path)).ok()?;
            Some((e.id(), e.filemode()))
        });
    let Some((blob, mode)) = found else {
        return Err(Error::PathNotFound { path: path.into() });
    };

    let mut recipe = Recipe::new();
    recipe.add(target, Edit::SetFile { path: path.into(), blob, mode });
    Ok(Plan {
        recipe,
        base: parent_of(repo, target)?,
        tip: head,
    })
}
