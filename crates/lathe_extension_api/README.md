# lathe_extension_api

Lathe-only APIs for [Lathe](https://github.com/paterschris/lathe) extensions.
Use it together with `zed_extension_api` (0.6 or newer): your extension stays
a normal Zed extension and gains commands, buffer access, buffer events and a
little UI when it runs in Lathe.

```toml
[dependencies]
lathe_extension_api = "0.1"
zed_extension_api = "0.7"
```

```rust
use zed_extension_api as zed;

struct InsertDate;

impl zed::Extension for InsertDate {
    fn new() -> Self {
        lathe_extension_api::register_command("insert-date", "Insert Date", || {
            let Some(buffer) = lathe_extension_api::active_buffer()? else {
                return Ok(());
            };
            buffer.edit(buffer.selections.iter().map(|range| (range.clone(), "2026-11-05".into())))
        })
        .expect("registering insert-date");
        InsertDate
    }
}

zed::register_extension!(InsertDate);
```

## extension.toml

An extension that uses this crate only works in Lathe, and must list the
permissions it needs. Put both keys above the first `[table]`.

```toml
requires = ["lathe"]
lathe_capabilities = ["buffer:read", "buffer:write"]
```

Lathe asks the user to allow these when the extension is installed, and users
can revoke them at any time. A call that needs a capability the extension did
not declare, or the user did not allow, returns an error.

| Capability | Allows |
|---|---|
| `buffer:read` | `active_buffer()`: text, path, language and selections |
| `buffer:write` | `ActiveBuffer::edit`, `ActiveBuffer::select` |
| `events:open` | receiving `BufferEvent::Opened` |
| `events:save` | receiving `BufferEvent::Saved` |
| `events:change` | receiving `BufferEvent::Changed` (after typing pauses) |
| `ui:status` | `set_status_text`, `show_notification` |
| `ui:prompt` | `prompt_input`, `quick_pick` (only while a command runs) |
| `tasks:spawn` | `spawn_task`: run a command in a terminal tab |

## API

- `register_command(id, title, handler)`: call from `zed::Extension::new`.
  Shown in the command palette as `<Extension Name>: <title>`; bind it with
  `["extension::RunCommand", { "extension_id": "...", "command": "..." }]`.
- `active_buffer() -> Option<ActiveBuffer>`: the file the command was run
  from, or the file an event is about. Offsets are UTF-8 byte offsets.
- `ActiveBuffer::edit(edits)`: one undoable change; ranges must not overlap.
  Edits against a snapshot that is no longer current are rejected, so read
  again with `active_buffer()` and retry.
- `ActiveBuffer::select(ranges)`: commands only.
- `on_buffer_event(handler)`: receives the events you declared. Handlers get
  two seconds; a handler that runs longer is stopped, and after three in a
  row the extension's events are paused until it is reinstalled or rebuilt.
- `set_status_text`, `show_notification`, `prompt_input`, `quick_pick`.
- `spawn_task(&TaskSpec)`: runs a command as a Lathe task and returns as soon
  as it starts, so it is safe from event handlers. A task with the same label
  as one still running waits for it, then reuses its terminal.
- `extension_settings_json()`: your extension's section of
  `"extension_settings": { "<id>": ... }`, as JSON. Needs no capability.
  Resolved for the file the command or event is about: a trusted project's
  `.zed/settings.json` overrides the user's settings for its own files;
  untrusted projects' settings are never seen.
- `ActiveBuffer::abs_path` / `worktree_root`: absolute locations for files on
  this machine, for building command lines.

## Views (API 0.2, not released yet)

0.2 adds views: a small UI the extension describes and Lathe draws with its
own components, so it follows the theme. It needs the `ui:views` capability,
and an extension that uses it only loads in a Lathe build with API 0.2.

```rust
use lathe_extension_api::{TextStyle, Tone, ViewBuilder, ViewEvent};

let mut view = ViewBuilder::column();
view.text("Results", TextStyle::Heading)
    .text_input("filter", "", Some("Filter"))
    .checkbox("hidden", "Show hidden", false)
    .scrolling(|list| {
        list.list_item("a", "first.rs", Some("2 KB"))
            .list_item("b", "second.rs", None);
    })
    .badge("2", Tone::Accent);
lathe_extension_api::update_view("results", &view.build())?;

lathe_extension_api::on_view_event(|view_id, element_id, event| {
    match event {
        ViewEvent::Clicked => { /* a button or list item */ }
        ViewEvent::Toggled(on) => { /* a checkbox, or a tree item's arrow */ }
        ViewEvent::InputChanged(text) => { /* sent after typing pauses */ }
        ViewEvent::Submitted(text) => { /* Enter in a text input */ }
        ViewEvent::MenuChosen(entry) => { /* a list item's right-click menu */ }
        ViewEvent::Selected(value) => { /* a dropdown option */ }
    }
    Ok(())
});
```

- Nodes: column, row, scroll, text (with a style and a theme color), button,
  list item (selected, disabled, indent, an expand arrow for trees, and a
  right-click menu via `.menu(...)`), text input, checkbox, select
  (dropdown), progress (bar or spinner), markdown, badge, icon, divider,
  spacer, and virtual list.
- Each update replaces the whole view. `remove_view` forgets it.
- Ids of buttons, list items, inputs, checkboxes, selects and virtual lists
  must be unique within a view. Lathe refuses views that are malformed or
  too big (2,000 nodes, nesting 16 deep, 32 views per extension).
- While the user is typing in an input, a new value from the extension
  doesn't overwrite what they've typed.
- Markdown links open only for `http`, `https` and `mailto`.
- A scroll area scrolls sideways too, so long rows aren't cut off.

### Long lists

A virtual list holds up to 1,000,000 rows without sending them. Lathe asks
for the rows on screen, at most 100 at a time, and keeps them until you
change the list's `revision`:

```rust
view.virtual_list("files", files.len() as u32, revision);

lathe_extension_api::on_provide_rows(|_view_id, _list_id, start, count| {
    Ok((start..(start + count).min(total))
        .map(|index| lathe_extension_api::list_row(&format!("file-{index}"), "name.rs", None))
        .collect())
});
```

Row events use the row's id as the element id, so keep row ids distinct
from the view's other ids.

### Where UI appears

A CSV preview, start to finish:

```rust
lathe_extension_api::register_preview("csv", "CSV Preview", &[".csv"])?;

fn draw(view_id: &str) -> lathe_extension_api::Result<()> {
    let Some(buffer) = lathe_extension_api::active_buffer()? else { return Ok(()) };
    let mut view = ViewBuilder::column();
    view.text(&format!("{} rows", buffer.text.lines().count()), TextStyle::Heading);
    lathe_extension_api::update_view(view_id, &view.build())
}

lathe_extension_api::on_surface_event(|event| match event {
    SurfaceEvent::PreviewOpened(file) => draw(&file.view_id),
    SurfaceEvent::PreviewChanged(view_id) => draw(view_id),
    _ => Ok(()),
});
```

- `register_preview(id, title, suffixes)` adds a file preview. Users open
  it from the button in the editor's toolbar or the command palette
  ("<Extension Name>: <title>") on a matching file, and it shows in a tab
  beside the editor, which stays open for editing. Each preview arrives as
  `SurfaceEvent::PreviewOpened` with a view id to fill in. Edits to the file
  arrive as `PreviewChanged` once typing pauses (your own edits don't). In
  both, `active_buffer()` returns the file (with `buffer:read`). Closing the
  tab sends `PreviewClosed` and Lathe drops the view. Open previews come back
  after a restart, once your extension has loaded.
- `register_panel(id, title, icon, dock)` adds a dock panel showing the
  view with the same id. Each dock side has one Extension Panels button;
  extension panels on that side are tabs inside it. Users open yours from
  that button or the palette ("<Extension Name>: Toggle <title>"). Its view
  gets `ViewShown` when the panel opens and `ViewHidden` when it closes or
  another tab is picked, so you can skip work while nobody's looking.
- Both are called from `zed::Extension::new`. Registering always works;
  they only appear while the user allows `ui:views`.
- `set_status_item(Some(&StatusItem { .. }))` puts a clickable item in the
  status bar (needs `ui:status`). Clicks arrive as
  `SurfaceEvent::StatusItemClicked`.
- `on_surface_event` also gets `ViewShown` and `ViewHidden`, so you can do
  work only while someone is looking.
- `on_file_event` hears files come and go, at two levels (needs
  `events:open`):
  - **Tabs:** `TabOpened` when the first editor tab showing a file appears,
    `TabClosed` when the last one closes. Moving a tab sends neither. Use
    these to follow what the user has open; during `TabOpened`,
    `active_buffer()` returns the file.
  - **Buffers:** `BufferClosed` is the other end of `BufferEvent::Opened`.
    Lathe also loads files nobody has a tab for (go to definition, search,
    language servers) and can keep one loaded after its tabs close.
  - When your extension loads, it gets `Opened` and `TabOpened` for what's
    already open, so lists built from them start out complete.
- `open_location(path, line, column)` opens a file (absolute, or relative to
  a folder in the window) with 1-based line and column. It only works while
  handling something the user did: a command, a view event or a status item
  click.

## Decorations (API 0.2, not released yet)

Highlights, underlines, gutter marks and short end-of-line messages in the
editor. They're anchored to the text, so they move with edits, and every
editor showing the file draws them. Needs `ui:decorations` (and `buffer:read`
to get the buffer).

```rust
use lathe_extension_api::{DecorationStyle, Tone, decoration};

let Some(buffer) = lathe_extension_api::active_buffer()? else { return Ok(()) };
let todos = buffer
    .text
    .match_indices("TODO")
    .map(|(start, found)| {
        decoration(start..start + found.len(), DecorationStyle::WavyUnderline, Tone::Warning, Some("todo"))
    })
    .collect::<Vec<_>>();
buffer.set_decorations("todos", &todos)?;
```

Each call replaces the layer you name; an empty list clears it. Limits:
5,000 decorations per layer, 8 layers per file, 200-character messages.
Decorating doesn't use up the snapshot, so you can still edit afterwards.

## Testing

The wasm exports are only compiled for `wasm32`, so `cargo test` runs your
extension's own unit tests natively. Build and package the extension with
Lathe's extension CLI (`zed-extension`, package `extension_cli`).

## Sharing your extension

Three ways, from least to most setup:

- **Archive URL.** Package it with `zed-extension`, host the `.tar.gz`
  anywhere, and send people the link. They paste it into the search box on
  Lathe's Extensions page.
- **Git URL.** People paste your repository's URL into the same box. Lathe
  clones and builds it, which needs Rust on their machine.
- **The Lathe registry.** Open a pull request to
  [lathe-extensions](https://github.com/paterschris/lathe-extensions). The
  maintainer reviews it, and once it's merged your extension shows up on
  everyone's Extensions page. The steps are in its
  [CONTRIBUTING.md](https://github.com/paterschris/lathe-extensions/blob/main/CONTRIBUTING.md).

Every way asks the user to allow your `lathe_capabilities` before installing.
