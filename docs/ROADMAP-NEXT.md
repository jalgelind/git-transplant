# git-transplant — Roadmap (next)

Forward-looking plan. Shipped history lives in [ROADMAP.md](ROADMAP.md); the
engine architecture in [DESIGN.md](DESIGN.md). Everything here is **open work**,
derived from a workflow investigation and a five-reviewer codebase audit.

## Where we are

Both halves now work and are hardened: **180 tests**, clippy clean. Commands
`fix`, `move-file`, `absorb`, `drop`, `reorder`, `squash`, `split`, `reword`,
`tui`, `undo` (`undo --list`), plus `--ignore-whitespace`, `--dry-run`, `--no-restack`,
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

~~**Not done in M4:** `split` is CLI-only and splits by **path**, not by
hunk.~~ **Done in M5** — and it was wiring, as predicted. `recipe::split_at`
takes a prebuilt synthetic, so splitting by path and splitting by hunk differ
only in how that commit's tree is built. In the TUI it needed **no new key**:
while a commit's hunks are open the commit list grows a phantom
`+ new commit here` row, and `t` on it routes the picked hunks there.

**M5 — the TUI catches up with the CLI.** The screen was designed for four
operations and the CLI had grown to eleven. Four slices: (1) `Mode` deleted —
`move-file` is a `Source` like the other two, one axis instead of two — the
keymap's second line scoped to the FOCUSED PANE (the structural fix for the
80-column clipping this screen shipped twice), `Constraint::Min(32)` on the
commit pane, `rebase -i`'s letters (`e`/`s`/`d`/`r`) removing the last shift
key, apply reloading in place instead of quitting, plus `u` undo and `c`
conflict-rule cycling. (2) An inline prompt on the status line — not `$EDITOR`,
not a popup — and `r` reword, which preserves the commit body. (3) Split by
hunk. (4) `i` ignore-whitespace. The phantom row is deliberately NOT a member of
`App::commits`: `commit_cursor` indexes that vector everywhere and three helpers
map an Oid back to a stack position, so a real row there would have shifted the
rewrite span, the move direction and the replay base at once.

**M6 — legibility.** Two reported problems with one shared cause: the screen
spent too many rows saying too little, in a colour half of all users cannot read.
(1) Chrome carries **no foreground colour** — it dims the terminal's own default.
`DarkGray` is ANSI bright-black, which on a black background is very nearly the
background (the reported "dark grey on black" help text) and only ever worked for
light terminals; DIM inherits, so it is legible on both, and a terminal that
ignores DIM falls back to full contrast — too loud, never invisible. (2) The
three permanent keymap lines became one, plus a `?` popup scoped to (focus,
source) — the same structural argument M5's line-2 scoping made, with room to say
what a key *means* rather than just naming it. That returns two of a small
terminal's twenty-four rows to the panes, and removes the widest thing on screen,
which is what kept clipping at 80 columns. The popup is transient (any key closes
it) and the closing key does nothing else, so dismissing help can never be the
keystroke that drops a commit. The phantom row gets its own help: every commit
verb refuses there, so offering them would be exactly the lie the scoping exists
to prevent.

**M7 — closing the backlog.** Six items, three of them argued closed rather than
built. Each is written up under "Correctness & cleanup backlog" below.

**GPG: warn, do not re-sign.** `git::recommit` cannot sign — git2 exposes no
signing API whatsoever — so every rewritten commit comes out unsigned, and until
now the tool said nothing. It now counts the signed commits in the range (off
`raw_header`, so `gpgsig-sha256` counts too, and at no extra odb cost) and
reports the loss: an `Outcome` warning on the CLI, and a `· N GPG signature(s)
will be LOST` clause in all three of the TUI's arming lines.

Re-signing was weighed and **declined**. It would mean shelling out to `gpg` or
`git commit-tree -S` once per rewritten commit: a hard external dependency, a
subprocess per commit across a stack of dozens, a passphrase prompt in the
middle of a rewrite that is supposed to be abortable byte-clean, and an error
path (gpg missing, key expired, agent locked) *after* the replay has succeeded.
`git rebase --exec 'git commit --amend --no-edit -S' <base>` already does it on
demand, and the warning names that command. The warning is the permanent answer
unless someone shows up who actually signs a stack every day.

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
| T8 | ~~`split`~~ ✅ | Done in M4 by path, in M5 by hunk. It needed *no* engine change either time: the split-off commit is a dangling synthetic in the replay order, and `recipe::split_at` just takes it prebuilt |
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
- **One-pass MIXED split in the TUI** (some of a commit's hunks into the phantom
  new commit, the rest to existing targets). Closed as WONTFIX in M7, on value
  rather than cost — and the cost estimate in the old marker was wrong, which is
  worth saying: `recipe::shaped` already takes an `edited` index for exactly the
  prefix-trim problem (squash uses it), so `split_at` would take `build_recipe`'s
  recipe plus the oldest touched index and the rest falls out. Perhaps fifteen
  lines.

  It is declined because the **outcome is already reachable in two applies**,
  each individually previewable and individually abortable, and those two applies
  produce the same commits — including dropping the source commit when both its
  hunks leave. That is now asserted end to end
  (`tui::what_a_mixed_selection_wanted_is_reachable_in_two_applies`, which
  compares the final tip tree byte for byte against the original). So the refusal
  costs a keystroke, not a capability, while a one-pass version widens the blast
  radius of the single riskiest operation here — a rewrite that half-applies.
  `t` routes one hunk at a time, so a mixed selection is usually a slip. Revisit
  if anyone reports actually hitting the refusal.

## Correctness & cleanup backlog (from the audit)

Low severity, none urgent, all verified:

- `undo` walks exactly **one** step: because it records its own move as a
  `transplant:` entry, a second `undo` is a redo rather than a step further back.
  Walking a whole history of transplants would mean skipping entries whose
  `id_new` no longer matches — **deliberately still not built.** That behaviour
  is coherent and tested; what was missing was only *visibility*, so M7 added
  `undo --list`: the branch's `transplant:` entries newest-first with their
  before/after oids, and a `*` on the one `undo` would take. `undo` now consumes
  the head of that same list, so the mark and the action cannot drift apart. The
  listing shows the `transplant: undo (…)` entry too, which is what makes the
  redo behaviour legible instead of surprising.

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
- ~~`restack` misses `refs/stash`, checks only ref *tips* (not descendants), and
  turns a `references()` error into one warning rather than a refusal.~~
  **All three closed in M7, and one of the three premises turned out to be
  false.**
  - **Ref scan moved before the promote.** `ops::sibling_refs` snapshots the
    candidate refs *first* and returns `Result`, so a refdb we cannot read
    refuses the whole operation with nothing moved, instead of reporting
    "nothing stranded" about something that was never checked. Probing found a
    second, worse case the review missed: an unreadable `refs/heads/` makes
    git2's `references()` **succeed and yield an empty list** — no error to
    propagate at all. The tell is that our own branch must appear in any honest
    listing; its absence is now the refusal.
  - **Descendant refs: warn, don't move.** A branch whose tip is outside the
    range but whose merge-base is inside it is stranded on orphaned history and
    used to be silent. Landing it means replaying *its* commits, which is a
    rebase and not a ref move — so it is named, with the exact
    `git rebase --onto <new> <fork> <branch>` that fixes it (verified by
    running it). Moving it would be a second, unrelated rewrite hiding inside
    the first one.
  - **`refs/stash` needs nothing — the premise was wrong.** A stash is applied
    as a 3-way merge of `stash^..stash` onto whatever HEAD is now, and
    `refs/stash` keeps its own base commit alive, so a stash over a rewritten
    commit is *not* un-appliable. Checked with `git stash apply` after a real
    `git rebase`, then pinned as a test that applies a stash across one of our
    own rewrites. Ignoring it is the correct behaviour, now for a stated reason
    rather than by omission.
- Remote-tracking refs are ignored by design — `restack` moves local branches
  only, and pushing the restacked stack stays the user's (or `gt`/`spr`'s) call.
- ~~`promote(sync=true)` leaves the new tip unnamed if the ref move fails~~
  **Named.** The error now carries it: "the worktree already holds <tip>;
  `git reset --hard <tip>` keeps it, `git checkout -f <branch>` discards it".
  The ordering itself is unchanged — checking out after the ref move trades this
  case for a worse one (ref moved, worktree stale) and neither is reachable
  without a concurrent writer.
- ~~Backward `move-file` resurrects a file deleted at the target and re-added
  later~~ **Refused in M7.** The heuristic ("first descendant carrying the path
  introduces it") is fine as long as the path does not exist *before* the target;
  when it does, some commit between there and the target deleted it, and planting
  it back both resurrects a file the history removes and falsifies the one thing
  `move-file` promises — that the file *first appears* at the target. The walk
  back to the root now refuses and names the deleting commit. Supporting it was
  rejected: there is no coherent thing "anchor this file at a commit that deletes
  it" could mean.

- ~~`replay` returns the original `tip` when `base=None` and every commit drops~~
  **Contract encoded in M7: it refuses.** Returning the original tip made every
  caller print "no change" for a *total collapse* — the worst possible summary of
  what happened. There is no branch with zero commits to collapse to, so with no
  `base` to land on the whole replay is an error. (With a `base` it is fine and
  unchanged: the branch lands on the base.)

  The map hole is *narrower* than the note said, and what is left of it is
  correct rather than deferred: the problem is a drop at the very START of a
  `base=None` range, not the whole range dropping. Such a commit's rewritten tree
  is the EMPTY tree, and no commit in the rewritten stack has one — so there is
  genuinely no counterpart to name, and mapping the ref anywhere else would send
  it to different content. Both halves are now pinned by tests.
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
5. **Moving hunks out of one commit into another** — only jj matches it. And
   splitting a commit *by hunk* from a TUI, through a phantom destination row
   rather than a mode.
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
