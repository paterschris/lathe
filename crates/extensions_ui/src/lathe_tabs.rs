//! Which files have editor tabs, for the `tab-opened` and `tab-closed` file
//! events. Counted from workspace item events rather than buffer lifetimes,
//! because Lathe keeps buffers loaded for things other than tabs (language
//! servers, search, go to definition) and frees them on its own schedule.

use std::sync::Arc;

use collections::HashMap;
use editor::Editor;
use extension_host::wasm_host::lathe::LatheBufferEvent;
use gpui::{App, AppContext as _, Entity, EntityId, WeakEntity};
use language::Buffer;
use workspace::{Event as WorkspaceEvent, Workspace};

use crate::lathe_editor::file_location;

/// A file's (project path, absolute path), which identifies it here.
pub type FilePaths = (Option<String>, Option<String>);

/// Receives `TabOpened` (with the file's buffer) and `TabClosed`.
pub type TabSink = Arc<dyn Fn(LatheBufferEvent, Option<Entity<Buffer>>, FilePaths, &mut App)>;

struct OpenFile {
    tabs: usize,
    buffer: WeakEntity<Buffer>,
}

/// Every file with at least one editor tab, across all windows.
#[derive(Default)]
pub struct OpenTabs {
    files: HashMap<FilePaths, OpenFile>,
    /// The file each tab (workspace item) shows.
    items: HashMap<EntityId, FilePaths>,
}

impl OpenTabs {
    /// Files with tabs and their buffers, for telling a newly loaded
    /// extension about them.
    pub fn open_files(&self) -> Vec<(FilePaths, Entity<Buffer>)> {
        self.files
            .iter()
            .filter_map(|(paths, file)| Some((paths.clone(), file.buffer.upgrade()?)))
            .collect()
    }
}

pub fn init(sink: TabSink, cx: &mut App) -> Entity<OpenTabs> {
    let open_tabs = cx.new(|_| OpenTabs::default());
    cx.observe_new({
        let open_tabs = open_tabs.clone();
        move |_: &mut Workspace, _, cx| {
            let open_tabs = open_tabs.clone();
            let sink = sink.clone();
            cx.subscribe_self(move |_, event: &WorkspaceEvent, cx| match event {
                WorkspaceEvent::ItemAdded { item } => {
                    let Some(editor) = item.act_as::<Editor>(cx) else {
                        return;
                    };
                    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
                        return;
                    };
                    if buffer.read(cx).file().is_none() {
                        return;
                    }
                    let location = file_location(buffer.read(cx), cx);
                    let paths = (location.path, location.abs_path);
                    tab_added(&open_tabs, item.item_id(), paths, buffer, &sink, cx);
                }
                WorkspaceEvent::ItemRemoved { item_id } => {
                    tab_removed(&open_tabs, *item_id, &sink, cx);
                }
                _ => {}
            })
            .detach();
        }
    })
    .detach();
    open_tabs
}

fn tab_added(
    open_tabs: &Entity<OpenTabs>,
    item_id: EntityId,
    paths: FilePaths,
    buffer: Entity<Buffer>,
    sink: &TabSink,
    cx: &mut App,
) {
    let first = open_tabs.update(cx, |open_tabs, _| {
        if open_tabs.items.insert(item_id, paths.clone()).is_some() {
            return false;
        }
        // A file whose last tab just closed keeps its entry until the
        // deferred check in `tab_removed`, so a tab moved between panes
        // finds it here and isn't reported as newly opened.
        let newly_open = !open_tabs.files.contains_key(&paths);
        let file = open_tabs.files.entry(paths.clone()).or_insert(OpenFile {
            tabs: 0,
            buffer: buffer.downgrade(),
        });
        file.tabs += 1;
        file.buffer = buffer.downgrade();
        newly_open
    });
    if first {
        sink(LatheBufferEvent::TabOpened, Some(buffer), paths, cx);
    }
}

fn tab_removed(open_tabs: &Entity<OpenTabs>, item_id: EntityId, sink: &TabSink, cx: &mut App) {
    let last = open_tabs.update(cx, |open_tabs, _| {
        let paths = open_tabs.items.remove(&item_id)?;
        let file = open_tabs.files.get_mut(&paths)?;
        file.tabs = file.tabs.saturating_sub(1);
        (file.tabs == 0).then_some(paths)
    });
    let Some(paths) = last else {
        return;
    };
    // Moving a tab between panes removes it and then adds it; decide once
    // both have happened, so a move isn't reported as closed and reopened.
    let open_tabs = open_tabs.downgrade();
    let sink = sink.clone();
    cx.defer(move |cx| {
        let Some(open_tabs) = open_tabs.upgrade() else {
            return;
        };
        let closed = open_tabs.update(cx, |open_tabs, _| {
            if open_tabs
                .files
                .get(&paths)
                .is_some_and(|file| file.tabs == 0)
            {
                open_tabs.files.remove(&paths);
                true
            } else {
                false
            }
        });
        if closed {
            sink(LatheBufferEvent::TabClosed, None, paths, cx);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use parking_lot::Mutex;
    use project::Project;
    use serde_json::json;
    use util::path;
    use util::rel_path::rel_path;
    use workspace::{AppState, SaveIntent, SplitDirection};

    type Seen = Arc<Mutex<Vec<(LatheBufferEvent, Option<String>, bool)>>>;

    async fn open(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Seen, &mut VisualTestContext) {
        let seen = Seen::default();
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            let seen = seen.clone();
            init(
                Arc::new(move |event, buffer, (path, _), _| {
                    seen.lock().push((event, path, buffer.is_some()))
                }),
                cx,
            );
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({ "a.rs": "// TODO: a", "b.rs": "fn b() {}" }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new(None, project, app_state.clone(), window, cx)
        });
        (workspace, seen, cx)
    }

    async fn open_file(workspace: &Entity<Workspace>, name: &str, cx: &mut VisualTestContext) {
        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
                .expect("a worktree")
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path(name)), None, true, window, cx)
            })
            .await
            .expect("opening the file");
        cx.run_until_parked();
    }

    async fn close_active(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let item_id = pane
            .read_with(cx, |pane, _| pane.active_item().map(|item| item.item_id()))
            .expect("an active tab");
        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
        })
        .await
        .unwrap();
        cx.run_until_parked();
    }

    fn take(seen: &Seen) -> Vec<(LatheBufferEvent, Option<String>, bool)> {
        std::mem::take(&mut *seen.lock())
    }

    #[gpui::test]
    async fn test_tab_events_follow_tabs(cx: &mut TestAppContext) {
        let (workspace, seen, cx) = open(cx).await;

        open_file(&workspace, "a.rs", cx).await;
        assert_eq!(
            take(&seen),
            vec![(LatheBufferEvent::TabOpened, Some("a.rs".into()), true)],
            "the first tab opens the file, with its buffer"
        );

        // A second tab on the same file, in a split.
        workspace
            .update_in(cx, |workspace, window, cx| {
                let pane = workspace.active_pane().clone();
                workspace.split_and_clone(pane, SplitDirection::Right, window, cx)
            })
            .await
            .expect("a split with a copy of the tab");
        cx.run_until_parked();
        assert!(
            take(&seen).is_empty(),
            "a second tab on an open file is no event"
        );

        close_active(&workspace, cx).await;
        assert!(take(&seen).is_empty(), "one tab is still open");

        open_file(&workspace, "b.rs", cx).await;
        assert_eq!(
            take(&seen),
            vec![(LatheBufferEvent::TabOpened, Some("b.rs".into()), true)]
        );

        // Close a.rs's last tab, wherever it is.
        let (pane, item_id) = workspace.read_with(cx, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .find_map(|pane| {
                    pane.read(cx)
                        .items()
                        .find(|item| item.tab_content_text(0, cx).as_ref() == "a.rs")
                        .map(|item| (pane.clone(), item.item_id()))
                })
                .expect("a.rs still has a tab")
        });
        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
        })
        .await
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            take(&seen),
            vec![(LatheBufferEvent::TabClosed, Some("a.rs".into()), false)],
            "closing the last tab closes the file"
        );
    }

    #[gpui::test]
    async fn test_moving_a_tab_is_not_closing_it(cx: &mut TestAppContext) {
        let (workspace, seen, cx) = open(cx).await;
        open_file(&workspace, "a.rs", cx).await;
        take(&seen);

        let (from, item) = workspace.read_with(cx, |workspace, cx| {
            let pane = workspace.active_pane().clone();
            let item = pane.read(cx).active_item().expect("a tab");
            (pane, item)
        });
        let to = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(from.clone(), SplitDirection::Right, window, cx)
        });
        // Move the tab: remove it from one pane and add it to the other, in
        // one update, the way dragging does.
        workspace.update_in(cx, |_, window, cx| {
            from.update(cx, |pane, cx| {
                pane.remove_item(item.item_id(), false, false, window, cx)
            });
            to.update(cx, |pane, cx| {
                pane.add_item(item.boxed_clone(), true, true, None, window, cx)
            });
        });
        cx.run_until_parked();
        let events = take(&seen);
        assert!(events.is_empty(), "{events:?}");
    }
}
