use std::{path::PathBuf, sync::Arc};

use file_icons::FileIcons;
use fuzzy_nucleo::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action as _, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    Task, TaskExt, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathExt as _};
use workspace::{
    ModalView, OpenMode, OpenOptions, OpenVisible, Workspace, recent_files::RecentFilesDb,
};

pub struct RecentFiles {
    picker: Entity<Picker<RecentFilesDelegate>>,
}

impl RecentFiles {
    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let weak_workspace = cx.weak_entity();
        let fs = workspace.app_state().fs.clone();
        let focus_handle = workspace.focus_handle(cx);
        workspace.toggle_modal(window, cx, |window, cx| {
            let delegate =
                RecentFilesDelegate::new(cx.entity().downgrade(), weak_workspace, focus_handle);
            let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));

            let db = RecentFilesDb::global(cx);
            let picker_handle = picker.downgrade();
            cx.spawn_in(window, async move |_, cx| {
                let files = db.recent_files(fs).await.log_err().unwrap_or_default();
                picker_handle
                    .update_in(cx, |picker, window, cx| {
                        picker.delegate.set_files(files);
                        picker.refresh(window, cx);
                    })
                    .ok();
            })
            .detach();

            Self { picker }
        });
    }
}

impl Render for RecentFiles {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RecentFiles")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

impl Focusable for RecentFiles {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for RecentFiles {}
impl ModalView for RecentFiles {}

struct RecentFile {
    path: PathBuf,
    /// The path shown to the user, with the home directory as `~`.
    display_path: SharedString,
    file_name: SharedString,
}

pub struct RecentFilesDelegate {
    recent_files: WeakEntity<RecentFiles>,
    workspace: WeakEntity<Workspace>,
    /// Where footer key bindings are looked up. Reading it from the modal would
    /// read the picker while it is rendering.
    focus_handle: FocusHandle,
    files: Vec<RecentFile>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl RecentFilesDelegate {
    fn new(
        recent_files: WeakEntity<RecentFiles>,
        workspace: WeakEntity<Workspace>,
        focus_handle: FocusHandle,
    ) -> Self {
        Self {
            recent_files,
            workspace,
            focus_handle,
            files: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
        }
    }

    fn set_files(&mut self, paths: Vec<PathBuf>) {
        self.files = paths
            .into_iter()
            .map(|path| {
                let display_path =
                    SharedString::from(path.compact().to_string_lossy().into_owned());
                let file_name = path
                    .file_name()
                    .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| display_path.clone());
                RecentFile {
                    path,
                    display_path,
                    file_name,
                }
            })
            .collect();
    }

    fn remove_file(&mut self, match_index: usize, cx: &mut Context<Picker<Self>>) {
        let Some(file_index) = self.matches.get(match_index).map(|mat| mat.candidate_id) else {
            return;
        };
        if file_index >= self.files.len() {
            return;
        }
        let removed = self.files.remove(file_index);
        let db = RecentFilesDb::global(cx);
        cx.background_spawn(async move {
            db.remove_recent_file(removed.path.to_string_lossy().into_owned())
                .await
                .log_err();
        })
        .detach();
    }
}

impl PickerDelegate for RecentFilesDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "recent files"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search recent files…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(if self.files.is_empty() {
            "No recent files".into()
        } else {
            "No matching files".into()
        })
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim_start();
        self.matches = if query.is_empty() {
            self.files
                .iter()
                .enumerate()
                .map(|(candidate_id, file)| StringMatch {
                    candidate_id,
                    string: file.display_path.clone(),
                    positions: Vec::new(),
                    score: 0.0,
                })
                .collect()
        } else {
            let candidates = self
                .files
                .iter()
                .enumerate()
                .map(|(id, file)| StringMatchCandidate::new(id, &file.display_path))
                .collect::<Vec<_>>();
            match_strings(
                &candidates,
                query,
                fuzzy_nucleo::StringMatchOptions {
                    max_results: fuzzy_nucleo::MaxResults::limit::<100>(),
                    case: fuzzy_nucleo::Case::smart_if_uppercase_in(query),
                    ..Default::default()
                },
            )
        };
        self.selected_index = 0;
        cx.notify();
        Task::ready(())
    }

    /// Opens only the chosen file. The default reuses the current window when
    /// it has nothing open and otherwise opens a new window; the secondary
    /// action opens the file as a tab in the current window instead.
    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(file) = self
            .matches
            .get(self.selected_index)
            .and_then(|mat| self.files.get(mat.candidate_id))
        else {
            return;
        };
        let path = file.path.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                if secondary {
                    workspace
                        .open_abs_path(
                            path,
                            OpenOptions {
                                visible: Some(OpenVisible::None),
                                ..Default::default()
                            },
                            window,
                            cx,
                        )
                        .detach_and_log_err(cx);
                } else {
                    workspace
                        .open_workspace_for_paths(OpenMode::NewWindow, vec![path], window, cx)
                        .detach_and_log_err(cx);
                }
            })
            .log_err();
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.recent_files
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let file = self.files.get(mat.candidate_id)?;

        // The display path ends with the file name, so match positions past its
        // start belong to the name and earlier ones to the directory.
        let name_start = file.display_path.len().saturating_sub(file.file_name.len());
        let (directory_positions, name_positions): (Vec<usize>, Vec<usize>) = mat
            .positions
            .iter()
            .partition(|position| **position < name_start);
        let name_positions = name_positions
            .into_iter()
            .map(|position| position - name_start)
            .collect::<Vec<_>>();
        let directory = file.display_path.get(..name_start).unwrap_or_default();

        let icon = FileIcons::get_icon(&file.path, cx)
            .map(Icon::from_path)
            .unwrap_or_else(|| Icon::new(IconName::File))
            .color(Color::Muted);

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(icon)
                .child(
                    h_flex()
                        .gap_2()
                        .min_w_0()
                        .child(HighlightedLabel::new(
                            file.file_name.clone(),
                            name_positions,
                        ))
                        .child(
                            HighlightedLabel::new(directory.to_string(), directory_positions)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .end_slot::<IconButton>(selected.then(|| {
                    IconButton::new(("remove-recent-file", ix), IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Remove from Recent Files"))
                        .on_click(cx.listener(move |picker, _, window, cx| {
                            cx.stop_propagation();
                            picker.delegate.remove_file(ix, cx);
                            picker.refresh(window, cx);
                        }))
                })),
        )
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        if self.matches.is_empty() {
            return None;
        }
        let focus_handle = self.focus_handle.clone();
        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_1()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Button::new("open-in-current-window", "Open in This Window")
                        .key_binding(KeyBinding::for_action_in(
                            &menu::SecondaryConfirm,
                            &focus_handle,
                            cx,
                        ))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx)
                        }),
                )
                .child(
                    Button::new("open-file", "Open")
                        .key_binding(KeyBinding::for_action_in(&menu::Confirm, &focus_handle, cx))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                        }),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::path::Path;
    use util::path;
    use workspace::{AppState, MultiWorkspace, open_paths};

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            let state = AppState::test(cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }

    fn worktree_roots(window: gpui::AnyWindowHandle, cx: &mut TestAppContext) -> Vec<PathBuf> {
        let multi_workspace = window.downcast::<MultiWorkspace>().unwrap();
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                    .collect()
            })
            .unwrap()
    }

    /// Every worktree, including the hidden ones single files open into.
    fn all_worktree_roots(window: gpui::AnyWindowHandle, cx: &mut TestAppContext) -> Vec<PathBuf> {
        let multi_workspace = window.downcast::<MultiWorkspace>().unwrap();
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .project()
                    .read(cx)
                    .worktrees(cx)
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                    .collect()
            })
            .unwrap()
    }

    #[gpui::test]
    async fn test_recent_file_opens_only_that_file(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/project"),
                json!({ "src": { "main.rs": "fn main() {}" }, "other.rs": "" }),
            )
            .await;

        cx.update(|cx| {
            open_paths(
                &[PathBuf::from(path!("/project"))],
                app_state.clone(),
                workspace::OpenOptions::default(),
                cx,
            )
        })
        .await
        .unwrap();
        cx.run_until_parked();

        let window = cx.update(|cx| cx.windows()[0]);
        let workspace = window
            .downcast::<MultiWorkspace>()
            .unwrap()
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window, cx);

        let main_rs = PathBuf::from(path!("/project/src/main.rs"));
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(main_rs.clone(), OpenOptions::default(), window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let recorded = cx
            .update(|_, cx| RecentFilesDb::global(cx))
            .recent_files(app_state.fs.clone())
            .await
            .unwrap();
        assert_eq!(
            recorded,
            vec![main_rs.clone()],
            "activating a file should record it"
        );

        cx.dispatch_action(zed_actions::OpenRecentFile);
        cx.run_until_parked();
        let picker = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<RecentFiles>(cx)
                .expect("the recent files picker should open")
                .read(cx)
                .picker
                .clone()
        });
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.match_count(), 1);
            assert_eq!(picker.delegate.files[0].path, main_rs);
        });

        // This window already has a project, so the file gets a window of its own
        // holding just that file, not the folder it came from.
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx)
        });
        cx.run_until_parked();

        let windows = cx.update(|_, cx| cx.windows());
        assert_eq!(windows.len(), 2, "the file should open in a new window");
        assert_eq!(
            all_worktree_roots(windows[1], cx),
            vec![main_rs.clone()],
            "only the file should be opened, without its project folder"
        );
        assert_eq!(
            worktree_roots(window, cx),
            vec![PathBuf::from(path!("/project"))],
            "the original window keeps its project"
        );
    }

    #[gpui::test]
    async fn test_recent_file_from_empty_window_reuses_it(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/project"), json!({ "lib.rs": "" }))
            .await;
        let lib_rs = PathBuf::from(path!("/project/lib.rs"));
        cx.update(|cx| RecentFilesDb::global(cx))
            .record_recent_file(lib_rs.clone())
            .await
            .unwrap();

        // A freshly launched instance: one window with nothing open.
        cx.update(|cx| {
            workspace::open_new(
                workspace::OpenOptions::default(),
                app_state.clone(),
                cx,
                |_, _, _| {},
            )
        })
        .await
        .unwrap();
        cx.run_until_parked();

        let window = cx.update(|cx| cx.windows()[0]);
        let workspace = window
            .downcast::<MultiWorkspace>()
            .unwrap()
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window, cx);

        workspace.update_in(cx, |workspace, window, cx| {
            RecentFiles::toggle(workspace, window, cx)
        });
        cx.run_until_parked();
        let picker = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<RecentFiles>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx)
        });
        cx.run_until_parked();

        assert_eq!(
            cx.update(|_, cx| cx.windows().len()),
            1,
            "the empty window should be reused rather than left behind"
        );
        assert_eq!(
            all_worktree_roots(window, cx),
            vec![lib_rs.clone()],
            "only the file should be opened, without its project folder"
        );
        let active_path = workspace.read_with(cx, |workspace, cx| {
            let project_path = workspace.active_item(cx)?.project_path(cx)?;
            workspace
                .project()
                .read(cx)
                .absolute_path(&project_path, cx)
        });
        assert_eq!(active_path, Some(lib_rs));
    }

    #[gpui::test]
    async fn test_recent_file_secondary_opens_in_current_window(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/project"), json!({ "a.rs": "" }))
            .await;
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/elsewhere"), json!({ "notes.md": "" }))
            .await;
        cx.update(|cx| RecentFilesDb::global(cx))
            .record_recent_file(PathBuf::from(path!("/elsewhere/notes.md")))
            .await
            .unwrap();

        cx.update(|cx| {
            open_paths(
                &[PathBuf::from(path!("/project"))],
                app_state.clone(),
                workspace::OpenOptions::default(),
                cx,
            )
        })
        .await
        .unwrap();
        cx.run_until_parked();

        let window = cx.update(|cx| cx.windows()[0]);
        let workspace = window
            .downcast::<MultiWorkspace>()
            .unwrap()
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window, cx);

        workspace.update_in(cx, |workspace, window, cx| {
            RecentFiles::toggle(workspace, window, cx)
        });
        cx.run_until_parked();
        let picker = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<RecentFiles>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(true, window, cx)
        });
        cx.run_until_parked();

        assert_eq!(cx.update(|_, cx| cx.windows().len()), 1);
        let active_path = workspace.read_with(cx, |workspace, cx| {
            let project_path = workspace.active_item(cx)?.project_path(cx)?;
            workspace
                .project()
                .read(cx)
                .absolute_path(&project_path, cx)
        });
        assert_eq!(
            active_path.as_deref(),
            Some(Path::new(path!("/elsewhere/notes.md")))
        );
        assert_eq!(
            worktree_roots(window, cx),
            vec![PathBuf::from(path!("/project"))],
            "opening the file here must not add its folder to the project"
        );
    }
}
