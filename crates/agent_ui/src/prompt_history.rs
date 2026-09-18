//! History of the prompts sent from an agent panel composer, walked with the
//! up/down arrows the way a shell walks its command history.
//!
//! The history is scoped to a thread in a workspace instance and lives for the
//! session only.

use std::collections::VecDeque;

use agent_client_protocol::schema::v1 as acp;
use collections::HashMap;
use gpui::{App, EntityId, Global};

use crate::thread_metadata_store::ThreadId;

const MAX_ENTRIES: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PromptHistoryScope {
    workspace_id: EntityId,
    thread_id: ThreadId,
}

impl PromptHistoryScope {
    pub(crate) fn new(workspace_id: EntityId, thread_id: ThreadId) -> Self {
        Self {
            workspace_id,
            thread_id,
        }
    }
}

#[derive(Default)]
struct GlobalPromptHistory {
    by_scope: HashMap<PromptHistoryScope, VecDeque<Vec<acp::ContentBlock>>>,
}

impl Global for GlobalPromptHistory {}

/// Where a single message editor currently sits within the global history.
///
/// `position` can point at a different prompt than it did when it was set, if
/// enough prompts were sent while the editor was mid-walk that the entry it
/// referred to was dropped from the front of the history.
#[derive(Default)]
pub struct PromptHistoryCursor {
    position: Option<usize>,
    /// What the editor held before the walk started, restored when the user
    /// walks forward past the newest entry.
    draft: Option<Vec<acp::ContentBlock>>,
}

/// Records `prompt` as the newest entry, dropping any earlier copy of it so
/// repeating a prompt doesn't leave duplicates to walk past.
pub fn push(scope: PromptHistoryScope, prompt: Vec<acp::ContentBlock>, cx: &mut App) {
    if !has_content(&prompt) {
        return;
    }

    let history = cx.default_global::<GlobalPromptHistory>();
    let entries = history.by_scope.entry(scope).or_default();
    entries.retain(|entry| entry != &prompt);
    entries.push_back(prompt);
    while entries.len() > MAX_ENTRIES {
        entries.pop_front();
    }
}

/// Returns the prompt one step older than the cursor's position, stashing
/// `draft` on the cursor so [`next`] can restore it.
pub fn previous(
    scope: PromptHistoryScope,
    cursor: &mut PromptHistoryCursor,
    draft: Vec<acp::ContentBlock>,
    cx: &App,
) -> Option<Vec<acp::ContentBlock>> {
    let history = cx.try_global::<GlobalPromptHistory>()?;
    let entries = history.by_scope.get(&scope)?;
    let position = match cursor.position {
        Some(position) => position.checked_sub(1)?,
        None => entries.len().checked_sub(1)?,
    };
    let prompt = entries.get(position)?.clone();

    if cursor.position.is_none() {
        cursor.draft = Some(draft);
    }
    cursor.position = Some(position);
    Some(prompt)
}

/// Returns the prompt one step newer than the cursor's position, or the stashed
/// draft once the walk moves past the newest entry.
pub fn next(
    scope: PromptHistoryScope,
    cursor: &mut PromptHistoryCursor,
    cx: &App,
) -> Option<Vec<acp::ContentBlock>> {
    let history = cx.try_global::<GlobalPromptHistory>()?;
    let entries = history.by_scope.get(&scope)?;
    let position = cursor.position? + 1;

    match entries.get(position) {
        Some(prompt) => {
            let prompt = prompt.clone();
            cursor.position = Some(position);
            Some(prompt)
        }
        None => {
            cursor.position = None;
            Some(cursor.draft.take().unwrap_or_default())
        }
    }
}

fn has_content(prompt: &[acp::ContentBlock]) -> bool {
    prompt.iter().any(|block| match block {
        acp::ContentBlock::Text(text) => !text.text.trim().is_empty(),
        _ => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn text_prompt(text: &str) -> Vec<acp::ContentBlock> {
        vec![acp::ContentBlock::Text(acp::TextContent::new(text))]
    }

    fn prompt_text(prompt: &[acp::ContentBlock]) -> String {
        prompt
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[gpui::test]
    fn test_walking_history_preserves_the_draft(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let scope = PromptHistoryScope::new(EntityId::from(1), ThreadId::new());
            push(scope, text_prompt("older"), cx);
            push(scope, text_prompt("newer"), cx);

            let mut cursor = PromptHistoryCursor::default();
            let draft = text_prompt("draft");

            assert_eq!(
                prompt_text(&previous(scope, &mut cursor, draft.clone(), cx).unwrap()),
                "newer"
            );
            assert_eq!(
                prompt_text(&previous(scope, &mut cursor, draft.clone(), cx).unwrap()),
                "older"
            );
            // The oldest entry is the end of the walk.
            assert!(previous(scope, &mut cursor, draft, cx).is_none());

            assert_eq!(prompt_text(&next(scope, &mut cursor, cx).unwrap()), "newer");
            assert_eq!(prompt_text(&next(scope, &mut cursor, cx).unwrap()), "draft");
            assert!(next(scope, &mut cursor, cx).is_none());
        });
    }

    #[gpui::test]
    fn test_resending_a_prompt_does_not_duplicate_it(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let scope = PromptHistoryScope::new(EntityId::from(1), ThreadId::new());
            push(scope, text_prompt("one"), cx);
            push(scope, text_prompt("two"), cx);
            push(scope, text_prompt("one"), cx);

            let mut cursor = PromptHistoryCursor::default();
            let draft = Vec::new();
            assert_eq!(
                prompt_text(&previous(scope, &mut cursor, draft.clone(), cx).unwrap()),
                "one"
            );
            assert_eq!(
                prompt_text(&previous(scope, &mut cursor, draft.clone(), cx).unwrap()),
                "two"
            );
            assert!(previous(scope, &mut cursor, draft, cx).is_none());
        });
    }

    #[gpui::test]
    fn test_blank_prompts_are_not_recorded(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let scope = PromptHistoryScope::new(EntityId::from(1), ThreadId::new());
            push(scope, Vec::new(), cx);
            push(scope, text_prompt("   \n "), cx);

            let mut cursor = PromptHistoryCursor::default();
            assert!(previous(scope, &mut cursor, Vec::new(), cx).is_none());
        });
    }

    #[gpui::test]
    fn test_history_is_scoped_to_workspace_and_thread(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let thread = ThreadId::new();
            let first_workspace = PromptHistoryScope::new(EntityId::from(1), thread);
            let second_workspace = PromptHistoryScope::new(EntityId::from(2), thread);
            let second_thread = PromptHistoryScope::new(EntityId::from(1), ThreadId::new());
            push(first_workspace, text_prompt("first workspace"), cx);
            push(second_workspace, text_prompt("second workspace"), cx);
            push(second_thread, text_prompt("second thread"), cx);

            let mut first_cursor = PromptHistoryCursor::default();
            assert_eq!(
                prompt_text(
                    &previous(first_workspace, &mut first_cursor, Vec::new(), cx)
                        .expect("first workspace prompt should be available"),
                ),
                "first workspace"
            );

            let mut second_cursor = PromptHistoryCursor::default();
            assert_eq!(
                prompt_text(
                    &previous(second_workspace, &mut second_cursor, Vec::new(), cx)
                        .expect("second workspace prompt should be available"),
                ),
                "second workspace"
            );

            let mut second_thread_cursor = PromptHistoryCursor::default();
            assert_eq!(
                prompt_text(
                    &previous(second_thread, &mut second_thread_cursor, Vec::new(), cx)
                        .expect("second thread prompt should be available"),
                ),
                "second thread"
            );
        });
    }
}
