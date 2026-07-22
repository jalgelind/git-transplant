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

Things it does that the alternatives don't:

- **Your whole stack moves together.** Every other branch pointing into the
  rewritten range is restacked onto its rewritten counterpart — tags aren't,
  because a tag names a commit. See [Stacked PRs](#stacked-prs).
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
- **Conflicts tell you where the change belongs** — or resolve by a fixed rule
  (`--ours`/`--theirs`/`--union`) with no sequencer state to babysit.
- **You can move hunks *out* of one commit and into another.** `git absorb`
  can't do this at all.
- **Reorder, drop, squash and split without `rebase -i`** — with a live preview
  and a byte-identical abort. Sapling's ISL punts reordering to `histedit`;
  git-branchless has no TUI reorder. See
  [Reshaping the stack](#reshaping-the-stack--drop--reorder--squash--split).
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
| …a commit shouldn't be there at all | `drop <rev>` |
| …a commit belongs somewhere else in the stack | `reorder <rev> --before/--after <rev>` |
| …two commits should be one | `squash <rev>` |
| …one commit should be two | `split <rev> <paths>…` |
| …a commit message is wrong | `reword <rev> -m <msg>` |
| …you want to see and pick, hunk by hunk | `tui` |
| …you want that last run back | `undo` (`undo --list` to see the history first) |

Any of them takes `--dry-run` (`-n`) to report what would happen and change
nothing, `--no-restack` to leave sibling branches where they are, and
`--ours`/`--theirs`/`--union` to resolve conflicts by a fixed rule instead of
aborting (see [Conflict rules](#conflict-rules)).

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
- A commit that held *nothing but* the moved file has nothing left to say once
  the file lives elsewhere, so it is dropped — and named, because it takes its
  message with it:

  ```console
  $ git-transplant move-file build.sh HEAD~2
  main now at ccdee1b1 (was 3331ce72; undo: git-transplant undo)
  dropped eb8bfb96 add build script (became empty; its message is gone)
  ```

### `reword <rev> -m <msg>` — the message was wrong

Author, date and content are preserved; only the message changes, and the
commits after it are replayed:

```console
$ git log --oneline
6972ed9 add cli
ec8ef23 add parsr
635de2b add main
$ git-transplant reword HEAD~1 -m "add parser"
main now at 50508bef (was 6972ed96; undo: git-transplant undo)
$ git log --oneline
50508be add cli
c2ab4a0 add parser
635de2b add main
$ git show -s --format='%an %ad %s' HEAD~1
Demo Wed Jul 22 04:15:50 2026 +0200 add parser
```

`-m` is required — no editor is spawned. Firing up `$EDITOR` means a temp file,
a child process and an "aborted, the message was empty" path, for something you
can type inline; `git commit --amend -m` sets the same precedent.

Since the tree never changes, this is the one rewrite that neither needs a clean
worktree nor checks anything out.

### Reshaping the stack — `drop` / `reorder` / `squash` / `split`

These change the *shape* of the stack rather than the contents of one commit.
They are the operations that otherwise send you to `git rebase -i` — and once
you're there, you do the fixups there too.

They are not a second engine. The replay merges every commit against **its own**
original parent tree, so it was always an order-agnostic cherry-pick; these
verbs just hand it a different list of commits. Which means they inherit
everything: byte-identical abort, compare-and-swap promotion, `--dry-run`,
`undo`, and sibling restacking.

All four need a clean worktree (they take no staged input) and all four print
the tip they came from.

**`drop <rev>`** — the commit's change vanishes; later commits replay on top:

```console
$ git log --oneline
53bdf16 add cli
b52544c temp debug notes
953f80a add parser
809dfd1 add main
$ git-transplant -n drop HEAD~1
main would move 53bdf164 -> 330439d8 (dry run; nothing changed)
$ git-transplant drop HEAD~1
main now at 330439d8 (was 53bdf164; undo: git-transplant undo)
$ git log --oneline
330439d add cli
953f80a add parser
809dfd1 add main
```

**`reorder <rev> --before|--after <anchor>`** — positioning is *absolute*
against another commit, not a step count: it reads the way you'd say it out
loud, moves any distance in one shot, and can't be misread as "swap these two"
the way a two-positional `reorder A B` can.

```console
$ git-transplant reorder HEAD --before HEAD~1
main now at a4361ba8 (was 330439d8; undo: git-transplant undo)
$ git log --oneline
a4361ba add parser
3f33168 add cli
809dfd1 add main
$ git-transplant undo
main restored to 330439d8 (was a4361ba8; redo: git-transplant undo)
```

**`squash <rev>`** — folds a commit into its parent and **keeps both messages**,
parent first, blank line between — the same choice `git rebase -i`'s `squash`
makes. A commit message is something you typed; half of it disappearing silently
is a bug, not a convenience. `-m` overrides:

```console
$ git-transplant squash HEAD
main now at 3d2b1d44 (was 330439d8; undo: git-transplant undo)
$ git log -1 --format=%B
add parser

add cli

```

**`split <rev> <paths>…`** — the named paths become a commit *before* `<rev>`;
everything else stays. The split-off commit's message defaults to
`"<summary> (part 1)"` (`-m` overrides); the remainder keeps the original:

```console
$ git show --stat --oneline HEAD
b8bdcf5 add a and b
 a.rs | 1 +
 b.rs | 1 +
 2 files changed, 2 insertions(+)
$ git-transplant split HEAD a.rs
main now at 7dc25c91 (was b8bdcf58; undo: git-transplant undo)
$ git log --oneline -2
7dc25c9 add a and b
05bc555 add a and b (part 1)
```

Splitting *hunk by hunk* rather than file by file is the TUI's `s` flow.

**When it can't be done.** `drop` and `reorder` genuinely conflict when a later
commit depends on the lines you're moving — and the abort is byte-clean:

```console
$ git log --oneline
d6e0327 add punctuation
f5d017c greet the world
dc306be add main
$ git-transplant drop HEAD~1
Error: conflict while rewriting d6e03279 in main.rs
$ git log --oneline -1 && git reflog -1
d6e0327 add punctuation
d6e0327 HEAD@{0}: commit: add punctuation
```

Ref *and* reflog untouched — there is nothing to clean up and nothing to
`--abort`. If you want it done anyway, pick a rule: `--ours`, `--theirs` or
`--union` (see [Conflict rules](#conflict-rules)). `squash` and `split` cannot
conflict at all: both merge a change onto the very tree it was authored against,
so the 3-way merge is trivial and the commits above them see an unchanged tree.

**Your other branches come too.** A sibling branch follows its *commit*, not its
old position:

```console
$ git log --oneline --decorate
cbadb17 (HEAD -> main, pr-3) add cli
1bb66db (pr-2) temp debug notes
28208d4 (pr-1) add parser
9ab95f7 add main
$ git-transplant drop pr-2
main now at a637d686 (was cbadb176; undo: git-transplant undo)
restacked pr-2 1bb66db1 -> 28208d44
restacked pr-3 cbadb176 -> a637d686
```

`pr-2` sat on the commit that was dropped, so it lands on that commit's parent —
which is now what its branch actually contains.

### `tui` — see it and pick

```console
$ git-transplant tui
```

One screen. It offers the newest **50** commits by default — every row costs a
tree diff to load and widens the blame window, and nobody reorders the commit
400 back. `--base <rev>` overrides it in either direction, and the commit pane
says when the view is bounded (`commits · 50 shown (--base widens)`).

The left pane is your stack; the right pane shows either the selected commit's
diff (while you browse) or whatever you are picking from (once you focus it with
`Tab`). It is **object–verb**: the focused pane IS the object selector, and there
is exactly one state axis — the *source* of the right pane's rows.

Three sources, three things you can move:

- **Staged changes** (the default) → fold them into older commits. This is
  `absorb`/`fix` with your hands on the wheel.
- **A commit's own hunks** → press `e` on a commit to load *its* hunks, pick some
  with `Space`, then go to the destination commit and press `t`. This moves work
  between existing commits, which no CLI flag exposes.
- **A whole file** (`m`) → re-anchor it at another commit; this is `move-file`.

And, in the commit list, **the shape of the stack itself**: `[` and `]` move the
selected commit up and down, `d` marks it dropped, `s` squashes it into its
parent. The pending edit shows in the list (`✗` / `⇣`, and the reorder is drawn
where it would land) until you preview or apply it. **This is the part that
exists nowhere else**: Sapling's ISL hands reordering off to `histedit`, and
git-branchless has no TUI reorder at all.

`Enter` is a two-step gate: the first press reports the scope
(`rewrite 3 commit(s) on main …`), the second applies. `p` previews. `Esc`
cancels a pending shape edit and puts the list back. Applying does **not** quit —
the screen reloads onto the stack it just produced, so you can keep going, and
`u` undoes the last transplant (one key, no gate: it moves the branch ref and
nothing else, and pressing it again is the redo).

One line of keymap is always on screen, and `?` opens the rest — scoped to the
screen you are actually on, because most of the eleven operations' keys no-op on
any given one (`d drop` does nothing in the hunk pane):

```
↑↓ nav · ←→ pane · ⏎ apply · ? help · q quit
```

```
┌commits · ◀ = target (t sets)─┐┌[DIFF] bbc8e88d c2 (Tab: pick staged hunks)───┐
│▶  bbc8e88d c2                ││diff --git a/f.rs b/f.rs                      │
│  ◀3885670f c1                ││index a52ef27..147f509 100644                 │
│       ┌ Commits — any key closes ────────────────────────────────────┐       │
│       │The stack, newest first — edit one, or send hunks to it.      │       │
│       │                                                              │       │
│       │    e  open this commit's hunks, to take some out             │       │
│       │    t  make this commit the destination                       │       │
│       │    f  send every picked hunk here at once                    │       │
│       │  [ ]  move this commit earlier / later                       │       │
│       │    d  drop this commit                                       │       │
│       │    s  squash it into the one below                           │       │
│       │    r  reword its message                                     │       │
│       │                                                              │       │
│       │    p  preview — what would change, nothing written           │       │
│       │    ⏎  apply (press twice; the first press reports scope)     │       │
│       │    u  undo the last transplant                               │       │
│       │c / i  conflict rule · ignore whitespace                      │       │
│       │  Esc  step back · q quit                                     │       │
│       └──────────────────────────────────────────────────────────────┘       │
└──────────────────────────────┘└──────────────────────────────────────────────┘
↑↓ nav · ←→ pane · ⏎ apply · ? help · q quit
staged · hunk 1/1 · 1 picked · [x] f.rs → 3885670f c1
Enter: absorb 1 staged hunk(s) into inferred commits · p: preview first
```

Cross to the hunk pane and the same key describes *that* screen instead — `Spc
pick / unpick this hunk`, `a accept the target git blame inferred`, and no shape
verbs at all. It is transient: **any** key closes it, and the key that closes it
does nothing else, so dismissing help can never be the keystroke that drops a
commit.

**Splitting a commit by hunk** needs no new key at all. While a commit's hunks
are open, the commit list grows a phantom row at the top — `+ new commit here`,
a destination that does not exist yet. Pick the hunks you want to separate, put
the cursor on that row, and press `t`: they route into a new commit inserted
immediately before the source (marked `⌁`). `Enter` names it and applies.

```
┌commits · + = new commit here─┐┌[NEW COMMIT] ⏎ names it and splits────────────┐
│▶ ◀+ new commit here          ││A new commit, inserted before the one these hu│
│  ⌁3d5c7a54 c2 two unrelated e││f.rs @@ -1,5 +1,5 @@                          │
│   452075f7 c1 base           ││                                              │
└──────────────────────────────┘└──────────────────────────────────────────────┘
↑↓ nav · ←→ pane · ⏎ apply · ? help · q quit
from 3d5c7a54 · hunk 1/2 · 1 picked · [x] f.rs → + new commit
hunk → a NEW commit before the source (split) — ⏎ names it
```

`?` there describes the phantom row rather than the commit list, because it is a
destination and not a commit — every commit verb refuses on it, so offering them
would be exactly the lie the scoping exists to prevent:

```
┌ + new commit here — any key closes ──────────────────────────┐
│A destination that does not exist yet — split hunks into it.  │
│                                                              │
│    t  route the picked hunks into a new commit               │
│    ⏎  name it and apply the split                            │
│   ↑↓  back down to the real commits                          │
```

That is `split` at hunk granularity, which the CLI's `split <rev> <paths>…`
cannot do — and it is the same plan underneath: the split-off commit is a
dangling synthetic that takes a slot in the replay order ahead of `rev`, so it
cannot conflict. The message defaults to `<summary> (part 1)`, matching the CLI.

`r` rewords the commit under the cursor through an inline prompt that replaces
the status line — the hint and context lines above it stay exactly where they
were, so you can still see what you are naming:

```
↑↓ nav · ←→ pane · ⏎ apply · ? help · q quit
staged · hunk 1/1 · 1 picked · [x] f.rs → 3885670f c1
message: c2 renamed▏   ⏎ ok · Esc cancel
```

It is prefilled with the summary and **preserves the body**: only the headline
is edited, so rewording never silently deletes the paragraphs under it. While it
is open it swallows every key — `q` does not quit, `Enter` does not apply, and
`?` types a question mark instead of opening help. There is no `$EDITOR`, for the
same reason `reword -m` refused one: a temp file, a child process and an
empty-message abort path, for something you can type inline.

Arrow-key driven — deliberately not vim bindings, and **no shift keys at all**;
the letters are `git rebase -i`'s where they exist. Scoping `?` to the **focused
pane** is what keeps it both short and true: one flat list of every verb neither
fits the box nor stays honest, since most keys refuse on any given screen. `f`
routes every selected hunk to the commit under the cursor (a "fix"); `a` resets
targets back to what inference suggested (an "absorb").

`c` cycles the conflict rule abort → ours → theirs → union and `i` toggles
`--ignore-whitespace`. Both re-preview immediately and both leave a **sticky
badge** on the context line for as long as they are set — a merge rule or a
whitespace mode you cannot see is the dangerous one:

```
↑↓ nav · ←→ pane · ⏎ apply · ? help · q quit
staged · hunk 1/1 · 1 picked · [x] f.rs → 3885670f c1 · rule:ours · ignore-ws
clean, would move master to e299a864
```

Because the TUI never writes your worktree, all of this works with uncommitted
work on disk — `rebase -i` refuses outright.

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

## Conflict rules

`--ours`, `--theirs` and `--union`. When a conflict is real rather than
incidental, you can resolve every conflicting region by a fixed rule instead of
aborting. There is **no
`--continue`, no sequencer, nothing on disk** — you pick a rule, the run either
completes or aborts byte-clean, exactly as before.

Which side is which is the whole difficulty, so: **ours** is the stack you are
replaying *onto* — the rewritten commit the change lands on. **theirs** is the
change being applied: your staged hunk, or the commit being replayed into that
position. This is `git rebase`'s sense of the words, not `git merge`'s — "ours"
is never your working copy.

```console
$ git log --oneline
167c0fe bump it again
f3dd0cc bump the timeout
b4684d2 add config
$ git-transplant drop HEAD~1
Error: conflict while rewriting 167c0fef in cfg.txt

$ git-transplant --theirs drop HEAD~1      # keep the commit being replayed
main now at ccee3582 (was 167c0fef; undo: git-transplant undo)
$ cat cfg.txt
timeout = 30
```

`--ours` is the mirror image, and here it makes the point about honesty: keeping
the stack's own version leaves "bump it again" with nothing to say, so it is
dropped — and said out loud:

```console
$ git-transplant --ours drop HEAD~1
main now at b4684d22 (was 167c0fef; undo: git-transplant undo)
dropped 167c0fef bump it again (became empty; its message is gone)
$ cat cfg.txt
timeout = 1
```

`--union` keeps both sides, in order, with no conflict markers:

```console
$ git-transplant --union drop HEAD~1
main now at 7e943e14 (was a3778ad0; undo: git-transplant undo)
$ cat cfg.txt
timeout = 1
timeout = 30
```

The three are mutually exclusive, they are global (any verb that can conflict
takes them, and the TUI honours them too), and they resolve conflicting
*regions* — a clash git cannot resolve at file level (a delete against a modify)
still aborts, byte-clean.

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

`undo --list` shows what it has to work with, and marks the one it would take:

```console
$ git-transplant undo --list
main: 2 git-transplant operation(s), newest first
* 34ba82dc -> cf42ac99  transplant: reword 34ba82dc
  ec99d03a -> 34ba82dc  transplant: fix into 5b4f440c
(* = what `git-transplant undo` reverses, putting main back at 34ba82dc)
```

Two things worth knowing:

- **It moves the ref, and only the ref.** Your worktree and index are never
  checked out or reset, because an undo that can destroy work on disk is not an
  undo. The change the operation folded away simply reappears as an uncommitted
  edit — the state you were in before you ran it.
- **The undo is itself recorded as a `transplant:` entry**, so running `undo`
  twice is a redo. `--list` shows that rather than hiding it — after an undo,
  the marked entry *is* the undo:

  ```console
  $ git-transplant undo
  main restored to 34ba82dc (was cf42ac99; redo: git-transplant undo)

  $ git-transplant undo --list
  main: 3 git-transplant operation(s), newest first
  * cf42ac99 -> 34ba82dc  transplant: undo (transplant: reword 34ba82dc)
    34ba82dc -> cf42ac99  transplant: reword 34ba82dc
    ec99d03a -> 34ba82dc  transplant: fix into 5b4f440c
  (* = what `git-transplant undo` reverses, putting main back at cf42ac99)
  ```

  So `undo` is exactly one step, in whichever direction you last went. Walking
  further back is deliberately not built: it would mean skipping entries whose
  `id_new` no longer matches, and the reflog is right there.
- **Restacked siblings come back too** (see [Stacked PRs](#stacked-prs)) — each
  by the same compare-and-swap, so one that has moved on since is left alone.

The reflog is enough here because this tool only ever *moves existing branches* —
it never creates or deletes refs, which is the case a reflog cannot
recover (and the reason git-branchless keeps its own event log). If you'd rather
do it by hand, the old tip is printed on every run, and it's all in the reflog:

```console
$ git reflog
a74cc22 HEAD@{0}: transplant: fix into 0835331e
9dfa2db HEAD@{1}: commit: add client
697c1b8 HEAD@{2}: commit: add server

$ git reset --hard HEAD@{1}
```

## Stacked PRs

If you use ghstack, spr or Graphite, every commit in your stack has a branch on
it — and rewriting the stack would strand all of them on orphaned history. It
doesn't: **every other local branch pointing into the rewritten range is carried
to its rewritten counterpart**, through the same compare-and-swap ref move, with
its own `transplant: restack …` reflog entry.

```console
$ git log --oneline --decorate --all
29add56 (HEAD -> main, pr-3) add cli
0c55eef (tag: v0.1, pr-2) add server
fc0c476 add parser

$ git add -p                     # a fix that belongs in "add parser"
$ git-transplant absorb
absorbed 1 hunk(s) (0 left staged); main now at bac0e6d3 (was 29add56f; undo: git-transplant undo)
restacked pr-2 0c55eef1 -> 5b373306
restacked pr-3 29add56f -> bac0e6d3
warning: tag v0.1 still points at 0c55eef1 (kept; a tag names a commit)

$ git log --oneline --decorate main pr-2 pr-3
bac0e6d (HEAD -> main, pr-3) add cli
5b37330 (pr-2) add server
d9292be add parser
```

The whole stack moved together. `undo` walks the siblings back too:

```console
$ git-transplant undo
main restored to 29add56f (was bac0e6d3; redo: git-transplant undo)
un-restacked pr-2 5b373306 -> 0c55eef1
un-restacked pr-3 bac0e6d3 -> 29add56f
```

A branch whose **tip** is outside the rewrite but whose **fork point** is inside
it is a different problem: landing it means replaying its own commits, which is
a rebase, not a ref move. It is named, with the command that fixes it:

```console
$ git-transplant fix HEAD~2
main now at 795ac92b (was c2cf7511; undo: git-transplant undo)
warning: feature forked at f226882a, which was rewritten — its own commits are now on orphaned history (`git rebase --onto c870f549 f226882a feature`)
```

Four things are deliberately left alone:

- **Tags.** A tag names a *specific historical commit* — silently redefining
  what `v0.1` points at because an unrelated branch was rewritten is not a
  favour. Tags are warned about, never moved.
- **`refs/stash`.** A stash is applied as a 3-way merge of `stash^..stash` onto
  whatever HEAD is *now*, and `refs/stash` keeps its own base commit alive, so
  rewriting that base leaves the stash perfectly appliable. There is nothing to
  move and nothing to warn about; a test applies a stash across a rewrite to
  keep that true.
- **Branches checked out in another `git worktree`.** Moving one would leave
  that worktree's HEAD pointing somewhere its files and index don't match, so
  it's refused with a warning.
- **Anything, under `--no-restack`** — the old warn-only behaviour, if you'd
  rather move the refs yourself:

  ```console
  $ git-transplant absorb --no-restack
  absorbed 1 hunk(s) (0 left staged); main now at bac0e6d3 (was 29add56f; undo: git-transplant undo)
  warning: pr-2 still points into the rewritten range (now orphaned)
  warning: pr-3 still points into the rewritten range (now orphaned)
  warning: tag v0.1 still points at 0c55eef1 (kept; a tag names a commit)
  ```

Restacking is **on by default** because the failure it prevents is silent: a
stranded branch still resolves, still pushes, and only turns into a mess at
review time. Opting out is one flag; noticing you needed it is a bad afternoon.
`--dry-run` lists the moves before any of them happen.

A branch sitting on a commit that gets *dropped* (`absorb` removes a commit whose
change was fully folded elsewhere) lands on that commit's **rewritten parent** —
which has the identical tree, since being identical is exactly why it was
dropped. The branch keeps naming the same content.

## Requirements and limits

- **Linear history.** The stack it will rewrite stops at the first merge commit.
  A merge deeper in your history is fine — it just bounds the window.
- **Work in progress is fine for `fix` and `absorb`.** They fold your *staged*
  change, and the rewritten tip's tree is that same index tree — so unrelated
  unstaged edits are simply left alone (the checkout is skipped rather than
  allowed to run over them), exactly as in the TUI:

  ```console
  $ git status --short
  M  lib.rs                        # the fix, staged
   M notes.txt                     # unrelated work in progress
  $ git-transplant absorb
  absorbed 1 hunk(s) (0 left staged); main now at 04b1b633 (was 1f83ef4b; undo: git-transplant undo)
  $ git status --short
   M notes.txt
  ```

  `move-file` and the shape verbs (`drop`/`reorder`/`squash`/`split`) take no
  staged input at all, so from the CLI they still require a *fully* clean tree.
  `reword` needs nothing: it changes no tree.
- **Text files only.** Binary and non-UTF-8 files are skipped rather than
  risked; they're reported, not silently dropped.
- **Tags never move** (see [Stacked PRs](#stacked-prs)), and neither does a
  branch checked out in another `git worktree`.
- **GPG signatures are dropped on rewritten commits** — but never silently. Any
  run that would rewrite a signed commit says how many signatures it costs, and
  `--dry-run` says it before anything moves:

  ```console
  $ git-transplant --dry-run fix HEAD~2
  main would move 2c12efd8 -> 954b0b16 (dry run; nothing changed)
  warning: 1 signed commit(s) in the rewritten range — the rewrites are UNSIGNED (re-sign with `git rebase --exec 'git commit --amend --no-edit -S' <base>`)
  ```

  The TUI puts the same count in its arming line, next to the commit count. It
  is a warning rather than a re-sign because git2 has no signing support at all:
  re-signing would mean shelling out to `gpg` once per rewritten commit, which
  is a hard dependency and a per-commit subprocess across a whole stack, to
  reproduce what `git rebase --exec` already does on demand.

## How it works

One idea, in [`docs/DESIGN.md`](docs/DESIGN.md): every operation is a **recipe**
of per-commit edits handed to a single in-memory replay. The replay walks the
stack oldest-first, merges each commit onto its rewritten parent, injects any
edits for that commit, and produces new commit objects that nothing references
yet. Only if the whole walk succeeds does the branch ref move.

That's why abort is free, why preview is exact, and why there's no sequencer
state on disk.

Because each commit is merged against **its own** original parent tree, the walk
is order-agnostic: hand it a *permuted* or *shortened* list of commits and you
have reorder, drop, squash and split — plan-builders, not a second engine.

## Development

```console
$ cargo test          # 147 tests
$ cargo clippy --all-targets
```

Roadmap and known gaps: [`docs/ROADMAP-NEXT.md`](docs/ROADMAP-NEXT.md).
