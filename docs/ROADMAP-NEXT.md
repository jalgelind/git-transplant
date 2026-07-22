# git-transplant — Roadmap (next)

Forward-looking plan. Shipped history lives in [ROADMAP.md](ROADMAP.md); the
engine architecture in [DESIGN.md](DESIGN.md). Everything here is **open work**,
derived from a workflow investigation and a five-reviewer codebase audit.

## Where we are

Both halves now work and are hardened: **147 tests**, clippy clean. Commands
`fix`, `move-file`, `absorb`, `drop`, `reorder`, `squash`, `split`, `reword`,
`tui`, `undo`, plus `--ignore-whitespace`, `--dry-run`, `--no-restack`,
`--ours`/`--theirs`/`--union` and `tui --base`. A README
exists, written by running the binary and quoting its real output. The engine is
an in-memory replay producing dangling objects,
promoted by a compare-and-swap ref move with a reflog entry — so a failed run
leaves the repo byte-identical and a branch that moved underneath you is never
clobbered. Those guarantees are now *visible*: every run prints the tip it came
from, `--dry-run` shows the outcome first, and `undo` walks it back.

## The core finding — closed in M4

**Coverage was excellent on one axis and absent on the other.** The tool owns
*"this change belongs to an older commit"* — `fix`/`absorb`/the TUI cover it
better than any peer, and moving hunks *out of* an existing commit is something
`git absorb` cannot do at all. But every operation that changed the **shape** of
the stack — reorder, drop, squash, split — had no path. Users dropped to
`git rebase -i`, **and once they were there they did the fixup there too**.

The gap was as small as predicted. `replay` merges each commit against *its own
original parent tree*, so the loop was **already an order-agnostic cherry-pick**;
`engine::replay_order` just takes the `Vec<Oid>` instead of deriving it, and
`replay` is now a two-line wrapper that derives it. Reorder and drop are pure
permutation/subset. Squash is a subset plus `ApplyChange(child)` at the parent
and a message override. Split turned out to need no engine change either: the
split-off commit is a **dangling synthetic parented at `rev`'s parent that simply
takes a slot in the order** ahead of `rev`. Total engine delta: ~30 lines.

Two properties fell out of that construction and are now asserted:

- **`squash` cannot conflict.** The child's delta is merged onto the parent's own
  original tree, so `ours == base` and the merge is trivial; the commits above
  see an unchanged tree.
- **`split` cannot conflict**, for the same reason — and `rev`'s rewritten tree
  is byte-identical to its original, so nothing above it can break.

`drop` and `reorder` conflict for real, and abort byte-clean (ref *and* reflog)
— or resolve by a fixed rule, since `--ours`/`--theirs`/`--union` landed. And
`reword` was indeed ~5 lines on top of `Recipe::set_message`.

## Adoption blockers

1. ~~No README.~~ **Done** — and writing it *found a bug*: the TUI silently
   dropped staged binary files instead of reporting them. Verifying prose against
   the binary is now the standard for this repo; claims get a real terminal
   transcript or they don't ship.
2. ~~**Rewriting a stack strands sibling branch refs**~~ **Done in M3.**
   `engine::replay` now returns the old→new map it always computed, and
   `ops::restack` promotes every other local branch in the range through the same
   compare-and-swap. On by default (`--no-restack` opts out); tags and branches
   held by a linked worktree are warned about, not moved.

## Recommended sequence

Ordered by the user-visible outcome each milestone buys, not by raw effort.

~~**M1 — Credibility** (hours). `move`'s "path not found" lie (T6) and the naming
/ help / short-branch polish (rest of T5).~~ **Done.** `move-file` re-anchors in
both directions (`fixup`/`move` kept as aliases), `--help` lost the op B/C/D
jargon, and the CLI prints `main`, not `refs/heads/main`.

~~**M2 — Confidence** (days). `undo` + print the old tip (T1), `--dry-run` (T2).~~
**Done.** Every run prints `main now at <new> (was <old>; undo: git-transplant
undo)`; `undo` walks the branch's own reflog back through the same
compare-and-swap promote (ref only — it never writes the worktree, so it cannot
destroy work, and it is its own redo); `-n` runs the full replay and reports the
tip it would produce, with a `hg absorb -n`-style routing table for `absorb`.

~~**M3 — Stacked-PR safe** (small). Restack siblings (T3).~~ **Done.** Sibling
branches follow the rewrite by default; a branch on a *dropped* commit lands on
that commit's rewritten parent (identical tree, which is why it was dropped);
tags never move; a branch checked out in a linked worktree is refused; `undo`
walks the sibling moves back too.

~~**M4 — The strategic bet** (weeks). `reorder`/`drop`/`squash` (T7) then `split`
(T8).~~ **Done.** All four ship as CLI verbs, and `[`/`]`/`d`/`S` expose
reorder/drop/squash in the TUI's commit pane through the existing `p` preview and
two-step Enter — reorder with live preview and byte-identical abort, which exists
in no other tool:

```console
$ git-transplant drop pr-2
main now at a637d686 (was cbadb176; undo: git-transplant undo)
restacked pr-2 1bb66db1 -> 28208d44
restacked pr-3 cbadb176 -> a637d686
```

~~**Ongoing / opportunistic.** `reword` (T4), `--ours/--theirs` (T9), `--base`
(T10), correctness backlog.~~ **Done** — all four, in that order:

```console
$ git-transplant drop HEAD~1
Error: conflict while rewriting 167c0fef in cfg.txt
$ git-transplant --theirs drop HEAD~1
main now at ccee3582 (was 167c0fef; undo: git-transplant undo)
$ git-transplant --ours drop HEAD~1
main now at b4684d22 (was 167c0fef; undo: git-transplant undo)
dropped 167c0fef bump it again (became empty; its message is gone)
```

The `--ours` run is the whole design in one line: a fixed rule can empty a
commit, and the tool says so rather than losing the message quietly.

**Not done in M4:** `split` is CLI-only and splits by **path**, not by hunk.
Hunk-granular splitting is reachable today through the TUI's `s` flow (load a
commit's hunks, route them elsewhere), but "split this commit in two *here*" is
not a TUI gesture. The plan-level primitive it would need already exists — a
synthetic commit in the order — so it is wiring, not design.

## Tier 1 — high value, cheap given the engine

| # | Item | Note |
|---|---|---|
| T1 | ~~`undo` + always print the old tip~~ ✅ | Done in M2 |
| T2 | ~~`--dry-run` / `absorb -n`~~ ✅ | Done in M2 |
| T3 | ~~Restack sibling refs instead of warning~~ ✅ | Done in M3 |
| T4 | ~~`reword <rev> -m`~~ ✅ | Done. `set_message` + `replay`, no plan builder at all. `-m` is REQUIRED (no `$EDITOR`: a temp file, a child process and an empty-message abort path, for something you can type). The tree is unchanged, so it is the one verb that needs no clean tree and checks nothing out |
| T5 | ~~README + naming & help text~~ ✅ | Done in M1 |
| T6 | ~~Fix `move`'s misleading error~~ ✅ | Done in M1 — the backward case is *supported*, not just reported |

## Tier 2 — real, moderate

| # | Item | Note |
|---|---|---|
| T7 | ~~`reorder` / `drop` / `squash`~~ ✅ | Done in M4, exactly as predicted: `engine::replay_order` takes the `Vec<Oid>`; reorder = permute, drop = omit, squash = omit + `ApplyChange` at the parent + concatenated messages |
| T8 | ~~`split`~~ ✅ (by path) | Done in M4 — and it needed *no* engine change: the split-off commit is a dangling synthetic in the replay order. Hunk-granular splitting from the TUI is still open |
| T9 | ~~`--ours/--theirs/--union`~~ ✅ | Done, and *global* rather than per-verb: one `git::Merge` (ignore-ws + `file_favor`) replaces the `ignore_ws: bool` the engine passed around, so every merge — replay, `ApplyChange`, `RevertChange` — honours it, TUI included. Zero persisted state |
| T10 | ~~`--base` bound~~ ✅ | Done for `tui`, with a **default of 50** (`hg absorb`'s cap; `git absorb` uses 10), overridable either way. The bound is not a display filter: it is threaded into `recipe::stack`, so a shape edit plans against exactly the list on screen |

## Tier 3 — explicitly NOT doing

- **Event-log undo** (SQLite + hooks) — the reflog covers this single-branch model.
- **jj-style first-class conflicts** — different object model; the current
  byte-clean abort is already better than rebase's half-state.
- **Interactive `--continue`** — the design was killed twice, correctly, and
  `--ours`/`--theirs`/`--union` is the deliberately lazy answer instead: a rule
  chosen up front, no sequencer, no state on disk. Tracked as backlog #9 in
  [ROADMAP.md](ROADMAP.md) with a corrected design if anyone ever needs more.
- **Anything remote** (PR creation, push, landing) — `gt submit` / `spr` /
  `ghstack` own that. We move *local* refs only, and that is the correctness story.
- **Merge-commit support** — the linear restriction is what keeps the engine simple.

## Correctness & cleanup backlog (from the audit)

Low severity, none urgent, all verified:

- `undo` walks exactly **one** step: because it records its own move as a
  `transplant:` entry, a second `undo` is a redo rather than a step further back.
  Walking a whole history of transplants would mean skipping entries whose
  `id_new` no longer matches — deliberately not built until someone wants it.

- ~~`mv` replays with `drop_empty` off, leaving an empty commit~~ **Fixed.**
  `mv` now replays with `drop_empty` on, like `git rebase`. M1 deferred this
  because it moved three tests' expectations; what changed since is that M4 made
  the drop *reported* (`dropped <oid> <summary> (became empty; its message is
  gone)`), so the objection — a commit and its message vanishing silently — no
  longer applies. The three tests now assert the drop and that it is named.
- ~~`drop_empty` deletes commits with **no report**~~ — fixed in M4:
  `Replay::dropped` / `Outcome::dropped` name every commit that vanished and the
  CLI prints `dropped <oid> <summary> (became empty; its message is gone)`. The
  *accidental* squash is no longer silent — including from `absorb`, which is
  now pinned by a test. ~~`empties_source()` in the TUI is still wrong in both
  directions~~ **fixed by deleting it**: the arming step runs the replay (the
  same call `p` makes) and names whatever `drop_empty` actually removed, so it
  can neither miss a drop caused by a file skipped at load nor promise one for a
  commit that survives.
- `restack` misses `refs/stash`, checks only ref *tips* (not descendants), and
  turns a `references()` error into one warning rather than a refusal. (The
  linked-worktree case *is* handled: those branches are refused, not moved.)
- Remote-tracking refs are ignored by design — `restack` moves local branches
  only, and pushing the restacked stack stays the user's (or `gt`/`spr`'s) call.
- ~~`promote(sync=true)` leaves the new tip unnamed if the ref move fails~~
  **Named.** The error now carries it: "the worktree already holds <tip>;
  `git reset --hard <tip>` keeps it, `git checkout -f <branch>` discards it".
  The ordering itself is unchanged — checking out after the ref move trades this
  case for a worse one (ref moved, worktree stale) and neither is reachable
  without a concurrent writer.
- `replay` returns the original `tip` when `base=None` and every commit drops —
  degenerate, but an inconsistent contract. The same case is the one hole in the
  old→new map: such a commit has no rewritten parent to land a ref on, so a
  branch there is warned about rather than moved.
- ~~**CLI/TUI inconsistency** on unstaged churn~~ **Aligned, on the TUI's
  terms.** The guard only ever existed to protect the force checkout, and that
  checkout is *tidiness*: `fix`/`absorb` fold the INDEX, and the rewritten tip's
  tree is that same index tree, so not checking out is already consistent. So
  the checkout is now skipped when it would clobber, instead of the whole
  operation being refused. `move-file` and the shape verbs keep their
  clean-tree requirement — they take no staged input, so their checkout is not
  a no-op.
- ~~Simplification: blob reading 3×; the diff parsed twice per file; test
  fixtures re-declared~~ **Done.** One `git::blob_at`; `patch::hunks` returns the
  display lines it was already computing, so `tui::diff_lines` and
  `FileEntry::lines` are gone and each file is parsed once; `App::picked()`
  replaces the count computed in three places; `recipe::parent_of` is public and
  reused; the shared test fixtures live in `tests/common`. ~130 lines of
  duplication deleted, ~40 net after the shared helpers.

## What we already do better than prior art

1. **Byte-identical abort.** No `.git/rebase-merge/`, no `--abort`.
   `git absorb --and-rebase` inherits rebase's mess.
2. **Compare-and-swap ref promotion** — refuses to clobber a branch that moved
   underneath you. No peer does this.
3. **Preview is literally execute minus the ref move** — it cannot disagree.
4. **Conflicts name the commit that owns the lines.** Nobody else does.
5. **Moving hunks out of one commit into another** — only jj matches it, and
   ours is hidden behind one undocumented keystroke.
6. **The TUI never touches the worktree**, so you can reorganise with WIP
   present; `rebase -i` outright refuses.
7. **Reorder / drop / squash in a TUI with a live preview and a byte-identical
   abort.** Sapling's ISL hands reordering to `histedit`; git-branchless has no
   TUI reorder. This one is genuinely unique (M4).
8. **Conflict resolution with no state to babysit.** `--ours`/`--theirs`/
   `--union` either finish the run or abort byte-clean; there is no half-applied
   sequencer to `--continue`, and no `.git/` directory to clean up afterwards.

These now open the README — they are the reasons to choose this tool. Keep them
true: each one is load-bearing, and each has a test behind it.
