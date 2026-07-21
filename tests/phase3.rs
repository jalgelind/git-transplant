//! Phase 3 library layer: commutation inference (#5) + hunk-subset fold (#6).

mod common;
use common::TestRepo;

use git_transplant::engine::{self, Edit, Recipe};
use git_transplant::{inference, patch};

/// `n` lines "<prefix>1".."<prefix>n", each newline-terminated.
fn lines(prefix: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{prefix}{i}\n")).collect()
}

/// A stack where c1 owns lines 1-8, c2 owns 9-16, c3 owns line 17.
fn owned_stack(t: &TestRepo) -> (git2::Oid, git2::Oid, git2::Oid) {
    let a = lines("a", 8);
    let c1 = t.commit("c1", &[("src.rs", &a)]);
    let ab = format!("{a}{}", lines("b", 8));
    let c2 = t.commit("c2", &[("src.rs", &ab)]);
    let abc = format!("{ab}c1\n");
    let c3 = t.commit("c3", &[("src.rs", &abc)]);
    (c1, c2, c3)
}

#[test]
fn inference_distributes_hunks_to_owning_commits() {
    let t = TestRepo::new();
    let (c1, c2, c3) = owned_stack(&t);

    let old = t.read_at(c3, "src.rs").unwrap();
    // change line 2 (owned by c1) and line 10 (owned by c2)
    let mut nl: Vec<String> = old.split_inclusive('\n').map(String::from).collect();
    nl[1] = "A2\n".into();
    nl[9] = "B2\n".into();
    let new: String = nl.concat();

    let hs = patch::hunks(old.as_bytes(), new.as_bytes()).unwrap();
    assert_eq!(hs.len(), 2, "expected two separate hunks");

    let targets = inference::infer_targets(&t.repo, "src.rs", &hs, &[c1, c2, c3]).unwrap();
    assert_eq!(targets, vec![Some(c1), Some(c2)], "each hunk routed to its owner");
}

#[test]
fn inference_reports_no_home_outside_window() {
    let t = TestRepo::new();
    let (c1, c2, c3) = owned_stack(&t);

    let old = t.read_at(c3, "src.rs").unwrap();
    // change line 2, owned by c1 — but exclude c1 from the window
    let mut nl: Vec<String> = old.split_inclusive('\n').map(String::from).collect();
    nl[1] = "A2\n".into();
    let new: String = nl.concat();

    let hs = patch::hunks(old.as_bytes(), new.as_bytes()).unwrap();
    let targets = inference::infer_targets(&t.repo, "src.rs", &hs, &[c2, c3]).unwrap();
    assert_eq!(targets, vec![None], "line owned before the window has no home");
    let _ = c1;
}

#[test]
fn hunk_subset_fold_lands_only_selected_hunk() {
    let t = TestRepo::new();
    let (c1, _c2, c3) = owned_stack(&t);

    let old = t.read_at(c3, "src.rs").unwrap();
    let mut nl: Vec<String> = old.split_inclusive('\n').map(String::from).collect();
    nl[1] = "A2\n".into(); // hunk 1 (line 2, owned by c1)
    nl[9] = "B2\n".into(); // hunk 2 (line 10) — deliberately NOT selected
    let new: String = nl.concat();

    let hs = patch::hunks(old.as_bytes(), new.as_bytes()).unwrap();
    // build a synthetic carrying ONLY the first hunk, fold it into c1
    let synth = patch::synthetic_for_hunks(&t.repo, c3, "src.rs", &old, &hs, &[true, false]).unwrap();
    let mut recipe = Recipe::new();
    recipe.add(c1, Edit::ApplyChange(synth));
    let new_tip = engine::replay(&t.repo, None, c3, &recipe, false).unwrap();

    let c1p = t.nth_parent(new_tip, 2);
    let c1_txt = t.read_at(c1p, "src.rs").unwrap();
    let head_txt = t.read_at(new_tip, "src.rs").unwrap();
    assert!(c1_txt.contains("A2\n"), "selected hunk folded into c1");
    assert!(head_txt.contains("A2\n"), "and carried to the tip");
    assert!(!head_txt.contains("B2\n"), "unselected hunk must NOT be applied");
    assert!(head_txt.contains("b2\n"), "line 10 keeps its original content");
}
