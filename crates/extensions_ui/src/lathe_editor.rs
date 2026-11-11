use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, bail};
use editor::{Editor, MultiBufferOffset, SelectionEffects};
use extension_host::wasm_host::lathe::{
    ActiveBufferSnapshot, LatheDecoration, LatheEditorDelegate, validate_ranges,
};
use gpui::{AnyWindowHandle, AsyncApp, Entity, WeakEntity};
use language::Buffer;
use parking_lot::Mutex;

/// Gives an extension command access to the editor it was run from.
///
/// Every snapshot handed out gets a fresh version number, remembered with the
/// buffer's version at that moment. An edit or selection change is only
/// applied when it names the latest snapshot and the buffer has not changed
/// since, so an extension never overwrites text it has not seen.
pub struct ExtensionEditorDelegate {
    editor: WeakEntity<Editor>,
    window: AnyWindowHandle,
    next_version: AtomicU64,
    latest: Mutex<Option<(u64, clock::Global)>>,
}

impl ExtensionEditorDelegate {
    pub fn new(editor: WeakEntity<Editor>, window: AnyWindowHandle) -> Self {
        Self {
            editor,
            window,
            next_version: AtomicU64::new(1),
            latest: Mutex::new(None),
        }
    }

    fn editor_and_buffer(
        &self,
        cx: &mut AsyncApp,
    ) -> Result<Option<(Entity<Editor>, Entity<Buffer>)>> {
        let editor = self.editor.upgrade().context("the editor was closed")?;
        let buffer = editor.read_with(cx, |editor, cx| editor.buffer().read(cx).as_singleton());
        Ok(buffer.map(|buffer| (editor, buffer)))
    }
}

impl LatheEditorDelegate for ExtensionEditorDelegate {
    fn snapshot(&self, cx: &mut AsyncApp) -> Result<Option<ActiveBufferSnapshot>> {
        let Some((editor, buffer)) = self.editor_and_buffer(cx)? else {
            return Ok(None);
        };
        let selections = self.window.update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                let display_snapshot = editor.display_snapshot(cx);
                editor
                    .selections
                    .all::<MultiBufferOffset>(&display_snapshot)
                    .into_iter()
                    .map(|selection| selection.start.0..selection.end.0)
                    .collect::<Vec<_>>()
            })
        })?;
        let (buffer_version, location, language, text) = buffer.read_with(cx, |buffer, cx| {
            (
                buffer.version(),
                file_location(buffer, cx),
                buffer
                    .language()
                    .map(|language| language.name().to_string()),
                buffer.text(),
            )
        });
        let version = self.next_version.fetch_add(1, Ordering::Relaxed);
        *self.latest.lock() = Some((version, buffer_version));
        Ok(Some(ActiveBufferSnapshot {
            version,
            path: location.path,
            abs_path: location.abs_path,
            worktree_root: location.worktree_root,
            language,
            text,
            selections,
        }))
    }

    fn edit(
        &self,
        version: u64,
        edits: Vec<(Range<usize>, String)>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (editor, buffer) = self
            .editor_and_buffer(cx)?
            .context("the editor no longer shows a single file")?;
        let text = claim(&self.latest, version, &buffer, cx)?;
        validate_ranges(&text, edits.iter().map(|(range, _)| range), true)?;
        if editor.read_with(cx, |editor, cx| editor.read_only(cx)) {
            bail!("the editor is read-only");
        }
        self.window.update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.transact(window, cx, |editor, _window, cx| {
                    editor.edit(
                        edits.into_iter().map(|(range, text)| {
                            (
                                MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                                text,
                            )
                        }),
                        cx,
                    );
                });
            })
        })?;
        Ok(())
    }

    fn set_decorations(
        &self,
        version: u64,
        extension_id: Arc<str>,
        layer: Arc<str>,
        decorations: Vec<LatheDecoration>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (_, buffer) = self
            .editor_and_buffer(cx)?
            .context("the editor no longer shows a single file")?;
        set_decorations(
            &self.latest,
            version,
            &buffer,
            extension_id,
            layer,
            decorations,
            cx,
        )
    }

    fn file_paths(&self, cx: &mut AsyncApp) -> (Option<String>, Option<String>) {
        let Ok(Some((_, buffer))) = self.editor_and_buffer(cx) else {
            return (None, None);
        };
        let location = buffer.read_with(cx, |buffer, cx| file_location(buffer, cx));
        (location.path, location.abs_path)
    }

    fn settings_location(
        &self,
        cx: &mut AsyncApp,
    ) -> Option<(settings::WorktreeId, Arc<util::rel_path::RelPath>)> {
        let (_, buffer) = self.editor_and_buffer(cx).ok()??;
        buffer.read_with(cx, |buffer, cx| settings_location(buffer, cx))
    }

    fn select(&self, version: u64, ranges: Vec<Range<usize>>, cx: &mut AsyncApp) -> Result<()> {
        let (editor, buffer) = self
            .editor_and_buffer(cx)?
            .context("the editor no longer shows a single file")?;
        let text = claim(&self.latest, version, &buffer, cx)?;
        validate_ranges(&text, &ranges, false)?;
        if ranges.is_empty() {
            bail!("at least one selection is required");
        }
        self.window.update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                    selections.select_ranges(
                        ranges.into_iter().map(|range| {
                            MultiBufferOffset(range.start)..MultiBufferOffset(range.end)
                        }),
                    )
                });
            })
        })?;
        Ok(())
    }
}

/// Gives an extension's buffer event handler access to the buffer the event
/// is about. Same version rules as [`ExtensionEditorDelegate`]; there is no
/// editor, so there are no selections to read or change.
pub struct ExtensionBufferDelegate {
    buffer: WeakEntity<Buffer>,
    next_version: AtomicU64,
    latest: Mutex<Option<(u64, clock::Global)>>,
    edited_by_extensions: ExtensionEditedBuffers,
}

/// Buffer versions produced by extension edits during event handlers, so the
/// resulting change is not reported back to extensions as a new `changed`
/// event (which would let an extension that edits on change loop forever).
#[derive(Clone, Default)]
pub struct ExtensionEditedBuffers(Arc<Mutex<collections::HashMap<text::BufferId, clock::Global>>>);

impl ExtensionEditedBuffers {
    /// Whether the buffer is exactly as an extension left it.
    pub fn is_unchanged_since_extension_edit(&self, buffer: &Buffer) -> bool {
        self.0
            .lock()
            .get(&buffer.remote_id())
            .is_some_and(|version| *version == buffer.version())
    }

    pub fn forget(&self, buffer_id: text::BufferId) {
        self.0.lock().remove(&buffer_id);
    }
}

impl ExtensionBufferDelegate {
    pub fn new(buffer: WeakEntity<Buffer>, edited_by_extensions: ExtensionEditedBuffers) -> Self {
        Self {
            buffer,
            next_version: AtomicU64::new(1),
            latest: Mutex::new(None),
            edited_by_extensions,
        }
    }
}

impl LatheEditorDelegate for ExtensionBufferDelegate {
    fn snapshot(&self, cx: &mut AsyncApp) -> Result<Option<ActiveBufferSnapshot>> {
        let Some(buffer) = self.buffer.upgrade() else {
            return Ok(None);
        };
        let (buffer_version, location, language, text) = buffer.read_with(cx, |buffer, cx| {
            (
                buffer.version(),
                file_location(buffer, cx),
                buffer
                    .language()
                    .map(|language| language.name().to_string()),
                buffer.text(),
            )
        });
        let version = self.next_version.fetch_add(1, Ordering::Relaxed);
        *self.latest.lock() = Some((version, buffer_version));
        Ok(Some(ActiveBufferSnapshot {
            version,
            path: location.path,
            abs_path: location.abs_path,
            worktree_root: location.worktree_root,
            language,
            text,
            selections: Vec::new(),
        }))
    }

    fn edit(
        &self,
        version: u64,
        edits: Vec<(Range<usize>, String)>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let buffer = self.buffer.upgrade().context("the buffer was closed")?;
        let text = claim(&self.latest, version, &buffer, cx)?;
        validate_ranges(&text, edits.iter().map(|(range, _)| range), true)?;
        if buffer.read_with(cx, |buffer, _| buffer.read_only()) {
            bail!("the buffer is read-only");
        }
        let (buffer_id, version_after_edit) = buffer.update(cx, |buffer, cx| {
            buffer.edit(edits, None, cx);
            (buffer.remote_id(), buffer.version())
        });
        self.edited_by_extensions
            .0
            .lock()
            .insert(buffer_id, version_after_edit);
        Ok(())
    }

    fn set_decorations(
        &self,
        version: u64,
        extension_id: Arc<str>,
        layer: Arc<str>,
        decorations: Vec<LatheDecoration>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let buffer = self.buffer.upgrade().context("the buffer was closed")?;
        set_decorations(
            &self.latest,
            version,
            &buffer,
            extension_id,
            layer,
            decorations,
            cx,
        )
    }

    fn file_paths(&self, cx: &mut AsyncApp) -> (Option<String>, Option<String>) {
        let Some(buffer) = self.buffer.upgrade() else {
            return (None, None);
        };
        let location = buffer.read_with(cx, |buffer, cx| file_location(buffer, cx));
        (location.path, location.abs_path)
    }

    fn settings_location(
        &self,
        cx: &mut AsyncApp,
    ) -> Option<(settings::WorktreeId, Arc<util::rel_path::RelPath>)> {
        let buffer = self.buffer.upgrade()?;
        buffer.read_with(cx, |buffer, cx| settings_location(buffer, cx))
    }

    fn select(&self, _version: u64, _ranges: Vec<Range<usize>>, _cx: &mut AsyncApp) -> Result<()> {
        bail!("selections can only be changed from a command run in an editor")
    }
}

fn settings_location(
    buffer: &Buffer,
    cx: &gpui::App,
) -> Option<(settings::WorktreeId, Arc<util::rel_path::RelPath>)> {
    let file = buffer.file()?;
    Some((file.worktree_id(cx), file.path().clone()))
}

#[derive(Default)]
pub(crate) struct FileLocation {
    pub(crate) path: Option<String>,
    pub(crate) abs_path: Option<String>,
    pub(crate) worktree_root: Option<String>,
}

pub(crate) fn file_location(buffer: &Buffer, cx: &gpui::App) -> FileLocation {
    let Some(file) = buffer.file() else {
        return FileLocation::default();
    };
    let path = Some(file.path().as_unix_str().to_string());
    let Some(local) = file.as_local() else {
        return FileLocation {
            path,
            ..Default::default()
        };
    };
    let abs_path = local.abs_path(cx);
    // The project folder is the absolute path minus the project-relative one.
    // A worktree that is a single file has an empty relative path; its folder
    // is the file's parent.
    let depth = file.path().components().count();
    let worktree_root = if depth == 0 {
        abs_path.parent().map(std::path::Path::to_path_buf)
    } else {
        abs_path
            .ancestors()
            .nth(depth)
            .map(std::path::Path::to_path_buf)
    };
    FileLocation {
        path,
        abs_path: Some(abs_path.to_string_lossy().into_owned()),
        worktree_root: worktree_root.map(|root| root.to_string_lossy().into_owned()),
    }
}

/// Checks that `version` is the latest snapshot and the buffer is unchanged,
/// then forgets it so the same snapshot cannot be used twice.
fn claim(
    latest: &Mutex<Option<(u64, clock::Global)>>,
    version: u64,
    buffer: &Entity<Buffer>,
    cx: &mut AsyncApp,
) -> Result<String> {
    let mut latest = latest.lock();
    let text = check_current(latest.as_ref(), version, buffer, cx)?;
    *latest = None;
    Ok(text)
}

/// Checks that `version` is the latest snapshot and the buffer is unchanged
/// since, returning its text.
fn check_current(
    latest: Option<&(u64, clock::Global)>,
    version: u64,
    buffer: &Entity<Buffer>,
    cx: &mut AsyncApp,
) -> Result<String> {
    let Some((latest_version, buffer_version)) = latest else {
        bail!("read the buffer with active_buffer() before changing it");
    };
    let (current_version, text) =
        buffer.read_with(cx, |buffer, _| (buffer.version(), buffer.text()));
    if *latest_version != version || *buffer_version != current_version {
        bail!("the buffer changed since it was read; read it again and retry");
    }
    Ok(text)
}

/// Decorations don't use up the snapshot, unlike edits: an extension can
/// decorate and then edit from the same read.
fn set_decorations(
    latest: &Mutex<Option<(u64, clock::Global)>>,
    version: u64,
    buffer: &Entity<Buffer>,
    extension_id: Arc<str>,
    layer: Arc<str>,
    decorations: Vec<LatheDecoration>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let text = check_current(latest.lock().as_ref(), version, buffer, cx)?;
    validate_ranges(
        &text,
        decorations.iter().map(|decoration| &decoration.range),
        false,
    )?;
    let store = cx
        .update(|cx| {
            cx.try_global::<crate::lathe_decorations::GlobalExtensionDecorations>()
                .map(|global| global.0.clone())
        })
        .context("decorations are not available in this build of Lathe")?;
    store.update(cx, |store, cx| {
        store.set(buffer, extension_id, layer, decorations, cx)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::{Editor, actions::Undo};
    use gpui::{AppContext as _, TestAppContext};
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
    }

    #[gpui::test]
    async fn test_editor_delegate(cx: &mut TestAppContext) {
        init_test(cx);
        let buffer = cx.new(|cx| Buffer::local("héllo\nwörld\n", cx));
        let window =
            cx.add_window(|window, cx| Editor::for_buffer(buffer.clone(), None, window, cx));
        let editor = window.root(cx).unwrap();
        window
            .update(cx, |editor, window, cx| {
                editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                    selections.select_ranges([
                        MultiBufferOffset(0)..MultiBufferOffset(6),
                        MultiBufferOffset(7)..MultiBufferOffset(13),
                    ])
                });
            })
            .unwrap();

        let delegate = ExtensionEditorDelegate::new(editor.downgrade(), window.into());
        let mut async_cx = cx.to_async();

        let snapshot = delegate.snapshot(&mut async_cx).unwrap().unwrap();
        assert_eq!(snapshot.text, "héllo\nwörld\n");
        assert_eq!(snapshot.selections, vec![0..6, 7..13]);
        assert_eq!(snapshot.path, None);

        let error = delegate
            .edit(snapshot.version, vec![(0..2, "x".into())], &mut async_cx)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("character boundary"),
            "{error:#}"
        );
        let error = delegate
            .edit(snapshot.version, vec![(0..1, "x".into())], &mut async_cx)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("read the buffer"),
            "a rejected edit still uses up the snapshot: {error:#}"
        );

        let snapshot = delegate.snapshot(&mut async_cx).unwrap().unwrap();
        delegate
            .edit(
                snapshot.version,
                snapshot
                    .selections
                    .iter()
                    .map(|range| (range.clone(), snapshot.text[range.clone()].to_uppercase()))
                    .collect(),
                &mut async_cx,
            )
            .unwrap();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "HÉLLO\nWÖRLD\n"
        );

        let error = delegate
            .edit(snapshot.version, vec![(0..0, "!".into())], &mut async_cx)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("read the buffer"),
            "{error:#}"
        );

        let stale = delegate.snapshot(&mut async_cx).unwrap().unwrap();
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "user typed ")], None, cx)
        });
        let error = delegate
            .select(stale.version, vec![0..1], &mut async_cx)
            .unwrap_err();
        assert!(format!("{error:#}").contains("changed since"), "{error:#}");
        buffer.update(cx, |buffer, cx| buffer.undo(cx));

        let snapshot = delegate.snapshot(&mut async_cx).unwrap().unwrap();
        delegate
            .select(snapshot.version, vec![3..3], &mut async_cx)
            .unwrap();
        let snapshot = delegate.snapshot(&mut async_cx).unwrap().unwrap();
        assert_eq!(snapshot.selections, vec![3..3]);

        window
            .update(cx, |editor, window, cx| editor.undo(&Undo, window, cx))
            .unwrap();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "héllo\nwörld\n",
            "both selections were upcased in one undo step"
        );
    }
}
