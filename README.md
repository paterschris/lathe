# Lathe

A custom code editor forked from [Zed](https://zed.dev), with enhancements for our team’s workflow.

## Changes from upstream Zed

- **Custom theme and syntax highlighting** — tuned for our codebase
- **Terminal awaiting-input indicator** — shows a return icon in the terminal tab and title bar when Claude Code (or other interactive prompts) are waiting for your input

## Install (pre-built)

Download the latest `.zip` for your Mac from [Releases](https://github.com/paterschris/lathe/releases), unzip, and drag **Lathe.app** to `/Applications`.

Currently only **Apple Silicon** builds are provided.

## Build from source

### Prerequisites

- **macOS** (Apple Silicon or Intel)
- **Rust** — install via [rustup](https://rustup.rs):
  ```sh
  curl --proto ‘=https’ --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```
- **Xcode Command Line Tools**:
  ```sh
  xcode-select --install
  ```

### Build and install

```sh
# Clone the repo
git clone git@github.com:paterschris/lathe.git
cd lathe

# Build the app bundle (takes ~10-15 min on first build)
script/build-fork

# Install to /Applications
script/install-fork
```

The fork installs as **"Lathe.app"** and runs alongside stock Zed without conflicts.

## Updating

```sh
git pull
script/build-fork
script/install-fork
```

## Running without installing

After building, you can open the app bundle directly without copying to `/Applications`:

```sh
open "$(find target -name 'Lathe.app' -path '*/bundle/osx/*' | head -1)"
```

## Releasing

To build, package, and publish a GitHub release in one step:

```sh
script/release-fork
```

Requires the [GitHub CLI](https://cli.github.com/) (`brew install gh`). If a release for the current version already exists, the new build is uploaded alongside the existing assets.

## Notes

- First builds take significantly longer than incremental rebuilds
- The app shares settings and extensions with stock Zed (same `~/Library/Application Support/Zed` directory)
- The `cargo-bundle` tool is installed automatically on first build from Zed’s fork at [zed-industries/cargo-bundle](https://github.com/zed-industries/cargo-bundle)
