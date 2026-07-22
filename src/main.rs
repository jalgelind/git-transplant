use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use git2::Repository;

use git_transplant::{ops, tui};

// cli tool to move changes around inside a stack of commits ("git transplant").
// The whole tool is one in-memory replay engine driven by a per-commit recipe;
// see docs/DESIGN.md and docs/ROADMAP.md.

#[derive(Debug, Parser)]
#[command(name = "git-transplant", version, author = "Johannes")]
#[command(group = clap::ArgGroup::new("favor").multiple(false))]
#[command(after_help = CONFLICT_HELP)]
struct Opts {
    /// Ignore whitespace when merging (dissolves reindent-adjacent conflicts).
    #[arg(long, global = true)]
    ignore_whitespace: bool,

    /// On conflict keep OURS: the stack you are replaying onto (see below).
    #[arg(long, global = true, group = "favor")]
    ours: bool,

    /// On conflict keep THEIRS: the change being applied (see below).
    #[arg(long, global = true, group = "favor")]
    theirs: bool,

    /// On conflict keep BOTH sides, in order, with no conflict markers.
    #[arg(long, global = true, group = "favor")]
    union: bool,

    /// Replay everything and report the result, but don't move the branch.
    #[arg(long, short = 'n', global = true)]
    dry_run: bool,

    /// Leave other branches pointing into the rewritten range (warn, don't move).
    #[arg(long, global = true)]
    no_restack: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

/// Which side is which is the whole difficulty of these flags, so spell it out
/// once, in the `--help` the user is already reading.
const CONFLICT_HELP: &str = "\
CONFLICT RULES (--ours / --theirs / --union)

  Every merge here is between the stack being replayed onto and the change being
  applied to it:

    ours    the version already in the stack at that point — the rewritten
            commit the change is landing on. NOT your working copy.
    theirs  the change being applied: your staged hunk, or the commit being
            replayed/moved into that position.

  This is `git rebase`'s sense of the words, not `git merge`'s. Picking a side
  resolves every conflicting REGION that way instead of aborting; --union keeps
  both. Conflicts git cannot resolve at file level (a delete against a modify)
  still abort, and the abort is still byte-identical.";

impl Opts {
    fn ops(&self) -> ops::Opts {
        ops::Opts {
            ignore_ws: self.ignore_whitespace,
            favor: match (self.ours, self.theirs, self.union) {
                (true, _, _) => Some(git2::FileFavor::Ours),
                (_, true, _) => Some(git2::FileFavor::Theirs),
                (_, _, true) => Some(git2::FileFavor::Union),
                _ => None,
            },
            dry_run: self.dry_run,
            no_restack: self.no_restack,
        }
    }
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
    /// Remove a commit; everything after it replays as if it never existed.
    Drop {
        /// Commit to remove (revspec).
        rev: String,
    },
    /// Move a commit to another position in the stack.
    // Absolute positioning (`--before` / `--after` an anchor), not a relative
    // step count: it reads the way you say it out loud, works for any distance
    // in one shot, and can't be misread as "reorder these two" the way a
    // two-positional `reorder A B` can.
    #[command(group = clap::ArgGroup::new("anchor").required(true))]
    Reorder {
        /// Commit to move (revspec).
        rev: String,
        /// Put <rev> immediately before this commit (on its older side).
        #[arg(long, group = "anchor")]
        before: Option<String>,
        /// Put <rev> immediately after this commit (on its newer side).
        #[arg(long, group = "anchor")]
        after: Option<String>,
    },
    /// Fold a commit into its parent, keeping both commit messages.
    Squash {
        /// Commit to fold into its parent (revspec).
        rev: String,
        /// Message for the combined commit (default: both, parent first).
        #[arg(long, short = 'm')]
        message: Option<String>,
    },
    /// Replace a commit's message, keeping its author, date and content.
    // No editor: `-m` is required. Spawning $EDITOR means a temp file, a child
    // process and an "aborted because the message was empty" path — for a flag
    // the user can just type. `git commit --amend -m` sets the precedent.
    Reword {
        /// Commit to reword (revspec).
        rev: String,
        /// The new message.
        #[arg(long, short = 'm')]
        message: String,
    },
    /// Split a commit in two: <paths> become a commit before it, the rest stay.
    Split {
        /// Commit to split (revspec).
        rev: String,
        /// Paths whose changes move into the split-off commit.
        #[arg(required = true)]
        paths: Vec<String>,
        /// Message for the split-off commit (default: "<summary> (part 1)").
        #[arg(long, short = 'm')]
        message: Option<String>,
    },
    /// Pick hunks on screen — fold staged ones back, or move hunks between
    /// existing commits.
    Tui {
        /// Oldest commit to show (revspec); default: the newest 50 commits.
        #[arg(long)]
        base: Option<String>,
    },
    /// Put the branch back where the last git-transplant run found it.
    Undo {
        /// Show this branch's git-transplant history instead of undoing.
        #[arg(long)]
        list: bool,
    },
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

/// Resolve an optional `--base`-style revspec.
fn resolve_opt(repo: &Repository, rev: Option<&str>) -> Result<Option<git2::Oid>> {
    rev.map(|r| git_transplant::git::resolve(repo, r))
        .transpose()
        .map_err(anyhow::Error::msg)
}

fn restack_verb(dry: bool) -> &'static str {
    if dry {
        "would restack"
    } else {
        "restacked"
    }
}

/// Sibling branches carried across the rewrite. Printed on stdout, not stderr:
/// unlike the warnings this is something that *worked*.
fn report_restacks(repo: &Repository, o: &ops::Outcome, verb: &str) {
    for r in &o.restacked {
        println!("{verb} {r}");
    }
    // A commit that vanished because its change was already present is an
    // accidental squash — and it takes its message with it. Say so.
    for d in &o.dropped {
        let summary = repo
            .find_commit(*d)
            .ok()
            .and_then(|c| c.summary().map(str::to_owned))
            .unwrap_or_default();
        println!("dropped {d:.8} {summary} (became empty; its message is gone)");
    }
    for w in &o.warnings {
        eprintln!("warning: {w}");
    }
}

/// `undo --list` — the branch's git-transplant history, newest first, with the
/// entry a plain `undo` would reverse marked. Reading it off the reflog by hand
/// works, but only if you already know the message prefix and which direction
/// `old -> new` runs; this says both.
fn list_undo(repo: &Repository) -> Result<()> {
    let (branch, entries) = ops::undo_list(repo).map_err(anyhow::Error::msg)?;
    let branch = ops::short_branch(&branch);
    if entries.is_empty() {
        println!("{branch}: no git-transplant operations in its reflog");
        return Ok(());
    }
    println!("{branch}: {} git-transplant operation(s), newest first", entries.len());
    for e in &entries {
        let mark = if e.next { "*" } else { " " };
        println!("{mark} {:.8} -> {:.8}  {}", e.old, e.new, e.message);
    }
    // Naming the oid makes this checkable against `git log` before committing to it.
    let next = &entries[0];
    println!("(* = what `git-transplant undo` reverses, putting {branch} back at {:.8})", next.old);
    Ok(())
}

fn main() -> Result<()> {
    let opts = Opts::parse();
    let repo = Repository::discover(".").context("not inside a git repository")?;

    let gopts = opts.ops();

    // The TUI owns its own screen and reporting; arg-driven ops share one path.
    if let Cmd::Tui { base } = &opts.cmd {
        let base = resolve_opt(&repo, base.as_deref())?;
        return tui::run(&repo, base, gopts);
    }

    match opts.cmd {
        Cmd::Undo { list: true } => list_undo(&repo)?,
        Cmd::Undo { .. } => {
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
                report_restacks(&repo, &o, "un-restacked");
                // Undo moves the ref only, so whatever the undone op folded in is
                // still on disk — now as an uncommitted change.
                if ops::require_fully_clean(&repo).is_err() {
                    println!("worktree untouched: the undone change is uncommitted again");
                }
            }
        }
        Cmd::Absorb { base } => {
            let a = ops::collapse(&repo, resolve_opt(&repo, base.as_deref())?, &gopts)
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
                    report_restacks(&repo, &o, restack_verb(opts.dry_run));
                }
                None => println!("nothing absorbed ({} hunk(s) had no home in range)", a.orphans),
            }
        }
        cmd => {
            let outcome = match cmd {
                Cmd::Fix { target } => ops::fix(&repo, &target, &gopts),
                Cmd::MoveFile { path, target } => ops::mv(&repo, &path, &target, &gopts),
                Cmd::Drop { rev } => ops::drop_commit(&repo, &rev, &gopts),
                Cmd::Reorder { rev, before, after } => {
                    // clap's ArgGroup guarantees exactly one of the two.
                    let (anchor, before) = match (&before, &after) {
                        (Some(a), _) => (a, true),
                        (_, Some(a)) => (a, false),
                        _ => unreachable!(),
                    };
                    ops::reorder(&repo, &rev, anchor, before, &gopts)
                }
                Cmd::Reword { rev, message } => ops::reword(&repo, &rev, &message, &gopts),
                Cmd::Squash { rev, message } => {
                    ops::squash(&repo, &rev, message.as_deref(), &gopts)
                }
                Cmd::Split { rev, paths, message } => {
                    ops::split(&repo, &rev, &paths, message.as_deref(), &gopts)
                }
                Cmd::Absorb { .. } | Cmd::Tui { .. } | Cmd::Undo { .. } => unreachable!(),
            }
            .map_err(anyhow::Error::msg)?;

            if outcome.new_tip == outcome.old_tip {
                println!("no change");
            } else {
                println!("{}", report(&outcome, opts.dry_run));
            }
            report_restacks(&repo, &outcome, restack_verb(opts.dry_run));
        }
    }
    Ok(())
}
