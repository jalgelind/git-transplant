# git-transplant — Roadmap (next)

Forward-looking plan. For what's already shipped see [ROADMAP.md](ROADMAP.md)
(phases 0–3) and [DESIGN.md](DESIGN.md) (engine architecture).

## Where we are

All operations work, hardened across six adversarial reviews + an expert TUI
review. **86 tests** (unit + integration + TUI state + TUI rendering), clippy
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

## 3. TUI UX overhaul (from a full workflow review) — ✅ SHIPPED (#11–#19)

All nine items below are implemented and locked in with `TestBackend` render
regressions. Two refinements beyond the review: a **persistent context line**
(always naming the hunk under the cursor, so keys never act on hidden state —
better than blocking them), and a **full-width status bar** (rendering revealed
the keymap was clipped inside the right column, losing `p preview · Enter apply ·
q quit` entirely — a bug the review's own renders hadn't isolated).


An expert UX pass walked every workflow against real `TestBackend` renders at
100×30 and 80×24. Verdict: the engine model is sound and the panes are clean, but
**the TUI leaks its state machine to the user** — the primary action is
unadvertised, the "where does this land" marker clips off-screen, keys act on
hidden state, and `Enter` rewrites history with no confirmation. Grouped fixes
below (tracked as tasks #11–#19); each lands with a `TestBackend` render
assertion, since that harness already exists.

### 3a. Orientation — tell the user what this screen does  (#11, #12)
- **Launch status is a stale second keymap.** `load` seeds `status` with
  `"j/k move · Tab pane · Space select · …"`, rendered *below* the real keymap —
  two keymaps, different wording, and the feedback slot shows no feedback.
  Replace with the value-prop: `"Enter: absorb all staged hunks (inferred
  targets) · t: retarget · p: preview"`. Add `· Enter: absorb` to the `[HUNKS]`
  title.
- **`◀` target marker clips off-screen.** It's appended *after* the summary in a
  ~26-col pane, so real summaries truncate it away — the one cue for where a hunk
  lands is invisible. Move it to a fixed left gutter beside the `▶` cursor. Show
  hunk targets as `→ 5ccb1777 add scaffolding…` (oid + summary), plus a
  `◀ target · ▶ cursor` legend.

### 3b. Don't act on what isn't visible  (#13, #14)
- **Selection/targeting keys ignore focus.** `Space`/`a`/`A`/`t` check `mode` but
  not `focus`; with the default commit-focus they mutate a hunk the user cannot
  see ("hunk → d9516b09" — *which* hunk?). Either auto-focus the hunk pane on
  those keys or require it focused with a status hint.
- **`Enter` is destructive with no confirmation**, from any pane. Gate it:
  first `Enter` reports `rewrite N commits, master → d951…  (Enter again to
  apply · p to preview)`; second applies.

### 3c. Layout that survives a real terminal  (#15)
- The `Length(4)` status area overflows at ≤80 cols (keymap wraps to 2 lines,
  status to 2) and **silently drops the `sel/total hunks` count** — the most
  useful state indicator is the first casualty. Give it room, elide the keymap to
  one line, or move counts into a pane title.

### 3d. Mode honesty  (#16)
- **Move mode + staged changes is a guaranteed dead end**, surfaced only on
  `Enter` (`require_fully_clean` rejects staged too) — and you almost always open
  this tool *because* you have staged changes. Cue it up front in the title.
- The keymap doesn't adapt: Move mode still advertises `Space sel · a all→cur ·
  A infer`, all no-ops there. Also `"1 tracked files"`.

### 3e. Content fidelity  (#17, #18)
- **Staged *new*/deleted files vanish** — `load` filters `Delta::Modified`, so a
  repo with only `brand_new.rs` staged renders "no staged changes": misleading.
  Surface them as informational no-home rows or a status note.
- **Commit-diff header renders as one run-on line** (`diff --git a/f.rs
  b/f.rsindex a52…--- …+++ b`) — git's multi-line file header is kept as a single
  entry; split on `\n`. Long diffs also clip with no scroll.

### 3f. Wording pass  (#19)
Short branch name (`master`, not `refs/heads/master`) in preview/apply status;
unify the no-op wording between preview and apply.

### Still open (not UX-review derived)
- **`--base` bound for the TUI window** — `load` walks full history; add a
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
