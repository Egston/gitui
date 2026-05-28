use super::{
	repository::repo, CommitId, RepoPath, SharedCommitFilterFn,
};
use crate::error::Result;
use crate::LogOrdering;
use git2::{Commit, Oid, Repository};
use gix::revision::Walk;
use gix::traverse::commit::topo;
use std::{
	cmp::Ordering,
	collections::{BinaryHeap, HashSet},
};

struct TimeOrderedCommit<'a>(Commit<'a>);

impl Eq for TimeOrderedCommit<'_> {}

impl PartialEq for TimeOrderedCommit<'_> {
	fn eq(&self, other: &Self) -> bool {
		self.0.time().eq(&other.0.time())
	}
}

impl PartialOrd for TimeOrderedCommit<'_> {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for TimeOrderedCommit<'_> {
	fn cmp(&self, other: &Self) -> Ordering {
		self.0.time().cmp(&other.0.time())
	}
}

///
pub struct LogWalker<'a> {
	commits: BinaryHeap<TimeOrderedCommit<'a>>,
	visited: HashSet<Oid>,
	limit: usize,
	repo: &'a Repository,
	filter: Option<SharedCommitFilterFn>,
	first_parent: bool,
}

impl<'a> LogWalker<'a> {
	///
	pub fn new(repo: &'a Repository, limit: usize) -> Result<Self> {
		let c = repo.head()?.peel_to_commit()?;

		let mut commits = BinaryHeap::with_capacity(10);
		commits.push(TimeOrderedCommit(c));

		Ok(Self {
			commits,
			limit,
			visited: HashSet::with_capacity(1000),
			repo,
			filter: None,
			first_parent: false,
		})
	}

	///
	pub fn visited(&self) -> usize {
		self.visited.len()
	}

	///
	#[must_use]
	pub fn filter(
		self,
		filter: Option<SharedCommitFilterFn>,
	) -> Self {
		Self { filter, ..self }
	}

	/// Only traverse the first parent of each commit
	/// (like `git log --first-parent`).
	#[must_use]
	pub fn first_parent(self, v: bool) -> Self {
		Self {
			first_parent: v,
			..self
		}
	}

	///
	pub fn read(&mut self, out: &mut Vec<CommitId>) -> Result<usize> {
		let mut count = 0_usize;

		while let Some(c) = self.commits.pop() {
			let parent_limit =
				if self.first_parent { 1 } else { usize::MAX };
			for p in c.0.parents().take(parent_limit) {
				self.visit(p);
			}

			let id: CommitId = c.0.id().into();
			let commit_should_be_included =
				if let Some(ref filter) = self.filter {
					filter(self.repo, &id)?
				} else {
					true
				};

			if commit_should_be_included {
				out.push(id);
			}

			count += 1;
			if count == self.limit {
				break;
			}
		}

		Ok(count)
	}

	//
	fn visit(&mut self, c: Commit<'a>) {
		if self.visited.insert(c.id()) {
			self.commits.push(TimeOrderedCommit(c));
		}
	}
}

type TopoWalk = gix::traverse::commit::Topo<
	gix::OdbHandle,
	fn(&gix::oid) -> bool,
>;

enum WalkSource<'a> {
	Simple(Walk<'a>),
	Topo(Box<TopoWalk>),
}

/// This is separate from `LogWalker` because filtering currently (June 2024) works through
/// `SharedCommitFilterFn`.
///
/// `SharedCommitFilterFn` requires access to a `git2::repo::Repository` because, under the hood,
/// it calls into functions that work with a `git2::repo::Repository`. It seems unwise to open a
/// repo both through `gix::discover` and `Repository::open_ext` at the same time, so there is a
/// separate struct that works with `gix::Repository` only.
///
/// A more long-term option is to refactor filtering to work with a `gix::Repository` and to remove
/// `LogWalker` once this is done, but this is a larger effort.
pub struct LogWalkerWithoutFilter<'a> {
	walk: WalkSource<'a>,
	limit: usize,
	visited: usize,
}

impl<'a> LogWalkerWithoutFilter<'a> {
	/// Create a walker over the gix repo. `ordering` chooses between
	/// commit-time and topological walk; `first_parent` mimics
	/// `git log --first-parent` by following only the first parent
	/// at each step (composes with either ordering).
	pub fn new(
		repo: &'a mut gix::Repository,
		limit: usize,
		ordering: LogOrdering,
		first_parent: bool,
	) -> Result<Self> {
		// This seems to be an object cache size that yields optimal
		// performance. There's no specific reason this is 2^14, so
		// benchmarking might reveal that there's better values.
		repo.object_cache_size_if_unset(2_usize.pow(14));

		let commit = repo.head()?.peel_to_commit()?;
		let tips = [commit.id];

		let walk = match ordering {
			LogOrdering::Time => {
				let mut platform = repo
					.rev_walk(tips)
					.sorting(
						gix::revision::walk::Sorting::ByCommitTime(
							gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
						),
					)
					.use_commit_graph(false);
				if first_parent {
					platform = platform.first_parent_only();
				}
				WalkSource::Simple(platform.all()?)
			}
			LogOrdering::Topo => {
				let mut builder = topo::Builder::from_iters(
					repo.objects.clone(),
					tips,
					None::<Vec<gix::ObjectId>>,
				)
				.sorting(topo::Sorting::TopoOrder);
				if first_parent {
					builder = builder.parents(
						gix::traverse::commit::Parents::First,
					);
				}
				WalkSource::Topo(Box::new(builder.build()?))
			}
		};

		Ok(Self {
			walk,
			limit,
			visited: 0,
		})
	}

	///
	pub const fn visited(&self) -> usize {
		self.visited
	}

	///
	pub fn read(&mut self, out: &mut Vec<CommitId>) -> Result<usize> {
		let mut count = 0_usize;

		loop {
			#[allow(clippy::redundant_closure_for_method_calls)]
			let next_id = match &mut self.walk {
				WalkSource::Simple(w) => {
					w.next().and_then(|r| r.ok()).map(|i| i.id)
				}
				WalkSource::Topo(w) => {
					w.next().and_then(|r| r.ok()).map(|i| i.id)
				}
			};

			let Some(id) = next_id else { break };
			out.push(id.into());

			count += 1;
			if count == self.limit {
				break;
			}
		}

		self.visited += count;

		Ok(count)
	}
}

/// Returns the oldest commit on HEAD's first-parent spine that is a
/// descendant of (or equal to) `target`.
///
/// For an off-spine branch commit this is the merge commit that
/// integrated its branch into the mainline; for a commit already on
/// the spine it is the commit itself. Returns `None` if `target` is
/// not an ancestor of HEAD.
pub fn find_integrating_merge(
	repo_path: &RepoPath,
	target: CommitId,
) -> Result<Option<CommitId>> {
	let repo = repo(repo_path)?;
	let target_oid: Oid = target.into();
	let head_oid = repo.head()?.peel_to_commit()?.id();

	// Short-circuit: if target isn't an ancestor of HEAD at all,
	// there is no integrating merge to find. `merge_base` answers
	// this in one call instead of an O(spine_depth) walk.
	if repo.merge_base(head_oid, target_oid).ok() != Some(target_oid)
	{
		return Ok(None);
	}

	let mut current = repo.find_commit(head_oid)?;
	let mut answer: Option<CommitId> = None;

	loop {
		let cur_oid = current.id();
		// `cur_oid` descends from `target` iff `merge_base(cur,
		// target) == target`. This is the same predicate as
		// `graph_descendant_of` but typically cheaper on long
		// histories — libgit2's merge-base short-circuits at the
		// first reachable ancestor.
		let is_desc = cur_oid == target_oid
			|| repo.merge_base(cur_oid, target_oid).ok()
				== Some(target_oid);

		if is_desc {
			// still on/above the integrating merge: remember it and
			// keep descending the first-parent spine.
			answer = Some(cur_oid.into());
		} else {
			// dropped below it; the last remembered spine commit is
			// the integrating merge.
			break;
		}

		match current.parent(0) {
			Ok(p) => current = p,
			Err(_) => break,
		}
	}

	Ok(answer)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::error::Result;
	use crate::sync::commit_filter::{SearchFields, SearchOptions};
	use crate::sync::repository::gix_repo;
	use crate::sync::tests::write_commit_file;
	use crate::sync::{
		commit, get_commits_info, stage_add_file,
		tests::repo_init_empty,
	};
	use crate::sync::{
		diff_contains_file, filter_commit_by_search, LogFilterSearch,
		LogFilterSearchOptions, RepoPath,
	};
	use pretty_assertions::assert_eq;
	use std::{fs::File, io::Write, path::Path};

	#[test]
	fn test_limit() -> Result<()> {
		let file_path = Path::new("foo");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		commit(repo_path, "commit1").unwrap();
		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		let oid2 = commit(repo_path, "commit2").unwrap();

		let mut items = Vec::new();
		let mut walk = LogWalker::new(&repo, 1)?;
		walk.read(&mut items).unwrap();

		assert_eq!(items.len(), 1);
		assert_eq!(items[0], oid2);

		Ok(())
	}

	#[test]
	fn test_logwalker() -> Result<()> {
		let file_path = Path::new("foo");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		commit(repo_path, "commit1").unwrap();
		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		let oid2 = commit(repo_path, "commit2").unwrap();

		let mut items = Vec::new();
		let mut walk = LogWalker::new(&repo, 100)?;
		walk.read(&mut items).unwrap();

		let info = get_commits_info(repo_path, &items, 50).unwrap();
		dbg!(&info);

		assert_eq!(items.len(), 2);
		assert_eq!(items[0], oid2);

		let mut items = Vec::new();
		walk.read(&mut items).unwrap();

		assert_eq!(items.len(), 0);

		Ok(())
	}

	#[test]
	fn test_logwalker_without_filter() -> Result<()> {
		let file_path = Path::new("foo");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		commit(repo_path, "commit1").unwrap();
		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		let oid2 = commit(repo_path, "commit2").unwrap();

		let mut repo: gix::Repository = gix_repo(repo_path)?;
		let mut walk = LogWalkerWithoutFilter::new(
			&mut repo,
			100,
			crate::LogOrdering::Time,
			false,
		)?;
		let mut items = Vec::new();
		assert!(matches!(walk.read(&mut items), Ok(2)));

		let info = get_commits_info(repo_path, &items, 50).unwrap();
		dbg!(&info);

		assert_eq!(items.len(), 2);
		assert_eq!(items[0], oid2);

		let mut items = Vec::new();
		assert!(matches!(walk.read(&mut items), Ok(0)));

		assert_eq!(items.len(), 0);

		Ok(())
	}

	#[test]
	fn test_logwalker_first_parent() -> Result<()> {
		let file_path = Path::new("foo");
		let branch_file = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		// commit A on master
		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		let commit_a = commit(repo_path, "commit A").unwrap();

		// branch "feat" at A, checkout, commit B
		repo.branch(
			"feat",
			&repo.find_commit(commit_a.into()).unwrap(),
			false,
		)
		.unwrap();
		repo.set_head("refs/heads/feat").unwrap();
		repo.checkout_head(Some(
			git2::build::CheckoutBuilder::default().force(),
		))
		.unwrap();

		File::create(root.join(branch_file))?.write_all(b"b")?;
		stage_add_file(repo_path, branch_file).unwrap();
		let commit_b = commit(repo_path, "commit B on feat").unwrap();

		// back to master, commit C
		repo.set_head("refs/heads/master").unwrap();
		repo.checkout_head(Some(
			git2::build::CheckoutBuilder::default().force(),
		))
		.unwrap();
		File::create(root.join(file_path))?.write_all(b"c")?;
		stage_add_file(repo_path, file_path).unwrap();
		let commit_c = commit(repo_path, "commit C on main").unwrap();

		// merge feat into master: parents [C, B]
		let merge_commit_id = {
			let feat_commit =
				repo.find_commit(commit_b.into()).unwrap();
			let main_commit =
				repo.find_commit(commit_c.into()).unwrap();
			let mut index = repo
				.merge_commits(&main_commit, &feat_commit, None)
				.unwrap();
			let tree_id = index.write_tree_to(&repo).unwrap();
			let tree = repo.find_tree(tree_id).unwrap();
			let sig = repo.signature().unwrap();
			let oid = repo
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

		// full walk: M, C, B, A
		let mut items = Vec::new();
		let mut walk = LogWalker::new(&repo, 100)?;
		walk.read(&mut items)?;
		assert_eq!(
			items.len(),
			4,
			"full walk should return 4 commits"
		);
		assert!(
			items.contains(&commit_b),
			"full walk must include branch commit"
		);

		// first-parent walk: M, C, A (no B)
		let mut items = Vec::new();
		let mut walk = LogWalker::new(&repo, 100)?.first_parent(true);
		walk.read(&mut items)?;
		assert_eq!(
			items.len(),
			3,
			"first-parent walk should return 3 commits"
		);
		assert!(
			!items.contains(&commit_b),
			"first-parent walk must NOT include branch commit"
		);
		assert!(
			items.contains(&merge_commit_id),
			"must include merge commit"
		);
		assert!(items.contains(&commit_a), "must include commit A");

		Ok(())
	}

	#[test]
	fn test_logwalker_with_filter() -> Result<()> {
		let file_path = Path::new("foo");
		let second_file_path = Path::new("baz");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(&repo_path, file_path).unwrap();

		let _first_commit_id = commit(&repo_path, "commit1").unwrap();

		File::create(root.join(second_file_path))?.write_all(b"a")?;
		stage_add_file(&repo_path, second_file_path).unwrap();

		let second_commit_id = commit(&repo_path, "commit2").unwrap();

		File::create(root.join(file_path))?.write_all(b"b")?;
		stage_add_file(&repo_path, file_path).unwrap();

		let _third_commit_id = commit(&repo_path, "commit3").unwrap();

		let diff_contains_baz = diff_contains_file("baz".into());

		let mut items = Vec::new();
		let mut walker = LogWalker::new(&repo, 100)?
			.filter(Some(diff_contains_baz));
		walker.read(&mut items).unwrap();

		assert_eq!(items.len(), 1);
		assert_eq!(items[0], second_commit_id);

		let mut items = Vec::new();
		walker.read(&mut items).unwrap();

		assert_eq!(items.len(), 0);

		let diff_contains_bar = diff_contains_file("bar".into());

		let mut items = Vec::new();
		let mut walker = LogWalker::new(&repo, 100)?
			.filter(Some(diff_contains_bar));
		walker.read(&mut items).unwrap();

		assert_eq!(items.len(), 0);

		Ok(())
	}

	/// Build a test repo with the following history:
	///
	/// ```text
	///   R  (root, current-time timestamp — VERY NEW)
	///   |\
	///   | C  (mainline, ts=2_000 — intentionally OLD)
	///   | |
	///   | B1 (branch, ts=3_000)
	///   | |
	///   | B2 (branch, ts=4_000)
	///   |/
	///   M  (merge, ts=5_000, parents=[C, B2])
	/// ```
	///
	/// Because R was created with the real wall-clock time (via
	/// `commit()`) its timestamp is far in the future relative to
	/// C/B1/B2/M.  Under **time-order** the priority queue pops R
	/// before C (R.ts >> C.ts=2000), so the output is:
	///   M, B2, B1, R, C   ← R appears before C (child-before-parent
	///                         violated!)
	///
	/// Under **topo-order** the indegree algorithm enforces that every
	/// child is emitted before its parents, so the output is:
	///   M, B2, B1, C, R   ← C before R (correct)
	///
	/// The test asserts `pos(C) < pos(R)`, which passes with topo-order
	/// and fails with time-order.
	#[test]
	fn test_logwalker_without_filter_topo_order() -> Result<()> {
		use crate::LogOrdering;

		let file_path = Path::new("foo");
		let branch_file = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		// Root commit R — uses current wall-clock time, so its
		// timestamp is VERY large relative to C/B1/B2/M below.
		File::create(root.join(file_path))?.write_all(b"r")?;
		stage_add_file(repo_path, file_path).unwrap();
		let commit_r = commit(repo_path, "R root").unwrap();

		// Mainline commit C: child of R, but with a deliberately
		// OLD timestamp (2_000) so time-order will pop R before C.
		File::create(root.join(file_path))?.write_all(b"c")?;
		stage_add_file(repo_path, file_path).unwrap();
		let commit_c = {
			let sig = git2::Signature::new(
				"a",
				"a@b.c",
				&git2::Time::new(2_000, 0),
			)
			.unwrap();
			let tree = repo
				.find_tree(
					repo.index().unwrap().write_tree().unwrap(),
				)
				.unwrap();
			let parent =
				repo.head().unwrap().peel_to_commit().unwrap();
			let oid = repo
				.commit(
					Some("HEAD"),
					&sig,
					&sig,
					"C old mainline",
					&tree,
					&[&parent],
				)
				.unwrap();
			CommitId::new(oid)
		};

		// Branch off R, two newer branch commits B1 (ts=3_000) and
		// B2 (ts=4_000).
		let commit_r_raw = {
			repo.find_commit(commit_c.into())
				.unwrap()
				.parent(0)
				.unwrap()
		};
		repo.branch("feat", &commit_r_raw, false).unwrap();
		repo.set_head("refs/heads/feat").unwrap();
		repo.checkout_head(Some(
			git2::build::CheckoutBuilder::default().force(),
		))
		.unwrap();

		File::create(root.join(branch_file))?.write_all(b"b1")?;
		stage_add_file(repo_path, branch_file).unwrap();
		let _commit_b1 = {
			let sig = git2::Signature::new(
				"a",
				"a@b.c",
				&git2::Time::new(3_000, 0),
			)
			.unwrap();
			let tree = repo
				.find_tree(
					repo.index().unwrap().write_tree().unwrap(),
				)
				.unwrap();
			let parent =
				repo.head().unwrap().peel_to_commit().unwrap();
			repo.commit(
				Some("HEAD"),
				&sig,
				&sig,
				"B1",
				&tree,
				&[&parent],
			)
			.unwrap()
		};

		File::create(root.join(branch_file))?.write_all(b"b2")?;
		stage_add_file(repo_path, branch_file).unwrap();
		let commit_b2 = {
			let sig = git2::Signature::new(
				"a",
				"a@b.c",
				&git2::Time::new(4_000, 0),
			)
			.unwrap();
			let tree = repo
				.find_tree(
					repo.index().unwrap().write_tree().unwrap(),
				)
				.unwrap();
			let parent =
				repo.head().unwrap().peel_to_commit().unwrap();
			let oid = repo
				.commit(
					Some("HEAD"),
					&sig,
					&sig,
					"B2",
					&tree,
					&[&parent],
				)
				.unwrap();
			CommitId::new(oid)
		};

		// Back to master; create merge M (ts=5_000), parents=[C, B2].
		repo.set_head("refs/heads/master").unwrap();
		repo.checkout_head(Some(
			git2::build::CheckoutBuilder::default().force(),
		))
		.unwrap();
		let c_raw = repo.find_commit(commit_c.into()).unwrap();
		let b2_raw = repo.find_commit(commit_b2.into()).unwrap();
		let _merge = {
			let mut index =
				repo.merge_commits(&c_raw, &b2_raw, None).unwrap();
			let tree_id = index.write_tree_to(&repo).unwrap();
			let tree = repo.find_tree(tree_id).unwrap();
			let sig = git2::Signature::new(
				"a",
				"a@b.c",
				&git2::Time::new(5_000, 0),
			)
			.unwrap();
			repo.commit(
				Some("HEAD"),
				&sig,
				&sig,
				"merge feat",
				&tree,
				&[&c_raw, &b2_raw],
			)
			.unwrap()
		};

		// Topo-order must emit C before R (C is a child of R, so it
		// must appear before its parent regardless of timestamps).
		// Time-order VIOLATES this: R.ts >> C.ts, so R pops out of
		// the priority queue before C is emitted.
		let mut grepo = gix_repo(repo_path)?;
		let mut walk = LogWalkerWithoutFilter::new(
			&mut grepo,
			100,
			LogOrdering::Topo,
			false,
		)?;
		let mut items = Vec::new();
		walk.read(&mut items)?;

		let pos = |c: &CommitId| {
			items
				.iter()
				.position(|x| x == c)
				.expect("commit not found in walk output")
		};
		assert!(
			pos(&commit_c) < pos(&commit_r),
			"topo-order must emit child C before parent R \
			 (time-order would violate this because R.ts >> \
			 C.ts); got C={} R={}",
			pos(&commit_c),
			pos(&commit_r),
		);

		Ok(())
	}

	/// gix walker with `first_parent=true` mirrors the git2 walker
	/// behaviour: the off-spine branch commits are skipped, both the
	/// mainline commit and the integrating merge are kept. Composes
	/// with `LogOrdering::Topo` (regression for the previous gap
	/// where first-parent silently dropped the topo preference).
	#[test]
	fn test_gix_first_parent_excludes_branch_commits() -> Result<()> {
		let file_path = Path::new("foo");
		let branch_file = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		// A on master
		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		let commit_a = commit(repo_path, "A").unwrap();

		// feat at A, then B
		repo.branch(
			"feat",
			&repo.find_commit(commit_a.into()).unwrap(),
			false,
		)
		.unwrap();
		repo.set_head("refs/heads/feat").unwrap();
		repo.checkout_head(Some(
			git2::build::CheckoutBuilder::default().force(),
		))
		.unwrap();
		File::create(root.join(branch_file))?.write_all(b"b")?;
		stage_add_file(repo_path, branch_file).unwrap();
		let commit_b = commit(repo_path, "B on feat").unwrap();

		// back to master, C, then merge
		repo.set_head("refs/heads/master").unwrap();
		repo.checkout_head(Some(
			git2::build::CheckoutBuilder::default().force(),
		))
		.unwrap();
		File::create(root.join(file_path))?.write_all(b"c")?;
		stage_add_file(repo_path, file_path).unwrap();
		let commit_c = commit(repo_path, "C on main").unwrap();
		let merge_id = {
			let main_c = repo.find_commit(commit_c.into()).unwrap();
			let feat_c = repo.find_commit(commit_b.into()).unwrap();
			let mut idx =
				repo.merge_commits(&main_c, &feat_c, None).unwrap();
			let tree = repo
				.find_tree(idx.write_tree_to(&repo).unwrap())
				.unwrap();
			let sig = repo.signature().unwrap();
			CommitId::new(
				repo.commit(
					Some("HEAD"),
					&sig,
					&sig,
					"merge feat",
					&tree,
					&[&main_c, &feat_c],
				)
				.unwrap(),
			)
		};

		for ordering in
			[crate::LogOrdering::Time, crate::LogOrdering::Topo]
		{
			let mut grepo = gix_repo(repo_path)?;
			let mut walk = LogWalkerWithoutFilter::new(
				&mut grepo, 100, ordering, true,
			)?;
			let mut items = Vec::new();
			walk.read(&mut items)?;

			assert!(
				!items.contains(&commit_b),
				"gix first-parent (ordering={ordering:?}) must \
				 NOT include branch commit B; got {items:?}",
			);
			assert!(items.contains(&commit_a), "must include A");
			assert!(items.contains(&merge_id), "must include merge",);
		}

		Ok(())
	}

	#[test]
	fn test_find_integrating_merge() -> Result<()> {
		let file_path = Path::new("foo");
		let branch_file = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();

		// base + M1 (mainline)
		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(&repo_path, file_path).unwrap();
		let _base = commit(&repo_path, "base").unwrap();
		File::create(root.join(file_path))?.write_all(b"a2")?;
		stage_add_file(&repo_path, file_path).unwrap();
		let m1 = commit(&repo_path, "M1 mainline").unwrap();

		// branch feat off M1: b1, b2
		repo.branch(
			"feat",
			&repo.find_commit(m1.into()).unwrap(),
			false,
		)
		.unwrap();
		repo.set_head("refs/heads/feat").unwrap();
		repo.checkout_head(Some(
			git2::build::CheckoutBuilder::default().force(),
		))
		.unwrap();
		File::create(root.join(branch_file))?.write_all(b"b1")?;
		stage_add_file(&repo_path, branch_file).unwrap();
		let b1 = commit(&repo_path, "b1").unwrap();
		File::create(root.join(branch_file))?.write_all(b"b2")?;
		stage_add_file(&repo_path, branch_file).unwrap();
		let b2 = commit(&repo_path, "b2").unwrap();

		// back to master, merge → M2
		repo.set_head("refs/heads/master").unwrap();
		repo.checkout_head(Some(
			git2::build::CheckoutBuilder::default().force(),
		))
		.unwrap();
		let m2 = {
			let feat_c = repo.find_commit(b2.into()).unwrap();
			let main_c = repo.find_commit(m1.into()).unwrap();
			let mut index =
				repo.merge_commits(&main_c, &feat_c, None).unwrap();
			let tree_id = index.write_tree_to(&repo).unwrap();
			let tree = repo.find_tree(tree_id).unwrap();
			let sig = repo.signature().unwrap();
			let oid = repo
				.commit(
					Some("HEAD"),
					&sig,
					&sig,
					"M2 merge",
					&tree,
					&[&main_c, &feat_c],
				)
				.unwrap();
			CommitId::new(oid)
		};

		// off-spine branch commit → its integrating merge
		assert_eq!(find_integrating_merge(&repo_path, b1)?, Some(m2));
		// on-spine merge → itself
		assert_eq!(find_integrating_merge(&repo_path, m2)?, Some(m2));
		// plain mainline commit → itself
		assert_eq!(find_integrating_merge(&repo_path, m1)?, Some(m1));

		Ok(())
	}

	#[test]
	fn test_logwalker_with_filter_search() {
		let (_td, repo) = repo_init_empty().unwrap();

		write_commit_file(&repo, "foo", "a", "commit1");
		let second_commit_id = write_commit_file(
			&repo,
			"baz",
			"a",
			"my commit msg (#2)",
		);
		write_commit_file(&repo, "foo", "b", "commit3");

		let log_filter = filter_commit_by_search(
			LogFilterSearch::new(LogFilterSearchOptions {
				fields: SearchFields::MESSAGE_SUMMARY,
				options: SearchOptions::FUZZY_SEARCH,
				search_pattern: String::from("my msg"),
			}),
		);

		let mut items = Vec::new();
		let mut walker = LogWalker::new(&repo, 100)
			.unwrap()
			.filter(Some(log_filter));
		walker.read(&mut items).unwrap();

		assert_eq!(items.len(), 1);
		assert_eq!(items[0], second_commit_id);

		let log_filter = filter_commit_by_search(
			LogFilterSearch::new(LogFilterSearchOptions {
				fields: SearchFields::FILENAMES,
				options: SearchOptions::FUZZY_SEARCH,
				search_pattern: String::from("fo"),
			}),
		);

		let mut items = Vec::new();
		let mut walker = LogWalker::new(&repo, 100)
			.unwrap()
			.filter(Some(log_filter));
		walker.read(&mut items).unwrap();

		assert_eq!(items.len(), 2);
	}
}
