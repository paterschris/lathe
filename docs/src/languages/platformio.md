# PlatformIO

PlatformIO support is built into Lathe. Open a folder containing a `platformio.ini` and you get:

- Syntax highlighting and an outline for `platformio.ini`.
- Build, upload, monitor, test, check and clean tasks for every environment declared in the file.
- Code intelligence for your firmware sources through [clangd](./cpp.md), using the compilation database PlatformIO generates.

Lathe drives the [PlatformIO Core CLI](https://docs.platformio.org/en/latest/core/userguide/index.html) (`pio`). It does not bundle it, so install PlatformIO Core first.

## Tasks

Open the task picker (`task::Spawn`) from any C, C++ or `platformio.ini` buffer inside the project. Lathe reads `platformio.ini`, finds the `[env:*]` sections, and offers per-environment tasks:

| Task | Command |
| --- | --- |
| Build | `pio run -e <env>` |
| Upload | `pio run -t upload -e <env>` |
| Upload and Monitor | `pio run -t upload -t monitor -e <env>` |
| Monitor | `pio device monitor -e <env>` |
| Test | `pio test -e <env>` |
| Check | `pio check -e <env>` |
| Clean | `pio run -t clean -e <env>` |

Plus a few project-wide entries: `Build (all environments)`, `Clean (all environments)`, `Rebuild IntelliSense index` and `List devices`.

Environments named in `[platformio] default_envs` are listed first. Tasks run with the directory containing `platformio.ini` as their working directory, so they work the same whether that directory is your worktree root or a subfolder of a larger repository.

`Monitor` opens its own terminal tab, since the serial monitor is interactive and stays open. The rest reuse the task terminal.

## Code intelligence

clangd needs a `compile_commands.json` describing the include paths, defines and compiler flags for your board. PlatformIO generates it:

```sh
pio run -t compiledb
```

Run the **PlatformIO: Rebuild IntelliSense index** task to do this from the editor. The file lands in the project root, where clangd finds it without extra configuration.

Re-run it after you change `board`, `framework`, `build_flags` or `lib_deps` in `platformio.ini`, or after adding a library. Until you do, clangd is working from the previous build configuration and may report includes it cannot resolve.

If clangd still cannot find your headers, check that the build actually succeeded at least once. `compiledb` describes the build PlatformIO would run, so it needs the platform and toolchain packages already installed.

## Configuration

### Locating the `pio` executable

Lathe looks for `pio` in this order:

1. The `PLATFORMIO_PATH` task variable, if you set one.
2. `~/.platformio/penv/bin/pio` (`~/.platformio/penv/Scripts/pio.exe` on Windows), which is where PlatformIO's own installer puts it.
3. `pio` on `$PATH`.

The second entry covers the common case where PlatformIO was installed by its installer script or by the VS Code extension and never added to `$PATH`. To point at a different installation, set the variable in your settings:

```json
{
  "languages": {
    "C++": {
      "tasks": {
        "variables": {
          "PLATFORMIO_PATH": "/opt/platformio/penv/bin/pio"
        }
      }
    }
  }
}
```

### Hiding the tasks

The PlatformIO tasks come from the language task provider, so they follow the usual setting:

```json
{
  "languages": {
    "C++": {
      "tasks": {
        "enabled": false
      }
    }
  }
}
```

## Debugging

Lathe does not yet wire `pio debug` up to its debugger. You can attach manually with the GDB adapter, pointing it at the toolchain GDB and the `.elf` that `pio project metadata --json-output` reports for your environment.
