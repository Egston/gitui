# First-parent filtering & commit graph improvements — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebase PR #2890 (commit graph) onto master, improve lane coloring, and add a first-parent toggle mode to the log view.

**Architecture:** Stack commits on top of `philocalyst/gitui:branching` so the layers can be rebased independently if the upstream PR updates. First-parent is implemented in the git2 walker (`LogWalker`) so no gix changes are needed. The gix fast-path is skipped when `first_parent=true` since both paths already share this routing logic.

**Tech Stack:** Rust, git2 (0.20), gix (0.78), ratatui, cargo nextest.

---

## Orientation

Read these files before starting:

- `asyncgit/src/sync/logwalker.rs` — `LogWalker` (git2) and `LogWalkerWithoutFilter` (gix)
- `asyncgit/src/revlog.rs` — `AsyncLog`, `fetch_helper`, `fetch_helper_with_filter`, `fetch_helper_without_filter`
- `src/tabs/revlog.rs` — `Revlog` struct, `update()`, `event()`, `draw()`
- `src/components/commitlist.rs` — `CommitList`, `draw()` title block (~line 790)
- `asyncgit/src/graph/walker.rs` — `GraphWalker`, lane/color assignments (**exists only after Task 1 lands**)
- `docs/superpowers/specs/2026-05-27-first-parent-and-graph-design.md` — the approved spec

Run `make check` before touching anything to establish a clean baseline.

---

## File map

| File | Layer | Change |
|---|---|---|
| `asyncgit/src/graph/walker.rs` | 1 | add `lane_colors: HashMap<usize, usize>` field; `assign_color()` fn |
| `asyncgit/src/sync/logwalker.rs` | Phase 1 | `LogWalker::first_parent` field + builder + `read()` change |
| `asyncgit/src/revlog.rs` | Phase 1 | `AsyncLog::first_parent` field + setter; route to git2 walker when true |
| `src/tabs/revlog.rs` | Phase 1 | `Revlog::first_parent_mode` + key `P` + toggle logic |
| `src/components/commitlist.rs` | Phase 1 | `first_parent_mode: bool` field; badge in title |

---

## Task 1: Set up remotes and create working branch

**Files:** git operations only (no source changes)

- [ ] **Step 1: Add remotes**

```bash
cd /home/petr/projects/cli/gitui
git remote add upstream https://github.com/gitui-org/gitui.git 2>/dev/null || echo "already exists"
git remote add philocalyst https://github.com/philocalyst/gitui.git 2>/dev/null || echo "already exists"
git fetch upstream master
git fetch philocalyst branching
```

- [ ] **Step 2: Create the graph-base branch from the PR tip**

```bash
git checkout -b feature/graph-base philocalyst/branching
```

- [ ] **Step 3: Verify what commits are on top of upstream master**

```bash
git log --oneline upstream/master..HEAD | head -20
```

Expected: you see ~35 commits from philocalyst's PR. If fewer, the fetch may not have gotten all commits — re-run `git fetch philocalyst branching`.

---

## Task 2: Rebase PR #2890 onto current upstream master

**Files:** git operations only

- [ ] **Step 1: Rebase**

```bash
git rebase upstream/master
```

If conflicts appear, resolve them file by file. The PR adds new files (`asyncgit/src/graph/`) and modifies `src/components/commitlist.rs` and `src/tabs/revlog.rs`. Conflicts are most likely in the modified files. For each conflict:
- Open the file, look at `<<<<`, `====`, `>>>>` markers
- Keep both sides if they're additive (e.g. new imports + new fields)
- After resolving: `git add <file>` then `git rebase --continue`

- [ ] **Step 2: Verify the tree builds**

```bash
make check
```

Expected: all five checks pass (`fmt clippy test sort deny`). If clippy fails, fix the warnings — the codebase treats lints as errors. If tests fail, examine the failures; they're likely in graph-specific tests the PR added.

- [ ] **Step 3: Commit note (no code change needed)**

The rebase itself creates no new commit. Verify you're on `feature/graph-base` and the tip is the last PR commit:

```bash
git log --oneline -5
git status
```

Expected: clean working tree, on `feature/graph-base`.

---

## Task 3: Lane coloring fix

**Files:**
- Modify: `asyncgit/src/graph/walker.rs`

`★ Insight ─────────────────────────────────────`
This uses greedy graph coloring. Git commit graphs are planar, so by the
four-color theorem, 4 colors always suffice. With 6 available, the greedy
algorithm (assign the lowest-index color unused by direct neighbors) never
fails. The key is that "neighbors" here means adjacent lanes in the current
lane vector, not topological adjacency in the commit graph.
`─────────────────────────────────────────────────`

- [ ] **Step 1: Locate where colors are currently assigned in `walker.rs`**

Search for `% 6` or `color` in `asyncgit/src/graph/walker.rs`. Find the line that assigns a color index to a lane (it will look like `lane_idx % 6` or similar). Note the surrounding function name — this is where your change goes.

- [ ] **Step 2: Write the failing test**

Add to the `#[cfg(test)]` block at the bottom of `asyncgit/src/graph/walker.rs`:

```rust
#[test]
fn test_no_adjacent_lanes_share_color() {
    // Build a history with enough concurrent branches to overflow 6 lanes
    // so that naive `idx % 6` would give two adjacent lanes the same color.
    // Use git2-testing to create the repo.
    use crate::sync::tests::repo_init_empty;
    use crate::sync::{commit, stage_add_file};
    use std::{fs::File, io::Write, path::Path};

    let (_td, repo) = repo_init_empty().unwrap();
    let root = repo.path().parent().unwrap();
    let repo_path: &crate::sync::RepoPath =
        &root.as_os_str().to_str().unwrap().into();

    // Create a linear chain first, then 7 branches off commit 1 all merging in.
    // For the coloring test we just need GraphWalker to produce rows and
    // assert the color invariant holds.
    // Simpler: unit-test assign_color() directly once it's extracted.
    // Stub: confirm the function exists.
    let _ = GraphWalker::assign_color(6, &[], &std::collections::HashMap::new());
}
```

Run:
```bash
cargo nextest run -p asyncgit test_no_adjacent_lanes_share_color
```
Expected: **FAIL** — `assign_color` does not exist yet.

- [ ] **Step 3: Add `lane_colors` field and `assign_color` to `GraphWalker`**

In `asyncgit/src/graph/walker.rs`:

**3a.** Add to imports at the top:
```rust
use std::collections::{HashMap, HashSet};
```
(only if not already imported)

**3b.** Add `lane_colors` to the `GraphWalker` struct:
```rust
pub struct GraphWalker {
    // ...existing fields...
    lane_colors: HashMap<usize, usize>,
}
```

**3c.** Initialize it in `GraphWalker::new()` (or wherever the struct is constructed):
```rust
lane_colors: HashMap::new(),
```

**3d.** Add the `assign_color` method (as a public associated function so the test can call it):
```rust
pub fn assign_color(
    lane_idx: usize,
    current_lane_indices: &[usize], // indices of currently active lanes
    lane_colors: &HashMap<usize, usize>,
) -> usize {
    // Collect colors of immediate neighbors (lane_idx-1 and lane_idx+1)
    let neighbor_colors: HashSet<usize> = current_lane_indices
        .iter()
        .filter(|&&i| {
            i == lane_idx.saturating_sub(1) || i == lane_idx + 1
        })
        .filter_map(|i| lane_colors.get(i).copied())
        .collect();
    // Return the smallest color index not used by any neighbor
    (0_usize..).find(|c| !neighbor_colors.contains(c)).unwrap_or(0)
}
```

**Note:** The exact signature may need adapting to how the lane buffer is structured in PR #2890's code. The invariant to enforce is: when a new lane opens, call `assign_color`, store the result in `self.lane_colors[lane_idx]`, and use it instead of `lane_idx % 6`. When a lane closes, remove its entry: `self.lane_colors.remove(&lane_idx)`.

Find the place in `compute_rows()` or wherever `% 6` is currently used. Replace with a lookup into `self.lane_colors`, inserting via `assign_color` for new lanes.

- [ ] **Step 4: Update the test to be meaningful**

Replace the stub test with a proper unit test of the invariant:

```rust
#[test]
fn test_no_adjacent_lanes_share_color() {
    use std::collections::HashMap;

    // Simulate 7 lanes opening one by one, each adjacent to the previous.
    let mut lane_colors: HashMap<usize, usize> = HashMap::new();
    let mut active_lanes: Vec<usize> = Vec::new();

    for lane_idx in 0..7 {
        active_lanes.push(lane_idx);
        let color = GraphWalker::assign_color(
            lane_idx,
            &active_lanes,
            &lane_colors,
        );
        lane_colors.insert(lane_idx, color);
    }

    // Assert: no two adjacent lanes share a color
    for i in 0..6 {
        let c_i = lane_colors[&i];
        let c_next = lane_colors[&(i + 1)];
        assert_ne!(
            c_i, c_next,
            "lanes {i} and {} share color {c_i}",
            i + 1
        );
    }
}
```

- [ ] **Step 5: Run the test**

```bash
cargo nextest run -p asyncgit test_no_adjacent_lanes_share_color
```

Expected: **PASS**.

- [ ] **Step 6: Run full check**

```bash
make check
```

Expected: all pass. Fix any clippy issues (unused imports, HashMap not used, etc.).

- [ ] **Step 7: Commit**

```bash
cd /home/petr/projects/cli/gitui
git add asyncgit/src/graph/walker.rs
git commit -m "fix: use greedy graph coloring for adjacent lane color assignment

Adjacent lanes previously shared colors when lane_idx % 6 wrapped
around. Assign the lowest-index color not used by either neighbor so
visually adjacent branches always have distinct colors.

Review-Needed: committed by Claude Code
AI-Assisted: one-shot (claude-sonnet-4-6[1m])"
```

---

## Task 4: Add `first_parent` to `LogWalker`

**Files:**
- Modify: `asyncgit/src/sync/logwalker.rs`
- Test: inline `#[cfg(test)]` in same file

`★ Insight ─────────────────────────────────────`
The reason first-parent must be in the traversal (not a filter): `LogWalker`
uses a `BinaryHeap` keyed by commit time. When we enqueue all parents of a
merge commit, the branch commits enter the heap and their ancestors follow.
A filter would hide them in output but the heap would still be traversed.
Only stopping the enqueue at the source gives true first-parent semantics.
`─────────────────────────────────────────────────`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)]` block in `asyncgit/src/sync/logwalker.rs`:

```rust
#[test]
fn test_logwalker_first_parent() -> Result<()> {
    use crate::sync::branch::create_branch;
    use crate::sync::{checkout_branch, merge_commit};
    // OR use the low-level git2 API directly if those helpers don't exist.
    // We need: main branch with 2 commits, a feature branch with 1 commit,
    // then a merge commit on main.

    let file_path = Path::new("foo");
    let branch_file = Path::new("bar");
    let (_td, repo) = repo_init_empty().unwrap();
    let root = repo.path().parent().unwrap();
    let repo_path: &RepoPath =
        &root.as_os_str().to_str().unwrap().into();

    // commit A on main
    File::create(root.join(file_path))?.write_all(b"a")?;
    stage_add_file(repo_path, file_path).unwrap();
    let commit_a = commit(repo_path, "commit A").unwrap();

    // create branch, commit B
    let repo2 = repo_path;
    // Use git2 directly if branch helpers aren't in scope:
    let branch_ref = {
        let r = repo_path;
        // create branch "feat" at commit_a
        let git_repo = crate::sync::repository::repo(r).unwrap();
        git_repo
            .branch(
                "feat",
                &git_repo.find_commit(commit_a.into()).unwrap(),
                false,
            )
            .unwrap();
        // checkout feat
        git_repo
            .set_head("refs/heads/feat")
            .unwrap();
        git_repo
            .checkout_head(Some(
                git2::build::CheckoutBuilder::default()
                    .force(),
            ))
            .unwrap();
        drop(git_repo);
    };

    File::create(root.join(branch_file))?.write_all(b"b")?;
    stage_add_file(repo_path, branch_file).unwrap();
    let commit_b = commit(repo_path, "commit B on feat").unwrap();

    // checkout main
    {
        let git_repo =
            crate::sync::repository::repo(repo_path).unwrap();
        git_repo.set_head("refs/heads/master").unwrap();
        git_repo
            .checkout_head(Some(
                git2::build::CheckoutBuilder::default()
                    .force(),
            ))
            .unwrap();
    }

    // commit C on main (direct)
    File::create(root.join(file_path))?.write_all(b"c")?;
    stage_add_file(repo_path, file_path).unwrap();
    let commit_c = commit(repo_path, "commit C on main").unwrap();

    // merge feat into main
    let merge_commit_id = {
        let git_repo =
            crate::sync::repository::repo(repo_path).unwrap();
        let feat_commit = git_repo
            .find_commit(commit_b.into())
            .unwrap();
        let main_commit = git_repo
            .find_commit(commit_c.into())
            .unwrap();
        let mut index = git_repo
            .merge_commits(&main_commit, &feat_commit, None)
            .unwrap();
        let tree_id = index.write_tree_to(&git_repo).unwrap();
        let tree = git_repo.find_tree(tree_id).unwrap();
        let sig = git_repo.signature().unwrap();
        let oid = git_repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "merge feat",
                &tree,
                &[&main_commit, &feat_commit],
            )
            .unwrap();
        CommitId::new(oid)
    };

    // Full walk: should include merge, commit_c, commit_b, commit_a (4 total)
    let mut items = Vec::new();
    let mut walk = LogWalker::new(&repo, 100)?;
    walk.read(&mut items)?;
    assert_eq!(items.len(), 4, "full walk should return 4 commits");
    assert!(items.contains(&commit_b), "full walk must include branch commit");

    // First-parent walk: should include merge, commit_c, commit_a (3 total, no commit_b)
    let mut items = Vec::new();
    let mut walk = LogWalker::new(&repo, 100)?.first_parent(true);
    walk.read(&mut items)?;
    assert_eq!(items.len(), 3, "first-parent walk should return 3 commits");
    assert!(
        !items.contains(&commit_b),
        "first-parent walk must NOT include branch commit"
    );
    assert!(
        items.contains(&merge_commit_id),
        "first-parent walk must include the merge commit"
    );
    assert!(
        items.contains(&commit_a),
        "first-parent walk must include commit A"
    );

    Ok(())
}
```

Run:
```bash
cargo nextest run -p asyncgit test_logwalker_first_parent
```
Expected: **FAIL** — `.first_parent()` method does not exist yet.

- [ ] **Step 2: Add `first_parent` field and builder to `LogWalker`**

In `asyncgit/src/sync/logwalker.rs`, modify the `LogWalker` struct:

```rust
pub struct LogWalker<'a> {
    commits: BinaryHeap<TimeOrderedCommit<'a>>,
    visited: HashSet<Oid>,
    limit: usize,
    repo: &'a Repository,
    filter: Option<SharedCommitFilterFn>,
    first_parent: bool,   // <-- add this
}
```

Add the builder method after the existing `filter()` builder:

```rust
/// Only traverse the first parent of each commit (like `git log --first-parent`).
#[must_use]
pub fn first_parent(self, v: bool) -> Self {
    Self { first_parent: v, ..self }
}
```

Update `new()` to initialize the field:

```rust
Ok(Self {
    commits,
    limit,
    visited: HashSet::with_capacity(1000),
    repo,
    filter: None,
    first_parent: false,   // <-- add this
})
```

- [ ] **Step 3: Update `read()` to respect `first_parent`**

In `read()`, find the line:
```rust
for p in c.0.parents() {
    self.visit(p);
}
```

Replace with:
```rust
let parents_to_visit: Vec<Commit<'_>> = if self.first_parent {
    c.0.parents().take(1).collect()
} else {
    c.0.parents().collect()
};
for p in parents_to_visit {
    self.visit(p);
}
```

- [ ] **Step 4: Run the test**

```bash
cargo nextest run -p asyncgit test_logwalker_first_parent
```

Expected: **PASS**.

If the test's repo-setup code doesn't compile (helpers may differ), adjust to use the same pattern as existing tests in the file — look at `test_logwalker_with_filter` for the exact import/helper shape, and use `git2` directly for branch creation and merging.

- [ ] **Step 5: Run full check**

```bash
make check
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add asyncgit/src/sync/logwalker.rs
git commit -m "feat(asyncgit): add first_parent traversal mode to LogWalker

When enabled, only the first parent of each commit is enqueued in the
BinaryHeap walk. Branch commits are never visited, matching the output
of git log --first-parent.

Review-Needed: committed by Claude Code
AI-Assisted: one-shot (claude-sonnet-4-6[1m])"
```

---

## Task 5: Thread `first_parent` through `AsyncLog`

**Files:**
- Modify: `asyncgit/src/revlog.rs`

- [ ] **Step 1: Add `first_parent` field to `AsyncLog`**

In the `AsyncLog` struct in `asyncgit/src/revlog.rs`, add:

```rust
pub struct AsyncLog {
    current: Arc<Mutex<AsyncLogResult>>,
    current_head: Arc<Mutex<Option<CommitId>>>,
    sender: Sender<AsyncGitNotification>,
    pending: Arc<AtomicBool>,
    background: Arc<AtomicBool>,
    filter: Option<SharedCommitFilterFn>,
    partial_extract: AtomicBool,
    repo: RepoPath,
    first_parent: bool,   // <-- add this
}
```

Update `AsyncLog::new()` to initialize it:

```rust
pub fn new(
    repo: RepoPath,
    sender: &Sender<AsyncGitNotification>,
    filter: Option<SharedCommitFilterFn>,
) -> Self {
    Self {
        // ...existing fields...
        first_parent: false,   // <-- add this
    }
}
```

- [ ] **Step 2: Add `set_first_parent` setter**

Add after `set_background()`:

```rust
/// Toggle first-parent traversal. Clears the cached head so the next
/// `fetch()` call starts a fresh walk.
pub fn set_first_parent(&mut self, v: bool) -> Result<()> {
    if self.first_parent != v {
        self.first_parent = v;
        // Reset cached head so head_changed() returns true → fresh fetch
        *self.current_head.lock()? = None;
    }
    Ok(())
}
```

Note: `set_background()` takes `&self` (atomics), but `set_first_parent` takes `&mut self` (plain bool). This is intentional — `first_parent` is only changed from the UI thread before calling `fetch()`.

- [ ] **Step 3: Thread `first_parent` into the spawned task**

In `fetch()`, the current code spawns:

```rust
rayon_core::spawn(move || {
    Self::fetch_helper(
        &repo_path,
        &arc_current,
        &arc_background,
        &sender,
        filter,
    )
    .expect("failed to fetch");
    // ...
});
```

Capture `first_parent` before the spawn and pass it through:

```rust
let first_parent = self.first_parent;   // capture before move

rayon_core::spawn(move || {
    Self::fetch_helper(
        &repo_path,
        &arc_current,
        &arc_background,
        &sender,
        filter,
        first_parent,    // <-- new argument
    )
    .expect("failed to fetch");
    // ...
});
```

- [ ] **Step 4: Update `fetch_helper` and its callees**

**`fetch_helper` signature:**
```rust
fn fetch_helper(
    repo_path: &RepoPath,
    arc_current: &Arc<Mutex<AsyncLogResult>>,
    arc_background: &Arc<AtomicBool>,
    sender: &Sender<AsyncGitNotification>,
    filter: Option<SharedCommitFilterFn>,
    first_parent: bool,    // <-- new
) -> Result<()> {
    // When first_parent is true, always use the git2 walker even without a filter.
    // The gix fast-path doesn't support first-parent traversal.
    if filter.is_some() || first_parent {
        Self::fetch_helper_with_filter(
            repo_path,
            arc_current,
            arc_background,
            sender,
            filter,
            first_parent,
        )
    } else {
        Self::fetch_helper_without_filter(
            repo_path,
            arc_current,
            arc_background,
            sender,
        )
    }
}
```

**`fetch_helper_with_filter` signature and walker construction:**

Change the signature to accept `filter: Option<SharedCommitFilterFn>` (was `SharedCommitFilterFn`) and `first_parent: bool`:

```rust
fn fetch_helper_with_filter(
    repo_path: &RepoPath,
    arc_current: &Arc<Mutex<AsyncLogResult>>,
    arc_background: &Arc<AtomicBool>,
    sender: &Sender<AsyncGitNotification>,
    filter: Option<SharedCommitFilterFn>,   // now Option
    first_parent: bool,                     // new
) -> Result<()> {
    let start_time = Instant::now();

    let mut entries = vec![CommitId::default(); LIMIT_COUNT];
    entries.resize(0, CommitId::default());

    let r = repo(repo_path)?;
    let mut walker = LogWalker::new(&r, LIMIT_COUNT)?
        .filter(filter)             // accepts Option directly — no change needed
        .first_parent(first_parent);

    loop {
        entries.clear();
        let read = walker.read(&mut entries)?;

        let mut current = arc_current.lock()?;
        current.commits.extend(entries.iter());
        current.duration = start_time.elapsed();

        if read == 0 {
            break;
        }
        Self::notify(sender);

        let sleep_duration = if arc_background.load(Ordering::Relaxed) {
            SLEEP_BACKGROUND
        } else {
            SLEEP_FOREGROUND
        };
        thread::sleep(sleep_duration);
    }

    log::trace!("revlog visited: {}", walker.visited());
    Ok(())
}
```

`LogWalker::filter()` already takes `Option<SharedCommitFilterFn>`, so passing `None` is fine.

`fetch_helper_without_filter` is unchanged.

- [ ] **Step 5: Verify it compiles**

```bash
cargo check -p asyncgit
```

Expected: no errors. Fix any type mismatches.

- [ ] **Step 6: Run check**

```bash
make check
```

Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add asyncgit/src/revlog.rs
git commit -m "feat(asyncgit): thread first_parent mode through AsyncLog

set_first_parent() resets the cached head so the next fetch() restarts
the walk. When first_parent=true, the gix fast-path is bypassed in favour
of LogWalker which now supports first-parent traversal.

Review-Needed: committed by Claude Code
AI-Assisted: one-shot (claude-sonnet-4-6[1m])"
```

---

## Task 6: Wire the toggle in the UI

**Files:**
- Modify: `src/components/commitlist.rs`
- Modify: `src/tabs/revlog.rs`

- [ ] **Step 1: Add badge support to `CommitList`**

In `src/components/commitlist.rs`, add `first_parent_mode: bool` to the struct:

```rust
pub struct CommitList {
    // ...existing fields...
    first_parent_mode: bool,
}
```

Initialize in `new()`:
```rust
Self {
    // ...existing fields...
    first_parent_mode: false,
}
```

Add a setter:
```rust
pub fn set_first_parent_mode(&mut self, v: bool) {
    self.first_parent_mode = v;
}
```

In `draw()`, find the title format line (~line 790):
```rust
let title = format!(
    "{} {}/{}",
    self.title,
    self.commits.len().saturating_sub(self.selection),
    self.commits.len(),
);
```

Change to:
```rust
let badge = if self.first_parent_mode { " [⊢1P]" } else { "" };
let title = format!(
    "{}{} {}/{}",
    self.title,
    badge,
    self.commits.len().saturating_sub(self.selection),
    self.commits.len(),
);
```

- [ ] **Step 2: Verify the badge compiles**

```bash
cargo check -p gitui
```

- [ ] **Step 3: Add `first_parent_mode` to `Revlog` and wire the toggle**

In `src/tabs/revlog.rs`, add to the `Revlog` struct:
```rust
pub struct Revlog {
    // ...existing fields...
    first_parent_mode: bool,
}
```

Initialize in `new()`:
```rust
Self {
    // ...existing fields...
    first_parent_mode: false,
}
```

Add a `toggle_first_parent` helper:
```rust
fn toggle_first_parent(&mut self) -> Result<()> {
    self.first_parent_mode = !self.first_parent_mode;
    self.git_log.set_first_parent(self.first_parent_mode)?;
    self.list.set_first_parent_mode(self.first_parent_mode);
    self.list.clear();
    self.git_log.fetch()?;
    Ok(())
}
```

- [ ] **Step 4: Add the key binding**

In `event()`, inside the `if self.visible` block, find where other single-key handlers are. Add before the final `Ok(EventState::NotConsumed)`:

```rust
} else if let Event::Key(k) = ev {
    // ...existing key handlers...
    if key_match(k, self.key_config.keys.enter) {
        // ...
    } else if k.code == KeyCode::Char('P')
        && k.modifiers.contains(KeyModifiers::SHIFT)
    {
        self.toggle_first_parent()?;
        return Ok(EventState::Consumed);
    }
```

Make sure `KeyCode` and `KeyModifiers` are imported. They come from `crossterm::event::{KeyCode, KeyModifiers}` — check the existing imports at the top of `revlog.rs` and add if missing.

- [ ] **Step 5: Run the app manually to verify**

```bash
make debug
```

Navigate to a repo with branches. Press `Shift+P` — the title should show `[⊢1P]` and the branch commits should disappear. Press again — they return.

- [ ] **Step 6: Run full check**

```bash
make check
```

Expected: all pass. Fix any unused import or dead-code warnings.

- [ ] **Step 7: Commit**

```bash
git add src/components/commitlist.rs src/tabs/revlog.rs
git commit -m "feat: add first-parent toggle to log view (Shift+P)

Press Shift+P to switch between full log and first-parent mode.
In first-parent mode only commits on the main branch spine are shown;
branch commits are hidden. A [⊢1P] badge appears in the log title.
Toggles cleanly alongside the existing search filter.

Review-Needed: committed by Claude Code
AI-Assisted: one-shot (claude-sonnet-4-6[1m])"
```

---

## Task 7: Final verification

- [ ] **Step 1: Create the feature/first-parent branch at this point**

```bash
git checkout -b feature/first-parent
```

(If you've been working on `feature/graph-base` throughout, tag the split point so graph-base and first-parent are tracked separately.)

- [ ] **Step 2: Full make check**

```bash
make check
```

Expected: all five checks pass. No regressions.

- [ ] **Step 3: Manual smoke test on a real repo with merges**

```bash
# Test on the gitui repo itself (it has merge commits)
make debug
```

In the log view:
1. Scroll through a few merge commits — verify graph renders correctly with no adjacent lanes sharing color.
2. Press `Shift+P` — verify branch commits disappear, `[⊢1P]` badge appears.
3. Press `Shift+P` again — verify full log returns.
4. Combine with search: press `/`, type something, press `Shift+P` — both filters should apply.
5. Press `g` (graph toggle from PR #2890) — verify graph toggles independently.

- [ ] **Step 4: Summarize the branch topology**

```bash
git log --oneline upstream/master..feature/first-parent | head -10
```

Expected: you see commits for: coloring fix, LogWalker first_parent, AsyncLog threading, UI toggle.

---

## Implementation notes

**Unused variable in Task 4 test:** The `let branch_ref = { ... }` block returns `()` — remove the binding (`let branch_ref =`) and use a bare `{ ... }` block expression to avoid an unused-variable clippy error.

**If the merge test in Task 4 doesn't compile:** The helpers for creating branches and merge commits may not be directly available in the `logwalker.rs` test scope. Use `git2` directly:
- `repo.branch("feat", &commit, false)` to create a branch
- `repo.set_head("refs/heads/feat")` + `repo.checkout_head()` to switch
- `repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&parent1, &parent2])` for the merge commit

Look at `git2-testing/src/lib.rs` for helper patterns.

**If `assign_color` needs adaptation to PR #2890's actual lane buffer shape:** The lane buffer uses `current: Vec<Option<Chunk>>` with `None` for empty slots. Adjust the neighbor scan to check `i.saturating_sub(1)` and `i+1` within the `current` slice, skipping `None` slots.

**If `KeyCode` / `KeyModifiers` aren't imported in `revlog.rs`:** Add:
```rust
use crossterm::event::{KeyCode, KeyModifiers};
```

**Key config system:** gitui has a `key_config.rs` / `KEY_CONFIG.md` that allows user remapping. Adding `Shift+P` as a hardcoded check is intentional for now — a proper `log_first_parent` key config entry can be added as follow-up using the same pattern as other keys in `src/keys/key_list.rs`.
