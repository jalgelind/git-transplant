//! Regression: `move` must preserve a file's mode (executable bit / symlink),
//! not silently rewrite it to 0o100644. (Bug found in engine review.)

mod common;
use common::TestRepo;

use git_transplant::ops;

#[test]
fn move_preserves_executable_bit() {
    let t = TestRepo::new();
    let _c1 = t.commit("c1", &[("readme.md", "hi\n")]);
    // introduce an executable script at c2
    let _c2 = t.commit_exec("c2", "build.sh", "#!/bin/sh\necho hi\n");
    // unrelated churn, then the target
    let _c3 = t.commit("c3", &[("readme.md", "hello\n")]);
    let c4 = {
        // keep build.sh executable across c3/c4 by re-committing it as exec
        t.commit_exec("c4", "build.sh", "#!/bin/sh\necho hi\n")
    };

    assert_eq!(t.mode_at(c4, "build.sh"), Some(0o100755), "precondition: exec at target");

    let out = ops::mv(&t.repo, "build.sh", &c4.to_string(), false, false).unwrap();
    // build.sh should still be executable at the new tip.
    assert_eq!(
        t.mode_at(out.new_tip, "build.sh"),
        Some(0o100755),
        "move must preserve the executable bit"
    );
}

/// Same guarantee in the *backward* direction, where the mode comes from the
/// commit that introduces the file rather than from the target's tree.
#[test]
fn move_backward_preserves_executable_bit() {
    let t = TestRepo::new();
    let c1 = t.commit("c1", &[("readme.md", "hi\n")]);
    let c2 = t.commit("c2", &[("readme.md", "hello\n")]);
    let _c3 = t.commit_exec("c3", "build.sh", "#!/bin/sh\necho hi\n"); // intro, newer

    let out = ops::mv(&t.repo, "build.sh", &c2.to_string(), false, false).unwrap();

    let c2p = t.nth_parent(out.new_tip, 1);
    assert_eq!(t.mode_at(c2p, "build.sh"), Some(0o100755), "exec at the new anchor");
    assert_eq!(t.mode_at(out.new_tip, "build.sh"), Some(0o100755), "exec at the tip");
    assert_eq!(t.read_at(c1, "build.sh"), None, "still absent before the new anchor");
}
