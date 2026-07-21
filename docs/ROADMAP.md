# git-transplant — Roadmap

See [DESIGN.md](DESIGN.md) for the engine architecture. This is the build order.
Principle: prove the risky part first, ship value with the fewest deps, add the
TUI only where it is genuinely earned.

## Phase 0 — De-risk spike (do before committing to the plan)

~40 lines, throwaway repo in a tempdir:

- Build a 3-commit stack.
- Fold a staged change into commit 1 via `cherrypick_commit` + `revert_commit` +
  `Index::write_tree_to`.
- Assert: commit 1's diff now carries the change; branch tip moved; the conflict
  case leaves the ref oid **unchanged**.

Goal: confirm `cherrypick_commit` / `revert_commit` round-trip cleanly through
`write_tree_to`. If they do, the whole engine is downhill. If `apply_to_tree` +
`Diff::from_buffer` fights us later (Phase 2), the fallback is `git apply`
plumbing — but Phase 1 doesn't touch that path.

## Phase 1 — Engine + `fix` (op C)

- `engine.rs`: `replay(repo, base, recipe, commit_ref)` (temp-ref, conflict-abort,
  reflog on success).
- `fix <target>`: recipe `{target: Add(staged)}`. Non-interactive — input is the
  staged diff.
- **One integration test is the coverage**: temp repo, run `fix`, assert target's
  diff changed, tip moved, worktree clean, conflict case aborts with repo unchanged.

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

## Open questions

- "Related lines" (A/D): how to detect which hunks in other commits are *related* to
  the selected change? Candidates: same file+line region, blame-of-touched-lines.
  → **git-absorb solves exactly this by blame; study it (see below).**
- Undo UX beyond reflog: is an event-log worth it? → **see git-branchless.**
- `--drop-empty` default per op (on for A/D, off for C) — confirm against real use.

## Inspiration to mine (next)

- **git-absorb** — infers `$target` automatically from blame instead of making the
  user name it. Directly informs op C/D and the "related lines" problem. Rust,
  libgit2.
- **git-branchless** — in-memory rebase engine, `git move`, `smartlog` TUI, and the
  undo/event-log. Directly informs the engine, the commit-list UI, and undo. Rust,
  libgit2.

## Deferred (named, not lost)

`patch.rs` + ratatui until Phase 3 · conflict *resolution* (abort-only for now) ·
merge commits in range (rejected) · stash integration for dirty worktrees ·
threaded dry-run for large stacks · GPG re-signing.
