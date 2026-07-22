# git-transplant

Move changes around inside a stack of commits — without the
`git commit --fixup` + `rebase -i --autosquash` dance.

You're maintaining a branch as a series of logical commits. Review feedback lands
on commit 3 of 7. The fix belongs *there*, not in a "fixup" commit at the tip.
`git-transplant` puts it there and replays the rest of the stack for you.

```console
$ git add -p                     # stage just the fix
$ git-transplant absorb
absorbed 1 hunk(s) (0 left staged); main now at cd521b1d (was 0d74445b; undo: git-transplant undo)
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
- **Preview is literally the real run minus the ref move** — `--dry-run` is the
  same code path with the result thrown away. It cannot disagree with what
  actually happens.
- **One command to undo**, and every run prints the tip it came from.
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
| …you want that last run back | `undo` |

Any of them takes `--dry-run` (`-n`) to report what would happen and change
nothing.

### `absorb` — let it work out the target

Stage a fix and let blame route each hunk to the commit that last touched those
lines. Hunks with no owner are **left staged** rather than guessed at:

```console
$ git-transplant absorb --base HEAD~2
absorbed 1 hunk(s) (1 left staged); main now at 9564bf24 (was 2f864b21; undo: git-transplant undo)
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
main now at f3c1ee5c (was 1d4bc3f8; undo: git-transplant undo)
$ git ls-tree HEAD build.sh
100755 blob 2b2219c3bd89ea6aa77c87ace021a8df576c657b	build.sh
```

*Earlier* — `build.sh` landed with the entry point but belongs back with the
parser:

```console
$ git-transplant move-file build.sh HEAD~2
main now at 1d4bc3f8 (was f3c1ee5c; undo: git-transplant undo)
$ git log --format='%h %s' --name-only
1d4bc3f add entry point

main.rs
f33325d add cli

cli.rs
fd0cefd add parser

build.sh
parser.rs
```

(That new tip is the *old* one: moving the file back reproduces the original
commits byte for byte.)

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
Error: conflict while rewriting b5b3d939 in f.rs — 088b1166 owns those lines; try `fix 088b1166` or `absorb`

$ git-transplant --ignore-whitespace fix HEAD~1
main now at 4c64220f (was 088b1166; undo: git-transplant undo)
```

The flag is global — it works before or after the subcommand.

## Seeing it first — `--dry-run`

`--dry-run` (`-n`) is the whole operation *except* the branch move: same guards,
same replay, same conflicts, and the tip it reports is the one you would get.
Works on `fix`, `move-file`, `absorb` and `undo`.

```console
$ git-transplant --dry-run fix HEAD~2
main would move 9dfa2dbf -> a74cc220 (dry run; nothing changed)
```

For `absorb` it also prints the routing table — which hunk lands in which commit,
before anything is rewritten:

```console
$ git-transplant absorb -n
parser.rs
    @@ -1,5 +1,5 @@ -> fe24056a add parser
    @@ -9,7 +9,7 @@ fn parse(s: &str) -> Ast { -> dd46f8f0 add cli
would absorb 2 hunk(s) (0 left staged); main would move 5e358f60 -> 9ac3ba87 (dry run; nothing changed)

$ git log --oneline -1
5e358f6 add entry point
```

Nothing moved: not the branch, not the reflog, not the worktree.

## Undoing

Every run tells you where it came from and how to get back:

```console
$ git-transplant absorb
absorbed 2 hunk(s) (0 left staged); main now at 9ac3ba87 (was 5e358f60; undo: git-transplant undo)

$ git-transplant undo
main restored to 5e358f60 (was 9ac3ba87; redo: git-transplant undo)
worktree untouched: the undone change is uncommitted again
```

`undo` reads the branch's reflog, finds the newest `transplant:` entry, and puts
the branch back where that entry found it — through the same compare-and-swap ref
move, so it refuses if the branch moved in the meantime:

```console
$ git-transplant undo
Error: main has moved since `transplant: fix into 0835331e` (now 9d00070d, expected a74cc220); refusing to undo
```

Two things worth knowing:

- **It moves the ref, and only the ref.** Your worktree and index are never
  checked out or reset, because an undo that can destroy work on disk is not an
  undo. The change the operation folded away simply reappears as an uncommitted
  edit — the state you were in before you ran it.
- **The undo is itself recorded as a `transplant:` entry**, so running `undo`
  twice is a redo.

The reflog is enough here because this tool only ever *moves one existing
branch* — it never creates or deletes refs, which is the case a reflog cannot
recover (and the reason git-branchless keeps its own event log). If you'd rather
do it by hand, the old tip is printed on every run, and it's all in the reflog:

```console
$ git reflog
a74cc22 HEAD@{0}: transplant: fix into 0835331e
9dfa2db HEAD@{1}: commit: add client
697c1b8 HEAD@{2}: commit: add server

$ git reset --hard HEAD@{1}
```

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
$ cargo test          # 101 tests
$ cargo clippy --all-targets
```

Roadmap and known gaps: [`docs/ROADMAP-NEXT.md`](docs/ROADMAP-NEXT.md).
