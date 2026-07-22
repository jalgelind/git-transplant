# git-transplant — Design

A CLI (+ TUI) for moving changes around inside a stack of commits: fold a change
into an *earlier* commit and keep the rest of the stack replayed and consistent —
without the `git commit --fixup` + `rebase -i --autosquash` conflict dance.

## Motivation

You work on a feature as a **stack of commits** (a PR chain, or a branch you keep
logically clean). At the tip you make a change that really belongs to an earlier
commit `$target`. The correct move is to fold it into `$target` so every commit
stays self-consistent (bisectable, reviewable). Today that means a fixup commit +
interactive rebase, which drops you into conflict hell with no in-memory safety.

## The four operations (from the original note)

| | Op | Meaning |
|---|---|---|
| A | collapse | Move selected hunks (and "related lines" scattered across commits) into `$target` |
| B | move file | Whole-file A: file appears *at* `$target` — removed from its ancestors if it was introduced earlier, planted at `$target` if it was introduced later |
| C | fix | Fold a staged change / fixup commit into `$target`; abort if replay conflicts |
| D | fix+gather | C, plus vacuum related hunks out of the in-between commits into `$target` |

All four are the same primitive: **fold a change into `$target`, replay the rest.**
A/B/D additionally *extract* the change from where it currently lives.

The **shape** operations (drop / reorder / squash / split, added later) are the
same engine with a different *commit list*: because each commit is replayed
against its own original parent tree, the walk is order-agnostic, so a permuted,
shortened, or lengthened `Vec<Oid>` is the whole mechanism.

## Insight 1 — direction decides difficulty

- **Backward** (change lives *newer* than `$target`, e.g. working tree → older
  commit): fold into `$target`, replay newer commits. The 3-way merge is
  **idempotent** — a newer source commit that already contains the identical
  change becomes a no-op when replayed onto a chain that now has it. The change
  *evaporates* from the source and *appears* at target automatically. Easy, clean.
  Covers C, and A/D when sources are newer than target.
- **Forward** (change lives *older* than `$target`): you must *subtract* it from an
  ancestor whose descendants were authored assuming it exists. Conflict-prone by
  nature — often *correctly* (a line can't move forward past a commit that edits
  it). Covers B, and A/D when sources are older. Same machinery, more aborts.

## Insight 2 — one recipe-replay engine covers all four

```text
replay(repo, base, recipe):            # recipe: Oid -> [Add|Sub] edits
  new_parent = base
  for Ci in base..HEAD (oldest-first; reject merge commits):
      idx = cherrypick_commit(Ci, new_parent, 0)      # replay Ci onto rewritten parent
      if idx.has_conflicts(): abort
      tree = idx.write_tree_to(repo)
      Ci' = commit(tree, parent=new_parent, meta=Ci)  # author preserved
      for edit in recipe[Ci]:                          # inject the move here
          Ci' = apply_edit(Ci', edit)                  # may conflict -> abort
      new_parent = Ci'
  return new_parent      # dangling objects only; branch not moved yet
```

- **C** = `{target: Add(staged)}`
- **B** = `{intro..target^: Sub(file), target: Add(file)}`
- **A/D** = `{source: Sub(H), target: Add(H)}`

The recipe map is the *correct minimum*, not speculative abstraction: B cannot be
expressed without per-commit subtract-here / add-there. C is a one-entry recipe.
Build the engine once; light up subcommands one at a time.

## Insight 3 — no hand-rolled patch text in Phase 1

Represent each edit as a **synthetic whole-object change** and reuse git2's own
merge plumbing instead of serializing unified diffs:

| edit | git2 call | content source |
|---|---|---|
| Add whole staged change | `cherrypick_commit(synthetic)` | `Index::write_tree()` = "HEAD+staged" tree |
| Subtract whole change | `revert_commit(synthetic, tip)` | applies the inverse, with conflict detection |
| Add/remove whole file (B) | `TreeBuilder.insert/remove` | direct tree edit, no merge |

`apply_to_tree` + `Diff::from_buffer` — the fiddliest, riskiest code — is needed
**only** for sub-commit hunk *subsets* (Phase 2, the TUI selector). Phase 1 (C, B,
whole-commit A/D) rests entirely on `cherrypick_commit` / `revert_commit` /
`TreeBuilder`, all well-trodden. `patch.rs` doesn't exist until the TUI needs it.

## Target selection — explicit or inferred (commutation)

Who decides `$target`? Two modes, same engine:

- **Explicit**: the user names it (`fix <target>`). Full control; needed when you
  disagree with inference or want a *forward* move.
- **Inferred** (git-absorb's insight): for each hunk, test whether it **commutes**
  with the tip commit, then the next, and so on. The first commit it does *not*
  commute with is the one that last touched those lines — that hunk's target. A hunk
  that commutes with every commit in range has no home → leave it in the working
  tree and warn. Different hunks resolve to different commits, so inference does op
  D ("take related changes") for free.

Inference *builds the recipe*; the replay engine *executes* it. The TUI's best form
is inference-assisted: pre-fill each hunk's target from commutation, let the user
confirm or override before replay. Bound the search to a stack window (`--base`,
default last N commits) for safety and speed.

## Handling adjacent edits

Conflicts on the same line — or a line another hunk uses as context — are the one
hard limit: no line-based 3-way merge resolves them, and neither can we. We make
them *rare* and *honest* instead. In order of leverage:

1. **Pick the right target (commutation).** An adjacency conflict is the *signal*
   that the target is earlier than where the change belongs. Commutation stops at
   exactly the commit that owns those lines, so inferred targets almost never hit
   it. Force a fix into an earlier commit → conflict; let commutation pick → clean.
2. **Tuned merge options for spurious cases.** Use patience/histogram diff and
   expose `--ignore-whitespace`. Proven in `tests/gaps.rs`: a reindent on the
   fix's line conflicts by default and merges clean with `ignore_whitespace`.
   Whitespace churn adjacent to a fix is the most common spurious conflict.
3. **Per-hunk granularity (Phase 2).** Applying hunk-by-hunk shrinks the blast
   radius so an unrelated change two lines away doesn't drag the fix into a conflict.
4. **Genuine conflict → clean abort + retarget hint.** On a real same-line clash,
   abort byte-clean (nothing was ever referenced) and report the commutation target:
   "commit X between <target> and HEAD also edits these lines — fold there, or rerun
   with `--interactive`."
5. **Interactive resolution** — deferred escape hatch for when the user insists on
   the earlier target and must resolve by hand.

Engine impact: replay merges pass `MergeOptions { patience, ignore_whitespace }`;
conflict reporting computes the commutation target for the hint.

## Atomicity, preview, undo — one mechanism

- The engine builds the whole new chain as **dangling objects** — no ref, no temp
  ref, nothing named. The branch moves **only on full success**, via a
  compare-and-swap against the tip we started from (so a branch that moved
  underneath us is never force-overwritten), with a reflog message. On any
  conflict nothing was ever referenced: the repo is **byte-identical**, with no
  `rebase --abort` state to clean up.
- **Preview == execute minus the ref-move.** Dry-run is the engine with
  the ref move — the same `replay` call, with the returned oid discarded. Same
  code path, so the preview can never disagree with the result.
- Two independent undo paths: the reflog entry, and the fact nothing moved on
  failure.
- Worktree handling depends on whether anything is left over. `fix`/`move`, and
  `absorb` when every hunk found a home, force-checkout the new tip (a clean
  tree). `absorb` with orphan hunks — and the whole TUI — move the ref *only*,
  because a checkout would wipe the un-folded work still sitting in the worktree.

## Edge-case ledger

| case | decision |
|---|---|
| Merge commit in range | an explicit range containing one is rejected; the window the TUI/`absorb` offer stops at the first merge instead, so a merge deeper in history doesn't block the linear stack above it |
| Replayed commit empties (A/D fully collapsed) | `--drop-empty`; default on for A/D, off for C |
| GPG signatures | silently dropped on rewrite (not yet warned — see ROADMAP-NEXT) |
| Author / committer | author preserved fully; committer identity preserved (stable oids on no-op, stable stack order) |
| Root commit in range | apply recipe to the empty tree |
| Dirty worktree | `fix`/`absorb` keep it (they fold the index; the checkout is skipped, never forced over it); `move-file` and the shape verbs abort; `reword` doesn't care |
| Branch not checked out / detached HEAD | move ref only, no worktree update |
| Low-context merge (adjacent edits, tiny file) | git's line merger reports a spurious conflict → abort with an honest message; real code with surrounding context merges clean (validated in `tests/gaps.rs`) |

## Module layout

```text
src/
  main.rs      clap dispatch -> fix | move-file | absorb | drop | reorder
                                | squash | split | reword | tui | undo
  engine.rs    replay(repo, base, tip, recipe, merge, drop_empty) -> Replay
               replay_order(…, order: &[Oid], …)  same walk, EXPLICIT order
  recipe.rs    build a recipe for each op from git state; `Shaped` plans carry
               an explicit commit order (drop/reorder/squash/split)
  git.rs       resolve rev, linear-range check, commit-with-meta, blob read,
               `Merge` (ignore-whitespace + --ours/--theirs/--union favor)
  patch.rs     Hunk parsing, apply_selected, synthetic_for_hunks
  inference.rs commutation/blame target inference
  ops.rs       fix / mv / collapse(absorb) + promote (ref move, compare-and-swap)
  tui.rs       ratatui app: commit list | commit-diff or hunk selector | status
```

Selection (recipe-building) never touches execution (replay). Both front-ends —
non-interactive CLI and the TUI — produce a recipe and hand it to one engine.
