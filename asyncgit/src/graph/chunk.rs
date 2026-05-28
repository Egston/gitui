//! Per-lane cell stored in the graph buffer.

/// Tag describing what a [`Chunk`] represents in a lane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Markers {
	/// Placeholder for an uncommitted (working-tree) entry.
	Uncommitted,
	/// A normal commit row.
	Commit,
}

/// One cell of the lane buffer.
///
/// `alias` identifies the commit occupying the cell (via
/// [`super::oids::GraphOids`]), and `parent_a` / `parent_b` are the
/// lane aliases this commit is still waiting to consume. A cell with
/// both parents cleared is dropped.
#[derive(Clone, Debug)]
pub struct Chunk {
	/// Alias of the commit owning this lane, if any.
	pub alias: Option<usize>,
	/// First-parent lane alias still pending consumption.
	pub parent_a: Option<usize>,
	/// Second-parent lane alias (merge side), still pending.
	pub parent_b: Option<usize>,
	/// What this cell represents.
	pub marker: Markers,
}
