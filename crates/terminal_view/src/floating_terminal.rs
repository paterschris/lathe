//! Moving a terminal out of its workspace into its own native window, and
//! handing it back again.
//!
//! Editors are *cloned* into a floating window, but a terminal is
//! *transferred*: the same `TerminalView` entity, and therefore the same PTY,
//! process, scrollback and input state, is removed from its source pane and
//! re-added to the floating window. GPUI routes window-bound async work through
//! the most recently rendered window for an entity, so the terminal is never
//! mounted in two windows at once: every transfer removes before it adds.
//!
//! `FloatingWindowManager` owns the window's lifecycle and its close path, so
//! closing a floating terminal window hands the terminal back rather than
//! killing it. This module only supplies the return handler and remembers which
//! terminal belongs to which window.

use anyhow::{Context as _, Result};
use collections::HashMap;
use gpui::{
    Action as _, App, AppContext as _, Entity, EntityId, Global, WeakEntity, Window, WindowHandle,
    WindowId,
};
use util::ResultExt as _;
use workspace::{CloseWindow, FloatingWindowManager, MultiWorkspace, Pane, Workspace};

use crate::TerminalView;
use crate::terminal_panel::TerminalPanel;

/// A terminal that was moved into a floating window, and where it came from.
///
/// `FloatingWindowManager` records one source per window, but a window can be
/// given terminals from several places, and a window can also hold a terminal
/// that was simply started inside it. Only the moved ones have somewhere to go
/// back to, and each goes back to its own pane.
struct TransferredTerminal {
    view: WeakEntity<TerminalView>,
    source_window: WindowHandle<MultiWorkspace>,
    source_workspace: WeakEntity<Workspace>,
    source_pane: WeakEntity<Pane>,
    /// Whether the terminal was living in the terminal panel rather than in the
    /// center. Recorded on the way out because the pane it came from may be
    /// closed by the time it comes back, and it decides both where an orphaned
    /// terminal lands and whether the panel is revealed for it.
    from_terminal_panel: bool,
}

/// The moved terminals each floating window is currently holding.
#[derive(Default)]
struct TransferredTerminals(HashMap<WindowId, Vec<TransferredTerminal>>);

impl Global for TransferredTerminals {}

/// Whether `terminal_view` was moved into `window_id` and still has somewhere
/// to go back to.
///
/// A source workspace that has since closed leaves nothing to return to, so a
/// terminal whose source is gone reports false and stops offering a return it
/// cannot make.
pub(crate) fn is_transferred_terminal(
    window_id: WindowId,
    terminal_view: &WeakEntity<TerminalView>,
    cx: &App,
) -> bool {
    cx.try_global::<TransferredTerminals>()
        .and_then(|transferred| transferred.0.get(&window_id))
        .is_some_and(|terminals| {
            terminals.iter().any(|transferred| {
                transferred.view.entity_id() == terminal_view.entity_id()
                    && transferred.source_workspace.upgrade().is_some()
            })
        })
}

/// Where a terminal is being moved to.
pub(crate) enum MoveTarget {
    NewWindow,
    /// A floating window that is already open. Held by id and resolved at move
    /// time, because a menu entry naming a window outlives the listing it was
    /// built from and that window may have closed in between.
    ExistingWindow(WindowId),
}

/// Moves `terminal_view` out of the workspace it currently lives in and into a
/// floating window, either a new one or one that is already open.
///
/// The target window is resolved before the terminal is removed from its source
/// pane, so that a window that fails to open, or one that has closed since the
/// menu naming it was built, leaves the terminal exactly where it was.
pub(crate) fn move_terminal_to_window(
    terminal_view: Entity<TerminalView>,
    source_workspace: Entity<Workspace>,
    target: MoveTarget,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let source_window = window
        .window_handle()
        .downcast::<MultiWorkspace>()
        .context("terminal's window is not a workspace window")?;
    let source_pane = source_workspace
        .read(cx)
        .pane_for(&terminal_view)
        .context("terminal is not in a pane")?;
    let from_terminal_panel = source_workspace
        .read(cx)
        .panel::<TerminalPanel>(cx)
        .is_some_and(|panel| {
            panel
                .read(cx)
                .panes()
                .iter()
                .any(|panel_pane| **panel_pane == source_pane)
        });

    let floating_window = match target {
        MoveTarget::NewWindow => {
            let project = source_workspace.read(cx).project().clone();
            let app_state = source_workspace.read(cx).app_state().clone();
            let options = (app_state.build_window_options)(None, cx);
            cx.open_window(options, |window, cx| {
                let workspace =
                    cx.new(|cx| Workspace::new_floating(project, app_state, window, cx));
                cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
            })?
        }
        MoveTarget::ExistingWindow(window_id) => workspace::floating_windows(cx)
            .into_iter()
            .find(|(candidate, _)| candidate.window_id() == window_id)
            .map(|(candidate, _)| candidate)
            .context("the window to move the terminal to is no longer open")?,
    };

    source_pane.update(cx, |pane, cx| {
        pane.remove_item(terminal_view.entity_id(), false, true, window, cx);
    });

    floating_window.update(cx, |multi_workspace, window, cx| {
        multi_workspace.workspace().update(cx, |workspace, cx| {
            workspace.add_item_to_active_pane(
                Box::new(terminal_view.clone()),
                None,
                true,
                window,
                cx,
            );
        });
        window.activate_window();
    })?;

    let window_id = floating_window.window_id();
    cx.default_global::<TransferredTerminals>()
        .0
        .entry(window_id)
        .or_default()
        .push(TransferredTerminal {
            view: terminal_view.downgrade(),
            source_window,
            source_workspace: source_workspace.downgrade(),
            source_pane: source_pane.downgrade(),
            from_terminal_panel,
        });

    // A window that already has a registration keeps it: its return handler
    // hands back whatever this window is holding when it runs, so terminals
    // moved in later are covered by the handler registered for the first.
    let registration = if FloatingWindowManager::is_registered(window_id, cx) {
        Ok(())
    } else {
        FloatingWindowManager::register(
            floating_window,
            source_workspace.downgrade(),
            source_pane.downgrade(),
            move |cx: &mut App| {
                return_terminals(floating_window, cx);
                // The moves back are deferred until after the window has been
                // torn down, so they cannot report their outcome here; each
                // logs its own failures instead.
                Ok(())
            },
            cx,
        )
    };

    if let Err(error) = registration {
        remove_transfer(window_id, terminal_view.entity_id(), cx);
        return Err(error);
    }

    cx.on_window_closed(move |cx, closed_window_id| {
        if closed_window_id == window_id && cx.has_global::<TransferredTerminals>() {
            cx.global_mut::<TransferredTerminals>().0.remove(&window_id);
        }
    })
    .detach();

    Ok(())
}

/// Asks the floating window hosting this terminal to hand it back.
///
/// The return runs as part of the window's ordinary close path, so this is the
/// same command as closing the window.
pub(crate) fn return_from_window(window: &mut Window, cx: &mut App) {
    window.dispatch_action(CloseWindow.boxed_clone(), cx);
}

/// Drops one terminal's transfer record, leaving any others in that window.
fn remove_transfer(window_id: WindowId, terminal_view: EntityId, cx: &mut App) {
    let transferred = cx.default_global::<TransferredTerminals>();
    let emptied = {
        let Some(terminals) = transferred.0.get_mut(&window_id) else {
            return;
        };
        terminals.retain(|candidate| candidate.view.entity_id() != terminal_view);
        terminals.is_empty()
    };
    if emptied {
        transferred.0.remove(&window_id);
    }
}

/// Hands every terminal that was moved into `floating_window` back to the pane
/// it came from.
///
/// The records are taken out of the map first, because the window is on its way
/// out either way: a terminal whose source has closed is left to go through its
/// usual cleanup with the window rather than kept in a map nothing will read
/// again.
fn return_terminals(floating_window: WindowHandle<MultiWorkspace>, cx: &mut App) {
    let Some(transferred) = cx
        .default_global::<TransferredTerminals>()
        .0
        .remove(&floating_window.window_id())
    else {
        return;
    };

    for terminal in transferred {
        return_terminal(floating_window, terminal, cx);
    }
}

/// Puts a moved terminal back where it came from.
///
/// Deferred so that it runs after the close path has torn the floating window
/// down, keeping the terminal from being mounted in two windows at once. The
/// strong handle taken here keeps the terminal, and its process, alive across
/// that teardown.
fn return_terminal(
    floating_window: WindowHandle<MultiWorkspace>,
    transferred: TransferredTerminal,
    cx: &mut App,
) {
    let source_window = transferred.source_window;
    // With nothing to return, or nowhere to return it to, the window simply
    // closes and the terminal goes through its usual cleanup.
    let Some((terminal_view, source_workspace)) = transferred
        .view
        .upgrade()
        .zip(transferred.source_workspace.upgrade())
    else {
        return;
    };
    let source_pane = transferred.source_pane;
    let from_terminal_panel = transferred.from_terminal_panel;

    cx.defer(move |cx| {
        floating_window
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    if let Some(pane) = workspace.pane_for(&terminal_view) {
                        // The pane closes with the terminal if that was all it
                        // held. A window that survives the return (because it
                        // has items of its own) would otherwise be left with an
                        // empty pane, which draws no tab bar and holds nothing,
                        // so the user has no way to get rid of it.
                        pane.update(cx, |pane, cx| {
                            pane.remove_item(terminal_view.entity_id(), false, true, window, cx);
                        });
                    }
                });
            })
            .ok();

        source_window
            .update(cx, |_, window, cx| {
                let terminal_panel = source_workspace.read(cx).panel::<TerminalPanel>(cx);
                let panel_owns_pane = |pane: &Entity<Pane>, cx: &App| {
                    terminal_panel
                        .as_ref()
                        .is_some_and(|panel| panel.read(cx).panes().contains(&pane))
                };
                // A pane that was emptied by the move may have been closed with
                // it, and adding to a pane the workspace no longer lays out
                // would put the terminal somewhere the user cannot see.
                let source_pane = source_pane.upgrade().filter(|pane| {
                    let workspace = source_workspace.read(cx);
                    workspace.panes().contains(pane) || panel_owns_pane(pane, cx)
                });
                // Revealing the terminal panel is only right for a terminal that
                // belongs in it. Doing it for a center terminal leaves an empty
                // dock open that the user has no way to close.
                let reveal_panel = match &source_pane {
                    Some(pane) => panel_owns_pane(pane, cx),
                    None => from_terminal_panel,
                };
                // With its pane gone, a panel terminal joins the panel's remaining
                // pane, while a center terminal falls through to the workspace's
                // active pane rather than being buried in a dock it never
                // belonged to.
                let pane = source_pane.or_else(|| {
                    terminal_panel
                        .as_ref()
                        .filter(|_| from_terminal_panel)
                        .map(|panel| panel.read(cx).active_pane.clone())
                });

                match pane {
                    Some(pane) => {
                        // The terminal goes back into its pane before the panel
                        // is revealed: a terminal panel that is activated while
                        // empty spawns a replacement shell.
                        pane.update(cx, |pane, cx| {
                            pane.add_item(Box::new(terminal_view), true, true, None, window, cx);
                        });
                        if reveal_panel {
                            source_workspace.update(cx, |workspace, cx| {
                                workspace.focus_panel::<TerminalPanel>(window, cx);
                            });
                        }
                    }
                    None => source_workspace.update(cx, |workspace, cx| {
                        workspace.add_item_to_active_pane(
                            Box::new(terminal_view),
                            None,
                            true,
                            window,
                            cx,
                        );
                    }),
                }
                window.activate_window();
            })
            .log_err();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_panel::TerminalPanel;
    use gpui::{Entity, TestAppContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use task::RevealStrategy;
    use terminal::{InteractivePromptKind, Terminal};
    use workspace::item::Item as _;
    use workspace::{SplitDirection, SplitMode};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    /// Builds a workspace window holding a terminal panel with a single running
    /// terminal in it.
    async fn setup(
        cx: &mut TestAppContext,
    ) -> (
        WindowHandle<MultiWorkspace>,
        Entity<Workspace>,
        Entity<TerminalPanel>,
        Entity<TerminalView>,
        Entity<Terminal>,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let source_window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = source_window
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        let terminal_panel = source_window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                    workspace.add_panel(panel.clone(), window, cx);
                    panel
                })
            })
            .unwrap();

        source_window
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        let (terminal_view, terminal) = terminal_panel.read_with(cx, |panel, cx| {
            let view = panel
                .active_pane
                .read(cx)
                .active_item()
                .expect("a terminal was added to the panel")
                .downcast::<TerminalView>()
                .expect("the panel's item is a terminal");
            let terminal = view.read(cx).terminal().clone();
            (view, terminal)
        });

        (
            source_window,
            workspace,
            terminal_panel,
            terminal_view,
            terminal,
        )
    }

    fn registered_floating_window(cx: &mut TestAppContext) -> Option<WindowHandle<MultiWorkspace>> {
        cx.update(|cx| {
            cx.try_global::<TransferredTerminals>()
                .and_then(|transferred| transferred.0.keys().copied().next())
                .and_then(|window_id| FloatingWindowManager::window_for(window_id, cx))
        })
    }

    #[gpui::test]
    async fn test_move_terminal_to_new_window_transfers_the_same_terminal(cx: &mut TestAppContext) {
        let (source_window, workspace, terminal_panel, terminal_view, terminal) = setup(cx).await;

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::NewWindow,
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        terminal_panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.active_pane.read(cx).items_len(),
                0,
                "the terminal should have left its source pane"
            );
        });

        let floating_window =
            registered_floating_window(cx).expect("a floating terminal window was registered");
        let floating_workspace = floating_window
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        floating_workspace.read_with(cx, |floating_workspace, cx| {
            assert!(floating_workspace.is_floating());
            assert_eq!(floating_workspace.database_id(), None);
            let moved = floating_workspace
                .active_pane()
                .read(cx)
                .active_item()
                .expect("the terminal was added to the floating window")
                .downcast::<TerminalView>()
                .expect("the floating item is a terminal");
            assert_eq!(
                moved.entity_id(),
                terminal_view.entity_id(),
                "the same terminal view should be moved, not a new one"
            );
            assert_eq!(
                moved.read(cx).terminal().entity_id(),
                terminal.entity_id(),
                "the underlying terminal, and therefore its process, should be unchanged"
            );
            assert_eq!(
                moved.read(cx).workspace_id(),
                None,
                "a floating terminal must not claim persisted terminal ownership"
            );
        });
    }

    #[gpui::test]
    async fn test_returning_a_floating_terminal_to_its_workspace(cx: &mut TestAppContext) {
        let (source_window, workspace, terminal_panel, terminal_view, terminal) = setup(cx).await;
        let source_workspace_id = terminal_view.read_with(cx, |view, _| view.workspace_id());

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::NewWindow,
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        let floating_window =
            registered_floating_window(cx).expect("a floating terminal window was registered");
        floating_window
            .update(cx, |_, window, cx| return_from_window(window, cx))
            .unwrap();
        cx.run_until_parked();

        terminal_panel.read_with(cx, |panel, cx| {
            let returned = panel
                .active_pane
                .read(cx)
                .active_item()
                .expect("the terminal returned to its source pane")
                .downcast::<TerminalView>()
                .expect("the returned item is a terminal");
            assert_eq!(returned.entity_id(), terminal_view.entity_id());
            assert_eq!(
                returned.read(cx).terminal().entity_id(),
                terminal.entity_id(),
                "returning must not restart the terminal's process"
            );
            assert_eq!(
                returned.read(cx).workspace_id(),
                source_workspace_id,
                "the returned terminal should be owned by its source workspace again"
            );
        });

        assert!(
            registered_floating_window(cx).is_none(),
            "the floating window should no longer be registered"
        );
    }

    /// With its source gone there is nothing to hand the terminal back to, so
    /// the window has to close the ordinary way rather than refuse to close.
    #[gpui::test]
    async fn test_a_floating_terminal_closes_when_its_source_is_gone(cx: &mut TestAppContext) {
        let (source_window, workspace, _terminal_panel, terminal_view, _terminal) = setup(cx).await;

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::NewWindow,
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        let floating_window =
            registered_floating_window(cx).expect("a floating terminal window was registered");

        drop(workspace);
        source_window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();

        assert!(
            !tab_menu_labels(floating_window, &terminal_view, cx)
                .contains(&"Return to Workspace".to_string()),
            "with its source gone the terminal should stop offering a return it cannot make"
        );

        floating_window
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.close_window(&CloseWindow, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert!(
            floating_window.update(cx, |_, _, _| ()).is_err(),
            "the floating window should have closed instead of holding a terminal it cannot return"
        );
        assert!(
            cx.update(|cx| !FloatingWindowManager::is_registered(floating_window.window_id(), cx)),
            "the closed window should no longer be registered"
        );
    }

    /// Step 3's acceptance criterion: the terminal's process and view state have
    /// to come through a move and a return intact. The re-add path runs
    /// `added_to_workspace`, which mutates the view, so this guards against that
    /// path resetting anything.
    ///
    /// The scrollback itself lives on the `Terminal` entity, which is moved
    /// rather than recreated, so it is covered by asserting that entity's
    /// identity. Its line count is deliberately not asserted: the grid reflows
    /// when the terminal is rendered at the new window's size.
    ///
    /// The awaiting-input indicator is the one piece of state a move does not
    /// carry over, because the plan has the move focus the terminal and
    /// `focus_in` clears that indicator, exactly as clicking the tab would.
    #[gpui::test]
    async fn test_a_move_and_return_preserves_terminal_state(cx: &mut TestAppContext) {
        let (source_window, workspace, terminal_panel, terminal_view, terminal) = setup(cx).await;

        source_window
            .update(cx, |_, _, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.write_output(b"scrollback one\nscrollback two\n", cx);
                });
                terminal_view.update(cx, |terminal_view, cx| {
                    terminal_view.set_custom_title(Some("deploy watch".into()), cx);
                    terminal_view
                        .set_awaiting_input_for_test(Some(InteractivePromptKind::Confirmation));
                });
            })
            .unwrap();
        cx.run_until_parked();

        // The shell's own pid, not `Terminal::pid`, which reports whichever
        // process is in the foreground and so moves as the shell runs commands.
        let shell_pid = |terminal: &Terminal| terminal.pid_getter().map(|pid| pid.fallback_pid());
        let shell_pid_before = terminal.read_with(cx, |terminal, _| shell_pid(terminal));
        assert!(
            shell_pid_before.is_some(),
            "the fixture terminal should have a live process to preserve"
        );

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::NewWindow,
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        let assert_preserved = |stage: &str, cx: &mut TestAppContext| {
            terminal_view.read_with(cx, |terminal_view, cx| {
                assert_eq!(
                    terminal_view.terminal().entity_id(),
                    terminal.entity_id(),
                    "{stage}: the emulator holding the scrollback should be the same one"
                );
                assert_eq!(
                    terminal_view.custom_title(),
                    Some("deploy watch"),
                    "{stage}: a renamed terminal should keep its name"
                );
                assert_eq!(
                    terminal_view.awaiting_input(),
                    None,
                    "{stage}: focusing the moved terminal should clear its \
                     awaiting-input indicator, the same as clicking its tab"
                );
                assert_eq!(
                    shell_pid(terminal.read(cx)),
                    shell_pid_before,
                    "{stage}: the shell process should be the same one"
                );
            });
        };

        assert_preserved("after the move", cx);

        // Output keeps flowing into the same terminal in its new window. Enough
        // lines to overflow the grid and grow the scrollback, since `total_lines`
        // does not move until the visible rows are full.
        let floating_window =
            registered_floating_window(cx).expect("a floating terminal window was registered");
        let lines_before_output = terminal.read_with(cx, |terminal, _| terminal.total_lines());
        floating_window
            .update(cx, |_, _, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.write_output(&b"still running\n".repeat(200), cx);
                });
            })
            .unwrap();
        cx.run_until_parked();
        terminal.read_with(cx, |terminal, _| {
            assert!(
                terminal.total_lines() > lines_before_output,
                "a moved terminal should still take output"
            );
        });

        floating_window
            .update(cx, |_, window, cx| return_from_window(window, cx))
            .unwrap();
        cx.run_until_parked();

        assert_preserved("after the return", cx);
        terminal_panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.active_pane.read(cx).items_len(),
                1,
                "the return should leave exactly one terminal, not a duplicate"
            );
        });
    }

    fn tab_menu_labels(
        window: WindowHandle<MultiWorkspace>,
        terminal_view: &Entity<TerminalView>,
        cx: &mut TestAppContext,
    ) -> Vec<String> {
        window
            .update(cx, |_, window, cx| {
                terminal_view.update(cx, |terminal_view, cx| {
                    terminal_view
                        .tab_extra_context_menu_actions(window, cx)
                        .into_iter()
                        .map(|(label, _)| label.to_string())
                        .collect()
                })
            })
            .unwrap()
    }

    /// Opens a second floating window the way "Open in New Window" does, with no
    /// registration of its own, and returns it with its workspace.
    async fn open_floating_window(
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> (WindowHandle<MultiWorkspace>, Entity<Workspace>) {
        let (project, app_state) = workspace.read_with(cx, |workspace, _| {
            (workspace.project().clone(), workspace.app_state().clone())
        });
        let window = cx
            .update(|cx| {
                let options = (app_state.build_window_options)(None, cx);
                cx.open_window(options, |window, cx| {
                    let workspace =
                        cx.new(|cx| Workspace::new_floating(project, app_state, window, cx));
                    cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
                })
            })
            .unwrap();
        let floating_workspace = window
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        (window, floating_workspace)
    }

    #[gpui::test]
    async fn test_moving_a_terminal_into_an_already_open_window(cx: &mut TestAppContext) {
        let (source_window, workspace, terminal_panel, terminal_view, terminal) = setup(cx).await;
        let (other_window, other_workspace) = open_floating_window(&workspace, cx).await;

        assert!(
            tab_menu_labels(source_window, &terminal_view, cx)
                .iter()
                .any(|label| label.starts_with("Move to Window:")),
            "an open additional window should be offered as a destination"
        );

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::ExistingWindow(other_window.window_id()),
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        assert_eq!(
            cx.update(|cx| cx.windows()).len(),
            2,
            "moving into an open window should not open another one"
        );
        other_workspace.read_with(cx, |other_workspace, cx| {
            let moved = other_workspace
                .active_pane()
                .read(cx)
                .active_item()
                .expect("the terminal was added to the window that was already open")
                .downcast::<TerminalView>()
                .expect("the item is a terminal");
            assert_eq!(moved.entity_id(), terminal_view.entity_id());
            assert_eq!(
                moved.read(cx).terminal().entity_id(),
                terminal.entity_id(),
                "moving into an open window must not restart the process"
            );
        });
        terminal_panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.active_pane.read(cx).items_len(), 0);
        });
        assert!(
            tab_menu_labels(other_window, &terminal_view, cx)
                .contains(&"Return to Workspace".to_string()),
            "a terminal moved into an existing window should still offer to go back"
        );

        other_window
            .update(cx, |_, window, cx| return_from_window(window, cx))
            .unwrap();
        cx.run_until_parked();

        terminal_panel.read_with(cx, |panel, cx| {
            let returned = panel
                .active_pane
                .read(cx)
                .active_item()
                .expect("the terminal returned to its source pane")
                .downcast::<TerminalView>()
                .expect("the returned item is a terminal");
            assert_eq!(returned.entity_id(), terminal_view.entity_id());
        });
        assert!(
            other_window.update(cx, |_, _, _| ()).is_err(),
            "a window left holding nothing should close behind the returned terminal"
        );
    }

    /// A terminal that was in the center, not in the terminal panel, has to come
    /// back to the center. Revealing the panel for it would leave an empty dock
    /// open that the user cannot get rid of.
    #[gpui::test]
    async fn test_returning_a_center_terminal_leaves_the_panel_alone(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let source_window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = source_window
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        source_window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                    workspace.add_panel(panel, window, cx);
                })
            })
            .unwrap();
        source_window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        let terminal_view = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .active_item()
                .expect("a terminal was opened in the center")
                .downcast::<TerminalView>()
                .expect("the center item is a terminal")
        });
        assert!(
            !workspace.read_with(cx, |workspace, cx| workspace
                .bottom_dock()
                .read(cx)
                .is_open()),
            "the terminal panel starts closed in this fixture"
        );

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::NewWindow,
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        let floating_window =
            registered_floating_window(cx).expect("a floating terminal window was registered");
        floating_window
            .update(cx, |_, window, cx| return_from_window(window, cx))
            .unwrap();
        cx.run_until_parked();

        assert!(
            !workspace.read_with(cx, |workspace, cx| workspace
                .bottom_dock()
                .read(cx)
                .is_open()),
            "returning a center terminal must not open the terminal panel"
        );
        let landed_in_the_center = workspace.read_with(cx, |workspace, cx| {
            workspace.panes().iter().any(|pane| {
                pane.read(cx)
                    .items()
                    .any(|item| item.item_id() == terminal_view.entity_id())
            })
        });
        assert!(
            landed_in_the_center,
            "the terminal should be back in a pane that is still part of the workspace"
        );
    }

    /// A terminal given a pane of its own inside the window it was moved to, and
    /// then handed back, must not leave that pane behind. An empty pane draws no
    /// tab bar and holds nothing, so there is nothing for the user to close.
    #[gpui::test]
    async fn test_returning_a_terminal_does_not_leave_an_empty_pane_behind(
        cx: &mut TestAppContext,
    ) {
        let (source_window, workspace, _terminal_panel, terminal_view, _terminal) = setup(cx).await;
        let (other_window, other_workspace) = open_floating_window(&workspace, cx).await;

        // The window has something of its own, so it survives the return.
        other_window
            .update(cx, |_, window, cx| {
                other_workspace.update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::ExistingWindow(other_window.window_id()),
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        // Split the moved terminal into a pane of its own, the way dragging it
        // into the bottom half of the window does.
        other_window
            .update(cx, |_, window, cx| {
                other_workspace.update(cx, |workspace, cx| {
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.split(SplitDirection::Down, SplitMode::MovePane, window, cx);
                    });
                })
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            other_workspace.read_with(cx, |workspace, _| workspace.panes().len()),
            2,
            "the terminal should now have a pane of its own"
        );

        other_window
            .update(cx, |_, window, cx| return_from_window(window, cx))
            .unwrap();
        cx.run_until_parked();

        assert!(
            other_window.update(cx, |_, _, _| ()).is_ok(),
            "the window still holds a terminal of its own, so it stays open"
        );
        let empty_panes = other_workspace.read_with(cx, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .filter(|pane| pane.read(cx).items_len() == 0)
                .count()
        });
        assert_eq!(
            empty_panes, 0,
            "the pane the terminal vacated should have closed with it, \
             not been left as a blank section the user cannot get rid of"
        );
    }

    /// The panel's own terminal, moved out and back, has to end up in a pane the
    /// panel actually lays out, and leave the dock in a state that matches what
    /// it is holding.
    #[gpui::test]
    async fn test_panel_state_across_a_move_and_return(cx: &mut TestAppContext) {
        let (source_window, workspace, terminal_panel, terminal_view, _terminal) = setup(cx).await;

        let dock_open = |cx: &mut TestAppContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace.bottom_dock().read(cx).is_open()
            })
        };
        let panel_lays_out_its_active_pane = |cx: &mut TestAppContext| {
            terminal_panel.read_with(cx, |panel, _| {
                panel.panes().contains(&&panel.active_pane.clone())
            })
        };

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::NewWindow,
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        let floating_window =
            registered_floating_window(cx).expect("a floating terminal window was registered");
        floating_window
            .update(cx, |_, window, cx| return_from_window(window, cx))
            .unwrap();
        cx.run_until_parked();

        assert!(
            panel_lays_out_its_active_pane(cx),
            "the returned terminal must be in a pane the panel renders"
        );
        assert!(
            dock_open(cx),
            "the dock should be showing the terminal it was given back"
        );
    }

    /// A terminal split into the bottom half of the center is alone in its pane,
    /// so moving it out closes that pane. It has to come back somewhere the
    /// workspace still lays out, and not by opening the terminal panel.
    #[gpui::test]
    async fn test_returning_a_terminal_whose_split_pane_closed(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let source_window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = source_window
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        source_window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                    workspace.add_panel(panel, window, cx);
                })
            })
            .unwrap();

        // One terminal in the original pane, then a second in a pane split off
        // below it, which is the arrangement that leaves an empty pane behind.
        for _ in 0..2 {
            source_window
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                            project.create_terminal_shell(None, cx)
                        })
                    })
                })
                .unwrap()
                .await
                .unwrap();
            cx.run_until_parked();
        }
        let terminal_view = source_window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let terminal_view = workspace
                        .active_pane()
                        .read(cx)
                        .active_item()
                        .expect("two terminals were opened")
                        .downcast::<TerminalView>()
                        .expect("the item is a terminal");
                    workspace.split_item(
                        SplitDirection::Down,
                        Box::new(terminal_view.clone()),
                        window,
                        cx,
                    );
                    terminal_view
                })
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.panes().len()),
            2,
            "the terminal should be alone in a pane of its own"
        );

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::NewWindow,
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.panes().len()),
            1,
            "the emptied split pane should close with the move"
        );

        let floating_window =
            registered_floating_window(cx).expect("a floating terminal window was registered");
        floating_window
            .update(cx, |_, window, cx| return_from_window(window, cx))
            .unwrap();
        cx.run_until_parked();

        assert!(
            !workspace.read_with(cx, |workspace, cx| workspace
                .bottom_dock()
                .read(cx)
                .is_open()),
            "a center terminal must not open the terminal panel on its way back"
        );
        let landed_in_the_center = workspace.read_with(cx, |workspace, cx| {
            workspace.panes().iter().any(|pane| {
                pane.read(cx)
                    .items()
                    .any(|item| item.item_id() == terminal_view.entity_id())
            })
        });
        assert!(
            landed_in_the_center,
            "the terminal should come back to a pane the workspace still lays out"
        );
    }

    /// Another project's windows are its own arrangement, so they are not
    /// offered as somewhere to put this project's terminal.
    #[gpui::test]
    async fn test_a_window_from_another_project_is_not_offered(cx: &mut TestAppContext) {
        let (source_window, _workspace, _terminal_panel, terminal_view, _terminal) =
            setup(cx).await;

        let other_fs = FakeFs::new(cx.executor());
        let other_project = Project::test(other_fs, [], cx).await;
        let other_project_window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(other_project, window, cx));
        let other_project_workspace = other_project_window
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let (floating_window, _) = open_floating_window(&other_project_workspace, cx).await;

        let labels = tab_menu_labels(source_window, &terminal_view, cx);
        assert!(
            !labels
                .iter()
                .any(|label| label.starts_with("Move to Window:")),
            "only this project's windows should be offered, got {labels:?}"
        );

        // It is still a floating window: the filter is the project, not the
        // kind of window.
        assert!(
            cx.update(|cx| workspace::floating_windows(cx))
                .iter()
                .any(|(candidate, _)| candidate.window_id() == floating_window.window_id()),
            "the other project's window should still be a floating window"
        );
    }

    /// The window a terminal was moved into may be showing something of its own,
    /// which the return has no claim on.
    #[gpui::test]
    async fn test_a_window_keeps_its_own_items_when_a_moved_terminal_returns(
        cx: &mut TestAppContext,
    ) {
        let (source_window, workspace, terminal_panel, terminal_view, _terminal) = setup(cx).await;
        let (other_window, other_workspace) = open_floating_window(&workspace, cx).await;

        other_window
            .update(cx, |_, window, cx| {
                other_workspace.update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();
        let started_in_place = other_workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .active_item()
                .expect("a terminal was started in the window")
                .item_id()
        });

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::ExistingWindow(other_window.window_id()),
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();
        other_workspace.read_with(cx, |other_workspace, cx| {
            assert_eq!(other_workspace.active_pane().read(cx).items_len(), 2);
        });

        other_window
            .update(cx, |_, window, cx| return_from_window(window, cx))
            .unwrap();
        cx.run_until_parked();

        terminal_panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.active_pane.read(cx).items_len(),
                1,
                "the moved terminal should be back in its source pane"
            );
        });
        assert!(
            other_window.update(cx, |_, _, _| ()).is_ok(),
            "a window still showing something of its own should stay open"
        );
        other_workspace.read_with(cx, |other_workspace, cx| {
            let remaining = other_workspace.active_pane().read(cx).items().next();
            assert_eq!(
                remaining.map(|item| item.item_id()),
                Some(started_in_place),
                "only the moved terminal should have left"
            );
        });
    }

    /// A floating window opened for some other item (an editor, say) can still
    /// have a terminal started inside it. That terminal was never transferred,
    /// so it has no workspace to be handed back to.
    #[gpui::test]
    async fn test_only_a_transferred_terminal_offers_to_return(cx: &mut TestAppContext) {
        let (source_window, workspace, _terminal_panel, terminal_view, _terminal) = setup(cx).await;

        source_window
            .update(cx, |_, window, cx| {
                move_terminal_to_window(
                    terminal_view.clone(),
                    workspace.clone(),
                    MoveTarget::NewWindow,
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        let transferred_window =
            registered_floating_window(cx).expect("a floating terminal window was registered");
        assert!(
            tab_menu_labels(transferred_window, &terminal_view, cx)
                .contains(&"Return to Workspace".to_string()),
            "the moved terminal should offer to go back"
        );

        // A second floating window, built the way the editor's "Open in New
        // Window" builds one, with a terminal started in it directly.
        let (project, app_state) = workspace.read_with(cx, |workspace, _| {
            (workspace.project().clone(), workspace.app_state().clone())
        });
        let other_window = cx
            .update(|cx| {
                let options = (app_state.build_window_options)(None, cx);
                cx.open_window(options, |window, cx| {
                    let workspace =
                        cx.new(|cx| Workspace::new_floating(project, app_state, window, cx));
                    cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
                })
            })
            .unwrap();
        let other_workspace = other_window
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();

        other_window
            .update(cx, |_, window, cx| {
                other_workspace.update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        let started_in_place = other_workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .active_item()
                .expect("a terminal was started in the floating window")
                .downcast::<TerminalView>()
                .expect("the item is a terminal")
        });

        assert!(
            started_in_place.read_with(cx, |terminal_view, cx| terminal_view.is_floating(cx)),
            "the terminal is in a floating workspace"
        );
        let labels = tab_menu_labels(other_window, &started_in_place, cx);
        assert!(
            !labels.contains(&"Return to Workspace".to_string()),
            "a terminal that was never moved must not offer to go back, got {labels:?}"
        );
        assert!(
            !labels.contains(&"Move to New Window".to_string()),
            "a terminal already in a floating window must not be movable again, got {labels:?}"
        );
    }
}
