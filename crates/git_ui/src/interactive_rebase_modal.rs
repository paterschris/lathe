use std::collections::HashMap;

use editor::Editor;
use git::Oid;
use git::repository::{RebaseAction, RebaseTodoEntry};
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle, Subscription,
    WeakEntity, px,
};
use project::git_store::{
    CommitDataState, GitStore, Repository, RepositoryId, undo_log::UndoAction,
};
use ui::{
    ActiveTheme, App, Button, ButtonCommon, ButtonStyle, Clickable, Color, Context, ContextMenu,
    Divider, FluentBuilder, Icon, IconButton, IconName, IconSize, InteractiveElement, IntoElement,
    Label, LabelCommon, LabelSize, ParentElement, PopoverMenu, Render, SharedString, StyledExt,
    Tooltip, Window, WithScrollbar, div, h_flex, prelude::*, v_flex,
};
use workspace::{ModalView, Workspace};

/// One row in the interactive-rebase plan. The commit's subject is resolved
/// lazily at render time from the `Repository`, so we don't need to thread it
/// through callers that only have the lightweight graph data.
#[derive(Clone)]
pub struct RebasePlanEntry {
    pub sha: SharedString,
    pub short_sha: SharedString,
    pub action: RebaseAction,
}

#[derive(Clone)]
struct DraggedRebaseEntry {
    index: usize,
    short_sha: SharedString,
    subject: SharedString,
}

struct DraggedRebaseEntryView {
    short_sha: SharedString,
    subject: SharedString,
}

impl Render for DraggedRebaseEntryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .bg(cx.theme().colors().background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .px_2()
            .py_0p5()
            .gap_2()
            .max_w(px(360.))
            .child(
                Label::new(self.short_sha.clone())
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(
                Label::new(self.subject.clone())
                    .size(LabelSize::Small)
                    .truncate(),
            )
    }
}

/// Actions exposed in the per-row dropdown. Ordered the same way the cycle
/// behaviour used to walk them so muscle memory still finds them in order.
const SELECTABLE_ACTIONS: &[RebaseAction] = &[
    RebaseAction::Pick,
    RebaseAction::Reword,
    RebaseAction::Squash,
    RebaseAction::Fixup,
    RebaseAction::Drop,
    RebaseAction::Edit,
];

/// Modal that mirrors `git rebase -i`. The user sees the list of commits to be
/// replayed (oldest at the top, matching git's own todo file), picks an action
/// per row via a dropdown, optionally reorders rows, and clicks "Start rebase".
pub struct InteractiveRebaseModal {
    focus_handle: FocusHandle,
    entries: Vec<RebasePlanEntry>,
    /// Commit order as originally proposed, so an all-pick plan that was only
    /// reordered still runs instead of being treated as a no-op.
    original_order: Vec<SharedString>,
    /// Argument passed to `git rebase -i <upstream>` — the commit *just before*
    /// the oldest entry. Everything after this gets rewritten.
    upstream: SharedString,
    repo_id: RepositoryId,
    git_store: Entity<GitStore>,
    /// Source of truth for commit subjects shown in the modal. Subscribed to
    /// for change notifications so newly-loaded subjects render automatically.
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    /// Branch + tip captured before the rebase so the Undo toast can restore.
    pre_state: Option<(String, String)>,
    in_progress: bool,
    last_error: Option<SharedString>,
    /// Per-commit editors for the reword text input, keyed by sha so they follow
    /// their row when it's reordered. Allocated lazily the first time the user
    /// switches that row's action to `Reword`. Kept across action switches so
    /// the typed message survives picking another action and back.
    reword_editors: HashMap<SharedString, Entity<Editor>>,
    scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for InteractiveRebaseModal {}
impl ModalView for InteractiveRebaseModal {}

impl Focusable for InteractiveRebaseModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl InteractiveRebaseModal {
    pub fn new(
        entries: Vec<RebasePlanEntry>,
        upstream: impl Into<SharedString>,
        repo_id: RepositoryId,
        git_store: Entity<GitStore>,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        pre_state: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Kick off fetches for any subjects that aren't already cached so they
        // resolve in the background while the modal is open.
        repository.update(cx, |repo, cx| {
            for entry in &entries {
                if let Ok(oid) = entry.sha.parse::<Oid>() {
                    let _ = repo.fetch_commit_data(oid, false, cx);
                }
            }
        });
        let repo_subscription = cx.observe(&repository, |_, _, cx| cx.notify());
        let original_order = entries.iter().map(|entry| entry.sha.clone()).collect();
        Self {
            focus_handle: cx.focus_handle(),
            entries,
            original_order,
            upstream: upstream.into(),
            repo_id,
            git_store,
            repository,
            workspace,
            pre_state,
            in_progress: false,
            last_error: None,
            reword_editors: HashMap::new(),
            scroll_handle: ScrollHandle::new(),
            _subscriptions: vec![repo_subscription],
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Resolve the commit subject for `entry` from the repository's cached
    /// commit data. Returns "Loading…" while the fetch is in flight.
    fn entry_subject(
        &self,
        sha: &SharedString,
        short_sha: &SharedString,
        cx: &mut App,
    ) -> SharedString {
        let Ok(oid) = sha.parse::<Oid>() else {
            return short_sha.clone();
        };
        let state = self.repository.update(cx, |repo, cx| {
            repo.fetch_commit_data(oid, false, cx).clone()
        });
        match state {
            CommitDataState::Loaded(data) => data.subject.clone(),
            CommitDataState::Loading(_) => "Loading…".into(),
        }
    }

    fn set_action(
        &mut self,
        index: usize,
        action: RebaseAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        let previous = entry.action;
        entry.action = action;
        if action == RebaseAction::Reword && previous != RebaseAction::Reword {
            let sha = entry.sha.clone();
            if !self.reword_editors.contains_key(&sha) {
                let short_sha = entry.short_sha.clone();
                let initial = self.entry_subject(&sha, &short_sha, cx).to_string();
                let editor = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_text(initial.as_str(), window, cx);
                    editor
                });
                self.reword_editors.insert(sha.clone(), editor);
            }
            if let Some(editor) = self.reword_editors.get(&sha).cloned() {
                editor.update(cx, |editor, cx| {
                    let handle = editor.focus_handle(cx);
                    handle.focus(window, cx);
                    editor.select_all(&Default::default(), window, cx);
                });
            }
        }
        cx.notify();
    }

    fn move_entry(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if self.in_progress || from == to || from >= self.entries.len() {
            return;
        }
        let to = to.min(self.entries.len() - 1);
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        cx.notify();
    }

    fn is_reordered(&self) -> bool {
        self.entries
            .iter()
            .map(|entry| &entry.sha)
            .ne(self.original_order.iter())
    }

    /// Catches plans git would reject only after starting. A dropped commit
    /// produces nothing to fold into, so the first row that isn't a drop is the
    /// one that must not be a squash or fixup.
    fn plan_error(&self) -> Option<SharedString> {
        let first_kept = self
            .entries
            .iter()
            .find(|entry| entry.action != RebaseAction::Drop)?;
        match first_kept.action {
            RebaseAction::Squash | RebaseAction::Fixup => Some(
                format!(
                    "{} can't {} into the commit before it, it's the first commit kept.",
                    first_kept.short_sha,
                    action_label(first_kept.action)
                )
                .into(),
            ),
            _ => None,
        }
    }

    fn start_rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_progress || self.plan_error().is_some() {
            return;
        }
        // If every row is "pick" in the original order, running the rebase
        // would be a no-op and just potentially trigger conflicts. Treat as
        // cancel.
        if !self.is_reordered() && self.entries.iter().all(|e| e.action == RebaseAction::Pick) {
            cx.emit(DismissEvent);
            return;
        }

        let todo: Vec<RebaseTodoEntry> = self
            .entries
            .iter()
            .map(|entry| {
                let message = if entry.action == RebaseAction::Reword {
                    self.reword_editors.get(&entry.sha).and_then(|ed| {
                        let text = ed.read(cx).text(cx);
                        let trimmed = text.trim_end_matches('\n');
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    })
                } else {
                    None
                };
                RebaseTodoEntry {
                    action: entry.action,
                    commit: entry.sha.to_string(),
                    message,
                }
            })
            .collect();
        let upstream = self.upstream.to_string();
        let repo_id = self.repo_id;
        let git_store = self.git_store.clone();
        let workspace = self.workspace.clone();
        let pre_state = self.pre_state.clone();

        let Some(repo) = git_store.read(cx).repositories().get(&repo_id).cloned() else {
            return;
        };
        let receiver = repo.update(cx, |repo, _cx| repo.rebase_interactive(upstream, todo));

        self.in_progress = true;
        self.last_error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| match receiver.await {
            Ok(Ok(())) => {
                if let Some((branch, sha)) = pre_state.clone() {
                    cx.update(|_, cx| {
                        git_store.update(cx, |store, cx| {
                            store.record_undo(
                                repo_id,
                                "Interactive rebase".to_string(),
                                UndoAction::RestoreBranchTip {
                                    branch,
                                    sha,
                                    is_current: true,
                                },
                                cx,
                            );
                        });
                        let _ = workspace;
                    })
                    .ok();
                }
                this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
            }
            Ok(Err(error)) => {
                let message: SharedString = format!("{error}").into();
                this.update(cx, |this, cx| {
                    this.in_progress = false;
                    this.last_error = Some(message);
                    cx.notify();
                })
                .ok();
            }
            Err(_) => {
                this.update(cx, |this, cx| {
                    this.in_progress = false;
                    this.last_error = Some("Rebase cancelled".into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
}

fn action_label(action: RebaseAction) -> &'static str {
    match action {
        RebaseAction::Pick => "pick",
        RebaseAction::Reword => "reword",
        RebaseAction::Squash => "squash",
        RebaseAction::Fixup => "fixup",
        RebaseAction::Drop => "drop",
        RebaseAction::Edit => "edit",
    }
}

fn action_color(action: RebaseAction) -> Color {
    match action {
        RebaseAction::Pick => Color::Default,
        RebaseAction::Reword => Color::Accent,
        RebaseAction::Squash | RebaseAction::Fixup => Color::Warning,
        RebaseAction::Drop => Color::Error,
        RebaseAction::Edit => Color::Info,
    }
}

impl Render for InteractiveRebaseModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entry_count = self.entries.len();
        let in_progress = self.in_progress;
        let weak_self = cx.weak_entity();

        // Pre-compute everything that needs `cx` so the row-builder closure
        // below doesn't have to capture the context mutably (which would clash
        // with the `cx.listener` and `cx.theme()` calls that follow).
        let row_data: Vec<(
            SharedString,
            SharedString,
            RebaseAction,
            Option<Entity<Editor>>,
        )> = self
            .entries
            .iter()
            .map(|entry| {
                let subject = self.entry_subject(&entry.sha, &entry.short_sha, cx);
                let editor = if entry.action == RebaseAction::Reword {
                    self.reword_editors.get(&entry.sha).cloned()
                } else {
                    None
                };
                (entry.short_sha.clone(), subject, entry.action, editor)
            })
            .collect();
        let theme = cx.theme();

        let rows: Vec<_> = row_data
            .into_iter()
            .enumerate()
            .map(|(idx, (short_sha, subject, action, reword_editor))| {
                let chip_label: SharedString = action_label(action).into();
                let chip_color = action_color(action);
                let trigger_button = Button::new(("rebase-action-btn", idx), chip_label)
                    .label_size(LabelSize::Small)
                    .color(chip_color)
                    .style(ButtonStyle::Outlined)
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .disabled(in_progress);
                let menu_owner = weak_self.clone();
                let action_menu = PopoverMenu::new(("rebase-action-menu", idx))
                    .trigger(trigger_button)
                    .menu(move |window, cx| {
                        let menu_owner = menu_owner.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for &candidate in SELECTABLE_ACTIONS {
                                let menu_owner = menu_owner.clone();
                                menu =
                                    menu.entry(action_label(candidate), None, move |window, cx| {
                                        menu_owner
                                            .update(cx, |this, cx| {
                                                this.set_action(idx, candidate, window, cx);
                                            })
                                            .ok();
                                    });
                            }
                            menu
                        }))
                    });

                let move_buttons = h_flex()
                    .flex_none()
                    .child(
                        IconButton::new(("rebase-move-up", idx), IconName::ArrowUp)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .disabled(in_progress || idx == 0)
                            .tooltip(Tooltip::text("Move up"))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.move_entry(idx, idx.saturating_sub(1), cx);
                            })),
                    )
                    .child(
                        IconButton::new(("rebase-move-down", idx), IconName::ArrowDown)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .disabled(in_progress || idx + 1 >= entry_count)
                            .tooltip(Tooltip::text("Move down"))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.move_entry(idx, idx + 1, cx);
                            })),
                    );

                let drag_payload = DraggedRebaseEntry {
                    index: idx,
                    short_sha: short_sha.clone(),
                    subject: subject.clone(),
                };
                // Only the header is draggable so text selection inside the
                // reword editor below doesn't start a row drag.
                let row_header = h_flex()
                    .id(("rebase-row-header", idx))
                    .w_full()
                    .gap_2()
                    .child(move_buttons)
                    .child(action_menu)
                    .child(
                        Label::new(short_sha)
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .when(reword_editor.is_none(), |this| {
                        this.child(Label::new(subject).size(LabelSize::Small).truncate())
                    })
                    .when(!in_progress, |this| {
                        this.cursor_grab()
                            .on_drag(drag_payload, |payload, _, _, cx| {
                                cx.new(|_| DraggedRebaseEntryView {
                                    short_sha: payload.short_sha.clone(),
                                    subject: payload.subject.clone(),
                                })
                            })
                    });

                v_flex()
                    .id(("rebase-row", idx))
                    .w_full()
                    .py_1()
                    .px_3()
                    .gap_1()
                    .drag_over::<DraggedRebaseEntry>(move |style, payload, _window, cx| {
                        if payload.index == idx {
                            style
                        } else {
                            style.bg(cx.theme().colors().drop_target_background)
                        }
                    })
                    .on_drop(
                        cx.listener(move |this, payload: &DraggedRebaseEntry, _window, cx| {
                            this.move_entry(payload.index, idx, cx);
                        }),
                    )
                    .child(row_header)
                    .when_some(reword_editor, |this, editor| {
                        this.child(
                            div()
                                .w_full()
                                .pl_8()
                                .py_1()
                                .child(editor.into_any_element()),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        // A live plan problem outranks a stale error from an earlier attempt,
        // since it's what the user has to fix before they can start again.
        let plan_error = self.plan_error();
        let plan_is_valid = plan_error.is_none();
        let footer_error = plan_error.or_else(|| self.last_error.clone());
        let has_footer_error = footer_error.is_some();

        let count_label: SharedString =
            format!("{entry_count} commit(s) onto {}", self.upstream).into();

        v_flex()
            .key_context("InteractiveRebaseModal")
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(ui::rems(40.))
            .max_h(ui::rems(40.))
            .overflow_hidden()
            .child(
                v_flex()
                    .w_full()
                    .p_3()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(Label::new("Interactive rebase").size(LabelSize::Large))
                    .child(
                        Label::new(count_label)
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
            )
            .child(
                div()
                    .id("rebase-rows")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .py_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(rows)
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx),
            )
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .w_full()
                    .p_3()
                    .gap_2()
                    .justify_between()
                    .when_some(footer_error, |this, error| {
                        this.child(Label::new(error).color(Color::Error).size(LabelSize::Small))
                    })
                    .when(!has_footer_error, |this| {
                        this.child(
                            Label::new(
                                "Pick an action per commit, drag or use the arrows to reorder.",
                            )
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("rebase-cancel", "Cancel")
                                    .style(ButtonStyle::Subtle)
                                    .disabled(in_progress)
                                    .on_click(cx.listener(|_, _, _window, cx| {
                                        cx.emit(DismissEvent);
                                    })),
                            )
                            .child(
                                Button::new(
                                    "rebase-start",
                                    if in_progress {
                                        "Rebasing…"
                                    } else {
                                        "Start rebase"
                                    },
                                )
                                .style(ButtonStyle::Filled)
                                .disabled(in_progress || !plan_is_valid)
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.start_rebase(window, cx);
                                    },
                                )),
                            ),
                    ),
            )
    }
}
