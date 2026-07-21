# git-transplant — Roadmap

See [DESIGN.md](DESIGN.md) for the engine architecture. This is the build order.
Principle: prove the risky part first, ship value with the fewest deps, add the
TUI only where it is genuinely earned.

## Status — Phases 0–3 shipped + hardened (2026-07-21)

All operations work and are tested (**46 tests**: engine, ops, inference, patch,
TUI state + end-to-end; plus `examples/spike.rs`). Commands:

- `fix <target>` — fold the staged change into a commit (op C); on conflict,
  inference names the commit that owns the lines (retarget hint).
- `move <path> <target>` — re-anchor a file, preserving mode/exec bit (op B).
- `absorb [--base <rev>]` — distribute staged hunks to their owning commits,
  git-absorb style (op D); no-home hunks stay staged; empties dropped.
- `tui` — one interactive screen for **all** operations: Hunks mode = fix
  (`a` = route all to cursor) / absorb (`A` = inference) / manual per-hunk (`t`);
  Move mode (`m`) = op B; `p` previews (dry-run replay), `Enter` applies.
- `--ignore-whitespace` global flag.

Hardened after **five** adversarial reviews across two rounds (engine / ops-safety
/ patch / test-gaps / new-code) and an expert TUI review:
nested-path synthetic build, filemode preservation, atomic ref-last promote,
non-tree descent guard, binary/non-UTF-8 skip, blame-absent safety, and a
**data-loss fix** — `absorb`/TUI force-checkout used to wipe un-folded (orphan /
deselected) hunks; they now move the ref only, leaving that work staged.

Backlog #10 (done): `--drop-empty`, orphan-hunk preservation, abandoned-branch
warning, arrow-key nav + testable TUI. Deferred: event-log undo, `--base` bound
for the TUI window, GPG re-signing, stash integration.

## Backlog #9 — interactive conflict resolution (`--continue`): DEFERRED, designed

The engine is a single in-memory pass with clean abort; interactive resolution
needs working-dir state across process runs, which is a poor fit and — done
badly in a history-rewriting tool — dangerous. The safe path already covers the
common case: conflict → clean abort + retarget hint, or use `absorb` (commutation
picks a target that doesn't conflict). Recommended design when built:

1. On conflict at commit `C`, write the conflicted merge to the worktree with
   markers and persist state under `.git/transplant/`: the serialized recipe,
   `base`/`tip`/`branch`/`ignore_ws`, and a `{commit → resolved-tree}` override
   map. Pin synthetic commits with a ref so they survive gc.
2. `transplant continue`: read the resolved worktree as `C`'s override tree, then
   **re-run replay from scratch** applying the override at `C` (`replay` gains an
   `overrides: &HashMap<Oid,Oid>` param). Re-running (cheap on small stacks)
   avoids persisting a partial chain. Conflict again later → save that override,
   repeat. `transplant abort` deletes the state — repo is already byte-identical.

Estimated ~200 lines + serialization; a focused follow-up, not a rushed add.

## Phase 0 — De-risk spike ✅ VALIDATED

`examples/spike.rs` (`cargo run --example spike`) — throwaway repo in a tempdir:
builds a 3-commit stack, folds a staged fix into the **root** commit by
cherry-picking a tip-parented synthetic onto it, replays the stack in memory, then
checks revert and conflict detection. All six assertions pass — the whole engine
(`cherrypick_commit` / `revert_commit` / `write_tree_to`) is buildable as designed.

Two gotchas it surfaced (fold into Phase 1):

- **Low-context spurious conflicts.** A tiny file with edits on *adjacent* lines and
  no separating context merges as a false conflict (git's line merger, matches
  git-absorb's non-commuting behavior). Real code with surrounding context merges
  clean. Not a bug to fix — a limitation to report honestly on abort.
- **`commit(Some("HEAD"), …)` enforces first-parent == current tip.** Build rewritten
  and synthetic commits detached (`update_ref = None`); only move the branch ref at
  the very end via `Reference::set_target` (with a reflog message).

`apply_to_tree` + `Diff::from_buffer` (Phase 2 hunk subsets) is still unproven — the
fallback there is `git apply` plumbing, and Phase 1 doesn't touch it.

## Phase 1 — Engine + `fix` (op C)

- `engine.rs`: `replay(repo, base, recipe, commit_ref)` (temp-ref, conflict-abort,
  reflog on success). Replay merges pass `MergeOptions { patience, ignore_whitespace }`
  (see DESIGN → Handling adjacent edits); expose `--ignore-whitespace`.
- `fix <target>`: recipe `{target: Add(staged)}`. Non-interactive — input is the
  staged diff.
- Integration tests (see Test matrix): happy fold + replay, atomicity on conflict,
  idempotent absorption, dirty-tree abort, root-target edge, merge-in-range reject.

## Phase 2 — `move` (op B)

- `move <path> <target>`: recipe `{intro..target^: Sub(file), target: Add(file)}`,
  built with `TreeBuilder`. No merge, no patch text.
- Non-interactive.

## Phase 3 — TUI + hunk-level A/D

The only place a full-screen UI is earned: marking hunks blind through a text
multiselect defeats a precision hunk-surgery tool — you must see the diff while
selecting.

- `patch.rs`: `Diff::from_buffer` / reverse / hunk-subset filtering to build partial
  synthetic trees for sub-commit selection.
- `tui/` (ratatui, immediate-mode): commit list | diff+hunk selector | preview pane.
  Preview pane = engine dry-run (`commit_ref = false`), so it can't disagree with
  the result.
- `collapse` (A) and `fix --gather` (D) wired to the selector.

## Dependency trajectory

| phase | deps | notes |
|---|---|---|
| 1–2 | `git2`, `clap`, `anyhow` | C and B are arg-driven; no interaction needed |
| 3 | `+ ratatui` (brings crossterm 0.28), **drop `inquire`** | never run two terminal backends |

`inquire` (crossterm 0.25) is currently **unused speculative weight**. Keep it only
if a `--pick` target-browser is wanted before Phase 3; otherwise drop it. When
ratatui lands it also renders the trivial list/select screens, so inquire retires.

## Inspiration digest — git-absorb & git-branchless

**git-absorb** (`hg absorb` port; Rust + libgit2) — automatic fixup absorption.

- **Adopt: commutation for target inference.** For each hunk, test if it commutes
  with the tip commit, then the next, … The first commit it does *not* commute with
  is the target. Resolves the old "related lines" question cleanly (see DESIGN →
  Target selection). Different hunks → different commits = op D for free.
- **Adopt: bounded stack window.** `--base <ref>`, default last ~10 commits. Safety
  + speed bound on the search.
- **Adopt: honest fallback.** Hunk commutes with everything → no home; leave it in
  the working tree and warn. Don't guess.
- **Scoping call:** the *fully automatic* path is already solved by
  `git absorb --and-rebase`. Our differentiated value is **interactive, precise**
  hunk→commit surgery, **explicit target override**, and **forward moves** (op B) —
  none of which absorb does. Best form: inference **pre-fills** the TUI, user
  confirms/overrides. Don't reimplement absorb's auto path for its own sake.

**git-branchless** (Rust + libgit2) — patch-stack suite.

- **Validates the core:** its rebase is **in-memory, avoids the working copy** —
  exactly our tree-based replay. Called the fastest rebase impl for this reason.
  Ref: Waleed Khan, *Lightning-fast rebases with git-move*.
- **Validates preview:** "speculative merges" = pre-compute conflicts to warn early
  = our dry-run-is-preview. Keep it.
- **Consider later: event log + `git undo`** — undoes commits, amends, rebases,
  checkouts, branch moves. Richer than our reflog + temp-ref. Deferred nicety; our
  two undo paths cover MVP.
- **New deferred concern: abandoned descendants / restack.** Rewriting a commit
  strands any *other* branch/commit pointing into the range. We move one branch ref;
  if several point into the rewritten range they'd need restacking. MVP = single
  linear branch; detect and warn if others point in.
- **YAGNI:** segmented-changelog DAG, sparse indexes, multithreading — huge-monorepo
  perf. Personal stacks are tiny. Explicitly skip.

## Test matrix

Each phase ships its own checks; don't write them ahead of the code (YAGNI).

| phase | tests |
|---|---|
| 0 ✅ | `examples/spike.rs`: replay correctness, fold-into-root, revert strips fix, same-line conflict detected, **genuine adjacency → clean abort**, **whitespace-adjacent → merges with `ignore_whitespace`**, new tip oid |
| 1 | happy fold into a mid-stack target + replay; **atomicity** (on conflict, branch oid *and* reflog unchanged, repo byte-identical); **idempotent absorption** (fold a change a newer commit also has → that commit empties, no double-apply); dirty-tree abort; root-commit target; merge-in-range rejected |
| 2 | file removed from ancestors + appears at target; forward-move blocked when an intermediate commit modifies the file (genuine conflict) |
| 3 | **commutation inference** (fix that commutes past N commits lands at the right one); **multi-hunk distribution** (two regions → two target commits, op D); `Diff::from_buffer` hunk-subset round-trip; **retarget hint** computed on a forced-early-target conflict |

## Open questions

- Explicit vs inferred as the *default* for `fix` — infer with `--target` override,
  or require `--target` and offer `--auto`? Lean: infer by default, override to force.
- Default merge algorithm — patience vs histogram; is `--ignore-whitespace` on or off
  by default? (Whitespace-adjacent mitigation validated; default TBD.)
- `--drop-empty` default per op (on for A/D, off for C) — confirm against real use.
- Multi-target `fix` (hunks land in different commits) — CLI output format when it's
  not a single target.

## Deferred (named, not lost)

`patch.rs` + ratatui until Phase 3 · conflict *resolution* (abort-only for now) ·
merge commits in range (rejected) · stash integration for dirty worktrees ·
threaded dry-run for large stacks · GPG re-signing.
