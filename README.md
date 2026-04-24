# Lathe

A customized code editor forked from [Zed](https://zed.dev), focused on better terminal workflows, visual git integration, and deep theme customization.

Lathe is a personal fork of Zed. I maintain it so I can ship small editor tweaks for my own workflow without waiting on upstream review, and without needing each change to fit Zed's product scope. Upstream Zed is the primary project — this fork tracks it closely and layers on my own changes.

**Platforms:** macOS (Apple Silicon) and Linux (x86_64). Windows support is planned — upstream Zed builds on Windows, so it should be a manageable effort, but Lathe isn't yet built or tested there.

**Stability:** Lathe is maintained for my own daily use. Upstream syncs can occasionally introduce breakage; bug reports are welcome.

## Features

### Custom theme and syntax highlighting
Lathe ships with its own default theme and a refined syntax highlighting palette applied across all supported languages. The theme is tuned for long coding sessions — balanced contrast, distinct-but-not-loud accent colors for keywords, strings, and types, and deliberate choices for diagnostic and git-status colors so the editor stays readable when things go wrong. Pair it with the Theme Customizer (below) to tweak any of the 135+ color tokens to taste.

![Default Theme](assets/screenshots/default-theme-code.png)


### Theme Customizer
A built-in panel for editing all 135+ theme colors with HSLA sliders and live preview. Includes category filters, Lathe-specific color badges, and per-color reset. Open via the command palette: `theme customizer: Open`.

![Command Palette](assets/screenshots/theme-customizer-command-palette.png)

![Theme Customizer](assets/screenshots/theme-customizer.gif)

### Git-aware tab and panel styling
Tabs and project panel entries are color-coded by git status — modified, created, deleted, conflict, error, and warning states each get distinct colors.

![Git Tab Styling](assets/screenshots/git-aware-editor-tabs.png)

### Terminal awaiting-input indicator
Shows a return icon in the terminal tab and title bar when Claude Code or other interactive prompts are waiting for input.

![Awaiting Input Indicator](assets/screenshots/awaiting-input-indicator.gif)

### Active terminal tab tint
Terminal tabs get a subtle green background when active, making them easy to spot among editor tabs.

### Terminal focus fix
Switching to a terminal tab via ctrl+tab properly activates the cursor without needing to click into the terminal.

### Multi-account collab switcher
Sign into more than one Zed Cloud account and switch between them from the avatar menu. Saved accounts are listed under **Accounts** by their GitHub username, with **Add Account…** and **Sign Out** actions.

### Copy collab link dialog
When generating a shareable collab link, a dialog lets you pick which saved account to link from — useful when you work across personal and work Zed Cloud accounts.

### Workspace groups with account binding
Save the set of currently open projects as a named **workspace group**, then reopen the whole group in a new window later. Each group can optionally be bound to a saved collab account, so opening the group automatically switches to that account first.

Commands (via the command palette):

- `workspace groups: Save Workspace Group`
- `workspace groups: Open Workspace Group`
- `workspace groups: Update Current Workspace Group`
- `workspace groups: Rename Current Workspace Group`
- `workspace groups: Bind Workspace Group Account`
- `workspace groups: Unbind Workspace Group Account`

## Install

### Homebrew (recommended)

```sh
brew tap paterschris/tap
brew install --cask lathe
```

### Manual download

Download the latest release from [Releases](https://github.com/paterschris/lathe/releases):

- **macOS**: Download the `.dmg`, open it, and drag **Lathe.app** to `/Applications`. A `.zip` is also available if you prefer. macOS builds are code-signed and notarized by Apple.
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

To run the build without installing to `/Applications`, launch the bundle directly:

```sh
open target/release/bundle/osx/Lathe.app
```

## Release channels

Lathe ships on two channels:

- **Stable** — tagged `vX.Y.Z`, the recommended build for daily use.
- **Beta** — tagged `vX.Y.Z-beta`, published as GitHub prereleases with a distinct app icon. Beta builds typically contain the latest upstream Zed sync before it's rolled into stable.

Homebrew installs stable by default. To try a beta, grab the `-beta` asset from [Releases](https://github.com/paterschris/lathe/releases).

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

**Last synced with upstream Zed: 2026-04-22.**

## License

Lathe inherits its licensing from upstream Zed:

- The application is licensed under the [GNU Affero General Public License v3.0](LICENSE-AGPL).
- `gpui` and several foundational crates are licensed under the [GNU General Public License v3.0](LICENSE-GPL).
- Other components are licensed under the [Apache License 2.0](LICENSE-APACHE).

All upstream license terms are preserved. See the individual `LICENSE-*` files at the repo root.

## Contributing

Lathe is primarily a personal fork, but I want to preserve the open-source feel of Zed. If you hit a bug, want a tweak, or have an idea that fits the spirit of the fork, feel free to open an issue or PR. See [CONTRIBUTING.md](CONTRIBUTING.md) for the inherited Zed guidelines; Lathe-specific conventions live in [CLAUDE.md](CLAUDE.md) and `.rules`.

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
