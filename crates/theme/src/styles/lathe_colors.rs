#![allow(missing_docs)]

use gpui::Hsla;
use refineable::Refineable;

#[derive(Refineable, Clone, Debug, PartialEq)]
#[refineable(Debug, serde::Deserialize)]
pub struct LatheThemeColors {
    pub tab_modified_foreground: Hsla,
    pub tab_modified_background: Hsla,
    pub tab_created_foreground: Hsla,
    pub tab_created_background: Hsla,
    pub tab_deleted_foreground: Hsla,
    pub tab_deleted_background: Hsla,
    pub tab_conflict_foreground: Hsla,
    pub tab_conflict_background: Hsla,
    pub tab_error_foreground: Hsla,
    pub tab_error_background: Hsla,
    pub tab_warning_foreground: Hsla,
    pub tab_warning_background: Hsla,
    pub tab_dirty_background: Hsla,
    pub panel_modified_background: Hsla,
    pub panel_created_background: Hsla,
    pub panel_deleted_background: Hsla,
    pub panel_conflict_background: Hsla,
    pub gutter_added_background: Hsla,
    pub gutter_modified_background: Hsla,
    pub gutter_deleted_background: Hsla,
}
