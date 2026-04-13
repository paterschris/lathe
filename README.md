# Lathe

A customized code editor forked from [Zed](https://zed.dev), focused on better terminal workflows, visual git integration, and deep theme customization.

## Features

### Terminal awaiting-input indicator
Shows a return icon in the terminal tab and title bar when Claude Code or other interactive prompts are waiting for input.

![Awaiting Input Indicator](assets/screenshots/awaiting-input-indicator.gif)

### Git-aware tab and panel styling
Tabs and project panel entries are color-coded by git status — modified, created, deleted, conflict, error, and warning states each get distinct colors.

![Git Tab Styling](assets/screenshots/git-aware-editor-tabs.png)

### Theme Customizer
A built-in panel for editing all 135+ theme colors with HSLA sliders and live preview. Includes category filters, Lathe-specific color badges, and per-color reset. Open via the command palette: `theme customizer: Open`.

![Command Palette](assets/screenshots/theme-customizer-command-pallette.png)

![Theme Customizer](assets/screenshots/theme-customizer.gif)

### Active terminal tab tint
Terminal tabs get a subtle green background when active, making them easy to spot among editor tabs.

### Terminal focus fix
Switching to a terminal tab via ctrl+tab properly activates the cursor without needing to click into the terminal.

## Install

### Homebrew (recommended)

```sh
brew tap paterschris/tap
brew install --cask lathe
```

### Manual download

Download the latest release from [Releases](https://github.com/paterschris/lathe/releases):

- **macOS**: Download the `.zip`, unzip, and drag **Lathe.app** to `/Applications`.
- **Linux**: Download the `.tar.gz` and extract it, or use the install script after building from source (see below).

### Build from source

**macOS:**

```sh
git clone git@github.com:paterschris/lathe.git
cd lathe
script/build-fork      # ~10-15 min first time
script/install-fork    # copies to /Applications
```

**Linux:**

```sh
git clone git@github.com:paterschris/lathe.git
cd lathe
script/build-fork-linux      # installs system deps, builds
script/package-fork-linux    # creates .tar.gz
script/install-fork-linux    # installs to ~/.local/share/lathe, symlinks CLI to ~/.local/bin
```

Installs as **Lathe** and runs alongside stock Zed without conflicts.

## Updating

### Homebrew

```sh
brew upgrade lathe
```

### Manual / source (macOS)

```sh
git pull
script/build-fork
script/install-fork
```

### Manual / source (Linux)

```sh
git pull
script/build-fork-linux
script/package-fork-linux
script/install-fork-linux
```

## Relationship to Zed

Lathe periodically merges from [upstream Zed](https://github.com/zed-industries/zed) to stay current with new features and fixes. Custom changes are kept in separate commits to make merges straightforward.

## Releasing

```sh
script/release-fork
```

Builds, packages, and publishes a GitHub release. Requires the [GitHub CLI](https://cli.github.com/).

## Notes

- First builds take significantly longer than incremental rebuilds
- On macOS, the app shares settings and extensions with stock Zed (`~/Library/Application Support/Zed`)
- On Linux, the app installs to `~/.local/share/lathe` with the CLI symlinked to `~/.local/bin/lathe`
- `cargo-bundle` is installed automatically from [zed-industries/cargo-bundle](https://github.com/zed-industries/cargo-bundle)
