//! Converts Lathe's thread state into protocol entries and prompts.
//!
//! Each `AgentThreadEntry` maps to exactly one protocol entry, because the
//! phone applies `update_entry` and `remove_entries` by index. Entries the
//! protocol has no kind for are mapped to the closest one that renders.

use crate::protocol::{
    self, Entry, FileDiff, PermissionOption, PermissionOptionKind, ToolCallStatus,
};
use acp_thread::{
    AcpThread, AgentThreadEntry, AssistantMessageChunk, ElicitationStatus, PermissionOptions,
    PermissionRequest, ToolCall, ToolCallContent,
};
use agent_client_protocol::v2::schema as acp;
use gpui::App;

/// Diffs bigger than this are left out; the phone shows the tool call without
/// them rather than a frame the relay would reject.
const MAX_DIFF_BYTES: usize = 32 * 1024;
/// Files bigger than this aren't diffed at all.
const MAX_DIFF_INPUT_BYTES: usize = 512 * 1024;
/// A single entry never takes more than this, so any one of them fits in a
/// frame with room left for the rest of a page.
pub const MAX_ENTRY_BYTES: usize = 48 * 1024;
const TRUNCATED_NOTE: &str = "\n\n_Truncated. The rest is in Lathe._";

pub fn entry(thread: &AcpThread, entry: &AgentThreadEntry, cx: &App) -> Entry {
    fit(full_entry(thread, entry, cx))
}

pub fn entry_size(entry: &Entry) -> usize {
    serde_json::to_string(entry).map_or(usize::MAX, |json| json.len())
}

/// Shrinks an entry to `MAX_ENTRY_BYTES`: diffs go first, then long text is
/// cut with a note pointing back to Lathe.
fn fit(mut entry: Entry) -> Entry {
    while entry_size(&entry) > MAX_ENTRY_BYTES {
        match &mut entry {
            Entry::ToolCall {
                diff: Some(diffs), ..
            } if !diffs.is_empty() => {
                diffs.pop();
            }
            _ => break,
        }
    }
    if let Entry::ToolCall { diff, .. } = &mut entry
        && diff.as_ref().is_some_and(|diffs| diffs.is_empty())
    {
        *diff = None;
    }
    if entry_size(&entry) <= MAX_ENTRY_BYTES {
        return entry;
    }
    match &mut entry {
        // JSON escaping can grow text, so the cut leaves generous room.
        Entry::UserMessage { text: content }
        | Entry::AssistantMessage { markdown: content }
        | Entry::Thought { markdown: content }
        | Entry::ToolCall { title: content, .. } => {
            truncate_with_note(content, MAX_ENTRY_BYTES / 2)
        }
        Entry::Plan { items } => {
            for item in items.iter_mut() {
                truncate_with_note(&mut item.text, 512);
            }
            while items.len() > 1
                && entry_size(&Entry::Plan {
                    items: items.clone(),
                }) > MAX_ENTRY_BYTES
            {
                items.pop();
            }
        }
    }
    entry
}

fn truncate_with_note(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes.saturating_sub(TRUNCATED_NOTE.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(TRUNCATED_NOTE);
}

fn full_entry(thread: &AcpThread, entry: &AgentThreadEntry, cx: &App) -> Entry {
    match entry {
        AgentThreadEntry::UserMessage(message) => Entry::UserMessage {
            text: message.content.to_markdown(cx),
        },
        AgentThreadEntry::AssistantMessage(message) => {
            let (mut text, mut thoughts) = (Vec::new(), Vec::new());
            for chunk in &message.chunks {
                match chunk {
                    AssistantMessageChunk::Message { block, .. } => {
                        text.push(block.to_markdown(cx))
                    }
                    AssistantMessageChunk::Thought { block, .. } => {
                        thoughts.push(block.to_markdown(cx))
                    }
                }
            }
            // One Lathe entry can hold both thinking and the reply. The reply is
            // what the phone needs, so thinking only shows on its own.
            if text.iter().any(|text| !text.is_empty()) {
                Entry::AssistantMessage {
                    markdown: text.join("\n\n"),
                }
            } else {
                Entry::Thought {
                    markdown: thoughts.join("\n\n"),
                }
            }
        }
        AgentThreadEntry::SessionMessage(message) => Entry::AssistantMessage {
            markdown: format!("_Session message_\n\n{}", message.content.to_markdown(cx)),
        },
        AgentThreadEntry::ToolCall(call) => tool_call(call, cx),
        AgentThreadEntry::Elicitation(id) => {
            let message = thread
                .elicitation(id)
                .map(|(_, elicitation)| elicitation.request.message.clone())
                .unwrap_or_default();
            Entry::AssistantMessage {
                markdown: format!("_{}_", message.trim()),
            }
        }
        AgentThreadEntry::ContextCompaction(compaction) => Entry::Thought {
            markdown: if compaction.is_in_progress() {
                "Compacting the conversation…".into()
            } else {
                format!(
                    "Compacted the conversation.\n\n{}",
                    compaction.summary.to_markdown(cx)
                )
            },
        },
    }
}

fn tool_call(call: &ToolCall, cx: &App) -> Entry {
    let diffs = diffs(call, cx);
    Entry::ToolCall {
        tool_call_id: call.id.0.to_string(),
        title: call.label.read(cx).source().to_string(),
        tool_kind: tool_kind(call.kind()),
        status: match call.status() {
            acp_thread::ToolCallStatus::Pending
            | acp_thread::ToolCallStatus::WaitingForConfirmation
            | acp_thread::ToolCallStatus::Paused => ToolCallStatus::Pending,
            acp_thread::ToolCallStatus::InProgress => ToolCallStatus::Running,
            acp_thread::ToolCallStatus::Completed => ToolCallStatus::Completed,
            acp_thread::ToolCallStatus::Failed => ToolCallStatus::Failed,
            acp_thread::ToolCallStatus::Rejected | acp_thread::ToolCallStatus::Canceled => {
                ToolCallStatus::Rejected
            }
        },
        diff: (!diffs.is_empty()).then_some(diffs),
    }
}

pub fn tool_kind(kind: &acp::ToolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".into())
}

fn diffs(call: &ToolCall, cx: &App) -> Vec<FileDiff> {
    call.content()
        .iter()
        .filter_map(|content| match content {
            ToolCallContent::Diff(diff) | ToolCallContent::LegacyDiff { diff, .. } => {
                let diff = diff.read(cx);
                let path = diff.file_path(cx)?;
                let new_text = diff.new_text(cx);
                file_diff(path, diff.base_text(), &new_text)
            }
            ToolCallContent::DiffPatch { source, .. } => {
                let text = source.patch.as_ref()?.text.clone();
                let path = source
                    .changes
                    .first()
                    .map(acp_thread::diff_change_label)
                    .unwrap_or_else(|| "patch".into());
                (text.len() <= MAX_DIFF_BYTES).then_some(FileDiff {
                    path,
                    unified_diff: text,
                })
            }
            _ => None,
        })
        .collect()
}

fn file_diff(path: String, old_text: &str, new_text: &str) -> Option<FileDiff> {
    if old_text.len() > MAX_DIFF_INPUT_BYTES || new_text.len() > MAX_DIFF_INPUT_BYTES {
        return None;
    }
    let unified_diff = language::unified_diff(old_text, new_text);
    (!unified_diff.is_empty() && unified_diff.len() <= MAX_DIFF_BYTES)
        .then_some(FileDiff { path, unified_diff })
}

pub fn request_id(request: &PermissionRequest) -> String {
    serde_json::to_value(request.id)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn tool_authorization(
    thread_id: &str,
    thread: &AcpThread,
    request: &PermissionRequest,
    cx: &App,
) -> protocol::DesktopMessage {
    let call = request
        .legacy_tool_call_id()
        .and_then(|id| thread.tool_call(id))
        .map(|(_, call)| call);
    let (tool_title, tool_kind_name, diff_preview) = match (call, request.generic_request()) {
        (Some(call), _) => {
            let diffs = diffs(call, cx);
            (
                call.label.read(cx).source().to_string(),
                tool_kind(call.kind()),
                (!diffs.is_empty()).then_some(diffs),
            )
        }
        (None, Some(generic)) => (generic.title.clone(), "other".into(), None),
        (None, None) => ("Tool call".into(), "other".into(), None),
    };
    protocol::DesktopMessage::ToolAuthorization {
        thread_id: thread_id.to_string(),
        request_id: request_id(request),
        tool_title,
        tool_kind: tool_kind_name,
        options: permission_options(request),
        diff_preview,
    }
}

pub fn permission_options(request: &PermissionRequest) -> Vec<PermissionOption> {
    let options: Vec<&acp::PermissionOption> =
        match (request.legacy_options(), request.generic_request()) {
            (Some(PermissionOptions::Flat(options)), _) => options.iter().collect(),
            (
                Some(
                    PermissionOptions::Dropdown(choices)
                    | PermissionOptions::DropdownWithPatterns { choices, .. },
                ),
                _,
            ) => choices
                .iter()
                .flat_map(|choice| [&choice.allow, &choice.deny])
                .collect(),
            (None, Some(generic)) => generic.options.iter().collect(),
            (None, None) => Vec::new(),
        };
    options
        .into_iter()
        .filter_map(|option| {
            Some(PermissionOption {
                option_id: option.option_id.0.to_string(),
                label: option.name.clone(),
                kind: match option.kind {
                    acp::PermissionOptionKind::AllowOnce => PermissionOptionKind::AllowOnce,
                    acp::PermissionOptionKind::AllowAlways => PermissionOptionKind::AllowAlways,
                    acp::PermissionOptionKind::RejectOnce => PermissionOptionKind::RejectOnce,
                    acp::PermissionOptionKind::RejectAlways => PermissionOptionKind::RejectAlways,
                    _ => return None,
                },
            })
        })
        .collect()
}

/// Pending form elicitations; URL elicitations need a browser, so they stay on
/// the desktop.
pub fn pending_elicitations(thread_id: &str, thread: &AcpThread) -> Vec<protocol::DesktopMessage> {
    thread
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            AgentThreadEntry::Elicitation(id) => thread.elicitation(id),
            _ => None,
        })
        .filter(|(_, elicitation)| matches!(elicitation.status, ElicitationStatus::Pending { .. }))
        .filter_map(|(_, elicitation)| elicitation_prompt(thread_id, elicitation))
        .collect()
}

pub fn elicitation_prompt(
    thread_id: &str,
    elicitation: &acp_thread::Elicitation,
) -> Option<protocol::DesktopMessage> {
    let acp::ElicitationMode::Form(form) = &elicitation.request.mode else {
        return None;
    };
    Some(protocol::DesktopMessage::Elicitation {
        thread_id: thread_id.to_string(),
        elicitation_id: elicitation.id.0.to_string(),
        message: elicitation.request.message.clone(),
        schema: serde_json::to_value(&form.requested_schema).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(bytes: usize) -> FileDiff {
        FileDiff {
            path: "src/big.rs".into(),
            unified_diff: "+".repeat(bytes),
        }
    }

    #[test]
    fn leaves_small_entries_alone() {
        let entry = Entry::AssistantMessage {
            markdown: "Done.".into(),
        };
        assert_eq!(fit(entry.clone()), entry);
    }

    #[test]
    fn drops_diffs_before_touching_text() {
        let entry = fit(Entry::ToolCall {
            tool_call_id: "1".into(),
            title: "Edit".into(),
            tool_kind: "edit".into(),
            status: ToolCallStatus::Completed,
            diff: Some(vec![diff(20_000), diff(20_000), diff(20_000)]),
        });
        assert!(entry_size(&entry) <= MAX_ENTRY_BYTES);
        let Entry::ToolCall { title, diff, .. } = entry else {
            panic!()
        };
        assert_eq!(title, "Edit");
        assert_eq!(diff.map(|diffs| diffs.len()), Some(2));
    }

    #[test]
    fn truncates_long_text_with_a_note() {
        let entry = fit(Entry::UserMessage {
            text: "é".repeat(100_000),
        });
        assert!(entry_size(&entry) <= MAX_ENTRY_BYTES);
        let Entry::UserMessage { text } = entry else {
            panic!()
        };
        assert!(text.ends_with(TRUNCATED_NOTE));
    }

    #[test]
    fn caps_text_that_escaping_inflates() {
        // Each control character becomes six bytes of JSON.
        let entry = fit(Entry::AssistantMessage {
            markdown: "\u{1}".repeat(40_000),
        });
        assert!(
            entry_size(&entry) <= MAX_ENTRY_BYTES * 3,
            "{}",
            entry_size(&entry)
        );
    }
}
