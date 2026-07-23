# git-transplant

Move changes around inside a stack of commits — without the
`git commit --fixup` + `rebase -i --autosquash` dance.

You keep a branch as a series of logical commits. Review feedback lands on
commit 3 of 7; the fix belongs *there*, not in a fixup commit at the tip.
`git-transplant` puts it there and replays the rest of the stack.

```console
$ git add -p                     # stage just the fix
$ git-transplant absorb
absorbed 1 hunk(s) (0 left staged); main now at cd521b1d (was 0d74445b; undo: git-transplant undo)
```

The hunk was folded into the commit that owns those lines, every later commit
was replayed on top, and your worktree is clean.

## Common tasks

| I want to… | Command |
|---|---|
| fold a staged fix into the commit that owns those lines | `git-transplant absorb` |
| fold it into a commit I name | `git-transplant fix <rev>` |
| move a whole file to the commit it belongs in | `git-transplant move-file <path> <rev>` |
| drop a commit | `git-transplant drop <rev>` |
| reorder a commit | `git-transplant reorder <rev> --before\|--after <rev>` |
| squash a commit into its parent | `git-transplant squash <rev>` |
| split a commit in two | `git-transplant split <rev> <paths>…` |
| fix a commit message | `git-transplant reword <rev> -m <msg>` |
| pick hunks by hand, staged or not | `git-transplant tui` |
| undo the last run | `git-transplant undo` |

Every command takes `-n`/`--dry-run` (report, change nothing), `--no-restack`
(leave sibling branches where they are), and `--ours`/`--theirs`/`--union`
(resolve conflicts by a fixed rule instead of aborting).

## Install

```console
$ cargo install --path .
```

That puts `git-transplant` on your `PATH`, which also makes `git transplant …`
work — git runs any `git-<name>` executable as a subcommand.

## Why git-transplant

- **A failed run leaves your repo byte-identical.** The rewritten stack is built
  as unreferenced git objects; the branch moves only on full success. No
  `.git/rebase-merge/`, no half state, nothing to `--abort`.
- **It won't clobber a branch that moved under you.** The ref update is a
  compare-and-swap against the tip it started from.
- **Your whole stack moves together.** Every other branch pointing into the
  rewritten range is restacked onto its counterpart. See [Stacked PRs](#stacked-prs).
- **Move hunks *out* of a commit and into another** — something `git absorb`
  can't do at all. Reorder, drop, squash and split without `rebase -i`.
- **`--dry-run` is the real run minus the ref move** — the same code path, so it
  cannot disagree with what actually happens.
- **The TUI never writes your worktree**, so you can reorganise history with work
  in progress on disk, and fold unstaged (or untracked) work directly.

## Commands

### `absorb` — let blame pick the target

Routes each staged hunk to the commit that last touched those lines. Hunks with
no owner are left staged rather than guessed at.

```console
$ git-transplant absorb --base HEAD~2
absorbed 1 hunk(s) (1 left staged); main now at 9564bf24 (was 2f864b21; undo: git-transplant undo)
$ git status --short
M  a.txt
```

`--base <rev>` bounds how far back it looks; without it the search runs to the
root (or the first merge — see [Requirements](#requirements-and-limits)).

### `fix <target>` — you know where it goes

Folds the staged change into `<target>`. Aim at the wrong commit and it refuses,
naming the right one:

```console
$ git-transplant fix HEAD~1
Error: conflict while rewriting 1ab2bb72 in cfg.txt — 7b7de062 owns those lines; try `fix 7b7de062` or `absorb`
```

Nothing moved. `fixup` is an alias.

### `move-file <path> <target>` — a file landed in the wrong commit

Re-anchors a whole file so it first appears at `<target>`, in either direction.
File modes survive.

```console
$ git-transplant move-file build.sh HEAD~2
main now at 1d4bc3f8 (was f3c1ee5c; undo: git-transplant undo)
```

`move` is a hidden alias, but prefer the spelled-out name: in git-branchless
`git move` means moving a *subtree of commits*.

- Moving a file *later* requires its content to be unchanged across the commits
  it's removed from; if something in between edits it, the move is refused rather
  than guessed at. Moving *earlier* has no such case.
- A commit that held nothing but the moved file is dropped — and named, because
  it takes its message with it.

### `reword <rev> -m <msg>` — the message was wrong

Author, date and content are preserved; only the message changes, and later
commits are replayed.

```console
$ git-transplant reword HEAD~1 -m "add parser"
main now at 50508bef (was 6972ed96; undo: git-transplant undo)
```

`-m` is required — no editor is spawned. Since the tree never changes, this is
the one rewrite that needs no clean worktree.

### `drop` / `reorder` / `squash` / `split` — reshape the stack

These change the shape of the stack rather than the contents of one commit. All
four need a clean worktree and print the tip they came from.

```console
$ git-transplant drop HEAD~1
main now at 330439d8 (was 53bdf164; undo: git-transplant undo)

$ git-transplant reorder HEAD --before HEAD~1
main now at a4361ba8 (was 330439d8; undo: git-transplant undo)

$ git-transplant squash HEAD          # keeps both messages, parent first (-m overrides)
main now at 3d2b1d44 (was 330439d8; undo: git-transplant undo)

$ git-transplant split HEAD a.rs      # a.rs becomes a commit before HEAD
main now at 7dc25c91 (was b8bdcf58; undo: git-transplant undo)
```

`reorder` positions *absolutely* against another commit (`--before`/`--after`),
so it can't be misread as "swap these two". Splitting *hunk by hunk* rather than
file by file is the TUI's `e` → `+ new commit` flow.

`drop` and `reorder` can genuinely conflict; the abort leaves ref and reflog
untouched. `squash` and `split` cannot conflict — each merges a change onto the
very tree it was authored against.

### `tui` — see it and pick

```console
$ git-transplant tui
```

The left pane is your stack. The right pane is split: what you're picking from
on top, the diff of the commit under the cursor below — so the hunk and its
destination are on screen together. Commit rows show the branches on them and a
count of how many picked hunks land there.

```
┌commits · ◀ target · N routed─┐┌[STAGED HUNKS] 1/1 selected · Enter: absorb───┐
│     + new commit at the tip  ││▶ [x] f.rs @@ -1,5 +1,…  → 18143cd5 c1        │
│▶    88fad2df c2 (master)     ││   l1                                         │
│  ◀1 18143cd5 c1              ││  -l2                                         │
│                              ││  +L2                                         │
│                              ││   l3                                         │
│                              │└──────────────────────────────────────────────┘
│                              │┌[DIFF] 88fad2df c2 (Tab: pick the hunks above)┐
│                              ││diff --git a/f.rs b/f.rs                      │
│                              ││@@ -6,3 +6,4 @@ l5                            │
│                              ││ l6                                           │
└──────────────────────────────┘└──────────────────────────────────────────────┘
↑↓ nav · ←→ pane · ⏎ apply · ? help · q quit
staged · hunk 1/1 · 1 picked · [x] f.rs → 18143cd5 c1
```

Four sources, four things you can move (`?` lists the keys for the current pane):

- **Staged changes** (default) — fold them into older commits.
- **Unstaged changes** (`w`) — the same, for work you never staged, including
  untracked files. The worktree is never written; the index is advanced only for
  the paths you folded.
- **A commit's own hunks** (`e`) — pick hunks with `Space`, go to a destination
  commit, `t`. Moves work between existing commits. Route them to the `+ new
  commit` row instead to split.
- **A whole file** (`m`) — re-anchor it; `/` filters the list.

In the commit pane, `[`/`]` move a commit, `d` drops, `s` squashes, `r` rewords
— through the same preview and two-step `Enter` as everything else. Applying
reloads the screen rather than quitting; `--base <rev>` bounds the stack
(default: newest 50).

## Conflicts

When a conflict is real rather than incidental, resolve every conflicting region
by a fixed rule instead of aborting. There is no `--continue`, no sequencer,
nothing on disk: you pick a rule and the run completes or aborts byte-clean.

**ours** is the stack you're replaying *onto*; **theirs** is the change being
applied (your staged hunk, or the commit being replayed). This is `git rebase`'s
sense of the words — "ours" is never your working copy.

```console
$ git-transplant drop HEAD~1
Error: conflict while rewriting 167c0fef in cfg.txt

$ git-transplant --theirs drop HEAD~1      # keep the commit being replayed
main now at ccee3582 (was 167c0fef; undo: git-transplant undo)
```

`--union` keeps both sides in order; `--ours`/`--theirs` keep one. They're
mutually exclusive, global, and honoured by the TUI. If a later commit reindented
the line you're fixing, `--ignore-whitespace` folds it cleanly.

## Preview — `--dry-run`

`-n`/`--dry-run` is the whole operation except the branch move: same guards, same
replay, same conflicts, and the tip it reports is the one you'd get.

```console
$ git-transplant --dry-run fix HEAD~2
main would move 9dfa2dbf -> a74cc220 (dry run; nothing changed)
```

For `absorb` it also prints the routing table — which hunk lands in which commit
— before anything is rewritten:

```console
$ git-transplant absorb -n
parser.rs
    @@ -1,5 +1,5 @@ -> fe24056a add parser
    @@ -9,7 +9,7 @@ fn parse(s: &str) -> Ast { -> dd46f8f0 add cli
would absorb 2 hunk(s) (0 left staged); main would move 5e358f60 -> 9ac3ba87 (dry run; nothing changed)
```

## Undo

Every run prints where it came from; `undo` puts the branch back:

```console
$ git-transplant absorb
absorbed 2 hunk(s) (0 left staged); main now at 9ac3ba87 (was 5e358f60; undo: git-transplant undo)

$ git-transplant undo
main restored to 5e358f60 (was 9ac3ba87; redo: git-transplant undo)
worktree untouched: the undone change is uncommitted again
```

It moves the ref and only the ref — an undo that can destroy work on disk isn't
one — through the same compare-and-swap, so it refuses if the branch moved since.
It's recorded as its own entry, so running `undo` twice is a redo. `undo --list`
shows the transplant entries and marks the one it would take. Restacked siblings
come back too.

## Stacked PRs

With ghstack, spr or Graphite every commit in your stack has a branch on it, and
rewriting the stack would strand them. It doesn't: every other local branch
pointing into the rewritten range is carried to its counterpart, on by default.

```console
$ git-transplant absorb
absorbed 1 hunk(s) (0 left staged); main now at bac0e6d3 (was 29add56f; undo: git-transplant undo)
restacked pr-2 0c55eef1 -> 5b373306
restacked pr-3 29add56f -> bac0e6d3
warning: tag v0.1 still points at 0c55eef1 (kept; a tag names a commit)
```

`undo` walks the siblings back too. Left alone deliberately: **tags** (a tag
names a specific commit — warned, not moved), **`refs/stash`** (still appliable
over a rewritten base), branches **checked out in another worktree** (refused),
and everything under **`--no-restack`**. A branch whose fork point is inside the
rewrite but whose tip is outside is a rebase, not a ref move — it's named with
the `git rebase --onto` that fixes it.

## Requirements and limits

- **Linear history.** The rewritable stack stops at the first merge commit; a
  merge deeper in history just bounds the window.
- **WIP is fine for `fix`/`absorb`** — they fold the *staged* change and leave
  unstaged edits alone. `move-file` and the shape verbs take no staged input, so
  from the CLI they need a fully clean tree; `reword` needs nothing.
- **Text files only.** Binary and non-UTF-8 files are reported and skipped, never
  silently dropped.
- **GPG signatures are dropped on rewritten commits**, but never silently — every
  run (and `--dry-run`) says how many signatures it costs and how to re-sign.

## How it works

Every operation is a **recipe** of per-commit edits handed to one in-memory
replay. The replay walks the stack oldest-first, merges each commit onto its
rewritten parent, injects that commit's edits, and produces new objects nothing
references yet; only on full success does the branch move. Because each commit is
merged against *its own* original parent tree, the walk is order-agnostic — hand
it a permuted or shortened commit list and you have reorder, drop, squash and
split.

That's why abort is free, preview is exact, and there's no sequencer state on
disk. Details in [`docs/DESIGN.md`](docs/DESIGN.md).

## Development

```console
$ cargo test          # 191 tests
$ cargo clippy --all-targets
```

Roadmap and known gaps: [`docs/ROADMAP-NEXT.md`](docs/ROADMAP-NEXT.md).
