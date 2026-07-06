//! Lathe-owned extensions to the git panel.
//!
//! This is a child module of [`super`] (`git_panel`), declared there via
//! `#[path = "git_panel_lathe.rs"] mod lathe;`. Being a child module, it can
//! reach `GitPanel`'s private fields and methods, so Lathe feature code can move
//! out of the upstream-owned `git_panel.rs` file without loosening any
//! visibility or changing behavior. The upstream file keeps only the narrow call
//! sites (`lathe::...` for free items) and the `impl super::GitPanel` methods
//! below, which upstream code invokes as ordinary methods on `GitPanel`.
//!
//! See `EDOC/lathe-extraction-plan.md` (WP1) for the migration of the remaining
//! git-panel customizations (history tab, repos strip, inline hunks) into this
//! module.

use super::*;

/// One rendered row in the Explorer's flat row list. Headers are interleaved
/// with their section's entries, indexed back into `explorer_entries`.
enum ExplorerRow {
    Header {
        section: ExplorerSection,
        count: usize,
        collapsed: bool,
    },
    Folder {
        section: ExplorerSection,
        path: SharedString,
        name: SharedString,
        depth: usize,
        collapsed: bool,
        count: usize,
    },
    Entry {
        entry_ix: usize,
        depth: usize,
    },
}

/// One of the four section headers in the Explorer tab. Section order is
/// fixed at Local → Remote → Worktrees → Stashes; this enum identifies which
/// row was clicked so callers can pick the right action (checkout / activate
/// worktree / apply stash).
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub(crate) enum ExplorerSection {
    Local,
    Remote,
    Worktrees,
    Stashes,
}

impl ExplorerSection {
    fn label(self) -> &'static str {
        match self {
            Self::Local => "LOCAL",
            Self::Remote => "REMOTE",
            Self::Worktrees => "WORKTREES",
            Self::Stashes => "STASHES",
        }
    }
}

/// Sourced rows for the Explorer tab. Each row is whatever the section list
/// renders one of: a branch entry (local or remote), a linked-worktree entry,
/// or a stash entry. Held in a single `Vec` keyed by index so keyboard
/// navigation and selection can stay flat.
#[derive(Debug, Clone)]
pub(crate) enum ExplorerEntry {
    LocalBranch(Branch),
    RemoteBranch(Branch),
    Worktree(::git::repository::Worktree),
    Stash(::git::stash::StashEntry),
}

/// Payload for the explorer-row drag-and-drop. Carries the source branch name
/// from the row being dragged. Dropping it onto another branch row triggers a
/// rebase of source onto target.
#[derive(Clone)]
pub(crate) struct DraggedExplorerBranch {
    pub name: SharedString,
}

pub(crate) struct DraggedBranchView {
    pub name: SharedString,
}

impl Render for DraggedBranchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .bg(cx.theme().colors().background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .px_2()
            .py_0p5()
            .gap_1()
            .child(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new(self.name.clone()).size(LabelSize::Small))
    }
}

impl ExplorerEntry {
    fn section(&self) -> ExplorerSection {
        match self {
            Self::LocalBranch(_) => ExplorerSection::Local,
            Self::RemoteBranch(_) => ExplorerSection::Remote,
            Self::Worktree(_) => ExplorerSection::Worktrees,
            Self::Stash(_) => ExplorerSection::Stashes,
        }
    }

    /// User-visible label used both for rendering and for filter matching.
    fn label(&self) -> SharedString {
        match self {
            Self::LocalBranch(branch) | Self::RemoteBranch(branch) => {
                SharedString::from(branch.name().to_string())
            }
            Self::Worktree(worktree) => worktree
                .ref_name
                .clone()
                .unwrap_or_else(|| SharedString::from(worktree.sha.to_string())),
            Self::Stash(stash) => SharedString::from(stash.message.clone()),
        }
    }

    /// Commit the row points at. Used by the auto-scroll-to-commit
    /// integration with the graph view. `None` for entries that don't have a
    /// single resolvable commit (e.g. a worktree whose head couldn't be
    /// parsed as an oid).
    fn target_commit(&self) -> Option<::git::Oid> {
        match self {
            Self::LocalBranch(branch) | Self::RemoteBranch(branch) => branch
                .most_recent_commit
                .as_ref()
                .and_then(|commit| ::std::str::FromStr::from_str(commit.sha.as_ref()).ok()),
            Self::Worktree(worktree) => ::std::str::FromStr::from_str(worktree.sha.as_ref()).ok(),
            Self::Stash(stash) => Some(stash.oid),
        }
    }
}

#[derive(Default)]
struct ExplorerFolderNode {
    name: SharedString,
    full_path: SharedString,
    children: BTreeMap<SharedString, ExplorerFolderNode>,
    entry_ix: Option<usize>,
}

impl ExplorerFolderNode {
    fn leaf_count(&self) -> usize {
        let mut total = if self.entry_ix.is_some() { 1 } else { 0 };
        for child in self.children.values() {
            total += child.leaf_count();
        }
        total
    }
}

fn build_explorer_folder_tree(
    explorer_entries: &[ExplorerEntry],
    indices: &[usize],
) -> ExplorerFolderNode {
    let mut root = ExplorerFolderNode::default();
    for &ix in indices {
        let Some(entry) = explorer_entries.get(ix) else {
            continue;
        };
        let label = entry.label();
        let parts: Vec<&str> = label.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        let mut node = &mut root;
        let mut full_path = String::new();
        let last = parts.len() - 1;
        for (i, part) in parts.iter().enumerate() {
            if !full_path.is_empty() {
                full_path.push('/');
            }
            full_path.push_str(part);
            let segment = SharedString::from(part.to_string());
            let path = SharedString::from(full_path.clone());
            node = node
                .children
                .entry(segment.clone())
                .or_insert_with(|| ExplorerFolderNode {
                    name: segment,
                    full_path: path,
                    children: BTreeMap::new(),
                    entry_ix: None,
                });
            if i == last {
                node.entry_ix = Some(ix);
            }
        }
    }
    root
}

fn flatten_folder_tree(
    node: &ExplorerFolderNode,
    section: ExplorerSection,
    depth: usize,
    rows: &mut Vec<ExplorerRow>,
    collapsed_folders: &HashSet<(ExplorerSection, SharedString)>,
) {
    // GitKraken-style ordering: folders (alphabetical) first at each level,
    // then leaf entries (alphabetical) at the same level.
    let mut child_folders = Vec::new();
    let mut child_leaves = Vec::new();
    for child in node.children.values() {
        if child.children.is_empty() && child.entry_ix.is_some() {
            child_leaves.push(child);
        } else {
            child_folders.push(child);
        }
    }
    for folder in child_folders {
        let key = (section, folder.full_path.clone());
        let is_collapsed = collapsed_folders.contains(&key);
        rows.push(ExplorerRow::Folder {
            section,
            path: folder.full_path.clone(),
            name: folder.name.clone(),
            depth,
            collapsed: is_collapsed,
            count: folder.leaf_count(),
        });
        if !is_collapsed {
            flatten_folder_tree(folder, section, depth + 1, rows, collapsed_folders);
        }
        // A "folder" that also has its own entry (e.g. a branch named exactly
        // the same as a parent of another branch) gets a leaf row right after
        // its folder subtree at the same depth.
        if let Some(ix) = folder.entry_ix {
            rows.push(ExplorerRow::Entry { entry_ix: ix, depth });
        }
    }
    for leaf in child_leaves {
        if let Some(ix) = leaf.entry_ix {
            rows.push(ExplorerRow::Entry { entry_ix: ix, depth });
        }
    }
}

/// Open the raw output of a git command in a read-only editor in the center
/// group. Used by the error toast's "View Log" action and the push-result
/// toast. ANSI control codes are stripped via [`GitOutputHandler`] so the log
/// reads as plain text.
pub(super) fn open_output(
    operation: impl Into<SharedString>,
    workspace: &mut Workspace,
    output: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let operation = operation.into();

    let mut handler = GitOutputHandler::default();
    let mut processor = ansi::Processor::<ansi::StdSyncHandler>::default();
    processor.advance(&mut handler, output.as_bytes());
    let plain_text = handler.output;

    let buffer = cx.new(|cx| Buffer::local(plain_text.as_str(), cx));
    buffer.update(cx, |buffer, cx| {
        buffer.set_capability(language::Capability::ReadOnly, cx);
    });
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_buffer(buffer, None, window, cx);
        editor.buffer().update(cx, |buffer, cx| {
            buffer.set_title(format!("Output from git {operation}"), cx);
        });
        editor.set_read_only(true);
        editor
    });

    workspace.add_item_to_center(Box::new(editor), window, cx);
}

/// ANSI handler that accumulates a git command's output as plain text, honoring
/// carriage returns (so progress lines that redraw in place collapse to their
/// final state) and tabs.
#[derive(Default)]
struct GitOutputHandler {
    output: String,
    line_start: usize,
}

impl ansi::Handler for GitOutputHandler {
    fn input(&mut self, c: char) {
        self.output.push(c);
    }

    fn linefeed(&mut self) {
        self.output.push('\n');
        self.line_start = self.output.len();
    }

    fn carriage_return(&mut self) {
        self.output.truncate(self.line_start);
    }

    fn put_tab(&mut self, count: u16) {
        self.output
            .extend(std::iter::repeat_n('\t', count as usize));
    }
}

#[derive(Clone, Copy)]
pub(super) enum StashOp {
    Pop,
    Apply,
}

impl StashOp {
    fn label(self) -> &'static str {
        match self {
            StashOp::Pop => "stash pop",
            StashOp::Apply => "stash apply",
        }
    }
}

/// Run a stash pop/apply against `repo`, surfacing any failure as an error toast.
pub(super) fn run_stash_op(
    cx: &mut App,
    workspace: WeakEntity<Workspace>,
    repo: Entity<Repository>,
    op: StashOp,
    index: usize,
) {
    let label = op.label();
    cx.spawn(async move |cx| {
        let task = repo.update(cx, |repo, cx| match op {
            StashOp::Pop => repo.stash_pop(Some(index), cx),
            StashOp::Apply => repo.stash_apply(Some(index), cx),
        });
        if let Err(err) = task.await {
            let Some(workspace) = workspace.upgrade() else {
                log::error!("git {label} failed: {err:?}");
                return;
            };
            cx.update(|cx| show_error_toast(workspace, label, err, cx));
        }
    })
    .detach();
}

/// Await the result of a branch operation kicked off elsewhere, refresh the
/// explorer on success, and surface any failure as an error toast.
pub(super) fn run_branch_op(
    cx: &mut App,
    workspace: WeakEntity<Workspace>,
    panel: WeakEntity<GitPanel>,
    receiver: oneshot::Receiver<anyhow::Result<()>>,
    action: impl Into<SharedString>,
) {
    let action = action.into();
    cx.spawn(async move |cx| {
        let result = receiver.await;
        let err = match result {
            Ok(Ok(())) => {
                panel
                    .update(cx, |panel, cx| panel.refresh_explorer_data(cx))
                    .ok();
                return;
            }
            Ok(Err(e)) => e,
            Err(_) => anyhow::anyhow!("operation cancelled"),
        };
        let Ok(workspace) = workspace.upgrade().ok_or(()) else {
            log::error!("git {action} failed: {err:?}");
            return;
        };
        let _ = cx.update(|cx| show_error_toast(workspace, action, err, cx));
    })
    .detach();
}

impl super::GitPanel {
    /// Kick off async loads of the things the Explorer tab needs to render
    /// (branches via the git CLI; worktrees and stashes are already cached on
    /// the repository). Results land in `explorer_entries` on the foreground
    /// thread.
    pub(super) fn refresh_explorer_data(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            self.explorer_entries.clear();
            return;
        };
        self.populate_cached_explorer_entries(cx);
        let branches_rx = repo.update(cx, |repo, _| repo.branches());
        self.explorer_load_task = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(branches)) = branches_rx.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.merge_branches_into_explorer(branches.branches);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Toggle the collapsed state for one folder path within a section.
    fn toggle_explorer_folder(
        &mut self,
        section: ExplorerSection,
        path: SharedString,
        cx: &mut Context<Self>,
    ) {
        let key = (section, path);
        if self.explorer_collapsed_folders.contains(&key) {
            self.explorer_collapsed_folders.remove(&key);
        } else {
            self.explorer_collapsed_folders.insert(key);
        }
        cx.notify();
    }

    /// Populate `explorer_entries` from data already on the cached repository
    /// snapshot (linked worktrees, stash entries) so the tab renders
    /// immediately while the async branch fetch is in flight.
    fn populate_cached_explorer_entries(&mut self, cx: &App) {
        let mut entries: Vec<ExplorerEntry> = Vec::new();
        if let Some(repo) = self.active_repository.as_ref() {
            let repo_read = repo.read(cx);
            for worktree in repo_read.linked_worktrees().iter() {
                entries.push(ExplorerEntry::Worktree(worktree.clone()));
            }
            for stash in repo_read.stash_entries.entries.iter() {
                entries.push(ExplorerEntry::Stash(stash.clone()));
            }
        }
        self.explorer_entries = entries;
    }

    /// Merge a freshly-fetched `Vec<Branch>` into `explorer_entries`,
    /// splitting on `refs/heads/` vs `refs/remotes/`. Replaces any previous
    /// branch entries while leaving worktrees/stashes alone.
    fn merge_branches_into_explorer(&mut self, branches: Vec<Branch>) {
        self.explorer_entries.retain(|entry| {
            !matches!(
                entry,
                ExplorerEntry::LocalBranch(_) | ExplorerEntry::RemoteBranch(_)
            )
        });
        let (locals, remotes): (Vec<_>, Vec<_>) = branches.into_iter().partition(|branch| {
            branch
                .ref_name
                .as_ref()
                .starts_with("refs/heads/")
        });
        // Section order matches what the UI shows top-to-bottom.
        let locals = locals.into_iter().map(ExplorerEntry::LocalBranch);
        let remotes = remotes.into_iter().map(ExplorerEntry::RemoteBranch);
        // Prepend so the relative ordering in the panel is Local, Remote,
        // Worktrees, Stashes (cached worktrees/stashes were appended first
        // by `populate_cached_explorer_entries`).
        let mut combined: Vec<ExplorerEntry> = locals.collect();
        combined.extend(remotes);
        combined.extend(std::mem::take(&mut self.explorer_entries));
        self.explorer_entries = combined;
    }

    fn explorer_filter_text(&self, cx: &App) -> String {
        self.explorer_filter.read(cx).text(cx).to_lowercase()
    }

    fn explorer_visible_entries(&self, cx: &App) -> Vec<(ExplorerSection, Vec<usize>)> {
        let filter = self.explorer_filter_text(cx);
        let needle = filter.trim();
        let sections = [
            ExplorerSection::Local,
            ExplorerSection::Remote,
            ExplorerSection::Worktrees,
            ExplorerSection::Stashes,
        ];
        sections
            .into_iter()
            .map(|section| {
                let indices = self
                    .explorer_entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| entry.section() == section)
                    .filter(|(_, entry)| {
                        needle.is_empty()
                            || entry
                                .label()
                                .to_lowercase()
                                .contains(needle)
                    })
                    .map(|(ix, _)| ix)
                    .collect();
                (section, indices)
            })
            .collect()
    }

    pub(super) fn render_explorer_tab(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sections = self.explorer_visible_entries(cx);
        let collapsed = self.explorer_collapsed_sections.clone();
        let collapsed_folders = self.explorer_collapsed_folders.clone();
        let filter_active = !self.explorer_filter_text(cx).trim().is_empty();

        // Build a flat list of rows: alternating section-header rows and
        // entry rows. We track each row's kind in a parallel vector so the
        // uniform_list closure can dispatch.
        let mut rows: Vec<ExplorerRow> = Vec::new();
        for (section, indices) in &sections {
            let is_collapsed = collapsed.contains(section);
            rows.push(ExplorerRow::Header {
                section: *section,
                count: indices.len(),
                collapsed: is_collapsed,
            });
            if is_collapsed {
                continue;
            }
            let tree_eligible = matches!(
                section,
                ExplorerSection::Local | ExplorerSection::Remote
            ) && !filter_active;
            if tree_eligible {
                let tree = build_explorer_folder_tree(&self.explorer_entries, indices);
                flatten_folder_tree(&tree, *section, 0, &mut rows, &collapsed_folders);
            } else {
                for ix in indices {
                    rows.push(ExplorerRow::Entry { entry_ix: *ix, depth: 0 });
                }
            }
        }

        let total_count = self.explorer_entries.len();
        let viewing_label = if total_count == 0 {
            "Loading…".to_string()
        } else {
            format!("Viewing {}", total_count)
        };

        let entries = std::sync::Arc::new(rows);
        let entries_for_list = entries.clone();
        let explorer_entries = self.explorer_entries.clone();

        v_flex()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .justify_between()
                    .child(
                        Label::new(viewing_label)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex().px_3().pb_1().child(
                    div()
                        .w_full()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .rounded_md()
                        .px_2()
                        .py_1()
                        .child(self.explorer_filter.clone()),
                ),
            )
            .child(
                uniform_list(
                    "git-explorer-list",
                    entries.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        let mut elements = Vec::with_capacity(range.end - range.start);
                        for ix in range {
                            let row = &entries_for_list[ix];
                            elements.push(this.render_explorer_row(ix, row, &explorer_entries, cx));
                        }
                        elements
                    }),
                )
                .track_scroll(&self.explorer_scroll_handle)
                .flex_grow(1.0)
                .size_full(),
            )
    }

    fn render_explorer_row(
        &self,
        row_ix: usize,
        row: &ExplorerRow,
        explorer_entries: &[ExplorerEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match row {
            ExplorerRow::Header {
                section,
                count,
                collapsed,
            } => {
                let section = *section;
                let collapsed = *collapsed;
                h_flex()
                    .id(("git-explorer-header", row_ix))
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(section.label())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_grow(1.0))
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.explorer_collapsed_sections.contains(&section) {
                            this.explorer_collapsed_sections.remove(&section);
                        } else {
                            this.explorer_collapsed_sections.insert(section);
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            ExplorerRow::Folder {
                section,
                path,
                name,
                depth,
                collapsed,
                count,
            } => {
                let section = *section;
                let path = path.clone();
                let collapsed = *collapsed;
                let depth = *depth;
                h_flex()
                    .id(("git-explorer-folder", row_ix))
                    .w_full()
                    .pl(px(20.0 + (depth as f32) * 14.0))
                    .pr_3()
                    .py_0p5()
                    .gap_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Icon::new(if collapsed {
                            IconName::Folder
                        } else {
                            IconName::FolderOpen
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(Label::new(name.clone()).size(LabelSize::Small))
                    .child(div().flex_1())
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_explorer_folder(section, path.clone(), cx);
                    }))
                    .into_any_element()
            }
            ExplorerRow::Entry { entry_ix, depth } => {
                let entry_ix = *entry_ix;
                let depth = *depth;
                let entry = match explorer_entries.get(entry_ix) {
                    Some(entry) => entry.clone(),
                    None => return div().into_any_element(),
                };
                let selected = self.explorer_selected_row == Some(row_ix);
                let full_label = entry.label();
                let label: SharedString = if depth > 0 {
                    let last = full_label
                        .as_ref()
                        .rsplit('/')
                        .next()
                        .unwrap_or(full_label.as_ref());
                    SharedString::from(last.to_string())
                } else {
                    full_label
                };
                let (icon, is_head) = match &entry {
                    ExplorerEntry::LocalBranch(b) => (IconName::GitBranch, b.is_head),
                    ExplorerEntry::RemoteBranch(_) => (IconName::GitBranch, false),
                    ExplorerEntry::Worktree(w) => (IconName::FolderOpen, w.is_main),
                    ExplorerEntry::Stash(_) => (IconName::Archive, false),
                };
                let (drag_source_name, drop_target_name) = match &entry {
                    ExplorerEntry::LocalBranch(b) => (
                        Some(SharedString::from(b.name().to_string())),
                        Some(SharedString::from(b.name().to_string())),
                    ),
                    ExplorerEntry::RemoteBranch(b) => {
                        (None, Some(SharedString::from(b.name().to_string())))
                    }
                    ExplorerEntry::Worktree(_) | ExplorerEntry::Stash(_) => (None, None),
                };
                let tracking_status = match &entry {
                    ExplorerEntry::LocalBranch(b) => b.tracking_status(),
                    _ => None,
                }
                .filter(|s| s.ahead > 0 || s.behind > 0);
                h_flex()
                    .id(("git-explorer-row", row_ix))
                    .w_full()
                    .pl(px(20.0 + (depth as f32) * 14.0))
                    .pr_3()
                    .py_0p5()
                    .gap_2()
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(cx.theme().colors().element_selected)
                    })
                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                    .child(Icon::new(icon).size(IconSize::Small).color(if is_head {
                        Color::Accent
                    } else {
                        Color::Muted
                    }))
                    .child(Label::new(label).size(LabelSize::Small).when(
                        is_head,
                        |label| label.color(Color::Accent),
                    ))
                    .child(div().flex_1())
                    .when_some(tracking_status, |this, status| {
                        this.child(render_tracking_chip(status))
                    })
                    .when_some(drag_source_name, |this, source| {
                        this.on_drag(
                            DraggedExplorerBranch { name: source },
                            |payload, _, _, cx| {
                                cx.new(|_| DraggedBranchView {
                                    name: payload.name.clone(),
                                })
                            },
                        )
                    })
                    .when_some(drop_target_name, |this, target_name| {
                        let target_for_drag = target_name.clone();
                        this.drag_over::<DraggedExplorerBranch>(
                            move |style, payload, _window, cx| {
                                if payload.name == target_for_drag {
                                    style
                                } else {
                                    style.bg(cx.theme().colors().drop_target_background)
                                }
                            },
                        )
                        .on_drop(cx.listener(
                            move |this, payload: &DraggedExplorerBranch, window, cx| {
                                if payload.name == target_name {
                                    return;
                                }
                                this.rebase_branch_onto(
                                    payload.name.to_string(),
                                    target_name.to_string(),
                                    window,
                                    cx,
                                );
                            },
                        ))
                    })
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.explorer_selected_row = Some(row_ix);
                        if event.click_count() > 1 {
                            this.checkout_explorer_entry(entry_ix, window, cx);
                        } else {
                            this.activate_explorer_entry(entry_ix, window, cx);
                        }
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.explorer_selected_row = Some(row_ix);
                            this.deploy_explorer_context_menu(
                                event.position,
                                entry_ix,
                                window,
                                cx,
                            );
                        }),
                    )
                    .into_any_element()
            }
        }
    }

    /// Handle a single click on an Explorer row. Purely navigational: it
    /// selects the row and dispatches `OpenAtCommit` so the Git Graph view
    /// opens (or activates, if already open) on the target commit. Double-
    /// click invokes `checkout_explorer_entry` instead for the destructive
    /// switch action.
    fn activate_explorer_entry(
        &mut self,
        entry_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.explorer_entries.get(entry_ix).cloned() else {
            return;
        };
        let Some(oid) = entry.target_commit() else {
            return;
        };
        // Keep the existing emit so any other subscribers (e.g. an already
        // open graph view) react instantly without re-dispatching the
        // open-graph action.
        cx.emit(Event::ScrollGraphToCommit(oid));
        // And dispatch the action that opens the Git Graph item if it's
        // not already in the workspace; the action's handler also activates
        // the existing graph and selects the commit.
        window.dispatch_action(
            Box::new(OpenAtCommit {
                sha: oid.to_string(),
            }),
            cx,
        );
    }

    /// Double-click on an Explorer row: switch to the underlying branch
    /// (local or remote). Worktree/stash double-click is a no-op for now —
    /// those still require the right-click menu.
    fn checkout_explorer_entry(
        &mut self,
        entry_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.explorer_entries.get(entry_ix).cloned() else {
            return;
        };
        let branch_name = match entry {
            ExplorerEntry::LocalBranch(b) | ExplorerEntry::RemoteBranch(b) => b.name().to_string(),
            ExplorerEntry::Worktree(_) | ExplorerEntry::Stash(_) => return,
        };
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        cx.spawn(async move |_, cx| {
            repo.update(cx, |repo, _| repo.change_branch(branch_name))
                .await??;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to change branch", window, cx, |_, _, _| None);
    }

    /// Drag-and-drop handler: rebase `source` branch onto `target`. Performs
    /// `git switch <source>` (so the source branch is checked out) and then
    /// `git rebase <target>`. Both steps run on the foreground; errors surface
    /// via the standard git-panel error toast.
    /// Drag-and-drop handler: confirm before rebasing `source` onto `target`.
    /// Instead of rewriting history immediately, this opens a modal that
    /// previews the commits to be replayed and offers a plain or interactive
    /// rebase.
    fn rebase_branch_onto(
        &mut self,
        source: String,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let repo_id = repo.read(cx).id;
        let source_is_current = repo
            .read(cx)
            .branch
            .as_ref()
            .map(|branch| branch.name() == source)
            .unwrap_or(false);
        let git_store = self.project.read(cx).git_store().clone();
        let workspace_weak = self.workspace.clone();

        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |_window, cx| {
                crate::rebase_confirm_modal::RebaseConfirmModal::new(
                    source.clone(),
                    source_is_current,
                    target.clone(),
                    target.clone(),
                    repo_id,
                    git_store,
                    repo,
                    workspace_weak,
                    cx,
                )
            });
        });
    }

    /// Push an empty source refspec (`:<remote_branch>`) to delete the
    /// branch on the upstream remote, then delete the local branch on
    /// success. Errors at either stage surface as the standard git error
    /// toast; on success the Explorer branch list is refreshed so the
    /// removed entries disappear without a manual reopen.
    fn delete_branch_remote(
        &mut self,
        branch_name: SharedString,
        remote_name: SharedString,
        remote_branch_name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_push_and_pull(cx) {
            self.show_error_toast(
                "delete remote branch",
                anyhow::anyhow!(
                    "deleting remote branches is not yet supported on remote projects"
                ),
                cx,
            );
            return;
        }
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let askpass =
            self.askpass_delegate(format!("git push {remote_name} --delete"), window, cx);
        let push_label: SharedString =
            format!("delete {branch_name} on {remote_name}").into();

        cx.spawn(async move |this, cx| {
            let push = repo.update(cx, |repo, cx| {
                repo.push(
                    SharedString::default(),
                    remote_branch_name,
                    remote_name,
                    None,
                    askpass,
                    cx,
                )
            });
            match push.await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    this.update(cx, |this, cx| this.show_error_toast(push_label, err, cx))?;
                    return anyhow::Ok(());
                }
                Err(_) => return anyhow::Ok(()),
            }

            let delete_local = repo.update(cx, |repo, _| {
                repo.delete_branch(false, branch_name.to_string(), false)
            });
            match delete_local.await {
                Ok(Ok(())) => {
                    this.update(cx, |this, cx| this.refresh_explorer_data(cx))?;
                }
                Ok(Err(err)) => {
                    this.update(cx, |this, cx| {
                        this.show_error_toast("delete local branch", err, cx)
                    })?;
                }
                Err(_) => {}
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn deploy_explorer_context_menu(
        &mut self,
        position: Point<Pixels>,
        entry_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.explorer_entries.get(entry_ix).cloned() else {
            return;
        };
        if let ExplorerEntry::Stash(stash) = &entry {
            self.deploy_stash_context_menu(position, stash.clone(), window, cx);
            return;
        }
        let (branch, is_remote) = match &entry {
            ExplorerEntry::LocalBranch(b) => (b.clone(), false),
            ExplorerEntry::RemoteBranch(b) => (b.clone(), true),
            ExplorerEntry::Worktree(_) | ExplorerEntry::Stash(_) => return,
        };
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let branch_name: SharedString = branch.name().to_string().into();
        let is_head = branch.is_head;
        let current_branch_name = repo
            .read(cx)
            .branch
            .as_ref()
            .map(|b| b.name().to_string());
        let workspace = self.workspace.clone();
        let panel = cx.entity().downgrade();
        // Local branches that actually have a remote-tracking upstream get the
        // "delete on origin too" entry. We skip it when the tracking ref is
        // `Gone` because there is no remote ref left to push a delete to.
        let upstream_for_remote_delete: Option<(SharedString, SharedString)> = if is_remote {
            None
        } else {
            branch.upstream.as_ref().and_then(|u| {
                if !matches!(u.tracking, UpstreamTracking::Tracked(_)) {
                    return None;
                }
                let remote = u.remote_name()?;
                let remote_branch = u.branch_name()?;
                Some((remote.to_string().into(), remote_branch.to_string().into()))
            })
        };

        let context_menu = ContextMenu::build(window, cx, |menu, _window, _cx| {
            let mut menu = menu
                .context(self.focus_handle.clone())
                .header(branch_name.clone());

            if !is_head {
                let name = branch_name.clone();
                let repo = repo.clone();
                let workspace = workspace.clone();
                let panel = panel.clone();
                menu = menu.entry("Checkout", None, move |_, cx| {
                    let receiver =
                        repo.update(cx, |repo, _| repo.change_branch(name.to_string()));
                    run_branch_op(cx, workspace.clone(), panel.clone(), receiver, "checkout");
                });
            }

            if let Some(commit) = branch.most_recent_commit.clone() {
                let sha: SharedString = commit.sha.to_string().into();
                let short_sha: SharedString =
                    sha.chars().take(7).collect::<String>().into();
                let repo = repo.clone();
                let workspace = workspace.clone();
                menu = menu.entry("Branch from here…", None, move |window, cx| {
                    let sha = sha.clone();
                    let short_sha = short_sha.clone();
                    let repo = repo.clone();
                    let workspace_weak = workspace.clone();
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, |window, cx| {
                                BranchFromCommitModal::new(
                                    sha,
                                    short_sha,
                                    repo,
                                    workspace_weak,
                                    window,
                                    cx,
                                )
                            });
                        })
                        .ok();
                });
            }

            menu = menu.separator();

            {
                let name = branch_name.clone();
                menu = menu.entry("Copy branch name", None, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(name.to_string()));
                });
            }

            let can_merge_or_rebase = !is_head && current_branch_name.is_some();
            if can_merge_or_rebase {
                let current = current_branch_name.clone().unwrap_or_default();

                let name = branch_name.clone();
                let repo_merge = repo.clone();
                let current_label = current.clone();
                let workspace_m = workspace.clone();
                let panel_m = panel.clone();
                menu = menu.entry(
                    format!("Merge into {current_label}"),
                    None,
                    move |_, cx| {
                        let receiver = repo_merge.update(cx, |repo, _| {
                            repo.merge(name.to_string(), MergeOptions::default())
                        });
                        run_branch_op(cx, workspace_m.clone(), panel_m.clone(), receiver, "merge");
                    },
                );

                let name = branch_name.clone();
                let repo_rebase = repo.clone();
                let workspace_r = workspace.clone();
                let panel_r = panel.clone();
                menu = menu.entry(
                    format!("Rebase {current} onto this"),
                    None,
                    move |_, cx| {
                        let receiver = repo_rebase.update(cx, |repo, _| {
                            repo.rebase(name.to_string(), RebaseOptions::default())
                        });
                        run_branch_op(cx, workspace_r.clone(), panel_r.clone(), receiver, "rebase");
                    },
                );
            }

            if !is_head {
                menu = menu.separator();
                let local_label = if upstream_for_remote_delete.is_some() {
                    "Delete locally"
                } else {
                    "Delete"
                };
                let name = branch_name.clone();
                let repo_del = repo.clone();
                let workspace_d = workspace.clone();
                let panel_d = panel.clone();
                menu = menu.entry(local_label, None, move |_, cx| {
                    let receiver = repo_del.update(cx, |repo, _| {
                        repo.delete_branch(is_remote, name.to_string(), false)
                    });
                    run_branch_op(cx, workspace_d.clone(), panel_d.clone(), receiver, "delete branch");
                });

                if let Some((remote_name, remote_branch_name)) = upstream_for_remote_delete {
                    let name = branch_name.clone();
                    let panel_dr = panel.clone();
                    menu = menu.entry(
                        format!("Delete on {remote_name} and locally"),
                        None,
                        move |window, cx| {
                            let name = name.clone();
                            let remote_name = remote_name.clone();
                            let remote_branch_name = remote_branch_name.clone();
                            panel_dr
                                .update(cx, |panel, cx| {
                                    panel.delete_branch_remote(
                                        name,
                                        remote_name,
                                        remote_branch_name,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        },
                    );
                }
            }

            menu
        });
        self.set_context_menu(context_menu, position, window, cx);
    }

    fn deploy_stash_context_menu(
        &mut self,
        position: Point<Pixels>,
        stash: ::git::stash::StashEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let workspace = self.workspace.clone();
        let panel = cx.entity().downgrade();
        let header: SharedString = stash.message.clone().into();
        let index = stash.index;

        let context_menu = ContextMenu::build(window, cx, |menu, _window, _cx| {
            let mut menu = menu.context(self.focus_handle.clone()).header(header.clone());

            {
                let repo = repo.clone();
                let workspace = workspace.clone();
                menu = menu.entry("Apply Stash", None, move |_, cx| {
                    run_stash_op(cx, workspace.clone(), repo.clone(), StashOp::Apply, index);
                });
            }

            {
                let repo = repo.clone();
                let workspace = workspace.clone();
                menu = menu.entry("Pop Stash", None, move |_, cx| {
                    run_stash_op(cx, workspace.clone(), repo.clone(), StashOp::Pop, index);
                });
            }

            menu = menu.separator();

            {
                let message = stash.message.clone();
                menu = menu.entry("Copy stash message", None, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(message.clone()));
                });
            }

            menu = menu.separator();

            {
                let repo = repo.clone();
                let workspace = workspace.clone();
                let panel = panel.clone();
                menu = menu.entry("Delete Stash", None, move |_, cx| {
                    let receiver = repo.update(cx, |repo, cx| repo.stash_drop(Some(index), cx));
                    run_branch_op(cx, workspace.clone(), panel.clone(), receiver, "stash drop");
                });
            }

            menu
        });
        self.set_context_menu(context_menu, position, window, cx);
    }
}

fn render_tracking_chip(status: UpstreamTrackingStatus) -> impl IntoElement {
    h_flex()
        .gap_0p5()
        .when(status.behind > 0, |this| {
            this.child(
                Icon::new(IconName::ArrowDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(status.behind.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        })
        .when(status.ahead > 0, |this| {
            this.child(
                Icon::new(IconName::ArrowUp)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(status.ahead.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        })
}

fn render_pinned_strip_row(ix: usize, path: String, cx: &Context<GitPanel>) -> AnyElement {
    let label: SharedString = std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string().into())
        .unwrap_or_else(|| path.clone().into());
    let path_buf = std::path::PathBuf::from(&path);
    h_flex()
        .id(("git-panel-pinned-row", ix))
        .h(rems(1.5))
        .px_2()
        .gap_1p5()
        .hover(|this| this.bg(cx.theme().colors().element_hover))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.dispatch_action(
                zed_actions::OpenWorktreeInNewWindow {
                    path: path_buf.clone(),
                }
                .boxed_clone(),
                cx,
            );
        })
        .child(
            Icon::new(IconName::Pin)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(label).size(LabelSize::XSmall).truncate())
        .into_any_element()
}

impl super::GitPanel {
    pub(crate) fn fetch_all_repositories(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_push_and_pull(cx) {
            return;
        }
        let workspace = self.workspace.clone();
        let project = self.project.clone();

        let repo_entities: Vec<_> = project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .cloned()
            .collect();

        if repo_entities.is_empty() {
            return;
        }
        telemetry::event!("Git Fetched All Repositories");

        let total = repo_entities.len();
        let mut fetch_tasks = Vec::with_capacity(total);
        for repo in repo_entities {
            let askpass = self.askpass_delegate("git fetch", window, cx);
            let receiver = repo.update(cx, |repo, cx| {
                repo.fetch(FetchOptions::All, askpass, cx)
            });
            fetch_tasks.push(receiver);
        }

        cx.spawn_in(window, async move |this, cx| {
            let results = futures::future::join_all(fetch_tasks).await;
            let mut succeeded = 0usize;
            let mut failed_with_errors = Vec::new();
            for result in results {
                match result {
                    Ok(Ok(_)) => succeeded += 1,
                    Ok(Err(error)) => failed_with_errors.push(format!("{error}")),
                    Err(_canceled) => {
                        failed_with_errors.push("cancelled".to_string());
                    }
                }
            }

            let summary: SharedString = if failed_with_errors.is_empty() {
                format!("Fetched {succeeded}/{total} repositories").into()
            } else {
                format!(
                    "Fetched {succeeded}/{total} repositories — {} failed",
                    failed_with_errors.len()
                )
                .into()
            };

            this.update_in(cx, |_, _window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        let toast = StatusToast::new(summary.clone(), cx, |this, _cx| {
                            this.icon(
                                ui::Icon::new(ui::IconName::Download)
                                    .size(ui::IconSize::Small)
                                    .color(ui::Color::Muted),
                            )
                            .dismiss_button(true)
                        });
                        workspace.toggle_status_toast(toast, cx);
                    })
                    .ok();
            })
            .ok();

            let _ = project;
            Ok::<(), anyhow::Error>(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn pull_all_repositories(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_push_and_pull(cx) {
            return;
        }
        let workspace = self.workspace.clone();
        let project = self.project.clone();

        let repo_entities: Vec<_> = project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .cloned()
            .collect();

        if repo_entities.is_empty() {
            return;
        }
        telemetry::event!("Git Pulled All Repositories");

        let total = repo_entities.len();
        let mut pull_tasks = Vec::new();
        let mut skipped_no_upstream = 0usize;
        for repo in repo_entities {
            // Per-repo: only pull when the current branch has a tracked
            // upstream we can derive a remote from. Anything else (detached
            // HEAD, no branch, upstream gone, no upstream at all) is skipped
            // so we don't prompt for input mid-bulk.
            let pull_args = repo.read_with(cx, |repo, _| {
                let branch = repo.branch.as_ref()?;
                let upstream = branch.upstream.as_ref()?;
                if upstream.tracking.is_gone() {
                    return None;
                }
                let remote = upstream.remote_name()?.to_string();
                Some(remote)
            });
            let Some(remote) = pull_args else {
                skipped_no_upstream += 1;
                continue;
            };
            let askpass = self.askpass_delegate(format!("git pull {remote}"), window, cx);
            let receiver = repo.update(cx, |repo, cx| {
                repo.pull(None, remote.into(), false, askpass, cx)
            });
            pull_tasks.push(receiver);
        }

        if pull_tasks.is_empty() {
            // Nothing to pull: tell the user why up front instead of silently
            // doing nothing.
            cx.spawn_in(window, async move |_, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        let summary: SharedString = format!(
                            "Pulled 0/{total} repositories; {skipped_no_upstream} skipped (no upstream)"
                        )
                        .into();
                        let toast = StatusToast::new(summary, cx, |this, _cx| {
                            this.icon(
                                ui::Icon::new(ui::IconName::ArrowCircle)
                                    .size(ui::IconSize::Small)
                                    .color(ui::Color::Muted),
                            )
                            .dismiss_button(true)
                        });
                        workspace.toggle_status_toast(toast, cx);
                    })
                    .ok();
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
            let _ = project;
            return;
        }

        cx.spawn_in(window, async move |this, cx| {
            let results = futures::future::join_all(pull_tasks).await;
            let mut succeeded = 0usize;
            let mut failed_with_errors = Vec::new();
            for result in results {
                match result {
                    Ok(Ok(_)) => succeeded += 1,
                    Ok(Err(error)) => failed_with_errors.push(format!("{error}")),
                    Err(_canceled) => {
                        failed_with_errors.push("cancelled".to_string());
                    }
                }
            }

            let mut summary = format!("Pulled {succeeded}/{total} repositories");
            if skipped_no_upstream > 0 {
                summary.push_str(&format!("; {skipped_no_upstream} skipped (no upstream)"));
            }
            if !failed_with_errors.is_empty() {
                summary.push_str(&format!(", {} failed", failed_with_errors.len()));
            }
            let summary: SharedString = summary.into();

            this.update_in(cx, |_, _window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        let toast = StatusToast::new(summary.clone(), cx, |this, _cx| {
                            this.icon(
                                ui::Icon::new(ui::IconName::ArrowCircle)
                                    .size(ui::IconSize::Small)
                                    .color(ui::Color::Muted),
                            )
                            .dismiss_button(true)
                        });
                        workspace.toggle_status_toast(toast, cx);
                    })
                    .ok();
            })
            .ok();

            let _ = project;
            Ok::<(), anyhow::Error>(())
        })
        .detach_and_log_err(cx);
    }

    pub(super) fn render_repos_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let project = self.project.clone();
        let git_store = project.read(cx).git_store().clone();
        let store_ref = git_store.read(cx);
        let mut repos: Vec<Entity<Repository>> = store_ref.repositories().values().cloned().collect();
        repos.sort_by(|a, b| {
            a.read(cx)
                .display_name()
                .to_lowercase()
                .cmp(&b.read(cx).display_name().to_lowercase())
        });
        let active_id = store_ref.active_repository().map(|r| r.read(cx).id);

        let pinned = crate::new_panel_settings::RepositoryDashboardPanelSettings::get_global(cx)
            .pinned_repos
            .clone();
        let count = repos.len();
        let has_pinned = !pinned.is_empty();
        let expanded = self.repos_strip_expanded;

        let header = h_flex()
            .h(rems(1.75))
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .justify_between()
            .child(
                h_flex()
                    .gap_1()
                    .id("git-panel-repos-strip-toggle")
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new("Repositories")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("({count})"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.repos_strip_expanded = !this.repos_strip_expanded;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("git-panel-fetch-all", IconName::Download)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Fetch all repositories"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    git::FetchAllRepositories.boxed_clone(),
                                    cx,
                                );
                            }),
                    )
                    .child(
                        IconButton::new("git-panel-pull-all", IconName::ArrowCircle)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Pull all repositories"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    git::PullAllRepositories.boxed_clone(),
                                    cx,
                                );
                            }),
                    )
                    .child(
                        IconButton::new("git-panel-open-graph", IconName::GitBranch)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open commit graph"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Open.boxed_clone(), cx);
                            }),
                    ),
            );

        let body = if expanded {
            let mut list = v_flex().py_0p5();
            for (ix, repo) in repos.iter().enumerate() {
                list = list.child(self.render_repo_strip_row(ix, repo.clone(), active_id, cx));
            }
            if has_pinned {
                list = list
                    .child(
                        Label::new("Pinned")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .children(pinned.iter().enumerate().map(|(ix, path)| {
                        render_pinned_strip_row(ix, path.clone(), cx)
                    }));
            }
            Some(
                list.border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .into_any_element(),
            )
        } else {
            None
        };

        v_flex()
            .child(header)
            .when_some(body, |this, body| this.child(body))
            .into_any_element()
    }

    fn render_repo_strip_row(
        &self,
        ix: usize,
        repo: Entity<Repository>,
        active_id: Option<RepositoryId>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let repo_ref = repo.read(cx);
        let id = repo_ref.id;
        let is_active = active_id == Some(id);
        let display_name = repo_ref.display_name();
        let branch_label: SharedString = repo_ref
            .branch
            .as_ref()
            .map(|branch| branch.name().to_string().into())
            .unwrap_or_else(|| "(detached)".into());
        let tracking = repo_ref
            .branch
            .as_ref()
            .and_then(|branch| branch.tracking_status());
        let dirty_count = repo_ref.status_summary().count;
        let work_dir = repo_ref.snapshot().work_directory_abs_path.to_path_buf();

        let row_bg = is_active.then(|| cx.theme().colors().ghost_element_selected);

        h_flex()
            .id(("git-panel-repo-strip-row", ix))
            .h(rems(1.5))
            .px_2()
            .gap_2()
            .when_some(row_bg, |this, color| this.bg(color))
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .on_mouse_down(MouseButton::Left, {
                let repo = repo.clone();
                move |_, _window, cx| {
                    repo.update(cx, |repo, cx| repo.set_as_active_repository(cx));
                }
            })
            .child(
                Icon::new(if is_active {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                })
                .size(IconSize::XSmall)
                .color(if is_active { Color::Accent } else { Color::Muted }),
            )
            .child(
                Label::new(display_name)
                    .size(LabelSize::XSmall)
                    .truncate(),
            )
            .child(
                Label::new(branch_label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
            .when_some(tracking, |this, status| {
                this.child(render_tracking_chip(status))
            })
            .when(dirty_count > 0, |this| {
                this.child(
                    Label::new(format!("{dirty_count}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Modified),
                )
            })
            .child(
                PopoverMenu::new(("git-panel-repo-strip-menu", ix))
                    .trigger(
                        IconButton::new(
                            ("git-panel-repo-strip-menu-trigger", ix),
                            IconName::Ellipsis,
                        )
                        .shape(ui::IconButtonShape::Square)
                        .icon_size(IconSize::XSmall),
                    )
                    .menu(move |window, cx| {
                        let repo = repo.clone();
                        let work_dir = work_dir.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                            menu.entry("Fetch this repository", None, {
                                let repo = repo.clone();
                                move |window, cx| {
                                    repo.update(cx, |repo, cx| {
                                        repo.set_as_active_repository(cx)
                                    });
                                    window.dispatch_action(git::Fetch.boxed_clone(), cx);
                                }
                            })
                            .entry("Pull this repository", None, {
                                let repo = repo.clone();
                                move |window, cx| {
                                    repo.update(cx, |repo, cx| {
                                        repo.set_as_active_repository(cx)
                                    });
                                    window.dispatch_action(git::Pull.boxed_clone(), cx);
                                }
                            })
                            .entry("Open in terminal", None, {
                                move |window, cx| {
                                    window.dispatch_action(
                                        workspace::OpenTerminal {
                                            working_directory: work_dir.clone(),
                                            local: false,
                                        }
                                        .boxed_clone(),
                                        cx,
                                    );
                                }
                            })
                        }))
                    }),
            )
            .into_any_element()
    }
}

impl super::GitPanel {
    pub(super) fn file_history(
        &mut self,
        _: &git::FileHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        maybe!({
            let entry = self.entries.get(self.selected_entry?)?.status_entry()?;
            let active_repo = self.active_repository.as_ref()?;
            let repo_path = entry.repo_path.clone();
            let git_store = self.project.read(cx).git_store();

            FileHistoryView::open(
                repo_path,
                git_store.downgrade(),
                active_repo.downgrade(),
                self.workspace.clone(),
                window,
                cx,
            );

            Some(())
        });
    }

    pub(super) fn open_file(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        maybe!({
            let entry = self.entries.get(self.selected_entry?)?.status_entry()?;
            let active_repo = self.active_repository.as_ref()?;
            let path = active_repo
                .read(cx)
                .repo_path_to_project_path(&entry.repo_path, cx)?;
            if entry.status.is_deleted() {
                return None;
            }

            let open_task = self
                .workspace
                .update(cx, |workspace, cx| {
                    workspace.open_path_preview(path, None, false, false, true, window, cx)
                })
                .ok()?;

            let workspace = self.workspace.clone();
            cx.spawn_in(window, async move |_, mut cx| {
                let item = open_task
                    .await
                    .notify_workspace_async_err(workspace, &mut cx)
                    .ok_or_else(|| anyhow::anyhow!("Failed to open file"))?;
                if let Some(active_editor) = item.downcast::<Editor>() {
                    if let Some(diff_task) =
                        active_editor.update(cx, |editor, _cx| editor.wait_for_diff_to_load())
                    {
                        diff_task.await;
                    }

                    cx.update(|window, cx| {
                        active_editor.update(cx, |editor, cx| {
                            editor.expand_all_diff_hunks(&ExpandAllDiffHunks, window, cx);

                            let snapshot = editor.snapshot(window, cx);
                            editor.go_to_hunk_before_or_after_position(
                                &snapshot,
                                language::Point::new(0, 0),
                                Direction::Next,
                                true,
                                window,
                                cx,
                            );
                        })
                    })
                    .log_err();
                }

                anyhow::Ok(())
            })
            .detach();

            Some(())
        });
    }

    pub(super) async fn load_commit_message_prompt(cx: &mut AsyncApp) -> String {
        let load = async {
            let store = cx.update(|cx| PromptStore::global(cx)).await.ok()?;
            store
                .update(cx, |s, cx| {
                    s.load(PromptId::BuiltIn(BuiltInPrompt::CommitMessage), cx)
                })
                .await
                .ok()
        };
        load.await
            .unwrap_or_else(|| BuiltInPrompt::CommitMessage.default_content().to_string())
    }

    pub(super) fn github_commit_author(&self, cx: &App) -> Option<(SharedString, SharedString)> {
        let workspace = self.workspace.upgrade()?;
        let workspace = workspace.read(cx);
        let bound_id = workspace.bound_collab_account_id()?.to_string();
        let client = workspace.client().clone();

        let remote_url = self.active_repository.as_ref()?.read(cx).default_remote_url()?;
        let provider_registry = GitHostingProviderRegistry::global(cx);
        let (provider, _) = git::parse_git_remote_url(provider_registry, &remote_url)?;
        if provider.name() != "GitHub" {
            return None;
        }

        let account = client
            .list_accounts()
            .into_iter()
            .find(|a| a.id == bound_id)?;
        let login = account.login.as_deref()?;
        let name = SharedString::from(login.to_string());
        let email = SharedString::from(format!(
            "{}+{}@users.noreply.github.com",
            account.user_id, login
        ));
        Some((name, email))
    }

    pub(super) fn activate_explorer_tab(
        &mut self,
        _: &ActivateExplorerTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_tab(GitPanelTab::Explorer, window, cx);
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(super) struct GitHunkEntry {
    pub(super) repo_path: RepoPath,
    /// Stable identifier within the hunk list so the stage handler can refetch
    /// the live `DiffHunk` (anchors stay valid across edits but the index in
    /// the list does not).
    hunk_index: usize,
    line_label: SharedString,
    is_staged: bool,
}

/// Cached buffer + diff for a file whose hunks have been (or are being)
/// surfaced in the panel. Stored on `GitPanel.hunk_states` keyed by repo path.
pub(super) enum HunkLoadState {
    Loading,
    Loaded {
        buffer: Entity<Buffer>,
        diff: Entity<buffer_diff::BufferDiff>,
        /// Subscription that listens for changes to the diff (e.g. the user
        /// stages, unstages, or restores a hunk from the diff view) and
        /// marks the panel as needing a hunk-row rebuild.
        _diff_subscription: Subscription,
    },
    Failed(SharedString),
}

impl super::GitPanel {
    pub(super) fn build_hunk_list_entries(
        &self,
        repo_path: &RepoPath,
        cx: &Context<Self>,
    ) -> Vec<GitListEntry> {
        let Some(state) = self.hunk_states.get(repo_path) else {
            return vec![GitListEntry::HunkLoading {
                repo_path: repo_path.clone(),
            }];
        };
        match state {
            HunkLoadState::Loading => vec![GitListEntry::HunkLoading {
                repo_path: repo_path.clone(),
            }],
            HunkLoadState::Failed(message) => vec![GitListEntry::HunkError {
                repo_path: repo_path.clone(),
                message: message.clone(),
            }],
            HunkLoadState::Loaded { buffer, diff, .. } => {
                let buffer_snapshot = buffer.read(cx).snapshot();
                let diff_snapshot = diff.read(cx).snapshot(cx);
                diff_snapshot
                    .hunks(&buffer_snapshot.text)
                    .enumerate()
                    .map(|(idx, hunk)| {
                        let start = hunk
                            .buffer_range
                            .start
                            .to_point(&buffer_snapshot.text)
                            .row
                            + 1;
                        let end = hunk
                            .buffer_range
                            .end
                            .to_point(&buffer_snapshot.text)
                            .row
                            + 1;
                        let line_label: SharedString = if start == end {
                            format!("Line {start}").into()
                        } else {
                            format!("Lines {start}-{end}").into()
                        };
                        GitListEntry::Hunk(GitHunkEntry {
                            repo_path: repo_path.clone(),
                            hunk_index: idx,
                            line_label,
                            is_staged: !hunk.status().has_secondary_hunk(),
                        })
                    })
                    .collect()
            }
        }
    }

    /// Flip the expanded state for a file's hunk subtree. When expanding for
    /// the first time, kicks off an async load of the buffer + unstaged diff;
    /// the panel rebuilds entries when the load completes (via the BufferDiff
    /// subscription installed in `ensure_hunks_loaded`).
    pub(super) fn toggle_file_expansion(
        &mut self,
        repo_path: RepoPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.expanded_files.contains(&repo_path) {
            self.expanded_files.remove(&repo_path);
            self.hunk_states.remove(&repo_path);
            self.update_visible_entries(window, cx);
            return;
        }
        self.expanded_files.insert(repo_path.clone());
        self.ensure_hunks_loaded(repo_path, window, cx);
        self.update_visible_entries(window, cx);
    }

    fn ensure_hunks_loaded(
        &mut self,
        repo_path: RepoPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.hunk_states.get(&repo_path), Some(HunkLoadState::Loaded { .. })) {
            return;
        }
        self.hunk_states
            .insert(repo_path.clone(), HunkLoadState::Loading);
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let project = self.project.clone();
        let git_store = project.read(cx).git_store().clone();
        let Some(project_path) = repo
            .read(cx)
            .repo_path_to_project_path(&repo_path, cx)
        else {
            self.hunk_states.insert(
                repo_path,
                HunkLoadState::Failed("no project path for repo path".into()),
            );
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            // `project` and `git_store` are strong handles; `Entity::update`
            // via `AppContext` returns the closure value directly (no Result
            // wrapper), so we don't `?` after these calls.
            let open_task = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx));
            let buffer = match open_task.await {
                Ok(buffer) => buffer,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.hunk_states.insert(
                            repo_path.clone(),
                            HunkLoadState::Failed(format!("open buffer failed: {err}").into()),
                        );
                        cx.notify();
                    })
                    .ok();
                    return anyhow::Ok(());
                }
            };
            // `open_uncommitted_diff` returns a BufferDiff whose
            // `secondary_diff` is wired up to the unstaged diff — required for
            // `stage_or_unstage_hunks` to compute a new index text. The plain
            // `open_unstaged_diff` returns a diff with `secondary_diff = None`,
            // which makes the stage/unstage call a no-op.
            let diff_task = git_store
                .update(cx, |store, cx| {
                    store.open_uncommitted_diff(buffer.clone(), cx)
                });
            let diff = match diff_task.await {
                Ok(diff) => diff,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.hunk_states.insert(
                            repo_path.clone(),
                            HunkLoadState::Failed(format!("load diff failed: {err}").into()),
                        );
                        cx.notify();
                    })
                    .ok();
                    return anyhow::Ok(());
                }
            };
            this.update_in(cx, |this, window, cx| {
                let subscription = cx.subscribe(&diff, |this, _diff, _event, cx| {
                    this.pending_hunk_refresh = true;
                    cx.notify();
                });
                this.hunk_states.insert(
                    repo_path.clone(),
                    HunkLoadState::Loaded {
                        buffer,
                        diff,
                        _diff_subscription: subscription,
                    },
                );
                this.update_visible_entries(window, cx);
            })
            .ok();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Discard a single hunk: replace its buffer range with the original
    /// content from the diff base (HEAD for uncommitted diffs). Mirrors the
    /// editor's `restore_diff_hunks` flow — also unstages the hunk so the
    /// discard is reflected in both worktree and index. No-op if the file is
    /// newly created (no diff base to revert to).
    fn discard_hunk(
        &mut self,
        repo_path: &RepoPath,
        hunk_index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(HunkLoadState::Loaded { buffer, diff, .. }) = self.hunk_states.get(repo_path)
        else {
            return;
        };
        let buffer = buffer.clone();
        let diff = diff.clone();
        let buffer_snapshot = buffer.read(cx).snapshot();
        let file_exists = buffer_snapshot
            .file()
            .is_some_and(|file| file.disk_state().exists());
        let diff_snapshot = diff.read(cx).snapshot(cx);
        let hunks: Vec<_> = diff_snapshot.hunks(&buffer_snapshot.text).collect();
        let Some(hunk) = hunks.get(hunk_index).cloned() else {
            return;
        };

        // Newly-created files have no base text to revert to; the equivalent
        // is "delete this hunk's lines from the buffer", which is what
        // `restore_diff_hunks` skips in the editor. We do the same.
        if hunk.diff_base_byte_range.is_empty()
            && hunk.buffer_range.start == hunk.buffer_range.end
        {
            return;
        }

        let original = diff_snapshot
            .base_text()
            .as_rope()
            .slice(hunk.diff_base_byte_range.clone())
            .to_string();

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(hunk.buffer_range.clone(), original)], None, cx);
        });

        // Also unstage the hunk so the discard is reflected in the index.
        diff.update(cx, |diff, cx| {
            diff.stage_or_unstage_hunks(false, &[hunk], &buffer_snapshot, file_exists, cx);
        });
        cx.notify();
    }

    /// Apply (or revert) a single hunk's edit against the buffer's index via
    /// the existing `BufferDiff::stage_or_unstage_hunks` primitive. Refetches
    /// the live `DiffHunk` so any edits between expanding the row and clicking
    /// the button are reflected.
    fn toggle_hunk_stage(
        &mut self,
        repo_path: &RepoPath,
        hunk_index: usize,
        stage: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(HunkLoadState::Loaded { buffer, diff, .. }) = self.hunk_states.get(repo_path)
        else {
            return;
        };
        let buffer = buffer.clone();
        let diff = diff.clone();
        let buffer_snapshot = buffer.read(cx).snapshot();
        let file_exists = buffer_snapshot
            .file()
            .is_some_and(|file| file.disk_state().exists());
        let diff_snapshot = diff.read(cx).snapshot(cx);
        let hunks: Vec<_> = diff_snapshot.hunks(&buffer_snapshot.text).collect();
        let Some(hunk) = hunks.get(hunk_index).cloned() else {
            return;
        };
        diff.update(cx, |diff, cx| {
            diff.stage_or_unstage_hunks(stage, &[hunk], &buffer_snapshot, file_exists, cx);
        });
        cx.notify();
    }

    pub(super) fn render_hunk_entry(
        &self,
        ix: usize,
        hunk: &GitHunkEntry,
        has_write_access: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let label = hunk.line_label.clone();
        let is_staged = hunk.is_staged;
        let repo_path = hunk.repo_path.clone();
        let repo_path_for_discard = hunk.repo_path.clone();
        let hunk_index = hunk.hunk_index;
        let action_label = if is_staged { "Unstage hunk" } else { "Stage hunk" };

        h_flex()
            .id(("git-hunk-row", ix))
            .h(rems(1.5))
            .pl(px(28.))
            .pr_2()
            .gap_2()
            .child(
                Icon::new(IconName::Hash)
                    .size(IconSize::XSmall)
                    .color(if is_staged { Color::Accent } else { Color::Muted }),
            )
            .child(
                Label::new(label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Button::new(("hunk-toggle", ix), action_label)
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::XSmall)
                    .disabled(!has_write_access)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_hunk_stage(&repo_path, hunk_index, !is_staged, cx);
                    })),
            )
            .child(
                Button::new(("hunk-discard", ix), "Discard")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::XSmall)
                    .color(Color::Error)
                    .disabled(!has_write_access)
                    .tooltip(Tooltip::text(
                        "Revert this hunk to its HEAD content (also unstages)",
                    ))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.discard_hunk(&repo_path_for_discard, hunk_index, cx);
                    })),
            )
            .into_any_element()
    }

    pub(super) fn render_hunk_placeholder(
        &self,
        ix: usize,
        message: impl Into<SharedString>,
        color: Color,
        _cx: &Context<Self>,
    ) -> AnyElement {
        h_flex()
            .id(("git-hunk-placeholder", ix))
            .h(rems(1.5))
            .pl(px(28.))
            .pr_2()
            .child(
                Label::new(message.into())
                    .size(LabelSize::XSmall)
                    .color(color),
            )
            .into_any_element()
    }
}

impl super::GitPanel {
    pub(super) fn stash_selected(
        &mut self,
        _: &StashFile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_repository) = self.active_repository.clone() else {
            return;
        };
        let Some(status_entry) = self
            .get_selected_entry()
            .and_then(|entry| entry.status_entry())
            .cloned()
        else {
            return;
        };

        let path = status_entry.repo_path;
        let message = format!("lathe-stash-file {}", path.as_unix_str());

        cx.spawn(async move |this, cx| {
            let stash_task = active_repository.update(cx, |repo, cx| {
                repo.stash_entries_with_message(vec![path], message, cx)
            });
            let result = stash_task.await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.show_error_toast("stash file", error, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_output_handler_strips_ansi_codes() {
        use alacritty_terminal::vte::ansi;

        let cases = [
            ("no escape codes here\n", "no escape codes here\n"),
            ("\x1b[31mhello\x1b[0m", "hello"),
            ("\x1b[1;32mfoo\x1b[0m bar", "foo bar"),
            ("progress 10%\rprogress 100%\n", "progress 100%\n"),
        ];

        for (input, expected) in cases {
            let mut handler = GitOutputHandler::default();
            let mut processor = ansi::Processor::<ansi::StdSyncHandler>::default();
            processor.advance(&mut handler, input.as_bytes());
            assert_eq!(handler.output, expected);
        }
    }
}
