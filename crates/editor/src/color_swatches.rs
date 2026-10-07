use std::time::Duration;

use collections::HashMap;
use gpui::{AppContext as _, Rgba, Task};
use multi_buffer::{Anchor, MultiBufferRow, MultiBufferSnapshot, ToOffset as _};
use project::InlayId;
use settings::Settings as _;
use text::Point;
use ui::Context;
use util::post_inc;

use crate::{
    Editor, EditorSettings, color_picker::color_literals,
    editor_settings::DocumentColorsRenderMode, inlays::Inlay,
};

const SCAN_DEBOUNCE: Duration = Duration::from_millis(75);

/// Scanning is linear in the buffer, so very large files (minified bundles,
/// generated data) are left without swatches rather than stalling on every
/// keystroke.
const MAX_SCANNED_BYTES: usize = 2 * 1024 * 1024;

/// Swatches for color literals found by scanning the text itself, so colors
/// get one whether or not a language server reports document colors.
#[derive(Default)]
pub(crate) struct ColorSwatches {
    inlays: Vec<(Anchor, [u8; 4], InlayId)>,
    _scan_task: Option<Task<()>>,
}

fn color_key(color: Rgba) -> [u8; 4] {
    [color.r, color.g, color.b, color.a].map(|channel| (channel.clamp(0., 1.) * 255.).round() as u8)
}

/// Languages where a short all-digit `#123` is far more likely to be an issue
/// or PR reference than a color.
fn is_prose(language_name: Option<&str>) -> bool {
    matches!(
        language_name,
        None | Some("Markdown" | "Git Commit" | "Plain Text")
    )
}

fn scan(snapshot: &MultiBufferSnapshot) -> Vec<(Point, Rgba)> {
    let mut found = Vec::new();
    for row in 0..=snapshot.max_point().row {
        let line_end = Point::new(row, snapshot.line_len(MultiBufferRow(row)));
        let line: String = snapshot
            .text_for_range(Point::new(row, 0)..line_end)
            .collect();
        let literals = color_literals(&line);
        if literals.is_empty() {
            continue;
        }
        let prose = is_prose(
            snapshot
                .language_at(Point::new(row, 0))
                .map(|language| language.name())
                .as_ref()
                .map(|name| name.as_ref()),
        );
        for literal in literals {
            if prose && literal.is_ambiguous_with_issue_number(&line) {
                continue;
            }
            found.push((
                Point::new(row, literal.range().start as u32),
                literal.color(),
            ));
        }
    }
    found
}

impl Editor {
    pub(crate) fn refresh_color_swatches(&mut self, cx: &mut Context<Self>) {
        let enabled = self.mode().is_full()
            && EditorSettings::get_global(cx).lsp_document_colors != DocumentColorsRenderMode::None;
        if !enabled {
            self.color_swatches._scan_task = None;
            let to_remove: Vec<InlayId> = self
                .color_swatches
                .inlays
                .drain(..)
                .map(|(_, _, id)| id)
                .collect();
            if !to_remove.is_empty() {
                self.splice_inlays(&to_remove, Vec::new(), cx);
            }
            return;
        }

        let snapshot = self.buffer().read(cx).snapshot(cx);
        self.color_swatches._scan_task = Some(cx.spawn(async move |editor, cx| {
            cx.background_executor().timer(SCAN_DEBOUNCE).await;
            let found = cx
                .background_spawn(async move {
                    if snapshot.len().0 > MAX_SCANNED_BYTES {
                        return (snapshot, Vec::new());
                    }
                    let found = scan(&snapshot);
                    (snapshot, found)
                })
                .await;
            editor
                .update(cx, |editor, cx| {
                    let (snapshot, found) = found;
                    editor.apply_color_swatches(&snapshot, found, cx);
                })
                .ok();
        }));
    }

    fn apply_color_swatches(
        &mut self,
        snapshot: &MultiBufferSnapshot,
        found: Vec<(Point, Rgba)>,
        cx: &mut Context<Self>,
    ) {
        let lsp_buffers_with_swatches = |anchor: Anchor| {
            snapshot
                .anchor_to_buffer_anchor(anchor)
                .is_some_and(|(buffer_anchor, _)| {
                    self.colors
                        .as_ref()
                        .is_some_and(|colors| colors.draws_inlays_for(buffer_anchor.buffer_id))
                })
        };

        // Existing swatches that still match a literal keep their inlay, so an
        // edit elsewhere in the file doesn't churn every swatch on screen.
        let mut existing: HashMap<(usize, [u8; 4]), InlayId> = self
            .color_swatches
            .inlays
            .drain(..)
            .map(|(anchor, color, id)| ((anchor.to_offset(snapshot).0, color), id))
            .collect();

        let mut inlays = Vec::with_capacity(found.len());
        let mut to_insert = Vec::new();
        for (point, color) in found {
            let anchor = snapshot.anchor_before(point);
            if lsp_buffers_with_swatches(anchor) {
                continue;
            }
            let key = color_key(color);
            let offset = anchor.to_offset(snapshot).0;
            let id = match existing.remove(&(offset, key)) {
                Some(id) => id,
                None => {
                    let inlay =
                        Inlay::color(post_inc(&mut self.next_color_inlay_id), anchor, color);
                    let id = inlay.id;
                    to_insert.push(inlay);
                    id
                }
            };
            inlays.push((anchor, key, id));
        }
        self.color_swatches.inlays = inlays;

        let to_remove: Vec<InlayId> = existing.into_values().collect();
        if !to_remove.is_empty() || !to_insert.is_empty() {
            self.splice_inlays(&to_remove, to_insert, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{editor_tests::init_test, test::build_editor};
    use gpui::TestAppContext;
    use multi_buffer::MultiBuffer;

    fn swatch_colors(editor: &Editor, cx: &gpui::App) -> Vec<[u8; 4]> {
        editor
            .all_inlays(cx)
            .into_iter()
            .filter_map(|inlay| inlay.get_color())
            .map(|color| color_key(Rgba::from(color)))
            .collect()
    }

    #[gpui::test]
    async fn swatches_follow_edits_without_a_language_server(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let buffer =
            cx.update(|cx| MultiBuffer::build_simple("a { color: #ff0000; }\nsee #123\n", cx));
        let (editor, cx) =
            cx.add_window_view(|window, cx| build_editor(buffer.clone(), window, cx));
        cx.executor().advance_clock(SCAN_DEBOUNCE * 2);
        cx.run_until_parked();

        // No language means prose, so the issue-like `#123` gets no swatch.
        editor.update(cx, |editor, cx| {
            assert_eq!(swatch_colors(editor, cx), vec![[255, 0, 0, 255]]);
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.set_text("rgb(0, 0, 255) and 0x00ff00", window, cx);
        });
        cx.executor().advance_clock(SCAN_DEBOUNCE * 2);
        cx.run_until_parked();

        editor.update(cx, |editor, cx| {
            assert_eq!(
                swatch_colors(editor, cx),
                vec![[0, 0, 255, 255], [0, 255, 0, 255]]
            );
        });
    }

    #[test]
    fn prose_languages_are_detected() {
        assert!(is_prose(None));
        assert!(is_prose(Some("Markdown")));
        assert!(is_prose(Some("Git Commit")));
        assert!(!is_prose(Some("CSS")));
        assert!(!is_prose(Some("Rust")));
    }
}
