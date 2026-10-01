//! Outline for a notebook.
//!
//! The editor's own outline (`crates/outline`) is built around a single `Editor` over a
//! singleton buffer: it calls `as_singleton()` and bails when that is `None`. A notebook
//! has neither, so it gets its own.
//!
//! What it lists is also different. In a plain file an outline is functions and classes;
//! in a notebook the unit of structure is the cell, and markdown headings are how people
//! actually organize one. So this lists cells, labelled by their heading or their first
//! definition, and selecting one jumps to it.

use std::sync::Arc;

use fuzzy::StringMatchCandidate;
use gpui::{DismissEvent, Entity, EventEmitter, Focusable, Task, WeakEntity, prelude::*};
use nbformat::v4::{CellId, CellType};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};

use super::NotebookEditor;

/// One cell, as the outline shows it.
#[derive(Clone)]
pub struct OutlineEntry {
    pub cell_id: CellId,
    pub cell_type: CellType,
    pub index: usize,
    pub label: String,
}

impl OutlineEntry {
    fn icon(&self) -> IconName {
        match self.cell_type {
            CellType::Markdown => IconName::Book,
            CellType::Code => IconName::Code,
            CellType::Raw => IconName::File,
        }
    }
}

pub struct NotebookOutline {
    picker: Entity<Picker<NotebookOutlineDelegate>>,
}

impl NotebookOutline {
    pub fn new(
        notebook: WeakEntity<NotebookEditor>,
        entries: Vec<OutlineEntry>,
        selected_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = NotebookOutlineDelegate {
            // The workspace's modal is this wrapper, not the picker inside it, so
            // dismissal has to be emitted through here or the popup stays open after a
            // selection.
            outline: cx.entity().downgrade(),
            notebook,
            entries: entries.clone(),
            matches: (0..entries.len())
                .map(|index| fuzzy::StringMatch {
                    candidate_id: index,
                    score: 0.0,
                    positions: Vec::new(),
                    string: entries[index].label.clone(),
                })
                .collect(),
            // Open on the cell the user is already on, so the outline is a map of where
            // they are rather than a jump to the top.
            selected_index,
        };

        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).max_height(rems(20.)));

        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for NotebookOutline {}

impl workspace::ModalView for NotebookOutline {}

impl Focusable for NotebookOutline {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for NotebookOutline {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

pub struct NotebookOutlineDelegate {
    outline: WeakEntity<NotebookOutline>,
    notebook: WeakEntity<NotebookEditor>,
    entries: Vec<OutlineEntry>,
    matches: Vec<fuzzy::StringMatch>,
    selected_index: usize,
}

impl NotebookOutlineDelegate {
    fn dismiss(&self, cx: &mut Context<Picker<Self>>) {
        self.outline
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
        cx.emit(DismissEvent);
    }
}

impl PickerDelegate for NotebookOutlineDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "notebook outline"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Go to cell...".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates: Vec<StringMatchCandidate> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| StringMatchCandidate::new(index, &entry.label))
            .collect();

        if query.is_empty() {
            self.matches = candidates
                .iter()
                .map(|candidate| fuzzy::StringMatch {
                    candidate_id: candidate.id,
                    score: 0.0,
                    positions: Vec::new(),
                    string: candidate.string.clone(),
                })
                .collect();
            self.selected_index = self.selected_index.min(self.matches.len().saturating_sub(1));
            cx.notify();
            return Task::ready(());
        }

        let executor = cx.background_executor().clone();
        cx.spawn(async move |picker, cx| {
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                true,
                true,
                100,
                &Default::default(),
                executor,
            )
            .await;

            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected_index)
            .and_then(|m| self.entries.get(m.candidate_id))
        else {
            return;
        };

        let cell_id = entry.cell_id.clone();
        self.notebook
            .update(cx, |notebook, cx| {
                notebook.select_cell_by_id_and_reveal(&cell_id, window, cx);
            })
            .ok();

        self.dismiss(cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.dismiss(cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let string_match = self.matches.get(ix)?;
        let entry = self.entries.get(string_match.candidate_id)?;

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(entry.icon()).size(IconSize::Small))
                .child(HighlightedLabel::new(
                    entry.label.clone(),
                    string_match.positions.clone(),
                ))
                .end_slot(
                    Label::new(format!("{}", entry.index + 1))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
    }
}
