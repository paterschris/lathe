use crate::Terminal;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractivePromptKind {
    GeneralInput,
    Confirmation,
    ChooseOption,
}

impl InteractivePromptKind {
    pub fn tooltip_text(&self) -> &'static str {
        match self {
            Self::GeneralInput => "Terminal awaiting input",
            Self::Confirmation => "Terminal awaiting confirmation",
            Self::ChooseOption => "Terminal awaiting selection",
        }
    }
}

impl Terminal {
    pub fn last_output_at(&self) -> Instant {
        self.last_output_at
    }

    pub fn interactive_prompt_kind(&self) -> Option<InteractivePromptKind> {
        let term = self.term.lock_unfair();
        let cursor_line = term.grid().cursor.point.line;
        let columns = term.grid().columns();

        let mut lines = Vec::new();
        for line_offset in 0..20 {
            let line_idx = Line(cursor_line.0 - line_offset);
            if line_idx.0 < term.topmost_line().0 {
                break;
            }
            let mut text = String::new();
            for col in 0..columns {
                text.push(term.grid()[line_idx][Column(col)].c);
            }
            lines.push(text.trim_end().to_string());
        }

        let combined = lines.join("\n");

        let is_prompt_char = |line: &str| {
            let trimmed = line.trim();
            trimmed.starts_with('❯')
                || trimmed.starts_with('\u{2771}')
                || trimmed.starts_with('›')
                || trimmed.starts_with('\u{203a}')
        };

        let has_numbered_options = lines.iter().any(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("1.") || trimmed.starts_with("› 1.") || trimmed.starts_with("❯ 1.")
        }) && lines.iter().any(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("2.") || trimmed.starts_with("3.")
        });
        if has_numbered_options {
            return Some(InteractivePromptKind::ChooseOption);
        }

        let has_yn_prompt = combined.contains("[y/n]")
            || combined.contains("[Y/n]")
            || combined.contains("[yes/no]");
        if has_yn_prompt {
            return Some(InteractivePromptKind::Confirmation);
        }

        let has_proceed_prompt = combined.contains("Would you like to proceed")
            || combined.contains("Shall I proceed")
            || combined.contains("Do you want to proceed");
        if has_proceed_prompt {
            return Some(InteractivePromptKind::Confirmation);
        }

        // Claude Code shows "❯" (U+2771) or "›" (U+203A) on the input line.
        // Check cursor line and a few lines up (status lines like "⏵⏵ bypass
        // permissions on" can appear between the prompt char and the cursor).
        let cursor_line_text = lines.first().map(|line| line.as_str()).unwrap_or("");
        let has_claude_prompt = is_prompt_char(cursor_line_text)
            || lines.iter().take(5).any(|line| is_prompt_char(line));

        if has_claude_prompt {
            // Filter out the initial Claude Code startup screen: the version
            // banner (e.g. "Claude Code v2.1.89") appears near the prompt
            // before any conversation has happened. This is not an actionable
            // "awaiting input" state, it's just the app having launched.
            // The version string also appears in the status bar during active
            // sessions, so we require that most lines are empty (characteristic
            // of the sparse startup splash, not a conversation in progress).
            let has_version_banner = combined.contains("Claude Code v");
            let non_empty_lines = lines.iter().filter(|line| !line.is_empty()).count();
            if has_version_banner && non_empty_lines <= 5 {
                return None;
            }
            return Some(InteractivePromptKind::GeneralInput);
        }

        None
    }
}
