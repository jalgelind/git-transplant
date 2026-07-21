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

    // A file absent from HEAD (freshly added) can't be blamed → no homes.
    let blame = match repo.blame_file(Path::new(path), None) {
        Ok(b) => b,
        Err(_) => return Ok(vec![None; hunks.len()]),
    };

    let mut out = Vec::with_capacity(hunks.len());
    for h in hunks {
        // Attribute the OLD lines the hunk actually changes (not the context
        // span). A pure insertion changes none, so anchor on the line directly
        // before it — `old_start` would be the hunk's FIRST context line, up to
        // 3 lines early, which mis-blames the insertion (and can then conflict).
        let changed = h.changed_old_lines();
        let lines: Vec<usize> = if !changed.is_empty() {
            changed.to_vec()
        } else if let Some(anchor) = h.insert_anchor() {
            vec![anchor]
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
                    if best.is_none_or(|(bp, _)| p > bp) {
                        best = Some((p, oid));
                    }
                }
            }
        }
        out.push(best.map(|(_, oid)| oid));
    }
    Ok(out)
}
