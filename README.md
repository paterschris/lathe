# Lathe

A customized code editor forked from [Zed](https://zed.dev), focused on better terminal workflows, visual git integration, and deep theme customization.

![Theme Customizer](assets/screenshots/theme-customizer.png)

## Features (beyond upstream Zed)

### Terminal awaiting-input indicator
Shows a return icon in the terminal tab and title bar when Claude Code or other interactive prompts are waiting for input.

![Awaiting Input Indicator](assets/screenshots/awaiting-input.png)

### Git-aware tab and panel styling
Tabs and project panel entries are color-coded by git status — modified, created, deleted, conflict, error, and warning states each get distinct colors.

![Git Tab Styling](assets/screenshots/git-tabs.png)

### Theme Customizer
A built-in panel for editing all 135+ theme colors with HSLA sliders and live preview. Includes category filters, Lathe-specific color badges, and per-color reset. Open via the command palette: `theme customizer: Open`.

![Theme Customizer Panel](assets/screenshots/theme-customizer-panel.png)

### Other enhancements
- **Active terminal tab tint** — Terminal tabs get a subtle green background when active
- **Terminal focus fix** — Ctrl+tab to a terminal tab properly activates the cursor
- **Custom theme and syntax highlighting** — Tuned color palette

## Install

### Homebrew (recommended)

```sh
brew install --cask --no-quarantine paterschris/lathe/lathe
```

### Manual download

Download the latest `.zip` from [Releases](https://github.com/paterschris/lathe/releases), unzip, and drag **Lathe.app** to `/Applications`.

Currently only **Apple Silicon (aarch64)** macOS builds are provided.

### Build from source

```sh
git clone git@github.com:paterschris/lathe.git
cd lathe
script/build-fork      # ~10-15 min first time
script/install-fork    # copies to /Applications
```

Installs as **Lathe.app** and runs alongside stock Zed without conflicts.

## Updating

### Homebrew

```sh
brew upgrade lathe
```

### Manual / source

```sh
git pull
script/build-fork
script/install-fork
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
- The app shares settings and extensions with stock Zed (`~/Library/Application Support/Zed`)
- `cargo-bundle` is installed automatically from [zed-industries/cargo-bundle](https://github.com/zed-industries/cargo-bundle)
