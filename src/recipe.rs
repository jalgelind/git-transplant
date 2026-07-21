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

/// op B — re-anchor `path` at `target`: remove it from every ancestor of
/// `target` that carries it (requires the content be unchanged across that
/// span), keep `target`'s copy.
pub fn mv(repo: &Repository, path: &str, target: Oid, head: Oid) -> Result<Plan> {
    if !git::is_ancestor(repo, target, head)? {
        return Err(Error::TargetNotAncestor);
    }
    let target_commit = repo.find_commit(target)?;
    let blob_target = target_commit
        .tree()?
        .get_path(Path::new(path))
        .map_err(|_| Error::PathNotFound { path: path.into() })?
        .id();

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

    // Re-add target's copy after the merge strips it during replay.
    recipe.add(
        target,
        Edit::SetFile {
            path: path.into(),
            blob: blob_target,
        },
    );

    Ok(Plan {
        recipe,
        base: parent_of(repo, intro)?,
        tip: head,
    })
}
