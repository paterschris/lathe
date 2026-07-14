use crate::TerminalView;
use gpui::{
    Animation, AnimationExt, AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    Styled, div, pulsating_between,
};
use settings::{AwaitingInputSound, Settings};
use std::time::Duration;
use terminal::{Modes, TaskState, TaskStatus, terminal_settings::TerminalSettings};
use ui::{Color, FluentBuilder, Icon, IconButton, IconName};
use workspace::item::ItemEvent;

pub(super) fn start_idle_timer(terminal_view: &mut TerminalView, cx: &mut Context<TerminalView>) {
    let threshold_secs = TerminalSettings::get_global(cx).awaiting_input_idle_threshold_secs;
    if threshold_secs == 0 {
        return;
    }
    let threshold = Duration::from_secs(threshold_secs);
    let awaiting_sound =
        awaiting_input_sound(TerminalSettings::get_global(cx).sound_on_awaiting_input);
    terminal_view.idle_timer = cx.spawn(async move |this, cx| {
        cx.background_executor().timer(threshold).await;
        this.update(cx, |this, cx| {
            if !this.has_had_input {
                return;
            }
            let terminal = this.terminal.read(cx);
            // The Terminal accessors `is_alternate_screen`, `is_at_prompt`, and
            // `child_exited` were dropped from the terminal crate during the upstream
            // merge, so reconstruct them here from the still-public terminal API. The
            // child-exited check uses the absence of a foreground process id as a proxy
            // because the underlying `child_exited` field has no public accessor.
            let is_alternate_screen = terminal.last_content.mode.contains(Modes::ALT_SCREEN);
            let child_exited = terminal.pid().is_none();
            let at_prompt = match (terminal.pid(), terminal.pid_getter()) {
                (Some(foreground_pid), Some(pid_getter)) => {
                    foreground_pid == pid_getter.fallback_pid()
                }
                _ => false,
            };
            let prompt_kind = terminal.interactive_prompt_kind();

            if prompt_kind.is_some() && !at_prompt && !is_alternate_screen && !child_exited {
                this.awaiting_input = prompt_kind;
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();

                if let Some(sound) = awaiting_sound {
                    audio::Audio::play_sound(sound, cx);
                }
            }
        })
        .ok();
    });
}

pub(super) fn clear(terminal_view: &mut TerminalView, cx: &mut Context<TerminalView>) {
    if terminal_view.awaiting_input.is_some() {
        terminal_view.awaiting_input = None;
        terminal_view.has_had_input = false;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }
}

pub(super) fn tab_icon(
    icon: IconName,
    color: Color,
    is_awaiting_input: bool,
    has_rerun_button: bool,
) -> AnyElement {
    let icon = div()
        .when(has_rerun_button, |this| {
            this.hover(|style| style.invisible().w_0())
        })
        .child(Icon::new(icon).color(color));

    if is_awaiting_input {
        icon.with_animation(
            "tab-awaiting-pulse",
            Animation::new(Duration::from_secs(2))
                .repeat()
                .with_easing(pulsating_between(0.4, 1.0)),
            |element, delta| element.opacity(delta),
        )
        .into_any_element()
    } else {
        icon.into_any_element()
    }
}

pub(super) fn tab_icon_state(
    task: Option<&TaskState>,
    is_awaiting_input: bool,
) -> (IconName, Color, Option<IconButton>) {
    match task {
        Some(task) => match &task.status {
            TaskStatus::Running => {
                let (icon, color) = if is_awaiting_input {
                    (IconName::Return, Color::Accent)
                } else {
                    (IconName::PlayFilled, Color::Disabled)
                };
                (icon, color, TerminalView::rerun_button(task))
            }
            TaskStatus::Unknown => (
                IconName::Warning,
                Color::Warning,
                TerminalView::rerun_button(task),
            ),
            TaskStatus::Completed { success } => {
                let rerun_button = TerminalView::rerun_button(task);

                if *success {
                    (IconName::Check, Color::Success, rerun_button)
                } else {
                    (IconName::XCircle, Color::Error, rerun_button)
                }
            }
        },
        None => {
            if is_awaiting_input {
                (IconName::Return, Color::Accent, None)
            } else {
                (IconName::Terminal, Color::Muted, None)
            }
        }
    }
}

fn awaiting_input_sound(setting: AwaitingInputSound) -> Option<audio::Sound> {
    match setting {
        AwaitingInputSound::Off => None,
        AwaitingInputSound::AgentDone => Some(audio::Sound::AgentDone),
        AwaitingInputSound::Mute => Some(audio::Sound::Mute),
        AwaitingInputSound::Unmute => Some(audio::Sound::Unmute),
        AwaitingInputSound::JoinedCall => Some(audio::Sound::Joined),
        AwaitingInputSound::GuestJoinedCall => Some(audio::Sound::GuestJoined),
        AwaitingInputSound::LeaveCall => Some(audio::Sound::Leave),
        AwaitingInputSound::StartScreenshare => Some(audio::Sound::StartScreenshare),
        AwaitingInputSound::StopScreenshare => Some(audio::Sound::StopScreenshare),
    }
}
