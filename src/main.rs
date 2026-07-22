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

    /// Replay everything and report the result, but don't move the branch.
    #[arg(long, short = 'n', global = true)]
    dry_run: bool,

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
    /// Put the branch back where the last git-transplant run found it.
    Undo,
}

/// One line per successful op: where the branch landed (or would land) and how to
/// get back. The old tip is always printed, so recovery by hand stays possible.
fn report(o: &ops::Outcome, dry: bool) -> String {
    if dry {
        format!(
            "{} would move {:.8} -> {:.8} (dry run; nothing changed)",
            o.short_branch(),
            o.old_tip,
            o.new_tip
        )
    } else {
        format!(
            "{} now at {:.8} (was {:.8}; undo: git-transplant undo)",
            o.short_branch(),
            o.new_tip,
            o.old_tip
        )
    }
}

fn main() -> Result<()> {
    let opts = Opts::parse();
    let repo = Repository::discover(".").context("not inside a git repository")?;

    // The TUI owns its own screen and reporting; arg-driven ops share one path.
    if let Cmd::Tui = opts.cmd {
        return tui::run(&repo, opts.ignore_whitespace);
    }

    match opts.cmd {
        Cmd::Undo => {
            let o = ops::undo(&repo, opts.dry_run).map_err(anyhow::Error::msg)?;
            if opts.dry_run {
                println!(
                    "{} would go back to {:.8} (from {:.8}) (dry run; nothing changed)",
                    o.short_branch(),
                    o.new_tip,
                    o.old_tip
                );
            } else {
                println!(
                    "{} restored to {:.8} (was {:.8}; redo: git-transplant undo)",
                    o.short_branch(),
                    o.new_tip,
                    o.old_tip
                );
                // Undo moves the ref only, so whatever the undone op folded in is
                // still on disk — now as an uncommitted change.
                if ops::require_fully_clean(&repo).is_err() {
                    println!("worktree untouched: the undone change is uncommitted again");
                }
            }
        }
        Cmd::Absorb { base } => {
            let base_oid = base
                .as_deref()
                .map(|r| git_transplant::git::resolve(&repo, r))
                .transpose()
                .map_err(anyhow::Error::msg)?;
            let a = ops::collapse(&repo, base_oid, opts.ignore_whitespace, opts.dry_run)
                .map_err(anyhow::Error::msg)?;
            // The routing table is the point of a dry-run absorb: which hunk lands
            // in which commit, before anything is rewritten (cf. `hg absorb -n`).
            if opts.dry_run {
                let mut last = "";
                for (path, header, target) in &a.routes {
                    if path != last {
                        println!("{path}");
                        last = path;
                    }
                    let summary = repo
                        .find_commit(*target)
                        .ok()
                        .and_then(|c| c.summary().map(str::to_owned))
                        .unwrap_or_default();
                    println!("    {header} -> {target:.8} {summary}");
                }
            }
            match a.outcome {
                Some(o) => {
                    println!(
                        "{} {} hunk(s) ({} left staged); {}",
                        if opts.dry_run { "would absorb" } else { "absorbed" },
                        a.folded,
                        a.orphans,
                        report(&o, opts.dry_run)
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
                Cmd::Fix { target } => {
                    ops::fix(&repo, &target, opts.ignore_whitespace, opts.dry_run)
                }
                Cmd::MoveFile { path, target } => {
                    ops::mv(&repo, &path, &target, opts.ignore_whitespace, opts.dry_run)
                }
                Cmd::Absorb { .. } | Cmd::Tui | Cmd::Undo => unreachable!(),
            }
            .map_err(anyhow::Error::msg)?;

            if outcome.new_tip == outcome.old_tip {
                println!("no change");
            } else {
                println!("{}", report(&outcome, opts.dry_run));
            }
            for w in &outcome.warnings {
                eprintln!("warning: {w}");
            }
        }
    }
    Ok(())
}
