//! Phase 3 library layer: commutation inference (#5) + hunk-subset fold (#6).

mod common;
use common::*;

use git_transplant::engine::{self, Edit, Recipe};
use git_transplant::{inference, patch};

#[test]
fn drop_empty_keeps_intentionally_empty_commits() {
    // A deliberately-empty commit in range must survive drop_empty.
    let t = TestRepo::new();
    let base = lines("l", 10);
    let c1 = t.commit("c1", &[("a.txt", &base)]);
    let _marker = t.commit("marker", &[("a.txt", &base)]); // empty: tree == c1
    // c3 edits line 10 — far from the fold's line-1 edit, so no conflict
    let mut c3v: Vec<String> = base.split_inclusive('\n').map(String::from).collect();
    c3v[9] = "L10\n".into();
    let c3 = t.commit("c3", &[("a.txt", &c3v.concat())]);

    // synth folds a line-1 edit into c1
    let mut sv: Vec<String> = base.split_inclusive('\n').map(String::from).collect();
    sv[0] = "L1\n".into();
    let c1c = t.repo.find_commit(c1).unwrap();
    let blob = t.repo.blob(sv.concat().as_bytes()).unwrap();
    let mut tb = t.repo.treebuilder(Some(&c1c.tree().unwrap())).unwrap();
    tb.insert("a.txt", blob, 0o100644).unwrap();
    let synth_tree = t.repo.find_tree(tb.write().unwrap()).unwrap();
    let sig = t.repo.signature().unwrap();
    let synth = t.repo.commit(None, &sig, &sig, "s", &synth_tree, &[&c1c]).unwrap();

    let mut recipe = Recipe::new();
    recipe.add(c1, Edit::ApplyChange(synth));
    let new_tip = engine::replay(&t.repo, None, c3, &recipe, false, true).unwrap().tip;

    // chain is c3' -> marker' -> c1'(root): the empty marker is NOT dropped
    let markerp = t.nth_parent(new_tip, 1);
    let c1p = t.nth_parent(new_tip, 2);
    assert_eq!(t.repo.find_commit(c1p).unwrap().parent_count(), 0, "c1' is the root");
    assert_eq!(
        t.repo.find_commit(markerp).unwrap().tree().unwrap().id(),
        t.repo.find_commit(c1p).unwrap().tree().unwrap().id(),
        "intentionally-empty marker preserved (empty vs its parent)"
    );
}

#[test]
fn drop_empty_removes_a_commit_absorbed_elsewhere() {
    // Fold c2's entire change into c1; with drop_empty, c2 (now empty) is dropped.
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "x\n")]);
    let c2 = t.commit("c2", &[("a.txt", "x\ny\n")]); // adds line y

    let c1c = t.repo.find_commit(c1).unwrap();
    let blob = t.repo.blob(b"x\ny\n").unwrap();
    let mut tb = t.repo.treebuilder(Some(&c1c.tree().unwrap())).unwrap();
    tb.insert("a.txt", blob, 0o100644).unwrap();
    let synth_tree = t.repo.find_tree(tb.write().unwrap()).unwrap();
    let sig = t.repo.signature().unwrap();
    let synth = t.repo.commit(None, &sig, &sig, "s", &synth_tree, &[&c1c]).unwrap();

    let mut recipe = Recipe::new();
    recipe.add(c1, Edit::ApplyChange(synth));

    // without drop_empty: c2' survives (empty) -> tip has a parent
    let kept = engine::replay(&t.repo, None, c2, &recipe, false, false).unwrap().tip;
    assert_eq!(t.repo.find_commit(kept).unwrap().parent_count(), 1);
    // with drop_empty: c2' is dropped -> tip IS c1' (a root commit, no parent)
    let dropped = engine::replay(&t.repo, None, c2, &recipe, false, true).unwrap().tip;
    assert_eq!(t.repo.find_commit(dropped).unwrap().parent_count(), 0, "empty c2 dropped");
    assert_eq!(t.read_at(dropped, "a.txt").as_deref(), Some("x\ny\n"));
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
    let synth = patch::synthetic_for_hunks(&t.repo, c3, "src.rs", &old, &hs, &[true, false], 0o100644).unwrap();
    let mut recipe = Recipe::new();
    recipe.add(c1, Edit::ApplyChange(synth));
    let new_tip = engine::replay(&t.repo, None, c3, &recipe, false, false).unwrap().tip;

    let c1p = t.nth_parent(new_tip, 2);
    let c1_txt = t.read_at(c1p, "src.rs").unwrap();
    let head_txt = t.read_at(new_tip, "src.rs").unwrap();
    assert!(c1_txt.contains("A2\n"), "selected hunk folded into c1");
    assert!(head_txt.contains("A2\n"), "and carried to the tip");
    assert!(!head_txt.contains("B2\n"), "unselected hunk must NOT be applied");
    assert!(head_txt.contains("b2\n"), "line 10 keeps its original content");
}
