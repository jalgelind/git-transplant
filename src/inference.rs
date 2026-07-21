//! Commutation-based target inference (task #5).
//!
//! git-absorb's insight: a hunk belongs to the newest commit that owns the lines
//! it touches — fold it there and adjacency conflicts don't arise (fold earlier
//! and they do). We operationalize "owns the lines" with `git blame`: for each
//! hunk, blame the OLD lines it changes, and pick the newest blamed commit that
//! falls inside the stack window (`base..HEAD`). A hunk whose lines are all owned
//! outside the window has no home — leave it and warn.

use std::collections::HashMap;
use std::path::Path;

use git2::{Oid, Repository};

use crate::patch::Hunk;
use crate::Result;

/// For each hunk of `path`, the inferred target commit (None = no home in window).
/// `window` is the stack commits oldest-first (base-exclusive .. HEAD).
pub fn infer_targets(
    repo: &Repository,
    path: &str,
    hunks: &[Hunk],
    window: &[Oid],
) -> Result<Vec<Option<Oid>>> {
    // stack position of each in-window commit; higher = newer (closer to HEAD).
    let pos: HashMap<Oid, usize> = window.iter().enumerate().map(|(i, o)| (*o, i)).collect();

    let blame = repo.blame_file(Path::new(path), None)?;

    let mut out = Vec::with_capacity(hunks.len());
    for h in hunks {
        // Attribute the OLD lines the hunk actually changes (not the context
        // span). Pure insertions have none → fall back to the preceding line.
        let changed = h.changed_old_lines();
        let lines: Vec<usize> = if !changed.is_empty() {
            changed.to_vec()
        } else if h.old_start > 0 {
            vec![h.old_start]
        } else {
            vec![]
        };

        let mut best: Option<(usize, Oid)> = None;
        for ln in lines {
            if let Some(bh) = blame.get_line(ln) {
                let oid = bh.final_commit_id();
                if let Some(&p) = pos.get(&oid) {
                    if best.map_or(true, |(bp, _)| p > bp) {
                        best = Some((p, oid));
                    }
                }
            }
        }
        out.push(best.map(|(_, oid)| oid));
    }
    Ok(out)
}
