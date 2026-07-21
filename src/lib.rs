//! git-transplant: move changes around inside a stack of commits.
//!
//! The whole tool is one in-memory replay engine (`engine::replay`) driven by a
//! `Recipe` of per-commit edits. See `docs/DESIGN.md`.

pub mod engine;
pub mod git;
pub mod inference;
pub mod ops;
pub mod patch;
pub mod recipe;
pub mod tui;

use git2::Oid;

/// Everything that can go wrong. History-rewriting failures are all *clean*:
/// the engine only ever creates dangling objects, so on any `Err` no ref has
/// moved and the repo is byte-identical.
#[derive(Debug)]
pub enum Error {
    Git(git2::Error),
    /// A 3-way merge conflicted while replaying/injecting at this commit.
    /// `suggested` is the commit inference thinks owns the changed lines (a
    /// better `fix` target), if different from the one requested.
    Conflict {
        commit: Oid,
        path: Option<String>,
        suggested: Option<Oid>,
    },
    /// A merge commit sits in the range to rewrite (linear history only).
    MergeInRange { commit: Oid },
    /// The requested base is not an ancestor of the tip.
    BaseNotAncestor,
    /// `target` is not an ancestor of HEAD (nothing to fold into).
    TargetNotAncestor,
    /// Working tree has changes other than the intended input.
    DirtyWorktree,
    /// A path was expected in a tree but is absent.
    PathNotFound { path: String },
    /// A whole-file move found the file modified within the span it must cross.
    FileModified { path: String, commit: Oid },
    /// Nothing to do.
    Empty(String),
    /// HEAD is not on a branch.
    DetachedHead,
    /// No staged change to fold.
    NothingStaged,
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<git2::Error> for Error {
    fn from(e: git2::Error) -> Self {
        Error::Git(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Git(e) => write!(f, "git error: {e}"),
            Error::Conflict { commit, path, suggested } => {
                match path {
                    Some(p) => write!(f, "conflict while rewriting {commit:.8} in {p}")?,
                    None => write!(f, "conflict while rewriting {commit:.8}")?,
                }
                if let Some(s) = suggested {
                    write!(f, " — {s:.8} owns those lines; try `fix {s:.8}` or `absorb`")?;
                }
                Ok(())
            }
            Error::MergeInRange { commit } => {
                write!(f, "merge commit {commit:.8} in range; only linear history is supported")
            }
            Error::BaseNotAncestor => write!(f, "base is not an ancestor of the tip"),
            Error::TargetNotAncestor => write!(f, "target is not an ancestor of HEAD"),
            Error::DirtyWorktree => write!(f, "working tree has unstaged/untracked changes; commit, stash, or clean first"),
            Error::PathNotFound { path } => write!(f, "path not found: {path}"),
            Error::FileModified { path, commit } => {
                write!(f, "{path} is modified at {commit:.8}; move is not clean (aborting)")
            }
            Error::Empty(msg) => write!(f, "{msg}"),
            Error::DetachedHead => write!(f, "HEAD is detached; check out a branch first"),
            Error::NothingStaged => write!(f, "nothing staged to fold; `git add` your fix first"),
        }
    }
}

impl std::error::Error for Error {}
