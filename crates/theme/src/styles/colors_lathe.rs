#![allow(missing_docs)]

use std::sync::Arc;

use gpui::Hsla;
use strum::{AsRefStr, EnumIter, IntoEnumIterator};

use crate::{StatusColors, ThemeColorField, ThemeColors, ThemeStyles};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub enum ColorCategory {
    Border,
    Background,
    Element,
    Text,
    Icon,
    Tab,
    Panel,
    Scrollbar,
    Minimap,
    Editor,
    Search,
    Vim,
    Debugger,
    Terminal,
    VersionControl,
    Gutter,
    Status,
    Player,
    Accent,
    Syntax,
    Other,
}

impl ColorCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Border => "Border",
            Self::Background => "Background",
            Self::Element => "Element",
            Self::Text => "Text",
            Self::Icon => "Icon",
            Self::Tab => "Tab",
            Self::Panel => "Panel",
            Self::Scrollbar => "Scrollbar",
            Self::Minimap => "Minimap",
            Self::Editor => "Editor",
            Self::Search => "Search",
            Self::Vim => "Vim",
            Self::Debugger => "Debugger",
            Self::Terminal => "Terminal",
            Self::VersionControl => "Version Control",
            Self::Gutter => "Gutter",
            Self::Status => "Status",
            Self::Player => "Player",
            Self::Accent => "Accent",
            Self::Syntax => "Syntax",
            Self::Other => "Other",
        }
    }
}

impl ThemeColorField {
    pub fn category(&self) -> ColorCategory {
        let name = self.as_ref();
        if name.contains("debugger") {
            ColorCategory::Debugger
        } else if name.starts_with("border") {
            ColorCategory::Border
        } else if name.starts_with("element")
            || name.starts_with("ghost_element")
            || name.starts_with("drop_target")
        {
            ColorCategory::Element
        } else if name.starts_with("text") || name.starts_with("link_text") {
            ColorCategory::Text
        } else if name.starts_with("icon") {
            ColorCategory::Icon
        } else if name.starts_with("tab") {
            ColorCategory::Tab
        } else if name.starts_with("panel") || name.starts_with("pane") {
            ColorCategory::Panel
        } else if name.starts_with("scrollbar") {
            ColorCategory::Scrollbar
        } else if name.starts_with("minimap") {
            ColorCategory::Minimap
        } else if name.starts_with("search") {
            ColorCategory::Search
        } else if name.starts_with("vim") {
            ColorCategory::Vim
        } else if name.starts_with("editor") {
            ColorCategory::Editor
        } else if name.starts_with("terminal") {
            ColorCategory::Terminal
        } else if name.starts_with("version_control") {
            ColorCategory::VersionControl
        } else if name.starts_with("gutter") {
            ColorCategory::Gutter
        } else if name.contains("background") || name.contains("surface") {
            ColorCategory::Background
        } else {
            ColorCategory::Other
        }
    }

    pub fn display_name(&self) -> String {
        self.as_ref().replace('_', " ")
    }

    pub fn is_lathe_custom(&self) -> bool {
        matches!(
            self,
            Self::TabModifiedForeground
                | Self::TabModifiedBackground
                | Self::TabCreatedForeground
                | Self::TabCreatedBackground
                | Self::TabDeletedForeground
                | Self::TabDeletedBackground
                | Self::TabConflictForeground
                | Self::TabConflictBackground
                | Self::TabErrorForeground
                | Self::TabErrorBackground
                | Self::TabWarningForeground
                | Self::TabWarningBackground
                | Self::TabDirtyBackground
                | Self::PanelModifiedBackground
                | Self::PanelCreatedBackground
                | Self::PanelDeletedBackground
                | Self::PanelConflictBackground
                | Self::GutterAddedBackground
                | Self::GutterModifiedBackground
                | Self::GutterDeletedBackground
        )
    }
}

impl ThemeColors {
    pub fn set_color(&mut self, field: ThemeColorField, value: Hsla) {
        match field {
            ThemeColorField::Border => self.border = value,
            ThemeColorField::BorderVariant => self.border_variant = value,
            ThemeColorField::BorderFocused => self.border_focused = value,
            ThemeColorField::BorderSelected => self.border_selected = value,
            ThemeColorField::BorderTransparent => self.border_transparent = value,
            ThemeColorField::BorderDisabled => self.border_disabled = value,
            ThemeColorField::ElevatedSurfaceBackground => self.elevated_surface_background = value,
            ThemeColorField::SurfaceBackground => self.surface_background = value,
            ThemeColorField::Background => self.background = value,
            ThemeColorField::ElementBackground => self.element_background = value,
            ThemeColorField::ElementHover => self.element_hover = value,
            ThemeColorField::ElementActive => self.element_active = value,
            ThemeColorField::ElementSelected => self.element_selected = value,
            ThemeColorField::ElementSelectionBackground => {
                self.element_selection_background = value
            }
            ThemeColorField::ElementDisabled => self.element_disabled = value,
            ThemeColorField::DropTargetBackground => self.drop_target_background = value,
            ThemeColorField::DropTargetBorder => self.drop_target_border = value,
            ThemeColorField::GhostElementBackground => self.ghost_element_background = value,
            ThemeColorField::GhostElementHover => self.ghost_element_hover = value,
            ThemeColorField::GhostElementActive => self.ghost_element_active = value,
            ThemeColorField::GhostElementSelected => self.ghost_element_selected = value,
            ThemeColorField::GhostElementDisabled => self.ghost_element_disabled = value,
            ThemeColorField::Text => self.text = value,
            ThemeColorField::TextMuted => self.text_muted = value,
            ThemeColorField::TextPlaceholder => self.text_placeholder = value,
            ThemeColorField::TextDisabled => self.text_disabled = value,
            ThemeColorField::TextAccent => self.text_accent = value,
            ThemeColorField::Icon => self.icon = value,
            ThemeColorField::IconMuted => self.icon_muted = value,
            ThemeColorField::IconDisabled => self.icon_disabled = value,
            ThemeColorField::IconPlaceholder => self.icon_placeholder = value,
            ThemeColorField::IconAccent => self.icon_accent = value,
            ThemeColorField::DebuggerAccent => self.debugger_accent = value,
            ThemeColorField::StatusBarBackground => self.status_bar_background = value,
            ThemeColorField::TitleBarBackground => self.title_bar_background = value,
            ThemeColorField::TitleBarInactiveBackground => {
                self.title_bar_inactive_background = value
            }
            ThemeColorField::ToolbarBackground => self.toolbar_background = value,
            ThemeColorField::TabBarBackground => self.tab_bar_background = value,
            ThemeColorField::TabInactiveBackground => self.tab_inactive_background = value,
            ThemeColorField::TabActiveBackground => self.tab_active_background = value,
            ThemeColorField::TabModifiedForeground => self.lathe.tab_modified_foreground = value,
            ThemeColorField::TabModifiedBackground => self.lathe.tab_modified_background = value,
            ThemeColorField::TabCreatedForeground => self.lathe.tab_created_foreground = value,
            ThemeColorField::TabCreatedBackground => self.lathe.tab_created_background = value,
            ThemeColorField::TabDeletedForeground => self.lathe.tab_deleted_foreground = value,
            ThemeColorField::TabDeletedBackground => self.lathe.tab_deleted_background = value,
            ThemeColorField::TabConflictForeground => self.lathe.tab_conflict_foreground = value,
            ThemeColorField::TabConflictBackground => self.lathe.tab_conflict_background = value,
            ThemeColorField::TabErrorForeground => self.lathe.tab_error_foreground = value,
            ThemeColorField::TabErrorBackground => self.lathe.tab_error_background = value,
            ThemeColorField::TabWarningForeground => self.lathe.tab_warning_foreground = value,
            ThemeColorField::TabWarningBackground => self.lathe.tab_warning_background = value,
            ThemeColorField::TabDirtyBackground => self.lathe.tab_dirty_background = value,
            ThemeColorField::SearchMatchBackground => self.search_match_background = value,
            ThemeColorField::SearchActiveMatchBackground => {
                self.search_active_match_background = value
            }
            ThemeColorField::PanelBackground => self.panel_background = value,
            ThemeColorField::PanelFocusedBorder => self.panel_focused_border = value,
            ThemeColorField::PanelModifiedBackground => {
                self.lathe.panel_modified_background = value
            }
            ThemeColorField::PanelCreatedBackground => self.lathe.panel_created_background = value,
            ThemeColorField::PanelDeletedBackground => self.lathe.panel_deleted_background = value,
            ThemeColorField::PanelConflictBackground => {
                self.lathe.panel_conflict_background = value
            }
            ThemeColorField::PanelIndentGuide => self.panel_indent_guide = value,
            ThemeColorField::PanelIndentGuideHover => self.panel_indent_guide_hover = value,
            ThemeColorField::PanelIndentGuideActive => self.panel_indent_guide_active = value,
            ThemeColorField::PanelOverlayBackground => self.panel_overlay_background = value,
            ThemeColorField::PanelOverlayHover => self.panel_overlay_hover = value,
            ThemeColorField::PaneFocusedBorder => self.pane_focused_border = value,
            ThemeColorField::PaneGroupBorder => self.pane_group_border = value,
            ThemeColorField::ScrollbarThumbBackground => self.scrollbar_thumb_background = value,
            ThemeColorField::ScrollbarThumbHoverBackground => {
                self.scrollbar_thumb_hover_background = value
            }
            ThemeColorField::ScrollbarThumbActiveBackground => {
                self.scrollbar_thumb_active_background = value
            }
            ThemeColorField::ScrollbarThumbBorder => self.scrollbar_thumb_border = value,
            ThemeColorField::ScrollbarTrackBackground => self.scrollbar_track_background = value,
            ThemeColorField::ScrollbarTrackBorder => self.scrollbar_track_border = value,
            ThemeColorField::MinimapThumbBackground => self.minimap_thumb_background = value,
            ThemeColorField::MinimapThumbHoverBackground => {
                self.minimap_thumb_hover_background = value
            }
            ThemeColorField::MinimapThumbActiveBackground => {
                self.minimap_thumb_active_background = value
            }
            ThemeColorField::MinimapThumbBorder => self.minimap_thumb_border = value,
            ThemeColorField::VimHelixJumpLabelForeground => {
                self.vim_helix_jump_label_foreground = value
            }
            ThemeColorField::VimNormalBackground => self.vim_normal_background = value,
            ThemeColorField::VimInsertBackground => self.vim_insert_background = value,
            ThemeColorField::VimReplaceBackground => self.vim_replace_background = value,
            ThemeColorField::VimVisualBackground => self.vim_visual_background = value,
            ThemeColorField::VimVisualLineBackground => self.vim_visual_line_background = value,
            ThemeColorField::VimVisualBlockBackground => self.vim_visual_block_background = value,
            ThemeColorField::VimYankBackground => self.vim_yank_background = value,
            ThemeColorField::VimHelixNormalBackground => self.vim_helix_normal_background = value,
            ThemeColorField::VimHelixSelectBackground => self.vim_helix_select_background = value,
            ThemeColorField::VimNormalForeground => self.vim_normal_foreground = value,
            ThemeColorField::VimInsertForeground => self.vim_insert_foreground = value,
            ThemeColorField::VimReplaceForeground => self.vim_replace_foreground = value,
            ThemeColorField::VimVisualForeground => self.vim_visual_foreground = value,
            ThemeColorField::VimVisualLineForeground => self.vim_visual_line_foreground = value,
            ThemeColorField::VimVisualBlockForeground => self.vim_visual_block_foreground = value,
            ThemeColorField::VimHelixNormalForeground => self.vim_helix_normal_foreground = value,
            ThemeColorField::VimHelixSelectForeground => self.vim_helix_select_foreground = value,
            ThemeColorField::EditorForeground => self.editor_foreground = value,
            ThemeColorField::EditorBackground => self.editor_background = value,
            ThemeColorField::EditorGutterBackground => self.editor_gutter_background = value,
            ThemeColorField::EditorSubheaderBackground => self.editor_subheader_background = value,
            ThemeColorField::EditorActiveLineBackground => {
                self.editor_active_line_background = value
            }
            ThemeColorField::EditorHighlightedLineBackground => {
                self.editor_highlighted_line_background = value
            }
            ThemeColorField::EditorDebuggerActiveLineBackground => {
                self.editor_debugger_active_line_background = value
            }
            ThemeColorField::EditorLineNumber => self.editor_line_number = value,
            ThemeColorField::EditorActiveLineNumber => self.editor_active_line_number = value,
            ThemeColorField::EditorHoverLineNumber => self.editor_hover_line_number = value,
            ThemeColorField::EditorInvisible => self.editor_invisible = value,
            ThemeColorField::EditorWrapGuide => self.editor_wrap_guide = value,
            ThemeColorField::EditorActiveWrapGuide => self.editor_active_wrap_guide = value,
            ThemeColorField::EditorIndentGuide => self.editor_indent_guide = value,
            ThemeColorField::EditorIndentGuideActive => self.editor_indent_guide_active = value,
            ThemeColorField::EditorDocumentHighlightReadBackground => {
                self.editor_document_highlight_read_background = value
            }
            ThemeColorField::EditorDocumentHighlightWriteBackground => {
                self.editor_document_highlight_write_background = value
            }
            ThemeColorField::EditorDocumentHighlightBracketBackground => {
                self.editor_document_highlight_bracket_background = value
            }
            ThemeColorField::EditorDiffHunkAddedBackground => {
                self.editor_diff_hunk_added_background = value
            }
            ThemeColorField::EditorDiffHunkAddedHollowBackground => {
                self.editor_diff_hunk_added_hollow_background = value
            }
            ThemeColorField::EditorDiffHunkAddedHollowBorder => {
                self.editor_diff_hunk_added_hollow_border = value
            }
            ThemeColorField::EditorDiffHunkDeletedBackground => {
                self.editor_diff_hunk_deleted_background = value
            }
            ThemeColorField::EditorDiffHunkDeletedHollowBackground => {
                self.editor_diff_hunk_deleted_hollow_background = value
            }
            ThemeColorField::EditorDiffHunkDeletedHollowBorder => {
                self.editor_diff_hunk_deleted_hollow_border = value
            }
            ThemeColorField::TerminalBackground => self.terminal_background = value,
            ThemeColorField::TerminalForeground => self.terminal_foreground = value,
            ThemeColorField::TerminalBrightForeground => self.terminal_bright_foreground = value,
            ThemeColorField::TerminalDimForeground => self.terminal_dim_foreground = value,
            ThemeColorField::TerminalAnsiBackground => self.terminal_ansi_background = value,
            ThemeColorField::TerminalAnsiBlack => self.terminal_ansi_black = value,
            ThemeColorField::TerminalAnsiBrightBlack => self.terminal_ansi_bright_black = value,
            ThemeColorField::TerminalAnsiDimBlack => self.terminal_ansi_dim_black = value,
            ThemeColorField::TerminalAnsiRed => self.terminal_ansi_red = value,
            ThemeColorField::TerminalAnsiBrightRed => self.terminal_ansi_bright_red = value,
            ThemeColorField::TerminalAnsiDimRed => self.terminal_ansi_dim_red = value,
            ThemeColorField::TerminalAnsiGreen => self.terminal_ansi_green = value,
            ThemeColorField::TerminalAnsiBrightGreen => self.terminal_ansi_bright_green = value,
            ThemeColorField::TerminalAnsiDimGreen => self.terminal_ansi_dim_green = value,
            ThemeColorField::TerminalAnsiYellow => self.terminal_ansi_yellow = value,
            ThemeColorField::TerminalAnsiBrightYellow => self.terminal_ansi_bright_yellow = value,
            ThemeColorField::TerminalAnsiDimYellow => self.terminal_ansi_dim_yellow = value,
            ThemeColorField::TerminalAnsiBlue => self.terminal_ansi_blue = value,
            ThemeColorField::TerminalAnsiBrightBlue => self.terminal_ansi_bright_blue = value,
            ThemeColorField::TerminalAnsiDimBlue => self.terminal_ansi_dim_blue = value,
            ThemeColorField::TerminalAnsiMagenta => self.terminal_ansi_magenta = value,
            ThemeColorField::TerminalAnsiBrightMagenta => self.terminal_ansi_bright_magenta = value,
            ThemeColorField::TerminalAnsiDimMagenta => self.terminal_ansi_dim_magenta = value,
            ThemeColorField::TerminalAnsiCyan => self.terminal_ansi_cyan = value,
            ThemeColorField::TerminalAnsiBrightCyan => self.terminal_ansi_bright_cyan = value,
            ThemeColorField::TerminalAnsiDimCyan => self.terminal_ansi_dim_cyan = value,
            ThemeColorField::TerminalAnsiWhite => self.terminal_ansi_white = value,
            ThemeColorField::TerminalAnsiBrightWhite => self.terminal_ansi_bright_white = value,
            ThemeColorField::TerminalAnsiDimWhite => self.terminal_ansi_dim_white = value,
            ThemeColorField::LinkTextHover => self.link_text_hover = value,
            ThemeColorField::VersionControlAdded => self.version_control_added = value,
            ThemeColorField::VersionControlDeleted => self.version_control_deleted = value,
            ThemeColorField::VersionControlModified => self.version_control_modified = value,
            ThemeColorField::VersionControlRenamed => self.version_control_renamed = value,
            ThemeColorField::VersionControlConflict => self.version_control_conflict = value,
            ThemeColorField::VersionControlIgnored => self.version_control_ignored = value,
            ThemeColorField::VersionControlWordAdded => self.version_control_word_added = value,
            ThemeColorField::VersionControlWordDeleted => self.version_control_word_deleted = value,
            ThemeColorField::VersionControlConflictMarkerOurs => {
                self.version_control_conflict_marker_ours = value
            }
            ThemeColorField::VersionControlConflictMarkerTheirs => {
                self.version_control_conflict_marker_theirs = value
            }
            ThemeColorField::GutterAddedBackground => self.lathe.gutter_added_background = value,
            ThemeColorField::GutterModifiedBackground => {
                self.lathe.gutter_modified_background = value
            }
            ThemeColorField::GutterDeletedBackground => {
                self.lathe.gutter_deleted_background = value
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum StatusColorField {
    Conflict,
    ConflictBackground,
    ConflictBorder,
    Created,
    CreatedBackground,
    CreatedBorder,
    Deleted,
    DeletedBackground,
    DeletedBorder,
    Error,
    ErrorBackground,
    ErrorBorder,
    Hidden,
    HiddenBackground,
    HiddenBorder,
    Hint,
    HintBackground,
    HintBorder,
    Ignored,
    IgnoredBackground,
    IgnoredBorder,
    Info,
    InfoBackground,
    InfoBorder,
    Modified,
    ModifiedBackground,
    ModifiedBorder,
    Predictive,
    PredictiveBackground,
    PredictiveBorder,
    Renamed,
    RenamedBackground,
    RenamedBorder,
    Success,
    SuccessBackground,
    SuccessBorder,
    Unreachable,
    UnreachableBackground,
    UnreachableBorder,
    Warning,
    WarningBackground,
    WarningBorder,
}

impl StatusColorField {
    pub fn display_name(&self) -> String {
        format!("status {}", self.as_ref().replace('_', " "))
    }
}

impl StatusColors {
    pub fn color(&self, field: StatusColorField) -> Hsla {
        match field {
            StatusColorField::Conflict => self.conflict,
            StatusColorField::ConflictBackground => self.conflict_background,
            StatusColorField::ConflictBorder => self.conflict_border,
            StatusColorField::Created => self.created,
            StatusColorField::CreatedBackground => self.created_background,
            StatusColorField::CreatedBorder => self.created_border,
            StatusColorField::Deleted => self.deleted,
            StatusColorField::DeletedBackground => self.deleted_background,
            StatusColorField::DeletedBorder => self.deleted_border,
            StatusColorField::Error => self.error,
            StatusColorField::ErrorBackground => self.error_background,
            StatusColorField::ErrorBorder => self.error_border,
            StatusColorField::Hidden => self.hidden,
            StatusColorField::HiddenBackground => self.hidden_background,
            StatusColorField::HiddenBorder => self.hidden_border,
            StatusColorField::Hint => self.hint,
            StatusColorField::HintBackground => self.hint_background,
            StatusColorField::HintBorder => self.hint_border,
            StatusColorField::Ignored => self.ignored,
            StatusColorField::IgnoredBackground => self.ignored_background,
            StatusColorField::IgnoredBorder => self.ignored_border,
            StatusColorField::Info => self.info,
            StatusColorField::InfoBackground => self.info_background,
            StatusColorField::InfoBorder => self.info_border,
            StatusColorField::Modified => self.modified,
            StatusColorField::ModifiedBackground => self.modified_background,
            StatusColorField::ModifiedBorder => self.modified_border,
            StatusColorField::Predictive => self.predictive,
            StatusColorField::PredictiveBackground => self.predictive_background,
            StatusColorField::PredictiveBorder => self.predictive_border,
            StatusColorField::Renamed => self.renamed,
            StatusColorField::RenamedBackground => self.renamed_background,
            StatusColorField::RenamedBorder => self.renamed_border,
            StatusColorField::Success => self.success,
            StatusColorField::SuccessBackground => self.success_background,
            StatusColorField::SuccessBorder => self.success_border,
            StatusColorField::Unreachable => self.unreachable,
            StatusColorField::UnreachableBackground => self.unreachable_background,
            StatusColorField::UnreachableBorder => self.unreachable_border,
            StatusColorField::Warning => self.warning,
            StatusColorField::WarningBackground => self.warning_background,
            StatusColorField::WarningBorder => self.warning_border,
        }
    }

    pub fn set_color(&mut self, field: StatusColorField, value: Hsla) {
        match field {
            StatusColorField::Conflict => self.conflict = value,
            StatusColorField::ConflictBackground => self.conflict_background = value,
            StatusColorField::ConflictBorder => self.conflict_border = value,
            StatusColorField::Created => self.created = value,
            StatusColorField::CreatedBackground => self.created_background = value,
            StatusColorField::CreatedBorder => self.created_border = value,
            StatusColorField::Deleted => self.deleted = value,
            StatusColorField::DeletedBackground => self.deleted_background = value,
            StatusColorField::DeletedBorder => self.deleted_border = value,
            StatusColorField::Error => self.error = value,
            StatusColorField::ErrorBackground => self.error_background = value,
            StatusColorField::ErrorBorder => self.error_border = value,
            StatusColorField::Hidden => self.hidden = value,
            StatusColorField::HiddenBackground => self.hidden_background = value,
            StatusColorField::HiddenBorder => self.hidden_border = value,
            StatusColorField::Hint => self.hint = value,
            StatusColorField::HintBackground => self.hint_background = value,
            StatusColorField::HintBorder => self.hint_border = value,
            StatusColorField::Ignored => self.ignored = value,
            StatusColorField::IgnoredBackground => self.ignored_background = value,
            StatusColorField::IgnoredBorder => self.ignored_border = value,
            StatusColorField::Info => self.info = value,
            StatusColorField::InfoBackground => self.info_background = value,
            StatusColorField::InfoBorder => self.info_border = value,
            StatusColorField::Modified => self.modified = value,
            StatusColorField::ModifiedBackground => self.modified_background = value,
            StatusColorField::ModifiedBorder => self.modified_border = value,
            StatusColorField::Predictive => self.predictive = value,
            StatusColorField::PredictiveBackground => self.predictive_background = value,
            StatusColorField::PredictiveBorder => self.predictive_border = value,
            StatusColorField::Renamed => self.renamed = value,
            StatusColorField::RenamedBackground => self.renamed_background = value,
            StatusColorField::RenamedBorder => self.renamed_border = value,
            StatusColorField::Success => self.success = value,
            StatusColorField::SuccessBackground => self.success_background = value,
            StatusColorField::SuccessBorder => self.success_border = value,
            StatusColorField::Unreachable => self.unreachable = value,
            StatusColorField::UnreachableBackground => self.unreachable_background = value,
            StatusColorField::UnreachableBorder => self.unreachable_border = value,
            StatusColorField::Warning => self.warning = value,
            StatusColorField::WarningBackground => self.warning_background = value,
            StatusColorField::WarningBorder => self.warning_border = value,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlayerColorChannel {
    Cursor,
    Background,
    Selection,
}

impl PlayerColorChannel {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::Background => "background",
            Self::Selection => "selection",
        }
    }
}

/// A single customizable color anywhere in the theme: the flat UI colors,
/// status colors, per-player collaboration colors, accent colors, and syntax
/// highlight colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CustomizableColor {
    Theme(ThemeColorField),
    Status(StatusColorField),
    Player(usize, PlayerColorChannel),
    Accent(usize),
    Syntax(usize),
}

impl CustomizableColor {
    pub fn category(&self) -> ColorCategory {
        match self {
            Self::Theme(field) => field.category(),
            Self::Status(_) => ColorCategory::Status,
            Self::Player(..) => ColorCategory::Player,
            Self::Accent(_) => ColorCategory::Accent,
            Self::Syntax(_) => ColorCategory::Syntax,
        }
    }

    pub fn is_lathe_custom(&self) -> bool {
        match self {
            Self::Theme(field) => field.is_lathe_custom(),
            _ => false,
        }
    }

    /// Stable identifier, also shown as the technical name in the editor pane.
    pub fn key(&self, styles: &ThemeStyles) -> String {
        match self {
            Self::Theme(field) => field.as_ref().to_string(),
            Self::Status(field) => format!("status.{}", field.as_ref()),
            Self::Player(index, channel) => format!("player.{index}.{}", channel.name()),
            Self::Accent(index) => format!("accents.{index}"),
            Self::Syntax(index) => format!(
                "syntax.{}",
                styles.syntax.get_capture_name(*index).unwrap_or("unknown")
            ),
        }
    }

    pub fn display_name(&self, styles: &ThemeStyles) -> String {
        match self {
            Self::Theme(field) => field.display_name(),
            Self::Status(field) => field.display_name(),
            Self::Player(index, channel) => format!("player {} {}", index + 1, channel.name()),
            Self::Accent(index) => format!("accent {}", index + 1),
            Self::Syntax(index) => format!(
                "syntax {}",
                styles.syntax.get_capture_name(*index).unwrap_or("unknown")
            ),
        }
    }
}

impl ThemeStyles {
    /// Every customizable color in this theme, ordered by group: theme colors,
    /// status colors, player colors, accents, then syntax highlights sorted by
    /// capture name.
    pub fn all_customizable_colors(&self) -> Vec<CustomizableColor> {
        let mut fields: Vec<CustomizableColor> = ThemeColorField::iter()
            .map(CustomizableColor::Theme)
            .collect();
        fields.extend(StatusColorField::iter().map(CustomizableColor::Status));
        for index in 0..self.player.0.len() {
            for channel in [
                PlayerColorChannel::Cursor,
                PlayerColorChannel::Background,
                PlayerColorChannel::Selection,
            ] {
                fields.push(CustomizableColor::Player(index, channel));
            }
        }
        fields.extend((0..self.accents.0.len()).map(CustomizableColor::Accent));
        fields.extend(
            self.syntax
                .capture_names_with_indices()
                .map(|(_, index)| CustomizableColor::Syntax(index)),
        );
        fields
    }

    pub fn customizable_color(&self, field: CustomizableColor) -> Hsla {
        match field {
            CustomizableColor::Theme(field) => self.colors.color(field),
            CustomizableColor::Status(field) => self.status.color(field),
            CustomizableColor::Player(index, channel) => {
                let player = self.player.0.get(index).copied().unwrap_or_default();
                match channel {
                    PlayerColorChannel::Cursor => player.cursor,
                    PlayerColorChannel::Background => player.background,
                    PlayerColorChannel::Selection => player.selection,
                }
            }
            CustomizableColor::Accent(index) => {
                self.accents.0.get(index).copied().unwrap_or_default()
            }
            CustomizableColor::Syntax(index) => self
                .syntax
                .highlight_color(index)
                .unwrap_or(self.colors.editor_foreground),
        }
    }

    pub fn set_customizable_color(&mut self, field: CustomizableColor, value: Hsla) {
        match field {
            CustomizableColor::Theme(field) => self.colors.set_color(field, value),
            CustomizableColor::Status(field) => self.status.set_color(field, value),
            CustomizableColor::Player(index, channel) => {
                if let Some(player) = self.player.0.get_mut(index) {
                    match channel {
                        PlayerColorChannel::Cursor => player.cursor = value,
                        PlayerColorChannel::Background => player.background = value,
                        PlayerColorChannel::Selection => player.selection = value,
                    }
                }
            }
            CustomizableColor::Accent(index) => {
                let mut accents = self.accents.0.to_vec();
                if let Some(slot) = accents.get_mut(index) {
                    *slot = value;
                    self.accents.0 = Arc::from(accents);
                }
            }
            CustomizableColor::Syntax(index) => {
                Arc::make_mut(&mut self.syntax).set_highlight_color(index, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Proves the field enums cover every struct field on which the light and
    // dark palettes disagree; a field missing from the enum leaves the light
    // value in place and fails the equality check.
    #[test]
    fn theme_color_field_covers_all_theme_colors() {
        let mut light = ThemeColors::light();
        let dark = ThemeColors::dark();
        for field in ThemeColorField::iter() {
            light.set_color(field, dark.color(field));
        }
        assert_eq!(light, dark);
    }

    #[test]
    fn status_color_field_covers_all_status_colors() {
        let mut light = StatusColors::light();
        let dark = StatusColors::dark();
        for field in StatusColorField::iter() {
            light.set_color(field, dark.color(field));
        }
        assert_eq!(light, dark);
    }

    #[test]
    fn every_theme_color_field_is_categorized() {
        for field in ThemeColorField::iter() {
            assert_ne!(
                field.category(),
                ColorCategory::Other,
                "{} is uncategorized",
                field.as_ref()
            );
        }
    }
}
