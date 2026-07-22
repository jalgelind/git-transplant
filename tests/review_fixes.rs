//! Regressions for defects found in the full-codebase review round.

mod common;
use common::TestRepo;

use git_transplant::engine::{self, Edit, Recipe};
use git_transplant::{git, inference, ops, patch, Error};

fn lines(prefix: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{prefix}{i}\n")).collect()
}

/// Pure insertions used to blame the hunk's FIRST CONTEXT line (up to 3 lines
/// early), routing the hunk to the wrong commit and aborting the whole absorb.
#[test]
fn pure_insertion_is_attributed_to_the_line_it_follows() {
    let t = TestRepo::new();
    let a = lines("a", 8);
    let c1 = t.commit("c1", &[("src.rs", &a)]);
    let ab = format!("{a}{}", lines("b", 8));
    let c2 = t.commit("c2", &[("src.rs", &ab)]);
    let c3 = t.commit("c3", &[("src.rs", &format!("{ab}tail\n"))]);

    // insert a new line directly after line 10 (`b2`), which c2 owns
    let old = t.read_at(c3, "src.rs").unwrap();
    let mut v: Vec<String> = old.split_inclusive('\n').map(String::from).collect();
    v.insert(10, "INSERTED\n".to_string());
    let new: String = v.concat();

    let hs = patch::hunks(old.as_bytes(), new.as_bytes()).unwrap();
    let targets = inference::infer_targets(&t.repo, "src.rs", &hs, &[c1, c2, c3]).unwrap();
    assert_eq!(targets, vec![Some(c2)], "insertion belongs to the owner of the line above it");

    // and the full absorb succeeds rather than conflicting in the wrong commit
    t.stage(&[("src.rs", &new)]);
    let a = ops::collapse(&t.repo, None, &Default::default()).unwrap();
    assert_eq!((a.folded, a.orphans), (1, 0));
}

/// A file ADDED by the source commit isn't in that commit's parent tree, so the
/// mode used to fall back to 0o100644 and silently drop the exec bit.
#[test]
fn moving_a_hunk_preserves_the_exec_bit_of_an_added_file() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("readme", "hi\n")]);
    let c2 = t.commit_exec("c2", "build.sh", "#!/bin/sh\necho hi\n");
    assert_eq!(t.mode_at(c2, "build.sh"), Some(0o100755), "precondition");

    // move build.sh's hunk from c2 back into c1, as the TUI does
    let c2c = t.repo.find_commit(c2).unwrap();
    let parent = c2c.parent(0).unwrap();
    let new_tree = c2c.tree().unwrap();
    let mode = new_tree.get_path(std::path::Path::new("build.sh")).unwrap().filemode();
    let old = String::new(); // absent in the parent
    let new = t.read_at(c2, "build.sh").unwrap();
    let hs = patch::hunks(old.as_bytes(), new.as_bytes()).unwrap();
    let synth =
        patch::synthetic_for_hunks(&t.repo, parent.id(), "build.sh", &old, &hs, &[true], mode)
            .unwrap();

    let mut recipe = Recipe::new();
    recipe.add(c1, Edit::ApplyChange(synth));
    let tip = engine::replay(&t.repo, None, c2, &recipe, false, true).unwrap().tip;
    assert_eq!(t.mode_at(tip, "build.sh"), Some(0o100755), "exec bit survives the move");
}

/// A merge anywhere in the ancestry used to abort the whole tool, even when the
/// stack the user works on is perfectly linear.
#[test]
fn a_merge_in_history_does_not_block_the_linear_stack_above_it() {
    let t = TestRepo::new();
    let base = t.commit("base", &[("a.txt", "1\n")]);
    let main_side = t.commit("main-side", &[("a.txt", "1\n"), ("m.txt", "m\n")]);

    // a sibling off `base`, then a merge commit, then linear work on top
    let sig = t.repo.signature().unwrap();
    let basec = t.repo.find_commit(base).unwrap();
    let sib = t
        .repo
        .commit(None, &sig, &sig, "sib", &basec.tree().unwrap(), &[&basec])
        .unwrap();
    let mainc = t.repo.find_commit(main_side).unwrap();
    let sibc = t.repo.find_commit(sib).unwrap();
    let merge = t
        .repo
        .commit(Some("HEAD"), &sig, &sig, "merge", &mainc.tree().unwrap(), &[&mainc, &sibc])
        .unwrap();
    let top = t.commit("top", &[("a.txt", "1\n"), ("m.txt", "m\n"), ("t.txt", "t\n")]);

    // strict walk still refuses (an explicit range containing a merge)
    assert!(matches!(
        engine::replay(&t.repo, Some(base), top, &Recipe::new(), false, false),
        Err(Error::MergeInRange { .. })
    ));
    // but the offered window is the linear run above the merge
    let win = git::linear_window(&t.repo, top).unwrap();
    let ids: Vec<_> = win.iter().map(|c| c.id()).collect();
    assert_eq!(ids, vec![top], "window stops at the merge, doesn't error");
    let _ = merge;
}

/// `SetFile` must not silently replace a directory with a blob.
#[test]
fn set_path_refuses_to_replace_a_directory_with_a_file() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("src/a.rs", "code\n"), ("keep.txt", "k\n")]);
    let blob = t.repo.blob(b"clobber\n").unwrap();
    let tree = t.repo.find_commit(c1).unwrap().tree().unwrap();

    let mut recipe = Recipe::new();
    recipe.add(c1, Edit::SetFile { path: "src".into(), blob, mode: 0o100644 });
    let r = engine::replay(&t.repo, None, c1, &recipe, false, false);
    assert!(matches!(r, Err(Error::Empty(_))), "got {r:?}");
    // the subtree is intact
    assert!(tree.get_path(std::path::Path::new("src/a.rs")).is_ok());
}

/// The branch ref must not be force-overwritten if it moved underneath us.
#[test]
fn promote_refuses_when_the_branch_moved_underneath() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "1\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "2\n")]);
    let stale = t.head();
    // someone else commits on the same branch after we captured `stale`
    let _c3 = t.commit("c3", &[("a.txt", "3\n")]);
    let moved_to = t.head();

    // promote with the stale old_tip must refuse rather than discard c3
    let err = ops::promote(&t.repo, "refs/heads/master", c1, stale, "test", false);
    assert!(err.is_err(), "must refuse a stale-head overwrite");
    assert_eq!(t.branch_oid(), moved_to, "c3 is still on the branch");
}

/// The forward direction (target NEWER than source) is the only path that emits
/// `Edit::RevertChange`, and it had no real coverage: the hunk must LEAVE the
/// source commit and APPEAR at the target, with everything else intact.
#[test]
fn forward_move_removes_the_hunk_from_its_source_commit() {
    let t = TestRepo::new();
    // c1 owns lines 1-12 of shared.rs; c2 edits line 2; c3 adds an unrelated file
    let base = lines("l", 12);
    let c1 = t.commit("c1", &[("shared.rs", &base)]);
    let mut v: Vec<String> = base.split_inclusive('\n').map(String::from).collect();
    v[1] = "EDITED-by-c2\n".into();
    let c2_body: String = v.concat();
    let c2 = t.commit("c2", &[("shared.rs", &c2_body)]);
    let c3 = t.commit("c3", &[("shared.rs", &c2_body), ("other.txt", "x\n")]);

    // Build the synthetic exactly as the TUI does for c2's own hunk.
    let c2c = t.repo.find_commit(c2).unwrap();
    let parent = c2c.parent(0).unwrap();
    let old = t.read_at(c1, "shared.rs").unwrap();
    let new = t.read_at(c2, "shared.rs").unwrap();
    let hs = patch::hunks(old.as_bytes(), new.as_bytes()).unwrap();
    assert_eq!(hs.len(), 1, "fixture precondition: one hunk in c2");
    let mode = c2c.tree().unwrap().get_path(std::path::Path::new("shared.rs")).unwrap().filemode();
    let synth =
        patch::synthetic_for_hunks(&t.repo, parent.id(), "shared.rs", &old, &hs, &[true], mode)
            .unwrap();

    // FORWARD move: revert at the source (c2), apply at the newer target (c3).
    let mut recipe = Recipe::new();
    recipe.add(c3, Edit::ApplyChange(synth));
    recipe.add(c2, Edit::RevertChange(synth));
    let tip = engine::replay(&t.repo, Some(c1), c3, &recipe, false, false).unwrap().tip;

    // c2' must no longer carry the edit; c3'(tip) must.
    let c2p = t.nth_parent(tip, 1);
    assert_eq!(
        t.read_at(c2p, "shared.rs").as_deref(),
        Some(base.as_str()),
        "the hunk LEFT the source commit"
    );
    assert!(
        t.read_at(tip, "shared.rs").unwrap().contains("EDITED-by-c2"),
        "and ARRIVED at the newer target"
    );
    // unrelated content is untouched
    assert_eq!(t.read_at(tip, "other.txt").as_deref(), Some("x\n"));
}
