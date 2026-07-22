//! M2 "Confidence": `--dry-run` reports without touching anything, and `undo`
//! puts the branch back where the last run found it.

mod common;
use common::*;

use git_transplant::{ops, Error};

const V1: &str = "fn main() {\n    let value = 1;\n    println!(\"{}\", value);\n}\n";
const V2: &str = "fn main() {\n    let value = 42;\n    println!(\"{}\", value);\n}\n";
const HELPER: &str = "\nfn helper() {}\n";

// ---- --dry-run ------------------------------------------------------------

#[test]
fn dry_run_fix_predicts_the_real_tip_and_moves_nothing() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("src.rs", V1)]);
    let _c2 = t.commit("c2", &[("src.rs", &format!("{V1}{HELPER}"))]);
    t.stage(&[("src.rs", &format!("{V2}{HELPER}"))]);

    let (before, reflog_before) = (t.branch_oid(), t.reflog_len());
    let dry = ops::fix(&t.repo, &c1.to_string(), &ops::Opts { dry_run: true, ..Default::default() }).unwrap();
    assert_eq!(t.branch_oid(), before, "dry run must not move the branch");
    assert_eq!(t.reflog_len(), reflog_before, "dry run must not write a reflog entry");

    let real = ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap();
    assert_eq!(dry.new_tip, real.new_tip, "the preview was the real run, minus the ref move");
    assert_eq!(dry.old_tip, real.old_tip);
    assert_ne!(t.branch_oid(), before, "the real run does move it");
}

#[test]
fn dry_run_move_file_predicts_the_real_tip_and_moves_nothing() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("a.txt", "a\n")]);
    let _c2 = t.commit("c2", &[("a.txt", "a\n"), ("feature.txt", "f\n")]);

    let (before, reflog_before) = (t.branch_oid(), t.reflog_len());
    let dry = ops::mv(&t.repo, "feature.txt", &c1.to_string(), &ops::Opts { dry_run: true, ..Default::default() }).unwrap();
    assert_eq!(t.branch_oid(), before);
    assert_eq!(t.reflog_len(), reflog_before);

    let real = ops::mv(&t.repo, "feature.txt", &c1.to_string(), &Default::default()).unwrap();
    assert_eq!(dry.new_tip, real.new_tip);
}

#[test]
fn dry_run_absorb_reports_the_routing_table_without_rewriting() {
    let t = TestRepo::new();
    let a = lines("a", 8);
    let c1 = t.commit("c1", &[("src.rs", &a)]);
    let ab = format!("{a}{}", lines("b", 8));
    let c2 = t.commit("c2", &[("src.rs", &ab)]);
    // change a line owned by c1 and a line owned by c2.
    let staged = ab.replace("a2\n", "A2\n").replace("b2\n", "B2\n");
    t.stage(&[("src.rs", &staged)]);

    let (before, reflog_before) = (t.branch_oid(), t.reflog_len());
    let dry = ops::collapse(&t.repo, None, &ops::Opts { dry_run: true, ..Default::default() }).unwrap();
    assert_eq!(t.branch_oid(), before, "dry run must not move the branch");
    assert_eq!(t.reflog_len(), reflog_before, "dry run must not write a reflog entry");
    assert_eq!((dry.folded, dry.orphans), (2, 0));

    // the routing table names the file, the hunk and the commit it would go to.
    let targets: Vec<_> = dry.routes.iter().map(|(_, _, oid)| *oid).collect();
    assert_eq!(targets, vec![c1, c2], "each hunk routed to the commit that owns it");
    assert!(dry.routes.iter().all(|(p, h, _)| p == "src.rs" && h.starts_with("@@")));

    let real = ops::collapse(&t.repo, None, &Default::default()).unwrap();
    assert_eq!(
        dry.outcome.unwrap().new_tip,
        real.outcome.unwrap().new_tip,
        "the preview was the real run, minus the ref move"
    );
}

// ---- undo -----------------------------------------------------------------

/// `fix` + one staged hunk: the state every undo test starts from.
fn fixed(t: &TestRepo) -> ops::Outcome {
    let c1 = t.commit("c1", &[("src.rs", V1)]);
    t.commit("c2", &[("src.rs", &format!("{V1}{HELPER}"))]);
    t.stage(&[("src.rs", &format!("{V2}{HELPER}"))]);
    ops::fix(&t.repo, &c1.to_string(), &Default::default()).unwrap()
}

#[test]
fn undo_restores_the_previous_tip() {
    let t = TestRepo::new();
    let out = fixed(&t);

    // a dry-run undo reports the restore without performing it.
    let dry = ops::undo(&t.repo, true).unwrap();
    assert_eq!(dry.new_tip, out.old_tip);
    assert_eq!(t.branch_oid(), out.new_tip, "dry-run undo leaves the branch alone");

    let u = ops::undo(&t.repo, false).unwrap();
    assert_eq!(u.new_tip, out.old_tip, "restored to the pre-transplant tip");
    assert_eq!(t.branch_oid(), out.old_tip);
    // The worktree is deliberately NOT reset: the folded change is still on disk,
    // now as an uncommitted edit, so undo can never destroy work.
    let on_disk = std::fs::read_to_string(t.dir.join("src.rs")).unwrap();
    assert_eq!(on_disk, format!("{V2}{HELPER}"), "undo must not touch the worktree");
    assert!(!t.is_clean(), "the undone change resurfaces as uncommitted");
}

#[test]
fn undo_of_an_undo_is_a_redo() {
    // The undo writes its own `transplant: undo (…)` reflog entry, so the next
    // undo walks that one — back to the rewritten tip.
    let t = TestRepo::new();
    let out = fixed(&t);
    ops::undo(&t.repo, false).unwrap();
    let redo = ops::undo(&t.repo, false).unwrap();
    assert_eq!(redo.new_tip, out.new_tip);
    assert_eq!(t.branch_oid(), out.new_tip);
}

#[test]
fn undo_refuses_when_the_branch_moved_since() {
    let t = TestRepo::new();
    let _out = fixed(&t);
    let moved = t.commit("c3", &[("other.txt", "o\n")]);

    match ops::undo(&t.repo, false) {
        Err(Error::Empty(m)) => assert!(m.contains("has moved since"), "got: {m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(t.branch_oid(), moved, "branch untouched by the refused undo");
}

#[test]
fn undo_without_a_transplant_entry_says_so() {
    let t = TestRepo::new();
    t.commit("c1", &[("src.rs", V1)]);
    t.commit("c2", &[("src.rs", &format!("{V1}{HELPER}"))]);

    match ops::undo(&t.repo, false) {
        Err(Error::Empty(m)) => assert!(m.contains("nothing to undo"), "got: {m}"),
        other => panic!("expected a clear error, got {other:?}"),
    }
}

#[test]
fn undo_list_reports_the_history_and_marks_what_undo_would_do() {
    let t = TestRepo::new();
    let first = fixed(&t);
    // A second operation on top, so "newest first" is actually testable.
    let second = ops::reword(&t.repo, "HEAD", "reworded", &Default::default()).unwrap();

    let (branch, entries) = ops::undo_list(&t.repo).unwrap();
    assert_eq!(ops::short_branch(&branch), "master");
    assert_eq!(entries.len(), 2, "both operations are listed: {entries:?}");

    assert!(entries[0].message.contains("reword"), "newest first: {:?}", entries[0]);
    assert!(entries[0].next, "and it is the one undo would act on");
    assert_eq!((entries[0].old, entries[0].new), (second.old_tip, second.new_tip));

    assert!(entries[1].message.contains("fix into"), "then the older one");
    assert!(!entries[1].next, "only one entry is marked");
    assert_eq!((entries[1].old, entries[1].new), (first.old_tip, first.new_tip));

    // The mark is a promise: a real undo lands exactly there.
    let u = ops::undo(&t.repo, false).unwrap();
    assert_eq!(u.new_tip, entries[0].old);
}

#[test]
fn undo_list_shows_the_undo_itself_so_a_redo_is_visible() {
    // `undo` records its own move, which is why a second undo is a REDO rather
    // than a step further back. The listing has to show that, not hide it.
    let t = TestRepo::new();
    fixed(&t);
    ops::undo(&t.repo, false).unwrap();

    let (_, entries) = ops::undo_list(&t.repo).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries[0].message.starts_with("transplant: undo ("), "{:?}", entries[0]);
    assert!(entries[0].next, "so the next `undo` is visibly a redo of this one");
}

#[test]
fn undo_list_on_a_branch_with_no_transplants_is_empty_not_an_error() {
    let t = TestRepo::new();
    t.commit("c1", &[("src.rs", V1)]);
    let (_, entries) = ops::undo_list(&t.repo).unwrap();
    assert!(entries.is_empty());
}
