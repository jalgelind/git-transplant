use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use git2::Repository;

use git_transplant::{ops, tui};

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
    /// Fold the staged change into <target>, then replay the commits after it.
    #[command(visible_alias = "fixup")]
    Fix {
        /// Commit to fold into (any revspec: hash, HEAD~2, a branch, …).
        target: String,
    },
    /// Re-anchor a whole file so it first appears at <target>, earlier or later.
    // `move` kept as a hidden alias: in git-branchless `git move` means "move a
    // subtree of commits", so the long name is the one to advertise.
    #[command(name = "move-file", alias = "move")]
    MoveFile {
        /// File path to move.
        path: String,
        /// Commit the file should belong to.
        target: String,
    },
    /// Send each staged hunk to the commit that last touched those lines.
    Absorb {
        /// Oldest commit to consider (revspec); default walks to the root.
        #[arg(long)]
        base: Option<String>,
    },
    /// Pick hunks on screen — fold staged ones back, or move hunks between
    /// existing commits.
    Tui,
}

fn main() -> Result<()> {
    let opts = Opts::parse();
    let repo = Repository::discover(".").context("not inside a git repository")?;

    // The TUI owns its own screen and reporting; arg-driven ops share one path.
    if let Cmd::Tui = opts.cmd {
        return tui::run(&repo, opts.ignore_whitespace);
    }

    match opts.cmd {
        Cmd::Absorb { base } => {
            let base_oid = base
                .as_deref()
                .map(|r| git_transplant::git::resolve(&repo, r))
                .transpose()
                .map_err(anyhow::Error::msg)?;
            let a = ops::collapse(&repo, base_oid, opts.ignore_whitespace).map_err(anyhow::Error::msg)?;
            match a.outcome {
                Some(o) => {
                    println!(
                        "absorbed {} hunk(s) ({} left staged); {} now at {}",
                        a.folded,
                        a.orphans,
                        o.short_branch(),
                        &o.new_tip.to_string()[..8]
                    );
                    for w in &o.warnings {
                        eprintln!("warning: {w}");
                    }
                }
                None => println!("nothing absorbed ({} hunk(s) had no home in range)", a.orphans),
            }
        }
        cmd => {
            let outcome = match cmd {
                Cmd::Fix { target } => ops::fix(&repo, &target, opts.ignore_whitespace),
                Cmd::MoveFile { path, target } => {
                    ops::mv(&repo, &path, &target, opts.ignore_whitespace)
                }
                Cmd::Absorb { .. } | Cmd::Tui => unreachable!(),
            }
            .map_err(anyhow::Error::msg)?;

            if outcome.new_tip == outcome.old_tip {
                println!("no change");
            } else {
                println!(
                    "{} now at {}",
                    outcome.short_branch(),
                    &outcome.new_tip.to_string()[..8]
                );
            }
            for w in &outcome.warnings {
                eprintln!("warning: {w}");
            }
        }
    }
    Ok(())
}
