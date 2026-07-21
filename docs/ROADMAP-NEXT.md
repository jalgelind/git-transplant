# git-transplant — Roadmap (next)

Forward-looking plan. For what's already shipped see [ROADMAP.md](ROADMAP.md)
(phases 0–3) and [DESIGN.md](DESIGN.md) (engine architecture).

## Where we are

All operations work, hardened across six adversarial reviews + an expert TUI
review. **50 tests** (unit + integration + TUI state + TUI rendering), clippy
clean. Commands: `fix`, `move`, `absorb`, `tui` (all-ops, commit-diff browsing),
`--ignore-whitespace`. Recent fixes: orphan-hunk data-loss, `drop_empty`
deliberately-empty guard, atomic ref-last promote, TUI arrow-nav + diff-follows-
focus.

Priorities below are ordered by value/effort.

## 1. TUI tests that drive the TUI  ← focus

The rendering approach is **proven** (`src/tui.rs` now has 3 `TestBackend` tests
that render the real `ui()` to an in-memory buffer and assert on displayed text).
Build it out into a full matrix. Boundary: pure state via `on_key`, rendering via
`TestBackend`, apply via `load()`→`on_key`→`execute` against a temp repo.

- **Rendering (TestBackend) — expand the 3 starters:**
  - commit-diff pane updates when the commit cursor moves (render, `Down`, render,
    assert a *different* commit's diff is shown).
  - hunk pane: selected/deselected checkbox `[x]`/`[ ]` reflects `Space`; per-hunk
    target label (`→ <oid>` / `(no home)`) renders; `▶` cursor + `◀` target marker.
  - status bar: `sel/total hunks`, mode string, and the keymap line are present.
  - empty states: no staged changes → `(empty commit)` / "no staged changes";
    move mode with a staged change → clean-tree hint.
- **Apply/integration (temp repo via the library):**
  - `fix`-style: route all selected to one commit (`a`) → `execute` → assert that
    commit carries the hunk, others don't.
  - `move` mode: pick file + dest (`t`) → `execute` → file re-anchored (mirror of
    `ops::mv`, thin).
  - **orphan preservation via the TUI:** deselect a hunk, apply, assert it stays
    staged in the worktree (the data-loss regression, exercised through the UI).
  - preview parity: `p` reports the same conflict `Enter` would, ref unmoved.
  - no-op: targets already hold the hunks → "no change", `applied == false`.
- **Harness helper:** a `drive(app, &[KeyCode])` that folds a key sequence through
  `on_key`, so multi-key scenarios read as scripts.

## 2. Conflict resolution

- **Lazy first — `fix --ours` / `--theirs` / `--union`** (~20 lines, no state):
  auto-resolve merge conflicts by a fixed `MergeOptions` file-favor rule. Covers a
  large share of real fixups; ship before anything interactive.
- **Interactive `--continue`** — the redesigned, scratch-area, per-`(commit,path)
  →blob` override approach (see [ROADMAP.md](ROADMAP.md) "Backlog #9"). ~200 lines
  + serialization + a keep-ref for gc-pinning + a staleness guard. Only after the
  lazy flags prove insufficient.

## 3. TUI polish (found during review)

- **Staged *new* files don't appear** — `load` filters `Delta::Modified`, so a
  staged added file shows no hunks. Decide: show as a whole-file "add" hunk
  (target = none, informational) or keep filtered with a status note.
- **Commit-diff scrolling** — long diffs clip (Paragraph, no scroll). Add a scroll
  offset driven by `j/k` when a `Diff` sub-focus is active, or PgUp/PgDn.
- **Move-mode UX** — surface "move needs a clean tree" up front when a staged
  change is present, instead of only on `Enter`.
- **`--base` bound for the TUI window** — today `load` walks full history; add a
  flag/param so huge repos don't blame/replay everything.

## 4. Low-priority correctness (noted, not urgent)

- `engine::replay_opts` all-drop returns `tip` (no-op) when `base=None` and every
  commit drops — degenerate, unreachable in real use; make the contract explicit.
- `abandoned_warnings` ignores remote-tracking refs — arguably should warn there
  too.
- `promote(sync=true)` checkouts before the ref move, so a `reference()` failure
  after checkout leaves worktree≠HEAD (rare, recoverable) — document or reorder.

## 5. Backlog niceties (deferred)

Event-log undo (git-branchless style; reflog covers MVP), GPG re-signing on
rewrite, stash integration for dirty trees, threaded dry-run for large stacks,
abandoned-descendant *restack* (beyond the current warning).
