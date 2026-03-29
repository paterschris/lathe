# Lathe — Custom Code Editor

Lathe is a custom fork of [Zed](https://zed.dev) with enhancements for our team's workflow.

## Changes from upstream

- **Custom theme and syntax highlighting** — tuned for our codebase
- **Terminal awaiting-input indicator** — shows a return icon (↩) in the terminal tab and title bar when Claude Code (or other interactive prompts) are waiting for your input

## Prerequisites

- **macOS** (Apple Silicon or Intel)
- **Rust** — install via [rustup](https://rustup.rs):
  ```sh
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```
- **Xcode Command Line Tools**:
  ```sh
  xcode-select --install
  ```

## Build and install

```sh
# Clone the repo
git clone <repo-url> lathe
cd lathe

# Build the app bundle (takes ~10-15 min on first build)
script/build-fork

# Install to /Applications
script/install-fork
```

The fork installs as **"Lathe Dev.app"** and runs alongside stock Zed without conflicts.

## Updating

```sh
git pull
script/build-fork
script/install-fork
```

## Running without installing

After building, you can run the binary directly:

```sh
# Run the binary
./target/release/zed

# Or open the app bundle
open "target/release/bundle/osx/Lathe Dev.app"
```

## Notes

- First builds take significantly longer than incremental rebuilds
- The app shares settings and extensions with stock Zed (same `~/Library/Application Support/Zed` directory)
- The `cargo-bundle` tool is installed automatically on first build from Zed's fork at [zed-industries/cargo-bundle](https://github.com/zed-industries/cargo-bundle)
