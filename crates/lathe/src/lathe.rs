//! Umbrella initialization for Lathe's feature crates.
//!
//! Each application entry point (`main.rs`, the `zed.rs` test harness, the
//! visual test runner) previously listed the individual Lathe crate `init`
//! calls inline. Those lists drifted out of sync (production omitted
//! `mobile_dev::init`, so mobile-dev actions never registered outside tests;
//! the test paths omitted `git_graph::init`). Routing every entry point through
//! [`init`] keeps the set consistent and collapses each upstream call site to a
//! single Lathe line.
//!
//! This crate sits *above* the feature crates: it depends on them so it can call
//! their `init`s. It deliberately is not part of [`lathe_hooks`], which stays a
//! `gpui`-only leaf so feature crates can depend on it to register hooks without
//! forming a dependency cycle.

use gpui::App;

/// Initialize every Lathe feature. Call once, early in application startup,
/// wherever upstream initializes its own subsystems.
///
/// Ordering among these is not significant: each `init` only registers actions,
/// observers, hooks, or serializable items. All are safe to run in headless and
/// test contexts.
pub fn init(cx: &mut App) {
    lathe_hooks::init(cx);
    pr_ui::init(cx);
    git_graph::init(cx);
    mobile_dev::init(cx);
}
