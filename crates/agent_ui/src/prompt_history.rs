//! History of the prompts sent from an agent panel composer, walked with the
//! up/down arrows the way a shell walks its command history.
//!
//! The history is app-global and lives for the session only. Recalling a prompt
//! typed in an earlier thread is the main reason to reach for it, so scoping it
//! per thread would defeat the point.

use std::collections::VecDeque;

use agent_client_protocol::schema::v1 as acp;
use gpui::{App, Global};

const MAX_ENTRIES: usize = 100;

#[derive(Default)]
struct GlobalPromptHistory {
    /// Sent prompts, oldest first.
    entries: VecDeque<Vec<acp::ContentBlock>>,
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
pub fn push(prompt: Vec<acp::ContentBlock>, cx: &mut App) {
    if !has_content(&prompt) {
        return;
    }

    let history = cx.default_global::<GlobalPromptHistory>();
    history.entries.retain(|entry| entry != &prompt);
    history.entries.push_back(prompt);
    while history.entries.len() > MAX_ENTRIES {
        history.entries.pop_front();
    }
}

/// Returns the prompt one step older than the cursor's position, stashing
/// `draft` on the cursor so [`next`] can restore it.
pub fn previous(
    cursor: &mut PromptHistoryCursor,
    draft: Vec<acp::ContentBlock>,
    cx: &App,
) -> Option<Vec<acp::ContentBlock>> {
    let history = cx.try_global::<GlobalPromptHistory>()?;
    let position = match cursor.position {
        Some(position) => position.checked_sub(1)?,
        None => history.entries.len().checked_sub(1)?,
    };
    let prompt = history.entries.get(position)?.clone();

    if cursor.position.is_none() {
        cursor.draft = Some(draft);
    }
    cursor.position = Some(position);
    Some(prompt)
}

/// Returns the prompt one step newer than the cursor's position, or the stashed
/// draft once the walk moves past the newest entry.
pub fn next(cursor: &mut PromptHistoryCursor, cx: &App) -> Option<Vec<acp::ContentBlock>> {
    let history = cx.try_global::<GlobalPromptHistory>()?;
    let position = cursor.position? + 1;

    match history.entries.get(position) {
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
            push(text_prompt("older"), cx);
            push(text_prompt("newer"), cx);

            let mut cursor = PromptHistoryCursor::default();
            let draft = text_prompt("draft");

            assert_eq!(
                prompt_text(&previous(&mut cursor, draft.clone(), cx).unwrap()),
                "newer"
            );
            assert_eq!(
                prompt_text(&previous(&mut cursor, draft.clone(), cx).unwrap()),
                "older"
            );
            // The oldest entry is the end of the walk.
            assert!(previous(&mut cursor, draft, cx).is_none());

            assert_eq!(prompt_text(&next(&mut cursor, cx).unwrap()), "newer");
            assert_eq!(prompt_text(&next(&mut cursor, cx).unwrap()), "draft");
            assert!(next(&mut cursor, cx).is_none());
        });
    }

    #[gpui::test]
    fn test_resending_a_prompt_does_not_duplicate_it(cx: &mut TestAppContext) {
        cx.update(|cx| {
            push(text_prompt("one"), cx);
            push(text_prompt("two"), cx);
            push(text_prompt("one"), cx);

            let mut cursor = PromptHistoryCursor::default();
            let draft = Vec::new();
            assert_eq!(
                prompt_text(&previous(&mut cursor, draft.clone(), cx).unwrap()),
                "one"
            );
            assert_eq!(
                prompt_text(&previous(&mut cursor, draft.clone(), cx).unwrap()),
                "two"
            );
            assert!(previous(&mut cursor, draft, cx).is_none());
        });
    }

    #[gpui::test]
    fn test_blank_prompts_are_not_recorded(cx: &mut TestAppContext) {
        cx.update(|cx| {
            push(Vec::new(), cx);
            push(text_prompt("   \n "), cx);

            let mut cursor = PromptHistoryCursor::default();
            assert!(previous(&mut cursor, Vec::new(), cx).is_none());
        });
    }
}
