use collections::HashMap;
use editor::Editor;
use git::{
    GitHostingProvider, ParsedGitRemote, PullRequestDetail, PullRequestMergeMethod,
    PullRequestReviewComment, PullRequestReviewVerdict, PullRequestState,
};
use gpui::{Empty, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, WeakEntity};
use notifications::status_toast::StatusToast;
use std::sync::Arc;
use ui::{Button, ButtonCommon, ButtonStyle, Clickable, Divider, prelude::*};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

/// Read-only workspace item that presents the metadata + inline-review comments
/// for a single pull request. The diff itself is fetched but rendered as a
/// monospace block for now — full hunk-aware rendering will come when the
/// dedicated `crates/pr_ui` lands.
pub struct PullRequestView {
    focus_handle: FocusHandle,
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: ParsedGitRemote,
    number: u32,
    workspace: WeakEntity<Workspace>,
    detail: Option<PullRequestDetail>,
    comments: Vec<PullRequestReviewComment>,
    diff_files: Vec<ParsedDiffFile>,
    error: Option<SharedString>,
    loading: bool,
    /// True while an approve / request-changes / merge call is in flight; the
    /// header buttons disable themselves to prevent double-fires.
    in_flight_action: bool,
    /// id of the inline comment the user is currently replying to, or `None`
    /// if no reply box is open. Lazily-created editor lives in `reply_editor`.
    reply_target: Option<u64>,
    reply_editor: Option<Entity<Editor>>,
    reply_in_flight: bool,
    _load_task: Option<Task<()>>,
    _action_task: Option<Task<()>>,
    _reply_task: Option<Task<()>>,
}

impl PullRequestView {
    pub fn new(
        provider: Arc<dyn GitHostingProvider + Send + Sync>,
        remote: ParsedGitRemote,
        number: u32,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            provider,
            remote,
            number,
            workspace,
            detail: None,
            comments: Vec::new(),
            diff_files: Vec::new(),
            error: None,
            loading: true,
            in_flight_action: false,
            reply_target: None,
            reply_editor: None,
            reply_in_flight: false,
            _load_task: None,
            _action_task: None,
            _reply_task: None,
        };
        this.refresh(cx);
        this
    }

    fn open_reply(&mut self, comment_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        // Replace any prior draft. One reply box at a time keeps the layout
        // straightforward and matches the GitHub UI.
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
            editor.set_placeholder_text("Write a reply…", window, cx);
            editor
        });
        let handle = editor.focus_handle(cx);
        self.reply_editor = Some(editor);
        self.reply_target = Some(comment_id);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn cancel_reply(&mut self, cx: &mut Context<Self>) {
        self.reply_target = None;
        self.reply_editor = None;
        cx.notify();
    }

    fn submit_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(in_reply_to) = self.reply_target else {
            return;
        };
        let Some(editor) = self.reply_editor.clone() else {
            return;
        };
        if self.reply_in_flight {
            return;
        }
        let body = editor.read(cx).text(cx);
        let body = body.trim().to_string();
        if body.is_empty() {
            return;
        }
        let provider = self.provider.clone();
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        let workspace = self.workspace.clone();
        self.reply_in_flight = true;
        cx.notify();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = provider
                .post_review_comment(&remote, number, in_reply_to, body.into(), http_client)
                .await;
            this.update(cx, |this, cx| {
                this.reply_in_flight = false;
                match result {
                    Ok(()) => {
                        this.reply_target = None;
                        this.reply_editor = None;
                        surface_toast(&workspace, "Posted reply".into(), cx);
                        this.refresh(cx);
                    }
                    Err(error) => {
                        surface_toast(
                            &workspace,
                            format!("Reply failed: {error}").into(),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self._reply_task = Some(task);
    }

    pub fn number(&self) -> u32 {
        self.number
    }

    pub fn remote(&self) -> &ParsedGitRemote {
        &self.remote
    }

    fn submit_review(&mut self, verdict: PullRequestReviewVerdict, cx: &mut Context<Self>) {
        if self.in_flight_action {
            return;
        }
        let label: SharedString = match verdict {
            PullRequestReviewVerdict::Approve => "Approved pull request".into(),
            PullRequestReviewVerdict::RequestChanges => {
                "Requested changes on pull request".into()
            }
            PullRequestReviewVerdict::Comment => "Posted review comment".into(),
        };
        let provider = self.provider.clone();
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        let workspace = self.workspace.clone();
        self.in_flight_action = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = provider
                .submit_review(&remote, number, verdict, None, http_client)
                .await;
            this.update(cx, |this, cx| {
                this.in_flight_action = false;
                match result {
                    Ok(()) => surface_toast(&workspace, label, cx),
                    Err(error) => {
                        surface_toast(
                            &workspace,
                            format!("Review failed: {error}").into(),
                            cx,
                        );
                    }
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        });
        self._action_task = Some(task);
    }

    fn merge(&mut self, method: PullRequestMergeMethod, cx: &mut Context<Self>) {
        if self.in_flight_action {
            return;
        }
        let method_label = match method {
            PullRequestMergeMethod::Merge => "merge commit",
            PullRequestMergeMethod::Squash => "squash",
            PullRequestMergeMethod::Rebase => "rebase",
        };
        let provider = self.provider.clone();
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        let workspace = self.workspace.clone();
        self.in_flight_action = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = provider
                .merge_pull_request(&remote, number, method, http_client)
                .await;
            this.update(cx, |this, cx| {
                this.in_flight_action = false;
                match result {
                    Ok(()) => surface_toast(
                        &workspace,
                        format!("Merged PR #{number} via {method_label}").into(),
                        cx,
                    ),
                    Err(error) => surface_toast(
                        &workspace,
                        format!("Merge failed: {error}").into(),
                        cx,
                    ),
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        });
        self._action_task = Some(task);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let provider = self.provider.clone();
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        self.loading = true;
        self.error = None;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let detail_fut = provider.get_pull_request(&remote, number, http_client.clone());
            let comments_fut =
                provider.get_pull_request_comments(&remote, number, http_client.clone());
            let diff_fut = provider.get_pull_request_diff(&remote, number, http_client);
            let (detail, comments, diff) =
                futures::future::join3(detail_fut, comments_fut, diff_fut).await;
            this.update(cx, |this, cx| {
                match detail {
                    Ok(d) => this.detail = Some(d),
                    Err(e) => this.error = Some(format!("{e}").into()),
                }
                this.comments = match comments {
                    Ok(c) => c,
                    Err(error) => {
                        // Comments failing is non-fatal — the header still
                        // renders. Log for visibility.
                        log::warn!("loading PR #{} comments failed: {error:?}", this.number);
                        Vec::new()
                    }
                };
                this.diff_files = match diff {
                    Ok(text) => parse_unified_diff(&text),
                    Err(error) => {
                        log::warn!("loading PR #{} diff failed: {error:?}", this.number);
                        Vec::new()
                    }
                };
                this.loading = false;
                cx.notify();
            })
            .ok();
        });
        self._load_task = Some(task);
    }
}

/// One line within a hunk. The text *includes* the original line content
/// without the leading `+`/`-`/` ` marker — that marker is captured by the
/// variant tag and the renderer reapplies it for display.
#[derive(Debug, Clone)]
enum ParsedDiffLine {
    Context(String),
    Addition(String),
    Deletion(String),
}

#[derive(Debug, Clone)]
struct ParsedDiffHunk {
    /// The `@@ -a,b +c,d @@` header line as-is. Useful for tooltips.
    header: String,
    lines: Vec<ParsedDiffLine>,
}

#[derive(Debug, Clone)]
struct ParsedDiffFile {
    /// Repo-relative path. For renames the *new* path is used.
    path: String,
    hunks: Vec<ParsedDiffHunk>,
}

/// Walk a unified diff string and split it into files and hunks. Handles the
/// shape produced by `git diff` and the GitHub `.diff` endpoint. Binary file
/// markers and rename headers are tolerated but skipped — they show up as
/// files with no hunks.
fn parse_unified_diff(text: &str) -> Vec<ParsedDiffFile> {
    let mut files: Vec<ParsedDiffFile> = Vec::new();
    let mut current_file: Option<ParsedDiffFile> = None;
    let mut current_hunk: Option<ParsedDiffHunk> = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(file) = current_file.take() {
                files.push(file);
            }
            current_hunk = None;
            // `diff --git a/foo b/bar` — take everything after the final space
            // before the `b/` prefix. The `+++ b/...` line that follows is
            // more reliable, so just start with an empty path and let the
            // `+++` handler fill it.
            let _ = rest;
            current_file = Some(ParsedDiffFile {
                path: String::new(),
                hunks: Vec::new(),
            });
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            if let Some(file) = current_file.as_mut() {
                file.path = path.to_string();
            }
        } else if line.starts_with("+++ ") {
            // Handle `+++ /dev/null` for deletions.
            if let Some(file) = current_file.as_mut()
                && file.path.is_empty()
            {
                file.path = line.trim_start_matches("+++ ").to_string();
            }
        } else if line.starts_with("@@") {
            if let (Some(file), Some(hunk)) = (current_file.as_mut(), current_hunk.take()) {
                file.hunks.push(hunk);
            }
            current_hunk = Some(ParsedDiffHunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
        } else if let Some(hunk) = current_hunk.as_mut() {
            if let Some(rest) = line.strip_prefix('+') {
                hunk.lines.push(ParsedDiffLine::Addition(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix('-') {
                hunk.lines.push(ParsedDiffLine::Deletion(rest.to_string()));
            } else if let Some(rest) = line.strip_prefix(' ') {
                hunk.lines.push(ParsedDiffLine::Context(rest.to_string()));
            }
            // Lines starting with `\` (e.g. "\ No newline at end of file") are
            // dropped to keep the rendered diff clean.
        }
    }

    if let (Some(mut file), Some(hunk)) = (current_file.take(), current_hunk.take()) {
        file.hunks.push(hunk);
        files.push(file);
    } else if let Some(file) = current_file.take() {
        files.push(file);
    }

    files
}

fn clone_remote(remote: &ParsedGitRemote) -> ParsedGitRemote {
    ParsedGitRemote {
        owner: remote.owner.clone(),
        repo: remote.repo.clone(),
    }
}

/// Fire a small status toast through the workspace if it's still alive.
fn surface_toast<C>(workspace: &WeakEntity<Workspace>, message: SharedString, cx: &mut C)
where
    C: gpui::AppContext,
{
    workspace
        .update(cx, |workspace, cx| {
            let toast =
                StatusToast::new(message.clone(), cx, |this, _cx| this.dismiss_button(true));
            workspace.toggle_status_toast(toast, cx);
        })
        .ok();
}

#[derive(Copy, Clone, Debug)]
pub enum PullRequestViewEvent {}

impl EventEmitter<PullRequestViewEvent> for PullRequestView {}
impl EventEmitter<ItemEvent> for PullRequestView {}

impl Focusable for PullRequestView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for PullRequestView {
    type Event = PullRequestViewEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match self.detail.as_ref() {
            Some(d) => format!("PR #{} — {}", d.number, d.title).into(),
            None => format!("PR #{}", self.number).into(),
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Pull Request View Opened")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn show_toolbar(&self) -> bool {
        false
    }
}

impl Render for PullRequestView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.loading && self.detail.is_none() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Label::new(format!("Loading PR #{}…", self.number)))
                .into_any_element();
        }

        if let Some(error) = self.error.clone()
            && self.detail.is_none()
        {
            return v_flex()
                .size_full()
                .p_4()
                .gap_2()
                .child(
                    Label::new(format!("Failed to load PR #{}", self.number))
                        .color(Color::Error),
                )
                .child(
                    Label::new(error)
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element();
        }

        let Some(detail) = self.detail.as_ref() else {
            return Empty.into_any_element();
        };

        // Group inline review comments by file path so the comments section
        // mirrors how reviewers read a PR. Stable insertion order (BTree could
        // also work, but HashMap keeps the original arrival order via
        // `entries.entry(...).or_default().push(...)`).
        let mut by_path: HashMap<SharedString, Vec<PullRequestReviewComment>> =
            HashMap::default();
        let mut path_order: Vec<SharedString> = Vec::new();
        for comment in &self.comments {
            if !by_path.contains_key(&comment.path) {
                path_order.push(comment.path.clone());
            }
            by_path
                .entry(comment.path.clone())
                .or_default()
                .push(comment.clone());
        }

        let in_flight = self.in_flight_action;
        let header = self.render_header(detail, in_flight, cx);
        let body = self.render_body(detail, cx);
        let comments = self.render_comments(&path_order, &by_path, cx);
        let diff = self.render_diff(cx);
        let editor_bg = cx.theme().colors().editor_background;
        let has_diff = !self.diff_files.is_empty();
        let has_comments = !self.comments.is_empty();

        v_flex()
            .size_full()
            .bg(editor_bg)
            .child(header)
            .child(Divider::horizontal())
            .child(
                v_flex()
                    .id("pr-view-scroll")
                    .flex_1()
                    .w_full()
                    .overflow_y_scroll()
                    .p_4()
                    .gap_4()
                    .child(body)
                    .when(has_diff, |this| {
                        this.child(Divider::horizontal()).child(diff)
                    })
                    .when(has_comments, |this| {
                        this.child(Divider::horizontal()).child(comments)
                    }),
            )
            .into_any_element()
    }
}

impl PullRequestView {
    fn render_header(
        &self,
        detail: &PullRequestDetail,
        in_flight: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state_label = match detail.state {
            PullRequestState::Open if detail.is_draft => "draft",
            PullRequestState::Open => "open",
            PullRequestState::Merged => "merged",
            PullRequestState::Closed => "closed",
        };
        let state_color = match detail.state {
            PullRequestState::Open if detail.is_draft => Color::Muted,
            PullRequestState::Open => Color::Success,
            PullRequestState::Merged => Color::Accent,
            PullRequestState::Closed => Color::Error,
        };
        let mergeable_label = match detail.is_mergeable {
            Some(true) => Some(("Mergeable", Color::Success)),
            Some(false) => Some(("Conflicts", Color::Error)),
            None => None,
        };
        // Action buttons are only relevant while the PR is open.
        let actions_enabled = matches!(detail.state, PullRequestState::Open) && !in_flight;
        let merge_enabled = actions_enabled && detail.is_mergeable.unwrap_or(false);

        v_flex()
            .w_full()
            .px_4()
            .py_3()
            .gap_1()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Label::new(format!("#{}", detail.number))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(Label::new(detail.title.clone()).size(LabelSize::Large))
                    .child(Label::new(state_label).color(state_color))
                    .when_some(mergeable_label, |this, (label, color)| {
                        this.child(Label::new(label).color(color).size(LabelSize::Small))
                    }),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Label::new(detail.author_login.clone())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(
                        Label::new(format!(
                            "{} → {}",
                            detail.source_branch, detail.target_branch
                        ))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                    )
                    .child(
                        Label::new(format!(
                            "{} file(s), +{} -{}",
                            detail.changed_files, detail.additions, detail.deletions,
                        ))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("pr-approve", "Approve")
                            .style(ButtonStyle::Outlined)
                            .disabled(!actions_enabled)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.submit_review(PullRequestReviewVerdict::Approve, cx);
                            })),
                    )
                    .child(
                        Button::new("pr-request-changes", "Request changes")
                            .style(ButtonStyle::Outlined)
                            .disabled(!actions_enabled)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.submit_review(
                                    PullRequestReviewVerdict::RequestChanges,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("pr-merge", "Merge")
                            .style(ButtonStyle::Filled)
                            .disabled(!merge_enabled)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.merge(PullRequestMergeMethod::Merge, cx);
                            })),
                    )
                    .child(
                        Button::new("pr-squash", "Squash & merge")
                            .style(ButtonStyle::Outlined)
                            .disabled(!merge_enabled)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.merge(PullRequestMergeMethod::Squash, cx);
                            })),
                    )
                    .when(in_flight, |this| {
                        this.child(
                            Label::new("…")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_body(&self, detail: &PullRequestDetail, cx: &Context<Self>) -> AnyElement {
        let _ = cx;
        if detail.body.is_empty() {
            return v_flex()
                .child(
                    Label::new("No description provided.")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element();
        }
        // Render the body as line-preserving plain text. A future revision can
        // hand this off to the `markdown` crate for full rendering.
        let lines = detail
            .body
            .lines()
            .map(|line| Label::new(line.to_string()).size(LabelSize::Small).into_any_element());
        v_flex().gap_0().children(lines).into_any_element()
    }

    fn render_diff(&self, cx: &Context<Self>) -> AnyElement {
        let _ = cx;
        if self.diff_files.is_empty() {
            return Empty.into_any_element();
        }
        let total_files = self.diff_files.len();
        let total_hunks: usize = self
            .diff_files
            .iter()
            .map(|file| file.hunks.len())
            .sum();
        let files = self.diff_files.iter().map(|file| {
            let path: SharedString = if file.path.is_empty() {
                "(unknown)".into()
            } else {
                file.path.clone().into()
            };
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(path).size(LabelSize::Small))
                        .child(
                            Label::new(format!("{} hunk(s)", file.hunks.len()))
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                )
                .children(file.hunks.iter().map(|hunk| {
                    let lines = hunk.lines.iter().map(|line| match line {
                        ParsedDiffLine::Addition(text) => Label::new(format!("+ {text}"))
                            .color(Color::Success)
                            .size(LabelSize::Small)
                            .into_any_element(),
                        ParsedDiffLine::Deletion(text) => Label::new(format!("- {text}"))
                            .color(Color::Error)
                            .size(LabelSize::Small)
                            .into_any_element(),
                        ParsedDiffLine::Context(text) => Label::new(format!("  {text}"))
                            .color(Color::Muted)
                            .size(LabelSize::Small)
                            .into_any_element(),
                    });
                    v_flex()
                        .gap_0()
                        .px_3()
                        .py_1()
                        .border_l_2()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Label::new(hunk.header.clone())
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                        .children(lines)
                }))
        });
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Changed files").size(LabelSize::Large))
                    .child(
                        Label::new(format!("{total_files} file(s), {total_hunks} hunk(s)"))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
            )
            .children(files)
            .into_any_element()
    }

    fn render_comments(
        &self,
        path_order: &[SharedString],
        by_path: &HashMap<SharedString, Vec<PullRequestReviewComment>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let border_color = cx.theme().colors().border_variant;
        let reply_target = self.reply_target;
        let reply_editor = self.reply_editor.clone();
        let reply_in_flight = self.reply_in_flight;

        let groups = path_order.iter().filter_map(|path| {
            let comments = by_path.get(path)?;
            let count = comments.len();
            Some(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(path.clone()))
                            .child(
                                Label::new(format!("{count} comment(s)"))
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            ),
                    )
                    .children(comments.iter().map(|c| {
                        let comment_id = c.id;
                        let author = c.author_login.clone();
                        let body = c.body.clone();
                        let line_label = c
                            .line
                            .map(|line| format!("line {line}"))
                            .unwrap_or_else(|| "file".to_string());
                        let is_replying = reply_target == Some(comment_id);
                        let editor_for_reply = is_replying.then(|| reply_editor.clone()).flatten();

                        v_flex()
                            .px_3()
                            .py_2()
                            .gap_1()
                            .border_l_2()
                            .border_color(border_color)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(Label::new(author).size(LabelSize::Small))
                                    .child(
                                        Label::new(line_label)
                                            .color(Color::Muted)
                                            .size(LabelSize::Small),
                                    ),
                            )
                            .child(Label::new(body).size(LabelSize::Small))
                            .child(
                                h_flex().gap_2().child(
                                    Button::new(("reply", comment_id as usize), "Reply")
                                        .style(ButtonStyle::Subtle)
                                        .label_size(LabelSize::Small)
                                        .disabled(is_replying || reply_in_flight)
                                        .on_click(cx.listener(
                                            move |this, _, window, cx| {
                                                this.open_reply(comment_id, window, cx);
                                            },
                                        )),
                                ),
                            )
                            .when_some(editor_for_reply, |this, editor| {
                                this.child(
                                    v_flex()
                                        .gap_1()
                                        .pt_1()
                                        .child(editor)
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .child(
                                                    Button::new(
                                                        ("reply-submit", comment_id as usize),
                                                        if reply_in_flight {
                                                            "Posting…"
                                                        } else {
                                                            "Post reply"
                                                        },
                                                    )
                                                    .style(ButtonStyle::Filled)
                                                    .label_size(LabelSize::Small)
                                                    .disabled(reply_in_flight)
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            this.submit_reply(window, cx);
                                                        },
                                                    )),
                                                )
                                                .child(
                                                    Button::new(
                                                        ("reply-cancel", comment_id as usize),
                                                        "Cancel",
                                                    )
                                                    .style(ButtonStyle::Subtle)
                                                    .label_size(LabelSize::Small)
                                                    .disabled(reply_in_flight)
                                                    .on_click(cx.listener(
                                                        |this, _, _window, cx| {
                                                            this.cancel_reply(cx);
                                                        },
                                                    )),
                                                ),
                                        ),
                                )
                            })
                    }))
                    .into_any_element(),
            )
        });
        v_flex()
            .gap_3()
            .child(Label::new("Inline review comments").size(LabelSize::Large))
            .children(groups)
            .into_any_element()
    }
}
