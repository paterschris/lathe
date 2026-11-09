use std::cell::Cell;
use std::rc::Rc;

use zed_extension_api as zed;

struct TestLatheExtension;

impl zed::Extension for TestLatheExtension {
    fn new() -> Self {
        let runs = Rc::new(Cell::new(0_u32));
        lathe_extension_api::register_command("count-runs", "Count Runs", {
            move || {
                runs.set(runs.get() + 1);
                if runs.get() > 1 {
                    return Err(format!("ran {} times", runs.get()));
                }
                Ok(())
            }
        })
        .expect("registering count-runs");
        lathe_extension_api::register_command("always-fails", "Always Fails", || {
            Err("this command always fails".to_string())
        })
        .expect("registering always-fails");
        lathe_extension_api::register_command("upcase-selections", "Upcase Selections", || {
            let Some(buffer) = lathe_extension_api::active_buffer()? else {
                return Err("no active buffer".to_string());
            };
            let mut selections = buffer.selections.clone();
            selections.sort_by_key(|range| range.start);
            let edits = selections
                .iter()
                .map(|range| (range.clone(), buffer.text[range.clone()].to_uppercase()))
                .collect::<Vec<_>>();
            buffer.edit(edits)?;

            let buffer = lathe_extension_api::active_buffer()?
                .ok_or_else(|| "buffer disappeared".to_string())?;
            let stale = buffer.clone();
            buffer.edit([(0..0, ">".to_string())])?;
            if stale.edit([(0..0, "!".to_string())]).is_ok() {
                return Err("a stale snapshot was allowed to edit".to_string());
            }
            Ok(())
        })
        .expect("registering upcase-selections");

        lathe_extension_api::register_command("ask", "Ask", || {
            let name = lathe_extension_api::prompt_input("Name", Some("your name"))?
                .unwrap_or_else(|| "nobody".to_string());
            let color = lathe_extension_api::quick_pick(
                "Color",
                &[
                    lathe_extension_api::PickItem {
                        id: "red".into(),
                        label: "Red".into(),
                        detail: None,
                    },
                    lathe_extension_api::PickItem {
                        id: "blue".into(),
                        label: "Blue".into(),
                        detail: Some("the calm one".into()),
                    },
                ],
            )?
            .unwrap_or_else(|| "none".to_string());
            lathe_extension_api::show_notification(
                lathe_extension_api::NotificationLevel::Info,
                &format!("{name} likes {color}"),
            )
        })
        .expect("registering ask");

        lathe_extension_api::register_command("run-task", "Run Task", || {
            let buffer = lathe_extension_api::active_buffer()?
                .ok_or_else(|| "no active buffer".to_string())?;
            let settings = lathe_extension_api::extension_settings_json()?
                .unwrap_or_else(|| "none".to_string());
            lathe_extension_api::spawn_task(&lathe_extension_api::TaskSpec {
                label: "echo".into(),
                command: "echo".into(),
                args: vec![
                    buffer.abs_path.unwrap_or_default(),
                    buffer.worktree_root.unwrap_or_default(),
                    settings,
                ],
                cwd: None,
                env: vec![("FROM".into(), "lathe".into())],
                reveal: lathe_extension_api::TaskReveal::NoFocus,
            })?;
            // A relative cwd is refused.
            match lathe_extension_api::spawn_task(&lathe_extension_api::TaskSpec {
                label: String::new(),
                command: "pwd".into(),
                args: vec![],
                cwd: Some("relative/dir".into()),
                env: vec![],
                reveal: lathe_extension_api::TaskReveal::Never,
            }) {
                Ok(()) => Err("a relative cwd was accepted".to_string()),
                Err(_) => Ok(()),
            }
        })
        .expect("registering run-task");

        lathe_extension_api::on_buffer_event(|event| {
            let buffer = lathe_extension_api::active_buffer()?
                .ok_or_else(|| "no buffer for event".to_string())?;
            match event {
                lathe_extension_api::BufferEvent::Saved => {
                    let words = buffer.text.split_whitespace().count();
                    lathe_extension_api::set_status_text(
                        Some(&format!("{words} words")),
                        buffer.path.as_deref(),
                    )
                }
                lathe_extension_api::BufferEvent::Changed => {
                    if buffer.text == "hang" {
                        // Never returns; the host must interrupt it.
                        loop {
                            std::hint::spin_loop();
                        }
                    }
                    // Prompts must be refused outside commands; report what happened.
                    let outcome = match lathe_extension_api::prompt_input("x", None) {
                        Ok(_) => "prompt allowed".to_string(),
                        Err(error) => error,
                    };
                    lathe_extension_api::set_status_text(Some(&outcome), None)
                }
                lathe_extension_api::BufferEvent::Opened => Ok(()),
            }
        });
        TestLatheExtension
    }
}

zed::register_extension!(TestLatheExtension);
