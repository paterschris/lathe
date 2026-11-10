# Lathe

A code editor. It's a fork of [Zed](https://zed.dev) with my own changes stacked on top: a mobile dev panel for React Native and Expo, an extension API that goes further than Zed's, more git tooling than upstream ships, pull request review inside the editor, and a theme I actually want to look at all day.

### [Download Lathe](https://github.com/paterschris/lathe/releases/latest)

macOS, Linux, Windows. On macOS, Homebrew works too:

```sh
brew install --cask paterschris/tap/lathe
```

Per-platform notes are under [Install](#install). Windows needs an extra paragraph of reading.

This is a personal fork. I maintain it because I wanted a handful of editor tweaks and didn't want to wait on upstream review for each one, or make the case that every one of them belongs in Zed's product scope. Most of them don't. Zed is still the real project here; Lathe just tracks it and layers my stuff on top.

**Platforms:** macOS (Apple Silicon), Linux (x86_64 and arm64), Windows (x86_64 and arm64, experimental).

**Stability:** I use this as my daily editor, which is most of the QA it gets. Upstream syncs break things now and then. Bug reports welcome.

## Features

Roughly ordered by how far each one is from stock Zed. One caveat on the git entry: upstream already has a commit graph, a tabbed git panel, worktree support, and single-file history, so that section only covers what Lathe puts on top of them.

1. [Mobile development](docs/features.md#mobile-development-expo--react-native) - a panel for Expo and bare React Native projects
2. [Extensions](docs/features.md#extensions) - an extension API past Zed's (commands, buffer edits, file events, terminal tasks) with per-extension permissions, more registries, install from a URL, disable and recommend extensions
3. [Merge conflicts and interactive rebase](docs/features.md#merge-conflicts-and-interactive-rebase) - a conflict resolution tab, full-file split view, drag-and-drop rebase
4. [Pull request reviews](docs/features.md#pull-request-reviews) - GitHub, GitLab, Bitbucket, in the editor, with several accounts per host
5. [Code navigation](docs/features.md#code-navigation) - definitions and references open in a peek instead of a new tab
6. [AI agent integration](docs/features.md#ai-agent-integration) - sign into several accounts, control approval levels, per-workspace thread history
7. [Theme and syntax highlighting](docs/features.md#theme-and-syntax-highlighting) - the default theme, plus a live customizer for all 200+ colors
8. [Git additions](docs/features.md#git-additions) - explorer tab, branch tree, compare any two commits, graph context menus, multi-select, undo, Git Flow
9. [Jupyter notebooks](docs/features.md#jupyter-notebooks) - saving keeps your outputs, cells share one document, and the kernel picker finds your project's venv
10. [Docks and panels](docs/features.md#docks-and-panels) - two panels per dock, drag panels to any dock, every panel allowed in the bottom dock
11. [Terminal, windows, and workspaces](docs/features.md#terminal-windows-and-workspaces) - extra windows for editors and terminals, an awaiting-input indicator, workspace groups, per-window zoom
12. [Editor additions](docs/features.md#editor-additions) - color swatches and a color picker, Open Recent File, project panel refresh
13. [AWS profiles](docs/features.md#aws-profiles) - a per-window profile selector
14. [Separate data from Zed](docs/features.md#separate-data-from-zed) - Lathe's own settings and data folders, imported from Zed on first launch

Screenshots and the long version live in **[docs/features.md](docs/features.md)**.

## Install

### Homebrew (recommended)

```sh
brew tap paterschris/tap
brew install --cask lathe
```

### Manual download

From [Releases](https://github.com/paterschris/lathe/releases):

- **macOS**: grab the `.dmg`, open it, drag **Lathe.app** to `/Applications`. There's a `.zip` too if you'd rather. Both are code-signed and notarized by Apple.
- **Linux**: the `.AppImage` (`chmod +x` it and run it; in-app updates replace the file in place), or the `.tar.gz`, extracted wherever you want it. Or build from source and use the install script below. Runtime requirements are the same as upstream Zed's: the host's ALSA (`libasound2` on Debian/Ubuntu, `alsa-lib` on Fedora/Arch) and working Vulkan drivers. Any normal desktop distro already has both.
- **Windows**: setup `.exe` or `.zip`, x86_64 or arm64. SmartScreen will probably still warn you, so read [Installing on Windows](#installing-on-windows) first.

### Installing on Windows

The setup `.exe` is the easy path. Lathe's Windows builds are signed, but the certificate hasn't built up SmartScreen reputation yet, so Defender will probably still warn you about it. Click **More info**, check that the file came from the Lathe GitHub release, then **Run anyway**.

If you'd rather have it portable, download the x86_64 `.zip`, open PowerShell in a Lathe source checkout, and run:

```powershell
script/install-fork-windows.ps1 -ArchivePath C:\path\to\Lathe-version-x86_64-windows.zip
```

That strips Mark-of-the-Web off the extracted files, installs Lathe under `%LOCALAPPDATA%\Programs\Lathe`, puts its CLI on your user `PATH`, and adds a Start Menu shortcut. If the install goes fine but no window ever shows up, run `script/diag-windows.ps1` from the checkout and paste the output into a bug report.

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

It installs as **Lathe** and runs alongside stock Zed without stepping on it, with its own settings, extensions, history and logs.

Want to try a build without installing it to `/Applications`? Launch the bundle where it sits:

```sh
open target/release/bundle/osx/Lathe.app
```

## Release channels

Two channels:

- **Stable**, tagged `vX.Y.Z`. Use this one.
- **Beta**, tagged `vX.Y.Z-beta` and published as GitHub prereleases, with a different app icon. Beta usually means the newest upstream Zed sync is in there and hasn't reached stable yet.

Homebrew gives you stable. For a beta, pull the `-beta` asset off [Releases](https://github.com/paterschris/lathe/releases).

Want a ping when a release goes out? **Watch > Custom > Releases** at the top of the repo. Starring won't do it, that just bookmarks the project.

Release notes also go up on [r/LatheEditor](https://www.reddit.com/r/LatheEditor/), which is the place for feature requests and general discussion too. Bugs and pull requests stay here in the repo.

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

### Update checks

In-app update checks go to Lathe's own server at `updates.lathe-ide.com`, which is how I count installs that are actually in use. Each check sends the Lathe version, channel, OS and architecture, plus a salted hash of an anonymous install ID; the raw ID and IP addresses aren't stored. Turn off `telemetry.metrics` and the hash isn't sent. Downloads still come straight from GitHub, and if Lathe's server is down the check falls back to GitHub. Details are in the [telemetry docs](docs/src/telemetry.md#lathe-update-checks).

## Relationship to Zed

Lathe merges from [upstream Zed](https://github.com/zed-industries/zed) every few weeks to pick up new features and fixes. My changes live in their own commits, which is what keeps those merges manageable.

**Last synced with upstream Zed: 2026-10-29.**

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

Upstream Zed relicensed from AGPL to GPL in May 2026 and this fork followed, so there's no AGPL license file anymore.

All upstream license terms are preserved. See the individual `LICENSE-*` files at the repo root.

## Contributing

It's mostly a personal fork, but I'd like it to keep Zed's open-source feel. Hit a bug, want a tweak, have an idea that fits? Open an issue or a PR. [CONTRIBUTING.md](CONTRIBUTING.md) has the inherited Zed guidelines; Lathe-specific conventions are in [CLAUDE.md](CLAUDE.md) and `.rules`.

## Releasing

```sh
script/release-fork
```

Builds, packages, and publishes a GitHub release. Needs the [GitHub CLI](https://cli.github.com/).

## Notes

- The first build takes a while. Incremental rebuilds are much faster.
- Lathe keeps its own settings and data: `~/.config/lathe` and `~/Library/Application Support/Lathe` on macOS, `$XDG_CONFIG_HOME/lathe` and `$XDG_DATA_HOME/lathe-data` on Linux, `%APPDATA%\Lathe` and `%LOCALAPPDATA%\Lathe` on Windows. On first launch it copies your settings, keymaps, themes, extensions, history and accounts over from stock Zed's folders. After that the two apps don't share changes.
- Linux installs land in `~/.local/share/lathe`, CLI symlinked to `~/.local/bin/lathe`. That folder is wiped on every update, which is why Lathe's data lives in `lathe-data` instead.
- `cargo-bundle` gets installed for you from [zed-industries/cargo-bundle](https://github.com/zed-industries/cargo-bundle)
