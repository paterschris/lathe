# Lathe

A customized code editor forked from [Zed](https://zed.dev), focused on mobile development workflows, deeper git tooling, in-editor code review, and theme customization.

Lathe is a personal fork of Zed. I maintain it so I can ship small editor tweaks for my own workflow without waiting on upstream review, and without needing each change to fit Zed's product scope. Upstream Zed is the primary project - this fork tracks it closely and layers on my own changes.

**Platforms:** macOS (Apple Silicon), Linux (x86_64), and Windows (x86_64; experimental).

**Stability:** Lathe is maintained for my own daily use. Upstream syncs can occasionally introduce breakage; bug reports are welcome.

## Features

Ordered by how much each one differentiates Lathe from stock Zed. Upstream already ships a commit graph, a tabbed git panel, and worktree support; the git section below covers what Lathe adds on top of those rather than restating them.

1. [Mobile development](#mobile-development-expo--react-native) - Expo and bare React Native panel
2. [Merge conflicts and interactive rebase](#merge-conflicts-and-interactive-rebase) - conflict resolution tab, full-file split view, drag-and-drop rebase
3. [Pull request reviews](#pull-request-reviews) - GitHub and Bitbucket Cloud, in-editor
4. [AI agent integration](#ai-agent-integration) - multi-account sign-in, approval control
5. [Theme and syntax highlighting](#theme-and-syntax-highlighting) - custom theme, live 135+ color customizer
6. [Git additions](#git-additions) - explorer tab, branch tree, undo, Git Flow
7. [AWS profiles](#aws-profiles) - per-window profile selector
8. [Terminal, windows, and workspaces](#terminal-windows-and-workspaces) - awaiting-input indicator, workspace groups, per-window zoom

---

## Mobile development (Expo / React Native)

Lathe ships a first-class Mobile panel that auto-detects the kind of mobile project you open (Expo or bare React Native) and surfaces only the workflows that apply to it. For bare React Native projects it reads the project's own package scripts and the run hints in its README, offers iOS scheme and Android variant dropdowns that feed those scripts, and installs Android builds reliably with `gradlew :app:install<Variant>` plus an `adb` launch (sidestepping the React Native CLI's flavored-APK bug). Long-running processes (Metro, builds, and any script you start) open as interactive terminal tabs with their own scrollback. The panel can also create an Android emulator (AVD) for you without Android Studio, shows a live device list with per-app logcat, drives one-click debug build & run and EAS cloud builds, and installs the whole Android toolchain (JDK 17 plus the Android SDK, licenses accepted) into a Lathe-managed directory. On macOS it covers iOS as well. A drop-in `.zed/tasks.json` template and a full setup walkthrough live in [docs/mobile-development.md](docs/mobile-development.md).

---

## Merge conflicts and interactive rebase

### Conflict reporting and resolution
When a merge, rebase, cherry-pick, revert, pull, or stash pop stops on conflicts, a notification names the conflicted files and offers **Resolve**, which opens a tab listing every conflicted file beside the merge editor for the selected one. Resolve a whole file as ours or theirs, stage files as you finish them, and abort or continue the operation from the same header without dropping to a terminal. The same tab is reachable any time from the git panel's Conflicts section or `git: resolve conflicts`.

### Merge conflict editor with full-file split view
Resolve conflicts with **Take ours** / **Take theirs** / **Take both** per conflict, or step through them with previous/next navigation. The split view shows each side as a complete file (every conflict region replaced by that side's kept content), scrolls both panes in lockstep, and highlights the selected conflict across them. "Edit manually" drops you into the buffer when the buttons aren't enough.

### Interactive rebase with drag-and-drop
The interactive rebase modal supports drag-and-drop reordering of commits and per-row pick / squash / edit / drop actions inline. Dragging a commit or branch onto another in the commit graph shows a confirmation modal with a preview of what the rebase will do before anything runs.

---

## Pull request reviews

A pull request panel with browser-based auth for GitHub and Bitbucket Cloud: browse PRs, read and leave review comments, see reviewers, and approve or request changes without leaving the editor. Verdict buttons toggle, so clicking Approve again retracts your approval. A separate section lists PRs you authored, with reviewer roll-ups showing where each one stands. Still being polished; the panel button can be hidden via `pull_request_panel.button`.

---

## AI agent integration

### Agent accounts and approval control
Sign in to multiple subscription accounts for the external agents in the Agent Panel (Claude Code, Codex, Gemini) and switch between them from the panel's account chip. Account selection is per-workspace, so a work project and a personal project can each stay on their own identity. An approval selector picks each agent's approval / sandbox level; the level is applied when the agent process spawns, so changes take effect on the next thread. Agents with their own native approval control keep it and skip the selector.

---

## Theme and syntax highlighting

### Custom theme and syntax palette
Lathe ships with its own default theme and a refined syntax highlighting palette applied across all supported languages. The theme is tuned for long coding sessions: balanced contrast, distinct-but-not-loud accent colors for keywords, strings, and types, and deliberate choices for diagnostic and git-status colors so the editor stays readable when things go wrong.

![Default Theme](assets/screenshots/default-theme-code.png)

### Theme Customizer
A built-in panel for editing all 135+ theme colors, including syntax token colors, with HSLA sliders and live preview. Includes category filters, Lathe-specific color badges, and per-color reset. Open via the command palette: `theme customizer: Open`.

![Command Palette](assets/screenshots/theme-customizer-command-palette.png)

![Theme Customizer](assets/screenshots/theme-customizer.gif)

---

## Git additions

Zed already ships the commit graph, the tabbed git panel, and worktree support. Everything below is what Lathe layers on top.

### Explorer tab and hierarchical branch folder tree
Lathe adds a third **Explorer** tab to the git panel, alongside upstream's Changes and History. It lists branches, worktrees, and stashes for the repository in one filterable tree, and renders Local and Remote branches as a collapsible folder tree that splits names on `/`. So `feature/auth/login` and `feature/auth/signup` collapse under a single `feature/auth/` folder you can fold or expand. Folders show counts of contained branches and remember their open/closed state per section. Local branches that exist on the remote get an on-remote indicator. When the filter input is active the tree flattens so filter results stay legible. A multi-repo strip keeps every repository in the workspace one click away, with fetch-all and pull-all actions, plus any external repositories pinned via `repository_dashboard_pinned_repos`.

### Undo for destructive operations
Branch resets, deletes, renames, and tag creation record an undo entry, and the resulting toast offers a one-click **Undo**. Discards stash defensively first, so they can be restored too. Up to 50 entries are kept per repository.

### Git Flow commands
Start and finish feature, release, and hotfix branches from the command palette. Finishing merges with `--no-ff` into the right target, tags releases and hotfixes, merges back into `develop`, and deletes the local branch. Failures surface as errors rather than half-completing silently.

- `git flow: Start Feature` / `git flow: Finish Feature`
- `git flow: Start Release` / `git flow: Finish Release`
- `git flow: Start Hotfix` / `git flow: Finish Hotfix`

### Branch from commit
From any commit in the history view or graph, create a new branch off that revision without first checking out. Useful for forking experimental work off a specific point.

### Detached HEAD checkout from history
Check out any commit SHA from the history view into detached HEAD. For inspecting old state without losing your current branch position. Local repositories only for now; collab projects bail with a clear error.

### Worktrees that start up to date
Creating a worktree from a remote branch fetches the latest origin state first, so the new worktree starts from the current remote tip instead of a stale local ref.

### File history view
Open the full commit history of a single file from the project panel and browse how it changed over time.

### Git-aware tab and panel styling
Tabs and project panel entries are color-coded by git status: modified, created, deleted, conflict, error, and warning states each get distinct colors.

![Git Tab Styling](assets/screenshots/git-aware-editor-tabs.png)

### Inline hunk staging
Expand any file row in the Changes list to see its individual hunks and stage or unstage them one at a time, without opening a diff view.

### Branch status indicator and git activity panel
The status bar shows the active repository's branch with its push/pull state. A separate git activity panel (docked bottom by default) shows in-flight git commands live, so long fetches and clones aren't invisible.

---

## AWS profiles

A status-bar AWS profile selector, scoped per window, so two windows can target different accounts at once. Everything Lathe spawns (terminals, tasks) inherits the selected `AWS_PROFILE`. The menu shows only profiles you've used in this workspace, with the rest behind **Show All Profiles**, and it polls SSO session status so an expired login is visible before a command fails. A project-local `.aws/config` takes over from the global one when present. The whole selector stays hidden unless the machine actually has AWS profiles configured.

---

## Terminal, windows, and workspaces

### Awaiting-input indicator
Shows a return icon in the terminal tab and title bar when Claude Code or other interactive prompts are waiting for input, with the tooltip distinguishing a general prompt, a confirmation, and a multiple-choice selection.

![Awaiting Input Indicator](assets/screenshots/awaiting-input-indicator.gif)

### Active terminal tab tint
Terminal tabs get a subtle green background when active, making them easy to spot among editor tabs.

### Terminal focus fix
Switching to a terminal tab via ctrl+tab properly activates the cursor without needing to click into the terminal.

### Workspace groups with account binding
Save the set of currently open projects as a named **workspace group**, then reopen the whole group in a new window later. Each group can optionally be bound to a saved collab account, so opening the group automatically switches to that account first. Groups can also be written to a portable `.lathe-workspace` file that travels with the project.

Commands (via the command palette):

- `workspace groups: Save Workspace Group`
- `workspace groups: Open Workspace Group`
- `workspace groups: Update Current Workspace Group`
- `workspace groups: Rename Current Workspace Group`
- `workspace groups: Bind Workspace Group Account`
- `workspace groups: Unbind Workspace Group Account`

### Per-window zoom
`Cmd +`, `Cmd -`, and `Cmd 0` adjust the buffer and UI font size of only the active Lathe window, so two windows side-by-side can be zoomed independently. The "Reset Zoom" menu action and `Cmd +scroll-wheel` (when mouse-wheel zoom is enabled) also stay scoped to the focused window. The persisted variants (the menu's `… (persisted)` items) still write to `settings.json` and apply globally.

### Multi-account collab switcher
Sign into more than one Zed Cloud account and switch between them from the avatar menu. Saved accounts are listed under **Accounts** by their GitHub username, with **Add Account…** and **Sign Out** actions.

### Copy collab link dialog
When generating a shareable collab link, a dialog lets you pick which saved account to link from, which helps when you work across personal and work Zed Cloud accounts.

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
- **Windows**: Download the x86_64 setup `.exe` or `.zip`. Windows builds are currently unsigned; see [Installing on Windows](#installing-on-windows).

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

**Last synced with upstream Zed: 2026-07-14.**

> **2026-04-24:** `main` was rewritten to fix a long-standing ancestry tangle that made the fork display as ~37k commits ahead and ~37k behind upstream. The new history is 9 thematic commits on top of `upstream/main`, and the source tree is unchanged. Original SHAs are preserved on the `archive/pre-rebuild-20260424` branch. Existing clones can recover with:
>
> ```sh
> git fetch origin
> git reset --hard origin/main
> ```

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
