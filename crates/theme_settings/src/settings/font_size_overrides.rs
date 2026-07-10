use collections::HashMap;
use gpui::{App, Context, Global, Pixels, Subscription, Window, WindowId, px};
use settings::Settings;

use super::{ThemeSettings, clamp_font_size};

/// In-memory per-window overrides for the buffer font size.
///
/// CMD+/- (and the scroll-wheel zoom) write into this map keyed by the
/// active window's id, so each window remembers its own zoom level.
#[derive(Default)]
struct BufferFontSize(HashMap<WindowId, Pixels>);

impl Global for BufferFontSize {}

#[derive(Default)]
pub(crate) struct UiFontSize(HashMap<WindowId, Pixels>);

impl Global for UiFontSize {}

/// In-memory per-window overrides for the UI font size in the agent panel.
#[derive(Default)]
pub struct AgentUiFontSize(HashMap<WindowId, Pixels>);

impl Global for AgentUiFontSize {}

/// In-memory per-window overrides for the buffer (user message) font size in the agent panel.
#[derive(Default)]
pub struct AgentBufferFontSize(HashMap<WindowId, Pixels>);

impl Global for AgentBufferFontSize {}

#[derive(Default)]
pub struct GitCommitBufferFontSize(HashMap<WindowId, Pixels>);

impl Global for GitCommitBufferFontSize {}

impl ThemeSettings {
    /// Returns the buffer font size for the given window, honoring per-window zoom.
    pub fn buffer_font_size_in_window(&self, window: &Window, cx: &App) -> Pixels {
        let id = window.window_handle().window_id();
        cx.try_global::<BufferFontSize>()
            .and_then(|sizes| sizes.0.get(&id).copied())
            .map(clamp_font_size)
            .unwrap_or_else(|| self.buffer_font_size(cx))
    }

    /// Returns the UI font size for the given window, honoring per-window zoom.
    pub fn ui_font_size_in_window(&self, window: &Window, cx: &App) -> Pixels {
        let id = window.window_handle().window_id();
        cx.try_global::<UiFontSize>()
            .and_then(|sizes| sizes.0.get(&id).copied())
            .map(clamp_font_size)
            .unwrap_or_else(|| self.ui_font_size(cx))
    }

    /// Returns the agent UI font size for the given window, honoring per-window zoom.
    pub fn agent_ui_font_size_in_window(&self, window: &Window, cx: &App) -> Pixels {
        let id = window.window_handle().window_id();
        cx.try_global::<AgentUiFontSize>()
            .and_then(|sizes| sizes.0.get(&id).copied())
            .map(clamp_font_size)
            .or_else(|| self.agent_ui_font_size.map(clamp_font_size))
            .unwrap_or_else(|| self.ui_font_size_in_window(window, cx))
    }

    /// Returns the agent buffer font size for the given window, honoring per-window zoom.
    pub fn agent_buffer_font_size_in_window(&self, window: &Window, cx: &App) -> Pixels {
        let id = window.window_handle().window_id();
        cx.try_global::<AgentBufferFontSize>()
            .and_then(|sizes| sizes.0.get(&id).copied())
            .map(clamp_font_size)
            .or_else(|| self.agent_buffer_font_size.map(clamp_font_size))
            .unwrap_or_else(|| self.buffer_font_size_in_window(window, cx))
    }

    pub fn git_commit_buffer_font_size_in_window(&self, window: &Window, cx: &App) -> Pixels {
        let id = window.window_handle().window_id();
        cx.try_global::<GitCommitBufferFontSize>()
            .and_then(|sizes| sizes.0.get(&id).copied())
            .map(clamp_font_size)
            .or_else(|| self.git_commit_buffer_font_size.map(clamp_font_size))
            .unwrap_or_else(|| self.buffer_font_size_in_window(window, cx))
    }
}

/// Remove every per-window font override for the given window. Called when a
/// window closes so the override maps don't leak.
pub fn clear_window_font_overrides(cx: &mut App, window_id: WindowId) {
    if cx.has_global::<BufferFontSize>() {
        cx.default_global::<BufferFontSize>().0.remove(&window_id);
    }
    if cx.has_global::<UiFontSize>() {
        cx.default_global::<UiFontSize>().0.remove(&window_id);
    }
    if cx.has_global::<AgentUiFontSize>() {
        cx.default_global::<AgentUiFontSize>().0.remove(&window_id);
    }
    if cx.has_global::<AgentBufferFontSize>() {
        cx.default_global::<AgentBufferFontSize>()
            .0
            .remove(&window_id);
    }
    if cx.has_global::<GitCommitBufferFontSize>() {
        cx.default_global::<GitCommitBufferFontSize>()
            .0
            .remove(&window_id);
    }
}

/// Observe changes to the adjusted buffer font size.
pub fn observe_buffer_font_size_adjustment<V: 'static>(
    cx: &mut Context<V>,
    f: impl 'static + Fn(&mut V, &mut Context<V>),
) -> Subscription {
    cx.observe_global::<BufferFontSize>(f)
}

/// Gets a font size adjusted by the delta between the active window's zoom-adjusted
/// buffer font size and the settings buffer font size. Per-window CMD+/- zoom is
/// honored when a window is supplied.
pub fn adjusted_font_size(size: Pixels, window: &Window, cx: &App) -> Pixels {
    let theme_settings = ThemeSettings::get_global(cx);
    let adjusted = theme_settings.buffer_font_size_in_window(window, cx);
    let settings_size = theme_settings.buffer_font_size;
    let delta = adjusted - settings_size;
    clamp_font_size(size + delta)
}

/// Adjusts the buffer font size for the given window only.
/// This will be effective until the window closes or the app is restarted.
pub fn adjust_buffer_font_size(
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(Pixels) -> Pixels,
) {
    let id = window.window_handle().window_id();
    let current = ThemeSettings::get_global(cx).buffer_font_size_in_window(window, cx);
    let new_size = clamp_font_size(f(current));
    cx.default_global::<BufferFontSize>().0.insert(id, new_size);
    window.refresh();
}

/// Resets the buffer font size override for the given window.
pub fn reset_buffer_font_size(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    if cx.has_global::<BufferFontSize>()
        && cx
            .default_global::<BufferFontSize>()
            .0
            .remove(&id)
            .is_some()
    {
        window.refresh();
    }
}

/// Clears every per-window buffer font size override. Used when the settings
/// file changes so per-window zoom is rebased on the new setting.
pub fn reset_buffer_font_size_for_all_windows(cx: &mut App) {
    if cx.has_global::<BufferFontSize>() {
        let already_empty = cx.default_global::<BufferFontSize>().0.is_empty();
        if !already_empty {
            cx.default_global::<BufferFontSize>().0.clear();
            cx.refresh_windows();
        }
    }
}

pub fn increase_buffer_font_size(window: &mut Window, cx: &mut App) {
    adjust_buffer_font_size(window, cx, |size| size + px(1.0));
}

pub fn decrease_buffer_font_size(window: &mut Window, cx: &mut App) {
    adjust_buffer_font_size(window, cx, |size| size - px(1.0));
}

#[allow(missing_docs)]
pub fn setup_ui_font(window: &mut Window, cx: &mut App) -> gpui::Font {
    let (ui_font, ui_font_size) = {
        let theme_settings = ThemeSettings::get_global(cx);
        let font = theme_settings.ui_font.clone();
        (font, theme_settings.ui_font_size_in_window(window, cx))
    };

    window.set_rem_size(ui_font_size);
    ui_font
}

/// Sets the adjusted UI font size for the given window only.
pub fn adjust_ui_font_size(window: &mut Window, cx: &mut App, f: impl FnOnce(Pixels) -> Pixels) {
    let id = window.window_handle().window_id();
    let current = ThemeSettings::get_global(cx).ui_font_size_in_window(window, cx);
    let new_size = clamp_font_size(f(current));
    cx.default_global::<UiFontSize>().0.insert(id, new_size);
    window.refresh();
}

/// Resets the UI font size override for the given window.
pub fn reset_ui_font_size(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    if cx.has_global::<UiFontSize>() && cx.default_global::<UiFontSize>().0.remove(&id).is_some() {
        window.refresh();
    }
}

pub fn reset_ui_font_size_for_all_windows(cx: &mut App) {
    if cx.has_global::<UiFontSize>() {
        let already_empty = cx.default_global::<UiFontSize>().0.is_empty();
        if !already_empty {
            cx.default_global::<UiFontSize>().0.clear();
            cx.refresh_windows();
        }
    }
}

/// Sets the adjusted font size of agent responses in the agent panel for the given window.
pub fn adjust_agent_ui_font_size(
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(Pixels) -> Pixels,
) {
    let id = window.window_handle().window_id();
    let current = ThemeSettings::get_global(cx).agent_ui_font_size_in_window(window, cx);
    let new_size = clamp_font_size(f(current));
    cx.default_global::<AgentUiFontSize>()
        .0
        .insert(id, new_size);
    window.refresh();
}

/// Resets the agent response font size override for the given window.
pub fn reset_agent_ui_font_size(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    if cx.has_global::<AgentUiFontSize>()
        && cx
            .default_global::<AgentUiFontSize>()
            .0
            .remove(&id)
            .is_some()
    {
        window.refresh();
    }
}

pub fn reset_agent_ui_font_size_for_all_windows(cx: &mut App) {
    if cx.has_global::<AgentUiFontSize>() {
        let already_empty = cx.default_global::<AgentUiFontSize>().0.is_empty();
        if !already_empty {
            cx.default_global::<AgentUiFontSize>().0.clear();
            cx.refresh_windows();
        }
    }
}

/// Sets the adjusted font size of user messages in the agent panel for the given window.
pub fn adjust_agent_buffer_font_size(
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(Pixels) -> Pixels,
) {
    let id = window.window_handle().window_id();
    let current = ThemeSettings::get_global(cx).agent_buffer_font_size_in_window(window, cx);
    let new_size = clamp_font_size(f(current));
    cx.default_global::<AgentBufferFontSize>()
        .0
        .insert(id, new_size);
    window.refresh();
}

/// Resets the user message font size override for the given window.
pub fn reset_agent_buffer_font_size(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    if cx.has_global::<AgentBufferFontSize>()
        && cx
            .default_global::<AgentBufferFontSize>()
            .0
            .remove(&id)
            .is_some()
    {
        window.refresh();
    }
}

pub fn reset_agent_buffer_font_size_for_all_windows(cx: &mut App) {
    if cx.has_global::<AgentBufferFontSize>() {
        let already_empty = cx.default_global::<AgentBufferFontSize>().0.is_empty();
        if !already_empty {
            cx.default_global::<AgentBufferFontSize>().0.clear();
            cx.refresh_windows();
        }
    }
}

pub fn adjust_git_commit_buffer_font_size(
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(Pixels) -> Pixels,
) {
    let id = window.window_handle().window_id();
    let current = ThemeSettings::get_global(cx).git_commit_buffer_font_size_in_window(window, cx);
    let new_size = clamp_font_size(f(current));
    cx.default_global::<GitCommitBufferFontSize>()
        .0
        .insert(id, new_size);
    window.refresh();
}

pub fn reset_git_commit_buffer_font_size(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    if cx.has_global::<GitCommitBufferFontSize>()
        && cx
            .default_global::<GitCommitBufferFontSize>()
            .0
            .remove(&id)
            .is_some()
    {
        window.refresh();
    }
}

pub fn reset_git_commit_buffer_font_size_for_all_windows(cx: &mut App) {
    if cx.has_global::<GitCommitBufferFontSize>() {
        let already_empty = cx.default_global::<GitCommitBufferFontSize>().0.is_empty();
        if !already_empty {
            cx.default_global::<GitCommitBufferFontSize>().0.clear();
            cx.refresh_windows();
        }
    }
}
