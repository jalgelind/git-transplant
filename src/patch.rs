//! Hunk-subset extraction for sub-commit selection (task #6).
//!
//! Pure-text hunk application: parse a file's diff into hunks, then rebuild the
//! file content applying only the selected hunks. This is what lets the TUI fold
//! *some* of a file's changes into a commit while leaving the rest behind — and
//! it sidesteps the fragile `Diff::from_buffer` + `apply_to_tree` path.

use git2::{DiffOptions, Patch, Repository};

use crate::Result;

/// One contiguous change region in a file diff.
#[derive(Debug, Clone)]
pub struct Hunk {
    /// The `@@ ... @@` header line, for display.
    pub header: String,
    /// 1-based first line in the OLD file this hunk covers.
    pub old_start: usize,
    /// Number of OLD lines the hunk replaces.
    pub old_lines: usize,
    /// The replacement lines (context + additions), each with its trailing `\n`.
    new_content: Vec<String>,
    /// OLD line numbers actually removed/modified (excludes context) — for blame.
    changed_old: Vec<usize>,
    added: usize,
    removed: usize,
}

impl Hunk {
    pub fn added(&self) -> usize {
        self.added
    }
    pub fn removed(&self) -> usize {
        self.removed
    }
    /// OLD line numbers this hunk actually changes (not the context span).
    pub fn changed_old_lines(&self) -> &[usize] {
        &self.changed_old
    }
}

/// Parse the diff between `old` and `new` blobs into hunks.
pub fn hunks(old: &[u8], new: &[u8]) -> Result<Vec<Hunk>> {
    let mut opts = DiffOptions::new();
    opts.context_lines(3);
    let patch = Patch::from_buffers(old, None, new, None, Some(&mut opts))?;
    let mut out = Vec::new();
    for i in 0..patch.num_hunks() {
        let (dh, _) = patch.hunk(i)?;
        let mut new_content = Vec::new();
        let mut changed_old = Vec::new();
        let (mut added, mut removed) = (0usize, 0usize);
        for j in 0..patch.num_lines_in_hunk(i)? {
            let line = patch.line_in_hunk(i, j)?;
            let text = String::from_utf8_lossy(line.content()).into_owned();
            match line.origin() {
                '+' => {
                    new_content.push(text);
                    added += 1;
                }
                ' ' => new_content.push(text),
                '-' => {
                    removed += 1;
                    if let Some(n) = line.old_lineno() {
                        changed_old.push(n as usize);
                    }
                }
                _ => {}
            }
        }
        out.push(Hunk {
            header: String::from_utf8_lossy(dh.header()).trim_end().to_string(),
            old_start: dh.old_start() as usize,
            old_lines: dh.old_lines() as usize,
            new_content,
            changed_old,
            added,
            removed,
        });
    }
    Ok(out)
}

/// Rebuild file content from `old`, applying only the hunks flagged in `selected`.
/// Hunks are non-overlapping and ordered; unselected regions keep the old text.
pub fn apply_selected(old: &str, hunks: &[Hunk], selected: &[bool]) -> String {
    let old_lines: Vec<&str> = split_keep_newlines(old);
    let mut out = String::new();
    let mut pos = 0usize; // 0-based index into old_lines
    for (h, &sel) in hunks.iter().zip(selected.iter()) {
        let start = h.old_start.saturating_sub(1);
        while pos < start && pos < old_lines.len() {
            out.push_str(old_lines[pos]);
            pos += 1;
        }
        if sel {
            for l in &h.new_content {
                out.push_str(l);
            }
        } else {
            for i in 0..h.old_lines {
                if pos + i < old_lines.len() {
                    out.push_str(old_lines[pos + i]);
                }
            }
        }
        pos += h.old_lines;
    }
    while pos < old_lines.len() {
        out.push_str(old_lines[pos]);
        pos += 1;
    }
    out
}

/// Build a synthetic commit whose diff (from `source`'s tree) applies only the
/// selected hunks of `path`. Returns the synthetic commit oid for
/// `engine::Edit::ApplyChange`. `new_full` is the fully-modified content.
pub fn synthetic_for_hunks(
    repo: &Repository,
    source: git2::Oid,
    path: &str,
    old_full: &str,
    hunks: &[Hunk],
    selected: &[bool],
) -> Result<git2::Oid> {
    let partial = apply_selected(old_full, hunks, selected);
    let blob = repo.blob(partial.as_bytes())?;
    let source_commit = repo.find_commit(source)?;
    let mut b = repo.treebuilder(Some(&source_commit.tree()?))?;
    b.insert(path, blob, 0o100644)?;
    let tree = repo.find_tree(b.write()?)?;
    let sig = crate::git::ident(repo);
    Ok(repo.commit(None, &sig, &sig, "transplant-partial", &tree, &[&source_commit])?)
}

fn split_keep_newlines(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.split_inclusive('\n').collect();
    if v.is_empty() {
        v.push("");
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\n";
    // change line 1 and line 11; the 3-line context windows don't touch -> two hunks
    const NEW: &str = "L1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nL11\n";

    fn two_hunks() -> Vec<Hunk> {
        let h = hunks(OLD.as_bytes(), NEW.as_bytes()).unwrap();
        assert_eq!(h.len(), 2, "expected two separated hunks, got {}", h.len());
        h
    }

    #[test]
    fn select_all_reconstructs_new() {
        let h = two_hunks();
        assert_eq!(apply_selected(OLD, &h, &[true, true]), NEW);
    }

    #[test]
    fn select_none_reconstructs_old() {
        let h = two_hunks();
        assert_eq!(apply_selected(OLD, &h, &[false, false]), OLD);
    }

    #[test]
    fn select_subset_applies_only_those() {
        let h = two_hunks();
        // first change only
        assert_eq!(
            apply_selected(OLD, &h, &[true, false]),
            "L1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\n"
        );
        // second change only
        assert_eq!(
            apply_selected(OLD, &h, &[false, true]),
            "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nL11\n"
        );
    }

    #[test]
    fn hunk_line_math() {
        let h = two_hunks();
        assert_eq!(h[0].old_start, 1);
        assert_eq!((h[0].added(), h[0].removed()), (1, 1));
        // precise changed line (line 1), not the whole context span
        assert_eq!(h[0].changed_old_lines(), &[1]);
        assert_eq!(h[1].changed_old_lines(), &[11]);
    }
}
