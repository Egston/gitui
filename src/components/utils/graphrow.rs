use asyncgit::graph::ConnectionType;

pub const SYM_COMMIT: &str = "o";
pub const SYM_COMMIT_BRANCH: &str = "*";
pub const SYM_COMMIT_MERGE: &str = "M";
pub const SYM_COMMIT_STASH: &str = "*";
pub const SYM_COMMIT_UNCOMMITTED: &str = "+";
pub const SYM_CROSS: &str = "╋";
pub const SYM_VERTICAL: &str = "┃";
pub const SYM_VERTICAL_DOTTED: &str = "╏";
pub const SYM_HORIZONTAL: &str = "━";
pub const SYM_MERGE_BRIDGE_START: &str = "┓";
pub const SYM_MERGE_BRIDGE_MID: &str = "━";
pub const SYM_MERGE_BRIDGE_END: &str = "┏";
pub const SYM_BRANCH_UP: &str = "┛";
pub const SYM_BRANCH_DOWN: &str = "┓";
pub const SYM_BRANCH_UP_RIGHT: &str = "┗";
pub const SYM_SPACE: &str = " ";

/// Map a graph connector to the glyph used to render it. Exhaustive
/// on `ConnectionType` so the compiler flags any new variant that
/// forgets to pick a glyph.
pub const fn glyph_for(ct: ConnectionType) -> &'static str {
	match ct {
		ConnectionType::Vertical => SYM_VERTICAL,
		ConnectionType::VerticalDotted => SYM_VERTICAL_DOTTED,
		ConnectionType::Cross => SYM_CROSS,
		ConnectionType::CommitNormal => SYM_COMMIT,
		ConnectionType::CommitBranch => SYM_COMMIT_BRANCH,
		ConnectionType::CommitMerge => SYM_COMMIT_MERGE,
		ConnectionType::CommitStash => SYM_COMMIT_STASH,
		ConnectionType::CommitUncommitted => SYM_COMMIT_UNCOMMITTED,
		ConnectionType::MergeBridgeStart => SYM_MERGE_BRIDGE_START,
		ConnectionType::MergeBridgeMid => SYM_MERGE_BRIDGE_MID,
		ConnectionType::MergeBridgeEnd => SYM_MERGE_BRIDGE_END,
		ConnectionType::BranchDown => SYM_BRANCH_DOWN,
		ConnectionType::BranchUp
		| ConnectionType::BranchUpMergeStart => SYM_BRANCH_UP,
		ConnectionType::BranchUpRight
		| ConnectionType::BranchUpRightMergeEnd => SYM_BRANCH_UP_RIGHT,
	}
}
