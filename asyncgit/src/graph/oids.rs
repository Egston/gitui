//! Compact numeric aliases for `CommitId`s used by the graph walker.

use crate::sync::CommitId;
use std::collections::HashMap;

/// Mapping of `CommitId` to a stable numeric alias. The walker uses
/// the alias as a small `usize` key instead of repeatedly hashing
/// 20-byte oids in the inner loop.
pub struct GraphOids(HashMap<CommitId, usize>);

impl Default for GraphOids {
	fn default() -> Self {
		Self::new()
	}
}

impl GraphOids {
	/// Create an empty alias table.
	pub fn new() -> Self {
		Self(HashMap::new())
	}

	/// Return the existing alias for `id` or assign the next free
	/// integer if this is the first time `id` is seen.
	pub fn get_or_insert(&mut self, id: &CommitId) -> usize {
		if let Some(&alias) = self.0.get(id) {
			return alias;
		}

		let alias = self.0.len();
		self.0.insert(*id, alias);
		alias
	}

	/// Look up the alias for `id` without inserting; `None` if the
	/// commit hasn't been processed yet.
	pub fn get(&self, id: &CommitId) -> Option<usize> {
		self.0.get(id).copied()
	}
}
