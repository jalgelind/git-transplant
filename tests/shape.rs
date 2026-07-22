//! M4 "shape": drop / reorder / squash / split — the operations that change the
//! SHAPE of a stack rather than the contents of one commit.
//!
//! All four are plan-builders over the same replay, so each test pins two
//! things: the resulting stack, and that a failure leaves the branch
//! byte-identical (ref AND reflog untouched — nothing to `--abort`).

mod common;

use common::TestRepo;
use git2::Oid;
use git_transplant::{engine, ops, Error};

/// Three commits, each adding its own file — independent, so any order works.
fn independent(t: &TestRepo) -> (Oid, Oid, Oid) {
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let c2 = t.commit("c2", &[("b.txt", "2\n")]);
    let c3 = t.commit("c3", &[("c.txt", "3\n")]);
    (c1, c2, c3)
}

/// Three commits all rewriting the SAME single line — nothing here commutes.
fn chained(t: &TestRepo) -> (Oid, Oid, Oid) {
    let c1 = t.commit("c1", &[("a.txt", "one\n")]);
    let c2 = t.commit("c2", &[("a.txt", "two\n")]);
    let c3 = t.commit("c3", &[("a.txt", "three\n")]);
    (c1, c2, c3)
}

/// Everything a failed operation must leave exactly as it found it.
fn snapshot(t: &TestRepo) -> (Oid, usize) {
    (t.branch_oid(), t.reflog_len())
}

fn summary(t: &TestRepo, oid: Oid) -> String {
    t.repo.find_commit(oid).unwrap().summary().unwrap_or("").to_string()
}

fn message(t: &TestRepo, oid: Oid) -> String {
    t.repo.find_commit(oid).unwrap().message().unwrap_or("").to_string()
}

fn branch_at(t: &TestRepo, name: &str, oid: Oid) {
    t.repo.reference(&format!("refs/heads/{name}"), oid, false, "test").unwrap();
}

fn oid_of(t: &TestRepo, name: &str) -> Oid {
    t.repo.refname_to_id(&format!("refs/heads/{name}")).unwrap()
}

// ── the insight the whole milestone rests on ────────────────────────────────

#[test]
fn the_replay_takes_an_explicit_order() {
    // `replay` merges every commit against ITS OWN original parent tree, so the
    // walk was always an order-agnostic cherry-pick. Handing it a permuted list
    // is the entire mechanism behind drop/reorder/squash/split.
    let t = TestRepo::new();
    let (c1, c2, c3) = independent(&t);
    let r = engine::replay_order(
        &t.repo,
        Some(c1),
        c3,
        &[c3, c2],
        &engine::Recipe::new(),
        false,
        true,
    )
    .unwrap();
    assert_eq!(t.read_at(t.nth_parent(r.tip, 1), "c.txt").as_deref(), Some("3\n"));
    assert_eq!(t.read_at(r.tip, "b.txt").as_deref(), Some("2\n"));
    assert_eq!(t.read_at(r.tip, "c.txt").as_deref(), Some("3\n"));
    assert_eq!(t.branch_oid(), c3, "a replay still moves no ref");
}

// ── drop ────────────────────────────────────────────────────────────────────

#[test]
fn drop_removes_the_commit_and_replays_the_rest() {
    let t = TestRepo::new();
    let (_c1, c2, _c3) = independent(&t);

    let out = ops::drop_commit(&t.repo, &c2.to_string(), &Default::default()).unwrap();

    assert_eq!(t.commit_count(), 2, "c2 is gone");
    assert_eq!(t.read_at(out.new_tip, "b.txt"), None, "its change vanished with it");
    assert_eq!(t.read_at(out.new_tip, "a.txt").as_deref(), Some("1\n"));
    assert_eq!(t.read_at(out.new_tip, "c.txt").as_deref(), Some("3\n"), "c3 replayed on top");
    assert_eq!(summary(&t, out.new_tip), "c3");
}

#[test]
fn drop_conflict_leaves_the_branch_byte_identical() {
    let t = TestRepo::new();
    let (_c1, c2, _c3) = chained(&t);
    let before = snapshot(&t);

    let err = ops::drop_commit(&t.repo, &c2.to_string(), &Default::default()).unwrap_err();

    assert!(matches!(err, Error::Conflict { .. }), "got {err}");
    assert_eq!(snapshot(&t), before, "ref and reflog untouched");
    assert!(t.is_clean(), "and no half-rebase left in the worktree");
}

#[test]
fn drop_keeps_the_restack_mapping_correct() {
    let t = TestRepo::new();
    let (c1, c2, c3) = independent(&t);
    branch_at(&t, "mid", c2); // sits on the commit being dropped
    branch_at(&t, "late", c3);

    let out = ops::drop_commit(&t.repo, &c2.to_string(), &Default::default()).unwrap();

    assert_eq!(oid_of(&t, "mid"), c1, "a ref on the dropped commit lands on its parent");
    assert_eq!(oid_of(&t, "late"), out.new_tip, "later refs follow their rewrite");
    assert!(out.warnings.is_empty(), "nothing stranded, got {:?}", out.warnings);
}

// ── reorder ─────────────────────────────────────────────────────────────────

#[test]
fn reorder_moves_a_commit_and_carries_every_change() {
    let t = TestRepo::new();
    let (c1, c2, c3) = independent(&t);

    // c3 --before c2  =>  c1, c3, c2
    let out = ops::reorder(&t.repo, &c3.to_string(), &c2.to_string(), true, &Default::default())
        .unwrap();

    assert_eq!(t.commit_count(), 3, "reorder loses nothing");
    let mid = t.nth_parent(out.new_tip, 1);
    assert_eq!((summary(&t, mid).as_str(), summary(&t, out.new_tip).as_str()), ("c3", "c2"));
    assert_eq!(t.read_at(mid, "c.txt").as_deref(), Some("3\n"), "c3 now comes first");
    assert_eq!(t.read_at(mid, "b.txt"), None, "b.txt only arrives with c2, now on top");
    assert_eq!(t.read_at(out.new_tip, "b.txt").as_deref(), Some("2\n"));
    assert_eq!(t.read_at(out.new_tip, "c.txt").as_deref(), Some("3\n"));
    assert_eq!(t.nth_parent(out.new_tip, 2), c1, "the untouched prefix is not rewritten");
}

#[test]
fn reorder_after_is_the_mirror_of_before() {
    let t = TestRepo::new();
    let (_c1, c2, c3) = independent(&t);

    // c2 --after c3 is the same request said the other way round.
    let out = ops::reorder(&t.repo, &c2.to_string(), &c3.to_string(), false, &Default::default())
        .unwrap();

    assert_eq!(summary(&t, t.nth_parent(out.new_tip, 1)), "c3");
    assert_eq!(summary(&t, out.new_tip), "c2");
}

#[test]
fn reorder_conflict_leaves_the_branch_byte_identical() {
    let t = TestRepo::new();
    let (_c1, c2, c3) = chained(&t);
    let before = snapshot(&t);

    let err = ops::reorder(&t.repo, &c3.to_string(), &c2.to_string(), true, &Default::default())
        .unwrap_err();

    assert!(matches!(err, Error::Conflict { .. }), "got {err}");
    assert_eq!(snapshot(&t), before, "ref and reflog untouched");
    assert!(t.is_clean());
}

#[test]
fn reorder_keeps_the_restack_mapping_correct() {
    let t = TestRepo::new();
    let (_c1, c2, c3) = independent(&t);
    branch_at(&t, "mid", c2);

    let out = ops::reorder(&t.repo, &c3.to_string(), &c2.to_string(), true, &Default::default())
        .unwrap();

    // A branch follows its COMMIT, not its old position: c2 is now the tip.
    assert_eq!(oid_of(&t, "mid"), out.new_tip);
    assert!(out.restacked.iter().any(|r| r.starts_with("mid ")), "got {:?}", out.restacked);
}

// ── squash ──────────────────────────────────────────────────────────────────

#[test]
fn squash_folds_into_the_parent_and_keeps_both_messages() {
    let t = TestRepo::new();
    let (_c1, c2, _c3) = independent(&t);

    let out = ops::squash(&t.repo, &c2.to_string(), None, &Default::default()).unwrap();

    assert_eq!(t.commit_count(), 2);
    let folded = t.nth_parent(out.new_tip, 1);
    assert_eq!(message(&t, folded), "c1\n\nc2\n", "neither message is thrown away");
    assert_eq!(t.read_at(folded, "a.txt").as_deref(), Some("1\n"));
    assert_eq!(t.read_at(folded, "b.txt").as_deref(), Some("2\n"), "c2's change came along");
    assert_eq!(t.read_at(out.new_tip, "c.txt").as_deref(), Some("3\n"));
}

#[test]
fn squash_takes_an_explicit_message() {
    let t = TestRepo::new();
    let (_c1, c2, _c3) = independent(&t);

    let out = ops::squash(&t.repo, &c2.to_string(), Some("both at once"), &Default::default())
        .unwrap();

    assert_eq!(message(&t, t.nth_parent(out.new_tip, 1)), "both at once");
}

#[test]
fn squash_cannot_conflict_even_on_a_chain() {
    // By construction: the child's delta is merged onto the parent's OWN original
    // tree, so `ours` == `base` and the merge is trivial. The pathological
    // same-line fixture that makes drop and reorder conflict is clean here.
    let t = TestRepo::new();
    let (_c1, c2, _c3) = chained(&t);

    let out = ops::squash(&t.repo, &c2.to_string(), None, &Default::default()).unwrap();

    assert_eq!(t.commit_count(), 2);
    assert_eq!(t.read_at(t.nth_parent(out.new_tip, 1), "a.txt").as_deref(), Some("two\n"));
    assert_eq!(t.read_at(out.new_tip, "a.txt").as_deref(), Some("three\n"));
}

#[test]
fn squashing_the_oldest_commit_refuses_and_changes_nothing() {
    // Squash's byte-identical case: it has no conflict to hit, so the failure
    // that must leave the branch alone is the one where there is no parent.
    let t = TestRepo::new();
    let (c1, _c2, _c3) = independent(&t);
    let before = snapshot(&t);

    let err = ops::squash(&t.repo, &c1.to_string(), None, &Default::default()).unwrap_err();

    assert!(format!("{err}").contains("oldest commit"), "got {err}");
    assert_eq!(snapshot(&t), before);
    assert!(t.is_clean());
}

#[test]
fn squash_keeps_the_restack_mapping_correct() {
    let t = TestRepo::new();
    let (_c1, c2, _c3) = independent(&t);
    branch_at(&t, "mid", c2);

    let out = ops::squash(&t.repo, &c2.to_string(), None, &Default::default()).unwrap();

    // The commit that swallowed c2 is where a ref on c2 belongs: same content.
    assert_eq!(oid_of(&t, "mid"), t.nth_parent(out.new_tip, 1));
}

// ── split ───────────────────────────────────────────────────────────────────

#[test]
fn split_makes_two_commits_from_one() {
    let t = TestRepo::new();
    t.commit("base", &[("keep.txt", "k\n")]);
    let c2 = t.commit("two things", &[("a.txt", "A\n"), ("b.txt", "B\n")]);

    let out = ops::split(&t.repo, &c2.to_string(), &["a.txt".into()], None, &Default::default())
        .unwrap();

    assert_eq!(t.commit_count(), 3);
    let first = t.nth_parent(out.new_tip, 1);
    assert_eq!(summary(&t, first), "two things (part 1)");
    assert_eq!(t.read_at(first, "a.txt").as_deref(), Some("A\n"));
    assert_eq!(t.read_at(first, "b.txt"), None, "only the named path split off");
    assert_eq!(summary(&t, out.new_tip), "two things", "the remainder keeps the message");
    assert_eq!(t.read_at(out.new_tip, "a.txt").as_deref(), Some("A\n"));
    assert_eq!(t.read_at(out.new_tip, "b.txt").as_deref(), Some("B\n"));
}

#[test]
fn split_refuses_to_take_the_whole_commit() {
    let t = TestRepo::new();
    t.commit("base", &[("keep.txt", "k\n")]);
    let c2 = t.commit("two things", &[("a.txt", "A\n"), ("b.txt", "B\n")]);
    let before = snapshot(&t);

    let paths = vec!["a.txt".to_string(), "b.txt".to_string()];
    let err = ops::split(&t.repo, &c2.to_string(), &paths, None, &Default::default()).unwrap_err();

    assert!(format!("{err}").contains("nothing would be left"), "got {err}");
    assert_eq!(snapshot(&t), before, "ref and reflog untouched");
    assert!(t.is_clean());
}

#[test]
fn split_cannot_conflict_and_an_unknown_path_changes_nothing() {
    // Same argument as squash: the split-off commit carries the target's OWN
    // content for those paths, so the target replays onto a tree whose
    // overlapping changes are byte-identical — its rewritten tree is unchanged,
    // and nothing above it can break either. The byte-identical case is a
    // refusal, not a conflict.
    let t = TestRepo::new();
    t.commit("base", &[("a.txt", "one\n"), ("keep.txt", "k\n")]);
    let c2 = t.commit("edit and add", &[("a.txt", "two\n"), ("new.txt", "n\n")]);
    t.commit("c3", &[("a.txt", "three\n")]); // re-edits the very same line
    let before = snapshot(&t);

    let miss = vec!["nope.txt".to_string()];
    let err = ops::split(&t.repo, &c2.to_string(), &miss, None, &Default::default()).unwrap_err();
    assert!(matches!(err, Error::PathNotFound { .. }), "got {err}");
    assert_eq!(snapshot(&t), before, "ref and reflog untouched");
    assert!(t.is_clean());

    let paths = vec!["a.txt".to_string()];
    let out = ops::split(&t.repo, &c2.to_string(), &paths, None, &Default::default()).unwrap();
    assert_eq!(t.commit_count(), 4, "base, split-off, remainder, c3");
    assert_eq!(t.read_at(out.new_tip, "a.txt").as_deref(), Some("three\n"), "c3 still on top");
    let split_off = t.nth_parent(out.new_tip, 2);
    assert_eq!(t.read_at(split_off, "a.txt").as_deref(), Some("two\n"));
    assert_eq!(t.read_at(split_off, "new.txt"), None, "the add stayed behind");
}

// ── the accidental squash ───────────────────────────────────────────────────

#[test]
fn a_commit_emptied_by_the_replay_is_reported_not_silent() {
    // `drop_empty` removes a commit whose change ended up already present — and
    // takes its message with it. That is the ACCIDENTAL squash; it is now named
    // in `Replay::dropped` (and printed by the CLI) instead of vanishing.
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "one\n")]);
    let c2 = t.commit("c2", &[("a.txt", "two\n")]);

    let mut recipe = engine::Recipe::new();
    recipe.add(c1, engine::Edit::ApplyChange(c2));
    let r = engine::replay(&t.repo, None, c2, &recipe, false, true).unwrap();

    assert_eq!(r.dropped, vec![c2], "the emptied commit is named");
    assert_eq!(t.read_at(r.tip, "a.txt").as_deref(), Some("two\n"));
}
