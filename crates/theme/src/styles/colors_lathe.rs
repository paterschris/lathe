#![allow(missing_docs)]

use gpui::Hsla;
use strum::EnumIter;

use crate::{ThemeColorField, ThemeColors};

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
    Terminal,
    VersionControl,
    Gutter,
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
            Self::Terminal => "Terminal",
            Self::VersionControl => "Version Control",
            Self::Gutter => "Gutter",
            Self::Other => "Other",
        }
    }
}

impl ThemeColorField {
    pub fn category(&self) -> ColorCategory {
        let name = self.as_ref();
        if name.starts_with("border") {
            ColorCategory::Border
        } else if name.starts_with("element")
            || name.starts_with("ghost_element")
            || name.starts_with("drop_target")
        {
            ColorCategory::Element
        } else if name.starts_with("text") {
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
            ThemeColorField::EditorLineNumber => self.editor_line_number = value,
            ThemeColorField::EditorActiveLineNumber => self.editor_active_line_number = value,
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
