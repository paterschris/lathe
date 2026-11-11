# Lathe Release Notes

Most recent releases first. Beta releases (`-beta` suffix) ship as GitHub pre-releases and typically batch new features ahead of a stable cut.

## v1.20.0-beta - 2026-11-11

Two things this time. You can manage extensions properly now (turn them off, control updates, recommend them per project). And extensions can draw their own UI: file previews, dock panels, clickable status items and highlights in the editor.

### Managing extensions

- **Turn an extension off without uninstalling it.** Disable and Enable are on its card and in its menu. Its settings and permissions stay put.
- `extension_auto_update` turns automatic updates on or off for everything. The card's menu does it per extension.
- **Recommended extensions per project.** List them in a project's `.zed/settings.json` under `recommended_extensions`. Opening the project offers to install the missing ones, with Don't Ask Again if you'd rather not.

### Extension UI (API 0.2, preview)

- **File previews.** An extension can preview a file type in a tab next to the editor, from a button in the editor toolbar or the command palette. The editor stays open, so you can still edit the file. Previews redraw as you type and come back after a restart.
- **Dock panels.** Extensions can add panels to the left, right or bottom dock. Each side gets one Extension Panels button with a tab per panel, and each panel has a "Toggle" entry in the palette.
- What extensions can draw: text, buttons, list items and trees, text inputs, checkboxes, dropdowns, right-click menus, progress bars, markdown, and lists that load as you scroll so a million rows doesn't send a million rows. It all uses Lathe's own components, so it follows your theme.
- **Highlights in the editor.** Backgrounds, underlines, squiggles, gutter marks and short messages at the end of a line. They move with your edits. Needs the new `ui:decorations` permission.
- Clickable status bar items.
- An extension can open a file at a line, but only while handling something you did (a command, a click in its panel, a click on its status item).
- Extensions can follow which files you have open in tabs, separately from files Lathe has loaded in the background.

0.2 isn't on crates.io yet, because publishing freezes it. To try it, point `lathe_extension_api` at this repo with a git dependency. Extensions built against 0.2 only load in this beta; Lathe 1.19 and the current stable skip them. 0.1 extensions work as before.

### Other changes

- The command palette shows Lathe's own commands as `lathe: ...` now. Typing `zed:` still finds them, and keymaps that say `zed::` keep working.
- The welcome screen says "Craft your masterpiece".

## v1.19.0-beta - 2026-11-09

Extensions can do a lot more in Lathe now. They can add commands, work with the file you're in, react when files are saved or edited, and run commands in the terminal. Each of those is a permission you approve. Zed extensions work exactly as before.

### Extensions

- **Commands.** Extensions can add commands to the command palette. Bind one to a key with `["extension::RunCommand", { "extension_id": "...", "command": "..." }]`.
- **Working with your files.** If you allow it, an extension can read the file you're in, edit it as a single undo step and move the cursor. It can also run when files are opened, saved or changed.
- Status bar text, notifications, a text prompt and a pick list. Prompts only show up from a command you ran, never on their own.
- **Commands in the terminal.** They run as normal tasks. Save again while one's still running and it reuses the same tab instead of piling up new ones.
- **Permissions.** Every one of these is a separate permission an extension has to declare. Lathe asks before installing from a registry or a URL, lists them on the extension's card, and has Allow and Revoke buttons. Folders you install as dev extensions are allowed automatically, since they're your own.
- `extension_settings` holds settings for individual extensions, keyed by extension id. A project's `.zed/settings.json` can set them too. Those only apply once you trust the project.
- An extension stuck on a file event gets stopped after two seconds. Three in a row and its file events pause until you reinstall or rebuild it.
- **More places to get extensions.** Lathe's extension registry is listed next to Zed's, and `extension_registries` adds more. You can also paste a `.tar.gz` or git URL into the Extensions search box to install from it. Registry downloads are checked against a checksum, and the list still loads when you're offline.
- Lathe-only extensions get a Lathe badge.

To build one, add `lathe_extension_api = "0.1"` from crates.io next to `zed_extension_api`. Its README covers the API and each permission.

### Lathe Companion

- **No pushes while you're at Lathe.** Phones only get notified once you've been away for 5 minutes. Change that with `companion.idle_minutes`, or set it to 0 to always get them. The title bar says Idle once pushes would go out.
- Notifications show the thread's or terminal's title.
- You can mute workspaces from the phone. Their threads and terminals stop notifying you.
- Answering a terminal from the phone clears its waiting-for-input marker, same as typing at the desktop.

Muting workspaces needs a companion app update.

### Pull requests

- The pull request panel has its own git account menu. When a repo's host has no account connected, it offers to connect one.

## v1.18.0 - 2026-11-05

Everything from the 1.17.0 beta: Lathe Companion can show your terminals.

### Lathe Companion

- **Terminals on your phone.** The companion app can now list the terminals in your open projects, including Claude Code, Codex and other CLI agents running in the agent panel. Open one to see its screen live, with scrollback and colors.
- **Typing into terminals is off until you turn it on.** It can run any command, so it has its own switch in the companion dialog ("Allow typing into terminals from the phone"), or set `companion.terminal_input`. With it off, the phone can still watch.
- You get a push when a terminal starts waiting for input, unless you already have it open on the phone.

Needs a companion app update that adds terminals. Until then, nothing changes on the phone.

## v1.17.0-beta - 2026-11-04

### Lathe Companion

- **Terminals on your phone.** The companion app can now list the terminals in your open projects, including Claude Code, Codex and other CLI agents running in the agent panel. Open one to see its screen live, with scrollback and colors.
- **Typing into terminals is off until you turn it on.** It can run any command, so it has its own switch in the companion dialog ("Allow typing into terminals from the phone"), or set `companion.terminal_input`. With it off, the phone can still watch.
- You get a push when a terminal starts waiting for input, unless you already have it open on the phone.

Needs a companion app update that adds terminals. Until then, nothing changes on the phone.

## v1.16.0 - 2026-11-02

The first stable release since 1.12.0. Everything from the 1.13.0, 1.14.0, 1.14.1 and 1.15.0 betas is in here, plus AI accounts for any installed agent and a rework of how Gemini signs in.

And the big one: **Lathe Companion**, a phone app for your agents. It's been riding along in the betas switched off. This is the first time it's in the notes, so everything it does is below.

### Before you update

- **Lathe needs macOS 12 (Monterey) or newer now.** Upstream raised the minimum.
- **The agent's Restore Checkpoint button is gone.** Upstream dropped checkpoints because taking one before every message made the agent slow to respond.
- **Gemini and "Log in with Google":** Google has turned that off for personal accounts and points you at Antigravity instead. If you use Gemini with a personal account, you need an AI Studio API key from a project with billing (or one that still has free-tier access). Existing API-key accounts keep working as they are.

### Lathe Companion

Keep an eye on your agents from your phone. Approve a tool call from the couch, answer a question from the grocery store, get pinged when a long run finishes.

**Getting set up.** It's off until you turn it on. Run **lathe companion: open companion** from the command palette and choose **Turn on**, then scan the QR code with the app. Lathe asks you to **Allow** the phone before it's paired, so a code someone else scans doesn't get them in. Once it's on, a phone icon in the title bar opens the same dialog.

- Code expired? **New code** makes another, and the old one stops working.
- **Copy link** gives you a link to open on the phone instead of scanning.
- The dialog lists your paired phones.

**What the phone can do:**

- See every agent thread in your open projects and follow them live as they stream.
- Approve or deny tool calls, answer an agent's questions, send messages.
- Start a new thread with any agent the project has. It opens in the agent panel without pulling the thread you have on screen out from under you.
- Rename, cancel and close threads. Closing archives a thread the same way Lathe's archive does, so you can bring it back.
- Change a thread's model and effort. That only affects that one thread, not your default for new ones. Modes stay on the desktop, since some of them skip permission prompts.
- Push notifications when a thread needs you or finishes.
- Pull requests: list them per workspace (including the ones waiting on your review), read the diff and comments, reply, leave inline comments, and submit a review. It goes through the git accounts you've already connected in Lathe. No host token ever leaves your computer.

**Privacy.** Everything between Lathe and your phone is end-to-end encrypted. The relay in the middle (`companion.lathe-ide.com`) only sees enough to route messages, not what's in them. Lathe's pairing identity lives in your system keychain.

**Your Mac stays awake** while an agent is running and a phone is paired, so a long run doesn't stall when the lid's open and nobody's touching it. Turn that off with `companion.keep_awake_while_agents_run`.

**Getting the app.** It's on the [App Store](https://apps.apple.com/us/app/lathe-companion/id6825627989) for iPhone. 7-day free trial, then $1.99 a month. Android isn't out yet.

### Added

- **Search Everywhere.** Press `shift shift` for one picker covering files, symbols, types and actions, with tabs to narrow it down. Symbols show up as each language server answers, so files and actions don't wait on them. In the JetBrains keymaps it takes over shift shift from the command palette.
- **Switch AI accounts without starting a new thread.** Pick another account from the chip in the agent panel and the thread you're in moves over with its history. Works for Claude and Codex. Other agents switch for new threads, and Lathe tells you so.
- **AI accounts for any installed agent.** Manage AI Accounts lists every agent you've installed, not just Claude, Gemini and Codex. Agents Lathe can't keep separate accounts for are grouped at the bottom with a short explanation. If one of your agents has a profile directory setting, describe it under `ai_account_profiles` in your settings and it gets accounts too. Accounts for an agent you've uninstalled stay listed, so their bindings aren't lost.
- **Gemini signs in its own way.** New Gemini accounts use the Gemini CLI's own sign-in options instead of a fixed API-key form. The API key field is optional when adding an account, and a lock button on each Gemini account sets or replaces its key later.
- **The right git account per repo.** With two GitHub (or GitLab, or Bitbucket) accounts connected, the pull request panel checks which one can push to each repo and uses it. `gh` and `glab` in an agent's terminal act as that same account.
- **Background git fetch.** Off by default. Turn it on with `git.auto_fetch.enabled` and set how often with `git.auto_fetch.interval_secs` (15 seconds minimum). Only runs in trusted repos and never prompts for credentials.
- **Refs column in the git graph.** Branch and tag badges get their own resizable column, which you can hide from the header's right-click menu.
- **Foldable Android emulator** in the Mobile panel, next to the phone and tablet ones.
- Several React Native or Expo projects in one workspace. The Mobile panel finds them all, and the project name becomes a dropdown that picks which one build, run and the scripts target.
- Debugger hovers show a variable tree you can move through with the keyboard.
- `"on_new_window": "readme"` opens the project's README when a window has nothing else open.
- Commit message generation uses your recent commit messages as style examples.

### Fixed

- **Remote development over SSH.** Releases never included the server binary a remote machine needs, so connecting had nothing to download. This release ships it.
- **Review comments on the right lines.** A comment on a removed line shows under that removed line instead of under whatever new line has the same number. Pull request diffs come from the PR's own changes, so commits that landed on the target branch after you branched don't show up as part of your PR.
- **GitHub errors you can read.** Creating a pull request with no new commits (and similar) shows GitHub's actual reason instead of a wall of JSON.
- Gemini accounts no longer share one Google login. The CLI kept every account's login in a single keychain entry, so Lathe now has it store them in each account's own folder.
- Discarding a file from the unstaged section keeps its staged changes.
- A repo whose folder moved while it was open gets its git state rebuilt instead of pointing at the old location.
- Language injections in notebook cells, plus a pile of smaller agent, editor and git fixes from upstream.

### Changed

- Caught up with upstream Zed twice (115 and 186 commits).
- Releases are built with Xcode 26, which upstream needs now.

### Known issues

- Moving an open conversation to another account only works for Claude and Codex. Gemini and other agents pick up the new account in new threads.
- Pull requests don't show up in the iPhone app yet. They're coming in the app's next release.
- Tested by hand on macOS. Windows and Linux builds haven't been tried on real machines for this release.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.15.0-beta - 2026-10-29

Caught up with upstream Zed again (186 commits). Two things to know first: Lathe now needs macOS 12 (Monterey) or newer, and the agent's "Restore Checkpoint" button is gone. Upstream dropped checkpoints because taking one before every message made the agent slow to respond.

### Added

- **Background git fetch.** Off by default. Turn it on with `git.auto_fetch.enabled` and set how often with `git.auto_fetch.interval_secs` (15 seconds minimum). It only runs in trusted repos and never prompts for credentials.
- **Debugger hovers** show an expandable variable tree you can move through with the keyboard.
- **Refs column in the git graph.** Branch and tag badges get their own resizable column, which you can hide from the column header's right-click menu. Dragging a branch onto a commit works from either column.
- `"on_new_window": "readme"` opens the project's README in a preview tab when a window has nothing else open.
- Commit message generation uses your recent commit messages as style examples.

### Fixed

- Discarding a file from the unstaged section keeps its staged changes.
- A repo whose folder moved while open gets its git state rebuilt instead of pointing at the old location.
- Language injections in notebook cells, and a pile of smaller agent, editor and git fixes from upstream.

### Under the hood

- Releases are built with Xcode 26, which upstream needs now.
- Downloaded language servers and tools still warn in the log when a release didn't publish a checksum. Upstream moved that download code, so the warning moved with it.

---

## v1.14.1-beta - 2026-10-27

Nothing new to try in this one. It carries work on a feature that isn't out yet, which stays off by default.

---

## v1.14.0-beta - 2026-10-23

Search Everywhere is the big one. Press shift twice.

### Added

- **Search Everywhere.** Press `shift shift` and you get one picker for files, symbols, types and actions, grouped into sections, with tabs (All, Types, Files, Symbols, Actions) to narrow it down. Symbols show up as each language server answers, so files and actions don't wait on them. Works in every keymap. In the JetBrains keymaps it takes over the shift shift binding from the command palette.
- **Foldable Android emulator.** The Mobile panel can create a Pixel 9 Pro Fold emulator next to the phone and tablet ones. It installs the newer Android command line tools it needs on its own.
- Several React Native or Expo projects in one workspace. The Mobile panel finds all of them (each folder, or the folders one level down), and the project name turns into a dropdown that picks which one build, run and the scripts target.

### Fixed

- **Remote development over SSH.** Connecting to a remote machine needs a server binary matching your Lathe version, and releases never included one, so it had nothing to download. Every release ships it now, starting with this one.

---

## v1.13.0-beta - 2026-10-20

Mostly about accounts. Switching AI accounts no longer leaves your open thread behind, and the pull request panel figures out which git account to use per repo.

### Added

- **Switch AI accounts without starting a new thread.** Pick another account from the chip in the agent panel and the thread you're in moves over with its history. It used to keep running on the old account (and billing it) until you opened a new thread. A small spinner sits next to the account name while it reloads, and the conversation stays on screen. Works for Claude and Codex. Gemini threads still need a new thread after a switch.
- **The right git account per repo.** With two GitHub (or GitLab, or Bitbucket) accounts connected, the pull request panel checks which one can actually push to each repo and uses that. No more switching accounts by hand because one of them can't see a private repo. Your workspace pick still wins when both can.
- Agents get the same account. `gh` and `glab` run from an agent's terminal now act as whichever account the pull request panel uses for that repo, instead of whatever your shell happens to be logged into. A token you set in the agent's own settings is left alone.

### Under the hood

- Caught up with upstream Zed (115 commits), mostly agent connection work.
- Linux installs without `rsync` weren't counted as active installs, since the update check bailed before checking in. They still can't update themselves. They do get counted now.

---

## v1.12.0 - 2026-10-16

The first stable release since 1.8.0. Everything from the 1.9.0, 1.10.0, 1.10.1, 1.11.0 and 1.11.2 betas is in here, plus commit comparison in the git graph and a change to how update checks work.

The biggest change is under the hood: Lathe now has its own settings and data folders instead of sharing Zed's. Your settings, history and accounts come along on first launch. Read **Before you update** if you've also been running Lathe Beta.

### Before you update

- **If you ran Beta 1.11.0 and Stable 1.8 has been saying "unable to open database file":** Beta 1.11.0 "repaired" the database Stable shares with Zed, and Stable couldn't open it after that. Quit Lathe, then rename `db.sqlite.before-lathe-repair` back to `db.sqlite` in `~/Library/Application Support/Zed/db/0-stable/` before updating, so your recent projects and history come with you. Nothing gets lost, since Stable hasn't saved anything to the repaired file.

### Added

- **Lathe keeps its own data.** Settings, keymaps, themes, extensions, history and accounts now live in Lathe's own folders (`~/.config/lathe` and `~/Library/Application Support/Lathe` on macOS, the `lathe` equivalents on Windows, `~/.local/share/lathe-data` on Linux). First launch copies everything over from Zed's folders. It takes about a second, and you shouldn't need to sign in to anything again. If an account's sign-in can't be carried over, Lathe tells you which one.
- **Compare any two commits.** In the git graph, Cmd-click a second commit, then right-click and choose **Compare Commits** to see everything that changed between them. They don't have to be next to each other or on the same branch. The diff has a button to swap which side is old and which is new. The existing entry is now called **View Diff Against Parent** to tell the two apart.
- **Two panels per dock.** Cmd-click a second panel's button and it opens under the first (or beside it, in the bottom dock), with a divider you can drag. Double-click the divider for 50/50. The split is saved per workspace.
- **Drag panels where you want them.** Drag a panel's button off the status bar and drop zones show up for each half of each dock, or right-click a button and use **Move to**. Every panel can go in the bottom dock now, including Project, Git, Outline, Pull Requests, Collab and Agent.
- A close button next to each open dock's panel buttons, plus **Close Left Panels** (or Right, Bottom) on every button's right-click menu. Reopening brings back the same panels in the same sizes.
- **Several accounts per git host.** Add a second GitHub, GitLab or Bitbucket account from the account menu and pick which one each workspace uses. Pull requests, reviews and the reviewer picker all follow that choice. Your existing connection carries over as the first account.
- **Color swatches and a color picker.** Hex, `0x` hex and `rgb()` / `hsl()` values get a swatch in front of them. Click it (or right-click and choose **Pick Color**) to adjust the color, and the new value is written back in the same notation.
- **The git graph caught up with upstream.** Ref badges sit in a gutter beside the lanes, columns can be hidden from the header's right-click menu, commit details collapse, changed files show as a tree or a flat list, and the graph remembers its state across restarts. Dragging branches and commits still works: drop a branch anywhere on a commit's row, and the drag shows the branch name or short hash.
- **File history in the graph.** "View History in Git Graph" in the git panel's right-click menu.
- The commit right-click menu has **Copy Tag** and a **Custom Commands** section for your git tasks. You can create a tag at HEAD or on any commit from the graph.
- **Multi-select in the git panel.** Shift-click and Cmd-click to mark files, then stage, unstage, discard, stash or open them together. Section headers show file counts, folders in tree view get their own right-click menu, and the commit editor can collapse.
- **Open Recent File** in the File menu and on the welcome page (`alt-cmd-shift-o` on macOS).
- **Linux AppImage** for x86_64 and aarch64. `chmod +x` it and run it. No libfuse2 needed, and in-app updates replace the file in place.
- **Refresh** in the project panel's right-click menu re-reads the tree from disk.
- Pull request panel sections can be resized. Double-click a divider to reset it.
- Gitiles and self-hosted Gerrit work as git hosts, permalinks included.
- The file finder prefills with your selected text. `alt-/` shows which bindings can finish a half-typed key chord. Markdown preview tabs have **Show Source**.
- New settings: `soft_wrap_indent`, `markdown_preview.heading_font_weight`, and a font family for Mermaid diagrams.

### Fixed

- **Switching AI accounts only affects that workspace.** It used to change the account for every window and every running Lathe instance.
- **Add Account not adding the account.** The browser said you were signed in, but Lathe kept the old account.
- **Commits credited to the wrong GitHub user.** Workspaces tied to a collab account wrote a GitHub noreply email built from your Zed id, not your GitHub id. Lathe now looks up the real id, and uses your git config until it has it.
- **Sign Out quietly switching you to your other account.**
- Running Beta and Stable side by side could swap account names and avatars between them, or flip windows between accounts.
- **Stock Zed's "Failed to load the database file" error.** On first launch Lathe repairs Zed's copy after backing it up next to the original (`db.sqlite.before-lathe-repair`). Restart Zed afterward and your recent projects are back.
- Lathe and Zed kept deleting each other's language server and debug adapter downloads. On Linux, Lathe wouldn't start while Zed was running, and the `lathe` command opened files in Zed.
- **Picking reviewers:** clicking a person ticks them instead of sending the request right away. A footer has **Clear** and **Request** buttons.
- Cmd-clicking a commit in the git graph no longer drops the commit you'd already selected.
- Connecting GitHub for pull requests: clicking outside the dialog after approving in the browser threw the sign-in away.
- Adding a Codex or API-key AI account didn't take effect until you restarted the agent.
- "My reviews" keeps pull requests you've already approved or requested changes on.
- Autosave and agent edits don't remove imports anymore. Saving with `cmd-s` works as before.
- Cmd-+ and Cmd-- zoom the agent panel again, and in the markdown preview they zoom the whole window, not just the preview.
- Agent file links like `main.rs:2` open from the first folder of a multi-folder project. Project folders keep the order you added them in.
- Rainbow parameter colors, compact terminal cards, and **Save Workspace** / **Save Workspace As…** in the File menu are back after upstream merges dropped them.
- Stop Metro and Stop Spotlight in the Mobile panel close the terminal and stop the process.
- Git fixes: searching by a full SHA-256 hash, branch badges lining up with the wrong commits in filter mode, slow commit diffs in shallow clones, revisions and file paths with the same name, remote URLs in partial clones, and creating a worktree from a bare repository.
- Folders symlinked from outside the project pick up file changes, and filesystems that need a polling watcher get one.
- Emoji, symbol and non-Latin fonts rendering as empty boxes.
- Crashes when pasting multiple selections at the end of a file, typing a partial key chord at end of file, and editing while an inlay hint request was in flight.
- Cmd-click doing nothing on a symbol named the same as its file. Auto-closing brackets getting inserted inside comments and strings. `.env` files highlighted as shell scripts.
- Prompts to external agents could be slow to send or not send at all. Remote project search could hang.
- Windows builds called themselves "Zed" in file properties.
- A batch of Markdown Preview fixes: images in tables and HTML blocks, task list checkboxes, nested code block copying, and search highlights covering the text.

### Changed

- **Update checks go through Lathe's own server** at `updates.lathe-ide.com` instead of straight to GitHub, so I can finally count how many installs are actually in use. Each check sends your Lathe version, channel, OS and architecture, plus a salted hash of your anonymous install ID. The raw ID never leaves your machine, and IP addresses aren't stored. Turn off `telemetry.metrics` and the hash isn't sent. Downloads still come straight from GitHub, and if Lathe's server is down, the check falls back to GitHub. Details are in the [telemetry docs](https://github.com/paterschris/lathe/blob/main/docs/src/telemetry.md#lathe-update-checks).
- Merged upstream Zed (225 commits): faster startup, a smaller binary, and better multi-cursor and project search performance.
- Project search runs as you type now, matching upstream. Set `"search_on_type": false` to go back to pressing Enter.
- Title bar menus open on click, not on hover. Set `title_bar.open_menus_on_hover` to get the old behavior back.
- A round of performance work on things that ran on every frame or keystroke: account menus, the git panel, git graph and outline, Mobile Dev's device polling, and the terminal's awaiting-input pulse. Long agent threads also open faster.
- Removed a few leftovers from upstream that showed up in settings or keymaps but did nothing.

### Linux

- Lathe's data lives in `~/.local/share/lathe-data`, not `~/.local/share/lathe`, which is where the installer puts Lathe Stable and wipes on every update. The move happens on first launch.
- `uninstall.sh` was removing Zed's folders instead of Lathe's.

### Known issues

- After this release, changes in Lathe and Zed don't show up in each other. That's the point, but worth knowing if you use both.
- AI accounts you added before 1.11 still keep their files in Zed's folder. They work fine there; new ones go in Lathe's.
- Dragging a commit onto a branch only works for the branch you have checked out.
- Panels can't go in the editor area as tabs yet.
- Tested by hand on macOS. The Windows credential copy and the AppImage haven't been tried on real machines, and Windows and Linux are otherwise untested.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.11.2-beta - 2026-10-13

Mostly fixes. 1.11.1 never went out, so its two fixes are in here too. If you also have Lathe Stable installed, read the first one.

### Fixed

- **Lathe Stable showing "unable to open database file" after Beta 1.11.0.** Beta's first launch "repaired" the database Lathe Stable still shares with Zed, and Stable couldn't open it after that, so it ran without saving anything. Beta now leaves Stable's database alone. Updating doesn't undo what 1.11.0 already did, so if Stable is broken for you, quit it and rename `db.sqlite.before-lathe-repair` back to `db.sqlite` in `~/Library/Application Support/Zed/db/0-stable/`. Nothing gets lost, since Stable hasn't saved anything to that file since the repair.
- **Add Account not adding the account.** The browser said you were signed in, but Lathe kept the old account. Switching back to Lathe mid-sign-in canceled it. Same thing happened from the command palette.
- **Commits credited to the wrong GitHub user.** Workspaces tied to a collab account were writing a GitHub noreply email built from your Zed id, not your GitHub id, so GitHub didn't link the commits to you. Fixed. Lathe now looks up the real id, and until it has it your git config is used instead.
- **Sign Out quietly switching you to your other account.** Signing out of one saved account made the other one active, so the next sign-in (or restart) used it without asking.
- Running Beta and Stable side by side could swap account names and avatars between them, or flip windows back and forth between accounts.
- **Connecting GitHub for pull requests.** Clicking anywhere outside the dialog after approving in the browser threw the sign-in away.
- Adding a Codex or API-key AI account didn't take effect until you restarted the agent.
- **Rainbow parameter colors** were back to coloring every parameter the same. An upstream merge dropped them again.
- `cmd-+` / `cmd--` / `cmd-0` in the markdown preview zoomed only the preview, not the window.
- **Compact terminal cards** had lost their button and stopped doing anything. Both are back.
- **Save Workspace** and **Save Workspace As…** are back in the File menu.
- Stop Metro and Stop Spotlight in the Mobile panel didn't do anything. Now they close the terminal and stop the process.

### Linux

- Lathe's data moves to `~/.local/share/lathe-data`. It was in `~/.local/share/lathe`, which is also where the installer puts Lathe Stable and wipes on every update. That would've deleted everything the first time Stable updated. The move happens on first launch.
- `uninstall.sh` was removing Zed's folders instead of Lathe's.

### Under the hood

- If copying something from Zed failed on first launch, the retry could give you an empty database and call it done. It now retries properly.
- When Lathe Stable moves to its own folders, it picks up whatever it changed in Zed's folders after Beta copied them, so your newer settings and threads come with it.
- Deleting an older AI account no longer deletes files that Lathe Stable 1.8 still uses.
- Removed a few leftovers from upstream that showed up in settings or keymaps but did nothing.

---

## v1.11.0-beta - 2026-10-12

Lathe finally has its own settings and data folders. Up to now it quietly shared them with stock Zed, and that turned out to break both apps. Your stuff comes along automatically on first launch.

### New

- **Lathe keeps its own data.** Settings, keymaps, themes, extensions, history and accounts now live in Lathe's own folders (`~/.config/lathe` and `~/Library/Application Support/Lathe` on macOS, the `lathe` equivalents on Linux and Windows). First launch copies everything over from Zed's folders. Takes about a second, and you shouldn't need to sign in to anything again. If an account's sign-in can't be carried over, Lathe tells you which one.
- **Open Recent File.** It's next to Open Recent Project in the File menu and on the welcome page (`alt-cmd-shift-o` on macOS). Picking a file opens just that file, not its whole project.
- **Linux AppImage.** Each release now has an `.AppImage` for x86_64 and aarch64. `chmod +x` it and run it. No libfuse2 needed, and in-app updates replace the file in place.

### Fixed

- **Stock Zed's "Failed to load the database file" error.** Recent Zed versions refused the database Lathe shared with them and ran without saving any history. On first launch Lathe repairs Zed's copy, after backing it up next to the original (`db.sqlite.before-lathe-repair`). Restart Zed afterward and your recent projects are back.
- Lathe and Zed kept deleting each other's language server and debug adapter downloads.
- On Linux, Lathe wouldn't start while Zed was running, and the `lathe` command opened files in Zed.
- Windows builds called themselves "Zed" in file properties. Beta showed up as "Zed Dev" with the dev icon.

### Improved

- Long agent threads open faster. Edit diffs and message formatting get built when you scroll to them, not all up front.

### Known issues

- After this, changes in Lathe and Zed don't show up in each other. That's the point, but worth knowing if you use both.
- AI accounts you added before this release still keep their files in Zed's folder. They work fine there; new ones go in Lathe's.
- Tested by hand on macOS. The Windows credential copy and the AppImage haven't been tried on real machines yet.

---

## v1.10.1-beta - 2026-10-10

Fixes for things the last upstream sync broke. Some of them had been quietly broken since an earlier sync, too.

### Fixed

- **Dragging in the git graph works again.** Branch badges moved into the gutter beside the graph in 1.10.0, and dropping a branch there did nothing. Now you can drop it anywhere on a commit's row, and that row lights up while you drag. Both drags also show what you're dragging now, the branch name or the short commit hash.
- In filter mode, branch badges in the graph lined up with the wrong commits.
- **Cmd-+ and Cmd-- zoom the agent panel again.** With focus in the panel they did nothing at all. The panel and the thread text scale with the rest of the window now.
- Agent file links like `main.rs:2` could open the file from the second folder of a multi-folder project instead of the first. Project folders also keep the order you added them in, instead of getting sorted alphabetically.
- Searching the git graph by a full SHA-256 commit hash.
- Commit diffs in shallow clones did a lot of extra work.
- Git commands could mix up a revision and a file path with the same name.
- Remote URLs weren't read correctly for partial clones.
- Creating a worktree from a bare repository.
- "Manage Skills" showed up in the command palette with AI turned off.

### Changed

- Project search runs as you type now, matching upstream. Set `"search_on_type": false` to go back to pressing Enter.

### Known issues

- Dragging a commit onto a branch only works for the branch you have checked out. Dropping it on any other branch is ignored.
- Tested by hand on macOS. Windows and Linux untested.

---

## v1.10.0-beta - 2026-10-08

Synced with upstream Zed (225 commits), and the git graph finally caught up. It had quietly missed every upstream graph change since April. That's fixed, and Lathe's own graph features are all still there.

### Added

- **The git graph got a lot of upstream work back.** Ref badges sit in a gutter beside the lanes. Columns can be hidden from the header's right-click menu. Commit details and the message collapse. Changed files show as a tree or a flat list. And the graph remembers its state across restarts. Dragging branches and commits still works, now on the new badges.
- **File history in the graph.** "View History in Git Graph" in the git panel's right-click menu. The old "View File History" view is still there, both work.
- The commit right-click menu has **View Diff**, **Copy Tag** and a **Custom Commands** section for your git tasks. Same on branch badges, plus Copy Ref Name.
- Create a tag at HEAD or on any commit from the graph.
- **Multi-select in the git panel.** Shift-click and Cmd-click to mark files, then stage, unstage, discard, stash or open them together. An earlier merge had dropped this before it ever shipped.
- File counts on the git panel's section headers. The commit editor can collapse.
- Folders in the git panel's tree view get their own right-click menu.
- Gitiles and self-hosted Gerrit work as git hosts, permalinks included.
- The file finder prefills with your selected text.
- `soft_wrap_indent` setting for how wrapped lines indent.
- `alt-/` shows which bindings can finish a half-typed key chord.
- `markdown_preview.heading_font_weight`, and a font family setting for Mermaid diagrams.
- Markdown preview tabs have **Show Source**.

### Fixed

- Creating a tag didn't show up in an open git graph until something else reloaded it.
- Commit templates that changed on disk stopped getting overwritten by a stale saved draft.
- Commit details and diffs no longer wait behind a fetch that's still running.
- Emoji, symbol and non-Latin fonts rendering as empty boxes.
- Crashes when pasting multiple selections at the end of a file, when typing a partial key chord at end of file, and when editing while an inlay hint request was in flight.
- Cmd-click doing nothing on a symbol named the same as its file.
- Auto-closing brackets got inserted inside comments and strings.
- Prompts to external agents could be slow to send, or not send at all.
- Remote project search could hang. SSH reconnects reuse what they already know about the remote OS.
- `.env` files were highlighted as shell scripts.
- A bunch of smaller Markdown Preview fixes: images in tables and HTML blocks, task list checkboxes, nested code block copying, search highlights covering the text.

### Changed

- Faster startup, a smaller binary, and better multi-cursor and project search performance, all from upstream.
- The outline panel picked up upstream's batched updates and file expand/collapse in place of Lathe's older copy.
- Title bar menus open on click now, not on hover. Set `title_bar.open_menus_on_hover` to get the old behavior back.

### Known issues

- Only the main branch badge on a commit can be dragged. Others are behind the "+N" chip, which has a right-click menu but isn't a drag source.
- Tested by hand on macOS. Windows and Linux untested.

---

## v1.9.0-beta - 2026-10-06

Mostly about layout and accounts. Docks can hold two panels now, every panel can live in the bottom dock, and the AI and git accounts you pick stay with the workspace you picked them in.

### Added

- **Two panels per dock.** Cmd-click a second panel's button and it opens under the first (or beside it, in the bottom dock), with a divider you can drag. Double-click the divider for 50/50. Clicking a button that shares the dock closes just that panel, and clicking it again puts it back where it was, same divider and all. The split is saved per workspace.
- **Drag panels where you want them.** Drag a panel's button off the status bar and drop zones show up for each half of each dock. Right-click a button for the same thing under **Move to**. The old Dock Left / Right / Bottom entries are still there.
- Every panel can go in the bottom dock now, including Project, Git, Outline, Pull Requests, Collab and Agent. Mobile Dev only goes in the bottom dock, since device logs need the width.
- A close button sits next to each open dock's panel buttons, and every button's right-click menu has **Close Left Panels** (or Right, Bottom). Reopening brings back the same panels in the same sizes.
- **Several accounts per git host.** Add a second GitHub, GitLab or Bitbucket account from the account menu and pick which one each workspace uses. Pull requests, reviews and the reviewer picker all follow that choice. Your existing connection carries over as the first account without signing in again.
- **Color swatches and a color picker.** Hex, `0x` hex and `rgb()` / `hsl()` values get a swatch in front of them, language server or not. Click it (or right-click and choose **Pick Color**) to adjust the color. The new value is written back in the same notation, as one undo step. An all-digit `#123` in Markdown or a commit message is left alone, since that's usually an issue number.
- **Refresh** in the project panel's right-click menu re-reads the tree from disk, for the times the file watcher misses something.
- Pull request panel sections can be dragged to the height you want. Double-click the divider to reset one.

### Fixed

- **Switching AI accounts only affects that workspace.** It used to change the account for every window and every running Lathe instance, because the choice was written to the shared settings file. It's now remembered per workspace, and the setting is just the default.
- "My reviews" keeps pull requests you've already approved or requested changes on. They used to disappear once you voted, because GitHub drops you from the requested reviewers list when you submit.
- Autosave and agent edits don't remove imports anymore. "Organize Imports" still sorts on those saves but skips the remove-unused step, so an import that was just added doesn't get deleted before anything uses it. Saving with `cmd-s` works as before.
- Folders symlinked from outside the project pick up file changes, and filesystems that need a polling watcher get one.
- A soft-wrap state that never settled, which left stale ranges on the scrollbar.

### Changed

- A round of performance work aimed at things that ran on every frame or keystroke. The account menus stopped re-reading their files on every render. The git panel, git graph and outline cache what they build instead of rebuilding it per keystroke. Mobile Dev only polls for devices while it's open or the project is a mobile one. And the terminal's awaiting-input pulse no longer re-renders the whole pane on every frame, which mattered with an agent waiting for hours.

### Known issues

- Panels can't go in the editor area as tabs yet.
- Tested by hand on macOS. Windows and Linux untested.

---

## v1.8.0 - 2026-10-04

The first stable release since 1.4.0. Everything from the 1.5.0, 1.6.0 and 1.7.0 betas is in here, plus a round of fixes that came out of testing them.

### Added

- **Jupyter notebooks you can actually rely on.** The notebook editor is no longer labeled experimental. It's still off by default, so turn on **Enable Notebook Editor** under Languages and Tools > Jupyter Notebooks. Most of the work was making saves safe:
  - Saving keeps your outputs. Images, tables, JSON and markdown outputs used to be rebuilt from whatever was on screen, so they vanished on every save, widgets included.
  - An untouched notebook saves to an empty `git diff`. Key order, indentation and the trailing newline all come from the file you opened.
  - Cells share one document, so a name defined in cell 1 resolves in cell 2. `cmd-shift-y` flips between the cells and the whole notebook as a `# %%` script, and you can edit either. Completions inside cells also need `jupyter.language_server_sidecar`, which writes a hidden `.yourfile.lathe.py` next to the notebook and deletes it on close.
  - The kernel picker waits for environment discovery before choosing, prefers an environment inside your project over whatever your active Python toolchain points at, and remembers what you picked by interpreter path. Pick an environment without ipykernel and it offers to install it.
  - A kernel log button sits next to restart and interrupt. Outline (`cmd-shift-o`), breadcrumbs and `cmd-f` work across cells.
- **PlatformIO support.** Open a folder with a `platformio.ini` and you get syntax highlighting and an outline for that file, build, upload, monitor, test, check and clean tasks per environment, and clangd code intelligence from the compilation database PlatformIO generates. Lathe drives the `pio` CLI but doesn't bundle it, so install PlatformIO Core first.
- **Choose whether Lathe downloads binaries for you.** Opening a file can make Lathe fetch Node.js, npm packages, a language server, a debug adapter, or files an extension asks for. Set `allow_binary_downloads` to `false` and it only uses what's on your `$PATH` or configured explicitly. Set `prompt_before_binary_downloads` to `true` and each download asks first. Approvals are remembered in `approved_binary_downloads` by name and version; approve with "always" (version `"*"`) to cover future updates. Downloads are still allowed by default.
- `keep_inactive_workspaces` keeps a project's tabs and terminals alive after you switch the window to another project or worktree, so switching back puts you where you were. On by default. It's also in the settings UI under Workspace Restoration.
- Agent panel terminals come back after a restart, with their titles and working directories.
- Subagents can run on a specific model. `spawn_agent` takes an optional `model` in `provider/model-id` form.
- The pull request view has a refresh button at the right of its title row.

### Fixed

- **Cmd-clicking a link opens it again.** With definitions set to open in a peek, which is the default, cmd-clicking a URL or a file path showed "No definitions found" instead of opening it. Only symbols go to the peek now.
- **Switching worktrees no longer closes your other projects.** With several repositories open in one window, switching one of them to a worktree dropped all the others. Switching back then landed you in a fresh workspace, so your terminals and tabs were gone. Now only the repository you switched changes, and switching back restores the original workspace.
- Notebook fixes. A slow kernel isn't treated as a dead one: startup used to give up at 45 seconds and stamp "cell could not be executed" on cells the kernel went on to run anyway. Discarding a hunk in the git panel sticks instead of being written back on the next autosave. Opening a notebook doesn't rewrite its `kernelspec`. Save As works.
- Edit prediction popovers and right-click menus were clipped to the editor's own bounds. Hard to notice in a full-size editor, obvious in a two-line notebook cell. Fixed everywhere.
- Hovering a document link shows its tooltip again, and a few text layout fixes from upstream are back: wrapped lines don't start with closing punctuation, text is pixel-snapped, and lines full of invisible characters stay within the line length limit. Earlier upstream merges had quietly dropped all of these.
- Shift+Up, Shift+Down and the other Shift navigation keys reach programs in the terminal's alternate screen (vim, less, htop) instead of scrolling Lathe's terminal history.
- AltGr characters type correctly in the terminal when `option_as_meta` is on.
- Edit > Copy and Edit > Paste work when a terminal is focused.
- C++ members under `public:`, `private:` and `protected:` indent one level deeper than the specifier, method bodies included.
- Deleted-line markers in the git gutter stay visible at small `git_gutter_width` values.
- Right-clicking selected text in an agent message offers "Copy" and "Copy as Markdown".
- Opening a pull request that's already open focuses its tab and reloads it, instead of opening a second one.
- Theme customizer changes survive a restart. They're saved to `theme_overrides` for the active theme.

### Changed

- Merged upstream Zed through `7fdb97cad5`: 186 commits, including ACP session notices, faster incremental diff loading, JSONL and NDJSON tabular preview, and `"..."` inheritance in `hidden_files`, `terminal.path_hyperlink_regexes` and the edit prediction exclusion settings.
- Turning off AI no longer throws away your other workspaces. It used to: with `disable_ai` on or the agent disabled, every project or worktree switch closed the previous workspace with its tabs and terminals. That's now controlled by `keep_inactive_workspaces` alone, and the threads sidebar is the only thing that goes away with AI.
- Git no longer depends on libgit2. Repositories open lazily, so a folder that becomes a repository after you open it gets picked up, and bare repositories work.
- Bug and crash report templates point at Lathe's own issue tracker, discussions and r/LatheEditor.

### Known issues

- Three settings currently do nothing: `outline_panel.multi_buffer_hide_symbols`, `instrumentation.performance_profiler.enabled` and `on_new_window`. An audit of past upstream merges turned these up along with other upstream changes that never made it into Lathe. They'll be restored in a later release.
- Notebooks: splitting one across two panes is disabled, `.ipynb` tabs don't reopen after a restart, LaTeX output shows as source, and widgets show as a placeholder.
- The notebook and worktree changes were tested by hand on macOS. The editor, workspace, git and notebook test suites pass on macOS. Windows and Linux are untested.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.7.0-beta - 2026-10-01

Jupyter notebooks. Upstream's editor loses data when you save it, so this is mostly a rewrite of the parts that matter.

### Added

- **Saving doesn't eat your outputs anymore.** This is the one that mattered. Outputs used to be rebuilt from whatever had been drawn on screen, so images, tables, JSON and markdown output vanished on every save. Widgets went with them. Each output now keeps its original nbformat next to the rendered view, and what goes back to disk is what came off it.
- **An untouched notebook saves to an empty `git diff`.** Key order, indentation, trailing newline, all kept from the file you opened. Opening one and hitting `cmd-s` used to rewrite the whole thing.
- **Cells share one document.** They're excerpts over a single buffer holding the notebook as a `# %%` script, so `message` defined in cell 1 resolves in cell 2. `cmd-shift-y` flips between the cell list and that script, and you can edit either. Completions inside cells need `jupyter.language_server_sidecar` as well, because Zed only attaches language servers to buffers backed by a real file. That setting writes a hidden `.yourfile.lathe.py` next to the notebook and deletes it when you close the tab.
- Kernel picking got a lot less dumb. Waits for environment discovery to finish before choosing, so your project's `.venv` doesn't lose a race to a bare `python3`. Prefers an environment inside the worktree over whatever your active Python toolchain happens to point at. Remembers the choice by interpreter path, since half your venvs are named "Python 3.12.13 (venv)". And if the environment you pick has no ipykernel, it offers to install it instead of starting something that can't work.
- **A kernel log button** in the status bar, next to restart and interrupt. Kernel stdout and stderr were always going to the app log. Now there's a way to get there.
- Outline (`cmd-shift-o`), breadcrumbs, and `cmd-f` all work across cells.

### Fixed

- **A slow kernel isn't a dead one.** Startup gave up at 45 seconds, dropped the kernel and stamped "cell could not be executed" on whatever was queued. Except the kernel process was already up by then and ran those same cells, so you'd get the correct answer with an error sitting underneath it. 45 seconds now just warns in the log. Only five minutes actually gives up.
- **Discarding a hunk in the git panel sticks.** The open notebook follows the file on disk. Before, it held onto the pre-discard metadata and the next autosave wrote all of it back, so your discard quietly undid itself as soon as focus changed.
- **Opening a notebook doesn't dirty it.** Launching a kernel rewrote `metadata.kernelspec`, which then showed up in `git diff` on a file you'd done nothing to.
- **Save As works.** `cmd-shift-s` did nothing at all. The implementation was there the whole time, the trait just said no.
- Edit prediction popovers and right-click menus were clipped to the editor's own bounds. Never noticeable in a full-window editor. Very noticeable in a two-line notebook cell. This one affects every editor, not just notebooks.

### Known issues

- Splitting a notebook across two panes is disabled. The two sides diverge.
- `.ipynb` tabs don't reopen after you quit and come back.
- LaTeX output shows as source. Widgets show as a placeholder.
- Tested by hand on macOS against a real notebook and a live Python kernel. Windows and Linux untested.

---

## v1.6.0-beta - 2026-09-25

A beta that catches Lathe up with 186 upstream Zed changes, adds PlatformIO support, and brings back several upstream fixes that earlier merges had dropped.

### Added

- **PlatformIO support.** Open a folder with a `platformio.ini` and you get syntax highlighting and an outline for the file, build, upload, monitor, test, check and clean tasks for each environment, and clangd code intelligence from the compilation database PlatformIO generates. Lathe drives the `pio` CLI but doesn't bundle it, so install PlatformIO Core first. See the PlatformIO page in the docs.
- **Agent panel terminals survive a restart.** Terminals you open in the agent panel are restored, with their titles and working directories, the next time the workspace opens.
- **Subagents can run on a specific model.** `spawn_agent` takes an optional `model` in `provider/model-id` form. Without it, a subagent uses the configured subagent model, or else its parent's model, as before.

### Fixed

- **Shift+Up, Shift+Down and the other Shift navigation keys reach programs running in the terminal's alternate screen** (vim, less, htop and similar) instead of scrolling Lathe's terminal history.
- **AltGr characters type correctly in the terminal** when `option_as_meta` is on, instead of being sent as meta sequences.
- **Edit > Copy and Edit > Paste work when a terminal is focused.**
- **C++ members under `public:`, `private:` and `protected:` indent one level deeper than the specifier**, including method bodies inside those sections.
- **Deleted-line markers in the git gutter stay visible** at small custom `git_gutter_width` values.
- **Right-clicking selected text in an agent message offers "Copy" and "Copy as Markdown"**, matching the rest of the editor.
- Git no longer depends on libgit2. Repositories are opened lazily, so a folder that becomes a repository after you open it is picked up, and bare repositories are supported.

### Changed

- Merged upstream Zed through `7fdb97cad5`: 186 commits, including ACP session notices, faster incremental diff loading, JSONL and NDJSON tabular preview, and `"..."` inheritance in `hidden_files`, `terminal.path_hyperlink_regexes` and the edit prediction exclusion settings.

### Known issues

- Verified with `cargo check --workspace --all-targets`, `./script/clippy`, and the test suites for the changed crates on macOS. None of it has been exercised in a running build, and Windows and Linux are untested.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.5.0-beta - 2026-09-24

A beta for control over what Lathe downloads, theme customizer edits that persist, and a pull request view that refreshes in place.

### Added

- **Choose whether Lathe downloads binaries for you.** Opening a file can make Lathe fetch Node.js, npm packages, a language server, a debug adapter, or files an extension asks for. Set `allow_binary_downloads` to `false` and Lathe uses only what is on your `$PATH` or configured explicitly, reporting an error instead of downloading. Also set `prompt_before_binary_downloads` to `true` and each download is put to you in one modal instead of refused. Approvals are remembered in `approved_binary_downloads` by name and version, so an update asks again; approve with "always" (a version of `"*"`) to allow future updates too. Downloads are allowed by default, as before.
- **Refresh a pull request from its tab.** The pull request view has a refresh button at the right of its title row.

### Fixed

- **Clicking a pull request that is already open no longer opens a second tab.** Opening it from the pull request panel or picker focuses the existing tab and reloads it.
- **Theme customizer changes survive a restart.** Edits are saved to `theme_overrides` for the active theme in settings.json, and the customizer picks up saved overrides when it opens or the theme changes.

### Changed

- The bug and crash report templates now point at Lathe's own issue tracker, discussions, and r/LatheEditor instead of upstream Zed.

### Known issues

- This release was verified with `cargo check --workspace --all-targets` on macOS. None of it has been exercised in a running build, and Windows and Linux are untested.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.4.0 - 2026-09-22

The first stable release since 1.2.0. It carries everything from the 1.3.0 beta plus the work below.

### Added

- **Open an editor, or move a terminal, into a window of its own.** "Open in New Window" on an editor tab puts a second view of the file in a window of its own. The terminal tab and context menus gain "Move to New Window", which moves the terminal itself, so its process, scrollback and input state come with it. Closing that window hands the terminal back to the pane it came from rather than killing it.
- **Send either one to a window that is already open.** With another window open on the same project, the editor tab menu offers "Open in Window" and the terminal menus offer "Move to Window", each listing those windows by what they are showing. An editor is copied, so the original tab stays where it is; a terminal moves, and can still be handed back afterwards. One window can hold terminals from several panes, and each goes back to its own pane on the way out.
- **Force delete a branch from the git panel.** Deleting an unmerged branch from the Explorer used to fail with a bare error toast and no way forward. `git branch -d` is still what runs first, because its refusal is the only warning that a branch has unmerged work, but that refusal now becomes a prompt offering the force delete. The local half of **Delete on `<remote>` and locally** goes through the same path.
- **A pull request you have reviewed stops showing as updated.** Submitting a review moves the pull request's timestamp on the host, so reviewing a row lit its updated indicator for your own action and left it lit until you opened the row again. A row carrying your approval or your requested changes now clears it. A comment-only review settles nothing, so it stays flagged.

### Fixed

- **A terminal starting in the background no longer steals focus from a modal.** A terminal took focus whenever its tab was activated, overriding the pane's decision not to focus it, so one finishing its startup while the command palette was open dismissed what you had just opened. Whether an activated item takes focus is the pane's call again.
- **Handing a terminal back no longer leaves a blank section behind.** When the terminal had a pane to itself in the window it had been moved to, returning it left that pane in the layout holding nothing, drawn as a band you could resize but not close. The pane now closes with the terminal. Returning a terminal that came from the center no longer reveals the terminal panel either, which used to leave an empty dock open.

### Known issues

- This release was verified with `cargo check --workspace --all-targets` and the test suites of the changed crates (`workspace`, `terminal_view`, `pr_ui`, `git_ui`) on macOS. Windows and Linux are untested, and the new window behavior was exercised in a running build on macOS only.
- Two draft-reload tests in `agent_panel` (`test_draft_promotion_creates_metadata_and_new_session_on_reload` and `test_new_draft_survives_reload_when_real_thread_is_active`) were already failing at the 1.2.0 tag. They were not revisited for this release.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.3.0-beta - 2026-09-18

A scoping pass on the agent panel's history, a compact mode for terminal cards, and the experimental notebook editor behind a setting.

### Added

- **Compact terminal cards.** A collapsed terminal card spent its header on the working directory and still carried the full command block underneath, so a thread that ran many commands scrolled mostly as boilerplate. With `agent.compact_terminal_cards` on, a collapsed card is one line: the header shows the first line of the command and the command block appears only once the card is expanded. A toggle sits with the other composer controls, and `agent: toggle compact terminal cards` writes the setting, so the choice sticks. Off by default.
- **The experimental Jupyter notebook editor can be switched on.** It previously registered only behind an upstream feature flag or the `LOCAL_NOTEBOOK_DEV` environment variable, neither of which a normal build can reach, so opening an `.ipynb` file always fell through to raw JSON. `jupyter.notebook_enabled`, also under **Languages & Tools -> Jupyter Notebooks** in the settings UI, now registers it as well, and takes effect in the running app. It is still experimental, and because upstream has no way to unregister an editor, turning it back off needs a restart.

### Fixed

- **Prompt history no longer bleeds between threads and windows.** The up/down walk read one app-global list, so prompts typed in another project's thread turned up in a composer that had nothing to do with them. History is now kept per thread per window. The editors for past and queued messages keep ordinary cursor movement, as before.
- **Thread History follows the window you are looking at.** Scoping it by worktree paths still let two windows opened on the same paths share one history, and switching the active workspace left the list showing the threads of the workspace you just left. Threads now record the workspace they were created in and the archive rebuilds when the active workspace changes. Threads written before this release fall back to the old path match, so nothing disappears from history.
- **A linked git worktree no longer duplicates its pull requests.** Opening a worktree beside its main checkout gave the pull request panel two sections listing the same pull requests, the second titled after the worktree directory (`.pr12-review` rather than `offline-mode`). Sections are now built per repository, with the main checkout providing the name and the remote.

### Changed

- The README opens with a download link and the Homebrew command, instead of leaving them halfway down the page.

### Known issues

- This release was verified by a full `cargo check --workspace --all-targets` and the test suites for the changed crates (`agent_ui`, `sidebar`, `pr_ui`, `repl`) on macOS. None of it was exercised in a running build, and Windows and Linux are untested.
- Two draft-reload tests in `agent_panel` (`test_draft_promotion_creates_metadata_and_new_session_on_reload` and `test_new_draft_survives_reload_when_real_thread_is_active`) fail. They fail at the 1.2.0 tag as well, so they predate this release rather than coming from it, and the underlying draft-restore behavior is unverified in both.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.2.0 - 2026-09-15

A large upstream merge, plus per-workspace panel placement and a quieter pull request panel.

### Added

- **Thread History is scoped to the current workspace again.** Upstream renamed the global Archive view to "Thread History" in April and deleted the per-workspace history view five days later, so the panel had been listing threads from every project at once. History now shows only the active workspace's threads, archived ones included. A thread opened in a linked git worktree still appears under its main workspace, so worktree isolation does not hide it.
- **The agent panel's dock is remembered per workspace.** Moving the panel used to write the global `agent.dock` setting, which every window shares, and which every release channel shares too because the config directory ignores the channel. Dragging the panel in one project moved it in all of them, stable and beta alike. A workspace that has been customized now records its own placement and stops following the setting. **Use Default Position** in the panel menu, offered only once a workspace has a placement of its own, hands it back to the global setting.
- **Mark a pull request as read without opening it.** Right-click a flagged row in the pull request panel and choose **Mark as Read** to clear its updated indicator. The entry appears only on rows that are actually flagged, and a pull request that changes again afterwards is flagged again, exactly as if it had been opened.

### Fixed

- **The welcome page scrolls again.** Its padding, maximum width and vertical centering sat on the scrolling container itself, so content taller than the window was clipped rather than reachable. Those now sit on an inner column that centers horizontally, and the scroll position is tracked.
- **Cycling a panel to its next dock no longer risks a panic.** Moving the focused panel resolved its destination while the dock was already being updated, which is the shape GPUI rejects with a double lease. The destination is now read first, through a new `next_position`, and applied outside the update.
- Relocating a panel between docks runs through one shared path, so the global-settings route and the new per-workspace route cannot drift apart in how they preserve a panel's visibility and size.

### Changed

- **Merged upstream Zed**, 262 commits. Among them: an LSP call hierarchy modal and its `call_hierarchy.modal_max_width` setting, agent client protocol 2.1, and Rust 1.98.1.

### Known issues

- This release was verified by a full `cargo check --workspace --all-targets` on macOS. The new per-workspace placement test compiles, but the test suite was not run before the cut, and Windows and Linux are unexercised.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.1.0 - 2026-09-11

The first stable release since 1.0.0. It carries everything from the 1.0.1 beta plus the work below.

### Added

- **The composer walks your prompt history.** Up and down in the agent panel's composer recall prompts you have already sent, the way a shell walks its command history. The keys only take over at the edges of the text, so moving around inside a multi-line prompt is unchanged, and the editors for past and queued messages keep ordinary cursor movement.
- **Create a Pixel Tablet Android virtual device from the Mobile panel.** The Android device menu offers separate phone and tablet AVDs, both on API 35 and the host-matched Google APIs image.
- **Window zoom scales the whole window.** `Cmd +`, `Cmd -`, and `Cmd 0` resize the editor, terminal, project panel, git panel, git graph, pull requests, agent panel, git commit editor, and markdown preview together, scoped to the active window.
- **Checking out a commit works in remote and collab projects.** Detached HEAD checkout from the history view and commit graph previously refused anything but a local repository.

### Fixed

- **The pull request panel shows every repository again.** It built its section list and then failed to store it, so a workspace with two repositories reported none at all and the panel read "No repositories open". Each repository now gets its own collapsible section with its open count.
- **Clicking an agent link to a file outside the project opens it.** The link was resolved against the project's worktrees, which return nothing for an outside path, so the click silently did nothing.
- **Creating a worktree from a remote branch starts from the remote tip.** The branch target resolved to `origin/main` rather than the fully qualified `refs/remotes/origin/main`.
- **Rainbow parameter highlighting works again.** Bundled themes define the eight parameter colors, and declarations and references take the color for their ordinal position.
- **Grouped runnable queries emit every runnable again**, restoring table-test discovery and per-item metadata.
- **The Mobile panel runs on Windows.** It shelled out to `./gradlew`, a Unix shell script, and opened terminals through `$SHELL -lic`, neither of which exists there; sixteen further lookups asked for `sdkmanager`, `avdmanager`, `adb`, `java`, and `emulator` by their Unix names and so reported the Android toolchain as missing rather than erroring.
- **New pull requests take their branches from pickers** rather than free text, so a mistyped branch cannot be submitted. Recorded API fixtures now cover close, reopen, reviewer requests, draft transitions, merge, and squash merge across GitHub, GitLab, and Bitbucket.
- **The peek view leaves hover popovers and context menus above it**, and is covered in multibuffers and splits; Vim `j` and `k` move its location list.
- Logging out of an agent that supports it now actually clears the authenticated state.

### Changed

- The feature catalog moved out of the README to [docs/features.md](docs/features.md), which doubles as the checklist of what this fork changes on top of upstream.

### Known issues

- **Two tests covering restoration of a promoted draft thread across a reload fail**, and have failed since v1.0.0. Reloading the agent panel may not restore a promoted thread's messages. Not a new regression, but not yet fixed.
- One `project` test covering git state refresh for a bare `.git` file fails, also pre-existing.
- **The Windows fixes above are verified by compilation and tests only.** No Windows machine has run them. Linux is likewise unexercised, as are the window zoom and peek view on both.
- Detached HEAD checkout over collab ships without a test; the fork has no remote-git test harness. The other sixteen Lathe git operations still refuse to run on remote projects.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.0.1-beta - 2026-09-10

Zoom now scales the whole window, not just the editor.

### Changed

- **`Cmd +`, `Cmd -`, and `Cmd 0` zoom every piece of text in the window together:** the editor, terminal, project panel, git panel, git graph, pull requests, agent panel, git commit editor, and markdown preview. The View menu zoom items and `Cmd + scroll-wheel` (when mouse-wheel zoom is enabled) do the same. Before this only the editor and terminal changed size, and the agent panel and markdown preview zoomed on their own when focused.
- The zoom is still scoped to the active window, so two windows side-by-side can be zoomed independently.
- The persisted zoom items write both `buffer_font_size` and `ui_font_size` to `settings.json`. Agent, git commit, and markdown preview sizes are only written when you had already set them. Reset clears all of them.

### Added

- **Create a Pixel Tablet Android virtual device from the Mobile panel.** The Android device menu now offers separate phone and tablet AVDs, both using API 35 and the host-matched Google APIs image.

### Fixed

- **Rainbow parameter highlighting works again.** Bundled themes define the eight parameter colors, and declarations and references now receive the color for their ordinal parameter position.
- **Grouped runnable queries emit every runnable again.** The buffer now uses the grouped-runnable resolver, restoring table-test discovery and per-item metadata.
- **New pull requests use local branch pickers.** The source and target branches must come from the repository's local branch list, so a mistyped branch cannot be submitted. Recorded API fixtures now cover close, reopen, reviewer requests, draft transitions, merge, and squash merge for GitHub, GitLab, and Bitbucket.
- **Peek leaves hover popovers and context menus above it.** The peek remains above the minimap at deferred-draw priority 0, while the editor draws those overlays at higher priorities. Peek is also covered in multibuffer and split editors, and Vim `j` and `k` now move its location list.
- **The Mobile panel can run on Windows.** It could not before: the panel shelled out to `./gradlew`, which is a Unix shell script rather than Windows' `gradlew.bat`, and it opened every terminal through `$SHELL -lic`, which on Windows means an unset variable, a `/bin/zsh` that does not exist, and POSIX-only flags. Sixteen further lookups asked for `sdkmanager`, `avdmanager`, `adb`, `java`, and `emulator` by their Unix names, so the Android toolchain reported itself as not installed rather than erroring. Run hints are also scraped from READMEs that spell the wrapper `gradlew.bat`.
- **Checking out a commit works in remote and collab projects.** Detached HEAD checkout from the history view and commit graph previously failed on anything but a local repository.
- An AWS profile whose name was empty wrote a malformed `credential_process` line into the AWS config. Empty names are now refused.

### Known issues

- The Windows fixes above are reasoned from Android's published tool layout and are verified by compilation and tests, but **no Windows runtime behavior has been observed**. Linux is likewise unexercised, as are the window zoom and peek view on both.
- Detached HEAD checkout over collab ships without a test; the fork has no remote-git test harness. The other 16 Lathe git operations still refuse to run on remote projects.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v1.0.0 - 2026-09-01

Lathe moves to 1.0 and off the version number it inherited from upstream. This release contains the peek view that shipped as v0.236.35-beta, plus the two fixes below.

### Added

- **Peek view for LSP results.** Find All References, Go to Definition, Declaration, Implementation and Type Definition open a panel below the cursor line instead of a new tab: a preview of the selected location beside the list of locations, grouped by file. Arrow keys move through the list and the preview follows; Enter or a double click goes to a location and closes the peek; Escape or clicking back into the code dismisses it. Cmd-click opens it too, since going to a definition is the case where being sent to another tab is least wanted. Alt-cmd-click still splits the pane.
- **The divider between the two panes can be dragged**, and where you leave it is remembered by later peeks and across restarts.

### Changed

- `lsp_results_location` defaults to `peek` rather than `multi_buffer`. Set it back to `multi_buffer` for the previous behaviour, or `picker` for the filterable list.
- In `peek`, a single result is shown in the peek rather than opened directly, which is the point of the mode. `multi_buffer` and `picker` still go straight to it.
- **Version numbers restart at 1.0.0.** Lathe had been carrying upstream Zed's 0.236.x line, which said nothing about the fork. Updating from 0.236.x is offered as normal.

### Fixed

- The peek list renders only the rows on screen. Before this, a symbol with thousands of references laid out every row on every frame.
- Vim mode no longer drives the editor behind the peek. The peek now declares the `menu` key context, which is what Vim's normal-mode bindings key off, so `j` and `k` move through the list rather than the buffer underneath.

### Known issues

- Tested on macOS, in ordinary single-file editors. The peek inside a multibuffer (project search results, diffs), in a split, and on Linux and Windows is unexercised. The Vim fix is reasoned from the keymap rather than confirmed by use.
- The peek paints above everything else in the editor. If it covers a hover popover or a context menu, that is why.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.35-beta - 2026-09-01

Finding references no longer takes you out of the file you are reading.

### Added

- **Peek view for LSP results.** Find All References, Go to Definition, Declaration, Implementation and Type Definition now open a panel below the cursor line instead of a new tab: a preview of the selected location beside the list of locations, grouped by file. Arrow keys move through the list and the preview follows; Enter or a double click goes to a location and closes the peek; Escape or clicking back into the code dismisses it. Cmd-click opens it too, since going to a definition is the case where being sent to another tab is least wanted. Alt-cmd-click still splits the pane.
- **The divider between the two panes can be dragged**, and where you leave it is remembered by later peeks and across restarts.

### Changed

- `lsp_results_location` defaults to `peek` rather than `multi_buffer`. Set it back to `multi_buffer` for the previous behaviour, or `picker` for the filterable list.
- In `peek`, a single result is shown in the peek rather than opened directly, which is the point of the mode. `multi_buffer` and `picker` still go straight to it.

### Known issues

- The list renders every result rather than only the visible ones. Find All References on a symbol with thousands of hits is expected to be slow.
- Testing was on macOS, in ordinary single-file editors, without Vim mode. The peek in a multibuffer (project search results, diffs), under Vim mode, in a split, and on Linux and Windows is unexercised.
- The peek paints above everything else in the editor. If it covers a hover popover or a context menu, that is why.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.34-beta - 2026-08-27

Fixes a data-loss class of bug that only shows up when two Lathe channels run at the same time, and finishes the pull request panel's separation between reviewing someone's work and acting on your own.

### Fixed

- **Running two channels side by side no longer breaks the running one.** Every channel shared one Node.js installation and npm cache under `node/`, and every launch reset it. Starting Beta therefore deleted the packages that an already-running Stable was executing from, and vice versa. Language servers and agent servers spawned before the wipe kept running against paths that no longer existed. Codex showed this as a total loss of tools: it could still talk, but every tool call failed to spawn its `codex-code-mode-host` sidecar, and it reported the workspace as unavailable. Each channel now gets its own `node/<channel>` directory.
- Creating a Bitbucket pull request as a draft ignored the draft option and produced an ordinary pull request.

### Improved

- **Author-only actions move out of the button row on pull requests you did not open.** Merge, Squash & merge, the draft toggle, and Decline / Close are collected under a **More** menu, so a destructive action is no longer one stray click away from **Approve**. They remain fully available, and the host still decides whether you are permitted to use them. Lathe resolves authorship from the account you connected; when the token cannot report an identity, the buttons stay in the row rather than being demoted for someone who may well be the author.

### Upgrade note

- The first launch after updating re-downloads the npm packages for your language servers and agent servers, because they now live under a per-channel directory. The old `node/` directory is left in place on purpose rather than cleaned up, since removing it would be the same cross-channel wipe one last time against a build that has not updated yet. Once every channel on the machine is on 0.236.34 or later, it is safe to delete by hand: `~/Library/Application Support/Zed/node/cache` and `~/Library/Application Support/Zed/node/node-v*` on macOS.

### Known issues

- The channel fix is verified by build and static analysis only. It has not been exercised by running two updated channels concurrently.
- The pull request overflow menu has not been exercised against a live host, and neither had the reviewer, draft and close work shipped in v0.236.33-beta. Declining and requesting reviewers on Bitbucket remain the only live-tested write paths.
- The New Pull Request dialog's branch fields are still free text rather than pickers.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.33-beta - 2026-08-26

Completes the write side of the pull request panel. Until now it could open, review, comment on and merge a pull request, but every path that did not end in a merge was missing: it listed closed pull requests while offering no way to close one, and showed reviewers with no way to request one.

### Added

- **Request reviewers** from the pull request view, through a searchable multi-select list of the accounts the host will accept. People are listed by name where the host reports one, with their handle underneath. Bitbucket and GitLab report real names; GitHub's collaborator listing does not, so it shows logins.
- **Close, decline and reopen** pull requests, using each host's own wording. Bitbucket calls it declining.
- **Convert to draft** and **Mark ready for review**, on hosts that model drafts.

### Improved

- Draft pull requests are now clearly marked. Draft previously rendered in the faintest grey available, in both the list and the detail header, and greyed out the row icon as well, which made a draft the least noticeable entry in a list of pull requests. It is now an amber badge in both places, shown alongside the state rather than replacing it, since a pull request is both open and draft.

### Fixed

- Creating a Bitbucket pull request as a draft ignored the draft option and always produced an ordinary pull request.

### Notes for reviewers on Bitbucket

Requesting a review reads the pull request first and merges into its existing reviewer list, because Bitbucket's update replaces that list wholesale. Adding someone will not displace reviewers already assigned.

### Known issues

- Draft transitions, reopening, and everything on GitHub and GitLab have not been exercised against a live host. Declining and requesting reviewers have been, on Bitbucket.
- The New Pull Request dialog's branch fields are still free text rather than pickers.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.32-beta - 2026-08-26

A fix release for the pull request panel shipped in v0.236.30-beta. Two of these were dead on arrival and only surfaced once the panel was used against a real repository.

### Fixed

- The panel's **New pull request** and **Reconnect** buttons did nothing when clicked. Both dispatched through the app rather than the window, which re-enters the active window's update from inside that same update; the failure was swallowed and left only a log line. Reconnect had been broken since the panel was rewritten and is reachable only with an expired credential, which is why it went unnoticed.
- API base URLs for self-hosted hosts discarded the scheme and port of the configured base URL. A GitHub Enterprise instance on a non-default port, or a self-managed GitLab reachable only over plain HTTP internally, addressed an endpoint nothing was listening on. Both now preserve scheme, host and port.

### Improved

- Inline review comments show a calendar date instead of the host's raw ISO-8601 timestamp.
- The pull request header reads `3 files, +25` rather than `3 file(s), +25 -0`.
- A reviewer who commented without a verdict gets a distinct icon, instead of a dash that read as stray punctuation and was indistinguishable from a reviewer who had not looked yet.

### Known issues

- Creating a pull request from the panel has not been exercised against a live host.
- The New Pull Request dialog's branch fields are free text rather than pickers.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**.

---

## v0.236.31-beta - 2026-08-25

An upstream sync, bringing Lathe up to date with 78 commits from Zed. The pull request panel and Windows code signing shipped in v0.236.30-beta and are unchanged here.

### Security

- Updated Wasmtime's WASI implementation to fix a filesystem sandbox escape. This affects extensions, which run in that sandbox.
- Disabled one-time code autofill in Lathe's text inputs.

### Editor

- Fixed `fold_at_level` folding a function's arguments instead of its body.
- Oversized LSP hover contents are truncated before display rather than overflowing.
- The gutter repaints immediately when bookmarks change.
- Format-on-save no longer runs against read-only files.
- Overlapping range-formatting results are deduplicated properly.
- `lsp_results_location` is now respected for go-to-declaration and go-to-type-definition.
- Added a configurable debounce timeout for inline completions.

### Git

- Fixed a crash when selecting collapsed sections in the git panel.
- Added an action to toggle the diff base.
- Added Tangled as a git hosting provider.
- Recursive blaming no longer emits useless toasts.
- Fixed global gitignore matching outside the worktree root.

### Markdown

- Improved preview typography and inline code rendering.
- Restored fallback language highlighting for untagged code blocks.

### Debugger

- Continue Program and Continue Thread are now separate actions.
- Python's locator passes through `env` variables from the task template.

### AI

- Added Gemini 3.5 Flash-Lite and removed deprecated Gemini models.
- Added the Baseten provider.
- Copilot Chat supports data-resident GitHub Enterprise.
- Provider rejection details are preserved instead of discarded, and transport errors now include the host.
- Failed agent connections can be reloaded from the panel.
- The most recently selected agent persists correctly.

### Platform

- **Linux**: demand-driven Wayland render loop, prewarmed font match caches, several X11 fixes, and GLib is no longer bundled in release archives.
- **Windows**: support for Restart Manager shutdown.
- **macOS**: fixed session restore when a window is closed with the X button.

### Workspace

- Recent navigation history persists across sessions.
- The terminal panel registers before serialized terminals are restored.
- Terminals no longer steal focus from an open modal.
- Undoing a rename removes directories the rename created.
- Added a Clear button to the settings search field.

### Known issues

- The pull request panel's GitLab and GitHub Enterprise support has not been exercised against a live host.
- Windows installers are signed, but SmartScreen still shows a reputation prompt. Choose **More info**, then **Run anyway**. The dialog names Christopher Paterson as the publisher rather than reporting an unknown one.

---

## v0.236.17-beta - 2026-06-16

GitKraken-parity push for the Git panel, plus a worktree-aware debug scenario fix.

### Git Explorer panel

- Branches in the Local and Remote sections render as a collapsible folder tree, splitting names on `/`. So `feature/auth/login` and `feature/auth/signup` collapse under a single `feature/auth/` folder that you can fold or expand. Folders show counts of contained branches and remember their open/closed state per section.
- When the filter input is active the tree flattens to a plain list so filter results stay legible.
- Explorer context menus thread a weak handle back to the panel, so successful branch operations (checkout, merge, rebase, delete) refresh the explorer data immediately instead of leaving stale rows.

### Branch from commit

- New BranchFromCommitModal: from any commit in the Git history view, create a new branch off that revision without first checking out.

### Detached HEAD checkout

- New `change_to_commit(revision)` on the repository backend, project GitStore, and FakeGitRepository. The UI can now check out an arbitrary SHA into detached HEAD from the history view. Local repositories only in this release; collab projects bail with a clear error.

### Interactive rebase modal

- Layout reworked. Drag-and-drop reordering of commits, and per-row action affordances (pick, squash, edit, drop) inline.

### Git graph

- Expanded edge routing and lane assignment so wider histories lay out without crossings.

### Debug scenarios

- When a task source is a worktree, the resulting debug scenario context is taken from that worktree's task context (and worktree id) rather than the globally-active one. Scenarios from every worktree context are now surfaced, not just the first.

### Release tooling

- `script/release-fork` now tags `-beta`, `-pre`, and `-rc` versions as GitHub pre-releases automatically.

---

## Lathe Beta, AI Account Switcher

A feature for managing multiple subscription-authenticated identities (accounts) per AI agent, with per-workspace binding, brand-accented UI, and per-account conversation history.

### Highlights

- **Multi-account support** for the three Tier A ACP-mode CLI agents: Claude Code, Gemini CLI, Codex CLI.
- **Workspace-bound by default**. Each workspace's `.zed/settings.json` can pin a different account per agent. Falls back to a global default, which itself falls back to the implicit single-account default when only one exists.
- **In-panel chip** in the Agent Panel header showing the active account for the active agent, brand-tinted with the agent's accent color (Claude Code burnt-orange, Gemini blue, Codex green). Click to switch, add, or manage.
- **Manage AI Accounts modal** (command palette: `agent: manage ai accounts`). List per agent, add / delete / set-default / verify-connection, expandable conversation history per account, brand-accented section dividers, empty-state hero on first run.
- **Add AI Account modal**. Agent picker, optional Sign-up link (opens provider's pricing page in browser), display-name input with case-insensitive uniqueness validation, brand-tinted Connect CTA.
- **Auto-trigger of agent login flow** after Connect:
  - Claude Code and Gemini: opens a fresh ACP thread for the agent (workspace already bound, env var injected at spawn). User types `/login` or `/auth`.
  - Codex: opens a dock terminal with `CODEX_HOME` env set and `codex login` running, since Codex uses browser-OAuth that needs a real terminal.
- **Per-account conversation history**. Click an account row to expand it; shows the 20 most recent conversations parsed from disk. Click any row to copy the agent's resume command (`claude --resume <id>`, `codex exec resume <id>`, `gemini --resume <id>`) to the clipboard.
- **Migration import from `claude-account-switcher`**. When `~/.claude-profiles/` exists, the Claude Code section header gains an "Import from claude-account-switcher" button. Imported profiles are registered by reference (no copying); the shell helper continues to work alongside.

### Implementation details

- New crate `crates/ai_accounts/` provides the descriptor / registry / parser / lifecycle layer. Storage: `paths::config_dir().join("ai_accounts.json")` for the index, `paths::data_dir().join("ai_accounts/<agent>/<id>/")` for new account directories.
- ACP server spawn (`crates/agent_servers/src/acp.rs`) reads `AiAccountsSettings` and the on-disk index at thread spawn time, resolves the bound account per agent, and injects the agent's config-dir env var (`CLAUDE_CONFIG_DIR`, `GEMINI_CONFIG_DIR`, `CODEX_HOME`) into the spawned subprocess.
- `last_used_at` is touched at ACP spawn and on chip switch so the Manage modal sorts most-used-first within each agent.
- Codex's keyring credential bypass is mitigated at create time: the per-account `config.toml` gets `cli_auth_credentials_store = "file"` written so OAuth tokens land inside the account's config dir rather than the OS keyring.

### Conversation history parsers

| Agent | Path | Format |
|---|---|---|
| Claude Code | `<config_dir>/projects/<project>/<session>.jsonl` | JSONL, first user message extracted as title |
| Codex CLI | `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-*.jsonl` | JSONL with date-partitioned dirs (also handles legacy flat layout); first line is `session_meta`, first `response_item` with `role: user` becomes the title |
| Gemini CLI | `<config_dir>/tmp/<project>/chats/session-*.json` | JSONL despite `.json` extension; first line is `metadata`; subsequent records have `type: "user"` / `"model"` |

### Polish

- Status toasts on every meaningful action (Connect, Delete, Import, Copy resume command, Verify outcome).
- Confirm-before-delete prompt with explicit destructive language.
- Optimistic Pending state during async verify.
- Empty-state hero block when no accounts exist anywhere, with primary "Add your first account" CTA + the import button when applicable.

### Out of scope (deliberate)

- API-key authentication for Claude / OpenAI / Google. Subscription auth only.
- Zed's first-party agent (different code path, Keychain-backed credentials).
- GitHub Copilot CLI and Cursor agent. Not yet integrated upstream in Lathe; deferred until they're first-class.
- Auto-injection of the `/login` slash command into the freshly-opened thread. Would require ~50 lines of plumbing across four files; saves one keystroke. Skipped.
- ACP-spawn-based conversation resumption. Currently clipboard-based. Auto-spawning a thread with `--resume` args needs deeper integration with the spawn pipeline, especially for Gemini's project-hash cwd requirement.

### Caveats per agent

- **Claude Code**: the npm shim hardcodes `~/.claude/` for some local-detection paths (anthropics/claude-code#2986, #3833). Auth and memory both honor `CLAUDE_CONFIG_DIR` so per-account isolation works for our use case, but flag if a future feature regresses.

  Lathe also defaults `ENABLE_CLAUDEAI_MCP_SERVERS=false` and `MCP_TIMEOUT=5000` per ACP spawn so claude.ai-managed cloud connectors (Gmail, Calendar, Drive that come down from a Claude Max account) don't hang thread startup waiting for OAuth that the ACP transport can't surface. To opt in to the cloud connectors, set `ENABLE_CLAUDEAI_MCP_SERVERS=true` in the workspace's `.zed/settings.json`:

  ```json
  {
    "agent_servers": {
      "claude-acp": {
        "env": { "ENABLE_CLAUDEAI_MCP_SERVERS": "true" }
      }
    }
  }
  ```

  The upstream issues asking for true lazy/deferred MCP loading (anthropics/claude-code#16254, #13700) were closed inactive; this default-off is the cleanest currently-available workaround.
- **Gemini CLI**: `GEMINI_CONFIG_DIR` is broken on Windows (google-gemini/gemini-cli#8248). macOS/Linux unaffected. Sessions are scoped by project hash, so resuming requires the same cwd as the original conversation.
- **Codex CLI**: see implementation note above re: `cli_auth_credentials_store=file`.
