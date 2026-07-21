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

    let out = ops::fix(&t.repo, &c1.to_string(), false).unwrap();

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
    let _c2 = t.commit("c2", &[("config.txt", "v=2\n")]); // same line changed
    t.stage(&[("config.txt", "v=3\n")]); // conflicting fix

    let before = t.branch_oid();
    let reflog_before = t.reflog_len();

    let r = ops::fix(&t.repo, &c1.to_string(), false);
    assert!(matches!(r, Err(Error::Conflict { .. })), "got {r:?}");

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

    let out = ops::fix(&t.repo, &c1.to_string(), false).unwrap();
    let c1p = t.nth_parent(out.new_tip, 1);
    assert_eq!(t.read_at(c1p, "a.txt").as_deref(), Some("1-fixed\n"));
    assert_eq!(t.read_at(c1p, "b.txt"), None, "b.txt not pulled back into the root");
    assert_eq!(t.read_at(out.new_tip, "a.txt").as_deref(), Some("1-fixed\n"));
}

#[test]
fn fix_rejects_dirty_worktree() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "2\n")]);
    t.stage(&[("a.txt", "3\n")]); // staged input
    t.dirty("a.txt", "99\n"); // plus an unstaged change

    let r = ops::fix(&t.repo, &c1.to_string(), false);
    assert!(matches!(r, Err(Error::DirtyWorktree)), "got {r:?}");
}

#[test]
fn fix_nothing_staged_errors() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "1\n"), ("b.txt", "x\n")]);
    let r = ops::fix(&t.repo, &c1.to_string(), false);
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

    let r = engine::replay(&t.repo, Some(c1), m, &Recipe::new(), false);
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
    let new_tip = engine::replay(&t.repo, None, c2, &recipe, false).unwrap();

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

    let out = ops::mv(&t.repo, "feature.txt", &c4.to_string(), false).unwrap();

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

    let r = ops::mv(&t.repo, "feature.txt", &c4.to_string(), false);
    assert!(matches!(r, Err(Error::FileModified { .. })), "got {r:?}");
}

#[test]
fn move_missing_path_errors() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("root.txt", "r\n")]);
    let c2 = t.commit("c2", &[("root.txt", "r\n")]);
    let r = ops::mv(&t.repo, "nope.txt", &c2.to_string(), false);
    assert!(matches!(r, Err(Error::PathNotFound { .. })), "got {r:?}");
}
