# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`gitui` is a terminal UI for git, written in Rust. It is a Cargo workspace: the
root crate is the TUI binary; the heavy git logic lives in the `asyncgit`
library crate.

## Common commands

The `Makefile` wraps the canonical invocations. Tests use **nextest**, not
`cargo test`.

- `make test` — run the full suite (`cargo nextest run --workspace`).
- Run one test: `cargo nextest run --workspace <substring>` (e.g.
  `cargo nextest run -p asyncgit test_log`).
- `make clippy` — lint (`cargo clippy --workspace --all-features`). Lints are
  treated as errors in this codebase (see "Lint policy" below).
- `make fmt` — format check (`cargo fmt -- --check`).
- `make sort` — `tombi format --check`, verifies `Cargo.toml` TOML formatting.
- `make deny` — `cargo deny check`, license/advisory checks (config in `deny.toml`).
- `make check` — runs `fmt clippy test sort deny`. **Run this before considering
  work done**; CI gates on all five.
- `make debug` — build & run with backtraces + timing feature against the
  current repo (`-l`).
- `make run-timing` — release run with scope-timing logs enabled.

MSRV is **1.88** (pinned in `rust-toolchain.toml`, enforced via `.clippy.toml`).

## Formatting rules (non-default)

`rustfmt.toml` sets `max_width = 70`, `hard_tabs = true`, Unix newlines. Code is
narrow and tab-indented — match it. The `.editorconfig` agrees.

## Lint policy

`asyncgit/src/lib.rs` and the other crate roots `#![deny(...)]` an aggressive
set: `clippy::all/perf/nursery/pedantic`, plus `clippy::unwrap_used`,
`clippy::panic`, and (in `filetreelist`) `clippy::expect_used`. **Do not
introduce `.unwrap()`, `.panic!()`, or `.expect()` in library code** — return
`Result` and propagate errors. A handful of pedantic lints are explicitly
`allow`ed at the crate root; check there before fighting a lint.

## Workspace layout

- `src/` — the TUI binary. Entry: `main.rs` → `gitui.rs` (terminal setup + main
  loop) → `app.rs` (top-level state).
- `asyncgit/` — all git operations. **`sync/`** holds synchronous git2/gix
  wrappers (commit, diff, status, branch, stash, rebase, remotes, …);
  the crate-root modules (`diff.rs`, `status.rs`, `revlog.rs`, `blame.rs`,
  `push.rs`, …) wrap those in background-thread "async" jobs.
- `filetreelist/` — reusable file-tree widget (used by the Files tab / revision
  browser).
- `git2-hooks/` — git hook discovery & execution on top of git2.
- `git2-testing/` — test-only helpers to spin up throwaway repos.
- `scopetime/` — `scope_time!` macro for the `timing` feature's profiling logs.

## Architecture (the big picture)

**Async/sync split.** The UI thread never runs slow git operations directly.
`asyncgit` exposes async wrappers (`AsyncDiff`, `AsyncStatus`, `AsyncLog`,
`AsyncPush`, …) and the generic `asyncjob::AsyncJob` trait + `AsyncSingleJob`.
These spawn work on a thread pool and report completion by sending an
`AsyncGitNotification` back through a channel. When adding a new slow git
feature: write the synchronous logic in `asyncgit/src/sync/`, then wrap it in an
async job that emits a notification.

**Component trait system** (`src/components/mod.rs`). Every UI element
implements:
- `DrawableComponent::draw()` — render into a `Rect`.
- `Component::commands()` — advertise the key bindings/actions it currently
  offers (drives the bottom command bar and the help popup).
- `Component::event()` — handle input, returning `EventState::Consumed` or
  `NotConsumed`.

Events flow through `event_pump()`, which dispatches to child components in
order until one consumes the event. Visibility gates command propagation via
`CommandBlocking::Blocking`/`PassingOn` (see `visibility_blocking`). New widgets
follow this trait trio and are wired into their parent's `event`/`draw`/
`commands` methods.

**Two communication channels** (converge in `gitui.rs::run_main_loop` via
`select_event`):
1. `Queue<InternalEvent>` (`src/queue.rs`) — synchronous, in-process UI actions
   (open a popup, switch tab, request confirmation, push a `StackablePopupOpen`).
   Components push events here instead of calling each other directly.
2. Async git notifications, ticker, filesystem watcher (`watcher.rs`, via
   `notify`), and input — all multiplexed in the main loop. `app.update()`
   handles ticks/notify; `app.update_async()` handles git-job results.

**App structure.** `App` (`src/app.rs`) owns the five tabs (`status_tab`,
`revlog`, `files_tab`, `stashing_tab`, `stashlist_tab`) and all popups. Popups
live in `src/popups/` and are layered via `popup_stack.rs`; reusable inner
widgets (lists, diff view, text input, commit details) live in
`src/components/`.

## Configuration files (RON)

User key bindings and theme are RON files loaded at runtime. `KEY_CONFIG.md`,
`THEMES.md`, and `vim_style_key_config.ron` document/exemplify the formats; the
loading code is under `src/keys/` (`key_config.rs`, `key_list.rs`) and
`asyncgit/src/sync/config.rs`. Default strings are centralized in
`src/strings.rs`.

## Snapshot tests

UI tests use **insta** (`insta::assert_snapshot!`) rendering against a ratatui
`TestBackend`; snapshots are committed in `src/snapshots/`. After an intentional
UI change, review/update with `cargo insta review` (or `INSTA_UPDATE=always`),
then commit the updated `.snap` files.

## Cargo features

Default = `ghemoji`, `regex-fancy`, `trace-libgit`, `vendor-openssl`. The
`regex-fancy`/`regex-onig` features (for syntect) are **mutually exclusive**.
`timing` enables `scopetime` profiling. `trace-libgit` enables libgit2 tracing.
