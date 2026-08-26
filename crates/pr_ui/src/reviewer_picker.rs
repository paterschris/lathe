//! Multi-select picker for requesting reviewers on a pull request.
//!
//! Hosts identify accounts differently (Bitbucket by uuid, GitHub by login,
//! GitLab by username), so candidates carry an opaque `handle` that is passed
//! back untouched rather than reconstructed from what is displayed.

use std::sync::Arc;

use collections::HashSet;
use git::{GitHostingProvider, ParsedGitRemote, ReviewerCandidate};
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, WeakEntity,
};
use picker::{Picker, PickerDelegate, PickerEditorPosition};
use ui::{Checkbox, ListItem, ListItemSpacing, ToggleState, prelude::*};
use workspace::ModalView;

use crate::pull_request_view::PullRequestView;

pub struct ReviewerPicker {
    picker: Entity<Picker<ReviewerPickerDelegate>>,
    width: ui::Rems,
}

impl ReviewerPicker {
    pub fn new(
        provider: Arc<dyn GitHostingProvider + Send + Sync>,
        remote: ParsedGitRemote,
        view: WeakEntity<PullRequestView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = ReviewerPickerDelegate {
            picker_entity: cx.entity().downgrade(),
            view,
            candidates: Vec::new(),
            filtered: Vec::new(),
            chosen: HashSet::default(),
            selected_index: 0,
            status: LoadStatus::Loading,
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .max_height(ui::rems(22.))
                .show_scrollbar(true)
        });
        let this = Self {
            picker: picker.clone(),
            width: ui::rems(34.),
        };
        Self::load(provider, remote, picker, cx);
        this
    }

    /// Fetches the candidate list once when the picker opens.
    fn load(
        provider: Arc<dyn GitHostingProvider + Send + Sync>,
        remote: ParsedGitRemote,
        picker: Entity<Picker<ReviewerPickerDelegate>>,
        cx: &mut Context<Self>,
    ) {
        let http_client = cx.http_client();
        let host = provider.base_url().host_str().map(|host| host.to_string());
        cx.spawn(async move |_, cx| {
            let auth = match host.as_deref() {
                Some(host) => git::git_host_credentials::auth_for_host(cx, host)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let result = provider
                .list_reviewer_candidates(&remote, auth, http_client)
                .await;
            picker
                .update(cx, |picker, cx| {
                    match result {
                        Ok(candidates) if candidates.is_empty() => {
                            picker.delegate.status = LoadStatus::Empty;
                        }
                        Ok(candidates) => {
                            picker.delegate.filtered = (0..candidates.len()).collect();
                            picker.delegate.candidates = candidates;
                            picker.delegate.status = LoadStatus::Loaded;
                        }
                        Err(error) => {
                            picker.delegate.status =
                                LoadStatus::Failed(format!("{error:#}").into());
                        }
                    }
                    cx.notify();
                });
        })
        .detach();
    }
}

impl EventEmitter<DismissEvent> for ReviewerPicker {}

impl Focusable for ReviewerPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ReviewerPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("ReviewerPicker")
            .w(self.width)
            .child(self.picker.clone())
    }
}

impl ModalView for ReviewerPicker {}

#[derive(Clone)]
enum LoadStatus {
    Loading,
    Loaded,
    /// The host answered, but nobody is listed. Not an error: a personal
    /// repository legitimately has no other members.
    Empty,
    Failed(SharedString),
}

pub struct ReviewerPickerDelegate {
    picker_entity: WeakEntity<ReviewerPicker>,
    view: WeakEntity<PullRequestView>,
    candidates: Vec<ReviewerCandidate>,
    /// Indices into `candidates` surviving the current filter.
    filtered: Vec<usize>,
    /// Indices into `candidates` the user has ticked.
    chosen: HashSet<usize>,
    selected_index: usize,
    status: LoadStatus,
}

impl PickerDelegate for ReviewerPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "reviewer picker"
    }

    fn match_count(&self) -> usize {
        match self.status {
            LoadStatus::Loaded => self.filtered.len(),
            // One row carries the loading, empty or error message.
            _ => 1,
        }
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix.min(self.filtered.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search people, then press tab to tick each one…".into()
    }

    fn editor_position(&self) -> PickerEditorPosition {
        PickerEditorPosition::Start
    }

    fn supports_multi_select(&self) -> bool {
        true
    }

    fn is_item_selected(&self, ix: usize) -> bool {
        self.filtered
            .get(ix)
            .is_some_and(|candidate| self.chosen.contains(candidate))
    }

    fn toggle_item_selected(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(&candidate) = self.filtered.get(ix) else {
            return;
        };
        if !self.chosen.remove(&candidate) {
            self.chosen.insert(candidate);
        }
        cx.notify();
    }

    fn selected_item_count(&self) -> usize {
        self.chosen.len()
    }

    fn clear_selection(&mut self, cx: &mut Context<Picker<Self>>) {
        self.chosen.clear();
        cx.notify();
    }

    fn confirm_multi(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.request(window, cx);
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.filtered = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                query.is_empty() || candidate.match_text().to_lowercase().contains(&query)
            })
            .map(|(ix, _)| ix)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.filtered.len().saturating_sub(1));
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        // With nothing ticked, confirming acts on the highlighted row, so a
        // single reviewer needs no ticking at all.
        if self.chosen.is_empty()
            && let Some(&candidate) = self.filtered.get(self.selected_index)
        {
            self.chosen.insert(candidate);
        }
        self.request(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker_entity
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match &self.status {
            LoadStatus::Loading => Some(
                ListItem::new(ix).inset(true).child(
                    Label::new("Loading people…")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                ),
            ),
            LoadStatus::Empty => Some(
                ListItem::new(ix).inset(true).child(
                    Label::new("This repository lists no other members")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                ),
            ),
            LoadStatus::Failed(reason) => Some(
                ListItem::new(ix).inset(true).child(
                    Label::new(reason.clone())
                        .color(Color::Error)
                        .size(LabelSize::Small),
                ),
            ),
            LoadStatus::Loaded => {
                let candidate = self.candidates.get(*self.filtered.get(ix)?)?;
                let ticked = self.is_item_selected(ix);
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .start_slot(
                            Checkbox::new(("reviewer", ix), ToggleState::from(ticked))
                                .disabled(true),
                        )
                        .child(
                            v_flex()
                                .child(Label::new(candidate.primary_label()))
                                // Only worth a second line when it says
                                // something the name does not.
                                .when(candidate.display_name.is_some(), |this| {
                                    this.child(
                                        Label::new(candidate.login.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                }),
                        ),
                )
            }
        }
    }
}

impl ReviewerPickerDelegate {
    /// Hands the ticked handles back to the pull request view and closes.
    fn request(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let handles: Vec<SharedString> = self
            .chosen
            .iter()
            .filter_map(|&ix| self.candidates.get(ix))
            .map(|candidate| candidate.handle.clone())
            .collect();
        if handles.is_empty() {
            return;
        }
        self.view
            .update(cx, |view, cx| view.request_reviewers(handles, cx))
            .ok();
        self.picker_entity
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
    }
}
