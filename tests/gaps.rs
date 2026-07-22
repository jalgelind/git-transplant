//! Coverage-gap tests for recently-added paths: --ignore-whitespace end-to-end,
//! absorb multi-file / orphan preservation, fix==HEAD, nested move.

mod common;
use common::*;

use git_transplant::{ops, Error};

#[test]
fn ignore_whitespace_resolves_a_reindent_adjacent_fix() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("f.rs", "fn f() {\n    let x = 1;\n}\n")]);
    // c2 reindents the same line (whitespace-only)
    let _c2 = t.commit("c2", &[("f.rs", "fn f() {\n        let x = 1;\n}\n")]);
    // stage a value fix on top of the reindented line
    t.stage(&[("f.rs", "fn f() {\n        let x = 2;\n}\n")]);

    // Without ignore-ws: the reindent + value edit clash on that line.
    assert!(
        matches!(ops::fix(&t.repo, &c1.to_string(), &Default::default()), Err(Error::Conflict { .. })),
        "expected a whitespace-adjacent conflict"
    );
    // With ignore-ws: the reindent is ignored, the value fix folds cleanly.
    let out = ops::fix(&t.repo, &c1.to_string(), &ops::Opts { ignore_ws: true, ..Default::default() }).expect("ignore-ws should resolve it");
    let c1p = t.nth_parent(out.new_tip, 1);
    let c1_txt = t.read_at(c1p, "f.rs").unwrap();
    assert!(c1_txt.contains("x = 2"), "value fix folded into c1");
    // The fix must also survive to the tip — asserting only c1' would pass even
    // if the replay dropped it on the way up.
    assert!(t.read_at(out.new_tip, "f.rs").unwrap().contains("x = 2"), "carried to the tip");
    // Pinned surprise: with ignore-ws the merge takes "theirs" wholesale, so c2's
    // reindent rides along into c1'. Documented, not accidental.
    assert!(c1_txt.contains("        let x = 2;"), "ignore-ws drags the reindent back: {c1_txt:?}");
}

#[test]
fn absorb_distributes_across_multiple_files() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("a.txt", &lines("a", 8))]);
    let _c2 = t.commit("c2", &[("a.txt", &lines("a", 8)), ("b.txt", &lines("b", 8))]);
    let c3 = t.commit(
        "c3",
        &[("a.txt", &lines("a", 8)), ("b.txt", &lines("b", 8)), ("c.txt", "marker\n")],
    );

    let a_new = edit(&t.read_at(c3, "a.txt").unwrap(), &[(1, "A2")]); // a.txt line2 -> c1
    let b_new = edit(&t.read_at(c3, "b.txt").unwrap(), &[(1, "B2")]); // b.txt line2 -> c2
    t.stage(&[("a.txt", &a_new), ("b.txt", &b_new)]);

    let a = ops::collapse(&t.repo, None, &Default::default()).unwrap();
    let out = a.outcome.expect("absorbed");
    assert_eq!((a.folded, a.orphans), (2, 0));
    let c1p = t.nth_parent(out.new_tip, 2);
    let c2p = t.nth_parent(out.new_tip, 1);
    assert!(t.read_at(c1p, "a.txt").unwrap().contains("A2\n"), "a.txt fix -> c1");
    assert!(t.read_at(c2p, "b.txt").unwrap().contains("B2\n"), "b.txt fix -> c2");
    // Negative: neither fix may leak into the other file's owning commit.
    assert!(!t.read_at(c1p, "a.txt").unwrap().contains("B2\n"), "no cross-file leak into c1");
    assert_eq!(t.read_at(c1p, "b.txt"), None, "b.txt does not exist yet at c1");
    assert!(!t.read_at(c2p, "a.txt").unwrap().contains("B2\n"), "b.txt's fix stayed out of a.txt");
    assert!(t.is_clean());
}

#[test]
fn absorb_preserves_orphan_hunks_in_the_worktree() {
    // A hunk with no home in the window must be LEFT in the worktree, not wiped
    // by the post-absorb checkout.
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("a.txt", &lines("a", 8))]);
    let _c2 = t.commit("c2", &[("a.txt", &lines("a", 8)), ("b.txt", &lines("b", 8))]);
    let c3 = t.commit(
        "c3",
        &[("a.txt", &lines("a", 8)), ("b.txt", &lines("b", 8)), ("c.txt", "m\n")],
    );

    // a.txt line2 is owned by c1 (OUT of window when base=c1); b.txt line2 by c2 (IN).
    let a_new = edit(&t.read_at(c3, "a.txt").unwrap(), &[(1, "A2")]);
    let b_new = edit(&t.read_at(c3, "b.txt").unwrap(), &[(1, "B2")]);
    t.stage(&[("a.txt", &a_new), ("b.txt", &b_new)]);

    let a = ops::collapse(&t.repo, Some(t.nth_parent(c3, 2)), &Default::default()).unwrap(); // base = c1
    assert_eq!((a.folded, a.orphans), (1, 1), "b.txt homed, a.txt orphaned");

    // The orphaned a.txt change must still be present AND still staged — "left
    // staged" is an index property; reading the file we just wrote proves nothing.
    let a_disk = std::fs::read_to_string(t.dir.join("a.txt")).unwrap();
    assert!(a_disk.contains("A2\n"), "orphan hunk must survive in the worktree, not be wiped");
    assert!(t.is_staged("a.txt"), "and it is still staged against the rewritten HEAD");
    assert!(!t.is_staged("b.txt"), "the absorbed file is no longer staged");
}

#[test]
fn fix_into_head_is_amend_like() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let c2 = t.commit("c2", &[("a.txt", "1\n"), ("b.txt", "x\n")]);
    t.stage(&[("a.txt", "1\n"), ("b.txt", "y\n")]); // amend b.txt at the tip

    let before = t.commit_count();
    let out = ops::fix(&t.repo, &c2.to_string(), &Default::default()).unwrap();
    assert_eq!(t.read_at(out.new_tip, "b.txt").as_deref(), Some("y\n"));
    assert_eq!(t.commit_count(), before, "amend-like: no commit added");
    assert!(t.is_clean());
}

#[test]
fn fix_rejects_non_ancestor_target() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "2\n")]); // head
    // a sibling commit off c1 (not an ancestor of HEAD)
    let sig = t.repo.signature().unwrap();
    let c1c = t.repo.find_commit(c1).unwrap();
    let sib = t.repo.commit(None, &sig, &sig, "sib", &c1c.tree().unwrap(), &[&c1c]).unwrap();
    t.stage(&[("a.txt", "3\n")]);
    assert!(matches!(
        ops::fix(&t.repo, &sib.to_string(), &Default::default()),
        Err(Error::TargetNotAncestor)
    ));
}

#[test]
fn move_target_is_intro_errors() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("root.txt", "r\n")]);
    let c2 = t.commit("c2", &[("root.txt", "r\n"), ("feature.txt", "f\n")]); // intro == target
    let _c3 = t.commit("c3", &[("root.txt", "r2\n"), ("feature.txt", "f\n")]);
    // nothing to remove before the intro commit
    assert!(matches!(
        ops::mv(&t.repo, "feature.txt", &c2.to_string(), &Default::default()),
        Err(Error::Empty(_))
    ));
}

#[test]
fn absorb_preserves_absent_trailing_newline() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("f.txt", "one\ntwo\nthree")]); // no trailing newline
    let _c2 = t.commit("c2", &[("f.txt", "one\ntwo\nthree"), ("m.txt", "m\n")]);
    t.stage(&[("f.txt", "one\nTWO\nthree")]); // change line 2, still no trailing newline

    let a = ops::collapse(&t.repo, None, &Default::default()).unwrap();
    let out = a.outcome.expect("absorbed");
    assert_eq!(
        t.read_at(out.new_tip, "f.txt").as_deref(),
        Some("one\nTWO\nthree"),
        "no spurious trailing newline introduced"
    );
}

/// M3 changed this test's expectation: an abandoned sibling used to be *warned*
/// about, and is now *moved*. `--no-restack` (asserted here) keeps the warning.
#[test]
fn fix_warns_about_abandoned_branch_only_with_no_restack() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let c2 = t.commit("c2", &[("a.txt", "1\n"), ("b.txt", "x\n")]); // head
    // a second branch pointing at c2 (which the fix will rewrite)
    t.repo
        .reference("refs/heads/feature", c2, false, "create")
        .unwrap();
    t.stage(&[("a.txt", "1-fixed\n"), ("b.txt", "x\n")]);

    let opts = ops::Opts { no_restack: true, ..Default::default() };
    let out = ops::fix(&t.repo, &c1.to_string(), &opts).unwrap();
    assert!(
        out.warnings.iter().any(|w| w.contains("feature")),
        "should warn that refs/heads/feature is now orphaned, got {:?}",
        out.warnings
    );
    assert!(out.restacked.is_empty(), "--no-restack must move nothing");
    assert_eq!(
        t.repo.refname_to_id("refs/heads/feature").unwrap(),
        c2,
        "sibling left exactly where it was"
    );
}

#[test]
fn move_handles_nested_paths() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("readme", "hi\n")]);
    let _c2 = t.commit("c2", &[("readme", "hi\n"), ("src/lib.rs", "code\n")]);
    let c3 = t.commit("c3", &[("readme", "ho\n"), ("src/lib.rs", "code\n")]);

    let out = ops::mv(&t.repo, "src/lib.rs", &c3.to_string(), &Default::default()).unwrap();
    let c2p = t.nth_parent(out.new_tip, 1);
    assert_eq!(t.read_at(c2p, "src/lib.rs"), None, "removed from ancestor");
    assert_eq!(t.read_at(out.new_tip, "src/lib.rs").as_deref(), Some("code\n"), "present at target");
}
