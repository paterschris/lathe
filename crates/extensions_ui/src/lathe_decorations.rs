//! Decorations from Lathe extensions: highlights, underlines, gutter marks
//! and end-of-line messages. They are anchored to the buffer's text, so they
//! follow edits, and stored per buffer, so every editor showing a buffer
//! draws them, including editors opened later.

use std::ops::Range;
use std::sync::Arc;

use anyhow::{Result, bail};
use collections::{BTreeMap, HashMap, HashSet};
use editor::{Editor, HighlightKey, Inlay};
use extension_host::ExtensionStore;
use extension_host::wasm_host::lathe::{
    LatheDecoration, LatheDecorationStyle, MAX_DECORATION_LAYERS,
};
use extension_host::wasm_host::lathe_views::Tone;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, HighlightStyle, Hsla,
    UnderlineStyle, WeakEntity, Window, px,
};
use language::{Buffer, Point};
use project::InlayId;
use text::BufferId;
use theme::{ActiveTheme as _, Theme};

const TONES: [Tone; 5] = [
    Tone::Neutral,
    Tone::Accent,
    Tone::Success,
    Tone::Warning,
    Tone::Error,
];
const STYLES: [LatheDecorationStyle; 3] = [
    LatheDecorationStyle::Background,
    LatheDecorationStyle::Underline,
    LatheDecorationStyle::WavyUnderline,
];

#[derive(Debug, Clone)]
struct AnchoredDecoration {
    range: Range<text::Anchor>,
    style: LatheDecorationStyle,
    tone: Tone,
    message: Option<Arc<str>>,
}

struct BufferDecorations {
    buffer: WeakEntity<Buffer>,
    /// Keyed by (extension id, layer).
    layers: BTreeMap<(Arc<str>, Arc<str>), Vec<AnchoredDecoration>>,
}

/// Every extension's decorations, per buffer.
#[derive(Default)]
pub struct ExtensionDecorations {
    buffers: HashMap<BufferId, BufferDecorations>,
    /// The message inlays each editor shows, to remove on its next update.
    editor_inlays: HashMap<EntityId, Vec<InlayId>>,
    next_inlay_id: usize,
}

/// The decorations of this buffer changed.
pub struct DecorationsChanged(pub BufferId);

impl EventEmitter<DecorationsChanged> for ExtensionDecorations {}

pub struct GlobalExtensionDecorations(pub Entity<ExtensionDecorations>);

impl Global for GlobalExtensionDecorations {}

impl ExtensionDecorations {
    /// Replaces `extension_id`'s decorations in `layer` of `buffer`. Ranges
    /// must already be checked against the buffer's current text.
    pub fn set(
        &mut self,
        buffer: &Entity<Buffer>,
        extension_id: Arc<str>,
        layer: Arc<str>,
        decorations: Vec<LatheDecoration>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        self.buffers
            .retain(|_, decorations| decorations.buffer.upgrade().is_some());
        let buffer_id = buffer.read(cx).remote_id();
        let entry = self
            .buffers
            .entry(buffer_id)
            .or_insert_with(|| BufferDecorations {
                buffer: buffer.downgrade(),
                layers: BTreeMap::default(),
            });
        let key = (extension_id.clone(), layer);
        if decorations.is_empty() {
            if entry.layers.remove(&key).is_none() {
                return Ok(());
            }
        } else {
            let layer_count = entry
                .layers
                .keys()
                .filter(|(extension, _)| *extension == extension_id)
                .count();
            if !entry.layers.contains_key(&key) && layer_count >= MAX_DECORATION_LAYERS {
                bail!(
                    "an extension can have at most {MAX_DECORATION_LAYERS} decoration layers per file"
                );
            }
            let buffer = buffer.read(cx);
            let anchored = decorations
                .into_iter()
                .map(|decoration| AnchoredDecoration {
                    range: buffer.anchor_before(decoration.range.start)
                        ..buffer.anchor_after(decoration.range.end),
                    style: decoration.style,
                    tone: decoration.tone,
                    message: decoration.message,
                })
                .collect();
            entry.layers.insert(key, anchored);
        }
        if entry.layers.is_empty() {
            self.buffers.remove(&buffer_id);
        }
        cx.emit(DecorationsChanged(buffer_id));
        Ok(())
    }

    /// Drops the decorations of extensions that are no longer loaded.
    fn retain_extensions(&mut self, loaded: &HashSet<Arc<str>>, cx: &mut Context<Self>) {
        let mut changed = Vec::new();
        self.buffers.retain(|buffer_id, decorations| {
            let before = decorations.layers.len();
            decorations
                .layers
                .retain(|(extension_id, _), _| loaded.contains(extension_id));
            if decorations.layers.len() != before {
                changed.push(*buffer_id);
            }
            !decorations.layers.is_empty()
        });
        for buffer_id in changed {
            cx.emit(DecorationsChanged(buffer_id));
        }
    }

    fn for_buffer(&self, buffer_id: BufferId) -> Vec<AnchoredDecoration> {
        self.buffers
            .get(&buffer_id)
            .map(|decorations| decorations.layers.values().flatten().cloned().collect())
            .unwrap_or_default()
    }
}

pub fn init(cx: &mut App) {
    let decorations = cx.new(|_| ExtensionDecorations::default());
    cx.set_global(GlobalExtensionDecorations(decorations.clone()));

    if let Some(store) = ExtensionStore::try_global(cx) {
        let decorations = decorations.clone();
        cx.observe(&store, move |store, cx| {
            let loaded = store
                .read(cx)
                .wasm_extensions
                .iter()
                .map(|(manifest, _)| manifest.id.clone())
                .collect::<HashSet<_>>();
            decorations.update(cx, |decorations, cx| {
                decorations.retain_extensions(&loaded, cx)
            });
        })
        .detach();
    }

    cx.observe_new(move |editor: &mut Editor, window, cx| {
        let Some(window) = window else {
            return;
        };
        watch_editor(editor, &decorations, window, cx);
    })
    .detach();
}

fn watch_editor(
    editor: &mut Editor,
    decorations: &Entity<ExtensionDecorations>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    cx.subscribe_in(
        decorations,
        window,
        |editor, decorations, DecorationsChanged(buffer_id), _window, cx| {
            if editor_buffer_id(editor, cx) == Some(*buffer_id) {
                apply(editor, decorations, cx);
            }
        },
    )
    .detach();
    let editor_id = cx.entity_id();
    let weak_decorations = decorations.downgrade();
    cx.on_release(move |_, cx| {
        weak_decorations
            .update(cx, |decorations, _| {
                decorations.editor_inlays.remove(&editor_id)
            })
            .ok();
    })
    .detach();
    if let Some(buffer_id) = editor_buffer_id(editor, cx)
        && decorations.read(cx).buffers.contains_key(&buffer_id)
    {
        apply(editor, decorations, cx);
    }
}

fn editor_buffer_id(editor: &Editor, cx: &App) -> Option<BufferId> {
    editor
        .buffer()
        .read(cx)
        .as_singleton()
        .map(|buffer| buffer.read(cx).remote_id())
}

/// Index of a (style, tone) pair, for its highlight key.
fn key_index(style: LatheDecorationStyle, tone: Tone) -> usize {
    let style = STYLES
        .iter()
        .position(|candidate| *candidate == style)
        .unwrap_or(0);
    let tone = TONES
        .iter()
        .position(|candidate| *candidate == tone)
        .unwrap_or(0);
    style * TONES.len() + tone
}

/// Redraws all extension decorations in `editor` from scratch.
fn apply(
    editor: &mut Editor,
    decorations: &Entity<ExtensionDecorations>,
    cx: &mut Context<Editor>,
) {
    for style in STYLES {
        for tone in TONES {
            let key = HighlightKey::LatheExtension(key_index(style, tone));
            editor.clear_background_highlights(key, cx);
            editor.clear_highlights(key, cx);
        }
    }
    clear_gutter(editor, cx);

    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let buffer_snapshot = buffer.read(cx).snapshot();
    let entries = decorations.read(cx).for_buffer(buffer_snapshot.remote_id());
    let multibuffer = editor.buffer().read(cx).snapshot(cx);

    let mut grouped: BTreeMap<(LatheDecorationStyle, usize), Vec<_>> = BTreeMap::default();
    let mut messages = Vec::new();
    for decoration in &entries {
        if let Some(message) = &decoration.message {
            let end = text::ToPoint::to_point(&decoration.range.end, &buffer_snapshot);
            let line_end = buffer_snapshot
                .anchor_after(Point::new(end.row, buffer_snapshot.line_len(end.row)));
            if let Some(position) = multibuffer.anchor_in_buffer(line_end) {
                messages.push((position, format!("  {message}")));
            }
        }
        if let Some(range) = multibuffer.anchor_range_in_buffer(decoration.range.clone()) {
            let tone = TONES
                .iter()
                .position(|tone| *tone == decoration.tone)
                .unwrap_or(0);
            grouped
                .entry((decoration.style, tone))
                .or_default()
                .push(range);
        }
    }

    let theme = cx.theme().clone();
    for ((style, tone_index), ranges) in grouped {
        let tone = TONES[tone_index];
        let key = HighlightKey::LatheExtension(key_index(style, tone));
        match style {
            LatheDecorationStyle::Background => {
                editor.highlight_background(
                    key,
                    &ranges,
                    move |_, theme: &Theme| tone_background(theme, tone),
                    cx,
                );
            }
            LatheDecorationStyle::Underline | LatheDecorationStyle::WavyUnderline => {
                editor.highlight_text_key(
                    key,
                    ranges,
                    HighlightStyle {
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            color: Some(tone_foreground(&theme, tone)),
                            wavy: style == LatheDecorationStyle::WavyUnderline,
                        }),
                        ..HighlightStyle::default()
                    },
                    false,
                    cx,
                );
            }
            LatheDecorationStyle::Gutter => highlight_gutter(editor, tone, ranges, cx),
        }
    }

    let editor_id = cx.entity_id();
    let (old_inlays, new_inlays) = decorations.update(cx, |decorations, _| {
        let old = decorations
            .editor_inlays
            .remove(&editor_id)
            .unwrap_or_default();
        let new = messages
            .into_iter()
            .map(|(position, text)| {
                decorations.next_inlay_id += 1;
                Inlay::lathe_extension(decorations.next_inlay_id, position, text)
            })
            .collect::<Vec<_>>();
        decorations
            .editor_inlays
            .insert(editor_id, new.iter().map(|inlay| inlay.id).collect());
        (old, new)
    });
    if !old_inlays.is_empty() || !new_inlays.is_empty() {
        editor.splice_inlays(&old_inlays, new_inlays, cx);
    }
}

fn tone_foreground(theme: &Theme, tone: Tone) -> Hsla {
    let status = theme.status();
    match tone {
        Tone::Neutral => status.hint,
        Tone::Accent => status.info,
        Tone::Success => status.success,
        Tone::Warning => status.warning,
        Tone::Error => status.error,
    }
}

fn tone_background(theme: &Theme, tone: Tone) -> Hsla {
    let status = theme.status();
    match tone {
        Tone::Neutral => status.hint_background,
        Tone::Accent => status.info_background,
        Tone::Success => status.success_background,
        Tone::Warning => status.warning_background,
        Tone::Error => status.error_background,
    }
}

// Gutter highlights are keyed by type and take a plain `fn`, so each tone gets
// a marker type and a color function.
struct GutterNeutral;
struct GutterAccent;
struct GutterSuccess;
struct GutterWarning;
struct GutterError;

fn highlight_gutter(
    editor: &mut Editor,
    tone: Tone,
    ranges: Vec<Range<editor::Anchor>>,
    cx: &mut Context<Editor>,
) {
    match tone {
        Tone::Neutral => editor.highlight_gutter::<GutterNeutral>(
            ranges,
            |cx| tone_foreground(cx.theme(), Tone::Neutral),
            cx,
        ),
        Tone::Accent => editor.highlight_gutter::<GutterAccent>(
            ranges,
            |cx| tone_foreground(cx.theme(), Tone::Accent),
            cx,
        ),
        Tone::Success => editor.highlight_gutter::<GutterSuccess>(
            ranges,
            |cx| tone_foreground(cx.theme(), Tone::Success),
            cx,
        ),
        Tone::Warning => editor.highlight_gutter::<GutterWarning>(
            ranges,
            |cx| tone_foreground(cx.theme(), Tone::Warning),
            cx,
        ),
        Tone::Error => editor.highlight_gutter::<GutterError>(
            ranges,
            |cx| tone_foreground(cx.theme(), Tone::Error),
            cx,
        ),
    }
}

fn clear_gutter(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor.clear_gutter_highlights::<GutterNeutral>(cx);
    editor.clear_gutter_highlights::<GutterAccent>(cx);
    editor.clear_gutter_highlights::<GutterSuccess>(cx);
    editor.clear_gutter_highlights::<GutterWarning>(cx);
    editor.clear_gutter_highlights::<GutterError>(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lathe_editor::{ExtensionBufferDelegate, ExtensionEditedBuffers};
    use extension_host::wasm_host::lathe::LatheEditorDelegate as _;
    use gpui::{TestAppContext, WindowHandle};
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            init(cx);
        });
    }

    fn decoration(
        range: Range<usize>,
        style: LatheDecorationStyle,
        tone: Tone,
        message: Option<&str>,
    ) -> LatheDecoration {
        LatheDecoration {
            range,
            style,
            tone,
            message: message.map(Into::into),
        }
    }

    fn store(cx: &mut TestAppContext) -> Entity<ExtensionDecorations> {
        cx.update(|cx| cx.global::<GlobalExtensionDecorations>().0.clone())
    }

    fn set(
        buffer: &Entity<Buffer>,
        extension: &str,
        layer: &str,
        decorations: Vec<LatheDecoration>,
        cx: &mut TestAppContext,
    ) -> Result<()> {
        let store = store(cx);
        let result = store.update(cx, |store, cx| {
            store.set(buffer, extension.into(), layer.into(), decorations, cx)
        });
        cx.run_until_parked();
        result
    }

    /// Byte ranges of an editor's underline highlights for a (style, tone).
    fn highlighted(
        editor: &WindowHandle<Editor>,
        style: LatheDecorationStyle,
        tone: Tone,
        cx: &mut TestAppContext,
    ) -> Vec<Range<usize>> {
        let key = HighlightKey::LatheExtension(key_index(style, tone));
        editor
            .update(cx, |editor, _, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                editor
                    .text_highlights(key, cx)
                    .map(|(_, ranges)| {
                        ranges
                            .iter()
                            .map(|range| {
                                editor::ToOffset::to_offset(&range.start, &snapshot).0
                                    ..editor::ToOffset::to_offset(&range.end, &snapshot).0
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap()
    }

    fn has_background(editor: &WindowHandle<Editor>, tone: Tone, cx: &mut TestAppContext) -> bool {
        let key = HighlightKey::LatheExtension(key_index(LatheDecorationStyle::Background, tone));
        editor
            .update(cx, |editor, _, _| editor.has_background_highlights(key))
            .unwrap()
    }

    fn messages(editor: &WindowHandle<Editor>, cx: &mut TestAppContext) -> Vec<String> {
        editor
            .update(cx, |editor, _, cx| {
                editor
                    .all_inlays(cx)
                    .into_iter()
                    .filter(|inlay| matches!(inlay.id, InlayId::LatheExtension(_)))
                    .map(|inlay| inlay.text().to_string())
                    .collect()
            })
            .unwrap()
    }

    #[gpui::test]
    async fn test_decorations_in_editors(cx: &mut TestAppContext) {
        init_test(cx);
        let buffer = cx.new(|cx| Buffer::local("let unused = 1;\nlet x = 2;\n", cx));
        let editor =
            cx.add_window(|window, cx| Editor::for_buffer(buffer.clone(), None, window, cx));
        cx.run_until_parked();

        set(
            &buffer,
            "lint",
            "warnings",
            vec![
                decoration(
                    4..10,
                    LatheDecorationStyle::WavyUnderline,
                    Tone::Warning,
                    Some("never read"),
                ),
                decoration(16..19, LatheDecorationStyle::Underline, Tone::Error, None),
                decoration(20..21, LatheDecorationStyle::Gutter, Tone::Accent, None),
            ],
            cx,
        )
        .unwrap();
        assert_eq!(
            highlighted(
                &editor,
                LatheDecorationStyle::WavyUnderline,
                Tone::Warning,
                cx
            ),
            vec![4..10]
        );
        assert_eq!(
            highlighted(&editor, LatheDecorationStyle::Underline, Tone::Error, cx),
            vec![16..19]
        );
        assert_eq!(messages(&editor, cx), vec!["  never read".to_string()]);
        let gutter_marks = editor
            .update(cx, |editor, _, cx| {
                let display = editor.display_snapshot(cx);
                let buffer = editor.buffer().read(cx).snapshot(cx);
                let all = buffer.anchor_before(editor::MultiBufferOffset(0))
                    ..buffer.anchor_after(buffer.len());
                editor.gutter_highlights_in_range(all, &display, cx).len()
            })
            .unwrap();
        assert_eq!(gutter_marks, 1, "the gutter decoration is drawn");
        let style = editor
            .update(cx, |editor, _, cx| {
                editor
                    .text_highlights(
                        HighlightKey::LatheExtension(key_index(
                            LatheDecorationStyle::WavyUnderline,
                            Tone::Warning,
                        )),
                        cx,
                    )
                    .map(|(style, _)| style)
            })
            .unwrap()
            .expect("the wavy underline is set");
        assert!(style.underline.is_some_and(|underline| underline.wavy));

        // Decorations are anchored: typing before them moves them along.
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "// hi\n")], None, cx));
        cx.run_until_parked();
        set(&buffer, "lint", "other", Vec::new(), cx).unwrap();
        assert_eq!(
            highlighted(
                &editor,
                LatheDecorationStyle::WavyUnderline,
                Tone::Warning,
                cx
            ),
            vec![10..16]
        );

        // An editor opened later shows the buffer's decorations too.
        let later =
            cx.add_window(|window, cx| Editor::for_buffer(buffer.clone(), None, window, cx));
        cx.run_until_parked();
        assert_eq!(
            highlighted(
                &later,
                LatheDecorationStyle::WavyUnderline,
                Tone::Warning,
                cx
            ),
            vec![10..16]
        );
        assert_eq!(messages(&later, cx), vec!["  never read".to_string()]);

        set(&buffer, "lint", "warnings", Vec::new(), cx).unwrap();
        for editor in [&editor, &later] {
            assert!(
                highlighted(
                    editor,
                    LatheDecorationStyle::WavyUnderline,
                    Tone::Warning,
                    cx
                )
                .is_empty()
            );
            assert!(
                messages(editor, cx).is_empty(),
                "clearing removes the messages"
            );
        }
    }

    #[gpui::test]
    async fn test_layers_background_and_unload(cx: &mut TestAppContext) {
        init_test(cx);
        let buffer = cx.new(|cx| Buffer::local("abcdef", cx));
        let editor =
            cx.add_window(|window, cx| Editor::for_buffer(buffer.clone(), None, window, cx));
        cx.run_until_parked();

        set(
            &buffer,
            "a",
            "spell",
            vec![decoration(
                0..3,
                LatheDecorationStyle::Background,
                Tone::Success,
                None,
            )],
            cx,
        )
        .unwrap();
        assert!(has_background(&editor, Tone::Success, cx));

        for layer in 1..MAX_DECORATION_LAYERS {
            set(
                &buffer,
                "a",
                &format!("layer-{layer}"),
                vec![decoration(
                    0..1,
                    LatheDecorationStyle::Underline,
                    Tone::Neutral,
                    None,
                )],
                cx,
            )
            .unwrap();
        }
        let error = set(
            &buffer,
            "a",
            "one-too-many",
            vec![decoration(
                0..1,
                LatheDecorationStyle::Underline,
                Tone::Neutral,
                None,
            )],
            cx,
        )
        .unwrap_err();
        assert!(error.to_string().contains("at most"), "{error}");
        set(
            &buffer,
            "b",
            "own-layers",
            vec![decoration(
                3..6,
                LatheDecorationStyle::Underline,
                Tone::Error,
                None,
            )],
            cx,
        )
        .expect("the cap is per extension");

        let store = store(cx);
        store.update(cx, |store, cx| {
            store.retain_extensions(&HashSet::from_iter([Arc::<str>::from("b")]), cx)
        });
        cx.run_until_parked();
        assert!(
            !has_background(&editor, Tone::Success, cx),
            "an unloaded extension's decorations go away"
        );
        assert_eq!(
            highlighted(&editor, LatheDecorationStyle::Underline, Tone::Error, cx),
            vec![3..6]
        );
    }

    #[gpui::test]
    async fn test_delegate_checks_version_and_ranges(cx: &mut TestAppContext) {
        init_test(cx);
        let buffer = cx.new(|cx| Buffer::local("hello", cx));
        let delegate =
            ExtensionBufferDelegate::new(buffer.downgrade(), ExtensionEditedBuffers::default());
        let mut async_cx = cx.to_async();
        let one = |range: Range<usize>| {
            vec![decoration(
                range,
                LatheDecorationStyle::Underline,
                Tone::Error,
                None,
            )]
        };

        let version = delegate.snapshot(&mut async_cx).unwrap().unwrap().version;
        let error = delegate
            .set_decorations(version, "ext".into(), "l".into(), one(0..99), &mut async_cx)
            .unwrap_err();
        assert!(format!("{error:#}").contains("past the end"), "{error:#}");
        delegate
            .set_decorations(version, "ext".into(), "l".into(), one(0..5), &mut async_cx)
            .expect("in bounds and current");
        delegate
            .set_decorations(version, "ext".into(), "l".into(), one(1..2), &mut async_cx)
            .expect("decorating doesn't use up the snapshot");

        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "x")], None, cx));
        let error = delegate
            .set_decorations(version, "ext".into(), "l".into(), one(0..1), &mut async_cx)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("changed since it was read"),
            "{error:#}"
        );
    }
}
