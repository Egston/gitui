use super::{CommitId, SharedCommitFilterFn};
use crate::error::Result;
use git2::{Commit, Oid, Repository};
use gix::revision::Walk;
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
	walk: Walk<'a>,
	limit: usize,
	visited: usize,
}

impl<'a> LogWalkerWithoutFilter<'a> {
	///
	pub fn new(
		repo: &'a mut gix::Repository,
		limit: usize,
	) -> Result<Self> {
		// This seems to be an object cache size that yields optimal performance. There’s no specific
		// reason this is 2^14, so benchmarking might reveal that there’s better values.
		repo.object_cache_size_if_unset(2_usize.pow(14));

		let commit = repo.head()?.peel_to_commit()?;

		let tips = [commit.id];

		let platform = repo
			.rev_walk(tips)
			.sorting(gix::revision::walk::Sorting::ByCommitTime(gix::traverse::commit::simple::CommitTimeOrder::NewestFirst))
			.use_commit_graph(false);

		let walk = platform.all()?;

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

		while let Some(Ok(info)) = self.walk.next() {
			out.push(info.id.into());

			count += 1;

			if count == self.limit {
				break;
			}
		}

		self.visited += count;

		Ok(count)
	}
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
		let mut walk = LogWalkerWithoutFilter::new(&mut repo, 100)?;
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
