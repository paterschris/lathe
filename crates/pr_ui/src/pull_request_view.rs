use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use collections::{HashMap, HashSet};
use editor::{
    Anchor, Editor, EditorEvent, MultiBuffer,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    multibuffer_context_lines,
};
use git::{
    DiffCommentSide, GitHostAuth, GitHostingProvider, ParsedGitRemote, PullRequestDetail,
    PullRequestMergeMethod, PullRequestReviewComment, PullRequestReviewVerdict,
    PullRequestReviewer, PullRequestState,
};
use gpui::{
    AsyncApp, ClickEvent, DragMoveEvent, Empty, Entity, EventEmitter, FocusHandle, Focusable,
    Pixels, SharedString, Subscription, Task, WeakEntity, http_client::HttpClient,
};
use language::{
    Buffer, Capability, DiskState, File, LanguageRegistry, LineEnding, Point, ReplicaId, Rope,
    TextBuffer,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use multi_buffer::{PathKey, ToPoint as _};
use notifications::status_toast::StatusToast;
use project::{Project, WorktreeId};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use ui::{
    Button, ButtonCommon, ButtonStyle, Clickable, Disclosure, Divider, TintColor, prelude::*,
};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

/// Read-only workspace item that presents a single pull request: metadata and
/// inline-review comments in a header strip, with the diff rendered in the same
/// multibuffer diff editor the git panel uses for staged changes.
pub struct PullRequestView {
    focus_handle: FocusHandle,
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: ParsedGitRemote,
    number: u32,
    workspace: WeakEntity<Workspace>,
    detail: Option<PullRequestDetail>,
    /// Rendered PR description (markdown). Built in `refresh` when the detail
    /// loads so the entity persists across frames.
    description_md: Option<Entity<Markdown>>,
    diff_files: Vec<ParsedDiffFile>,
    /// Multibuffer of synthesized per-file diffs, built off the unified patch in
    /// `refresh`. The editor over it is created lazily in `render`, which has the
    /// `Window` that `Editor::for_multibuffer` requires.
    diff_multibuffer: Option<Entity<MultiBuffer>>,
    diff_editor: Option<Entity<Editor>>,
    /// Inline review threads resolved to multibuffer anchors; each thread is
    /// inserted into the diff editor as one block decoration (replies indented)
    /// when the editor is (re)built.
    comment_threads: Vec<CommentThread>,
    /// Root ids of resolved threads the user has manually expanded. Resolved
    /// threads render collapsed by default; expanding one adds it here.
    expanded_threads: HashSet<u64>,
    /// Whether the description section is expanded (collapse hides its body but
    /// keeps the action buttons).
    description_expanded: bool,
    /// Custom description body height once the user drags the resize handle;
    /// `None` uses the default max height.
    description_height: Option<Pixels>,
    /// Markdown render entities for the inline comments, kept alive and observed
    /// so the diff editor re-measures (and grows) its comment blocks once their
    /// async parsing finishes. Without this, freshly-parsed comments overflow
    /// the single row a block is initially given.
    _comment_markdowns: Vec<Entity<Markdown>>,
    _comment_md_subscriptions: Vec<Subscription>,
    /// Reloads the view when a git host is (re)connected, so an expired account
    /// the user reconnects refreshes without reopening the pull request.
    _connection_subscription: Subscription,
    error: Option<SharedString>,
    /// Set instead of `error` when a load fails with HTTP 401, so the view can
    /// offer a targeted reconnect for this host rather than a generic failure.
    auth_error_host: Option<SharedString>,
    loading: bool,
    /// True while an approve / request-changes / merge call is in flight; the
    /// header buttons disable themselves to prevent double-fires.
    in_flight_action: bool,
    /// Root comment id of the thread whose inline reply composer is open.
    reply_target: Option<u64>,
    /// Composer for the open reply; created on `start_reply`, cleared on send or
    /// cancel. Rendered inside the target thread's diff-editor block.
    reply_editor: Option<Entity<Editor>>,
    reply_in_flight: bool,
    _load_task: Option<Task<()>>,
    _action_task: Option<Task<()>>,
    _reply_task: Option<Task<()>>,
    /// Per-file maps (path, buffer, line<->row) retained from the last diff build
    /// so a gutter "+" click can be resolved back to (path, new-file line).
    diff_file_maps: Vec<DiffFileMap>,
    /// State for an open "new inline comment" composer, opened from the diff
    /// gutter "+"; distinct from the reply composer above.
    new_comment: Option<NewComment>,
    new_comment_editor: Option<Entity<Editor>>,
    new_comment_in_flight: bool,
    _new_comment_task: Option<Task<()>>,
    /// Subscription to the current diff editor for its `AddPrCommentRequested`
    /// gutter events; re-created whenever the diff editor is (re)built.
    _diff_editor_subscription: Option<Subscription>,
}

/// An open composer for a brand-new inline review comment anchored at a diff
/// line (as opposed to a reply). `block_id` is the composer's decoration in the
/// diff editor, removed on cancel or successful submit.
struct NewComment {
    path: String,
    line: u32,
    block_id: Option<CustomBlockId>,
}

impl PullRequestView {
    pub fn new(
        provider: Arc<dyn GitHostingProvider + Send + Sync>,
        remote: ParsedGitRemote,
        number: u32,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let connection_subscription =
            git::git_host_credentials::observe_connections(cx, |this, cx| this.refresh(cx));
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            provider,
            remote,
            number,
            workspace,
            detail: None,
            description_md: None,
            diff_files: Vec::new(),
            diff_multibuffer: None,
            diff_editor: None,
            comment_threads: Vec::new(),
            expanded_threads: HashSet::default(),
            description_expanded: true,
            description_height: None,
            _comment_markdowns: Vec::new(),
            _comment_md_subscriptions: Vec::new(),
            _connection_subscription: connection_subscription,
            error: None,
            auth_error_host: None,
            loading: true,
            in_flight_action: false,
            reply_target: None,
            reply_editor: None,
            reply_in_flight: false,
            _load_task: None,
            _action_task: None,
            _reply_task: None,
            diff_file_maps: Vec::new(),
            new_comment: None,
            new_comment_editor: None,
            new_comment_in_flight: false,
            _new_comment_task: None,
            _diff_editor_subscription: None,
        };
        this.refresh(cx);
        this
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
            PullRequestReviewVerdict::RequestChanges => "Requested changes on pull request".into(),
            PullRequestReviewVerdict::Comment => "Posted review comment".into(),
        };
        let provider = self.provider.clone();
        let host = provider.base_url().host_str().map(|host| host.to_string());
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        let workspace = self.workspace.clone();
        self.in_flight_action = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let auth = match host.as_deref() {
                Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let result = provider
                .submit_review(&remote, number, verdict, None, auth, http_client)
                .await;
            this.update(cx, |this, cx| {
                this.in_flight_action = false;
                match result {
                    Ok(()) => surface_toast(&workspace, label, cx),
                    Err(error) => {
                        surface_toast(&workspace, action_error_message("Review", &error), cx);
                    }
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        });
        self._action_task = Some(task);
    }

    /// Retract the viewer's own approval / requested-changes. Called when they
    /// click a review button they've already submitted, making those buttons act
    /// as toggles rather than re-submitting the same verdict.
    fn retract_review(&mut self, verdict: PullRequestReviewVerdict, cx: &mut Context<Self>) {
        if self.in_flight_action {
            return;
        }
        let label: SharedString = match verdict {
            PullRequestReviewVerdict::Approve => "Removed approval".into(),
            PullRequestReviewVerdict::RequestChanges => "Removed requested changes".into(),
            PullRequestReviewVerdict::Comment => "Removed review".into(),
        };
        let provider = self.provider.clone();
        let host = provider.base_url().host_str().map(|host| host.to_string());
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        let workspace = self.workspace.clone();
        self.in_flight_action = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let auth = match host.as_deref() {
                Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let result = provider
                .remove_review(&remote, number, verdict, auth, http_client)
                .await;
            this.update(cx, |this, cx| {
                this.in_flight_action = false;
                match result {
                    Ok(()) => surface_toast(&workspace, label, cx),
                    Err(error) => {
                        surface_toast(&workspace, action_error_message("Review", &error), cx);
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
        let host = provider.base_url().host_str().map(|host| host.to_string());
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        let workspace = self.workspace.clone();
        self.in_flight_action = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let auth = match host.as_deref() {
                Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let result = provider
                .merge_pull_request(&remote, number, method, auth, http_client)
                .await;
            this.update(cx, |this, cx| {
                this.in_flight_action = false;
                match result {
                    Ok(()) => surface_toast(
                        &workspace,
                        format!("Merged PR #{number} via {method_label}").into(),
                        cx,
                    ),
                    Err(error) => {
                        surface_toast(&workspace, action_error_message("Merge", &error), cx)
                    }
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        });
        self._action_task = Some(task);
    }

    /// Opens an inline reply composer for the thread rooted at `root_id`. A fresh
    /// auto-height editor is created and focused; it is rendered inside that
    /// thread's diff-editor block (see `render_comment_thread`).
    fn start_reply(&mut self, root_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.reply_in_flight {
            return;
        }
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
            editor
        });
        editor.focus_handle(cx).focus(window, cx);
        self.reply_target = Some(root_id);
        self.reply_editor = Some(editor);
        cx.notify();
    }

    fn cancel_reply(&mut self, cx: &mut Context<Self>) {
        self.reply_target = None;
        self.reply_editor = None;
        cx.notify();
    }

    /// Expands or collapses a resolved thread. Notifies the diff editor (not the
    /// whole view) so only the affected block re-renders, avoiding a tab repaint.
    fn toggle_thread_expanded(&mut self, root_id: u64, cx: &mut Context<Self>) {
        if !self.expanded_threads.remove(&root_id) {
            self.expanded_threads.insert(root_id);
        }
        if let Some(editor) = self.diff_editor.clone() {
            editor.update(cx, |_editor, cx| cx.notify());
        }
    }

    fn submit_reply(&mut self, cx: &mut Context<Self>) {
        if self.reply_in_flight {
            return;
        }
        let Some(root_id) = self.reply_target else {
            return;
        };
        let Some(editor) = self.reply_editor.clone() else {
            return;
        };
        let body = editor.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            return;
        }
        let provider = self.provider.clone();
        let host = provider.base_url().host_str().map(|host| host.to_string());
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        let workspace = self.workspace.clone();
        self.reply_in_flight = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let auth = match host.as_deref() {
                Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let result = provider
                .post_review_comment(&remote, number, root_id, body.into(), auth, http_client)
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
                        surface_toast(&workspace, action_error_message("Reply", &error), cx)
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self._reply_task = Some(task);
    }

    fn handle_diff_editor_event(
        &mut self,
        _editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let EditorEvent::AddPrCommentRequested { anchor } = event {
            self.start_new_comment(*anchor, window, cx);
        }
    }

    /// Resolve a diff-editor gutter anchor back to (file path, new-file line) and
    /// open an inline composer block there for a new RIGHT-side review comment.
    fn start_new_comment(&mut self, anchor: Anchor, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multibuffer) = self.diff_multibuffer.clone() else {
            return;
        };
        let mb_snapshot = multibuffer.read(cx).snapshot(cx);
        let point = anchor.to_point(&mb_snapshot);
        let Some((buffer_snapshot, buffer_point)) = mb_snapshot.point_to_buffer_point(point) else {
            return;
        };
        let buffer_id = buffer_snapshot.remote_id();
        let row = buffer_point.row;
        let Some(map) = self
            .diff_file_maps
            .iter()
            .find(|map| map.buffer.read(cx).remote_id() == buffer_id)
        else {
            return;
        };
        let path = map.path.clone();
        // Only RIGHT-side (new-file) lines are supported for now. Full-content
        // buffers map row R to new-file line R+1; patch buffers carry an explicit
        // line->row map that we reverse. A row with no new-file line (a pure
        // deletion) cannot anchor a RIGHT-side comment yet.
        let line = if map.full_content {
            row + 1
        } else {
            match map
                .line_to_row
                .iter()
                .find(|entry| *entry.1 == row)
                .map(|entry| *entry.0)
            {
                Some(line) => line,
                None => {
                    surface_toast(
                        &self.workspace,
                        "Inline comments can only be added on added or context lines.".into(),
                        cx,
                    );
                    return;
                }
            }
        };

        // Replace any composer that is already open.
        self.cancel_new_comment(cx);

        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text("Comment…", window, cx);
            editor
        });
        editor.focus_handle(cx).focus(window, cx);
        self.new_comment_editor = Some(editor);

        let weak_view = cx.weak_entity();
        let block_id = self.diff_editor.as_ref().and_then(|diff_editor| {
            diff_editor.update(cx, |diff_editor, cx| {
                diff_editor
                    .insert_blocks(
                        vec![BlockProperties {
                            placement: BlockPlacement::Below(anchor),
                            height: Some(1),
                            style: BlockStyle::Flex,
                            render: Arc::new(move |block_cx| {
                                render_new_comment_composer(&weak_view, block_cx)
                            }),
                            priority: 0,
                        }],
                        None,
                        cx,
                    )
                    .into_iter()
                    .next()
            })
        });

        self.new_comment = Some(NewComment {
            path,
            line,
            block_id,
        });
        cx.notify();
    }

    fn cancel_new_comment(&mut self, cx: &mut Context<Self>) {
        if let Some(new_comment) = self.new_comment.take() {
            if let (Some(block_id), Some(diff_editor)) =
                (new_comment.block_id, self.diff_editor.clone())
            {
                diff_editor.update(cx, |diff_editor, cx| {
                    diff_editor.remove_blocks(HashSet::from_iter([block_id]), None, cx);
                });
            }
        }
        self.new_comment_editor = None;
        cx.notify();
    }

    fn submit_new_comment(&mut self, cx: &mut Context<Self>) {
        if self.new_comment_in_flight {
            return;
        }
        let Some(new_comment) = self.new_comment.as_ref() else {
            return;
        };
        let (path, line) = (new_comment.path.clone(), new_comment.line);
        let Some(editor) = self.new_comment_editor.clone() else {
            return;
        };
        let body = editor.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            return;
        }
        let commit_id = self
            .detail
            .as_ref()
            .map(|detail| detail.head_sha.clone())
            .unwrap_or_default();
        let provider = self.provider.clone();
        let host = provider.base_url().host_str().map(|host| host.to_string());
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        let workspace = self.workspace.clone();
        self.new_comment_in_flight = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let auth = match host.as_deref() {
                Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let result = provider
                .create_review_comment(
                    &remote,
                    number,
                    commit_id,
                    path.into(),
                    line,
                    DiffCommentSide::Right,
                    body.into(),
                    auth,
                    http_client,
                )
                .await;
            this.update(cx, |this, cx| {
                this.new_comment_in_flight = false;
                match result {
                    Ok(()) => {
                        this.cancel_new_comment(cx);
                        surface_toast(&workspace, "Posted comment".into(), cx);
                        this.refresh(cx);
                    }
                    Err(error) => {
                        surface_toast(&workspace, action_error_message("Comment", &error), cx)
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self._new_comment_task = Some(task);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let provider = self.provider.clone();
        let host = provider.base_url().host_str().map(|host| host.to_string());
        let remote = clone_remote(&self.remote);
        let number = self.number;
        let http_client = cx.http_client();
        self.loading = true;
        self.error = None;
        self.auth_error_host = None;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let auth = match host.as_deref() {
                Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let detail_fut =
                provider.get_pull_request(&remote, number, auth.clone(), http_client.clone());
            let comments_fut = provider.get_pull_request_comments(
                &remote,
                number,
                auth.clone(),
                http_client.clone(),
            );
            let diff_fut =
                provider.get_pull_request_diff(&remote, number, auth.clone(), http_client.clone());
            let (detail, comments, diff) =
                futures::future::join3(detail_fut, comments_fut, diff_fut).await;
            let files = match diff {
                Ok(text) => parse_unified_diff(&text),
                Err(error) => {
                    log::warn!("loading PR #{number} diff failed: {error:?}");
                    Vec::new()
                }
            };
            let comments = match comments {
                Ok(comments) => comments,
                Err(error) => {
                    // Comments failing is non-fatal; the header still renders.
                    log::warn!("loading PR #{number} comments failed: {error:?}");
                    Vec::new()
                }
            };
            // Read the project here, inside the async task, rather than in the
            // synchronous body of `refresh`: `refresh` is called from `new`
            // while the workspace is still being updated (the item is opened
            // inside `workspace.update`), and reading the workspace during that
            // update panics with a reentrant borrow.
            let project = this
                .update(cx, |this, cx| {
                    this.workspace
                        .upgrade()
                        .map(|workspace| workspace.read(cx).project().clone())
                })
                .ok()
                .flatten();
            // Everything needed to fetch full file content on demand, so review
            // comments on unchanged lines can be anchored to their exact line.
            let build_context = DiffBuildContext {
                provider: provider.clone(),
                remote: clone_remote(&remote),
                head_sha: detail
                    .as_ref()
                    .ok()
                    .map(|detail| detail.head_sha.to_string()),
                base_sha: detail
                    .as_ref()
                    .ok()
                    .map(|detail| detail.base_sha.to_string()),
                auth,
                http_client,
            };
            // Build a read-only multibuffer of per-file diffs so the editor can
            // render them with the gutter, syntax highlighting, and red/green
            // styling the git panel uses for staged changes. Files that carry
            // review comments are built from full content (windowed to the
            // relevant rows) so the comments anchor to their exact line.
            let (multibuffer, file_maps) = match project {
                Some(project) if !files.is_empty() || !comments.is_empty() => {
                    build_diff_multibuffer(&files, &comments, &build_context, project, cx)
                        .await
                        .log_err()
                        .unwrap_or((None, Vec::new()))
                }
                _ => (None, Vec::new()),
            };
            this.update(cx, |this, cx| {
                let language_registry = this
                    .workspace
                    .upgrade()
                    .map(|workspace| workspace.read(cx).project().read(cx).languages().clone());
                match detail {
                    Ok(detail) => {
                        this.description_md = if detail.body.is_empty() {
                            None
                        } else {
                            let body = detail.body.clone();
                            Some(cx.new(|cx| Markdown::new(body, language_registry, None, cx)))
                        };
                        this.detail = Some(detail);
                    }
                    Err(e) => match e.downcast_ref::<git::PullRequestAuthError>() {
                        Some(auth_error) => this.auth_error_host = Some(auth_error.host.clone()),
                        None => this.error = Some(format!("{e}").into()),
                    },
                }
                this.comment_threads =
                    resolve_comment_threads(&comments, &file_maps, multibuffer.as_ref(), cx);
                this.diff_file_maps = file_maps;
                this.diff_files = files;
                this.diff_multibuffer = multibuffer;
                this.diff_editor = None;
                // The diff editor is rebuilt by `ensure_diff_editor`, so drop any
                // open new-comment composer and its subscription to the old editor.
                this._diff_editor_subscription = None;
                this.new_comment = None;
                this.new_comment_editor = None;
                this.new_comment_in_flight = false;
                this._new_comment_task = None;
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
    /// 1-indexed start line of this hunk in the new (post-image) file, parsed
    /// from the `@@ -a,b +c,d @@` header. Used to map review-comment line
    /// numbers onto rows in the reconstructed buffer.
    new_start: u32,
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
                new_start: parse_hunk_new_start(line).unwrap_or(1),
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

/// Extracts the new-file start line from a `@@ -a,b +c,d @@` hunk header.
fn parse_hunk_new_start(header: &str) -> Option<u32> {
    let after_plus = header.split('+').nth(1)?;
    let number = after_plus.split(|ch: char| ch == ',' || ch == ' ').next()?;
    number.parse::<u32>().ok()
}

const FILE_NAMESPACE_SORT_PREFIX: u64 = 0;

/// Rebuilds approximate base/head text for a file from its parsed hunks. A
/// unified patch only carries the changed regions plus a few context lines, so
/// the resulting buffers contain those regions (like a unified diff view on the
/// web) rather than the whole file. `None` base text means the file is new.
fn reconstruct_file_texts(file: &ParsedDiffFile) -> (Option<String>, String, HashMap<u32, u32>) {
    let mut old_text = String::new();
    let mut new_text = String::new();
    let mut line_to_row: HashMap<u32, u32> = HashMap::default();
    let mut row: u32 = 0;
    for hunk in &file.hunks {
        let mut new_line = hunk.new_start;
        for line in &hunk.lines {
            match line {
                ParsedDiffLine::Context(text) => {
                    old_text.push_str(text);
                    old_text.push('\n');
                    new_text.push_str(text);
                    new_text.push('\n');
                    line_to_row.insert(new_line, row);
                    new_line += 1;
                    row += 1;
                }
                ParsedDiffLine::Deletion(text) => {
                    old_text.push_str(text);
                    old_text.push('\n');
                }
                ParsedDiffLine::Addition(text) => {
                    new_text.push_str(text);
                    new_text.push('\n');
                    line_to_row.insert(new_line, row);
                    new_line += 1;
                    row += 1;
                }
            }
        }
    }
    let old_text = if old_text.is_empty() {
        None
    } else {
        Some(old_text)
    };
    (old_text, new_text, line_to_row)
}

/// Everything needed to fetch full file content while building the diff, so that
/// review comments on unchanged lines can be anchored to their exact line.
struct DiffBuildContext {
    provider: Arc<dyn GitHostingProvider + Send + Sync>,
    remote: ParsedGitRemote,
    head_sha: Option<String>,
    base_sha: Option<String>,
    auth: Option<GitHostAuth>,
    http_client: Arc<dyn HttpClient>,
}

/// Builds a read-only multibuffer of per-file diffs, the same kind the git panel
/// renders for staged changes.
///
/// Files without comments are reconstructed cheaply from the unified patch.
/// Files that carry review comments are built from their full head content
/// (fetched from the host) so a comment on an unchanged line still has a real row
/// to anchor to; only the relevant rows are shown (the changed hunks plus a
/// window around each commented line). Comments on files that are not part of the
/// diff at all are surfaced the same way, as content-only windows.
async fn build_diff_multibuffer(
    files: &[ParsedDiffFile],
    comments: &[PullRequestReviewComment],
    build_context: &DiffBuildContext,
    project: Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<(Option<Entity<MultiBuffer>>, Vec<DiffFileMap>)> {
    let (language_registry, worktree_id) = cx.update(|cx| {
        let project = project.read(cx);
        (
            project.languages().clone(),
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id()),
        )
    });
    let Some(worktree_id) = worktree_id else {
        return Ok((None, Vec::new()));
    };

    let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadOnly));
    let mut file_maps = Vec::new();
    let mut handled_paths: HashSet<String> = HashSet::default();

    for file in files {
        let comments_for_file: Vec<&PullRequestReviewComment> = comments
            .iter()
            .filter(|comment| paths_match(&file.path, comment.path.as_ref()))
            .collect();

        let map = if comments_for_file.is_empty() {
            add_patch_file(&multibuffer, file, worktree_id, &language_registry, cx).await
        } else {
            handled_paths.insert(file.path.clone());
            // Build from full content so each commented line exists; fall back to
            // the patch reconstruction if the fetch fails.
            match add_changed_file_full(
                &multibuffer,
                file,
                &comments_for_file,
                build_context,
                worktree_id,
                &language_registry,
                cx,
            )
            .await
            {
                Ok(map) => Ok(map),
                Err(error) => {
                    log::warn!(
                        "full content for {} failed: {error:?}; using patch instead",
                        file.path
                    );
                    add_patch_file(&multibuffer, file, worktree_id, &language_registry, cx).await
                }
            }
        };
        match map {
            Ok(Some(map)) => file_maps.push(map),
            Ok(None) => {}
            Err(error) => log::warn!("building diff for {} failed: {error:?}", file.path),
        }
    }

    // Comments on files that are not part of the diff (unchanged code): fetch the
    // file and show only the rows around each comment.
    let mut standalone: HashMap<String, Vec<&PullRequestReviewComment>> = HashMap::default();
    for comment in comments {
        let path = comment.path.as_ref();
        if path.is_empty() || handled_paths.contains(path) {
            continue;
        }
        if files.iter().any(|file| paths_match(&file.path, path)) {
            continue;
        }
        standalone
            .entry(path.to_string())
            .or_default()
            .push(comment);
    }
    for (path, file_comments) in standalone {
        match add_unchanged_file(
            &multibuffer,
            &path,
            &file_comments,
            build_context,
            worktree_id,
            &language_registry,
            cx,
        )
        .await
        {
            Ok(Some(map)) => file_maps.push(map),
            Ok(None) => {}
            Err(error) => log::warn!("content for unchanged {path} failed: {error:?}"),
        }
    }

    let multibuffer = (!file_maps.is_empty()).then_some(multibuffer);
    Ok((multibuffer, file_maps))
}

/// Reconstructs a file's diff buffer from the unified patch (changed hunks plus a
/// few context lines) and adds it to the multibuffer. Cheap; no network.
async fn add_patch_file(
    multibuffer: &Entity<MultiBuffer>,
    file: &ParsedDiffFile,
    worktree_id: WorktreeId,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<Option<DiffFileMap>> {
    let (old_text, new_text, line_to_row) = reconstruct_file_texts(file);
    if new_text.is_empty() && old_text.is_none() {
        return Ok(None);
    }
    let Ok(rel_path) = RelPath::unix(&file.path) else {
        return Ok(None);
    };
    let rel_path: Arc<RelPath> = rel_path.into();
    let blob = pr_diff_file(
        rel_path.clone(),
        worktree_id,
        new_text.is_empty(),
        &file.path,
    );
    let buffer = build_buffer(new_text, blob, language_registry, cx).await?;
    let buffer_diff = build_buffer_diff(old_text, &buffer, language_registry, cx).await?;

    let excerpt_buffer = buffer.clone();
    multibuffer.update(cx, |multibuffer, cx| {
        let snapshot = excerpt_buffer.read(cx).snapshot();
        let excerpt_ranges = vec![Point::zero()..snapshot.max_point()];
        multibuffer.set_excerpts_for_path(
            PathKey::with_sort_prefix(FILE_NAMESPACE_SORT_PREFIX, rel_path.clone()),
            excerpt_buffer,
            excerpt_ranges,
            multibuffer_context_lines(cx),
            cx,
        );
        multibuffer.add_diff(buffer_diff, cx);
    });
    Ok(Some(DiffFileMap {
        path: file.path.clone(),
        buffer,
        line_to_row,
        full_content: false,
    }))
}

/// Builds a changed file from its full head content (so every commented line
/// exists) and diffs it against the base. Shows only the changed hunks plus a
/// window around each commented line.
async fn add_changed_file_full(
    multibuffer: &Entity<MultiBuffer>,
    file: &ParsedDiffFile,
    comments: &[&PullRequestReviewComment],
    build_context: &DiffBuildContext,
    worktree_id: WorktreeId,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<Option<DiffFileMap>> {
    let head_sha = build_context
        .head_sha
        .as_deref()
        .context("pull request head SHA unavailable")?;
    let head_text = build_context
        .provider
        .get_file_content(
            &build_context.remote,
            &file.path,
            head_sha,
            build_context.auth.clone(),
            build_context.http_client.clone(),
        )
        .await?;
    // Base content lets the diff highlight the change. A file added in this PR has
    // no base, so a fetch failure there is non-fatal.
    let base_text = match build_context.base_sha.as_deref() {
        Some(base_sha) => build_context
            .provider
            .get_file_content(
                &build_context.remote,
                &file.path,
                base_sha,
                build_context.auth.clone(),
                build_context.http_client.clone(),
            )
            .await
            .ok(),
        None => None,
    };

    let Ok(rel_path) = RelPath::unix(&file.path) else {
        return Ok(None);
    };
    let rel_path: Arc<RelPath> = rel_path.into();
    let blob = pr_diff_file(rel_path.clone(), worktree_id, false, &file.path);
    let buffer = build_buffer(head_text, blob, language_registry, cx).await?;
    let buffer_diff = build_buffer_diff(base_text, &buffer, language_registry, cx).await?;

    let excerpt_buffer = buffer.clone();
    multibuffer.update(cx, |multibuffer, cx| {
        let snapshot = excerpt_buffer.read(cx).snapshot();
        let max_point = snapshot.max_point();
        let mut ranges: Vec<Range<Point>> = Vec::new();
        for hunk in &file.hunks {
            if let Some((start, end)) = hunk_new_span(hunk) {
                ranges.push(line_range(start, end, max_point));
            }
        }
        for comment in comments {
            ranges.push(comment_range(comment.line, max_point));
        }
        if ranges.is_empty() {
            ranges.push(Point::zero()..Point::zero());
        }
        multibuffer.set_excerpts_for_path(
            PathKey::with_sort_prefix(FILE_NAMESPACE_SORT_PREFIX, rel_path.clone()),
            excerpt_buffer,
            ranges,
            multibuffer_context_lines(cx),
            cx,
        );
        multibuffer.add_diff(buffer_diff, cx);
    });
    Ok(Some(DiffFileMap {
        path: file.path.clone(),
        buffer,
        line_to_row: HashMap::default(),
        full_content: true,
    }))
}

/// Fetches a file that is unchanged in the PR but carries comments, and shows
/// just the rows around each comment, with no diff highlighting.
async fn add_unchanged_file(
    multibuffer: &Entity<MultiBuffer>,
    path: &str,
    comments: &[&PullRequestReviewComment],
    build_context: &DiffBuildContext,
    worktree_id: WorktreeId,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<Option<DiffFileMap>> {
    let head_sha = build_context
        .head_sha
        .as_deref()
        .context("pull request head SHA unavailable")?;
    let head_text = build_context
        .provider
        .get_file_content(
            &build_context.remote,
            path,
            head_sha,
            build_context.auth.clone(),
            build_context.http_client.clone(),
        )
        .await?;
    let Ok(rel_path) = RelPath::unix(path) else {
        return Ok(None);
    };
    let rel_path: Arc<RelPath> = rel_path.into();
    let blob = pr_diff_file(rel_path.clone(), worktree_id, false, path);
    let buffer = build_buffer(head_text, blob, language_registry, cx).await?;

    let excerpt_buffer = buffer.clone();
    multibuffer.update(cx, |multibuffer, cx| {
        let snapshot = excerpt_buffer.read(cx).snapshot();
        let max_point = snapshot.max_point();
        let mut ranges: Vec<Range<Point>> = comments
            .iter()
            .map(|comment| comment_range(comment.line, max_point))
            .collect();
        if ranges.is_empty() {
            ranges.push(Point::zero()..Point::zero());
        }
        multibuffer.set_excerpts_for_path(
            PathKey::with_sort_prefix(FILE_NAMESPACE_SORT_PREFIX, rel_path.clone()),
            excerpt_buffer,
            ranges,
            multibuffer_context_lines(cx),
            cx,
        );
    });
    Ok(Some(DiffFileMap {
        path: path.to_string(),
        buffer,
        line_to_row: HashMap::default(),
        full_content: true,
    }))
}

/// Builds the synthetic [`language::File`] for a diff buffer from its path.
fn pr_diff_file(
    path: Arc<RelPath>,
    worktree_id: WorktreeId,
    is_deleted: bool,
    full_path: &str,
) -> Arc<dyn File> {
    let display_name = full_path
        .rsplit('/')
        .next()
        .unwrap_or(full_path)
        .to_string();
    Arc::new(PrDiffFile {
        path,
        worktree_id,
        is_deleted,
        display_name,
    }) as Arc<dyn File>
}

/// New-file line span (1-based, inclusive) that a hunk covers, or `None` if it
/// adds no new-side lines (a pure deletion).
fn hunk_new_span(hunk: &ParsedDiffHunk) -> Option<(u32, u32)> {
    let new_len = hunk
        .lines
        .iter()
        .filter(|line| {
            matches!(
                line,
                ParsedDiffLine::Context(_) | ParsedDiffLine::Addition(_)
            )
        })
        .count() as u32;
    (new_len > 0).then(|| (hunk.new_start, hunk.new_start + new_len - 1))
}

/// Converts a 1-based inclusive line span to a `Point` range clamped to the
/// buffer.
fn line_range(start_line: u32, end_line: u32, max_point: Point) -> Range<Point> {
    let start = Point::new(start_line.saturating_sub(1), 0).min(max_point);
    let end = Point::new(end_line.saturating_sub(1), 0).min(max_point);
    start..end
}

/// Single-line `Point` range for a comment's line (or the file top when it pins
/// no line), clamped to the buffer.
fn comment_range(line: Option<u32>, max_point: Point) -> Range<Point> {
    let row = line.unwrap_or(1).saturating_sub(1);
    let point = Point::new(row, 0).min(max_point);
    point..point
}

/// Per-file buffer captured while building the diff multibuffer, used to anchor
/// review comments to their lines.
struct DiffFileMap {
    /// Repo-relative path of the file, as it appears in the diff.
    path: String,
    buffer: Entity<Buffer>,
    /// New-file line (1-indexed) -> buffer row (0-indexed). Only populated for
    /// patch-reconstructed files; full-content files map line N to row N-1.
    line_to_row: HashMap<u32, u32>,
    /// True when `buffer` holds the file's full content (so any line anchors),
    /// false when it is reconstructed from the patch (only hunk lines exist).
    full_content: bool,
}

/// One comment within a resolved review thread, tagged with its nesting depth
/// (0 = top-level) so replies can be indented in the diff view.
#[derive(Clone)]
struct ThreadComment {
    author: SharedString,
    location: SharedString,
    created_at: SharedString,
    body: SharedString,
    depth: usize,
}

/// A review thread resolved to a single multibuffer anchor (its root comment's
/// line). Rendered as one editor block so a thread's replies stack and indent
/// together instead of each comment competing for the same row.
#[derive(Clone)]
struct CommentThread {
    anchor: Anchor,
    /// Id of the thread's root comment; the target passed to `post_review_comment`
    /// when replying.
    root_id: u64,
    /// Whether the host marks this thread resolved. Resolved threads render
    /// collapsed by default.
    resolved: bool,
    comments: Vec<ThreadComment>,
}

/// Walks parent links to the id of the comment at the root of `comment`'s
/// thread. Bounded so a malformed parent cycle can't loop forever.
fn thread_root_id(
    by_id: &HashMap<u64, &PullRequestReviewComment>,
    comment: &PullRequestReviewComment,
) -> u64 {
    let mut current = comment;
    for _ in 0..64 {
        match current
            .parent_id
            .and_then(|parent_id| by_id.get(&parent_id))
        {
            Some(parent) => current = parent,
            None => break,
        }
    }
    current.id
}

/// Nesting depth of `comment` within its thread (0 for the root). Bounded like
/// [`thread_root_id`].
fn thread_depth(
    by_id: &HashMap<u64, &PullRequestReviewComment>,
    comment: &PullRequestReviewComment,
) -> usize {
    let mut current = comment;
    let mut depth = 0;
    for _ in 0..64 {
        match current
            .parent_id
            .and_then(|parent_id| by_id.get(&parent_id))
        {
            Some(parent) => {
                current = parent;
                depth += 1;
            }
            None => break,
        }
    }
    depth
}

/// Groups review comments into threads and resolves each thread to a multibuffer
/// anchor at its root comment's line. A comment whose path matches no diff file
/// (e.g. a PR-level comment) anchors to the top of the first file so it still
/// surfaces in the diff view.
fn resolve_comment_threads(
    comments: &[PullRequestReviewComment],
    file_maps: &[DiffFileMap],
    multibuffer: Option<&Entity<MultiBuffer>>,
    cx: &App,
) -> Vec<CommentThread> {
    let Some(multibuffer) = multibuffer else {
        return Vec::new();
    };
    let snapshot = multibuffer.read(cx).snapshot(cx);

    let by_id: HashMap<u64, &PullRequestReviewComment> = comments
        .iter()
        .map(|comment| (comment.id, comment))
        .collect();

    // Group comments by thread root, preserving first-seen order so threads
    // appear in the order the host returned them.
    let mut order: Vec<u64> = Vec::new();
    let mut groups: HashMap<u64, Vec<ThreadComment>> = HashMap::default();
    for comment in comments {
        let root_id = thread_root_id(&by_id, comment);
        let depth = thread_depth(&by_id, comment);
        if !groups.contains_key(&root_id) {
            order.push(root_id);
        }
        let location = match comment.line {
            Some(line) => SharedString::from(format!("line {line}")),
            None => SharedString::from("file"),
        };
        groups.entry(root_id).or_default().push(ThreadComment {
            author: comment.author_login.clone(),
            location,
            created_at: comment.created_at.clone(),
            body: comment.body.clone(),
            depth,
        });
    }

    let mut threads = Vec::new();
    for root_id in order {
        let Some(mut thread_comments) = groups.remove(&root_id) else {
            continue;
        };
        // Root first, then replies; a stable sort keeps siblings in host order.
        thread_comments.sort_by_key(|comment| comment.depth);
        let Some(&root) = by_id.get(&root_id) else {
            continue;
        };

        // Match the thread to its file by path, then anchor on a real row only
        // when the comment's line is actually present in the reconstructed diff.
        // The unified patch carries only changed hunks, so an outdated comment,
        // an old-side line, or a line outside any hunk has no row to land on.
        // Rather than clamp it to the nearest hunk line (which makes the card
        // claim a line it isn't on), place it at the top of its file and relabel
        // it honestly below.
        let matched_file = file_maps
            .iter()
            .find(|file| paths_match(&file.path, root.path.as_ref()));
        let exact_row = matched_file.and_then(|file| {
            let line = root.line?;
            if file.full_content {
                // Full-content files hold every line, so line N is row N-1.
                Some(line.saturating_sub(1))
            } else {
                file.line_to_row.get(&line).copied()
            }
        });
        let Some(file) = matched_file.or_else(|| file_maps.first()) else {
            continue;
        };
        if exact_row.is_none()
            && let Some(root_card) = thread_comments.first_mut()
        {
            root_card.location = off_diff_location(root, matched_file.is_some());
        }
        let row = exact_row.unwrap_or(0);

        let buffer_snapshot = file.buffer.read(cx).snapshot();
        let point = Point::new(row, 0).min(buffer_snapshot.max_point());
        let text_anchor = buffer_snapshot.anchor_before(point);
        let Some(anchor) = snapshot.anchor_in_buffer(text_anchor) else {
            continue;
        };
        threads.push(CommentThread {
            anchor,
            root_id,
            resolved: root.is_resolved,
            comments: thread_comments,
        });
    }
    threads
}

fn paths_match(diff_path: &str, comment_path: &str) -> bool {
    if diff_path.is_empty() || comment_path.is_empty() {
        return false;
    }
    // Require a path-segment boundary for suffix matches, so `b.ts` cannot match
    // `lib.ts` and an empty path cannot accidentally match the first file (every
    // string `ends_with("")`).
    diff_path == comment_path
        || diff_path.ends_with(&format!("/{comment_path}"))
        || comment_path.ends_with(&format!("/{diff_path}"))
}

/// Label for a comment that could not be anchored to an exact line in the
/// reconstructed diff (an outdated comment, an old-side line, or a file/region
/// the unified patch does not include). Surfaces the real source location so the
/// card never masquerades as sitting on a line it isn't, and names the file when
/// the comment belongs to one not shown here.
fn off_diff_location(comment: &PullRequestReviewComment, file_matched: bool) -> SharedString {
    let path = comment.path.as_ref();
    match comment.line {
        Some(line) if file_matched || path.is_empty() => {
            format!("line {line} (not in diff)").into()
        }
        Some(line) => format!("{path}:{line} (not in diff)").into(),
        None if path.is_empty() => "general comment".into(),
        None => format!("{path} (file comment)").into(),
    }
}

/// Full markdown styling for PR descriptions and comments: prose body in the UI
/// font, code blocks and inline code in the buffer font with syntax colors,
/// styled block quotes and links. The previous minimal style left code blocks and
/// inline code unstyled, so fenced snippets rendered as plain indented text.
///
/// Inline code spans (single backticks) are additionally tinted orange so the
/// short `identifier`-style references reviewers drop into prose stand out from
/// the surrounding text and from fenced code blocks.
fn pr_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = MarkdownStyle::themed(MarkdownFont::Editor, window, cx);
    // Render the description prose at the same size as the small header labels so
    // it reads as part of the metadata block sitting above the action buttons,
    // rather than a larger prose section. Code spans/blocks keep the buffer font.
    style.base_text_style.font_size = ui::TextSize::Small.rems(cx).into();
    let orange: gpui::Hsla = gpui::rgb(0xE5_8E_2A).into();
    style.inline_code.color = Some(orange);
    style.inline_code.background_color = Some(orange.opacity(0.12));
    style
}

/// Renders one resolved review thread as the body of an editor block: a stack of
/// comment cards (replies indented by nesting depth so a chain reads as a
/// conversation), followed by an inline reply affordance. Clicking "Reply" opens
/// a composer in place; reading the open-reply state off the view here is safe
/// because block rendering runs during the diff editor's layout, after the view's
/// own `render` has returned.
/// Drag payload identifying the description resize handle.
#[derive(Clone)]
struct DescriptionResizeDrag;

/// A reviewer chip for the PR header: the reviewer's name plus a verdict icon
/// (green check = approved, red x = changes requested, muted dash = pending).
fn render_reviewer_chip(reviewer: &PullRequestReviewer) -> impl IntoElement {
    let (icon, color) = match reviewer.verdict {
        Some(PullRequestReviewVerdict::Approve) => (IconName::Check, Color::Success),
        Some(PullRequestReviewVerdict::RequestChanges) => (IconName::Close, Color::Error),
        // Comment-only and not-yet-reviewed both read as "no verdict yet".
        Some(PullRequestReviewVerdict::Comment) | None => (IconName::Dash, Color::Muted),
    };
    h_flex()
        .gap_1()
        .child(Icon::new(icon).size(IconSize::Small).color(color))
        .child(
            Label::new(reviewer.login.clone())
                .size(LabelSize::Small)
                .color(if reviewer.is_me {
                    Color::Default
                } else {
                    Color::Muted
                }),
        )
}

/// Renders the inline composer block for a brand-new review comment, anchored at
/// the clicked diff line. Mirrors the reply composer footer.
fn render_new_comment_composer(
    weak_view: &WeakEntity<PullRequestView>,
    block_cx: &mut BlockContext,
) -> AnyElement {
    let accent = block_cx.app.theme().colors().border_focused;
    let editor_background = block_cx.app.theme().colors().editor_background;

    let Some(view) = weak_view.upgrade() else {
        return Empty.into_any_element();
    };
    let (editor, in_flight, location) = {
        let view = view.read(block_cx.app);
        let location = view
            .new_comment
            .as_ref()
            .map(|comment| format!("{}:{}", comment.path, comment.line));
        (
            view.new_comment_editor.clone(),
            view.new_comment_in_flight,
            location,
        )
    };
    let (Some(editor), Some(location)) = (editor, location) else {
        return Empty.into_any_element();
    };

    v_flex()
        .gap_1()
        .child(
            Label::new(format!("New comment on {location}"))
                .color(Color::Muted)
                .size(LabelSize::Small),
        )
        .child(
            div()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(accent)
                .bg(editor_background)
                .child(editor),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("pr-new-comment-send", "Comment")
                        .style(ButtonStyle::Filled)
                        .disabled(in_flight)
                        .on_click({
                            let weak_view = weak_view.clone();
                            move |_, _window, cx| {
                                weak_view
                                    .update(cx, |view, cx| view.submit_new_comment(cx))
                                    .ok();
                            }
                        }),
                )
                .child(
                    Button::new("pr-new-comment-cancel", "Cancel")
                        .style(ButtonStyle::Outlined)
                        .disabled(in_flight)
                        .on_click({
                            let weak_view = weak_view.clone();
                            move |_, _window, cx| {
                                weak_view
                                    .update(cx, |view, cx| view.cancel_new_comment(cx))
                                    .ok();
                            }
                        }),
                )
                .when(in_flight, |this| {
                    this.child(Label::new("…").color(Color::Muted).size(LabelSize::Small))
                }),
        )
        .into_any_element()
}

fn render_comment_thread(
    comments: &[(ThreadComment, Entity<Markdown>)],
    root_id: u64,
    resolved: bool,
    weak_view: &WeakEntity<PullRequestView>,
    block_cx: &mut BlockContext,
) -> AnyElement {
    let border = block_cx.app.theme().colors().border_variant;
    let background = block_cx.app.theme().colors().element_background;
    let accent = block_cx.app.theme().colors().border_focused;
    let editor_background = block_cx.app.theme().colors().editor_background;
    let style = pr_markdown_style(block_cx.window, block_cx.app);

    let (reply_open, reply_editor, reply_in_flight, expanded) = match weak_view.upgrade() {
        Some(view) => {
            let view = view.read(block_cx.app);
            (
                view.reply_target == Some(root_id),
                view.reply_editor.clone(),
                view.reply_in_flight,
                view.expanded_threads.contains(&root_id),
            )
        }
        None => (false, None, false, false),
    };

    // A resolved thread collapses to a single decorator row until expanded.
    if resolved && !expanded {
        let author = comments
            .first()
            .map(|(comment, _)| comment.author.clone())
            .unwrap_or_default();
        let count = comments.len();
        return h_flex()
            .mx_4()
            .my_1()
            .p_2()
            .gap_2()
            .flex_wrap()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(background)
            .child(
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Success),
            )
            .child(
                Label::new("Resolved")
                    .color(Color::Success)
                    .size(LabelSize::Small),
            )
            .child(Label::new(author).size(LabelSize::Small))
            .child(
                Label::new(format!(
                    "{count} {}",
                    if count == 1 { "comment" } else { "comments" }
                ))
                .color(Color::Muted)
                .size(LabelSize::Small),
            )
            .child(
                Button::new(("pr-thread-expand", root_id as usize), "Show")
                    .style(ButtonStyle::Outlined)
                    .on_click({
                        let weak_view = weak_view.clone();
                        move |_, _window, cx| {
                            weak_view
                                .update(cx, |view, cx| view.toggle_thread_expanded(root_id, cx))
                                .ok();
                        }
                    }),
            )
            .into_any_element();
    }

    let reply_editor = reply_open.then_some(reply_editor).flatten();

    let cards = v_flex()
        .gap_1()
        .children(comments.iter().map(|(comment, markdown)| {
            // Indent replies; cap the depth so a deep chain stays on-screen.
            let indent = px((comment.depth.min(6) as f32) * 16.0);
            v_flex()
                .ml(indent)
                .p_2()
                .gap_1()
                .rounded_md()
                .border_1()
                .border_color(border)
                .bg(background)
                .when(comment.depth > 0, |this| {
                    this.border_l_2().border_color(accent)
                })
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(Label::new(comment.author.clone()).size(LabelSize::Small))
                        .when(comment.depth == 0, |this| {
                            this.child(
                                Label::new(comment.location.clone())
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            )
                        })
                        .child(
                            Label::new(comment.created_at.clone())
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                )
                .child(MarkdownElement::new(markdown.clone(), style.clone()))
        }));

    let footer = match reply_editor {
        Some(editor) => v_flex()
            .gap_1()
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(accent)
                    .bg(editor_background)
                    .child(editor),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(("pr-reply-send", root_id as usize), "Reply")
                            .style(ButtonStyle::Filled)
                            .disabled(reply_in_flight)
                            .on_click({
                                let weak_view = weak_view.clone();
                                move |_, _window, cx| {
                                    weak_view.update(cx, |view, cx| view.submit_reply(cx)).ok();
                                }
                            }),
                    )
                    .child(
                        Button::new(("pr-reply-cancel", root_id as usize), "Cancel")
                            .style(ButtonStyle::Outlined)
                            .disabled(reply_in_flight)
                            .on_click({
                                let weak_view = weak_view.clone();
                                move |_, _window, cx| {
                                    weak_view.update(cx, |view, cx| view.cancel_reply(cx)).ok();
                                }
                            }),
                    )
                    .when(reply_in_flight, |this| {
                        this.child(Label::new("…").color(Color::Muted).size(LabelSize::Small))
                    }),
            )
            .into_any_element(),
        None => Button::new(("pr-reply", root_id as usize), "Reply")
            .style(ButtonStyle::Outlined)
            .on_click({
                let weak_view = weak_view.clone();
                move |_, window, cx| {
                    weak_view
                        .update(cx, |view, cx| view.start_reply(root_id, window, cx))
                        .ok();
                }
            })
            .into_any_element(),
    };

    v_flex()
        .mx_4()
        .my_1()
        .gap_1()
        // An expanded resolved thread keeps the decorator plus a control to
        // collapse it again.
        .when(resolved, |this| {
            this.child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Check)
                            .size(IconSize::Small)
                            .color(Color::Success),
                    )
                    .child(
                        Label::new("Resolved")
                            .color(Color::Success)
                            .size(LabelSize::Small),
                    )
                    .child(
                        Button::new(("pr-thread-collapse", root_id as usize), "Hide")
                            .style(ButtonStyle::Outlined)
                            .on_click({
                                let weak_view = weak_view.clone();
                                move |_, _window, cx| {
                                    weak_view
                                        .update(cx, |view, cx| {
                                            view.toggle_thread_expanded(root_id, cx)
                                        })
                                        .ok();
                                }
                            }),
                    ),
            )
        })
        .child(cards)
        .child(footer)
        .into_any_element()
}

async fn build_buffer(
    mut text: String,
    blob: Arc<dyn File>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language = cx.update(|cx| language_registry.language_for_file(&blob, Some(&text), cx));
    let language = if let Some(language) = language {
        language_registry
            .load_language(&language)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadWrite);
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

async fn build_buffer_diff(
    mut old_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<Entity<BufferDiff>> {
    if let Some(old_text) = &mut old_text {
        LineEnding::normalize(old_text);
    }

    let language = cx.update(|cx| buffer.read(cx).language().cloned());
    let buffer = cx.update(|cx| buffer.read(cx).snapshot());

    // `BufferDiff::new` seeds the internal base-text buffer with the language and
    // registry so syntax highlighting is available for the deleted side, then
    // `set_base_text` populates that base buffer from `old_text` and computes and
    // applies the diff snapshot in one step.
    let diff =
        cx.new(|cx| BufferDiff::new(&buffer.text, language, Some(language_registry.clone()), cx));

    let base_text = old_text.map(|old_text| Arc::<str>::from(old_text.as_str()));
    diff.update(cx, |diff, cx| {
        diff.set_base_text(base_text, buffer.text.clone(), cx)
    })
    .await;

    Ok(diff)
}

/// Minimal read-only [`language::File`] for a pull-request diff buffer. These
/// buffers are synthesized from the unified patch and never touch disk, so the
/// file reports a historic disk state (mirroring the commit view's blobs).
struct PrDiffFile {
    path: Arc<RelPath>,
    worktree_id: WorktreeId,
    is_deleted: bool,
    display_name: String,
}

impl File for PrDiffFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.path.as_std_path().to_path_buf()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_str()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _: &App) -> language::proto::File {
        unimplemented!("pull-request diff files are display-only")
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        true
    }
}

fn clone_remote(remote: &ParsedGitRemote) -> ParsedGitRemote {
    ParsedGitRemote {
        owner: remote.owner.clone(),
        repo: remote.repo.clone(),
    }
}

/// Fire a small status toast through the workspace if it's still alive.
/// Builds the toast message for a failed PR action, turning an HTTP 401 into a
/// clear reconnect instruction instead of a raw error string. The action's
/// surface (panel / picker) is where the reconnect button lives, so the toast
/// points there rather than carrying its own button.
fn action_error_message(action: &str, error: &anyhow::Error) -> SharedString {
    match error.downcast_ref::<git::PullRequestAuthError>() {
        Some(auth_error) => {
            let display = git::git_host_credentials::GitHostKind::from_host(&auth_error.host)
                .map(|kind| kind.display_name())
                .unwrap_or("the git host");
            format!(
                "{action} failed: your {display} sign-in has expired. \
                 Reconnect from the pull request panel."
            )
            .into()
        }
        None => format!("{action} failed: {error}").into(),
    }
}

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

/// Extracts the `YYYY-MM-DD` calendar date from an ISO-8601 timestamp such as
/// `2026-05-19T12:34:56+00:00`, matching how the host's own UI labels PR dates.
/// Returns `None` for empty or malformed input so the header omits the chip
/// rather than rendering a partial timestamp.
fn iso_calendar_date(timestamp: &str) -> Option<SharedString> {
    let date = timestamp.split('T').next().unwrap_or_default();
    let mut parts = date.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let well_formed = year.len() == 4
        && month.len() == 2
        && day.len() == 2
        && [year, month, day]
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()));
    well_formed.then(|| SharedString::from(date.to_owned()))
}

impl Render for PullRequestView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_diff_editor(window, cx);
        if self.loading && self.detail.is_none() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Label::new(format!("Loading PR #{}…", self.number)))
                .into_any_element();
        }

        if let Some(host) = self.auth_error_host.clone()
            && self.detail.is_none()
        {
            let host_for_action = host.to_string();
            let display = git::git_host_credentials::GitHostKind::from_host(&host)
                .map(|kind| kind.display_name())
                .unwrap_or("the git host");
            return v_flex()
                .size_full()
                .p_4()
                .gap_2()
                .child(Label::new("Connection expired").color(Color::Error))
                .child(
                    Label::new(format!(
                        "Your {display} sign-in is no longer valid. \
                         Reconnect to reload this pull request."
                    ))
                    .color(Color::Muted)
                    .size(LabelSize::Small),
                )
                .child(
                    Button::new("pull-request-view-reconnect", "Reconnect").on_click(cx.listener(
                        move |_, _, window, cx| {
                            window.dispatch_action(
                                Box::new(zed_actions::ConnectGitHost {
                                    host: host_for_action.clone(),
                                }),
                                cx,
                            );
                        },
                    )),
                )
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
                    Label::new(format!("Failed to load PR #{}", self.number)).color(Color::Error),
                )
                .child(Label::new(error).color(Color::Muted).size(LabelSize::Small))
                .into_any_element();
        }

        let Some(detail) = self.detail.as_ref() else {
            return Empty.into_any_element();
        };

        let in_flight = self.in_flight_action;
        let description = self
            .description_md
            .is_some()
            .then(|| self.render_body(window, cx));
        let header = self.render_header(detail, in_flight, description, cx);
        let editor_bg = cx.theme().colors().editor_background;
        let diff_editor = self.diff_editor.clone();
        let loading = self.loading;

        v_flex()
            .size_full()
            .bg(editor_bg)
            .child(header)
            .child(Divider::horizontal())
            // Inline comments render anchored inside the diff editor itself; the
            // description now lives in the header strip above the action buttons.
            .map(|this| match diff_editor {
                Some(editor) => this.child(div().flex_1().min_h_0().w_full().child(editor)),
                None => this.child(
                    v_flex().flex_1().items_center().justify_center().child(
                        Label::new(if loading {
                            "Loading changes…"
                        } else {
                            "No file changes to display."
                        })
                        .color(Color::Muted),
                    ),
                ),
            })
            .into_any_element()
    }
}

impl PullRequestView {
    fn render_header(
        &self,
        detail: &PullRequestDetail,
        in_flight: bool,
        description: Option<AnyElement>,
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
        // Enable merge unless the host reports a real conflict. Bitbucket never
        // reports mergeability and GitHub computes it lazily, so treating
        // "unknown" as mergeable keeps the buttons usable for anyone with merge
        // rights; the host still enforces permission and surfaces a rejection on
        // click for anyone who lacks them.
        let merge_enabled = actions_enabled && detail.is_mergeable != Some(false);
        let viewer_approved = detail.viewer_review == Some(PullRequestReviewVerdict::Approve);
        let viewer_requested_changes =
            detail.viewer_review == Some(PullRequestReviewVerdict::RequestChanges);

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
                    .flex_wrap()
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
                    .when_some(iso_calendar_date(&detail.created_at), |this, date| {
                        this.child(
                            Label::new(format!("Created {date}"))
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                    })
                    .when_some(iso_calendar_date(&detail.updated_at), |this, date| {
                        this.child(
                            Label::new(format!("Updated {date}"))
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                    })
                    .child({
                        // Bitbucket's PR detail omits these counts; fall back to
                        // the numbers parsed from the unified diff.
                        let (files, additions, deletions) = if detail.changed_files == 0
                            && detail.additions == 0
                            && detail.deletions == 0
                        {
                            self.computed_diff_stats()
                        } else {
                            (
                                detail.changed_files as usize,
                                detail.additions,
                                detail.deletions,
                            )
                        };
                        Label::new(format!("{files} file(s), +{additions} -{deletions}"))
                            .color(Color::Muted)
                            .size(LabelSize::Small)
                    })
                    .when_some(detail.commits, |this, commits| {
                        this.child(
                            Label::new(format!(
                                "{commits} commit{}",
                                if commits == 1 { "" } else { "s" }
                            ))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                        )
                    }),
            )
            .when_some(description, |this, description| {
                // Collapsible + drag-resizable description. Collapsing hides the
                // body but leaves the action buttons below in place; the buttons
                // live in a sibling row so a long body never pushes them off.
                let expanded = self.description_expanded;
                let height = self.description_height;
                this.child(
                    v_flex()
                        .flex_none()
                        .w_full()
                        .pt_2()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Disclosure::new("pr-view-description-disclosure", expanded)
                                        .on_toggle_expanded(Arc::new(cx.listener(
                                            |this, _, _window, cx| {
                                                this.description_expanded =
                                                    !this.description_expanded;
                                                cx.notify();
                                            },
                                        ))
                                            as Arc<dyn Fn(&ClickEvent, &mut Window, &mut App)>),
                                )
                                .child(
                                    Label::new("Description")
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                ),
                        )
                        .when(expanded, |this| {
                            let body = v_flex()
                                .id("pr-view-description")
                                .flex_none()
                                .w_full()
                                .overflow_y_scroll()
                                .child(description)
                                .on_drag_move(cx.listener(
                                    |this,
                                     event: &DragMoveEvent<DescriptionResizeDrag>,
                                     _window,
                                     cx| {
                                        // Body top is fixed; the mouse Y sets the
                                        // new bottom, so height = mouse.y - top.
                                        let raw = event.event.position.y - event.bounds.top();
                                        let min = px(48.);
                                        let max = px(720.);
                                        this.description_height = Some(if raw < min {
                                            min
                                        } else if raw > max {
                                            max
                                        } else {
                                            raw
                                        });
                                        cx.notify();
                                    },
                                ));
                            let body = match height {
                                Some(height) => body.h(height),
                                None => body.max_h(rems(13.)),
                            };
                            this.child(body).child(
                                div()
                                    .id("pr-view-description-resize")
                                    .h(px(6.))
                                    .w_full()
                                    .cursor_row_resize()
                                    .on_drag(DescriptionResizeDrag, |_, _, _, cx| {
                                        cx.new(|_| Empty)
                                    }),
                            )
                        }),
                )
            })
            .when(!detail.reviewers.is_empty(), |this| {
                this.child(
                    h_flex()
                        .gap_3()
                        .pt_2()
                        .flex_wrap()
                        .children(detail.reviewers.iter().map(render_reviewer_chip)),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new(
                            "pr-approve",
                            if viewer_approved {
                                "Approved"
                            } else {
                                "Approve"
                            },
                        )
                        .style(ButtonStyle::Tinted(TintColor::Success))
                        .when(viewer_approved, |button| {
                            button.start_icon(Icon::new(IconName::Check))
                        })
                        .disabled(!actions_enabled)
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                if viewer_approved {
                                    this.retract_review(PullRequestReviewVerdict::Approve, cx);
                                } else {
                                    this.submit_review(PullRequestReviewVerdict::Approve, cx);
                                }
                            },
                        )),
                    )
                    .child(
                        Button::new(
                            "pr-request-changes",
                            if viewer_requested_changes {
                                "Changes requested"
                            } else {
                                "Request changes"
                            },
                        )
                        .style(ButtonStyle::Tinted(TintColor::Error))
                        .when(viewer_requested_changes, |button| {
                            button.start_icon(Icon::new(IconName::Close))
                        })
                        .disabled(!actions_enabled)
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                if viewer_requested_changes {
                                    this.retract_review(
                                        PullRequestReviewVerdict::RequestChanges,
                                        cx,
                                    );
                                } else {
                                    this.submit_review(
                                        PullRequestReviewVerdict::RequestChanges,
                                        cx,
                                    );
                                }
                            },
                        )),
                    )
                    .child(
                        Button::new("pr-merge", "Merge")
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .disabled(!merge_enabled)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.merge(PullRequestMergeMethod::Merge, cx);
                            })),
                    )
                    .child(
                        Button::new("pr-squash", "Squash & merge")
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .disabled(!merge_enabled)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.merge(PullRequestMergeMethod::Squash, cx);
                            })),
                    )
                    .when(in_flight, |this| {
                        this.child(Label::new("…").color(Color::Muted).size(LabelSize::Small))
                    }),
            )
            .into_any_element()
    }

    fn render_body(&self, window: &Window, cx: &App) -> AnyElement {
        match &self.description_md {
            Some(markdown) => MarkdownElement::new(markdown.clone(), pr_markdown_style(window, cx))
                .into_any_element(),
            None => Empty.into_any_element(),
        }
    }

    /// Lazily builds the diff editor over the multibuffer prepared in `refresh`,
    /// inserting the resolved inline comments as block decorations. Done here,
    /// rather than in `refresh`, because `Editor::for_multibuffer` needs the
    /// `Window`, which `render` has but the windowless `refresh` task does not.
    fn ensure_diff_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.diff_editor.is_some() {
            return;
        }
        let Some(multibuffer) = self.diff_multibuffer.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        let language_registry = project.read(cx).languages().clone();

        // Build a Markdown entity for every comment up front, with the view's own
        // context, so the entities can be kept alive and observed for async parse
        // completion (see `_comment_md_subscriptions`).
        let weak_view = cx.weak_entity();
        let mut markdowns: Vec<Entity<Markdown>> = Vec::new();
        let rendered_threads: Vec<(Anchor, u64, bool, Vec<(ThreadComment, Entity<Markdown>)>)> =
            self.comment_threads
                .clone()
                .into_iter()
                .map(|thread| {
                    let comments = thread
                        .comments
                        .into_iter()
                        .map(|comment| {
                            let markdown = cx.new(|cx| {
                                Markdown::new(
                                    comment.body.clone(),
                                    Some(language_registry.clone()),
                                    None,
                                    cx,
                                )
                            });
                            markdowns.push(markdown.clone());
                            (comment, markdown)
                        })
                        .collect::<Vec<_>>();
                    (thread.anchor, thread.root_id, thread.resolved, comments)
                })
                .collect();

        let editor = cx.new(|cx| {
            let mut editor = Editor::for_multibuffer(multibuffer, Some(project), window, cx);
            editor.disable_inline_diagnostics();
            editor.set_show_bookmarks(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_expand_all_diff_hunks(cx);
            editor.set_show_pr_comment_gutter_button(true, cx);
            if !rendered_threads.is_empty() {
                let blocks = rendered_threads
                    .into_iter()
                    .map(|(anchor, root_id, resolved, comments)| {
                        let weak_view = weak_view.clone();
                        BlockProperties {
                            placement: BlockPlacement::Below(anchor),
                            // A concrete height (not `None`) is required: the
                            // editor only re-measures and grows blocks whose
                            // height is `Some`. A `None`-height block stays pinned
                            // at one row, so the comment markdown overflows and
                            // overlaps the rows below it.
                            height: Some(1),
                            style: BlockStyle::Flex,
                            render: Arc::new(move |block_cx| {
                                render_comment_thread(
                                    &comments, root_id, resolved, &weak_view, block_cx,
                                )
                            }),
                            priority: 0,
                        }
                    })
                    .collect::<Vec<_>>();
                editor.insert_blocks(blocks, None, cx);
            }
            editor
        });

        // Markdown parses asynchronously; when a parse finishes, re-lay-out the
        // diff editor so its comment blocks grow to fit. Notify the *editor*, not
        // the whole view: notifying the view would re-render its workspace tab on
        // every one of these (one per comment), which reads as the tab flickering.
        // The editor re-render is contained to the diff content.
        let editor_handle = editor.downgrade();
        self._comment_md_subscriptions = markdowns
            .iter()
            .map(|markdown| {
                let editor_handle = editor_handle.clone();
                cx.observe(markdown, move |_this, _markdown, cx| {
                    editor_handle.update(cx, |_editor, cx| cx.notify()).ok();
                })
            })
            .collect();
        self._comment_markdowns = markdowns;
        self._diff_editor_subscription =
            Some(cx.subscribe_in(&editor, window, Self::handle_diff_editor_event));
        self.diff_editor = Some(editor);
    }

    /// Bitbucket's PR detail endpoint omits additions/deletions/file counts, so
    /// fall back to counting them from the parsed unified diff for the header.
    fn computed_diff_stats(&self) -> (usize, u32, u32) {
        let mut additions = 0u32;
        let mut deletions = 0u32;
        for file in &self.diff_files {
            for hunk in &file.hunks {
                for line in &hunk.lines {
                    match line {
                        ParsedDiffLine::Addition(_) => additions += 1,
                        ParsedDiffLine::Deletion(_) => deletions += 1,
                        ParsedDiffLine::Context(_) => {}
                    }
                }
            }
        }
        (self.diff_files.len(), additions, deletions)
    }
}

#[cfg(test)]
mod tests {
    use super::iso_calendar_date;

    #[test]
    fn iso_calendar_date_extracts_date_portion() {
        // Bitbucket-style timestamp with fractional seconds and offset.
        assert_eq!(
            iso_calendar_date("2026-05-19T12:34:56.789000+00:00").as_deref(),
            Some("2026-05-19")
        );
        // GitHub-style timestamp with a `Z` suffix.
        assert_eq!(
            iso_calendar_date("2026-01-04T05:06:07Z").as_deref(),
            Some("2026-01-04")
        );
        // A bare date is accepted as-is.
        assert_eq!(
            iso_calendar_date("2026-05-19").as_deref(),
            Some("2026-05-19")
        );
    }

    #[test]
    fn iso_calendar_date_rejects_malformed_input() {
        assert_eq!(iso_calendar_date(""), None);
        assert_eq!(iso_calendar_date("not-a-date"), None);
        assert_eq!(iso_calendar_date("2026-5-9T00:00:00Z"), None);
        assert_eq!(iso_calendar_date("20260519"), None);
    }
}
