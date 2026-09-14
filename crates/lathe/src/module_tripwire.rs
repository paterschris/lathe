//! Compile-time guards that fail the build when Lathe code is dropped from an
//! upstream-owned file.
//!
//! Every Lathe module inside an upstream crate is reachable only through a
//! `mod` line in a file upstream also edits, most of them in
//! `crates/workspace/src/workspace.rs`. That single line is the easiest thing
//! in the repository for a merge to delete: the module file stays on disk, the
//! crate still compiles, and the feature simply stops existing. `docs/features.md`
//! opens by warning about this, and it has happened.
//!
//! The modules that survived did so by accident, because unrelated code in
//! another crate happened to import them, which turned a silent deletion into a
//! build error. This file makes that protection deliberate rather than lucky.
//!
//! Each `use` below exists solely to force a path to resolve. None of it runs.
//! If a merge drops a `mod` line, a `pub`, or a fork method, the build fails
//! here with a clear path to what went missing.
//!
//! Do not "clean up" these imports. Do not add `#[allow(unused)]` to silence
//! them. When a Lathe module is genuinely retired, delete its guard in the same
//! commit that retires it.

// Guards `mod portable_workspace;` and its public surface.
#[allow(unused_imports)]
use workspace::portable_workspace::{
    PORTABLE_WORKSPACE_EXTENSION, PortableWorkspace, ensure_portable_workspace_extension,
    has_portable_workspace_extension,
};

// Guards `pub mod theme_customizer;`. This module has no other caller outside
// `workspace.rs`, so without this guard it is the one that disappears quietly.
#[allow(unused_imports)]
use workspace::theme_customizer::init as _theme_customizer_init;

/// Guards `mod lathe;`, whose contents are inherent `impl Workspace` methods
/// rather than a namespaced module path. Naming each method keeps the failure
/// specific: losing the module reports every method that vanished, not one
/// unresolved import.
#[allow(dead_code)]
fn workspace_extensions_exist() {
    use workspace::Workspace;

    // Portable workspaces (`.lathe-workspace`).
    let _ = Workspace::save_workspace_as;
    let _ = Workspace::save_workspace;
    let _ = Workspace::portable_workspace_path;
    let _ = Workspace::set_portable_workspace_path;

    // Collab account binding for workspace groups.
    let _ = Workspace::bound_collab_account_id;

    // Terminal awaiting-input indicator.
    let _ = Workspace::any_item_awaiting_input;
    let _ = Workspace::awaiting_input_count;
    let _ = Workspace::first_awaiting_input_tooltip;
    let _ = Workspace::focus_first_awaiting_input;
}
