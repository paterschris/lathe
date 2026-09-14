# Plan: Get Linux and Windows installations working

Status (updated 2026-08-10): MANUAL INSTALL QA DONE, PLAN COMPLETE. Chris installed Lathe successfully on a Linux VM and on a real Windows machine, closing steps 4-6 and the last non-deferred item in this plan. Not captured from that pass: which Windows artifact (setup.exe vs zip), whether SmartScreen intervened, the Linux distro, and whether the old "installed but no window appears" failure recurred. Nothing here is blocking a release. Remaining items are all deliberate deferrals: Windows auto_update_helper/flat layout, Linux remote_server, iss polish, and Windows code signing. Separately, aarch64 on both Linux and Windows is still best-effort (`continue-on-error`) in release_fork.yml, so those two assets can go missing from a release with only a `::warning::`.

Previous status (2026-07-22): v0.236.21-beta SHIPPED with working installs. After v0.236.20's installs proved broken, an upstream-comparison pass found and fixed: cli editor discovery (probed only zed-editor/Zed.exe names), Windows runtime DLLs staged in lib\ instead of beside the editor exe, glibc 2.35 baseline (now 2.31 via ubuntu:20.04 container builds), and libstdc++ not bundled. CI smoke tests now launch via `lathe --version` on both platforms. Remaining: Chris's real-machine QA; deferred: Windows auto_update_helper/flat layout, Linux remote_server, iss polish.

Previous status (2026-07-21, night): SHIPPED. v0.236.20-beta is live as a prerelease with all seven assets (macOS dmg/zip signed+notarized, Linux x86_64 tar.gz, Windows x86_64 + aarch64 zip/setup.exe); stable v0.236.19 remains Latest. Both lanes merged to main earlier (Lane A `1f802a9771`, Lane B `dbe963b9f2`), full CI matrix green, plus two release-time fixes: refreshed APPLE_APP_PASSWORD/APPLE_ID secrets and the build-fork target-guard `|| return 0` fix (`32cd4d3dd7`). NOTE: main is on channel beta / 0.236.20; flip RELEASE_CHANNEL to stable before the next stable release. Remaining: manual install QA (plan steps 4-6).

Repo: `paterschris/lathe` (public fork of Zed, so GitHub-hosted runners are free).

This document is shared between two AI agents working in parallel. To avoid collisions the work is split into two lanes with strict, disjoint file ownership. Do not edit files owned by the other lane. If a task seems to require crossing the boundary, stop and flag it to Chris instead.

## Background (read first, both lanes)

- Latest release `v0.236.19` shipped macOS assets only. No release has ever shipped Linux or Windows assets.
- `.github/workflows/release_fork.yml` ("Release Lathe", workflow_dispatch) is already complete: build-macos + validate-macos smoke test, build-linux (ubuntu-22.04: `script/linux`, `script/build-fork-linux`, `script/package-fork-linux` producing a tar.gz), build-windows matrix (x86_64 + aarch64 on windows-2022: zip + Inno Setup installer), then create-release uploads everything (appends to the tag if it exists). Its last run (2026-06-04, run 26984135304) failed on an E0046 (`get_all_timings` missing on non-mac dispatchers). That was fixed on 2026-06-05, but the workflow was never dispatched again.
- "Lathe Windows CI" (`.github/workflows/lathe_windows_ci.yml`) was fully green (build + package + install/uninstall smoke test) from June 5 to June 23. It has failed on every `main` push since 2026-07-06, always at the `check-windows` job, always with the same first error:

  ```
  error[E0432]: unresolved import `crate::alacritty::current_child_signal_mask`
    --> crates\terminal\src\terminal.rs:63:5
  note: found an item that was configured out
    --> crates\terminal\src\alacritty.rs:154:15
  ```

  `current_child_signal_mask` is `#[cfg(not(windows))]` in `alacritty.rs`; every usage in `terminal.rs` (lines ~1045, ~1198) is already guarded; only the `use` at line 63 is unguarded. Because check dies at the first error, the full-workspace check and the full Windows build have not run since June 23, so more Windows errors (especially from the 2026-07-14 upstream merge) may be hiding behind this one.
- There is no Linux CI lane at all. The workflows inherited from upstream (`run_tests.yml`, `release_nightly.yml`, `run_bundling.yml`, `nix_build.yml`, `after_release.yml`) target runner labels that only exist in zed-industries' infra (`namespace-profile-*`, `self-32vcpu-windows-2022`), so their runs sit queued forever on this fork (several stuck at 13h+ right now). Linux compile breakage currently surfaces only at release time.
- Windows install pain: fork builds are unsigned (no signing in `script/package-fork-windows.ps1`), so SmartScreen blocks the setup.exe and Mark-of-the-Web / Defender can break the zip path. `script/diag-windows.ps1` exists for the "installed but no window appears" failure mode and that earlier incident was never root-caused.

## Lane A: Rust cross-platform fixes

Owner: Claude.
Owns: `crates/**` ONLY.
Must not touch: `.github/workflows/**`, `script/**`, `README*`, `docs/**`.
Branch: `lane-a-crate-fixes` (from current `main`).

1. Guard the import at `crates/terminal/src/terminal.rs:63`:

   ```diff
   +#[cfg(not(windows))]
    use crate::alacritty::current_child_signal_mask;
   ```

2. Optionally shorten the feedback loop before pushing by cross-checking locally on macOS (check does not link, so this often works for the leaf crates):

   ```bash
   rustup target add x86_64-pc-windows-msvc
   cargo check --target x86_64-pc-windows-msvc -p terminal -p workspace -p project -p agent_servers -p util -p install_cli
   ```

   If native build scripts fail locally, fall back to CI as the oracle.

3. Push the branch and open a PR (touching `crates/**` triggers Lathe Windows CI on PRs). Iterate until both check steps pass: "cargo check (Windows-touched crates)" then "cargo check (full workspace)". Fix every error inside `crates/**`. Expect a batch of `cfg(windows)` fallout from the July 14 upstream merge.

4. After merge to `main`, the heavy `build-windows` job runs (~1.5h): full build, package, install/uninstall smoke test. Fix any remaining crate-level breakage it surfaces.

5. Once Lane B's Linux CI exists on `main`, do the same loop for Linux errors (also `crates/**` only). To get ahead of it locally: `cargo check --target x86_64-unknown-linux-gnu` may partially work on macOS, or use a Linux container if available.

Definition of done for Lane A: `check-windows`, full Windows build job, and the Linux check job are all green on `main`.

## Lane B: CI workflows, packaging, and install scripts

Owner: Codex.
Owns: `.github/workflows/**`, `script/**`, `README*`, `docs/**` ONLY.
Must not touch: `crates/**` (including `crates/zed/resources/windows/lathe.iss`; if the Inno script needs changes, flag it to Chris).
Branch: `lane-b-ci-and-scripts` (from current `main`).

1. Add `.github/workflows/lathe_linux_ci.yml` mirroring the structure of `lathe_windows_ci.yml`:
   - PR + main-push triggers with the same path filters (adjusted for Linux scripts).
   - Cheap job: `cargo check --workspace` on `ubuntu-22.04` after installing deps via `script/linux` (use `CC=clang CXX=clang++` like the release workflow does). Use Swatinem/rust-cache with `continue-on-error: true` (same rationale as the Windows workflow comments).
   - Heavy job on main pushes only: `script/build-fork-linux`, `script/package-fork-linux`, then an install/uninstall smoke test using `script/install-fork-linux` (mirror the Windows smoke test: verify binaries exist, PATH/desktop entries, then clean up). Upload the tar.gz as an artifact, 7-day retention.
   - Note: this workflow will be red until Lane A fixes whatever Linux errors it surfaces. That is expected; land it anyway.

2. Prune the permanently-stuck upstream workflows. Either delete them or add a top-level `if: github.repository == 'zed-industries/zed'` guard to every job: `run_tests.yml`, `release_nightly.yml`, `run_bundling.yml`, `nix_build.yml`, `after_release.yml`. Prefer the guard over deletion where the file helps future upstream merges. Also cancel the currently queued stuck runs (`gh run cancel`). Community-automation workflows that merely skip can stay.

3. `release_fork.yml` hardening:
   - create-release currently hard-fails if any of the seven asset patterns is missing (macOS zip/dmg, Linux tar.gz, Windows x86_64 zip/exe, Windows aarch64 zip/exe). Make the aarch64-windows pair tolerant (copy if present, warn if not) so an aarch64-only failure cannot block a release.
   - Confirm the workflow still creates-or-appends correctly when the tag already exists (the v0.236.19 case).

4. Windows install UX in `script/install-fork-windows.ps1`: after extracting the zip, run `Unblock-File` on all extracted files to strip Mark-of-the-Web. Add a short "Installing on Windows" section to the README covering SmartScreen ("More info" then "Run anyway"), the zip vs installer paths, and `script/diag-windows.ps1` for troubleshooting.

Definition of done for Lane B: Linux CI workflow live on `main`, stuck workflows guarded or removed, release workflow tolerant of missing aarch64-windows, install script unblocks MotW, README documents Windows install.

## Coordination rules

- One branch per lane, both cut from the same `main`. Never commit to the other lane's paths; `git diff --name-only main` before pushing and verify every path is inside your lane.
- Do not rebase or merge the other lane's branch into yours. Merge order to `main`: Lane A's import fix can land first and independently; otherwise order does not matter.
- Neither lane pushes to `main` directly or dispatches "Release Lathe". Chris merges and dispatches.
- If a fix genuinely requires touching both territories (e.g. a workflow needs a new script flag AND a crate change), split it: each lane does its half, and the doc's "Handoffs" section below records the dependency.

## Handoffs and sequencing

1. Lane A lands the terminal.rs import fix; Windows CI check goes green or reveals the next error batch.
2. Lane B lands Linux CI; it reveals the Linux error list for Lane A.
3. Both lanes iterate independently until their definitions of done are met.
4. Chris dispatches "Release Lathe" from the Actions tab. Assets append to the current release.
5. Manual QA (Chris): Windows setup.exe and zip on a real machine (run `script/diag-windows.ps1` if no window appears); Linux tar.gz + `script/install-fork-linux` in a container or VM. Report findings back into this doc.
6. Follow-ups discovered during QA get appended here under a "QA findings" section, assigned to a lane by file ownership.

## Verification commands

```bash
# Watch CI on the fork (from the repo root)
gh run list -R paterschris/lathe --workflow lathe_windows_ci.yml --limit 5
gh run list -R paterschris/lathe --workflow lathe_linux_ci.yml --limit 5

# Failure details for a run
gh run view <run-id> -R paterschris/lathe --log-failed | grep -E 'error(\[|:)' | head -30

# Release assets present?
gh release view -R paterschris/lathe --json assets -q '.assets[].name'
```
