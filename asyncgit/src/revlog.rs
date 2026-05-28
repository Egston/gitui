use crate::{
	error::Result,
	graph::{GraphRow, GraphWalker},
	sync::{
		get_commits_info, gix_repo, repo, CommitId, LogWalker,
		LogWalkerWithoutFilter, RepoPath, SharedCommitFilterFn,
	},
	AsyncGitNotification, Error,
};
use crossbeam_channel::Sender;
use scopetime::scope_time;
use std::{
	collections::HashSet,
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc, Mutex,
	},
	thread,
	time::{Duration, Instant},
};

/// Ordering for the commit log walk.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LogOrdering {
	/// Commit-timestamp order (default, historical behaviour).
	#[default]
	Time,
	/// `git log --topo-order`: no parent shown before all its children,
	/// and separate lines of history are not intermixed.
	Topo,
}

///
#[derive(PartialEq, Eq, Debug)]
pub enum FetchStatus {
	/// previous fetch still running
	Pending,
	/// no change expected
	NoChange,
	/// new walk was started
	Started,
}

///
pub struct AsyncLogResult {
	///
	pub commits: Vec<CommitId>,
	///
	pub duration: Duration,
}
///
pub struct AsyncLog {
	current: Arc<Mutex<AsyncLogResult>>,
	current_head: Arc<Mutex<Option<CommitId>>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicBool>,
	background: Arc<AtomicBool>,
	filter: Option<SharedCommitFilterFn>,
	partial_extract: AtomicBool,
	repo: RepoPath,
	graph_walker: Arc<Mutex<GraphWalker>>,
	first_parent: bool,
	ordering: LogOrdering,
	/// Set when a mode change asks the in-flight walker to exit
	/// early. The worker reads this at the top of its inner loop
	/// and bails. Reset to `false` before each new walk.
	cancel: Arc<AtomicBool>,
}

static LIMIT_COUNT: usize = 3000;
static SLEEP_FOREGROUND: Duration = Duration::from_millis(2);
static SLEEP_BACKGROUND: Duration = Duration::from_secs(1);

impl AsyncLog {
	///
	pub fn new(
		repo: RepoPath,
		sender: &Sender<AsyncGitNotification>,
		filter: Option<SharedCommitFilterFn>,
	) -> Self {
		Self {
			repo,
			current: Arc::new(Mutex::new(AsyncLogResult {
				commits: Vec::new(),
				duration: Duration::default(),
			})),
			current_head: Arc::new(Mutex::new(None)),
			sender: sender.clone(),
			pending: Arc::new(AtomicBool::new(false)),
			background: Arc::new(AtomicBool::new(false)),
			filter,
			partial_extract: AtomicBool::new(false),
			graph_walker: Arc::new(Mutex::new(GraphWalker::new())),
			first_parent: false,
			ordering: LogOrdering::default(),
			cancel: Arc::new(AtomicBool::new(false)),
		}
	}

	pub fn get_graph_rows(
		&self,
		commit_slice: &[CommitId],
		global_start: usize,
		branch_tips: &HashSet<CommitId>,
		stashes: &HashSet<CommitId>,
		head_id: Option<&CommitId>,
	) -> Option<Vec<GraphRow>> {
		let walker_guard = self.graph_walker.lock().ok()?;
		let needed_end = global_start + commit_slice.len();
		if walker_guard.buffer.deltas.len() < needed_end {
			return None;
		}

		Some(walker_guard.compute_rows(
			commit_slice,
			global_start,
			branch_tips,
			stashes,
			head_id,
		))
	}

	///
	pub fn count(&self) -> Result<usize> {
		Ok(self.current.lock()?.commits.len())
	}

	///
	pub fn get_slice(
		&self,
		start_index: usize,
		amount: usize,
	) -> Result<Vec<CommitId>> {
		if self.partial_extract.load(Ordering::Relaxed) {
			return Err(Error::Generic(String::from("Faulty usage of AsyncLog: Cannot partially extract items and rely on get_items slice to still work!")));
		}

		let list = &self.current.lock()?.commits;
		let list_len = list.len();
		let min = start_index.min(list_len);
		let max = min + amount;
		let max = max.min(list_len);
		Ok(list[min..max].to_vec())
	}

	///
	pub fn get_items(&self) -> Result<Vec<CommitId>> {
		if self.partial_extract.load(Ordering::Relaxed) {
			return Err(Error::Generic(String::from("Faulty usage of AsyncLog: Cannot partially extract items and rely on get_items slice to still work!")));
		}

		let list = &self.current.lock()?.commits;
		Ok(list.clone())
	}

	///
	pub fn extract_items(&self) -> Result<Vec<CommitId>> {
		self.partial_extract.store(true, Ordering::Relaxed);
		let list = &mut self.current.lock()?.commits;
		let result = list.clone();
		list.clear();
		Ok(result)
	}

	///
	pub fn get_last_duration(&self) -> Result<Duration> {
		Ok(self.current.lock()?.duration)
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed)
	}

	///
	pub fn set_background(&self) {
		self.background.store(true, Ordering::Relaxed);
	}

	/// Toggle first-parent traversal. Signals any in-flight walker to
	/// exit early and resets the cached head so the next `fetch()`
	/// starts a fresh walk in the new mode.
	pub fn set_first_parent(&mut self, v: bool) -> Result<()> {
		if self.first_parent != v {
			self.first_parent = v;
			self.cancel.store(true, Ordering::Relaxed);
			*self.current_head.lock()? = None;
		}
		Ok(())
	}

	/// Set the commit-log ordering. Signals any in-flight walker to
	/// exit early and resets the cached head so the next `fetch()`
	/// starts a fresh walk in the new ordering.
	pub fn set_ordering(&mut self, v: LogOrdering) -> Result<()> {
		if self.ordering != v {
			self.ordering = v;
			self.cancel.store(true, Ordering::Relaxed);
			*self.current_head.lock()? = None;
		}
		Ok(())
	}

	///
	fn current_head(&self) -> Result<Option<CommitId>> {
		Ok(*self.current_head.lock()?)
	}

	///
	fn head_changed(&self) -> Result<bool> {
		if let Ok(head) = repo(&self.repo)?.head() {
			return Ok(
				head.target() != self.current_head()?.map(Into::into)
			);
		}
		Ok(false)
	}

	///
	pub fn fetch(&self) -> Result<FetchStatus> {
		self.background.store(false, Ordering::Relaxed);

		if self.is_pending() {
			return Ok(FetchStatus::Pending);
		}

		if !self.head_changed()? {
			return Ok(FetchStatus::NoChange);
		}

		self.pending.store(true, Ordering::Relaxed);
		self.cancel.store(false, Ordering::Relaxed);

		self.clear()?;

		let arc_current = Arc::clone(&self.current);
		let sender = self.sender.clone();
		let arc_pending = Arc::clone(&self.pending);
		let arc_background = Arc::clone(&self.background);
		let arc_graph_walker = Arc::clone(&self.graph_walker);
		let arc_cancel = Arc::clone(&self.cancel);
		let filter = self.filter.clone();
		let first_parent = self.first_parent;
		let ordering = self.ordering;
		let repo_path = self.repo.clone();

		if let Ok(head) = repo(&self.repo)?.head() {
			*self.current_head.lock()? =
				head.target().map(CommitId::new);
		}

		rayon_core::spawn(move || {
			scope_time!("async::revlog");

			Self::fetch_helper(
				&repo_path,
				&arc_current,
				&arc_background,
				&arc_cancel,
				&sender,
				&arc_graph_walker,
				filter,
				first_parent,
				ordering,
			)
			.expect("failed to fetch");

			arc_pending.store(false, Ordering::Relaxed);

			Self::notify(&sender);
		});

		Ok(FetchStatus::Started)
	}

	#[allow(clippy::too_many_arguments)]
	fn fetch_helper(
		repo_path: &RepoPath,
		arc_current: &Arc<Mutex<AsyncLogResult>>,
		arc_background: &Arc<AtomicBool>,
		arc_cancel: &Arc<AtomicBool>,
		sender: &Sender<AsyncGitNotification>,
		arc_graph_walker: &Arc<Mutex<GraphWalker>>,
		filter: Option<SharedCommitFilterFn>,
		first_parent: bool,
		ordering: LogOrdering,
	) -> Result<()> {
		if filter.is_some() || first_parent {
			// git2 path: first-parent output is linear, so ordering is
			// irrelevant here and intentionally ignored.
			Self::fetch_helper_with_filter(
				repo_path,
				arc_current,
				arc_background,
				arc_cancel,
				sender,
				arc_graph_walker,
				filter,
				first_parent,
			)
		} else {
			Self::fetch_helper_without_filter(
				repo_path,
				arc_current,
				arc_background,
				arc_cancel,
				sender,
				arc_graph_walker,
				ordering,
			)
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn fetch_helper_with_filter(
		repo_path: &RepoPath,
		arc_current: &Arc<Mutex<AsyncLogResult>>,
		arc_background: &Arc<AtomicBool>,
		arc_cancel: &Arc<AtomicBool>,
		sender: &Sender<AsyncGitNotification>,
		arc_graph_walker: &Arc<Mutex<GraphWalker>>,
		filter: Option<SharedCommitFilterFn>,
		first_parent: bool,
	) -> Result<()> {
		let start_time = Instant::now();

		let mut entries = vec![CommitId::default(); LIMIT_COUNT];
		entries.resize(0, CommitId::default());

		let r = repo(repo_path)?;
		let mut walker = LogWalker::new(&r, LIMIT_COUNT)?
			.filter(filter)
			.first_parent(first_parent);

		loop {
			if arc_cancel.load(Ordering::Relaxed) {
				break;
			}
			entries.clear();
			let read = walker.read(&mut entries)?;

			{
				let infos = get_commits_info(
					repo_path,
					&entries[0..read],
					1000,
				)?;
				let mut gw = arc_graph_walker.lock()?;
				for info in &infos {
					gw.process(info, first_parent);
				}
			}

			let mut current = arc_current.lock()?;
			current.commits.extend(entries.iter());
			current.duration = start_time.elapsed();

			if read == 0 {
				break;
			}
			Self::notify(sender);

			let sleep_duration =
				if arc_background.load(Ordering::Relaxed) {
					SLEEP_BACKGROUND
				} else {
					SLEEP_FOREGROUND
				};

			thread::sleep(sleep_duration);
		}

		log::trace!("revlog visited: {}", walker.visited());

		Ok(())
	}

	#[allow(clippy::too_many_arguments)]
	fn fetch_helper_without_filter(
		repo_path: &RepoPath,
		arc_current: &Arc<Mutex<AsyncLogResult>>,
		arc_background: &Arc<AtomicBool>,
		arc_cancel: &Arc<AtomicBool>,
		sender: &Sender<AsyncGitNotification>,
		arc_graph_walker: &Arc<Mutex<GraphWalker>>,
		ordering: LogOrdering,
	) -> Result<()> {
		let start_time = Instant::now();

		let mut entries = vec![CommitId::default(); LIMIT_COUNT];
		entries.resize(0, CommitId::default());

		let mut repo: gix::Repository = gix_repo(repo_path)?;

		// gix honours GIT_DIR / discovery; reuse the resolved gitdir
		// for the git2 side so both walkers target the same repo even
		// when `repo_path` is empty and only the environment locates it.
		let resolved_path = repo.workdir().map_or_else(
			|| RepoPath::Path(repo.git_dir().to_path_buf()),
			|wd| RepoPath::Workdir {
				gitdir: repo.git_dir().to_path_buf(),
				workdir: wd.to_path_buf(),
			},
		);

		let mut walker = LogWalkerWithoutFilter::new(
			&mut repo,
			LIMIT_COUNT,
			ordering,
		)?;

		loop {
			if arc_cancel.load(Ordering::Relaxed) {
				break;
			}
			entries.clear();
			let read = walker.read(&mut entries)?;

			{
				let infos = get_commits_info(
					&resolved_path,
					&entries[0..read],
					1000,
				)?;
				let mut gw = arc_graph_walker.lock()?;
				for info in &infos {
					// gix fast path is never used in first-parent
					// mode (fetch routes through the git2 walker)
					gw.process(info, false);
				}
			}

			let mut current = arc_current.lock()?;
			current.commits.extend(entries.iter());
			current.duration = start_time.elapsed();

			if read == 0 {
				break;
			}
			Self::notify(sender);

			let sleep_duration =
				if arc_background.load(Ordering::Relaxed) {
					SLEEP_BACKGROUND
				} else {
					SLEEP_FOREGROUND
				};

			thread::sleep(sleep_duration);
		}

		log::trace!("revlog visited: {}", walker.visited());

		Ok(())
	}

	fn clear(&self) -> Result<()> {
		self.current.lock()?.commits.clear();
		*self.current_head.lock()? = None;
		self.partial_extract.store(false, Ordering::Relaxed);
		*self.graph_walker.lock()? = GraphWalker::new();
		Ok(())
	}

	fn notify(sender: &Sender<AsyncGitNotification>) {
		sender
			.send(AsyncGitNotification::Log)
			.expect("error sending");
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::AtomicBool;
	use std::sync::{Arc, Mutex};
	use std::time::Duration;

	use crossbeam_channel::unbounded;
	use serial_test::serial;
	use tempfile::TempDir;

	use crate::graph::GraphWalker;
	use crate::sync::tests::{
		debug_cmd_print, repo_init, repo_init_empty,
	};
	use crate::sync::{commit, stage_add_file, CommitId, RepoPath};
	use crate::AsyncLog;

	use super::{AsyncLogResult, LogOrdering};

	use std::fs::File;
	use std::io::Write;
	use std::path::Path;

	#[test]
	#[serial]
	fn test_smoke_in_subdir() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();

		let (tx_git, _rx_git) = unbounded();

		debug_cmd_print(&repo_path, "mkdir subdir");

		let subdir = repo.path().parent().unwrap().join("subdir");
		let subdir_path: RepoPath =
			subdir.as_os_str().to_str().unwrap().into();

		let arc_current = Arc::new(Mutex::new(AsyncLogResult {
			commits: Vec::new(),
			duration: Duration::default(),
		}));
		let arc_background = Arc::new(AtomicBool::new(false));
		let arc_cancel = Arc::new(AtomicBool::new(false));
		let arc_graph_walker =
			Arc::new(Mutex::new(GraphWalker::new()));

		let result = AsyncLog::fetch_helper_without_filter(
			&subdir_path,
			&arc_current,
			&arc_background,
			&arc_cancel,
			&tx_git,
			&arc_graph_walker,
			LogOrdering::Time,
		);

		assert_eq!(result.unwrap(), ());
	}

	#[test]
	#[serial]
	fn test_env_variables() {
		let (_td, repo) = repo_init().unwrap();
		let git_dir = repo.path();

		let (tx_git, _rx_git) = unbounded();

		let empty_dir = TempDir::new().unwrap();
		let empty_path: RepoPath =
			empty_dir.path().to_str().unwrap().into();

		let arc_current = Arc::new(Mutex::new(AsyncLogResult {
			commits: Vec::new(),
			duration: Duration::default(),
		}));
		let arc_background = Arc::new(AtomicBool::new(false));
		let arc_cancel = Arc::new(AtomicBool::new(false));
		let arc_graph_walker =
			Arc::new(Mutex::new(GraphWalker::new()));

		std::env::set_var("GIT_DIR", git_dir);

		let result = AsyncLog::fetch_helper_without_filter(
			// We pass an empty path, thus testing whether `GIT_DIR`, set above, is taken into account.
			&empty_path,
			&arc_current,
			&arc_background,
			&arc_cancel,
			&tx_git,
			&arc_graph_walker,
			LogOrdering::Time,
		);

		std::env::remove_var("GIT_DIR");

		assert_eq!(result.unwrap(), ());
	}

	#[test]
	#[serial]
	fn test_fetch_first_parent_excludes_branch_commits() {
		let file_path = Path::new("foo");
		let branch_file = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();

		// commit A on master
		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"a")
			.unwrap();
		stage_add_file(&repo_path, file_path).unwrap();
		let commit_a = commit(&repo_path, "commit A").unwrap();

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

		File::create(root.join(branch_file))
			.unwrap()
			.write_all(b"b")
			.unwrap();
		stage_add_file(&repo_path, branch_file).unwrap();
		let commit_b =
			commit(&repo_path, "commit B on feat").unwrap();

		// back to master, commit C
		repo.set_head("refs/heads/master").unwrap();
		repo.checkout_head(Some(
			git2::build::CheckoutBuilder::default().force(),
		))
		.unwrap();
		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"c")
			.unwrap();
		stage_add_file(&repo_path, file_path).unwrap();
		let commit_c =
			commit(&repo_path, "commit C on main").unwrap();

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

		// first-parent walk: must NOT include B, must include A + M
		let arc_current = Arc::new(Mutex::new(AsyncLogResult {
			commits: Vec::new(),
			duration: Duration::default(),
		}));
		let arc_background = Arc::new(AtomicBool::new(false));
		let arc_cancel = Arc::new(AtomicBool::new(false));
		let arc_graph_walker =
			Arc::new(Mutex::new(GraphWalker::new()));
		let (tx_git, _rx_git) = unbounded();

		AsyncLog::fetch_helper(
			&repo_path,
			&arc_current,
			&arc_background,
			&arc_cancel,
			&tx_git,
			&arc_graph_walker,
			None,
			true,
			LogOrdering::Time,
		)
		.unwrap();

		let commits = arc_current.lock().unwrap().commits.clone();
		assert!(
			!commits.contains(&commit_b),
			"first-parent walk must NOT include branch commit B"
		);
		assert!(
			commits.contains(&commit_a),
			"first-parent walk must include commit A"
		);
		assert!(
			commits.contains(&merge_commit_id),
			"first-parent walk must include merge commit M"
		);

		// control: full walk (first_parent=false) includes B
		let arc_current = Arc::new(Mutex::new(AsyncLogResult {
			commits: Vec::new(),
			duration: Duration::default(),
		}));
		let arc_background = Arc::new(AtomicBool::new(false));
		let arc_cancel = Arc::new(AtomicBool::new(false));
		let arc_graph_walker =
			Arc::new(Mutex::new(GraphWalker::new()));
		let (tx_git, _rx_git) = unbounded();

		AsyncLog::fetch_helper(
			&repo_path,
			&arc_current,
			&arc_background,
			&arc_cancel,
			&tx_git,
			&arc_graph_walker,
			None,
			false,
			LogOrdering::Time,
		)
		.unwrap();

		let commits = arc_current.lock().unwrap().commits.clone();
		assert!(
			commits.contains(&commit_b),
			"full walk must include branch commit B"
		);
	}
}
