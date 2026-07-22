//! M3 "Stacked-PR safe": rewriting a stack carries sibling branches with it.
//! The warn-only behaviour is still reachable through `--no-restack`
//! (`gaps::fix_warns_about_abandoned_branch_only_with_no_restack`).

mod common;

use common::*;
use git2::Oid;
use git_transplant::{engine, ops};

/// c1, c2, c3 with `mid` on c2 — the shape every stacked-PR tool produces.
fn stack(t: &TestRepo) -> (Oid, Oid, Oid) {
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let c2 = t.commit("c2", &[("a.txt", "1\n"), ("b.txt", "x\n")]);
    let c3 = t.commit("c3", &[("a.txt", "1\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);
    (c1, c2, c3)
}

#[test]
fn mid_stack_branch_follows_the_rewrite() {
    let t = TestRepo::new();
    let (c1, c2, _c3) = stack(&t);
    branch_at(&t, "mid", c2);
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);

    let out = ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();

    let mid = oid_of(&t, "mid");
    assert_ne!(mid, c2, "mid must not be stranded on the orphaned c2");
    assert_eq!(mid, t.nth_parent(out.new_tip, 1), "mid is c2's rewritten counterpart");
    assert_eq!(
        t.read_at(mid, "a.txt").as_deref(),
        Some("1-fixed\n"),
        "and it carries the fix that was folded in below it"
    );
    assert!(
        out.restacked.iter().any(|r| r.starts_with("mid ")),
        "the move is reported, got {:?}",
        out.restacked
    );
    assert!(out.warnings.is_empty(), "nothing left to warn about, got {:?}", out.warnings);
}

#[test]
fn a_tag_is_never_moved() {
    let t = TestRepo::new();
    let (c1, c2, _c3) = stack(&t);
    // A lightweight tag on the commit the fix will rewrite.
    t.repo.reference("refs/tags/v1", c2, false, "test").unwrap();
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);

    let out = ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();

    assert_eq!(
        t.repo.refname_to_id("refs/tags/v1").unwrap(),
        c2,
        "a tag names a specific historical commit; it must stay put"
    );
    assert!(out.restacked.is_empty(), "a tag is not a restack, got {:?}", out.restacked);
    assert!(
        out.warnings.iter().any(|w| w.contains("v1")),
        "but it is still warned about, got {:?}",
        out.warnings
    );
}

#[test]
fn branch_on_a_dropped_commit_lands_on_its_rewritten_parent() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "one\n")]);
    // c2's whole change is `a.txt: one -> two`. Staging the inverse and absorbing
    // it folds the revert into c2 itself, which empties and is DROPPED.
    let c2 = t.commit("c2", &[("a.txt", "two\n")]);
    let c3 = t.commit("c3", &[("a.txt", "two\n"), ("b.txt", "x\n")]);
    branch_at(&t, "doomed", c2);
    t.stage(&[("a.txt", "one\n"), ("b.txt", "x\n")]);

    let a = ops::collapse(&t.repo, None, &Default::default()).unwrap();
    let out = a.outcome.expect("something absorbed");
    assert_eq!(t.commit_count(), 2, "c2 emptied and was dropped ({c1:.8}, {c3:.8} remain)");

    // c2 no longer exists in the rewritten stack. Its tree is by definition equal
    // to its rewritten parent's — that's *why* it was dropped — so sending the ref
    // there keeps it naming the same content instead of stranding it.
    let doomed = oid_of(&t, "doomed");
    assert_eq!(doomed, t.nth_parent(out.new_tip, 1), "landed on c1, c2's rewritten parent");
    assert_eq!(t.read_at(doomed, "a.txt").as_deref(), Some("one\n"), "same content as before");
    assert!(out.restacked.iter().any(|r| r.starts_with("doomed ")));
}

#[test]
fn a_dropped_commit_maps_to_its_rewritten_parent() {
    // The engine-level statement of the above, without the ops layer.
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "one\n")]);
    let c2 = t.commit("c2", &[("a.txt", "two\n")]);
    let mut recipe = engine::Recipe::new();
    recipe.add(c2, engine::Edit::RevertChange(c2)); // empties c2

    let r = engine::replay(&t.repo, None, c2, &recipe, false, true).unwrap();
    assert_eq!(r.map.get(&c1), Some(&r.tip), "c1 survived and is the new tip");
    assert_eq!(r.map.get(&c2), Some(&r.tip), "c2 was dropped -> its rewritten parent");
}

#[test]
fn a_sibling_outside_the_rewritten_range_is_untouched() {
    let t = TestRepo::new();
    let (c1, c2, _c3) = stack(&t);
    branch_at(&t, "below", c1); // c1 is the fix target's parent — never rewritten
    t.stage(&[("a.txt", "1\n"), ("b.txt", "x-fixed\n"), ("c.txt", "y\n")]);

    let out = ops::fix(&t.repo, &c2.to_string(), &Default::default()).unwrap();
    assert_eq!(oid_of(&t, "below"), c1, "not in the map, so not moved");
    assert!(out.restacked.is_empty(), "and not reported as moved, got {:?}", out.restacked);
}

#[test]
fn undo_refuses_to_clobber_a_sibling_that_moved_since() {
    let t = TestRepo::new();
    let (c1, c2, _c3) = stack(&t);
    branch_at(&t, "mid", c2);
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);
    ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();

    // Same compare-and-swap discipline as the branch itself: `mid` has moved on
    // since the restack, so undo must leave it where its owner put it.
    t.repo.reference("refs/heads/mid", c1, true, "committed elsewhere").unwrap();
    let u = ops::undo(&t.repo, false).unwrap();
    assert_eq!(oid_of(&t, "mid"), c1, "sibling that moved since is not clobbered");
    assert!(u.restacked.is_empty(), "and not claimed as restored, got {:?}", u.restacked);
}

#[test]
fn a_branch_checked_out_in_another_worktree_is_refused() {
    let t = TestRepo::new();
    let (c1, c2, _c3) = stack(&t);
    branch_at(&t, "mid", c2);
    let wt = t.dir.join("../wt-mid");
    let mut opts = git2::WorktreeAddOptions::new();
    let mid_ref = t.repo.find_reference("refs/heads/mid").unwrap();
    opts.reference(Some(&mid_ref));
    t.repo.worktree("wt-mid", &wt, Some(&opts)).unwrap();
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);

    let out = ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();

    assert_eq!(oid_of(&t, "mid"), c2, "moving it would desync that worktree's HEAD");
    assert!(out.restacked.is_empty());
    assert!(
        out.warnings.iter().any(|w| w.contains("mid") && w.contains("worktree")),
        "the refusal is reported, got {:?}",
        out.warnings
    );
    let _ = std::fs::remove_dir_all(&wt);
}

#[test]
fn undo_walks_the_siblings_back_too() {
    let t = TestRepo::new();
    let (c1, c2, _c3) = stack(&t);
    branch_at(&t, "mid", c2);
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);

    ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();
    assert_ne!(oid_of(&t, "mid"), c2);

    let u = ops::undo(&t.repo, false).unwrap();
    assert_eq!(oid_of(&t, "mid"), c2, "undo restores the whole stack, not just the branch");
    assert!(u.restacked.iter().any(|r| r.starts_with("mid ")), "reported, got {:?}", u.restacked);
}

#[test]
fn dry_run_moves_no_sibling() {
    let t = TestRepo::new();
    let (c1, c2, _c3) = stack(&t);
    branch_at(&t, "mid", c2);
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);

    let opts = ops::Opts { dry_run: true, ..Default::default() };
    let out = ops::fix(&t.repo, &c1.to_string(), &opts).unwrap();
    assert_eq!(oid_of(&t, "mid"), c2, "dry run changes nothing");
    assert!(
        out.restacked.iter().any(|r| r.starts_with("mid ")),
        "but it does report what it would move, got {:?}",
        out.restacked
    );
}

/// The tip is OUTSIDE the rewritten range, but the fork point is inside it — so
/// the branch still resolves and still pushes while sitting on orphaned history.
/// Checking only ref tips missed exactly this, which is the silent failure
/// restacking exists to prevent.
#[test]
fn a_branch_that_forked_inside_the_rewritten_range_is_warned_about() {
    let t = TestRepo::new();
    let (c1, c2, _c3) = stack(&t);
    // `feat` forks at c2 and carries a commit of its own.
    let feat_tip = {
        let sig = t.repo.signature().unwrap();
        let base = t.repo.find_commit(c2).unwrap();
        let tree = base.tree().unwrap();
        t.repo.commit(None, &sig, &sig, "feat work", &tree, &[&base]).unwrap()
    };
    branch_at(&t, "feat", feat_tip);
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);

    let out = ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();

    assert_eq!(oid_of(&t, "feat"), feat_tip, "not moved: landing it is a rebase, not a ref move");
    assert!(out.restacked.is_empty(), "and not claimed as restacked, got {:?}", out.restacked);
    let w = out.warnings.join("\n");
    assert!(w.contains("feat forked at"), "the fork point is named, got {w}");
    assert!(w.contains("git rebase --onto"), "and the fix is spelled out, got {w}");
}

/// A branch forked *below* the rewrite is not disturbed by it and must stay
/// quiet — a warning on every unrelated branch would train people to ignore them.
#[test]
fn a_branch_that_forked_below_the_rewrite_is_not_warned_about() {
    let t = TestRepo::new();
    let (c1, c2, _c3) = stack(&t);
    let old = {
        let sig = t.repo.signature().unwrap();
        let base = t.repo.find_commit(c1).unwrap();
        let tree = base.tree().unwrap();
        t.repo.commit(None, &sig, &sig, "old work", &tree, &[&base]).unwrap()
    };
    branch_at(&t, "old", old);
    t.stage(&[("a.txt", "1\n"), ("b.txt", "x-fixed\n"), ("c.txt", "y\n")]);

    // Rewrites c2..c3 only; `old` forks at c1, below the range.
    let out = ops::fix(&t.repo, &c2.to_string(), &Default::default()).unwrap();
    assert_eq!(oid_of(&t, "old"), old);
    assert!(out.warnings.is_empty(), "nothing to say about it, got {:?}", out.warnings);
}

/// A refdb we cannot read must REFUSE, not report that nothing was stranded.
/// A corrupt `packed-refs` is the reachable version of that (probed: git2's
/// `references()` returns an error for it, byte for byte this content).
#[test]
fn a_corrupt_ref_listing_refuses_and_leaves_the_branch_alone() {
    let t = TestRepo::new();
    let (c1, _c2, _c3) = stack(&t);
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);
    let before = snapshot(&t);
    std::fs::write(t.dir.join(".git/packed-refs"), "this is not a packed-refs file\n").unwrap();

    let r = ops::fix(&t.repo, &c1.to_string(), &Default::default());
    assert!(r.is_err(), "must refuse, got {r:?}");
    assert_eq!(snapshot(&t), before, "and leave the branch (and its reflog) untouched");
}

/// The nastier half of the same hole, found by probing rather than by reading:
/// an unreadable `refs/heads/` makes `references()` succeed and yield NOTHING.
/// No error to propagate — so the tell is our own branch missing from a listing
/// it must be in.
#[test]
fn a_listing_without_our_own_branch_is_refused() {
    let t = TestRepo::new();
    stack(&t);
    let r = ops::sibling_refs(&t.repo, "refs/heads/not-listed");
    assert!(r.is_err(), "an incomplete listing cannot clear anything, got {r:?}");
}

/// `refs/stash` is deliberately left alone. A stash is applied as a 3-way merge
/// of `stash^..stash` onto whatever HEAD is now, and `refs/stash` keeps its own
/// base commit alive, so rewriting that base does not strand it. This asserts
/// the claim rather than trusting it.
#[test]
fn a_stash_still_applies_after_its_base_is_rewritten() {
    let mut t = TestRepo::new();
    let (c1, _c2, _c3) = stack(&t);
    std::fs::write(t.dir.join("c.txt"), "y-wip\n").unwrap();
    let sig = t.repo.signature().unwrap();
    let stash = t.repo.stash_save(&sig, "wip", None).unwrap();

    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n"), ("c.txt", "y\n")]);
    let out = ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();
    assert_ne!(out.new_tip, out.old_tip, "the stash's base commit really was rewritten");
    assert_eq!(t.repo.refname_to_id("refs/stash").unwrap(), stash, "the stash ref never moves");

    t.repo.stash_apply(0, None).expect("the stash still applies");
    assert_eq!(
        std::fs::read_to_string(t.dir.join("c.txt")).unwrap(),
        "y-wip\n",
        "and it restores the work it held"
    );
}
