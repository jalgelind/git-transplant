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

    let out = ops::mv(&t.repo, "build.sh", &c4.to_string(), false).unwrap();
    // build.sh should still be executable at the new tip.
    assert_eq!(
        t.mode_at(out.new_tip, "build.sh"),
        Some(0o100755),
        "move must preserve the executable bit"
    );
}
