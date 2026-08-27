---
description: Review, open, and merge pull requests without leaving Lathe
title: Lathe pull request review
---

# Pull Requests

Lathe can list, review, open, and merge pull requests against your repository's
hosting provider. The diff renders in a real editor buffer, so it gets your
theme, your syntax highlighting, and your keybindings, and you can navigate it
the same way you navigate any other code.

## Supported hosts

| Host | List and review | Inline comments | Approve | Merge | Create |
| --- | --- | --- | --- | --- | --- |
| GitHub.com | Yes | Yes | Yes | Yes | Yes |
| GitHub Enterprise Server | Yes | Yes | Yes | Yes | Yes |
| GitLab.com | Yes | Yes | Approve only | Yes | Yes |
| GitLab self-managed | Yes | Yes | Approve only | Yes | Yes |
| Bitbucket Cloud | Yes | Yes | Yes | Yes | Yes |

Some behaviour differs by host, because the hosts themselves differ:

- **GitLab has no "request changes" verdict.** It models blocking review as
  unresolved discussion threads plus a withdrawn approval, so choosing "Request
  changes" posts your summary comment and removes any approval you had given.
- **GitLab merge strategy is a project setting**, not a per-merge choice, so the
  rebase option is unavailable and reports as such rather than quietly doing
  something else.
- **Bitbucket Data Center (self-hosted) is not supported.** It exposes a
  different REST API from Bitbucket Cloud, and Lathe does not speak it. It is
  not offered as a connectable host rather than failing halfway through.

## Connecting an account

Open the account menu in the title bar and choose **Connect** for your host.
Every host Lathe knows how to authenticate appears there, including enterprise
and self-hosted instances you have configured through
[`git_hosting_providers`](./reference/all-settings.md).

- **GitHub.com** uses the OAuth device flow: a browser window opens and you enter
  the code shown in the dialog.
- **GitHub Enterprise, GitLab.com, and self-managed GitLab** take a personal
  access token. The dialog links to the token page on your own instance. GitHub
  tokens need the `repo` and `read:org` scopes; GitLab tokens need `api`.
- **Bitbucket Cloud** takes your username and an app password or API token.

Credentials are stored in your operating system's keychain, one entry per host,
and are never written to your settings file.

### Adding an enterprise or self-hosted instance

Register the instance as a hosting provider, and it becomes connectable:

```json
{
  "git_hosting_providers": [
    {
      "provider": "github",
      "name": "BigCorp GitHub",
      "base_url": "https://github.bigcorp.com"
    }
  ]
}
```

Lathe uses the correct API base for each deployment automatically: GitHub.com is
served from `api.github.com`, while GitHub Enterprise Server is served from
`/api/v3` on the instance itself.

## The Pull Request Panel

Open the panel with {#action pull_request_panel::ToggleFocus}, or click the pull
request icon in the status bar.

Each row shows the number, title, author, source branch, and a reviewer roll-up:
one dot per reviewer, filled green for approved, filled red for changes
requested, and hollow for still pending. Hover the dots for the full list. Rows
you have already reviewed are tinted to match your own verdict.

When you have pull requests of your own open, they are grouped into a **Created
by you** section below the rest.

### Filtering and sorting

The dropdown in the panel header controls three independent things:

- **State**: open, closed, merged, or all.
- **My open reviews**: restrict the list to pull requests awaiting your review.
- **Sort**: recently updated (the host's own order), newest, oldest, or title.
  Sorting is applied to the rows already loaded, so it never refetches.

Long lists load a page at a time. Activate the **Load more** row at the bottom to
fetch the next page.

### Keyboard

| Action | macOS | Linux / Windows |
| --- | --- | --- |
| Focus the panel | `Ctrl+Shift+Alt+P` | `Ctrl+Shift+Alt+P` |
| Move through the list | `Up` / `Down` | `Up` / `Down` |
| First / last entry | `Cmd+Up` / `Cmd+Down` | `Ctrl+Home` / `Ctrl+End` |
| Open in Lathe | `Enter` | `Enter` |
| Open on the host's website | `Cmd+Enter` | `Ctrl+Enter` |
| Refresh | `Cmd+R` | `Ctrl+R` |
| Open selected in browser | `Cmd+Shift+O` | `Ctrl+Shift+O` |

Clicking a row opens it in Lathe; holding the primary modifier or middle-clicking
opens it on the host's website instead.

Right-click any row for **Open on Host Website**, **Copy Link**, **Copy Title**,
**Copy Branch Name**, and **Check Out Branch**. Checking out runs `git switch`,
which creates a local tracking branch when exactly one remote has it. If the
branch has never been fetched, the error names the fix.

## Reviewing a pull request

Opening a pull request adds a tab with the description, the metadata header, and
the full diff in a multibuffer.

The header shows state, mergeability, how far the branch has fallen behind its
base, the reviewer list, and a CI summary for the head commit. The CI chip is
coloured by the worst state present, so a run that is half finished never reads
as green; hover it for the full pass/fail/running breakdown. Repositories with no
CI simply show no chip rather than a false failure.

### Inline comments

Existing review threads appear inline in the diff at the line they were left on,
with replies indented underneath. Resolved threads are collapsed by default.

To start a new comment, click the **+** in the gutter next to any line. This works
on added and context lines, which anchor to the new side of the diff, and on
deleted lines, which anchor to the old side.

Comments on lines that are unchanged in this pull request still work: Lathe
fetches the file's full content so the line exists to anchor to.

### Reviewing and merging

The header buttons submit **Approve**, **Request changes**, or **Comment**, and
merge with a merge commit, squash, or rebase. Approve and Request changes act as
toggles: clicking the verdict you have already submitted retracts it.

On a pull request you did not open, the actions that belong to its author are
collected under a **More** menu instead of sitting in the button row: **Merge**,
**Squash & merge**, the draft toggle, and **Decline** / **Close**. They still
work, and the host still decides whether you are permitted to use them; the menu
only keeps them from being one stray click away from **Approve**. Lathe resolves
authorship from the account you connected, so when it cannot tell who you are
(an unauthenticated or heavily scoped token) the buttons stay in the row.

## Creating a pull request

Run {#action pull_request_panel::CreatePullRequest} from the command palette, or
click **+** in the panel header. The dialog prefills the source branch from your
current checkout and the target from the repository's default branch on the host.
The new pull request opens in a tab as soon as it is created.

## Settings

```json
{
  "pull_request_panel": {
    "button": true,
    "dock": "right",
    "default_width": 360
  }
}
```

- `button`: show the pull request icon in the status bar.
- `dock`: `"left"` or `"right"`. Dragging the panel to the other dock updates this.
- `default_width`: width in pixels.
