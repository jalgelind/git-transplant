// Phase 0 de-risk spike (see docs/ROADMAP.md).
// Proves the engine's linchpin git2 APIs behave: replay a stack in memory by
// cherry-picking each commit onto a rewritten parent, fold a staged change into
// the ROOT commit, and confirm revert + conflict detection work.
//
// Run: cargo run --example spike
//
// ponytail: throwaway proof; graduates into engine.rs's integration test later.

use std::fs;
use std::path::Path;
use std::process::exit;

use git2::{Commit, Repository, Signature, Tree};

// Realistic content: the fix touches one line in main(); later commits append
// functions far below, separated by blank lines. (A 3-line file with adjacent
// edits and no separating context merges as a spurious conflict — avoid that.)
const MAIN_V1: &str = "fn main() {\n    let value = 1;\n    println!(\"{}\", value);\n}\n";
const MAIN_V2: &str = "fn main() {\n    let value = 42;\n    println!(\"{}\", value);\n}\n";
const HELPER: &str = "\nfn helper() {\n    // placeholder\n}\n";
const OTHER: &str = "\nfn other() {\n    0\n}\n";

fn main() {
    let dir = std::env::temp_dir().join(format!("git-transplant-spike-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let ok = run(&dir).unwrap_or_else(|e| {
        eprintln!("spike errored: {e}");
        false
    });
    let _ = fs::remove_dir_all(&dir);
    if ok {
        println!("\nALL CHECKS PASSED — engine is buildable on these APIs.");
    } else {
        println!("\nSPIKE FAILED — revisit the approach before building the engine.");
        exit(1);
    }
}

/// Cherry-pick `picked` onto `onto` in memory; report conflicts, else return the merged tree.
fn cp<'a>(repo: &'a Repository, label: &str, picked: &Commit, onto: &Commit) -> Option<Tree<'a>> {
    let mut idx = repo.cherrypick_commit(picked, onto, 0, None).ok()?;
    if idx.has_conflicts() {
        let paths: Vec<String> = idx
            .conflicts()
            .map(|cs| {
                cs.flatten()
                    .filter_map(|c| c.our.or(c.their).or(c.ancestor))
                    .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        println!("  [conflict] {label}: {paths:?}");
        return None;
    }
    let oid = idx.write_tree_to(repo).ok()?;
    repo.find_tree(oid).ok()
}

fn run(dir: &Path) -> Result<bool, git2::Error> {
    fs::create_dir_all(dir).unwrap();
    let repo = Repository::init(dir)?;
    let sig = Signature::now("spike", "spike@test")?;
    let file = "src.rs";

    // helper: write file, stage it, commit (moving HEAD), return the commit
    let commit = |msg: &str, content: &str, parents: &[&Commit]| -> Result<git2::Oid, git2::Error> {
        fs::write(dir.join(file), content).unwrap();
        let mut index = repo.index()?;
        index.add_path(Path::new(file))?;
        index.write()?;
        let tree = repo.find_tree(index.write_tree()?)?;
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, parents)
    };
    // helper: build a commit object without moving any ref
    let mk = |msg: &str, tree: &Tree, parents: &[&Commit]| -> Result<Commit, git2::Error> {
        repo.find_commit(repo.commit(None, &sig, &sig, msg, tree, parents)?)
    };

    // --- 3-commit stack -----------------------------------------------------
    let c1 = repo.find_commit(commit("c1", MAIN_V1, &[])?)?;
    let c2 = repo.find_commit(commit("c2", &format!("{MAIN_V1}{HELPER}"), &[&c1])?)?;
    let c3 = repo.find_commit(commit("c3", &format!("{MAIN_V1}{HELPER}{OTHER}"), &[&c2])?)?;

    // --- stage a fix at the tip (op C input): value 1 -> 42 -----------------
    fs::write(dir.join(file), format!("{MAIN_V2}{HELPER}{OTHER}")).unwrap();
    let mut index = repo.index()?;
    index.add_path(Path::new(file))?;
    index.write()?;
    let staged_tree = repo.find_tree(index.write_tree()?)?;
    let f = mk("fixup", &staged_tree, &[&c3])?; // synthetic delta commit, parented at tip

    // --- fold F into the ROOT commit, then replay c2, c3 --------------------
    let (Some(c1p_tree), _) = (cp(&repo, "fold fix into c1", &f, &c1), ()) else { return Ok(false); };
    let c1p = mk("c1", &c1p_tree, &[])?;
    let Some(c2p_tree) = cp(&repo, "replay c2", &c2, &c1p) else { return Ok(false); };
    let c2p = mk("c2", &c2p_tree, &[&c1p])?;
    let Some(c3p_tree) = cp(&repo, "replay c3", &c3, &c2p) else { return Ok(false); };
    let c3p = mk("c3", &c3p_tree, &[&c2p])?;

    // --- revert path (op B / forward-move primitive) ------------------------
    let mut ridx = repo.revert_commit(&f, &c3p, 0, None)?;
    let reverted = repo.find_tree(ridx.write_tree_to(&repo)?)?;

    // --- conflict detection: two commits editing the same line --------------
    let root = mk("base", &blob_tree(&repo, file, "shared\n")?, &[])?;
    let a = mk("a", &blob_tree(&repo, file, "AAA\n")?, &[&root])?;
    let b = mk("b", &blob_tree(&repo, file, "BBB\n")?, &[&root])?;
    let conflict_idx = repo.cherrypick_commit(&b, &a, 0, None)?;

    // --- adjacent-edit handling --------------------------------------------
    // (1) genuine adjacency: tight file, target is missing the fix's trailing
    //     context. Must be DETECTED and aborted, never silently corrupted.
    let tbase = mk("tb", &blob_tree(&repo, file, "a\nb\nc\n")?, &[])?;
    let ttarget = mk("tt", &blob_tree(&repo, file, "a\n")?, &[])?;
    let tfix = mk("tf", &blob_tree(&repo, file, "a-fixed\nb\nc\n")?, &[&tbase])?;
    let tight = cp(&repo, "tight fold (adjacency)", &tfix, &ttarget);

    // (2) whitespace-adjacent spurious conflict: a reindent on the same line as
    //     the fix. Conflicts by default; dissolves with ignore_whitespace.
    let wbase = mk("wb", &blob_tree(&repo, file, "fn f() {\n    let x = 1;\n}\n")?, &[])?;
    let wours = mk("wo", &blob_tree(&repo, file, "fn f() {\n    let x = 42;\n}\n")?, &[])?;
    let wtheirs = mk("wt", &blob_tree(&repo, file, "fn f() {\n        let x = 1;\n}\n")?, &[&wbase])?;
    let ws_default = repo.cherrypick_commit(&wtheirs, &wours, 0, None)?;
    let mut mo = git2::MergeOptions::new();
    mo.ignore_whitespace(true);
    let ws_ignored = repo.cherrypick_commit(&wtheirs, &wours, 0, Some(&mo))?;

    // --- assertions ---------------------------------------------------------
    let read = |t: &Tree| -> String {
        let e = t.get_path(Path::new(file)).unwrap();
        let o = e.to_object(&repo).unwrap();
        String::from_utf8(o.as_blob().unwrap().content().to_vec()).unwrap()
    };
    let mut pass = true;
    let mut check = |cond: bool, label: &str| {
        println!("  [{}] {}", if cond { "ok" } else { "FAIL" }, label);
        pass &= cond;
    };

    check(read(&c1p_tree) == MAIN_V2, "fix folded into ROOT commit c1' (helper/other not pulled back)");
    check(read(&c2p_tree) == format!("{MAIN_V2}{HELPER}"), "c2' carries the fix");
    check(read(&c3p_tree) == format!("{MAIN_V2}{HELPER}{OTHER}"), "tip content preserved + fixed");
    check(read(&reverted) == format!("{MAIN_V1}{HELPER}{OTHER}"), "revert_commit strips the fix");
    check(conflict_idx.has_conflicts(), "same-line edits detected as a conflict");
    check(c3p.id() != c3.id(), "rewritten tip is a new commit oid");
    check(tight.is_none(), "genuine adjacency detected -> clean abort, no corruption");
    check(ws_default.has_conflicts(), "whitespace-adjacent edit conflicts under default merge");
    check(!ws_ignored.has_conflicts(), "...merges clean with ignore_whitespace merge option");

    Ok(pass)
}

/// Build a one-file tree from raw content, off the current index.
fn blob_tree<'a>(repo: &'a Repository, path: &str, content: &str) -> Result<Tree<'a>, git2::Error> {
    let dir = repo.workdir().unwrap();
    fs::write(dir.join(path), content).unwrap();
    let mut index = repo.index()?;
    index.add_path(Path::new(path))?;
    index.write()?;
    repo.find_tree(index.write_tree()?)
}
