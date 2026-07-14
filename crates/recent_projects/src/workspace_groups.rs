use std::{cmp, collections::BTreeMap, sync::Arc};

use anyhow::Context as _;
use client::{Client, accounts::CollabAccount};
use db::kvp::KeyValueStore;
use fuzzy_nucleo::{Case, LengthPenalty, StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, PromptLevel, Render, Styled, Subscription, Task, WeakEntity,
    Window, actions, rems,
};
use picker::{Picker, PickerDelegate};
use project::ProjectGroupKey;
use serde::{Deserialize, Serialize};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use workspace::{
    ModalView, MultiWorkspace, OpenMode, SerializedProjectGroup, Workspace,
    with_active_or_new_workspace,
};

const INDEX_KEY: &str = "workspace_groups";
const BINDINGS_KEY: &str = "workspace_group_account_bindings";

actions!(
    workspace_groups,
    [
        /// Save the currently open projects as a named workspace group.
        SaveWorkspaceGroup,
        /// Open a saved workspace group in a new window.
        OpenWorkspaceGroup,
        /// Overwrite the window's active group with the current set of projects.
        UpdateCurrentWorkspaceGroup,
        /// Rename the window's active workspace group.
        RenameCurrentWorkspaceGroup,
        /// Bind the currently active collab account to the window's workspace group.
        BindWorkspaceGroupAccount,
        /// Remove any collab account binding for the window's workspace group.
        UnbindWorkspaceGroupAccount,
    ]
);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedWorkspaceGroup {
    pub name: String,
    pub key: SerializedProjectGroup,
}

fn load_map(kvp: &KeyValueStore) -> BTreeMap<String, SerializedProjectGroup> {
    let Ok(Some(raw)) = kvp.read_kvp(INDEX_KEY) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn list_groups(kvp: &KeyValueStore) -> Vec<NamedWorkspaceGroup> {
    load_map(kvp)
        .into_iter()
        .map(|(name, key)| NamedWorkspaceGroup { name, key })
        .collect()
}

async fn save_group_async(
    kvp: KeyValueStore,
    name: String,
    key: SerializedProjectGroup,
) -> anyhow::Result<()> {
    let mut map = load_map(&kvp);
    map.insert(name, key);
    let json = serde_json::to_string(&map).context("serialize workspace groups")?;
    kvp.write_kvp(INDEX_KEY.to_string(), json).await
}

async fn delete_group_async(kvp: KeyValueStore, name: String) -> anyhow::Result<bool> {
    let mut map = load_map(&kvp);
    let existed = map.remove(&name).is_some();
    if existed {
        let json = serde_json::to_string(&map).context("serialize workspace groups")?;
        kvp.write_kvp(INDEX_KEY.to_string(), json).await?;
        let mut bindings = load_bindings(&kvp);
        if bindings.remove(&name).is_some() {
            write_bindings(&kvp, &bindings).await.log_err();
        }
    }
    Ok(existed)
}

async fn rename_group_async(
    kvp: KeyValueStore,
    old_name: String,
    new_name: String,
) -> anyhow::Result<bool> {
    if old_name == new_name {
        return Ok(true);
    }
    let mut map = load_map(&kvp);
    let Some(key) = map.remove(&old_name) else {
        return Ok(false);
    };
    map.insert(new_name.clone(), key);
    let json = serde_json::to_string(&map).context("serialize workspace groups")?;
    kvp.write_kvp(INDEX_KEY.to_string(), json).await?;
    let mut bindings = load_bindings(&kvp);
    if let Some(account_id) = bindings.remove(&old_name) {
        bindings.insert(new_name, account_id);
        write_bindings(&kvp, &bindings).await.log_err();
    }
    Ok(true)
}

fn load_bindings(kvp: &KeyValueStore) -> BTreeMap<String, String> {
    let Ok(Some(raw)) = kvp.read_kvp(BINDINGS_KEY) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

async fn write_bindings(
    kvp: &KeyValueStore,
    bindings: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(bindings).context("serialize workspace group bindings")?;
    kvp.write_kvp(BINDINGS_KEY.to_string(), json).await
}

pub fn binding_for(kvp: &KeyValueStore, group_name: &str) -> Option<String> {
    load_bindings(kvp).remove(group_name)
}

async fn set_binding_async(
    kvp: KeyValueStore,
    group_name: String,
    account_id: String,
) -> anyhow::Result<()> {
    let mut bindings = load_bindings(&kvp);
    bindings.insert(group_name, account_id);
    write_bindings(&kvp, &bindings).await
}

async fn clear_binding_async(kvp: KeyValueStore, group_name: String) -> anyhow::Result<bool> {
    let mut bindings = load_bindings(&kvp);
    if bindings.remove(&group_name).is_none() {
        return Ok(false);
    }
    write_bindings(&kvp, &bindings).await?;
    Ok(true)
}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &SaveWorkspaceGroup, cx| {
        handle_save(cx);
    });
    cx.on_action(|_: &OpenWorkspaceGroup, cx| {
        handle_open(cx);
    });
    cx.on_action(|_: &UpdateCurrentWorkspaceGroup, cx| {
        handle_update_current(cx);
    });
    cx.on_action(|_: &RenameCurrentWorkspaceGroup, cx| {
        handle_rename_current(cx);
    });
    cx.on_action(|_: &BindWorkspaceGroupAccount, cx| {
        handle_bind_account(cx);
    });
    cx.on_action(|_: &UnbindWorkspaceGroupAccount, cx| {
        handle_unbind_account(cx);
    });
}

fn active_multi_workspace(cx: &App) -> Option<gpui::WindowHandle<MultiWorkspace>> {
    cx.active_window()
        .and_then(|w| w.downcast::<MultiWorkspace>())
}

fn handle_update_current(cx: &mut App) {
    let Some(handle) = active_multi_workspace(cx) else {
        return;
    };
    cx.defer(move |cx| {
        let Ok(pair) = handle.update(cx, |mw, _, _| {
            let name = mw.workspace_group_name()?.to_owned();
            let key: ProjectGroupKey = mw.project_group_keys().into_iter().next()?;
            Some((name, SerializedProjectGroup::from_group(&key, true)))
        }) else {
            return;
        };
        let Some((name, key)) = pair else { return };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            save_group_async(kvp, name, key).await.log_err();
        })
        .detach();
    });
}

fn handle_rename_current(cx: &mut App) {
    let Some(handle) = active_multi_workspace(cx) else {
        return;
    };
    cx.defer(move |cx| {
        handle
            .update(cx, |mw, window, cx| {
                let Some(old_name) = mw.workspace_group_name().map(str::to_owned) else {
                    return;
                };
                let workspace = mw.workspace().clone();
                let mw_handle = cx.entity().downgrade();
                workspace.update(cx, |workspace, cx| {
                    let focus_handle = workspace.focus_handle(cx);
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        RenameWorkspaceGroupModal::new(
                            old_name,
                            mw_handle,
                            focus_handle,
                            window,
                            cx,
                        )
                    });
                });
            })
            .log_err();
    });
}

fn handle_bind_account(cx: &mut App) {
    let Some(handle) = active_multi_workspace(cx) else {
        return;
    };
    let client = Client::global(cx);
    let Some(account_id) = client.active_account_id() else {
        log::warn!("bind_workspace_group_account: no active collab account");
        return;
    };
    cx.defer(move |cx| {
        let Ok(group_name) =
            handle.update(cx, |mw, _, _| mw.workspace_group_name().map(str::to_owned))
        else {
            return;
        };
        let Some(group_name) = group_name else {
            log::warn!("bind_workspace_group_account: current window has no workspace group");
            return;
        };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            set_binding_async(kvp, group_name, account_id)
                .await
                .log_err();
        })
        .detach();
    });
}

fn handle_unbind_account(cx: &mut App) {
    let Some(handle) = active_multi_workspace(cx) else {
        return;
    };
    cx.defer(move |cx| {
        let Ok(group_name) =
            handle.update(cx, |mw, _, _| mw.workspace_group_name().map(str::to_owned))
        else {
            return;
        };
        let Some(group_name) = group_name else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            clear_binding_async(kvp, group_name).await.log_err();
        })
        .detach();
    });
}

fn handle_save(cx: &mut App) {
    let Some(window) = cx.active_window() else {
        return;
    };
    let Some(multi_workspace) = window.downcast::<MultiWorkspace>() else {
        return;
    };
    cx.defer(move |cx| {
        multi_workspace
            .update(cx, |mw, window, cx| {
                let keys: Vec<ProjectGroupKey> = mw.project_group_keys();
                if keys.is_empty() {
                    return;
                }
                let serialized: Vec<SerializedProjectGroup> = keys
                    .iter()
                    .map(|k| SerializedProjectGroup::from_group(k, true))
                    .collect();
                let preset_name = mw.workspace_group_name().map(str::to_owned);
                let mw_handle = cx.entity().downgrade();
                let workspace = mw.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    let focus_handle = workspace.focus_handle(cx);
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        SaveWorkspaceGroupModal::new(
                            serialized,
                            preset_name,
                            mw_handle,
                            focus_handle,
                            window,
                            cx,
                        )
                    });
                });
            })
            .log_err();
    });
}

fn handle_open(cx: &mut App) {
    with_active_or_new_workspace(cx, move |workspace, window, cx| {
        let weak = cx.entity().downgrade();
        let focus_handle = workspace.focus_handle(cx);
        workspace.toggle_modal(window, cx, move |window, cx| {
            OpenWorkspaceGroupModal::new(weak, focus_handle, window, cx)
        });
    });
}

// ============================= save modal =============================

pub struct SaveWorkspaceGroupModal {
    picker: Entity<Picker<SaveWorkspaceGroupDelegate>>,
    _subscription: Subscription,
}

impl SaveWorkspaceGroupModal {
    fn new(
        keys: Vec<SerializedProjectGroup>,
        preset_name: Option<String>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        _focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let existing_names = list_groups(&KeyValueStore::global(cx))
            .into_iter()
            .map(|g| g.name)
            .collect();
        let delegate = SaveWorkspaceGroupDelegate {
            keys,
            multi_workspace,
            existing_names,
            matches: Vec::new(),
            selected_index: 0,
            query: preset_name.clone().unwrap_or_default(),
        };
        let picker = cx.new(|cx| {
            let p = Picker::uniform_list(delegate, window, cx);
            if let Some(name) = preset_name.as_ref() {
                p.set_query(name.as_str(), window, cx);
            }
            p
        });
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        Self {
            picker,
            _subscription,
        }
    }
}

impl ModalView for SaveWorkspaceGroupModal {}
impl EventEmitter<DismissEvent> for SaveWorkspaceGroupModal {}
impl Focusable for SaveWorkspaceGroupModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for SaveWorkspaceGroupModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(34.))
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |this, cx| {
                    this.cancel(&Default::default(), window, cx);
                })
            }))
    }
}

struct SaveWorkspaceGroupDelegate {
    keys: Vec<SerializedProjectGroup>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    existing_names: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    query: String,
}

impl PickerDelegate for SaveWorkspaceGroupDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "save workspace group"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Name this group and press Enter".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let candidates = picker.read_with(cx, |picker, _| {
                picker
                    .delegate
                    .existing_names
                    .iter()
                    .enumerate()
                    .map(|(ix, name)| StringMatchCandidate::new(ix, name))
                    .collect::<Vec<_>>()
            });
            let Some(candidates) = candidates.log_err() else {
                return;
            };
            let matches: Vec<StringMatch> = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(ix, candidate)| StringMatch {
                        candidate_id: ix,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy_nucleo::match_strings(
                    &candidates,
                    &query,
                    Case::Smart,
                    LengthPenalty::On,
                    200,
                )
            };
            picker
                .update(cx, |picker, _| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let name = self.query.trim().to_string();
        if name.is_empty() {
            return;
        }
        let keys = self.keys.clone();
        let exists = self.existing_names.iter().any(|n| n == &name);
        let kvp = KeyValueStore::global(cx);
        let mw = self.multi_workspace.clone();
        let do_save = move |name: String,
                            key: SerializedProjectGroup,
                            kvp: KeyValueStore,
                            mw: WeakEntity<MultiWorkspace>,
                            cx: &mut gpui::AsyncApp| {
            let tag_name = name.clone();
            let save = cx.background_spawn(async move {
                save_group_async(kvp, name, key).await.log_err();
            });
            let mw_update = mw.update(cx, |mw, _cx| {
                mw.set_workspace_group_name(Some(tag_name));
            });
            (save, mw_update.ok())
        };
        if exists {
            let confirm = window.prompt(
                PromptLevel::Warning,
                &format!("Overwrite workspace group \"{name}\"?"),
                Some("This replaces the saved projects for that name."),
                &["Overwrite", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |picker, cx| {
                let Ok(choice) = confirm.await else { return };
                if choice != 0 {
                    return;
                }
                if let Some(key) = keys.into_iter().next() {
                    let (save_task, _) = do_save(name, key, kvp, mw, cx);
                    save_task.await;
                }
                picker
                    .update_in(cx, |_, _, cx| cx.emit(DismissEvent))
                    .log_err();
            })
            .detach();
        } else {
            cx.spawn_in(window, async move |picker, cx| {
                if let Some(key) = keys.into_iter().next() {
                    let (save_task, _) = do_save(name, key, kvp, mw, cx);
                    save_task.await;
                }
                picker
                    .update_in(cx, |_, _, cx| cx.emit(DismissEvent))
                    .log_err();
            })
            .detach();
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(ix)?;
        let is_exact = hit.string == self.query.trim();
        Some(
            ListItem::new(("save-wg", ix))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    hit.string.clone(),
                    hit.positions.clone(),
                ))
                .end_slot(
                    Label::new(if is_exact {
                        "will overwrite"
                    } else {
                        "existing"
                    })
                    .color(if is_exact {
                        Color::Warning
                    } else {
                        Color::Muted
                    })
                    .size(LabelSize::Small),
                ),
        )
    }
}

// ============================= open modal =============================

pub struct OpenWorkspaceGroupModal {
    picker: Entity<Picker<OpenWorkspaceGroupDelegate>>,
    _subscription: Subscription,
}

impl OpenWorkspaceGroupModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        _focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kvp = KeyValueStore::global(cx);
        let groups = list_groups(&kvp);
        let bindings = load_bindings(&kvp);
        let account_labels: BTreeMap<String, String> = Client::global(cx)
            .list_accounts()
            .into_iter()
            .map(|CollabAccount { id, label, .. }| (id, label))
            .collect();
        let delegate = OpenWorkspaceGroupDelegate {
            workspace,
            all_groups: groups,
            bindings,
            account_labels,
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        Self {
            picker,
            _subscription,
        }
    }
}

impl ModalView for OpenWorkspaceGroupModal {}
impl EventEmitter<DismissEvent> for OpenWorkspaceGroupModal {}
impl Focusable for OpenWorkspaceGroupModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for OpenWorkspaceGroupModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(40.))
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |this, cx| {
                    this.cancel(&Default::default(), window, cx);
                })
            }))
    }
}

struct OpenWorkspaceGroupDelegate {
    workspace: WeakEntity<Workspace>,
    all_groups: Vec<NamedWorkspaceGroup>,
    bindings: BTreeMap<String, String>,
    account_labels: BTreeMap<String, String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl OpenWorkspaceGroupDelegate {
    fn group_at(&self, ix: usize) -> Option<&NamedWorkspaceGroup> {
        let candidate_id = self.matches.get(ix)?.candidate_id;
        self.all_groups.get(candidate_id)
    }
}

impl PickerDelegate for OpenWorkspaceGroupDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "open workspace group"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Open a saved workspace group…".into()
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<gpui::SharedString> {
        if self.all_groups.is_empty() {
            Some("No saved workspace groups yet. Use \"Save Workspace Group\" first.".into())
        } else {
            Some("No matches".into())
        }
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |picker, cx| {
            let candidates = picker.read_with(cx, |picker, _| {
                picker
                    .delegate
                    .all_groups
                    .iter()
                    .enumerate()
                    .map(|(ix, g)| StringMatchCandidate::new(ix, &g.name))
                    .collect::<Vec<_>>()
            });
            let Some(candidates) = candidates.log_err() else {
                return;
            };
            let matches: Vec<StringMatch> = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(ix, candidate)| StringMatch {
                        candidate_id: ix,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy_nucleo::match_strings(
                    &candidates,
                    &query,
                    Case::Smart,
                    LengthPenalty::On,
                    200,
                )
            };
            picker
                .update(cx, |picker, _| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = cmp::min(
                        picker.delegate.selected_index,
                        picker.delegate.matches.len().saturating_sub(1),
                    );
                })
                .log_err();
        })
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(group) = self.group_at(self.selected_index).cloned() else {
            return;
        };
        if secondary {
            let name = group.name;
            let confirm = window.prompt(
                PromptLevel::Critical,
                &format!("Delete workspace group \"{name}\"?"),
                None,
                &["Delete", "Cancel"],
                cx,
            );
            let kvp = KeyValueStore::global(cx);
            cx.spawn_in(window, async move |picker, cx| {
                let Ok(choice) = confirm.await else { return };
                if choice != 0 {
                    return;
                }
                delete_group_async(kvp.clone(), name.clone())
                    .await
                    .log_err();
                picker
                    .update_in(cx, |picker, window, cx| {
                        picker.delegate.all_groups.retain(|g| g.name != name);
                        picker.update_matches(String::new(), window, cx);
                    })
                    .log_err();
            })
            .detach();
            return;
        }

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let key: ProjectGroupKey = group.key.into();
        let paths: Vec<std::path::PathBuf> = key.path_list().paths().to_vec();
        let group_name = group.name;
        let kvp = KeyValueStore::global(cx);
        let binding = binding_for(&kvp, &group_name);
        let client = Client::global(cx);
        let active_account_id = client.active_account_id();

        let task = workspace.update(cx, |workspace, cx| {
            workspace.open_workspace_for_paths(OpenMode::NewWindow, paths, window, cx)
        });
        cx.spawn_in(window, async move |_picker, cx| {
            let Ok(opened_workspace) = task.await else {
                return;
            };
            cx.update(|_, cx| {
                if let Some(mw) = opened_workspace
                    .read(cx)
                    .multi_workspace()
                    .and_then(|w| w.upgrade())
                {
                    mw.update(cx, |mw, _| {
                        mw.set_workspace_group_name(Some(group_name));
                    });
                }
            })
            .log_err();

            if let Some(account_id) = binding {
                if active_account_id.as_deref() != Some(account_id.as_str()) {
                    client.switch_account(account_id, cx).await.log_err();
                }
            }
        })
        .detach();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(ix)?;
        let group = self.all_groups.get(hit.candidate_id)?;
        let key: ProjectGroupKey = group.key.clone().into();
        let paths = key.path_list();
        let subtitle = paths
            .paths()
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" · ");

        let account_label = self.bindings.get(&group.name).map(|id| {
            self.account_labels
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.clone())
        });

        Some(
            ListItem::new(("open-wg", ix))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(HighlightedLabel::new(
                            hit.string.clone(),
                            hit.positions.clone(),
                        ))
                        .child(
                            Label::new(subtitle)
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                )
                .when_some(account_label, |this, label| {
                    this.end_slot(
                        Label::new(format!("@{label}"))
                            .color(Color::Accent)
                            .size(LabelSize::Small),
                    )
                }),
        )
    }
}

// ============================= rename modal =============================

pub struct RenameWorkspaceGroupModal {
    picker: Entity<Picker<RenameWorkspaceGroupDelegate>>,
    _subscription: Subscription,
}

impl RenameWorkspaceGroupModal {
    fn new(
        old_name: String,
        multi_workspace: WeakEntity<MultiWorkspace>,
        _focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let existing_names = list_groups(&KeyValueStore::global(cx))
            .into_iter()
            .map(|g| g.name)
            .filter(|n| n != &old_name)
            .collect();
        let delegate = RenameWorkspaceGroupDelegate {
            old_name: old_name.clone(),
            multi_workspace,
            existing_names,
            query: old_name.clone(),
        };
        let picker = cx.new(|cx| {
            let p = Picker::uniform_list(delegate, window, cx);
            p.set_query(old_name.as_str(), window, cx);
            p
        });
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        Self {
            picker,
            _subscription,
        }
    }
}

impl ModalView for RenameWorkspaceGroupModal {}
impl EventEmitter<DismissEvent> for RenameWorkspaceGroupModal {}
impl Focusable for RenameWorkspaceGroupModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for RenameWorkspaceGroupModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(34.))
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |this, cx| {
                    this.cancel(&Default::default(), window, cx);
                })
            }))
    }
}

struct RenameWorkspaceGroupDelegate {
    old_name: String,
    multi_workspace: WeakEntity<MultiWorkspace>,
    existing_names: Vec<String>,
    query: String,
}

impl PickerDelegate for RenameWorkspaceGroupDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "rename workspace group"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "New name for workspace group".into()
    }

    fn match_count(&self) -> usize {
        0
    }

    fn selected_index(&self) -> usize {
        0
    }

    fn set_selected_index(&mut self, _: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query;
        Task::ready(())
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<gpui::SharedString> {
        let new_name = self.query.trim();
        if new_name.is_empty() {
            Some("Type a new name and press Enter".into())
        } else if new_name == self.old_name {
            Some(format!("Rename \"{}\" — press Enter to confirm", self.old_name).into())
        } else if self.existing_names.iter().any(|n| n == new_name) {
            Some(format!("\"{new_name}\" is already taken").into())
        } else {
            Some(format!("Rename \"{}\" → \"{new_name}\"", self.old_name).into())
        }
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let new_name = self.query.trim().to_string();
        if new_name.is_empty() || new_name == self.old_name {
            cx.emit(DismissEvent);
            return;
        }
        if self.existing_names.iter().any(|n| n == &new_name) {
            return;
        }
        let kvp = KeyValueStore::global(cx);
        let old_name = self.old_name.clone();
        let mw = self.multi_workspace.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let ok = rename_group_async(kvp, old_name, new_name.clone())
                .await
                .log_err()
                .unwrap_or(false);
            if ok {
                mw.update(cx, |mw, _| {
                    mw.set_workspace_group_name(Some(new_name));
                })
                .log_err();
            }
            picker
                .update_in(cx, |_, _, cx| cx.emit(DismissEvent))
                .log_err();
        })
        .detach();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        _: usize,
        _: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        None
    }
}
