//! `reword` — the one rewrite that changes no tree at all.

mod common;
use common::TestRepo;

use git_transplant::{ops, Error};

fn message(t: &TestRepo, oid: git2::Oid) -> String {
    t.repo.find_commit(oid).unwrap().message().unwrap_or("").to_string()
}

#[test]
fn reword_replaces_the_message_and_keeps_everything_else() {
    let t = TestRepo::new();
    let c1 = t.commit("typpo in the summry", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "1\n"), ("b.txt", "2\n")]);

    let orig = t.repo.find_commit(c1).unwrap();
    let (tree, author, when) = (
        orig.tree().unwrap().id(),
        orig.author().name().unwrap().to_string(),
        orig.author().when(),
    );

    let out = ops::reword(&t.repo, &c1.to_string(), "typo in the summary", &Default::default())
        .unwrap();

    let new_c1 = t.repo.find_commit(t.nth_parent(out.new_tip, 1)).unwrap();
    assert_eq!(new_c1.message(), Some("typo in the summary\n"));
    assert_eq!(new_c1.tree().unwrap().id(), tree, "the content is untouched");
    assert_eq!(new_c1.author().name().unwrap(), author, "author preserved");
    assert_eq!(new_c1.author().when(), when, "author date preserved");
    assert_eq!(message(&t, out.new_tip), "c2", "the descendant replayed byte-for-byte");
    assert_eq!(t.commit_count(), 2);
}

/// The tree never changes, so — unlike every other verb — this one has no reason
/// to demand a clean worktree, and never checks anything out.
#[test]
fn reword_works_with_work_in_progress_on_disk() {
    let t = TestRepo::new();
    let c1 = t.commit("wrong", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "1\n"), ("b.txt", "2\n")]);
    t.dirty("b.txt", "half-finished\n");

    let out = ops::reword(&t.repo, &c1.to_string(), "right", &Default::default()).unwrap();

    assert_eq!(message(&t, t.nth_parent(out.new_tip, 1)), "right\n");
    assert_eq!(
        std::fs::read_to_string(t.dir.join("b.txt")).unwrap(),
        "half-finished\n",
        "the WIP is still there"
    );
}

#[test]
fn reword_of_a_non_ancestor_changes_nothing() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let c2 = t.commit("c2", &[("a.txt", "2\n")]);
    // a commit on another branch: not in HEAD's history
    let sig = t.repo.signature().unwrap();
    let c1c = t.repo.find_commit(c1).unwrap();
    let off = t.repo.commit(None, &sig, &sig, "elsewhere", &c1c.tree().unwrap(), &[&c1c]).unwrap();
    let before = (t.branch_oid(), t.reflog_len());

    let err = ops::reword(&t.repo, &off.to_string(), "nope", &Default::default()).unwrap_err();

    assert!(matches!(err, Error::TargetNotAncestor), "got {err}");
    assert_eq!((t.branch_oid(), t.reflog_len()), before, "ref and reflog untouched");
    assert_eq!(t.head(), c2);
}

#[test]
fn dry_run_reword_reports_the_tip_and_moves_nothing() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "2\n")]);
    let opts = ops::Opts { dry_run: true, ..Default::default() };

    let out = ops::reword(&t.repo, &c1.to_string(), "renamed", &opts).unwrap();

    assert_ne!(out.new_tip, out.old_tip, "it still reports the tip it would produce");
    assert_eq!(t.branch_oid(), out.old_tip, "but the branch has not moved");
    assert_eq!(message(&t, t.nth_parent(out.new_tip, 1)), "renamed\n", "the objects exist");
}
