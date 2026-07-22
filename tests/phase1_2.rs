//! Phase 1 (op C / `fix`) + Phase 2 (op B / `move`) matrix, per docs/ROADMAP.md.

mod common;
use common::TestRepo;

use git_transplant::engine::{self, Edit, Recipe};
use git_transplant::{ops, Error};

const V1: &str = "fn main() {\n    let value = 1;\n    println!(\"{}\", value);\n}\n";
const V2: &str = "fn main() {\n    let value = 42;\n    println!(\"{}\", value);\n}\n";
const HELPER: &str = "\nfn helper() {}\n";
const OTHER: &str = "\nfn other() {}\n";

fn cat(parts: &[&str]) -> String {
    parts.concat()
}

// ---- Phase 1: fix (op C) --------------------------------------------------

#[test]
fn fix_folds_into_target_and_replays() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("src.rs", V1)]);
    let _c2 = t.commit("c2", &[("src.rs", &cat(&[V1, HELPER]))]);
    let _c3 = t.commit("c3", &[("src.rs", &cat(&[V1, HELPER, OTHER]))]);
    // stage the fix at the tip: value 1 -> 42
    t.stage(&[("src.rs", &cat(&[V2, HELPER, OTHER]))]);

    let out = ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();

    // fix landed in c1 (2 ancestors back from the new tip), without dragging
    // the later functions into the root commit.
    let c1p = t.nth_parent(out.new_tip, 2);
    assert_eq!(t.read_at(c1p, "src.rs").as_deref(), Some(V2));
    // and it's carried all the way to the tip
    assert_eq!(t.read_at(out.new_tip, "src.rs"), Some(cat(&[V2, HELPER, OTHER])));
    assert_ne!(out.new_tip, out.old_tip);
    assert!(t.is_clean(), "worktree + index clean after fix");
}

#[test]
fn fix_is_atomic_on_conflict() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("config.txt", "v=1\n")]);
    let c2 = t.commit("c2", &[("config.txt", "v=2\n")]); // same line changed
    t.stage(&[("config.txt", "v=3\n")]); // conflicting fix

    let before = t.branch_oid();
    let reflog_before = t.reflog_len();

    let r = ops::fix(&t.repo, &c1.to_string(), &Default::default());
    match &r {
        Err(Error::Conflict { suggested, .. }) => {
            assert_eq!(*suggested, Some(c2), "retarget hint points at the owning commit");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }

    // Nothing moved: ref and reflog untouched, repo byte-identical.
    assert_eq!(t.branch_oid(), before, "branch ref must not move on conflict");
    assert_eq!(t.reflog_len(), reflog_before, "reflog must not grow on conflict");
    assert_eq!(t.read_at(t.head(), "config.txt").as_deref(), Some("v=2\n"));
}

#[test]
fn fix_into_root_commit() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "1\n"), ("b.txt", "x\n")]);
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n")]);

    let out = ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();
    let c1p = t.nth_parent(out.new_tip, 1);
    assert_eq!(t.read_at(c1p, "a.txt").as_deref(), Some("1-fixed\n"));
    assert_eq!(t.read_at(c1p, "b.txt"), None, "b.txt not pulled back into the root");
    assert_eq!(t.read_at(out.new_tip, "a.txt").as_deref(), Some("1-fixed\n"));
}

/// `fix` used to REFUSE with unrelated unstaged churn on disk, while the TUI ran
/// the same fold happily. The TUI was right: the fold takes the INDEX, and the
/// rewritten tip's tree *is* that index tree, so the checkout was only ever
/// tidiness. Now the churn simply survives — and the checkout is skipped rather
/// than allowed to eat it.
#[test]
fn fix_keeps_unrelated_unstaged_churn() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "1\n"), ("b.txt", "x\n")]);
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n")]); // staged input
    t.dirty("b.txt", "work in progress\n"); // unrelated, unstaged

    let out = ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();

    assert_eq!(t.read_at(t.nth_parent(out.new_tip, 1), "a.txt").as_deref(), Some("1-fixed\n"));
    assert_eq!(
        std::fs::read_to_string(t.dir.join("b.txt")).unwrap(),
        "work in progress\n",
        "the unstaged edit is still on disk — no force checkout ran over it"
    );
}

#[test]
fn fix_nothing_staged_errors() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "1\n"), ("b.txt", "x\n")]);
    let r = ops::fix(&t.repo, &c1.to_string(), &Default::default());
    assert!(matches!(r, Err(Error::NothingStaged)), "got {r:?}");
}

#[test]
fn merge_commit_in_range_is_rejected() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let c2 = t.commit("c2", &[("a.txt", "1\n"), ("f2.txt", "x\n")]);
    // sibling off c1, then a merge commit as the tip
    let sig = t.repo.signature().unwrap();
    let c1c = t.repo.find_commit(c1).unwrap();
    let sib = t
        .repo
        .commit(None, &sig, &sig, "sib", &c1c.tree().unwrap(), &[&c1c])
        .unwrap();
    let c2c = t.repo.find_commit(c2).unwrap();
    let sibc = t.repo.find_commit(sib).unwrap();
    let m = t
        .repo
        .commit(None, &sig, &sig, "merge", &c2c.tree().unwrap(), &[&c2c, &sibc])
        .unwrap();

    let r = engine::replay(&t.repo, Some(c1), m, &Recipe::new(), false, false);
    assert!(matches!(r, Err(Error::MergeInRange { .. })), "got {r:?}");
}

#[test]
fn idempotent_absorption_empties_the_newer_commit() {
    // Folding a change into c1 that c2 also makes -> c2 becomes empty, no double-apply.
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "x\n")]);
    let c2 = t.commit("c2", &[("a.txt", "x\ny\n")]); // adds line y

    let c1c = t.repo.find_commit(c1).unwrap();
    let blob = t.repo.blob(b"x\ny\n").unwrap();
    let mut tb = t.repo.treebuilder(Some(&c1c.tree().unwrap())).unwrap();
    tb.insert("a.txt", blob, 0o100644).unwrap();
    let synth_tree = t.repo.find_tree(tb.write().unwrap()).unwrap();
    let sig = t.repo.signature().unwrap();
    let synth = t
        .repo
        .commit(None, &sig, &sig, "synth", &synth_tree, &[&c1c])
        .unwrap();

    let mut recipe = Recipe::new();
    recipe.add(c1, Edit::ApplyChange(synth));
    let new_tip = engine::replay(&t.repo, None, c2, &recipe, false, false).unwrap().tip;

    let c1p = t.nth_parent(new_tip, 1);
    assert_eq!(t.read_at(c1p, "a.txt").as_deref(), Some("x\ny\n"));
    assert_eq!(t.read_at(new_tip, "a.txt").as_deref(), Some("x\ny\n"));
    let tip_tree = t.repo.find_commit(new_tip).unwrap().tree().unwrap().id();
    let c1p_tree = t.repo.find_commit(c1p).unwrap().tree().unwrap().id();
    assert_eq!(tip_tree, c1p_tree, "c2 emptied — its change was absorbed into c1");
}

// ---- Phase 2: move (op B) -------------------------------------------------

#[test]
fn move_reanchors_file_at_target() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("root.txt", "r\n")]);
    let _c2 = t.commit("c2", &[("root.txt", "r\n"), ("feature.txt", "feat\n")]); // intro
    let _c3 = t.commit("c3", &[("root.txt", "r2\n"), ("feature.txt", "feat\n")]); // unrelated churn
    let c4 = t.commit("c4", &[("root.txt", "r2\n"), ("feature.txt", "feat\n"), ("x.txt", "x\n")]);
    let _c5 = t.commit(
        "c5",
        &[("root.txt", "r2\n"), ("feature.txt", "feat\n"), ("x.txt", "x\n"), ("y.txt", "y\n")],
    );

    let out = ops::mv(&t.repo, "feature.txt", &c4.to_string(), &Default::default()).unwrap();

    // base (c1) is untouched; c2..c5 rewritten. From the new tip:
    let c2p = t.nth_parent(out.new_tip, 3);
    let c3p = t.nth_parent(out.new_tip, 2);
    let c4p = t.nth_parent(out.new_tip, 1);
    assert_eq!(t.read_at(c2p, "feature.txt"), None, "removed from intro");
    assert_eq!(t.read_at(c3p, "feature.txt"), None, "removed from intermediate");
    assert_eq!(t.read_at(c4p, "feature.txt").as_deref(), Some("feat\n"), "present at target");
    assert_eq!(t.read_at(out.new_tip, "feature.txt").as_deref(), Some("feat\n"), "present at head");
    assert!(t.is_clean());
}

#[test]
fn move_blocked_when_intermediate_modifies_file() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("root.txt", "r\n")]);
    let _c2 = t.commit("c2", &[("root.txt", "r\n"), ("feature.txt", "v1\n")]); // intro
    let _c3 = t.commit("c3", &[("root.txt", "r\n"), ("feature.txt", "v2\n")]); // MODIFIED
    let c4 = t.commit("c4", &[("root.txt", "r\n"), ("feature.txt", "v2\n")]);

    let r = ops::mv(&t.repo, "feature.txt", &c4.to_string(), &Default::default());
    assert!(matches!(r, Err(Error::FileModified { .. })), "got {r:?}");
}

#[test]
fn move_missing_path_errors() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("root.txt", "r\n")]);
    let c2 = t.commit("c2", &[("root.txt", "r\n")]);
    let r = ops::mv(&t.repo, "nope.txt", &c2.to_string(), &Default::default());
    assert!(matches!(r, Err(Error::PathNotFound { .. })), "got {r:?}");
}

/// The reported bug: f1..f4 added one per commit, `move f4.txt HEAD~2` used to
/// report `path not found: f4.txt` because the target's tree doesn't carry the
/// file yet. Moving a file *earlier* must work.
#[test]
fn move_backward_reanchors_file_earlier() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("f1.txt", "1\n")]);
    let c2 = t.commit("c2", &[("f1.txt", "1\n"), ("f2.txt", "2\n")]);
    let _c3 = t.commit("c3", &[("f1.txt", "1\n"), ("f2.txt", "2\n"), ("f3.txt", "3\n")]);
    let c4 = t.commit(
        "c4",
        &[("f1.txt", "1\n"), ("f2.txt", "2\n"), ("f3.txt", "3\n"), ("f4.txt", "4\n")],
    );

    let out = ops::mv(&t.repo, "f4.txt", &c2.to_string(), &Default::default()).unwrap();

    let c2p = t.nth_parent(out.new_tip, 1);
    assert_eq!(t.read_at(c1, "f4.txt"), None, "absent before the new anchor");
    assert_eq!(t.read_at(c2p, "f4.txt").as_deref(), Some("4\n"), "present at the new anchor");
    assert_eq!(t.read_at(out.new_tip, "f4.txt").as_deref(), Some("4\n"), "carried to the tip");
    // everything else is untouched
    assert_eq!(t.read_at(out.new_tip, "f3.txt").as_deref(), Some("3\n"));
    // c4 held NOTHING but f4.txt, so with the file gone it has nothing left to
    // say. It is dropped rather than kept as an empty commit — and named, so the
    // message it takes with it is never lost silently.
    assert_eq!(t.commit_count(), 3, "the emptied intro commit is gone");
    assert_eq!(out.dropped, vec![c4], "and it is reported");
    assert!(t.is_clean());
}

/// Edits made to the file *after* it was introduced are replayed on top of the
/// re-anchored copy — the tip keeps the latest content.
#[test]
fn move_backward_keeps_later_edits() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("root.txt", "r\n")]);
    let c2 = t.commit("c2", &[("root.txt", "r2\n")]);
    let c3 = t.commit("c3", &[("root.txt", "r2\n"), ("feature.txt", "v1\n")]); // intro
    let _c4 = t.commit("c4", &[("root.txt", "r2\n"), ("feature.txt", "v2\n")]); // edits it

    let out = ops::mv(&t.repo, "feature.txt", &c2.to_string(), &Default::default()).unwrap();

    // c3 introduced nothing else, so it empties and is dropped (and reported);
    // c2 is now the commit that introduces the file.
    let c2p = t.nth_parent(out.new_tip, 1);
    assert_eq!(out.dropped, vec![c3], "the emptied intro commit is named");
    assert_eq!(t.read_at(c2p, "feature.txt").as_deref(), Some("v1\n"), "anchored as introduced");
    assert_eq!(t.read_at(out.new_tip, "feature.txt").as_deref(), Some("v2\n"), "later edit kept");
}

/// A backward `move-file` picks the first descendant carrying the path as the
/// file's introduction. If the file was *deleted* at (or before) the target and
/// re-added later, that heuristic resurrects it at a commit whose whole point is
/// that it doesn't have the file — and "first appears at <target>" would be a
/// lie besides, since it appeared earlier too. Refuse, and name the deletion.
#[test]
fn move_to_a_commit_that_deletes_the_file_is_refused() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("foo.txt", "v1\n"), ("keep.txt", "k\n")]);
    let c2 = t.commit_removing("c2", "foo.txt");
    let c3 = t.commit("c3", &[("other.txt", "o\n")]);
    let _c4 = t.commit("c4", &[("foo.txt", "v2\n")]); // re-added later

    // Target IS the deleting commit.
    let e = ops::mv(&t.repo, "foo.txt", &c2.to_string(), &Default::default()).unwrap_err();
    let m = e.to_string();
    assert!(m.contains(&format!("{c2:.8}")), "names the deleting commit: {m}");
    assert!(m.contains("resurrect"), "and says what it refused to do: {m}");

    // Target sits between the delete and the re-add: same lie, same refusal.
    let e = ops::mv(&t.repo, "foo.txt", &c3.to_string(), &Default::default()).unwrap_err();
    assert!(e.to_string().contains(&format!("{c2:.8}")), "still names c2: {e}");

    assert_eq!(t.head(), t.branch_oid(), "and nothing was rewritten");
    assert_eq!(t.commit_count(), 4);
}
