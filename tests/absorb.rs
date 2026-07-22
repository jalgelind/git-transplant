//! op D (auto absorb) — distribute a multi-hunk staged change to owning commits.

mod common;
use common::TestRepo;

use git_transplant::ops;

fn lines(prefix: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{prefix}{i}\n")).collect()
}

/// c1 owns lines 1-8, c2 owns 9-16, c3 owns line 17.
fn owned_stack(t: &TestRepo) -> (git2::Oid, git2::Oid, git2::Oid) {
    let a = lines("a", 8);
    let c1 = t.commit("c1", &[("src.rs", &a)]);
    let ab = format!("{a}{}", lines("b", 8));
    let c2 = t.commit("c2", &[("src.rs", &ab)]);
    let abc = format!("{ab}c1\n");
    let c3 = t.commit("c3", &[("src.rs", &abc)]);
    (c1, c2, c3)
}

fn edit(base: &str, changes: &[(usize, &str)]) -> String {
    let mut v: Vec<String> = base.split_inclusive('\n').map(String::from).collect();
    for (idx0, s) in changes {
        v[*idx0] = format!("{s}\n");
    }
    v.concat()
}

#[test]
fn absorb_distributes_hunks_to_owning_commits() {
    let t = TestRepo::new();
    let (_c1, _c2, c3) = owned_stack(&t);
    let head_src = t.read_at(c3, "src.rs").unwrap();

    // change line 2 (owned by c1) and line 10 (owned by c2), then stage.
    let staged = edit(&head_src, &[(1, "A2"), (9, "B2")]);
    t.stage(&[("src.rs", &staged)]);

    let a = ops::collapse(&t.repo, None, false, false).unwrap();
    let out = a.outcome.expect("something absorbed");
    assert_eq!((a.folded, a.orphans), (2, 0));

    // whole stack rewritten (earliest target c1 is the root): c3'(0) c2'(1) c1'(2)
    let c1p = t.nth_parent(out.new_tip, 2);
    let c2p = t.nth_parent(out.new_tip, 1);
    let c1_txt = t.read_at(c1p, "src.rs").unwrap();
    let c2_txt = t.read_at(c2p, "src.rs").unwrap();
    let head_txt = t.read_at(out.new_tip, "src.rs").unwrap();

    assert!(c1_txt.contains("A2\n"), "line-2 fix landed in its owner c1");
    assert!(!c1_txt.contains("B2\n"), "line-10 fix did NOT leak into c1");
    assert!(c2_txt.contains("B2\n"), "line-10 fix landed in its owner c2");
    assert!(head_txt.contains("A2\n") && head_txt.contains("B2\n"), "both carried to tip");
    assert!(t.is_clean(), "worktree + index clean after absorb");
}

#[test]
fn absorb_works_for_nested_paths() {
    // Regression: synthetic_for_hunks must handle "src/app.rs" (treebuilder.insert
    // rejects slashes) — top-level paths would mask the bug.
    let t = TestRepo::new();
    let a = lines("a", 8);
    let c1 = t.commit("c1", &[("src/app.rs", &a)]);
    let ab = format!("{a}{}", lines("b", 8));
    let _c2 = t.commit("c2", &[("src/app.rs", &ab)]);
    let c3 = t.commit("c3", &[("src/app.rs", &format!("{ab}tail\n"))]);

    let head_src = t.read_at(c3, "src/app.rs").unwrap();
    let staged = edit(&head_src, &[(1, "A2")]); // line 2, owned by c1
    t.stage(&[("src/app.rs", &staged)]);

    let a = ops::collapse(&t.repo, None, false, false).unwrap();
    let out = a.outcome.expect("nested-path hunk absorbed");
    assert_eq!((a.folded, a.orphans), (1, 0));
    let c1p = t.nth_parent(out.new_tip, 2);
    assert!(t.read_at(c1p, "src/app.rs").unwrap().contains("A2\n"));
    let _ = c1;
}

#[test]
fn absorb_leaves_orphan_hunks_outside_window() {
    let t = TestRepo::new();
    let (_c1, c2, c3) = owned_stack(&t);
    let head_src = t.read_at(c3, "src.rs").unwrap();

    // change line 2 (owned by c1) but restrict the window to base = c2.
    let staged = edit(&head_src, &[(1, "A2")]);
    t.stage(&[("src.rs", &staged)]);
    let before = t.branch_oid();

    let a = ops::collapse(&t.repo, Some(c2), false, false).unwrap();
    assert!(a.outcome.is_none(), "nothing had a home in the window");
    assert_eq!((a.folded, a.orphans), (0, 1));
    assert_eq!(t.branch_oid(), before, "branch untouched when nothing absorbs");
}
