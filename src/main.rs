use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use git2::Repository;

use git_transplant::ops;

// cli tool to move changes around inside a stack of commits ("git transplant").
// The whole tool is one in-memory replay engine driven by a per-commit recipe;
// see docs/DESIGN.md and docs/ROADMAP.md.

#[derive(Debug, Parser)]
#[command(name = "git-transplant", version, author = "Johannes")]
struct Opts {
    /// Ignore whitespace when merging (dissolves reindent-adjacent conflicts).
    #[arg(long, global = true)]
    ignore_whitespace: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Fold the currently-staged change into <target> and replay the stack (op C).
    Fix {
        /// Commit to fold into (any revspec: hash, HEAD~2, a branch, …).
        target: String,
    },
    /// Re-anchor <path> at <target>, removing it from <target>'s ancestors (op B).
    Move {
        /// File path to move.
        path: String,
        /// Commit the file should belong to.
        target: String,
    },
}

fn main() -> Result<()> {
    let opts = Opts::parse();
    let repo = Repository::discover(".").context("not inside a git repository")?;

    let outcome = match opts.cmd {
        Cmd::Fix { target } => ops::fix(&repo, &target, opts.ignore_whitespace),
        Cmd::Move { path, target } => ops::mv(&repo, &path, &target, opts.ignore_whitespace),
    }
    .map_err(anyhow::Error::msg)?;

    let short = &outcome.new_tip.to_string()[..8];
    if outcome.new_tip == outcome.old_tip {
        println!("no change");
    } else {
        println!("{} now at {short}", outcome.branch);
    }
    Ok(())
}
