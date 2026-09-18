# Lathe

A customized code editor forked from [Zed](https://zed.dev), focused on mobile development workflows, deeper git tooling, in-editor code review, and theme customization.

### [Download Lathe](https://github.com/paterschris/lathe/releases/latest)

The latest release for macOS, Linux, and Windows. On macOS you can also install it with Homebrew:

```sh
brew install --cask paterschris/tap/lathe
```

Per-platform notes, including Windows, are in [Install](#install).

Lathe is a personal fork of Zed. I maintain it so I can ship small editor tweaks for my own workflow without waiting on upstream review, and without needing each change to fit Zed's product scope. Upstream Zed is the primary project - this fork tracks it closely and layers on my own changes.

**Platforms:** macOS (Apple Silicon), Linux (x86_64 and arm64), and Windows (x86_64 and arm64; experimental).

**Stability:** Lathe is maintained for my own daily use. Upstream syncs can occasionally introduce breakage; bug reports are welcome.

## Features

Ordered by how much each one differentiates Lathe from stock Zed. Upstream already ships a commit graph, a tabbed git panel, worktree support, and single-file history; the git section below covers what Lathe adds on top of those rather than restating them.

1. [Mobile development](docs/features.md#mobile-development-expo--react-native) - Expo and bare React Native panel
2. [Merge conflicts and interactive rebase](docs/features.md#merge-conflicts-and-interactive-rebase) - conflict resolution tab, full-file split view, drag-and-drop rebase
3. [Pull request reviews](docs/features.md#pull-request-reviews) - GitHub, GitLab, and Bitbucket, in-editor
4. [Code navigation](docs/features.md#code-navigation) - peek view for definitions and references
5. [AI agent integration](docs/features.md#ai-agent-integration) - multi-account sign-in, approval control, per-workspace thread history
6. [Theme and syntax highlighting](docs/features.md#theme-and-syntax-highlighting) - custom theme, live 200+ color customizer
7. [Git additions](docs/features.md#git-additions) - explorer tab, branch tree, undo, Git Flow
8. [AWS profiles](docs/features.md#aws-profiles) - per-window profile selector
9. [Terminal, windows, and workspaces](docs/features.md#terminal-windows-and-workspaces) - awaiting-input indicator, workspace groups, per-window zoom

Each of these is described in full, with screenshots, in **[docs/features.md](docs/features.md)**.

## Install

### Homebrew (recommended)

```sh
brew tap paterschris/tap
brew install --cask lathe
```

### Manual download

Download the latest release from [Releases](https://github.com/paterschris/lathe/releases):

- **macOS**: Download the `.dmg`, open it, and drag **Lathe.app** to `/Applications`. A `.zip` is also available if you prefer. macOS builds are code-signed and notarized by Apple.
- **Linux**: Download the `.tar.gz` and extract it, or use the install script after building from source (see below). Like upstream Zed, the editor needs the host's ALSA runtime (`libasound2` on Debian/Ubuntu, `alsa-lib` on Fedora/Arch) and working Vulkan drivers; both are preinstalled on typical desktop distros.
- **Windows**: Download the setup `.exe` or `.zip` for your architecture (x86_64 or arm64). Windows builds are currently unsigned; see [Installing on Windows](#installing-on-windows).

### Installing on Windows

The setup `.exe` is the simplest option. Because Lathe's Windows builds are unsigned, Microsoft Defender SmartScreen may show a warning. Select **More info**, verify that the file came from the Lathe GitHub release, then select **Run anyway**.

For the portable path, download the x86_64 `.zip`, open PowerShell in a Lathe source checkout, and run:

```powershell
script/install-fork-windows.ps1 -ArchivePath C:\path\to\Lathe-version-x86_64-windows.zip
```

The install script removes Mark-of-the-Web from the extracted files, installs Lathe under `%LOCALAPPDATA%\Programs\Lathe`, adds its CLI to your user `PATH`, and creates a Start Menu shortcut. If Lathe installs but no window appears, run `script/diag-windows.ps1` from the source checkout and include its output in a bug report.

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

**Windows:**

```powershell
git clone https://github.com/paterschris/lathe.git
cd lathe
script/build-fork-windows.ps1 -Architecture x86_64
script/package-fork-windows.ps1 -Architecture x86_64
script/install-fork-windows.ps1
```

Installs as **Lathe** and runs alongside stock Zed without conflicts.

To run the build without installing to `/Applications`, launch the bundle directly:

```sh
open target/release/bundle/osx/Lathe.app
```

## Release channels

Lathe ships on two channels:

- **Stable** - tagged `vX.Y.Z`, the recommended build for daily use.
- **Beta** - tagged `vX.Y.Z-beta`, published as GitHub prereleases with a distinct app icon. Beta builds typically contain the latest upstream Zed sync before it's rolled into stable.

Homebrew installs stable by default. To try a beta, grab the `-beta` asset from [Releases](https://github.com/paterschris/lathe/releases).

To be notified when a new version ships, use **Watch > Custom > Releases** at the top of this repository. Starring bookmarks the project but does not send release notifications.

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

**Last synced with upstream Zed: 2026-07-14.**

> **2026-04-24:** `main` was rewritten to fix a long-standing ancestry tangle that made the fork display as ~37k commits ahead and ~37k behind upstream. The new history is 9 thematic commits on top of `upstream/main`, and the source tree is unchanged. Original SHAs are preserved on the `archive/pre-rebuild-20260424` branch. Existing clones can recover with:
>
> ```sh
> git fetch origin
> git reset --hard origin/main
> ```

## License

Lathe inherits its licensing from upstream Zed:

- The source is licensed primarily under the [GNU General Public License v3.0 or later](LICENSE-GPL).
- Some components are licensed under the [Apache License 2.0](LICENSE-APACHE), where marked.

Upstream Zed relicensed from AGPL to GPL in May 2026, and this fork follows it, so there is no longer an AGPL license file.

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
