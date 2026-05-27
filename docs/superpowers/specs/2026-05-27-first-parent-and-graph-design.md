# First-parent filtering & commit graph improvements

**Date:** 2026-05-27  
**Status:** Approved, pending implementation  
**Repo:** gajdusek/gitui (fork of gitui-org/gitui)

---

## 1. Goals

Give users a way to view a simplified commit history focused on the mainline:

- **Phase 1** — toggle `--first-parent` mode: show only commits on the
  first-parent spine (merge commits + direct commits on main; branch
  commits hidden entirely).
- **Phase 2** — per-merge fold/collapse: hide branch commits behind
  `[+N]` badges on merge commits, expandable individually or all at once.

These serve a colleague review workflow: quickly scan what was merged and
when, drill into specific branches on demand.

Foundation: pull in and rebase [PR #2890](https://github.com/gitui-org/gitui/pull/2890)
(commit graph visualization by philocalyst), plus a lane-coloring
improvement. Both layers live below our feature commits so they can be
rebased cleanly if the upstream PR updates.

---

## 2. Out of scope

- Octopus merges (3+ parents) — deferred indefinitely
- Curve routing / time-based lane alignment
- Matching tig aesthetics exactly

---

## 3. Branch topology

```
gitui-org/gitui:master          ← upstream (tracked remote)
       ↑ rebased onto
philocalyst/gitui:branching     ← PR #2890 (tracked remote, read-only)
       ↑ our commits stacked here
gajdusek/gitui:feature/graph-base     ← layers 0+1
gajdusek/gitui:feature/first-parent   ← phase 1 (on top of graph-base)
(future) feature/fold-collapse        ← phase 2
```

If PR #2890 merges upstream: drop the `philocalyst` base layer, rebase
`feature/graph-base` directly onto `gitui-org/gitui:master`.

If PR #2890 updates but hasn't merged: `git fetch philocalyst branching`,
rebase `feature/graph-base` onto the new tip. Our commits are logically
independent so rebases should be clean.

---

## 4. Layer 0 — Rebase PR #2890 onto master

**Work:** fetch `philocalyst:branching`, rebase onto current
`gitui-org/gitui:master`, resolve conflicts.

**What PR #2890 adds:**

- `asyncgit/src/graph/` — new crate module: `mod.rs` (types), `walker.rs`
  (lane-state engine), `buffer.rs` (delta-compressed lane state with
  checkpoints every 100 commits), `chunk.rs` (per-lane slot).
- `src/components/utils/graphrow.rs` — Unicode symbol renderer.
- `CommitList` and `revlog.rs` wired to `GraphWalker`; toggle key `g`.

**Core types:**

```rust
pub struct GraphRow {
    pub lane_count: usize,
    pub commit_lane: usize,
    pub is_merge: bool,
    pub lanes: Vec<Option<(ConnectionType, usize)>>, // (symbol, color_idx)
    pub merge_bridge: Option<(usize, usize)>,
    // ...
}
```

Colors are `lane_idx % 6` over `[Blue, Yellow, Magenta, Cyan, Green, Red]`.

**Acceptance:** `make check` passes on the rebased branch.

---

## 5. Layer 1 — Lane coloring improvement

**Problem:** `lane_idx % 6` means lane 0 and lane 6 share Blue even when
they're adjacent, making parallel branches visually indistinguishable.

**Fix: greedy graph coloring**

When a new lane opens, assign it the lowest-indexed color not used by any
currently-active neighboring lane (lanes at positions `i-1`, `i+1` in the
`current` buffer). Once assigned, a lane keeps its color until it closes.

```rust
fn assign_color(
    lane_idx: usize,
    current: &[Option<Chunk>],
    lane_colors: &HashMap<usize, usize>,
) -> usize {
    let neighbor_colors: HashSet<usize> = [-1i64, 1]
        .iter()
        .filter_map(|d| {
            let ni = lane_idx as i64 + d;
            (ni >= 0).then(|| current.get(ni as usize)?.as_ref()
                .and_then(|c| lane_colors.get(&(ni as usize)).copied()))
        })
        .flatten()
        .collect();
    (0..).find(|c| !neighbor_colors.contains(c)).unwrap_or(0)
}
```

`lane_colors: HashMap<usize, usize>` maintained in `GraphWalker` alongside
the lane buffer. Entries removed when a lane closes.

Git commit graphs are planar; 4 colors always suffice with 6 available.

The snippet above is illustrative; the exact iterator chain will be
adjusted to match the actual `Chunk` field names during implementation.

**Scope:** ~50 lines in `walker.rs` + `mod.rs`. No protocol change.  
**Acceptance:** adjacent lanes never share a color in repos with 2+ concurrent
branches; `make check` passes.

---

## 6. Phase 1 — First-parent toggle

### 6.1 Behaviour

Toggle between:
- **Full log** (default) — all ancestors, interleaved by timestamp.
- **First-parent mode** — only commits reachable by following each commit's
  first parent from HEAD. Merge commits remain; branch commits disappear.
  Graph renders as a single straight column (correct by construction).

Key: `P` (uppercase; `g` = graph toggle from PR #2890, `p` free).  
Status badge: `[⊢ first-parent]` in the log header when active, same style
as existing filter/search badges.

### 6.2 Why not a filter function

`SharedCommitFilterFn` hides commits post-dequeue but still traverses the
full graph — branch ancestors pile up in the walk heap. First-parent must
stop enqueueing non-first-parent commits at the source.

### 6.3 Touch points

**`asyncgit/src/sync/logwalker.rs` — `LogWalker` (git2 path)**

```rust
pub struct LogWalker<'a> {
    // existing fields ...
    first_parent: bool,
}

impl<'a> LogWalker<'a> {
    pub fn first_parent(mut self, v: bool) -> Self {
        self.first_parent = v;
        self
    }
}

// in visit():
let parents: Box<dyn Iterator<Item = _>> = if self.first_parent {
    Box::new(c.0.parents().take(1))
} else {
    Box::new(c.0.parents())
};
for p in parents { self.visit(p); }
```

**`LogWalkerWithoutFilter` (gix path)**

gix v0.78: check for `Parents::First` on the walk options. If available,
use it. Otherwise apply `.take(1)` on the gix commit's parent iterator in
the equivalent position. ~5 lines either way.

**`asyncgit/src/revlog.rs` — `AsyncLog` / `AsyncLogRequest`**

Add `first_parent: bool` to `AsyncLogRequest`. Thread into walker
construction. On toggle: call the existing abort/restart sequence (same
path as search filter toggle).

**`src/tabs/revlog.rs` — tab state + key binding**

```rust
pub struct Revlog {
    // existing fields ...
    first_parent_mode: bool,
}
```

- On `P` keypress: flip `first_parent_mode`, call `reload_log()`.
- Pass `first_parent_mode` to `AsyncLog` on every reload.
- Render badge in `draw_header()`.

### 6.4 Graph interaction

`GraphWalker` still runs in first-parent mode. With only one commit per
"row" on the spine, it naturally produces a single vertical lane. No
special-casing needed.

### 6.5 Acceptance

- `P` toggles between full and first-parent log; badge appears/disappears.
- First-parent output matches `git log --first-parent` commit list.
- Combine with search filter: both apply simultaneously.
- `make check` passes.

---

## 7. Phase 2 — Fold/collapse (future)

*Designed at high level; detailed spec written when Phase 1 ships.*

### 7.1 Per-merge fold

Press `Space` on a merge commit to fold/unfold its branch:

```
Unfolded:                       Folded:
* g7h8i9  Merge feat/x         * g7h8i9  Merge feat/x  [+2]
|\                              * j0k1l2  chore: bump
| * p6q7r8  feat: thing
| * s9t0u1  test: coverage
|/
* j0k1l2  chore: bump
```

**State:** `folded_merges: HashSet<CommitId>` in `CommitList`.

**Which commits are "inside" a merge:** commits reachable from the merge's
second parent before they rejoin the first-parent spine. Computed via a
small reachability walk using the `GraphWalker` buffer's checkpoint system.
Result cached per merge commit when first folded.

**Render:** `CommitList::draw()` skips rows in the folded set. `[+N]`
count pre-computed at fold time.

### 7.2 Collapse-all / expand-all toggle

Key: `Shift+P` (or similar — finalise during Phase 2 design).

- **Collapse all:** populate `folded_merges` with every merge commit ID
  currently loaded. Shows mainline + `[+N]` badges.
- **Expand all:** clear `folded_merges`.

Individual folds remain toggleable on top of either global state.

### 7.3 Relationship to Phase 1

Phase 1 and Phase 2 are independent toggles and can be active
simultaneously (though combining them is redundant — first-parent already
hides branch commits, fold badges wouldn't be reachable).

### 7.4 Dependency

Requires PR #2890 (graph data) to be present; the reachability walk uses
`mergers_map` and the lane buffer from `GraphWalker`.

---

## 8. Key binding summary

| Key | Action | Phase |
|-----|--------|-------|
| `g` | Toggle graph visualization | PR #2890 |
| `P` | Toggle first-parent mode | Phase 1 |
| `Space` (on merge) | Fold/unfold that merge's branch | Phase 2 |
| `Shift+P` (tbd) | Collapse all / expand all | Phase 2 |

Exact Phase 2 keys confirmed during that phase's design.

---

## 9. Testing strategy

**Layer 1 (coloring):** unit test in `asyncgit/src/graph/` — construct a
graph with 7+ concurrent lanes, assert no two adjacent lanes share a color.

**Phase 1 (first-parent):** use `git2-testing` helpers to build a repo with
a merge commit; assert `LogWalker::first_parent(true)` returns exactly the
commits that `git log --first-parent` would return. Test that branch commits
are absent.

**Phase 2 (fold):** unit test `folded_merges` logic — fold a merge commit,
assert the right commit IDs are excluded from the render list. Test
fold/unfold idempotency and collapse-all/expand-all.

**Integration:** `make check` must pass after each layer.

---

## 10. Implementation order

1. `feature/graph-base`: fetch + rebase PR #2890, then layer 1 coloring fix
2. `feature/first-parent`: phase 1 on top of `graph-base`
3. `feature/fold-collapse`: phase 2 on top of `first-parent` (future)
