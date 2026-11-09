//! Whether you're working in Lathe. You're active while any Lathe window got
//! input (a key, a click, the mouse moving or a scroll) within the last
//! `companion.idle_minutes`, and idle after that. Paired phones only get push
//! notifications while you're idle: when you're at Lathe, you see the update
//! there.

use crate::CompanionSettings;
use gpui::App;
use settings::Settings as _;
use std::time::{Duration, Instant};

/// Whether you count as working in Lathe right now.
pub fn is_active(cx: &App) -> bool {
    let minutes = CompanionSettings::get_global(cx).idle_minutes;
    active_at(cx.last_input_at(), cx.background_executor().now(), minutes)
}

/// Active when the last input is less than `minutes` before `now`. With
/// `minutes` at 0, or before any input since launch, you're always idle.
pub(crate) fn active_at(last_input: Option<Instant>, now: Instant, minutes: u32) -> bool {
    minutes > 0
        && last_input.is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(u64::from(minutes) * 60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, point, px};
    use settings::SettingsStore;

    #[test]
    fn active_for_the_set_minutes_after_input() {
        let start = Instant::now();
        let after = |seconds: u64| start + Duration::from_secs(seconds);
        assert!(!active_at(None, start, 5), "no input since launch");
        assert!(active_at(Some(start), after(4 * 60 + 59), 5));
        assert!(!active_at(Some(start), after(5 * 60), 5));
        assert!(!active_at(Some(start), start, 0), "0 always sends pushes");
    }

    #[gpui::test]
    fn input_in_any_window_makes_you_active(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            CompanionSettings::register(cx);
        });
        assert!(!cx.update(|cx| is_active(cx)));
        let window = cx.add_empty_window();
        window.simulate_mouse_move(point(px(10.), px(10.)), None, Modifiers::default());
        assert!(window.update(|_, cx| is_active(cx)));
        window.executor().advance_clock(Duration::from_secs(5 * 60));
        assert!(!window.update(|_, cx| is_active(cx)));
        window.simulate_keystrokes("a");
        assert!(window.update(|_, cx| is_active(cx)));
    }
}
