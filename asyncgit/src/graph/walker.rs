use super::buffer::Buffer;
use super::chunk::{Chunk, Markers};
use super::oids::GraphOids;
use super::{ConnectionType, GraphRow};
use crate::sync::CommitId;
use im::Vector;
use std::collections::{HashMap, HashSet};

/// Streaming graph state machine. Consume commits with
/// [`Self::process`] in walker order; ask for rendered rows with
/// [`Self::compute_rows`].
pub struct GraphWalker {
	/// Lane state + per-step delta history.
	pub buffer: Buffer,
	/// `CommitId` → numeric alias table.
	pub oids: GraphOids,
	/// For each merge alias, the alias of its second parent (used
	/// to draw the merge-bridge).
	pub mergers_map: HashMap<usize, usize>,
}

impl Default for GraphWalker {
	fn default() -> Self {
		Self::new()
	}
}

impl GraphWalker {
	/// Construct an empty graph walker.
	pub fn new() -> Self {
		Self {
			buffer: Buffer::new(),
			oids: GraphOids::new(),
			mergers_map: HashMap::new(),
		}
	}

	/// Lowest color index not used by an immediately-adjacent
	/// (`lane_idx`-1 or `lane_idx`+1) currently-active lane.
	pub fn assign_color(
		lane_idx: usize,
		active_lane_indices: &[usize],
		lane_colors: &HashMap<usize, usize>,
	) -> usize {
		let neighbor_colors: HashSet<usize> = active_lane_indices
			.iter()
			.filter(|&&i| {
				i == lane_idx.saturating_sub(1) || i == lane_idx + 1
			})
			.filter_map(|i| lane_colors.get(i).copied())
			.collect();
		// At most two neighbors contribute a color, so a free
		// index always exists within this bounded range.
		(0_usize..=neighbor_colors.len())
			.find(|c| !neighbor_colors.contains(c))
			.unwrap_or(0)
	}

	/// Build the per-row color map with a single left-to-right
	/// greedy pass over every occupied lane (a populated `curr`
	/// slot) plus every `branching_lanes` index (these render a
	/// `BranchUp` glyph at an otherwise-empty slot and still need
	/// a color). Adjacent occupied lanes are guaranteed distinct
	/// colors.
	fn build_lane_colors(
		curr: &Vector<Option<Chunk>>,
		branching_lanes: &[usize],
	) -> HashMap<usize, usize> {
		let mut occupied: Vec<usize> = curr
			.iter()
			.enumerate()
			.filter_map(|(i, c)| c.as_ref().map(|_| i))
			.collect();
		for &i in branching_lanes {
			if !occupied.contains(&i) {
				occupied.push(i);
			}
		}
		occupied.sort_unstable();

		let mut lane_colors: HashMap<usize, usize> = HashMap::new();
		for &idx in &occupied {
			let color =
				Self::assign_color(idx, &occupied, &lane_colors);
			lane_colors.insert(idx, color);
		}
		lane_colors
	}

	/// Feed one commit's identity + parents into the graph state.
	/// Parents beyond the second are ignored, so octopus merges
	/// render as 2-parent merges.
	pub fn process(
		&mut self,
		id: &CommitId,
		parents: &[CommitId],
		first_parent: bool,
	) {
		let alias = self.oids.get_or_insert(id);
		let parent_a =
			parents.first().map(|p| self.oids.get_or_insert(p));
		// In first-parent mode the branch-side commits are never
		// walked, so a merge's second-parent lane could never close.
		// Ignore it entirely: the graph collapses to a single spine.
		let parent_b = if first_parent {
			None
		} else {
			parents.get(1).map(|p| self.oids.get_or_insert(p))
		};

		let chunk = Chunk {
			alias: Some(alias),
			parent_a,
			parent_b,
			marker: Markers::Commit,
		};

		if let Some(b) = parent_b {
			self.mergers_map.insert(alias, b);
		}

		if parent_a.is_some() && parent_b.is_some() {
			let already_tracked =
				self.buffer.current.iter().any(|c| {
					c.as_ref().is_some_and(|c| {
						c.parent_a == parent_b && c.parent_b.is_none()
					})
				});
			if !already_tracked {
				self.buffer.merger(alias);
			}
		}

		self.buffer.update(&chunk);
	}

	/// Lane state after the `global_idx`-th `process()` call, or an
	/// empty `Vector` if that step hasn't been recorded yet.
	pub fn snapshot_at(
		&self,
		global_idx: usize,
	) -> Vector<Option<Chunk>> {
		self.buffer
			.decompress(global_idx, global_idx)
			.into_iter()
			.next()
			.unwrap_or_default()
	}

	/// Render rows for `commit_range` (one [`GraphRow`] per id),
	/// reconstructing lane snapshots from the buffer history. The
	/// `branch_tips` / `stashes` / `head_id` sets drive per-commit
	/// glyph variants (branch tip, stash marker, HEAD dotted line).
	pub fn compute_rows(
		&self,
		commit_range: &[CommitId],
		global_start: usize,
		branch_tips: &HashSet<CommitId>,
		stashes: &HashSet<CommitId>,
		head_id: Option<&CommitId>,
	) -> Vec<GraphRow> {
		let end = global_start + commit_range.len().saturating_sub(1);
		let snapshots = self.buffer.decompress(global_start, end);

		commit_range
			.iter()
			.enumerate()
			.map(|(index, commit_id)| {
				let curr =
					snapshots.get(index).cloned().unwrap_or_default();
				let prev = if index > 0 {
					snapshots.get(index - 1).cloned()
				} else if global_start > 0 {
					Some(self.snapshot_at(global_start - 1))
				} else {
					None
				};

				self.render_row(
					commit_id,
					&curr,
					prev.as_ref(),
					branch_tips,
					stashes,
					head_id,
				)
			})
			.collect()
	}

	fn render_row(
		&self,
		commit_id: &CommitId,
		curr: &Vector<Option<Chunk>>,
		prev: Option<&Vector<Option<Chunk>>>,
		branch_tips: &HashSet<CommitId>,
		stashes: &HashSet<CommitId>,
		head_id: Option<&CommitId>,
	) -> GraphRow {
		let alias = self.oids.get(commit_id);
		// Every commit reaching `render_row` was already fed through
		// `process()`, which inserts an alias. A missing alias here
		// is a hard desync (commit rendered before being walked);
		// loud-fail in debug and log + fall back to lane 0 in
		// release so the UI doesn't crash on a graph oddity.
		debug_assert!(
			alias.is_some(),
			"render_row called for {commit_id:?} not in oids alias table",
		);
		if alias.is_none() {
			log::error!(
				"render_row: no alias for {commit_id:?}; \
				 painting lane 0 (graph may misrender this row)"
			);
		}
		let commit_lane = curr
			.iter()
			.position(|c| {
				c.as_ref().is_some_and(|chunk| {
					alias.is_some() && chunk.alias == alias
				})
			})
			.unwrap_or(0);

		let parent_b_alias =
			alias.and_then(|a| self.mergers_map.get(&a).copied());

		let is_merge = parent_b_alias.is_some();
		let is_branch_tip = branch_tips.contains(commit_id);
		let is_stash = stashes.contains(commit_id);

		let branching_lanes: Vec<usize> = prev
			.into_iter()
			.flatten() // Unwrapping the optional, returning empty vec when None
			.enumerate()
			.filter(|(i, pc)| {
				pc.is_some()
					&& curr.get(*i).is_none_or(Option::is_none)
			})
			.map(|(i, _)| i)
			.collect();

		let lane_colors =
			Self::build_lane_colors(curr, &branching_lanes);

		let mut lanes = vec![None; curr.len()];

		let merge_bridge = is_merge
			.then(|| {
				let target_lane = curr.iter().position(|c| {
					c.as_ref().is_some_and(|chunk| {
						parent_b_alias.is_some()
							&& chunk.parent_a == parent_b_alias
					})
				});
				target_lane.map(|t| {
					if t > commit_lane {
						(commit_lane, t)
					} else {
						(t, commit_lane)
					}
				})
			})
			.flatten();

		self.fill_lanes(
			&mut lanes,
			curr,
			alias,
			head_id,
			is_stash,
			is_merge,
			is_branch_tip,
			&branching_lanes,
			&lane_colors,
		);

		let branches = Self::draw_bridges(
			&mut lanes,
			merge_bridge,
			commit_lane,
			&branching_lanes,
			&lane_colors,
		);

		GraphRow {
			lane_count: curr.iter().flatten().count(),
			commit_lane,
			is_merge,
			is_branch_tip,
			is_stash,
			lanes,
			merge_bridge,
			branches,
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn fill_lanes(
		&self,
		lanes: &mut [Option<(ConnectionType, usize)>],
		curr: &Vector<Option<Chunk>>,
		alias: Option<usize>,
		head_id: Option<&CommitId>,
		is_stash: bool,
		is_merge: bool,
		is_branch_tip: bool,
		branching_lanes: &[usize],
		lane_colors: &HashMap<usize, usize>,
	) {
		for (lane_idx, chunk_item) in curr.iter().enumerate() {
			let color =
				lane_colors.get(&lane_idx).copied().unwrap_or(0);
			let Some(chunk) = chunk_item.as_ref() else {
				if branching_lanes.contains(&lane_idx) {
					lanes[lane_idx] =
						Some((ConnectionType::BranchUp, color));
				}
				continue;
			};

			if alias.is_some() && chunk.alias == alias {
				let conn_type =
					match (is_stash, is_merge, is_branch_tip) {
						(true, _, _) => ConnectionType::CommitStash,
						(_, true, _) => ConnectionType::CommitMerge,
						(_, _, true) => ConnectionType::CommitBranch,
						_ => ConnectionType::CommitNormal,
					};

				lanes[lane_idx] = Some((conn_type, color));
			} else {
				let is_dotted = head_id
					.and_then(|h| self.oids.get(h))
					.is_some_and(|ha| {
						chunk.parent_a == Some(ha)
							|| chunk.parent_b == Some(ha)
					}) && lane_idx == 0;

				let is_orphan = chunk.parent_a.is_none()
					&& chunk.parent_b.is_none();

				if is_orphan {
					continue;
				}

				let conn = if is_dotted {
					ConnectionType::VerticalDotted
				} else {
					ConnectionType::Vertical
				};

				lanes[lane_idx] = Some((conn, color));
			}
		}
	}

	fn draw_bridges(
		lanes: &mut Vec<Option<(ConnectionType, usize)>>,
		merge_bridge: Option<(usize, usize)>,
		commit_lane: usize,
		branching_lanes: &[usize],
		lane_colors: &HashMap<usize, usize>,
	) -> Vec<(usize, usize)> {
		if let Some((from, to)) = merge_bridge {
			let target_lane =
				if from == commit_lane { to } else { from };
			let color =
				lane_colors.get(&commit_lane).copied().unwrap_or(0);
			Self::draw_bridge(
				lanes,
				from,
				to,
				color,
				target_lane,
				ConnectionType::MergeBridgeMid,
				ConnectionType::MergeBridgeStart,
				ConnectionType::MergeBridgeEnd,
			);
		}

		let mut branches = Vec::new();
		for &branch_lane in branching_lanes {
			let from = std::cmp::min(branch_lane, commit_lane);
			let to = std::cmp::max(branch_lane, commit_lane);
			branches.push((from, to));

			if lanes.len() <= to {
				lanes.resize(to + 1, None);
			}

			let color =
				lane_colors.get(&branch_lane).copied().unwrap_or(0);
			Self::draw_bridge(
				lanes,
				from,
				to,
				color,
				branch_lane,
				ConnectionType::MergeBridgeMid,
				ConnectionType::BranchUp,
				ConnectionType::BranchUpRight,
			);
		}
		branches
	}

	#[allow(clippy::too_many_arguments)]
	fn draw_bridge(
		lanes: &mut [Option<(ConnectionType, usize)>],
		from: usize,
		to: usize,
		color: usize,
		corner_lane: usize,
		mid: ConnectionType,
		corner_right: ConnectionType,
		corner_left: ConnectionType,
	) {
		for lane in lanes.iter_mut().take(to).skip(from + 1) {
			match lane {
				Some((
					ConnectionType::Vertical
					| ConnectionType::VerticalDotted,
					_,
				)) => {
					*lane = Some((ConnectionType::Cross, color));
				}
				_ => {
					*lane = Some((mid, color));
				}
			}
		}

		if corner_lane == to {
			let new_corner = match (lanes[to], corner_right) {
				(
					Some((ConnectionType::MergeBridgeStart, _)),
					ConnectionType::BranchUp,
				) => ConnectionType::BranchUpMergeStart,
				_ => corner_right,
			};
			lanes[to] = Some((new_corner, color));
		} else if corner_lane == from {
			let new_corner = match (lanes[from], corner_left) {
				(
					Some((ConnectionType::MergeBridgeEnd, _)),
					ConnectionType::BranchUpRight,
				) => ConnectionType::BranchUpRightMergeEnd,
				_ => corner_left,
			};
			lanes[from] = Some((new_corner, color));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_no_adjacent_lanes_share_color() {
		use std::collections::HashMap;

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

		for i in 0..6 {
			let c_i = lane_colors[&i];
			let c_next = lane_colors[&(i + 1)];
			assert_ne!(
				c_i,
				c_next,
				"lanes {i} and {} share color {c_i}",
				i + 1
			);
		}
	}

	#[test]
	fn test_first_parent_graph_stays_single_lane() {
		// A first-parent walk hides branch-side commits. A merge
		// still lists two parents, but its second parent is never
		// walked - so processing with first_parent=true must ignore
		// it, otherwise the lane it opens never closes and the graph
		// grows an extra lane on every merge.
		let id = |n: u8| {
			CommitId::from_str_unchecked(&format!("{n:040}")).unwrap()
		};
		let (a, b, c, m) = (id(1), id(2), id(3), id(4));

		// first-parent spine, newest first: M(merge [C,B]), C, A
		let commits: [(CommitId, Vec<CommitId>); 3] =
			[(m, vec![c, b]), (c, vec![a]), (a, vec![])];

		let mut walker = GraphWalker::new();
		for (id, parents) in &commits {
			walker.process(id, parents, true);
		}

		let ids: Vec<CommitId> =
			commits.iter().map(|(id, _)| *id).collect();
		let rows = walker.compute_rows(
			&ids,
			0,
			&HashSet::new(),
			&HashSet::new(),
			None,
		);

		assert_eq!(rows.len(), 3);
		for row in &rows {
			assert_eq!(
				row.lane_count, 1,
				"first-parent graph must stay single-lane, got {}",
				row.lane_count
			);
			assert!(
				!row.is_merge,
				"second parent must be ignored in first-parent mode"
			);
		}
	}

	#[test]
	fn test_octopus_first_parent_stays_single_lane() {
		// Octopus merge (3+ parents) under first-parent mode: the
		// walker must follow only parent 0 and ignore parents 1..N
		// the same way it ignores parent 1 of a normal merge.
		// Regression guard for the comment on `process` claiming
		// "octopus merges are collapsed to normal merges".
		let id = |n: u8| {
			CommitId::from_str_unchecked(&format!("{n:040}")).unwrap()
		};
		let (a, b, c, m) = (id(1), id(2), id(3), id(5));

		// M is a 3-parent merge of A, B, C; A is the spine root.
		let commits: [(CommitId, Vec<CommitId>); 2] =
			[(m, vec![a, b, c]), (a, vec![])];

		let mut walker = GraphWalker::new();
		for (id, parents) in &commits {
			walker.process(id, parents, true);
		}

		let ids: Vec<CommitId> =
			commits.iter().map(|(id, _)| *id).collect();
		let rows = walker.compute_rows(
			&ids,
			0,
			&HashSet::new(),
			&HashSet::new(),
			None,
		);

		assert_eq!(rows.len(), 2);
		for row in &rows {
			assert_eq!(
				row.lane_count, 1,
				"octopus merge under first-parent must stay \
				 single-lane; got {}",
				row.lane_count,
			);
			assert!(
				!row.is_merge,
				"octopus second/third parents must be ignored in \
				 first-parent mode"
			);
		}
	}
}
