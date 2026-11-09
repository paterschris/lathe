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

## Testing

The wasm exports are only compiled for `wasm32`, so `cargo test` runs your
extension's own unit tests natively. Build and package the extension with
Lathe's extension CLI (`zed-extension`, package `extension_cli`).
