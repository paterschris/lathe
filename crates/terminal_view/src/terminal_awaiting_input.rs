use crate::TerminalView;
use gpui::Context;
use settings::{AwaitingInputSound, Settings};
use std::time::Duration;
use terminal::{Modes, terminal_settings::TerminalSettings};
use workspace::item::ItemEvent;

pub(super) fn start_idle_timer(
    terminal_view: &mut TerminalView,
    cx: &mut Context<TerminalView>,
) {
    let threshold_secs = TerminalSettings::get_global(cx).awaiting_input_idle_threshold_secs;
    if threshold_secs == 0 {
        return;
    }
    let threshold = Duration::from_secs(threshold_secs);
    let awaiting_sound = awaiting_input_sound(TerminalSettings::get_global(cx).sound_on_awaiting_input);
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
