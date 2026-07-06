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
