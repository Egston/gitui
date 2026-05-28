use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo,
		CommitDetailsComponent, CommitList, Component,
		DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	popups::{FileTreeOpen, InspectCommitOpen},
	queue::{InternalEvent, Queue, StackablePopupOpen},
	strings::{self, order},
	try_or_popup,
	ui::style::{SharedTheme, Theme},
};
use anyhow::Result;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	sync::{
		self, filter_commit_by_search, find_integrating_merge,
		CommitId, LogFilterSearch, LogFilterSearchOptions,
		RepoPathRef,
	},
	AsyncBranchesJob, AsyncCommitFilterJob, AsyncGitNotification,
	AsyncLog, AsyncTags, CommitFilesParams, FetchStatus, LogOrdering,
	ProgressPercent,
};
use crossbeam_channel::Sender;
use crossterm::event::Event;
use indexmap::IndexSet;
use ratatui::{
	layout::{Alignment, Constraint, Direction, Layout, Rect},
	text::Span,
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::{
	collections::HashSet,
	rc::Rc,
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc,
	},
	time::Duration,
};
use sync::CommitTags;

struct LogSearchResult {
	options: LogFilterSearchOptions,
	duration: Duration,
}

//TODO: deserves its own component
enum LogSearch {
	Off,
	Searching(
		AsyncSingleJob<AsyncCommitFilterJob>,
		LogFilterSearchOptions,
		Option<ProgressPercent>,
		Arc<AtomicBool>,
	),
	Results(LogSearchResult),
}

///
#[allow(clippy::struct_excessive_bools)]
pub struct Revlog {
	repo: RepoPathRef,
	commit_details: CommitDetailsComponent,
	list: CommitList,
	git_log: AsyncLog,
	search: LogSearch,
	git_tags: AsyncTags,
	git_local_branches: AsyncSingleJob<AsyncBranchesJob>,
	git_remote_branches: AsyncSingleJob<AsyncBranchesJob>,
	queue: Queue,
	visible: bool,
	key_config: SharedKeyConfig,
	sender: Sender<AsyncGitNotification>,
	theme: SharedTheme,
	first_parent_mode: bool,
	topo_order: bool,
	pending_selection: Option<CommitId>,
	/// True from the moment a mode-change toggle is dispatched until
	/// the next `fetch()` actually starts a new walk. While true,
	/// `update()` skips `extract_items()` so commits the previous
	/// (cancelled) walker is still draining can't leak into the list
	/// under the new badge.
	awaiting_mode_change: bool,
	options: crate::options::SharedOptions,
}

impl Revlog {
	///
	pub fn new(env: &Environment) -> Self {
		let topo_order = env.options.borrow().log_topo_order();
		let mut git_log = AsyncLog::new(
			env.repo.borrow().clone(),
			&env.sender_git,
			None,
		);
		if topo_order {
			// ignore error: head not resolvable yet just means
			// the first fetch starts fresh anyway
			let _ = git_log.set_ordering(LogOrdering::Topo);
		}
		let mut list = CommitList::new(
			env,
			&strings::log_title(&env.key_config),
		);
		list.set_title(Self::compose_title(
			&env.key_config,
			false,
			topo_order,
		));
		Self {
			repo: env.repo.clone(),
			queue: env.queue.clone(),
			commit_details: CommitDetailsComponent::new(env),
			list,
			git_log,
			search: LogSearch::Off,
			git_tags: AsyncTags::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			git_local_branches: AsyncSingleJob::new(
				env.sender_git.clone(),
			),
			git_remote_branches: AsyncSingleJob::new(
				env.sender_git.clone(),
			),
			visible: false,
			key_config: env.key_config.clone(),
			sender: env.sender_git.clone(),
			theme: env.theme.clone(),
			first_parent_mode: false,
			topo_order,
			pending_selection: None,
			awaiting_mode_change: false,
			options: env.options.clone(),
		}
	}

	/// Build the log-pane title with mode badges. Single source of
	/// truth for the badge text — `CommitList` itself only sees the
	/// finished string.
	fn compose_title(
		key_config: &crate::keys::SharedKeyConfig,
		first_parent: bool,
		topo: bool,
	) -> String {
		let base = strings::log_title(key_config);
		let fp = if first_parent {
			" [⊢ first-parent]"
		} else {
			""
		};
		let topo_badge = if topo { " [topo]" } else { "" };
		format!("{base}{fp}{topo_badge}")
	}

	fn refresh_title(&mut self) {
		self.list.set_title(Self::compose_title(
			&self.key_config,
			self.first_parent_mode,
			self.topo_order,
		));
	}

	fn toggle_first_parent(&mut self) -> Result<()> {
		let selected = self.selected_commit();
		self.first_parent_mode = !self.first_parent_mode;

		// choose which commit to re-select once the reload lands
		self.pending_selection = if self.first_parent_mode {
			// entering first-parent: land on the integrating
			// merge of the previously selected commit (itself
			// if already on the spine)
			match selected {
				Some(c) => match find_integrating_merge(
					&self.repo.borrow(),
					c,
				) {
					Ok(found) => found.or(Some(c)),
					Err(e) => {
						log::warn!(
							"find_integrating_merge({c:?}) failed: \
							 {e}; falling back to original commit"
						);
						Some(c)
					}
				},
				None => None,
			}
		} else {
			// leaving first-parent: the commit is in the
			// superset
			selected
		};

		self.git_log.set_first_parent(self.first_parent_mode)?;
		self.refresh_title();
		self.list.clear();
		self.awaiting_mode_change = true;
		// If the walker was idle, `fetch()` starts the new walk right
		// here and `Started` is the *only* transition for this mode
		// change — `update()` would otherwise only see Pending then
		// NoChange and never clear the gate. If the old walker is
		// still draining, `fetch()` returns Pending and the gate
		// stays up until `update()` later observes Started. The
		// AsyncLog-side precondition is pinned by
		// `test_async_log_fetch_started_contract_after_mode_change`.
		if self.git_log.fetch()? == FetchStatus::Started {
			self.awaiting_mode_change = false;
		}
		Ok(())
	}

	fn toggle_topo_order(&mut self) -> Result<()> {
		self.topo_order = !self.topo_order;
		self.pending_selection = self.selected_commit();

		self.git_log.set_ordering(if self.topo_order {
			LogOrdering::Topo
		} else {
			LogOrdering::Time
		})?;
		self.refresh_title();
		self.options
			.borrow_mut()
			.set_log_topo_order(self.topo_order);
		self.list.clear();
		self.awaiting_mode_change = true;
		if self.git_log.fetch()? == FetchStatus::Started {
			self.awaiting_mode_change = false;
		}
		Ok(())
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.git_log.is_pending()
			|| self.is_search_pending()
			|| self.git_tags.is_pending()
			|| self.git_local_branches.is_pending()
			|| self.git_remote_branches.is_pending()
			|| self.commit_details.any_work_pending()
	}

	const fn is_search_pending(&self) -> bool {
		matches!(self.search, LogSearch::Searching(_, _, _, _))
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if self.is_visible() {
			if self.git_log.fetch()? == FetchStatus::Started {
				self.list.clear();
				// fresh walk under the new mode is up; safe to
				// resume pulling streamed commits.
				self.awaiting_mode_change = false;
			}

			if !self.awaiting_mode_change {
				self.list.refresh_extend_data(
					self.git_log.extract_items()?,
				);
			}

			if let Some(target) = self.pending_selection {
				if self.list.select_commit(target).is_ok() {
					self.pending_selection = None;
				} else if !self.git_log.is_pending() {
					// the reload finished and the target never
					// loaded (e.g. an off-spine commit absent from
					// the new walk): give up rather than retry on
					// every tick for the rest of the session.
					self.pending_selection = None;
				}
			}

			let (slice, global_start) = self.list.get_loaded_slice();
			if !slice.is_empty() {
				let mut branch_tips = HashSet::new();
				let mut head_id = None;

				for (id, branches) in self.list.local_branches() {
					branch_tips.insert(*id);
					if head_id.is_none()
						&& branches.iter().any(|b| {
							b.local_details()
								.is_some_and(|d| d.is_head)
						}) {
						head_id = Some(*id);
					}
				}
				branch_tips
					.extend(self.list.remote_branches().keys());

				let stashes = HashSet::new();

				if let Some(rows) = self.git_log.get_graph_rows(
					&slice,
					global_start,
					&branch_tips,
					&stashes,
					head_id.as_ref(),
				) {
					self.list.set_graph_rows(rows);
				}
			}

			self.git_tags.request(Duration::from_secs(3), false)?;

			if self.commit_details.is_visible() {
				let commit = self.selected_commit();
				let tags = self.selected_commit_tags(commit.as_ref());

				self.commit_details.set_commits(
					commit.map(CommitFilesParams::from),
					tags.as_ref(),
				)?;
			}
		}

		Ok(())
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.visible {
			match ev {
				AsyncGitNotification::CommitFiles
				| AsyncGitNotification::Log => self.update()?,
				AsyncGitNotification::CommitFilter => {
					self.update_search_state();
				}
				AsyncGitNotification::Tags => {
					if let Some(tags) = self.git_tags.last()? {
						self.list.set_tags(tags);
						self.update()?;
					}
				}
				AsyncGitNotification::Branches => {
					if let Some(local_branches) =
						self.git_local_branches.take_last()
					{
						if let Some(Ok(local_branches)) =
							local_branches.result()
						{
							self.list
								.set_local_branches(local_branches);
							self.update()?;
						}
					}

					if let Some(remote_branches) =
						self.git_remote_branches.take_last()
					{
						if let Some(Ok(remote_branches)) =
							remote_branches.result()
						{
							self.list
								.set_remote_branches(remote_branches);
							self.update()?;
						}
					}
				}
				_ => (),
			}
		}

		Ok(())
	}

	fn selected_commit(&self) -> Option<CommitId> {
		self.list.selected_entry().map(|e| e.id)
	}

	fn selected_commit_tags(
		&self,
		commit: Option<&CommitId>,
	) -> Option<CommitTags> {
		let tags = self.list.tags();

		commit.and_then(|commit| {
			tags.and_then(|tags| tags.get(commit).cloned())
		})
	}

	///
	pub fn select_commit(&mut self, id: CommitId) -> Result<()> {
		self.list.select_commit(id)
	}

	fn revert_commit(&self) -> Result<()> {
		if let Some(c) = self.selected_commit() {
			sync::revert_commit(&self.repo.borrow(), c)?;
			self.queue.push(InternalEvent::TabSwitchStatus);
		}

		Ok(())
	}

	fn inspect_commit(&self) {
		if let Some(commit_id) = self.selected_commit() {
			let tags =
				self.selected_commit_tags(Some(commit_id).as_ref());
			self.queue.push(InternalEvent::OpenPopup(
				StackablePopupOpen::InspectCommit(
					InspectCommitOpen::new_with_tags(commit_id, tags),
				),
			));
		}
	}

	pub fn search(&mut self, options: LogFilterSearchOptions) {
		if !self.can_start_search() {
			return;
		}

		if matches!(
			self.search,
			LogSearch::Off | LogSearch::Results(_)
		) {
			log::info!("start search: {options:?}");

			let filter = filter_commit_by_search(
				LogFilterSearch::new(options.clone()),
			);

			let cancellation_flag = Arc::new(AtomicBool::new(false));

			let job = AsyncSingleJob::new(self.sender.clone());
			job.spawn(AsyncCommitFilterJob::new(
				self.repo.borrow().clone(),
				self.list.copy_items(),
				filter,
				Arc::clone(&cancellation_flag),
			));

			self.search = LogSearch::Searching(
				job,
				options,
				None,
				Arc::clone(&cancellation_flag),
			);

			self.list.set_highlighting(None);
		}
	}

	fn cancel_search(&mut self) -> bool {
		if let LogSearch::Searching(_, _, _, cancellation_flag) =
			&self.search
		{
			cancellation_flag.store(true, Ordering::Relaxed);
			self.list.set_highlighting(None);
			return true;
		}

		false
	}

	fn update_search_state(&mut self) {
		match &mut self.search {
			LogSearch::Off | LogSearch::Results(_) => (),
			LogSearch::Searching(
				search,
				options,
				progress,
				cancel,
			) => {
				if search.is_pending() {
					//update progress
					*progress = search.progress();
				} else if let Some(search) = search
					.take_last()
					.and_then(|search| search.result())
				{
					match search {
						Ok(search) => {
							let was_aborted =
								cancel.load(Ordering::Relaxed);

							self.search = if was_aborted {
								LogSearch::Off
							} else {
								self.list.set_highlighting(Some(
									Rc::new(
										search
											.result
											.into_iter()
											.collect::<IndexSet<_>>(),
									),
								));

								LogSearch::Results(LogSearchResult {
									options: options.clone(),
									duration: search.duration,
								})
							};
						}
						Err(err) => {
							self.queue.push(
								InternalEvent::ShowErrorMsg(format!(
									"search error: {err}",
								)),
							);

							self.search = LogSearch::Off;
						}
					}
				}
			}
		}
	}

	const fn is_in_search_mode(&self) -> bool {
		!matches!(self.search, LogSearch::Off)
	}

	fn draw_search(&self, f: &mut Frame, area: Rect) {
		let (text, title) = match &self.search {
			LogSearch::Searching(_, options, progress, _) => (
				format!("'{}'", options.search_pattern.clone()),
				format!(
					"({}%)",
					progress
						.map(|progress| progress.progress)
						.unwrap_or_default()
				),
			),
			LogSearch::Results(results) => {
				let info = self.list.highlighted_selection_info();

				(
					format!(
						"'{}' (duration: {:?})",
						results.options.search_pattern.clone(),
						results.duration,
					),
					format!(
						"({}/{})",
						(info.0 + 1).min(info.1),
						info.1
					),
				)
			}
			LogSearch::Off => (String::new(), String::new()),
		};

		f.render_widget(
			Paragraph::new(text)
				.block(
					Block::default()
						.title(Span::styled(
							format!(
								"{} {}",
								strings::POPUP_TITLE_LOG_SEARCH,
								title
							),
							self.theme.title(true),
						))
						.borders(Borders::ALL)
						.border_style(Theme::attention_block()),
				)
				.alignment(Alignment::Left),
			area,
		);
	}

	const fn can_close_search(&self) -> bool {
		self.is_in_search_mode() && !self.is_search_pending()
	}

	fn can_start_search(&self) -> bool {
		!self.git_log.is_pending() && !self.is_search_pending()
	}
}

impl DrawableComponent for Revlog {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		let area = if self.is_in_search_mode() {
			Layout::default()
				.direction(Direction::Vertical)
				.constraints(
					[Constraint::Min(1), Constraint::Length(3)]
						.as_ref(),
				)
				.split(area)
		} else {
			Rc::new([area])
		};

		let chunks = Layout::default()
			.direction(Direction::Horizontal)
			.constraints(
				[
					Constraint::Percentage(60),
					Constraint::Percentage(40),
				]
				.as_ref(),
			)
			.split(area[0]);

		if self.commit_details.is_visible() {
			self.list.draw(f, chunks[0])?;
			self.commit_details.draw(f, chunks[1])?;
		} else {
			self.list.draw(f, area[0])?;
		}

		if self.is_in_search_mode() {
			self.draw_search(f, area[1]);
		}

		Ok(())
	}
}

impl Component for Revlog {
	//TODO: cleanup
	#[allow(clippy::too_many_lines)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.visible {
			let event_used = self.list.event(ev)?;

			if event_used.is_consumed() {
				self.update()?;
				return Ok(EventState::Consumed);
			} else if let Event::Key(k) = ev {
				if key_match(k, self.key_config.keys.enter) {
					self.commit_details.toggle_visible()?;
					self.update()?;
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.exit_popup,
				) {
					if self.is_search_pending() {
						self.cancel_search();
					} else if self.can_close_search() {
						self.list.set_highlighting(None);
						self.search = LogSearch::Off;
					}
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.copy) {
					try_or_popup!(
						self,
						strings::POPUP_FAIL_COPY,
						self.list.copy_commit_hash()
					);
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.push) {
					self.queue.push(InternalEvent::PushTags);
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.log_tag_commit,
				) {
					return self.selected_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue
								.push(InternalEvent::TagCommit(id));
							Ok(EventState::Consumed)
						},
					);
				} else if key_match(
					k,
					self.key_config.keys.move_right,
				) && self.commit_details.is_visible()
				{
					self.inspect_commit();
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.select_branch,
				) && !self.is_search_pending()
				{
					self.queue.push(InternalEvent::SelectBranch);
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.status_reset_item,
				) && !self.is_search_pending()
				{
					try_or_popup!(
						self,
						"revert error:",
						self.revert_commit()
					);

					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.open_file_tree,
				) && !self.is_search_pending()
				{
					return self.selected_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue.push(
								InternalEvent::OpenPopup(
									StackablePopupOpen::FileTree(
										FileTreeOpen::new(id),
									),
								),
							);
							Ok(EventState::Consumed)
						},
					);
				} else if key_match(k, self.key_config.keys.tags) {
					self.queue.push(InternalEvent::Tags);
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.log_reset_commit,
				) && !self.is_search_pending()
				{
					return self.selected_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue.push(
								InternalEvent::OpenResetPopup(id),
							);
							Ok(EventState::Consumed)
						},
					);
				} else if key_match(
					k,
					self.key_config.keys.log_reword_commit,
				) && !self.is_search_pending()
				{
					return self.selected_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue.push(
								InternalEvent::RewordCommit(id),
							);
							Ok(EventState::Consumed)
						},
					);
				} else if key_match(k, self.key_config.keys.log_find)
					&& self.can_start_search()
				{
					self.queue
						.push(InternalEvent::OpenLogSearchPopup);
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.compare_commits,
				) && self.list.marked_count() > 0
					&& !self.is_search_pending()
				{
					if self.list.marked_count() == 1 {
						// compare against head
						self.queue.push(InternalEvent::OpenPopup(
							StackablePopupOpen::CompareCommits(
								InspectCommitOpen::new(
									self.list.marked_commits()[0],
								),
							),
						));
						return Ok(EventState::Consumed);
					} else if self.list.marked_count() == 2 {
						//compare two marked commits
						let marked = self.list.marked_commits();
						self.queue.push(InternalEvent::OpenPopup(
							StackablePopupOpen::CompareCommits(
								InspectCommitOpen {
									commit_id: marked[0],
									compare_id: Some(marked[1]),
									tags: None,
								},
							),
						));
						return Ok(EventState::Consumed);
					}
				} else if key_match(
					k,
					self.key_config.keys.log_first_parent,
				) && !self.is_search_pending()
				{
					self.toggle_first_parent()?;
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.log_ordering,
				) && !self.is_search_pending()
				{
					self.toggle_topo_order()?;
					return Ok(EventState::Consumed);
				}
			}
		}

		Ok(EventState::NotConsumed)
	}

	#[allow(clippy::too_many_lines)]
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			self.list.commands(out, force_all);
		}

		out.push(
			CommandInfo::new(
				strings::commands::log_close_search(&self.key_config),
				true,
				(self.visible
					&& (self.can_close_search()
						|| self.is_search_pending()))
					|| force_all,
			)
			.order(order::PRIORITY),
		);

		out.push(CommandInfo::new(
			strings::commands::log_details_toggle(&self.key_config),
			true,
			self.visible,
		));

		out.push(CommandInfo::new(
			strings::commands::log_first_parent(&self.key_config),
			true,
			self.visible,
		));

		out.push(
			CommandInfo::new(
				strings::commands::log_ordering(&self.key_config),
				true,
				self.visible,
			)
			.order(order::RARE_ACTION),
		);

		out.push(CommandInfo::new(
			strings::commands::commit_details_open(&self.key_config),
			true,
			(self.visible && self.commit_details.is_visible())
				|| force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::open_branch_select_popup(
				&self.key_config,
			),
			true,
			(self.visible && !self.is_search_pending()) || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::compare_with_head(&self.key_config),
			self.list.marked_count() == 1,
			(self.visible
				&& !self.is_search_pending()
				&& self.list.marked_count() <= 1)
				|| force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::compare_commits(&self.key_config),
			true,
			(self.visible
				&& !self.is_search_pending()
				&& self.list.marked_count() == 2)
				|| force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::copy_hash(&self.key_config),
			self.selected_commit().is_some(),
			self.visible || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::log_tag_commit(&self.key_config),
			self.selected_commit().is_some(),
			self.visible || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::log_checkout_commit(&self.key_config),
			self.selected_commit().is_some(),
			self.visible || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::open_tags_popup(&self.key_config),
			true,
			self.visible || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::push_tags(&self.key_config),
			true,
			self.visible || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::inspect_file_tree(&self.key_config),
			self.selected_commit().is_some(),
			(self.visible && !self.is_search_pending()) || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::revert_commit(&self.key_config),
			self.selected_commit().is_some(),
			(self.visible && !self.is_search_pending()) || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::log_reset_commit(&self.key_config),
			self.selected_commit().is_some(),
			(self.visible && !self.is_search_pending()) || force_all,
		));
		out.push(CommandInfo::new(
			strings::commands::log_reword_commit(&self.key_config),
			self.selected_commit().is_some(),
			(self.visible && !self.is_search_pending()) || force_all,
		));
		out.push(CommandInfo::new(
			strings::commands::log_find_commit(&self.key_config),
			self.can_start_search(),
			self.visible || force_all,
		));

		visibility_blocking(self)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
		self.git_log.set_background();
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;

		self.git_local_branches.spawn(AsyncBranchesJob::new(
			self.repo.borrow().clone(),
			true,
		));

		self.git_remote_branches.spawn(AsyncBranchesJob::new(
			self.repo.borrow().clone(),
			false,
		));

		self.update()?;

		Ok(())
	}
}
