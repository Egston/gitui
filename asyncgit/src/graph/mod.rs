//! Streaming commit-graph renderer used by the log view.
//!
//! The [`GraphWalker`] ingests commits in walker order and produces
//! per-row [`GraphRow`]s describing the glyphs and colors a TUI
//! should draw.

/// Lane snapshots backing [`GraphWalker`].
pub mod buffer;
/// Per-lane cell type stored in the buffer.
pub mod chunk;
/// Numeric aliases for `CommitId`s used inside the walker.
pub mod oids;
/// Streaming graph state machine.
pub mod walker;

pub use walker::GraphWalker;

/// The type of connector between nodes in the graph. The renderer
/// maps each variant to a fixed glyph; see `glyph_for` in the binary
/// crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionType {
	/// Solid vertical line: lane continues straight down.
	Vertical,
	/// Dotted vertical line: lane carries the HEAD commit.
	VerticalDotted,
	/// Two lanes cross at this cell.
	Cross,
	/// Normal commit glyph in this lane.
	CommitNormal,
	/// Commit that is the tip of a branch.
	CommitBranch,
	/// Merge commit (two parents).
	CommitMerge,
	/// Commit referenced by a stash.
	CommitStash,
	/// Working-tree / uncommitted entry.
	CommitUncommitted,
	/// Right end of a merge-bridge (┓): horizontal arc turns
	/// downward into the merger lane.
	MergeBridgeStart,
	/// Middle segment of a merge-bridge (━).
	MergeBridgeMid,
	/// Left end of a merge-bridge (┏): horizontal arc opens
	/// rightward from the merge commit.
	MergeBridgeEnd,
	/// Lane forks downward into a child branch.
	BranchDown,
	/// Lane closes upward into a parent commit (┛).
	BranchUp,
	/// Lane closes upward and to the right (┗).
	BranchUpRight,
	/// Combined `BranchUp` + `MergeBridgeStart` corner.
	BranchUpMergeStart,
	/// Combined `BranchUpRight` + `MergeBridgeEnd` corner.
	BranchUpRightMergeEnd,
}

/// One row of rendered graph data for a single commit, ready to be
/// turned into TUI cells by the renderer.
#[derive(Clone, Debug, Default)]
pub struct GraphRow {
	/// Number of active (non-`None`) lanes at this commit row.
	pub lane_count: usize,

	/// Which lane index this commit sits on.
	pub commit_lane: usize,

	/// Whether this is a merge commit (two parents).
	pub is_merge: bool,

	/// Whether this commit is a branch tip.
	pub is_branch_tip: bool,

	/// Whether this commit carries a stash marker.
	pub is_stash: bool,

	/// Connectors emitted per lane:
	/// `None` = empty space;
	/// `Some((ConnectionType, color_index))` = draw this connector
	/// in this color slot.
	pub lanes: Vec<Option<(ConnectionType, usize)>>,

	/// If this commit merges rightward, `(from_lane, to_lane)` — the
	/// span to draw the horizontal bridge across.
	pub merge_bridge: Option<(usize, usize)>,

	/// If this commit spawns one or more branches, the spans to draw
	/// horizontal bridges across.
	pub branches: Vec<(usize, usize)>,
}
