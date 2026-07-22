//! `--ours` / `--theirs` / `--union`: resolve conflicting regions by a fixed
//! rule instead of aborting. Zero persisted state — this is the whole of the
//! "conflict resolution" story, and deliberately so.
//!
//! Which side is which, HERE: **ours** is the stack being replayed onto (the
//! rewritten commit the change lands on), **theirs** is the change being applied
//! (the commit being replayed/moved, or the staged hunk). `git rebase`'s sense
//! of the words, not `git merge`'s: ours is never the working copy.

mod common;
use common::TestRepo;

use git2::{FileFavor, Oid};
use git_transplant::{ops, Error};

/// Three commits rewriting the SAME line: c2 and c3 cannot commute, so dropping
/// c2 makes c3 conflict for real.
fn chained(t: &TestRepo) -> (Oid, Oid, Oid) {
    let c1 = t.commit("c1", &[("a.txt", "one\n")]);
    let c2 = t.commit("c2", &[("a.txt", "two\n")]);
    let c3 = t.commit("c3", &[("a.txt", "three\n")]);
    (c1, c2, c3)
}

fn opts(favor: FileFavor) -> ops::Opts {
    ops::Opts { favor: Some(favor), ..Default::default() }
}

#[test]
fn without_a_rule_the_conflict_still_aborts() {
    // The baseline the flags change. (Also asserted in shape.rs; repeated here
    // because these tests are only meaningful against it.)
    let t = TestRepo::new();
    let (_c1, c2, _c3) = chained(&t);
    let before = (t.branch_oid(), t.reflog_len());

    let err = ops::drop_commit(&t.repo, &c2.to_string(), &Default::default()).unwrap_err();

    assert!(matches!(err, Error::Conflict { .. }), "got {err}");
    assert_eq!((t.branch_oid(), t.reflog_len()), before);
}

#[test]
fn theirs_keeps_the_change_being_applied() {
    let t = TestRepo::new();
    let (_c1, c2, _c3) = chained(&t);

    // Dropping c2 replays c3 onto c1. "theirs" = c3, the commit being replayed.
    let out = ops::drop_commit(&t.repo, &c2.to_string(), &opts(FileFavor::Theirs)).unwrap();

    assert_eq!(t.read_at(out.new_tip, "a.txt").as_deref(), Some("three\n"));
    assert_eq!(t.commit_count(), 2, "c2 is gone and c3 survived the conflict");
}

#[test]
fn ours_keeps_the_stack_being_replayed_onto() {
    let t = TestRepo::new();
    let (_c1, c2, _c3) = chained(&t);

    // Same replay, other rule: "ours" = c1's content, already in the stack.
    let out = ops::drop_commit(&t.repo, &c2.to_string(), &opts(FileFavor::Ours)).unwrap();

    assert_eq!(t.read_at(out.new_tip, "a.txt").as_deref(), Some("one\n"));
}

#[test]
fn union_keeps_both_sides_without_markers() {
    let t = TestRepo::new();
    let (_c1, c2, _c3) = chained(&t);

    let out = ops::drop_commit(&t.repo, &c2.to_string(), &opts(FileFavor::Union)).unwrap();

    let text = t.read_at(out.new_tip, "a.txt").unwrap();
    assert!(text.contains("one\n") && text.contains("three\n"), "both sides kept: {text:?}");
    assert!(!text.contains("<<<<"), "and no conflict markers: {text:?}");
}

/// It works on the hunk-folding side too, not just the shape verbs: `fix` aims a
/// staged change at a commit whose line a later commit also rewrote.
#[test]
fn theirs_resolves_a_fix_that_would_otherwise_abort() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("config.txt", "v=1\n")]);
    let _c2 = t.commit("c2", &[("config.txt", "v=2\n")]);
    t.stage(&[("config.txt", "v=3\n")]);

    assert!(ops::fix(&t.repo, &c1.to_string(), &Default::default()).is_err(), "aborts by default");

    let out = ops::fix(&t.repo, &c1.to_string(), &opts(FileFavor::Theirs)).unwrap();

    // The staged change is "theirs", so it wins at c1 — and c2 then replays on
    // top of it under the same rule, which is why the tip reads v=2 again.
    assert_eq!(t.read_at(t.nth_parent(out.new_tip, 1), "config.txt").as_deref(), Some("v=3\n"));
    assert_eq!(t.commit_count(), 2);
}

/// A rule resolves conflicting REGIONS. It is not a promise that nothing can
/// fail — a merge with no file-level resolution still aborts byte-clean.
#[test]
fn a_rule_does_not_silence_unrelated_failures() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "2\n")]);
    let before = (t.branch_oid(), t.reflog_len());

    // Nothing staged is still nothing staged, whatever the conflict rule says.
    let err = ops::fix(&t.repo, &c1.to_string(), &opts(FileFavor::Theirs)).unwrap_err();

    assert!(matches!(err, Error::NothingStaged), "got {err}");
    assert_eq!((t.branch_oid(), t.reflog_len()), before);
}
