//! Lathe-owned git-store extensions: GitKraken-style git operations
//! (cherry-pick, revert, merge, rebase, tags, reflog, stash-by-message, LFS,
//! submodules), detached checkout, file history, and the destructive-action
//! undo log.
//!
//! This is a child module of `git_store`, so it can reach the parent's private
//! `Repository`/`GitStore` fields and methods (`send_job`, `active_jobs`,
//! `undo_log`, `repositories`, ...). That lets these inherent methods move out
//! of the upstream-owned `git_store.rs` file with no behavior or visibility
//! change; upstream `git_store.rs` keeps only the struct/field/event definitions
//! and the proto registration these hook into.

use super::*;

impl super::GitStore {
    pub fn file_history(
        &self,
        repo: &Entity<Repository>,
        path: RepoPath,
        cx: &mut App,
    ) -> Task<Result<git::repository::FileHistory>> {
        let rx = repo.update(cx, |repo, _| repo.file_history(path));

        cx.spawn(|_: &mut AsyncApp| async move { rx.await? })
    }

    pub fn file_history_paginated(
        &self,
        repo: &Entity<Repository>,
        path: RepoPath,
        skip: usize,
        limit: Option<usize>,
        cx: &mut App,
    ) -> Task<Result<git::repository::FileHistory>> {
        let rx = repo.update(cx, |repo, _| repo.file_history_paginated(path, skip, limit));

        cx.spawn(|_: &mut AsyncApp| async move { rx.await? })
    }

    pub fn undo_log(&self) -> &undo_log::GitUndoLog {
        &self.undo_log
    }

    /// Record a destructive operation in the undo log and emit a change event.
    /// Callers should invoke this only after the underlying git operation has
    /// succeeded — recording before success would leave a phantom entry that
    /// cannot be undone.
    pub fn record_undo(
        &mut self,
        repository_id: RepositoryId,
        label: impl Into<SharedString>,
        action: undo_log::UndoAction,
        cx: &mut Context<Self>,
    ) -> undo_log::UndoId {
        let id = self.undo_log.record(repository_id, label, action);
        cx.emit(GitStoreEvent::UndoLogChanged(repository_id));
        id
    }

    /// Pop a specific undo entry from the log and run its undo action.
    pub fn undo(&mut self, undo_id: undo_log::UndoId, cx: &mut Context<Self>) -> Task<Result<()>> {
        let Some(entry) = self.undo_log.take(undo_id) else {
            return Task::ready(Err(anyhow!("undo entry not found")));
        };
        cx.emit(GitStoreEvent::UndoLogChanged(entry.repository_id));

        let Some(repository) = self.repositories.get(&entry.repository_id).cloned() else {
            return Task::ready(Err(anyhow!("repository for undo entry is gone")));
        };
        let action = entry.action;
        cx.spawn(async move |_, cx| {
            let receiver = repository.update(cx, |repo, cx| match action {
                undo_log::UndoAction::RestoreBranchTip {
                    branch: _,
                    sha,
                    is_current,
                } if is_current => {
                    // Cannot force-update the current branch — use reset --hard
                    // to point HEAD back at the prior tip.
                    repo.reset(sha, ResetMode::Hard, cx)
                }
                undo_log::UndoAction::RestoreBranchTip { branch, sha, .. } => {
                    repo.branch_force_update(branch, sha)
                }
                undo_log::UndoAction::DeleteTag { name } => repo.tag_delete(name),
                undo_log::UndoAction::RecreateBranch { name, sha } => {
                    repo.branch_force_update(name, sha)
                }
                undo_log::UndoAction::RenameBranch { from, to } => repo.rename_branch(from, to),
                undo_log::UndoAction::PopStashByMessage { message } => {
                    // stash_pop_by_message returns Task<Result<()>>; bridge to
                    // the Receiver shape every other arm produces.
                    let task = repo.stash_pop_by_message(message, cx);
                    let (tx, rx) = futures::channel::oneshot::channel();
                    cx.foreground_executor()
                        .spawn(async move {
                            let _ = tx.send(task.await);
                        })
                        .detach();
                    rx
                }
            });
            receiver.await?
        })
    }

    /// Undo the most recent destructive operation on the given repository.
    pub fn undo_latest(
        &mut self,
        repository_id: RepositoryId,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(latest) = self.undo_log.latest_for(repository_id) else {
            return Task::ready(Err(anyhow!("nothing to undo")));
        };
        let id = latest.id;
        self.undo(id, cx)
    }

    pub(super) async fn handle_file_history(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitFileHistory>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitFileHistoryResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let path = RepoPath::from_proto(&envelope.payload.path)?;
        let skip = envelope.payload.skip as usize;
        let limit = envelope.payload.limit.map(|l| l as usize);

        let file_history = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.file_history_paginated(path, skip, limit)
            })
            .await??;

        Ok(proto::GitFileHistoryResponse {
            entries: file_history
                .entries
                .into_iter()
                .map(|entry| proto::FileHistoryEntry {
                    sha: entry.sha.to_string(),
                    subject: entry.subject.to_string(),
                    message: entry.message.to_string(),
                    commit_timestamp: entry.commit_timestamp,
                    author_name: entry.author_name.to_string(),
                    author_email: entry.author_email.to_string(),
                })
                .collect(),
            path: file_history.path.as_unix_str().to_owned(),
        })
    }
}

impl super::Repository {
    pub fn file_history(
        &mut self,
        path: RepoPath,
    ) -> oneshot::Receiver<Result<git::repository::FileHistory>> {
        self.file_history_paginated(path, 0, None)
    }

    pub fn file_history_paginated(
        &mut self,
        path: RepoPath,
        skip: usize,
        limit: Option<usize>,
    ) -> oneshot::Receiver<Result<git::repository::FileHistory>> {
        let id = self.id;
        self.send_job("file_history", None, move |git_repo, _cx| async move {
            match git_repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.file_history_paginated(path, skip, limit).await
                }
                RepositoryState::Remote(RemoteRepositoryState { client, project_id }) => {
                    let response = client
                        .request(proto::GitFileHistory {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            path: path.as_unix_str().to_owned(),
                            skip: skip as u64,
                            limit: limit.map(|l| l as u64),
                        })
                        .await?;
                    Ok(git::repository::FileHistory {
                        entries: response
                            .entries
                            .into_iter()
                            .map(|entry| git::repository::FileHistoryEntry {
                                sha: entry.sha.into(),
                                subject: entry.subject.into(),
                                message: entry.message.into(),
                                commit_timestamp: entry.commit_timestamp,
                                author_name: entry.author_name.into(),
                                author_email: entry.author_email.into(),
                            })
                            .collect(),
                        path: RepoPath::from_proto(&response.path)?,
                    })
                }
            }
        })
    }

    pub fn stash_entries_with_message(
        &mut self,
        entries: Vec<RepoPath>,
        message: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, _| {
                this.send_job(
                    "stash_paths_with_message",
                    None,
                    move |git_repo, _cx| async move {
                        match git_repo {
                            RepositoryState::Local(LocalRepositoryState {
                                backend,
                                environment,
                                ..
                            }) => {
                                backend
                                    .stash_paths_with_message(entries, message, environment)
                                    .await
                            }
                            RepositoryState::Remote(_) => Err(anyhow!(
                                "stash with message is not supported on remote repositories"
                            )),
                        }
                    },
                )
            })?
            .await??;
            Ok(())
        })
    }

    /// Apply the most recent stash entry whose message matches `message`.
    /// Local-only — see `stash_entries_with_message`.
    pub fn stash_pop_by_message(
        &mut self,
        message: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, _| {
                this.send_job(
                    "stash_pop_by_message",
                    None,
                    move |git_repo, _cx| async move {
                        match git_repo {
                            RepositoryState::Local(LocalRepositoryState {
                                backend,
                                environment,
                                ..
                            }) => backend.stash_pop_by_message(message, environment).await,
                            RepositoryState::Remote(_) => Err(anyhow!(
                                "stash pop by message is not supported on remote repositories"
                            )),
                        }
                    },
                )
            })?
            .await??;
            Ok(())
        })
    }

    pub fn submodule_update(&mut self) -> oneshot::Receiver<anyhow::Result<()>> {
        self.send_job(
            "submodule_update",
            Some("git submodule update --init --recursive".into()),
            move |git_repo, _cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => backend.submodule_update(environment).await,
                    RepositoryState::Remote(_) => {
                        bail!("submodule update is not supported on remote repositories")
                    }
                }
            },
        )
    }

    pub fn lfs_fetch(&mut self) -> oneshot::Receiver<anyhow::Result<()>> {
        self.send_job(
            "lfs_fetch",
            Some("git lfs fetch".into()),
            move |git_repo, _cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => backend.lfs_fetch(environment).await,
                    RepositoryState::Remote(_) => {
                        bail!("git lfs fetch is not supported on remote repositories")
                    }
                }
            },
        )
    }

    pub fn lfs_pull(&mut self) -> oneshot::Receiver<anyhow::Result<()>> {
        self.send_job(
            "lfs_pull",
            Some("git lfs pull".into()),
            move |git_repo, _cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => backend.lfs_pull(environment).await,
                    RepositoryState::Remote(_) => {
                        bail!("git lfs pull is not supported on remote repositories")
                    }
                }
            },
        )
    }

    pub fn change_to_commit(&mut self, revision: String) -> oneshot::Receiver<Result<()>> {
        let label: SharedString = format!("git checkout {revision}").into();
        self.send_job(
            "change_to_commit",
            Some(label),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.change_to_commit(revision).await
                    }
                    RepositoryState::Remote(_) => {
                        bail!("detached checkout is not yet supported on remote projects")
                    }
                }
            },
        )
    }

    pub fn cherry_pick(
        &mut self,
        commits: Vec<String>,
        no_commit: bool,
    ) -> oneshot::Receiver<Result<()>> {
        let label = format!("git cherry-pick {}", commits.join(" "));
        self.send_job(
            "cherry_pick",
            Some(label.into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => backend.cherry_pick(commits, no_commit, environment).await,
                    RepositoryState::Remote(_) => {
                        bail!("cherry-pick is not yet supported on remote projects")
                    }
                }
            },
        )
    }

    pub fn revert(
        &mut self,
        commits: Vec<String>,
        no_commit: bool,
        mainline: Option<u32>,
    ) -> oneshot::Receiver<Result<()>> {
        let label = format!("git revert {}", commits.join(" "));
        self.send_job("revert", Some(label.into()), move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState {
                    backend,
                    environment,
                    ..
                }) => {
                    backend
                        .revert(commits, no_commit, mainline, environment)
                        .await
                }
                RepositoryState::Remote(_) => {
                    bail!("revert is not yet supported on remote projects")
                }
            }
        })
    }

    pub fn merge(
        &mut self,
        commit: String,
        options: MergeOptions,
    ) -> oneshot::Receiver<Result<()>> {
        let label = format!("git merge {commit}");
        self.send_job("merge", Some(label.into()), move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState {
                    backend,
                    environment,
                    ..
                }) => backend.merge(commit, options, environment).await,
                RepositoryState::Remote(_) => {
                    bail!("merge is not yet supported on remote projects")
                }
            }
        })
    }

    pub fn commits_in_range(
        &mut self,
        range: String,
    ) -> oneshot::Receiver<Result<Vec<CommitSummary>>> {
        let label: SharedString = format!("git log {range}").into();
        self.send_job(
            "commits_in_range",
            Some(label),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.commits_in_range(range).await
                    }
                    RepositoryState::Remote(_) => {
                        bail!("listing commits in a range is not yet supported on remote projects")
                    }
                }
            },
        )
    }

    pub fn rebase(
        &mut self,
        upstream: String,
        options: RebaseOptions,
    ) -> oneshot::Receiver<Result<()>> {
        let label = format!("git rebase {upstream}");
        self.send_job("rebase", Some(label.into()), move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState {
                    backend,
                    environment,
                    ..
                }) => backend.rebase(upstream, options, environment).await,
                RepositoryState::Remote(_) => {
                    bail!("rebase is not yet supported on remote projects")
                }
            }
        })
    }

    pub fn rebase_interactive(
        &mut self,
        upstream: String,
        todo: Vec<RebaseTodoEntry>,
    ) -> oneshot::Receiver<Result<()>> {
        let label: SharedString = format!("git rebase -i {upstream}").into();
        self.send_job(
            "rebase_interactive",
            Some(label),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => {
                        backend
                            .rebase_interactive(upstream, todo, environment)
                            .await
                    }
                    RepositoryState::Remote(_) => {
                        bail!("interactive rebase is not yet supported on remote projects")
                    }
                }
            },
        )
    }

    pub fn rebase_action(
        &mut self,
        action: RebaseInProgressAction,
    ) -> oneshot::Receiver<Result<()>> {
        let label: SharedString = match action {
            RebaseInProgressAction::Continue => "git rebase --continue".into(),
            RebaseInProgressAction::Skip => "git rebase --skip".into(),
            RebaseInProgressAction::Abort => "git rebase --abort".into(),
        };
        self.send_job("rebase_action", Some(label), move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState {
                    backend,
                    environment,
                    ..
                }) => backend.rebase_action(action, environment).await,
                RepositoryState::Remote(_) => {
                    bail!("rebase actions are not yet supported on remote projects")
                }
            }
        })
    }

    /// The merge/rebase/cherry-pick/revert this repository is part-way through,
    /// if any.
    pub fn operation_in_progress(
        &mut self,
    ) -> oneshot::Receiver<Result<Option<ConflictingOperation>>> {
        self.send_job("operation_in_progress", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.operation_in_progress().await
                }
                RepositoryState::Remote(_) => Ok(None),
            }
        })
    }

    pub fn resolve_operation(
        &mut self,
        operation: ConflictingOperation,
        action: ConflictResolutionAction,
    ) -> oneshot::Receiver<Result<()>> {
        let label: SharedString = match operation.subcommand() {
            Some(subcommand) => format!("git {subcommand} {}", action.flag()).into(),
            None => format!("git {operation}").into(),
        };
        self.send_job(
            "resolve_operation",
            Some(label),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => {
                        backend
                            .resolve_operation(operation, action, environment)
                            .await
                    }
                    RepositoryState::Remote(_) => {
                        bail!("resolving conflicts is not yet supported on remote projects")
                    }
                }
            },
        )
    }

    pub fn tag_create(
        &mut self,
        name: String,
        commit: String,
        message: Option<String>,
        force: bool,
    ) -> oneshot::Receiver<Result<()>> {
        let label: SharedString = format!("git tag {name} {commit}").into();
        self.send_job("tag_create", Some(label), move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState {
                    backend,
                    environment,
                    ..
                }) => {
                    backend
                        .tag_create(name, commit, message, force, environment)
                        .await
                }
                RepositoryState::Remote(_) => {
                    bail!("tag creation is not yet supported on remote projects")
                }
            }
        })
    }

    pub fn tag_delete(&mut self, name: String) -> oneshot::Receiver<Result<()>> {
        let label: SharedString = format!("git tag -d {name}").into();
        self.send_job("tag_delete", Some(label), move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.tag_delete(name).await
                }
                RepositoryState::Remote(_) => {
                    bail!("tag deletion is not yet supported on remote projects")
                }
            }
        })
    }

    pub fn list_tags(&mut self) -> oneshot::Receiver<Result<Vec<Tag>>> {
        self.send_job("list_tags", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.list_tags().await
                }
                RepositoryState::Remote(_) => {
                    bail!("listing tags is not yet supported on remote projects")
                }
            }
        })
    }

    pub fn branch_force_update(
        &mut self,
        name: String,
        commit: String,
    ) -> oneshot::Receiver<Result<()>> {
        let label: SharedString = format!("git branch -f {name} {commit}").into();
        self.send_job("branch_force", Some(label), move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState {
                    backend,
                    environment,
                    ..
                }) => backend.branch_force_update(name, commit, environment).await,
                RepositoryState::Remote(_) => {
                    bail!("branch force-update is not yet supported on remote projects")
                }
            }
        })
    }

    pub fn reflog(
        &mut self,
        ref_name: Option<String>,
        limit: Option<usize>,
    ) -> oneshot::Receiver<Result<Vec<ReflogEntry>>> {
        self.send_job("reflog", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.reflog(ref_name, limit).await
                }
                RepositoryState::Remote(_) => {
                    bail!("reading the reflog is not yet supported on remote projects")
                }
            }
        })
    }

    pub fn active_jobs(&self) -> Vec<JobInfo> {
        let mut jobs: Vec<_> = self.active_jobs.values().cloned().collect();
        jobs.sort_by_key(|job| job.start);
        jobs
    }
}

/// Spawn a detached background task that reads `GitProgressEvent`s from
/// `progress_rx` and surfaces them through the matching repository's
/// `active_jobs` entry, so the existing job indicator UI shows live progress.
///
/// The task terminates naturally when the sender side of the channel is dropped
/// (i.e. when the originating `fetch`/`pull`/`push` op completes).
pub(super) fn spawn_progress_relay(
    op_prefix: &'static str,
    repository: WeakEntity<Repository>,
    progress_rx: smol::channel::Receiver<GitProgressEvent>,
    cx: AsyncApp,
) {
    let foreground = cx.foreground_executor().clone();
    foreground
        .spawn(async move {
            let mut cx = cx;
            while let Ok(event) = progress_rx.recv().await {
                let message: SharedString = match event.percent {
                    Some(percent) => format!("{op_prefix} — {} {percent}%", event.phase).into(),
                    None => format!("{op_prefix} — {}", event.phase).into(),
                };
                repository
                    .update(&mut cx, |repo, cx| {
                        for info in repo.active_jobs.values_mut() {
                            if info.message.starts_with(op_prefix) {
                                info.message = message.clone();
                            }
                        }
                        cx.emit(JobsUpdated);
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
}
