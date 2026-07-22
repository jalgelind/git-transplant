//! Build a `Recipe` (+ replay range) for each operation from git state.
//! Selection lives here; execution lives in `engine::replay`.

use std::path::Path;

use git2::{Oid, Repository};

use crate::engine::{self, Edit, Recipe};
use crate::git;
use crate::{Error, Result};

/// A recipe plus the range to replay.
pub struct Plan {
    pub recipe: Recipe,
    pub base: Option<Oid>,
    pub tip: Oid,
}

/// A plan that replays an EXPLICIT commit order — the shape operations. `order`
/// is oldest-first and may drop, permute, or insert relative to the real graph.
pub struct Shaped {
    pub recipe: Recipe,
    pub base: Option<Oid>,
    pub tip: Oid,
    pub order: Vec<Oid>,
}

/// First parent of `c`, or None if it is a root commit — i.e. the `base` to
/// replay `c` itself from. Public because every caller that builds a range needs
/// exactly this, `ops` included.
pub fn parent_of(repo: &Repository, c: Oid) -> Result<Option<Oid>> {
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

// ── shape operations: drop / reorder / squash / split ───────────────────────
//
// All four are plan-builders. `engine::replay` merges every commit against its
// OWN original parent tree, so the walk was always an order-agnostic
// cherry-pick — these just hand it a different list.

/// The rewritable stack below `head`, oldest-first: `base..head` if given,
/// otherwise the linear run bounded by the first merge (the window `absorb`
/// offers). Every shape verb takes the same `base` so that a caller showing a
/// BOUNDED stack (the TUI's `--base`) plans against exactly the list it showed —
/// planning a reorder against a longer list would silently drop the difference.
pub fn stack(repo: &Repository, base: Option<Oid>, head: Oid) -> Result<Vec<Oid>> {
    let cs = match base {
        Some(_) => git::linear_commits(repo, base, head)?,
        None => git::linear_window(repo, head)?,
    };
    Ok(cs.iter().map(|c| c.id()).collect())
}

fn locate(ids: &[Oid], rev: Oid) -> Result<usize> {
    ids.iter().position(|&o| o == rev).ok_or(Error::TargetNotAncestor)
}

/// Package `want` (the desired oldest-first order) as a plan, trimming the
/// untouched common prefix so a rewrite only ever starts where the shape first
/// differs. `edited` forces an earlier start for a commit the recipe changes in
/// place even though it sits inside that prefix (squash's parent).
fn shaped(
    repo: &Repository,
    head: Oid,
    ids: &[Oid],
    want: Vec<Oid>,
    recipe: Recipe,
    edited: Option<usize>,
) -> Result<Shaped> {
    if want.as_slice() == ids {
        return Err(Error::Empty("the stack already has that shape".into()));
    }
    let diff = ids.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(want.len().min(ids.len()));
    let lo = edited.unwrap_or(diff).min(diff).min(ids.len() - 1);
    let base = parent_of(repo, ids[lo])?;
    let order = want[lo..].to_vec();
    if base.is_none() && order.is_empty() {
        return Err(Error::Empty("that would leave the branch with no commits at all".into()));
    }
    Ok(Shaped { recipe, base, tip: head, order })
}

/// Replay the stack in `want` order (oldest-first) — any permutation and/or
/// subset of it. Reorder and drop are both just this; the TUI's commit pane
/// hands over the order it is showing.
pub fn reshape(repo: &Repository, base: Option<Oid>, head: Oid, want: Vec<Oid>) -> Result<Shaped> {
    let ids = stack(repo, base, head)?;
    shaped(repo, head, &ids, want, Recipe::new(), None)
}

/// Omit `rev`; everything after it replays as if it had never been committed.
pub fn drop_commit(repo: &Repository, base: Option<Oid>, head: Oid, rev: Oid) -> Result<Shaped> {
    let ids = stack(repo, base, head)?;
    locate(&ids, rev)?;
    let want = ids.iter().copied().filter(|&o| o != rev).collect();
    shaped(repo, head, &ids, want, Recipe::new(), None)
}

/// Move `rev` to sit immediately before (older side) or after `anchor`.
pub fn reorder(
    repo: &Repository,
    base: Option<Oid>,
    head: Oid,
    rev: Oid,
    anchor: Oid,
    before: bool,
) -> Result<Shaped> {
    if rev == anchor {
        return Err(Error::Empty("a commit cannot be moved relative to itself".into()));
    }
    let ids = stack(repo, base, head)?;
    locate(&ids, rev)?;
    locate(&ids, anchor)?;
    let mut want: Vec<Oid> = ids.iter().copied().filter(|&o| o != rev).collect();
    let j = locate(&want, anchor)?; // anchor's index once `rev` is out of the way
    want.insert(if before { j } else { j + 1 }, rev);
    shaped(repo, head, &ids, want, Recipe::new(), None)
}

/// Fold `rev` into its parent: drop it from the order and inject its change at
/// the parent, whose message absorbs `rev`'s (or `msg`, if given).
pub fn squash(
    repo: &Repository,
    base: Option<Oid>,
    head: Oid,
    rev: Oid,
    msg: Option<&str>,
) -> Result<Shaped> {
    let ids = stack(repo, base, head)?;
    let i = locate(&ids, rev)?;
    if i == 0 {
        return Err(Error::Empty(format!(
            "{rev:.8} is the oldest commit in the stack — nothing to squash it into"
        )));
    }
    let parent = ids[i - 1];
    let mut recipe = Recipe::new();
    recipe.add(parent, Edit::ApplyChange(rev));
    recipe.set_message(
        parent,
        msg.map(str::to_string).unwrap_or_else(|| concat_messages(repo, parent, rev)),
    );
    let want = ids.iter().copied().filter(|&o| o != rev).collect();
    shaped(repo, head, &ids, want, recipe, Some(i - 1))
}

/// Squash's message policy: keep BOTH, parent first, blank line between — the
/// same choice `git rebase -i`'s `squash` makes. A commit message is something
/// the user typed; discarding half of it silently is how the accidental
/// `drop_empty` squash loses information today. `-m` overrides.
fn concat_messages(repo: &Repository, parent: Oid, child: Oid) -> String {
    let m = |o: Oid| {
        repo.find_commit(o)
            .ok()
            .and_then(|c| c.message().map(str::to_string))
            .unwrap_or_default()
    };
    format!("{}\n\n{}\n", m(parent).trim_end(), m(child).trim_end())
}

/// Split `rev` in two: a new commit carrying only `paths`' changes, then `rev`
/// itself with the remainder.
///
/// Still no new machinery — the split-off commit is a dangling synthetic that
/// simply takes a slot in the replay order ahead of `rev`, and `rev` then
/// replays onto a tree that already holds those paths, so its own 3-way merge
/// drops them idempotently.
pub fn split(
    repo: &Repository,
    base: Option<Oid>,
    head: Oid,
    rev: Oid,
    paths: &[String],
    msg: Option<&str>,
) -> Result<Shaped> {
    let ids = stack(repo, base, head)?;
    locate(&ids, rev)?;
    let c = repo.find_commit(rev)?;
    let parent = c
        .parent(0)
        .map_err(|_| Error::Empty(format!("{rev:.8} is a root commit; nothing to split off")))?;
    let (ptree, ctree) = (parent.tree()?, c.tree()?);

    let mut tree = ptree.id();
    for p in paths {
        // Absent on the child side = `rev` DELETED it, which splits off as a
        // removal. Absent on both sides is just a typo.
        let entry = ctree.get_path(Path::new(p)).ok().map(|e| (e.id(), e.filemode()));
        if entry.is_none() && ptree.get_path(Path::new(p)).is_err() {
            return Err(Error::PathNotFound { path: p.clone() });
        }
        tree = engine::set_path(repo, tree, p, entry)?;
    }
    if tree == ptree.id() {
        return Err(Error::Empty(format!("none of those paths change at {rev:.8}")));
    }
    if tree == ctree.id() {
        return Err(Error::Empty(format!(
            "those paths are the WHOLE of {rev:.8}; nothing would be left behind"
        )));
    }

    let text = msg
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} (part 1)", c.summary().unwrap_or("split")));
    let first = repo.commit(
        None,
        &c.author(),
        &c.committer(),
        &text,
        &repo.find_tree(tree)?,
        &[&parent],
    )?;
    split_at(repo, base, head, rev, first)
}

/// Insert an already-built synthetic commit into the order immediately before
/// `rev` — the whole of split, once the split-off commit exists.
///
/// `first` must be parented at `rev`'s parent, which is exactly what
/// [`crate::patch::synthetic_for_hunks`] produces for a commit source. `rev`
/// then replays onto a tree that already holds those changes, so its own 3-way
/// merge drops them idempotently and nothing above it can break.
///
/// Split by PATH builds `first` from a tree; the TUI builds it from selected
/// HUNKS. Only the construction differs, so only the construction lives apart.
pub fn split_at(
    repo: &Repository,
    base: Option<Oid>,
    head: Oid,
    rev: Oid,
    first: Oid,
) -> Result<Shaped> {
    let ids = stack(repo, base, head)?;
    let i = locate(&ids, rev)?;
    let mut want = ids.clone();
    want.insert(i, first);
    shaped(repo, head, &ids, want, Recipe::new(), None)
}
