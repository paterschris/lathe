use crate::TerminalView;
use gpui::{Context, Window};
use settings::{Settings, TerminalBell};
use terminal::terminal_settings::TerminalSettings;

pub(super) fn handle(
    terminal_view: &mut TerminalView,
    window: &mut Window,
    cx: &mut Context<TerminalView>,
) {
    terminal_view.has_bell = true;
    if let TerminalBell::System = TerminalSettings::get_global(cx).bell {
        window.play_system_bell();
    }
}
