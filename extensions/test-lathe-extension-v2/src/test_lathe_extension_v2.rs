use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use lathe_extension_api::{
    DecorationStyle, DockSide, StatusItem, SurfaceEvent, TextStyle, Tone, ViewBuilder, ViewEvent,
};

const FILE_COUNT: u32 = 250;
use zed_extension_api as zed;

struct TestLatheExtensionV2;

#[derive(Default)]
struct State {
    count: u32,
    filter: String,
    submitted: Option<String>,
    show_hidden: bool,
    menu_choice: Option<String>,
    sort: Option<String>,
    shown: bool,
    /// What happened when the extension tried to open a file from a
    /// `view-shown` event, which is not something the user did.
    open_from_event: Option<String>,
}

fn counter_view(state: &State) -> lathe_extension_api::View {
    let mut view = ViewBuilder::column();
    view.text("Counter", TextStyle::Heading)
        .text(&format!("Clicked {} times", state.count), TextStyle::Body)
        .nested(true, |row| {
            row.button("increment", "Add One", Some("plus"))
                .button("open", "Open main.rs", None)
                .spacer()
                .badge(&state.count.to_string(), Tone::Accent);
        })
        .text_input("filter", &state.filter, Some("Filter"))
        .checkbox("hidden", "Show hidden", state.show_hidden)
        .list_item("todo", "A TODO", None)
        .menu(&[("done", "Mark Done"), ("skip", "Skip")])
        .select(
            "sort",
            &[("name", "Name"), ("size", "Size")],
            state.sort.as_deref(),
        )
        .progress(None, Some("Working"))
        .virtual_list("files", FILE_COUNT, 0)
        .markdown("**Bold** and [a link](https://lathe-ide.com)")
        .toned_text(
            &format!(
                "filter={} submitted={} hidden={} menu={} sort={} shown={} open_from_event={}",
                state.filter,
                state.submitted.as_deref().unwrap_or("-"),
                state.show_hidden,
                state.menu_choice.as_deref().unwrap_or("-"),
                state.sort.as_deref().unwrap_or("-"),
                state.shown,
                state.open_from_event.as_deref().unwrap_or("-"),
            ),
            TextStyle::Muted,
            Tone::Neutral,
        );
    view.build()
}

/// A preview of the file being opened or changed, which `active_buffer()`
/// returns during those events.
fn preview_view(view_id: &str, title: &str) -> lathe_extension_api::Result<()> {
    let rows = lathe_extension_api::active_buffer()?
        .map(|buffer| buffer.text.lines().count().to_string())
        .unwrap_or_else(|| "-".into());
    let mut view = ViewBuilder::column();
    view.text(&format!("{title} {rows} rows"), TextStyle::Body);
    lathe_extension_api::update_view(view_id, &view.build())
}

impl zed::Extension for TestLatheExtensionV2 {
    fn new() -> Self {
        let state = Rc::new(RefCell::new(State::default()));

        lathe_extension_api::register_panel("counter", "Counter", Some("plus"), DockSide::Right)
            .expect("registering the counter panel");
        lathe_extension_api::register_preview("csv", "CSV Preview", &[".csv", ".tsv"])
            .expect("registering the csv preview");

        // A 0.1 API call from a 0.2 extension.
        lathe_extension_api::register_command("show-counter", "Show Counter", {
            let state = state.clone();
            move || {
                lathe_extension_api::set_status_item(Some(&StatusItem {
                    label: "Counter".into(),
                    icon: Some("plus".into()),
                    tooltip: Some("Open the counter".into()),
                    tone: Tone::Accent,
                }))?;
                lathe_extension_api::update_view("counter", &counter_view(&state.borrow()))
            }
        })
        .expect("registering show-counter");

        lathe_extension_api::register_command("hide-counter", "Hide Counter", || {
            lathe_extension_api::set_status_item(None)?;
            lathe_extension_api::remove_view("counter")?;
            // Removing again is not an error.
            lathe_extension_api::remove_view("counter")
        })
        .expect("registering hide-counter");

        lathe_extension_api::register_command("decorate", "Decorate TODOs", || {
            let Some(buffer) = lathe_extension_api::active_buffer()? else {
                return Err("decorations need a buffer".into());
            };
            let decorations = buffer
                .text
                .match_indices("TODO")
                .map(|(start, found)| {
                    lathe_extension_api::decoration(
                        start..start + found.len(),
                        DecorationStyle::WavyUnderline,
                        Tone::Warning,
                        Some("todo"),
                    )
                })
                .collect::<Vec<_>>();
            buffer.set_decorations("todos", &decorations)
        })
        .expect("registering decorate");

        lathe_extension_api::on_provide_rows(|view_id, list_id, start, count| {
            if (view_id, list_id) != ("counter", "files") {
                return Err(format!("unexpected list {view_id}/{list_id}"));
            }
            // Asked for one row: send two, which Lathe must refuse.
            if count == 1 {
                return Ok(vec![
                    lathe_extension_api::list_row("extra-a", "a", None),
                    lathe_extension_api::list_row("extra-b", "b", None),
                ]);
            }
            Ok((start..(start + count).min(FILE_COUNT))
                .map(|index| {
                    lathe_extension_api::list_row(
                        &format!("file-{index}"),
                        &format!("file {index}.rs"),
                        None,
                    )
                })
                .collect())
        });

        lathe_extension_api::register_command("bad-view", "Bad View", || {
            // Node 1 lists node 0 as its child: a cycle the host must refuse.
            let mut view = ViewBuilder::column();
            view.nested(false, |_| {});
            let mut view = view.build();
            view.nodes[1].children.push(0);
            lathe_extension_api::update_view("broken", &view)
        })
        .expect("registering bad-view");

        let previews = Rc::new(RefCell::new(HashMap::<String, String>::new()));
        lathe_extension_api::on_surface_event({
            let state = state.clone();
            move |event| {
                match event {
                    SurfaceEvent::ViewShown(view_id) if view_id == "counter" => {
                        let mut state = state.borrow_mut();
                        state.shown = true;
                        state.open_from_event =
                            Some(match lathe_extension_api::open_location("x", None, None) {
                                Ok(()) => "allowed".into(),
                                Err(_) => "refused".into(),
                            });
                    }
                    SurfaceEvent::ViewHidden(view_id) if view_id == "counter" => {
                        state.borrow_mut().shown = false;
                    }
                    SurfaceEvent::StatusItemClicked => {
                        return lathe_extension_api::open_location("src/main.rs", Some(3), Some(2));
                    }
                    SurfaceEvent::PreviewOpened(file) => {
                        let title = format!(
                            "{} {}",
                            file.preview_id,
                            file.path.as_deref().unwrap_or("-")
                        );
                        previews
                            .borrow_mut()
                            .insert(file.view_id.clone(), title.clone());
                        return preview_view(&file.view_id, &title);
                    }
                    SurfaceEvent::PreviewChanged(view_id) => {
                        let Some(title) = previews.borrow().get(view_id).cloned() else {
                            return Err(format!("change for unknown preview {view_id}"));
                        };
                        return preview_view(view_id, &title);
                    }
                    // Lathe has already dropped the preview's view.
                    SurfaceEvent::PreviewClosed(view_id) => {
                        previews.borrow_mut().remove(view_id);
                        return Ok(());
                    }
                    other => return Err(format!("unexpected surface event {other:?}")),
                }
                lathe_extension_api::update_view("counter", &counter_view(&state.borrow()))
            }
        });

        lathe_extension_api::on_view_event(move |view_id, element_id, event| {
            if view_id != "counter" {
                return Err(format!("unexpected view {view_id}"));
            }
            {
                let mut state = state.borrow_mut();
                match (element_id, event) {
                    ("increment", ViewEvent::Clicked) => state.count += 1,
                    ("open", ViewEvent::Clicked) => {
                        lathe_extension_api::open_location("/abs/notes.md", None, None)?
                    }
                    ("filter", ViewEvent::InputChanged(text)) => state.filter = text.clone(),
                    ("filter", ViewEvent::Submitted(text)) => state.submitted = Some(text.clone()),
                    ("hidden", ViewEvent::Toggled(on)) => state.show_hidden = *on,
                    ("todo", ViewEvent::MenuChosen(entry)) => {
                        state.menu_choice = Some(entry.clone())
                    }
                    ("sort", ViewEvent::Selected(value)) => state.sort = Some(value.clone()),
                    _ => return Err(format!("unexpected event {element_id}: {event:?}")),
                }
            }
            lathe_extension_api::update_view("counter", &counter_view(&state.borrow()))
        });
        TestLatheExtensionV2
    }
}

zed::register_extension!(TestLatheExtensionV2);
