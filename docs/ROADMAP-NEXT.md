# git-transplant — Roadmap (next)

Forward-looking plan. Shipped history lives in [ROADMAP.md](ROADMAP.md); the
engine architecture in [DESIGN.md](DESIGN.md). Everything here is **open work**,
derived from a workflow investigation and a five-reviewer codebase audit.

## Where we are

All four operations work and are hardened: **110 tests**, clippy clean. Commands
`fix`, `move-file`, `absorb`, `tui`, `undo`, plus `--ignore-whitespace`,
`--dry-run` and `--no-restack`. A README now exists, written by running the
binary and quoting its real output. The engine is an in-memory replay producing dangling objects,
promoted by a compare-and-swap ref move with a reflog entry — so a failed run
leaves the repo byte-identical and a branch that moved underneath you is never
clobbered. Those guarantees are now *visible*: every run prints the tip it came
from, `--dry-run` shows the outcome first, and `undo` walks it back.

## The core finding

**Coverage is excellent on one axis and absent on the other.** The tool owns
*"this change belongs to an older commit"* — `fix`/`absorb`/the TUI cover it
better than any peer, and moving hunks *out of* an existing commit is something
`git absorb` cannot do at all. But every operation that changes the **shape** of
the stack — reorder, drop, squash, split, reword — has no path. Users drop to
`git rebase -i`, **and once they are there they will do the fixup there too**.
The missing half cannibalises the half that works.

The gap is smaller than it looks: `replay` merges each commit against *its
own original parent tree*, so the loop is **already an order-agnostic
cherry-pick**. Reorder / drop / squash are a permuted-or-shortened commit vector
— a new plan-builder, not new machinery.

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

**M4 — The strategic bet** (weeks) ← *next*. `reorder`/`drop`/`squash` (T7) then `split`
(T8). This closes the shape gap that currently sends users to `rebase -i`. It is
also the only genuinely novel territory: reorder with live preview and
byte-identical abort exists nowhere.

**Ongoing / opportunistic.** `reword` (T4) is ~15 lines and can ride along with
any milestone. `--ours/--theirs` (T9), `--base` (T10) and the correctness backlog
(#30) are independent.

## Tier 1 — high value, cheap given the engine

| # | Item | Note |
|---|---|---|
| T1 | ~~`undo` + always print the old tip~~ ✅ | Done in M2 |
| T2 | ~~`--dry-run` / `absorb -n`~~ ✅ | Done in M2 |
| T3 | ~~Restack sibling refs instead of warning~~ ✅ | Done in M3 |
| T4 | `reword <rev> -m` | `recommit` already takes the original for metadata; add a message-override map. ~15 lines |
| T5 | ~~README + naming & help text~~ ✅ | Done in M1 |
| T6 | ~~Fix `move`'s misleading error~~ ✅ | Done in M1 — the backward case is *supported*, not just reported |

## Tier 2 — real, moderate

| # | Item | Note |
|---|---|---|
| T7 | `reorder` / `drop` / `squash` | Mostly a plan-builder: let `replay` take an explicit `Vec<Oid>` instead of deriving it. Reorder = permute; drop = omit; squash = drop + `ApplyChange` at the parent + a message policy. **The opening**: Sapling's ISL punts reordering to `histedit`, git-branchless has no TUI reorder — reorder with live preview and byte-identical abort exists nowhere |
| T8 | `split` / insert a new commit | The selection UI already exists (`s`); only "create a commit at this position" is missing from the replay loop |
| T9 | `fix --ours/--theirs/--union` | `MergeOptions::file_favor`, no persisted state. Ship before anything interactive |
| T10 | `--base` bound | `git absorb` defaults to 10 commits, `hg absorb` caps at 50; we blame/replay to the root |

## Tier 3 — explicitly NOT doing

- **Event-log undo** (SQLite + hooks) — the reflog covers this single-branch model.
- **jj-style first-class conflicts** — different object model; the current
  byte-clean abort is already better than rebase's half-state.
- **Interactive `--continue`** — the design was killed twice, correctly. Tracked
  as backlog #9 in [ROADMAP.md](ROADMAP.md) with a corrected design if ever needed.
- **Anything remote** (PR creation, push, landing) — `gt submit` / `spr` /
  `ghstack` own that. We move *local* refs only, and that is the correctness story.
- **Merge-commit support** — the linear restriction is what keeps the engine simple.

## Correctness & cleanup backlog (from the audit)

Low severity, none urgent, all verified:

- `undo` walks exactly **one** step: because it records its own move as a
  `transplant:` entry, a second `undo` is a redo rather than a step further back.
  Walking a whole history of transplants would mean skipping entries whose
  `id_new` no longer matches — deliberately not built until someone wants it.

- `mv` replays with `drop_empty` off, so re-anchoring a file whose intro commit
  held nothing else leaves a commit with an **empty tree** (both directions; now
  documented in the README rather than fixed).
- `drop_empty` deletes commits with **no report** — `absorb` never says how many
  it removed. The TUI warns, but `empties_source()` is wrong in both directions
  (ignores binaries skipped at load; can promise "DROPPED" for a survivor).
- `restack` misses `refs/stash`, checks only ref *tips* (not descendants), and
  turns a `references()` error into one warning rather than a refusal. (The
  linked-worktree case *is* handled: those branches are refused, not moved.)
- Remote-tracking refs are ignored by design — `restack` moves local branches
  only, and pushing the restacked stack stays the user's (or `gt`/`spr`'s) call.
- `promote(sync=true)` checks out *before* the ref move, so a failed `reference()`
  leaves worktree ≠ HEAD with the new tip dangling and unnamed in the error.
- `replay` returns the original `tip` when `base=None` and every commit drops —
  degenerate, but an inconsistent contract. The same case is the one hole in the
  old→new map: such a commit has no rewritten parent to land a ref on, so a
  branch there is warned about rather than moved.
- **CLI/TUI inconsistency**: `fix`/`absorb` hard-fail on unrelated *unstaged*
  churn; the TUI (correctly) does not, since it never writes the worktree.
- Simplification: blob reading implemented 3×; the diff is parsed **twice per
  file** (`patch::hunks` and `tui::diff_lines` run identical `Patch::from_buffers`
  on the same blobs); test fixtures re-declared across three files. ~-90 lines.

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

These now open the README — they are the reasons to choose this tool. Keep them
true: each one is load-bearing, and each has a test behind it.
