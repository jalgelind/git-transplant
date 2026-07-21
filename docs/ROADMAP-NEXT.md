# git-transplant — Roadmap (next)

Forward-looking plan. Shipped history lives in [ROADMAP.md](ROADMAP.md); the
engine architecture in [DESIGN.md](DESIGN.md). Everything here is **open work**,
derived from a workflow investigation and a five-reviewer codebase audit.

## Where we are

All four operations work and are hardened: **88 tests**, clippy clean. Commands
`fix`, `move`, `absorb`, `tui`, plus `--ignore-whitespace`. The engine is an
in-memory replay producing dangling objects, promoted by a compare-and-swap ref
move with a reflog entry — so a failed run leaves the repo byte-identical and a
branch that moved underneath you is never clobbered.

## The core finding

**Coverage is excellent on one axis and absent on the other.** The tool owns
*"this change belongs to an older commit"* — `fix`/`absorb`/the TUI cover it
better than any peer, and moving hunks *out of* an existing commit is something
`git absorb` cannot do at all. But every operation that changes the **shape** of
the stack — reorder, drop, squash, split, reword — has no path. Users drop to
`git rebase -i`, **and once they are there they will do the fixup there too**.
The missing half cannibalises the half that works.

The gap is smaller than it looks: `replay_opts` merges each commit against *its
own original parent tree*, so the loop is **already an order-agnostic
cherry-pick**. Reorder / drop / squash are a permuted-or-shortened commit vector
— a new plan-builder, not new machinery.

## Adoption blockers (fix before anything else)

1. **No README.** Nothing explains the model to a first-time user.
2. **Rewriting a stack strands sibling branch refs** — we only *warn*. That
   breaks every ghstack / spr / Graphite user. `abandoned_warnings` already
   detects them and `replay` already computes the old→new mapping and discards
   it, so restacking is mostly plumbing we already have.

## Tier 1 — high value, cheap given the engine

| # | Item | Note |
|---|---|---|
| T1 | `undo` + always print the old tip | Undo exists only via reflog and is never surfaced. Reflog is sufficient here (unlike git-branchless) because we only ever move one existing branch. ~40 lines |
| T2 | `--dry-run` / `absorb -n` | Preview already exists internally (TUI `p` = replay minus promote). Every peer has it |
| T3 | Restack sibling refs instead of warning | See blocker 2 |
| T4 | `reword <rev> -m` | `recommit` already takes the original for metadata; add a message-override map. ~15 lines |
| T5 | README + naming + help text | Rename `move` → `move-file` (in git-branchless, `git move` means *move a subtree of commits* — actively confusing). Alias `fix` → `fixup`. Drop the "op B/C/D" jargon. Print `main`, not `refs/heads/main`, in the CLI |
| T6 | Fix `move`'s misleading error | Verified: `move f4.txt HEAD~2` reports `path not found: f4.txt` while the file is plainly in the tree. `move` only re-anchors *forward*; say so, or support the backward case |

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

- `mv` replays with `drop_empty` off, so re-anchoring a file whose intro commit
  held nothing else leaves a commit with an **empty tree**.
- `drop_empty` deletes commits with **no report** — `absorb` never says how many
  it removed. The TUI warns, but `empties_source()` is wrong in both directions
  (ignores binaries skipped at load; can promise "DROPPED" for a survivor).
- `abandoned_warnings` misses `refs/stash`, checks only ref *tips* (not
  descendants), turns an error into "all clear", and ignores branches checked out
  in another linked worktree.
- `promote(sync=true)` checks out *before* the ref move, so a failed `reference()`
  leaves worktree ≠ HEAD with the new tip dangling and unnamed in the error.
- `replay_opts` returns the original `tip` when `base=None` and every commit
  drops — degenerate, but an inconsistent contract.
- **CLI/TUI inconsistency**: `fix`/`absorb` hard-fail on unrelated *unstaged*
  churn; the TUI (correctly) does not, since it never writes the worktree.
- Simplification: blob reading implemented 3×; the diff is parsed **twice per
  file** (`patch::hunks` and `tui::diff_lines` run identical `Patch::from_buffers`
  on the same blobs); test fixtures re-declared across three files. ~-90 lines.

## What we already do better — and advertise nowhere

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

These belong in the README (T5) — they are the reasons to choose this tool.
