# git-transplant — Roadmap

See [DESIGN.md](DESIGN.md) for the engine architecture. This is the build order.
Principle: prove the risky part first, ship value with the fewest deps, add the
TUI only where it is genuinely earned.

## Status — Phases 0–3 shipped + hardened (2026-07-21)

All operations work and are tested (**87 tests**: engine, ops, inference, patch,
TUI state + end-to-end). Commands:

- `fix <target>` — fold the staged change into a commit (op C); on conflict,
  inference names the commit that owns the lines (retarget hint).
- `move <path> <target>` — re-anchor a file, preserving mode/exec bit (op B).
- `absorb [--base <rev>]` — distribute staged hunks to their owning commits,
  git-absorb style (op D); no-home hunks stay staged; empties dropped.
- `tui` — one interactive screen for **all** operations. Hunks mode = fix
  (`f` routes all selected to the cursor commit) / absorb (`r` resets to
  inference) / manual per-hunk (`t`); `s` loads a commit's OWN hunks so you can
  move them into another commit; Move mode (`m`) = op B; `p` previews, `Enter`
  applies (two-step). Arrow-key based — no vim bindings.
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

## Backlog #9 — interactive conflict resolution (`--continue`): DROPPED

Decided against, not deferred. It asks for the one property this tool sells
against: the engine builds the whole rewritten stack as unreferenced objects and
moves the branch only on full success, so there is no `.git/rebase-merge/` and
nothing to `--abort`. `--continue` needs state persisted across process runs and
a user sitting *inside* a half-finished rewrite — rebase's failure mode, put back
into the tool whose pitch is that it has none.

Two independent reviews had already killed the naive design (resolve in the live
worktree, override per whole tree), and the reason is worth keeping: **the
worktree sits at the tip**, so "read the resolved worktree as commit C's tree"
splices tip-era content into an early commit. That is history corruption, not a
bug to fix later. A corrected design did exist — per-`(commit, path)` blob
overrides with the replay re-run from scratch, ~200 lines — and it is dropped on
value rather than cost: `--ours`/`--theirs`/`--union` resolve by a rule with
nothing on disk, a conflict already names the commit that owns those lines so the
usual answer is to *retarget* rather than resolve, and `--ignore-whitespace`
covers the reindent case. Revisit only if someone reports hitting that ceiling on
a real stack.

## Phase 0 — De-risk spike ✅ VALIDATED (since retired)

The spike (since deleted — every assertion now has a home in the test suites)
built a throwaway repo in a tempdir: a 3-commit stack, folding a staged fix into
the **root** commit by
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
| 0 ✅ | *(spike retired — its assertions now live in the suites below)* replay correctness, fold-into-root, revert strips fix, same-line conflict detected, **genuine adjacency → clean abort**, **whitespace-adjacent → merges with `ignore_whitespace`**, new tip oid |
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
