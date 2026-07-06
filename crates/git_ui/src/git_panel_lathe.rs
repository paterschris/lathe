//! Lathe-owned extensions to the git panel.
//!
//! This is a child module of [`super`] (`git_panel`), declared there via
//! `#[path = "git_panel_lathe.rs"] mod lathe;`. Being a child module, it can
//! reach `GitPanel`'s private fields and methods, so Lathe feature code can move
//! out of the upstream-owned `git_panel.rs` file without loosening any
//! visibility or changing behavior. The upstream file keeps only the narrow call
//! sites (`lathe::...`) that reach into these items.
//!
//! See `EDOC/lathe-extraction-plan.md` (WP1) for the migration of the remaining
//! git-panel customizations (explorer tab, history tab, repos strip, inline
//! hunks) into this module.

use super::*;

/// Open the raw output of a git command in a read-only editor in the center
/// group. Used by the error toast's "View Log" action and the push-result
/// toast. ANSI control codes are stripped via [`GitOutputHandler`] so the log
/// reads as plain text.
pub(super) fn open_output(
    operation: impl Into<SharedString>,
    workspace: &mut Workspace,
    output: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let operation = operation.into();

    let mut handler = GitOutputHandler::default();
    let mut processor = ansi::Processor::<ansi::StdSyncHandler>::default();
    processor.advance(&mut handler, output.as_bytes());
    let plain_text = handler.output;

    let buffer = cx.new(|cx| Buffer::local(plain_text.as_str(), cx));
    buffer.update(cx, |buffer, cx| {
        buffer.set_capability(language::Capability::ReadOnly, cx);
    });
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_buffer(buffer, None, window, cx);
        editor.buffer().update(cx, |buffer, cx| {
            buffer.set_title(format!("Output from git {operation}"), cx);
        });
        editor.set_read_only(true);
        editor
    });

    workspace.add_item_to_center(Box::new(editor), window, cx);
}

/// ANSI handler that accumulates a git command's output as plain text, honoring
/// carriage returns (so progress lines that redraw in place collapse to their
/// final state) and tabs.
#[derive(Default)]
struct GitOutputHandler {
    output: String,
    line_start: usize,
}

impl ansi::Handler for GitOutputHandler {
    fn input(&mut self, c: char) {
        self.output.push(c);
    }

    fn linefeed(&mut self) {
        self.output.push('\n');
        self.line_start = self.output.len();
    }

    fn carriage_return(&mut self) {
        self.output.truncate(self.line_start);
    }

    fn put_tab(&mut self, count: u16) {
        self.output
            .extend(std::iter::repeat_n('\t', count as usize));
    }
}

#[derive(Clone, Copy)]
pub(super) enum StashOp {
    Pop,
    Apply,
}

impl StashOp {
    fn label(self) -> &'static str {
        match self {
            StashOp::Pop => "stash pop",
            StashOp::Apply => "stash apply",
        }
    }
}

/// Run a stash pop/apply against `repo`, surfacing any failure as an error toast.
pub(super) fn run_stash_op(
    cx: &mut App,
    workspace: WeakEntity<Workspace>,
    repo: Entity<Repository>,
    op: StashOp,
    index: usize,
) {
    let label = op.label();
    cx.spawn(async move |cx| {
        let task = repo.update(cx, |repo, cx| match op {
            StashOp::Pop => repo.stash_pop(Some(index), cx),
            StashOp::Apply => repo.stash_apply(Some(index), cx),
        });
        if let Err(err) = task.await {
            let Some(workspace) = workspace.upgrade() else {
                log::error!("git {label} failed: {err:?}");
                return;
            };
            cx.update(|cx| show_error_toast(workspace, label, err, cx));
        }
    })
    .detach();
}

/// Await the result of a branch operation kicked off elsewhere, refresh the
/// explorer on success, and surface any failure as an error toast.
pub(super) fn run_branch_op(
    cx: &mut App,
    workspace: WeakEntity<Workspace>,
    panel: WeakEntity<GitPanel>,
    receiver: oneshot::Receiver<anyhow::Result<()>>,
    action: impl Into<SharedString>,
) {
    let action = action.into();
    cx.spawn(async move |cx| {
        let result = receiver.await;
        let err = match result {
            Ok(Ok(())) => {
                panel
                    .update(cx, |panel, cx| panel.refresh_explorer_data(cx))
                    .ok();
                return;
            }
            Ok(Err(e)) => e,
            Err(_) => anyhow::anyhow!("operation cancelled"),
        };
        let Ok(workspace) = workspace.upgrade().ok_or(()) else {
            log::error!("git {action} failed: {err:?}");
            return;
        };
        let _ = cx.update(|cx| show_error_toast(workspace, action, err, cx));
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_output_handler_strips_ansi_codes() {
        use alacritty_terminal::vte::ansi;

        let cases = [
            ("no escape codes here\n", "no escape codes here\n"),
            ("\x1b[31mhello\x1b[0m", "hello"),
            ("\x1b[1;32mfoo\x1b[0m bar", "foo bar"),
            ("progress 10%\rprogress 100%\n", "progress 100%\n"),
        ];

        for (input, expected) in cases {
            let mut handler = GitOutputHandler::default();
            let mut processor = ansi::Processor::<ansi::StdSyncHandler>::default();
            processor.advance(&mut handler, input.as_bytes());
            assert_eq!(handler.output, expected);
        }
    }
}
