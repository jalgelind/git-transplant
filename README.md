# git-transplant

Move changes around inside a stack of commits — without the
`git commit --fixup` + `rebase -i --autosquash` dance.

You're maintaining a branch as a series of logical commits. Review feedback lands
on commit 3 of 7. The fix belongs *there*, not in a "fixup" commit at the tip.
`git-transplant` puts it there and replays the rest of the stack for you.

```console
$ git add -p                     # stage just the fix
$ git-transplant absorb
absorbed 1 hunk(s) (0 left staged); main now at 3a71377f
```

That's it — the hunk was folded into the commit that owns those lines, every
later commit was replayed on top, and your worktree is clean.

## Why this one

Six things it does that the alternatives don't:

- **A failed run leaves your repo byte-identical.** The engine builds the whole
  rewritten stack as unreferenced git objects and moves the branch only on full
  success. There is no `.git/rebase-merge/`, no half-finished state, nothing to
  `--abort`. (`git absorb --and-rebase` inherits rebase's mess.)
- **It refuses to clobber a branch that moved underneath you.** The ref update is
  a compare-and-swap against the tip it started from. No other tool in this space
  does this.
- **Preview is literally the real run minus the ref move** — the same code path,
  with the result thrown away. It cannot disagree with what actually happens.
- **Conflicts tell you where the change belongs.**
- **You can move hunks *out* of one commit and into another.** `git absorb`
  can't do this at all.
- **The TUI never writes your worktree**, so you can reorganise history with
  work in progress on disk. `rebase -i` simply refuses.

## Install

```console
$ cargo install --path .
```

That puts `git-transplant` on your `PATH`, which also makes `git transplant …`
work — git finds any `git-<name>` executable as a subcommand.

## Which command?

| You know… | Use |
|---|---|
| …exactly which commit the change belongs to | `fix <target>` |
| …that it belongs *somewhere* back there | `absorb` |
| …a whole file was introduced in the wrong commit | `move-file <path> <target>` |
| …you want to see and pick, hunk by hunk | `tui` |

### `absorb` — let it work out the target

Stage a fix and let blame route each hunk to the commit that last touched those
lines. Hunks with no owner are **left staged** rather than guessed at:

```console
$ git-transplant absorb --base HEAD~2
absorbed 1 hunk(s) (1 left staged); main now at 0cff7e9d
$ git status --short
M  a.txt
```

`--base <rev>` bounds how far back it looks. Without it, the search runs to the
root (or the first merge commit — see *Requirements*).

### `fix <target>` — you know where it goes

Folds the staged change into `<target>`. If you aim at the wrong commit it
refuses, and tells you the right one:

```console
$ git-transplant fix HEAD~1
Error: conflict while rewriting 1ab2bb72 in cfg.txt — 7b7de062 owns those lines; try `fix 7b7de062` or `absorb`
```

Nothing moved. The branch is exactly where it was.

`fixup` is an alias, if that name is in your fingers.

### `move-file <path> <target>` — a file landed in the wrong commit

Re-anchors a whole file so it first appears at `<target>`, in **either**
direction. File modes survive.

*Later* — the file was introduced too early, so it's removed from the commits
before `<target>`:

```console
$ git-transplant move-file build.sh HEAD
main now at 380e5659
$ git ls-tree HEAD build.sh
100755 blob 4163036efa65bd4a469e752267498f01ea36a55c	build.sh
```

*Earlier* — `build.sh` landed with the entry point but belongs back with the
parser:

```console
$ git-transplant move-file build.sh HEAD~2
main now at 8bf7af54
$ git log --format='%h %s' --name-only
8bf7af5 add entry point

main.rs
6a3613c add cli

cli.rs
2225b42 add parser

build.sh
parser.rs
```

`move` still works as a (hidden) alias. The spelled-out name is the one to
reach for: in git-branchless, `git move` means "move a *subtree of commits*" — a
completely different operation.

**Limitations:**

- Moving a file *later* requires its content to be unchanged across the commits
  it's removed from. If something in between edits it, the move is refused
  (`<path> is modified at <oid>; move is not clean (aborting)`) rather than
  guessed at. Moving *earlier* has no such case — the file didn't exist yet.
- A commit that held *nothing but* the moved file survives as an empty commit.

### `tui` — see it and pick

```console
$ git-transplant tui
```

One screen. The left pane is your stack; the right pane shows either the
selected commit's diff (while you browse) or the hunk selector (once you focus
it with `Tab`).

Two things you can move:

- **Staged changes** → fold them into older commits. This is `absorb`/`fix` with
  your hands on the wheel.
- **A commit's own hunks** → press `s` on a commit to load *its* hunks, pick some
  with `Space`, then go to the destination commit and press `t`. This moves work
  between existing commits, which no CLI flag exposes.

`Enter` is a two-step gate: the first press reports the scope
(`rewrite 3 commit(s) on main …`), the second applies. `p` previews.

```
↑↓ nav · ←→/Tab pane · Home/End ends · PgUp/PgDn scroll · Esc back · q quit
Spc sel · t dest · s cmt-hunks · f fix-all · r reset · m move · p prev · ⏎ apply
```

Arrow-key driven — deliberately not vim bindings. `f` routes every selected hunk
to the commit under the cursor (a "fix"); `r` resets targets back to what
inference suggested (an "absorb"); `m` switches to move-a-whole-file mode.

## When a fix collides with a reindent

If a later commit reindented the line you're fixing, the merge conflicts. Ignore
whitespace and it folds cleanly:

```console
$ git-transplant fix HEAD~1
Error: conflict while rewriting a838ecf8 in f.rs — bf934d12 owns those lines; try `fix bf934d12` or `absorb`

$ git-transplant --ignore-whitespace fix HEAD~1
main now at a182ce81
```

The flag is global — it works before or after the subcommand.

## Undoing

Every operation writes a reflog entry, so the previous state is one command away:

```console
$ git reflog
0b31df0 HEAD@{0}: transplant: fix into b91461ae
b91461a HEAD@{1}: transplant: absorb staged change
6cd7ce3 HEAD@{2}: commit: wire cli

$ git reset --hard HEAD@{1}
```

There is no `git-transplant undo` yet, and the tool doesn't print the previous
tip on success — both are planned (see `docs/ROADMAP-NEXT.md`).

## Requirements and limits

- **Linear history.** The stack it will rewrite stops at the first merge commit.
  A merge deeper in your history is fine — it just bounds the window.
- **A clean-ish worktree for the CLI.** `fix` and `absorb` take your *staged*
  change as input and refuse to run with unrelated unstaged edits:

  ```console
  $ git-transplant absorb
  Error: working tree has unstaged/untracked changes; commit, stash, or clean first
  ```

  The TUI does **not** have this restriction, because it never writes your
  worktree.
- **Text files only.** Binary and non-UTF-8 files are skipped rather than
  risked; they're reported, not silently dropped.
- **Rewriting is rewriting.** Other branches pointing into the rewritten range
  are *warned about* but not moved — if you use stacked PRs, restack them
  yourself for now.
- GPG signatures are dropped on rewritten commits.

## How it works

One idea, in [`docs/DESIGN.md`](docs/DESIGN.md): every operation is a **recipe**
of per-commit edits handed to a single in-memory replay. The replay walks the
stack oldest-first, merges each commit onto its rewritten parent, injects any
edits for that commit, and produces new commit objects that nothing references
yet. Only if the whole walk succeeds does the branch ref move.

That's why abort is free, why preview is exact, and why there's no sequencer
state on disk.

## Development

```console
$ cargo test          # 94 tests
$ cargo clippy --all-targets
```

Roadmap and known gaps: [`docs/ROADMAP-NEXT.md`](docs/ROADMAP-NEXT.md).
