//! The notebook's live shared buffer.
//!
//! [`ShadowDocument`] is a pure projection: cells in, text out. This module holds the
//! running version of it. It owns one project-registered [`Buffer`] containing that
//! text and tracks each cell's position in it with anchors, so positions survive edits
//! made anywhere in the notebook.
//!
//! The point of it is that a cell editor can be an excerpt over this one buffer
//! instead of a detached `Buffer::local`. That gives a cell the project attachment it
//! needs for completions and diagnostics, and gives a language server one document in
//! which a name defined in an earlier cell is in scope.

use std::ops::Range;

use collections::HashMap;
use gpui::{App, Entity};
use language::{Anchor, Buffer, Language, Point, ToOffset as _, ToPoint as _};
use nbformat::v4::{Cell, CellId, CellType};
use project::{Project, ProjectPath};
use util::rel_path::RelPath;
use std::sync::Arc;

use super::cell::CellBacking;
use super::shadow_document::ShadowDocument;


/// The hidden sibling file a notebook's projection is written to when
/// `jupyter.language_server_sidecar` is on.
///
/// Zed only registers language servers for buffers backed by a local file
/// (`LspStore::register_buffer_with_language_servers`, which returns early when
/// `File::from_dyn` yields `None` because "we don't support non-file URI schemes in our
/// LSP impl"). So a notebook's cells can only get completions and diagnostics if the
/// buffer they excerpt is a real file on disk.
///
/// The file is named after the notebook with a leading dot, sits beside it so relative
/// imports and the project's Python configuration resolve, and is removed when the
/// notebook closes.
pub struct SidecarPath {
    pub project_path: ProjectPath,
}

impl SidecarPath {
    /// Derives the sidecar path for `notebook_path`, using `extension` for the
    /// notebook's language (`py` for Python).
    ///
    /// Returns `None` when the notebook's own path has no file name, which a path
    /// pointing at a directory would not.
    pub fn for_notebook(notebook_path: &ProjectPath, extension: &str) -> Option<Self> {
        let file_name = notebook_path.path.file_name()?;
        let stem = file_name.strip_suffix(".ipynb").unwrap_or(file_name);
        let sidecar_name = format!(".{stem}.lathe.{extension}");

        let mut path = notebook_path
            .path
            .parent()
            .unwrap_or(RelPath::empty())
            .to_rel_path_buf();
        path.push_component(&sidecar_name).ok()?;

        Some(Self {
            project_path: ProjectPath {
                worktree_id: notebook_path.worktree_id,
                path: path.as_rel_path().into_arc(),
            },
        })
    }
}

/// Where a cell lives inside the shared buffer.
struct CellAnchors {
    /// The cell's own source. Biased so that typing at either edge stays inside the
    /// cell rather than leaking into the marker or the separator.
    source: Range<Anchor>,
    /// Everything belonging to the cell: marker line, source and trailing separator.
    /// Removing this range removes the cell.
    region: Range<Anchor>,
}

pub struct ShadowBuffer {
    buffer: Entity<Buffer>,
    cells: HashMap<CellId, CellAnchors>,
    /// Cell order as the buffer lays it out, which is what decides where a new cell's
    /// text is inserted.
    order: Vec<CellId>,
    comment_prefix: String,
}

impl ShadowBuffer {
    /// Derives the sidecar path for a notebook. See [`SidecarPath`].
    pub(super) fn sidecar_path(
        notebook_path: &ProjectPath,
        extension: &str,
    ) -> Option<SidecarPath> {
        SidecarPath::for_notebook(notebook_path, extension)
    }

    /// Projects `cells` into a buffer registered with `project`.
    ///
    /// The buffer is not backed by a file. A language server that requires a path will
    /// not attach to it; giving the projection a real path is a separate question from
    /// whether the projection itself is correct, and is tracked in the roadmap.
    pub fn new(
        project: &Entity<Project>,
        cells: &[Cell],
        language: Option<Arc<Language>>,
        comment_prefix: &str,
        cx: &mut App,
    ) -> Self {
        let document = ShadowDocument::from_cells(cells, comment_prefix);

        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(document.text(), language, true, cx)
        });

        let (anchors, order) = buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let mut anchors = HashMap::default();
            let mut order = Vec::with_capacity(document.cells().len());

            for cell in document.cells() {
                order.push(cell.id.clone());
                anchors.insert(
                    cell.id.clone(),
                    CellAnchors {
                        source: snapshot.anchor_before(cell.source_range.start)
                            ..snapshot.anchor_after(cell.source_range.end),
                        region: snapshot.anchor_before(cell.region_range.start)
                            ..snapshot.anchor_after(cell.region_range.end),
                    },
                );
            }

            (anchors, order)
        });

        Self {
            buffer,
            cells: anchors,
            order,
            comment_prefix: comment_prefix.to_string(),
        }
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    /// The point range a cell editor should excerpt.
    pub fn source_range(&self, cell_id: &CellId, cx: &App) -> Option<Range<Point>> {
        let anchors = self.cells.get(cell_id)?;
        let snapshot = self.buffer.read(cx).snapshot();
        Some(anchors.source.start.to_point(&snapshot)..anchors.source.end.to_point(&snapshot))
    }

    /// A cell's current text, read back out of the shared buffer.
    pub fn source_for(&self, cell_id: &CellId, cx: &App) -> Option<String> {
        let anchors = self.cells.get(cell_id)?;
        let snapshot = self.buffer.read(cx).snapshot();
        let range = anchors.source.start.to_offset(&snapshot)..anchors.source.end.to_offset(&snapshot);
        Some(snapshot.text_for_range(range).collect())
    }

    /// Adds a code cell after `after`, or at the top when `after` is `None`.
    ///
    /// Only code cells are projected into the buffer. Markdown and raw cells appear in
    /// [`ShadowDocument`] in commented form for jupytext fidelity, but they keep their
    /// own buffers in the editor, so adding one here would create a range nothing
    /// excerpts.
    pub fn insert_code_cell(
        &mut self,
        cell_id: CellId,
        after: Option<&CellId>,
        cx: &mut App,
    ) {
        self.insert_code_cell_with_source(cell_id, after, "", cx);
    }

    /// Inserts a code cell carrying `source`, used both for a new empty cell and for
    /// the re-insertion half of a move.
    pub(super) fn insert_code_cell_with_source(
        &mut self,
        cell_id: CellId,
        after: Option<&CellId>,
        source: &str,
        cx: &mut App,
    ) {
        let marker = format!("{} %%", self.comment_prefix);
        let snapshot = self.buffer.read(cx).snapshot();

        let (offset, insert_index) = match after.and_then(|id| {
            self.cells
                .get(id)
                .map(|anchors| anchors.region.end.to_offset(&snapshot))
                .zip(self.order.iter().position(|existing| existing == id))
        }) {
            Some((offset, index)) => (offset, index + 1),
            None => (0, 0),
        };

        // A cell that is not the last one in the buffer needs the blank separator
        // after it; the last one does not, so that the buffer has no trailing blank.
        let is_last = insert_index == self.order.len();
        let leading = if is_last && offset > 0 { "\n" } else { "" };
        let trailing = if is_last { "\n" } else { "\n\n\n" };
        let text = format!("{leading}{marker}\n{source}{trailing}");

        let source_start = offset + leading.len() + marker.len() + 1;
        let source_end = source_start + source.len();

        self.buffer.update(cx, |buffer, cx| {
            buffer.edit([(offset..offset, text.clone())], None, cx);
        });

        let snapshot = self.buffer.read(cx).snapshot();
        self.cells.insert(
            cell_id.clone(),
            CellAnchors {
                source: snapshot.anchor_before(source_start)
                    ..snapshot.anchor_after(source_end),
                region: snapshot.anchor_before(offset)
                    ..snapshot.anchor_after(offset + text.len()),
            },
        );
        self.order.insert(insert_index, cell_id);
    }

    /// Moves a cell to sit after `after`, or to the top when `after` is `None`.
    ///
    /// Implemented as a remove and re-insert of the cell's text, so only the moved
    /// cell's anchors are invalidated; every other cell keeps its editor and its
    /// position. Returns the moved cell's new source range, which its editor has to
    /// re-excerpt.
    pub(super) fn move_cell(
        &mut self,
        cell_id: &CellId,
        after: Option<&CellId>,
        cx: &mut App,
    ) -> Option<Range<Point>> {
        let source = self.source_for(cell_id, cx)?;
        self.remove_cell(cell_id, cx);
        self.insert_code_cell_with_source(cell_id.clone(), after, &source, cx);
        self.source_range(cell_id, cx)
    }

    /// Removes a cell's text from the buffer. A cell with no projected text (markdown,
    /// raw) is simply forgotten.
    pub fn remove_cell(&mut self, cell_id: &CellId, cx: &mut App) {
        self.order.retain(|existing| existing != cell_id);

        let Some(anchors) = self.cells.remove(cell_id) else {
            return;
        };

        let snapshot = self.buffer.read(cx).snapshot();
        let range = anchors.region.start.to_offset(&snapshot)..anchors.region.end.to_offset(&snapshot);
        if range.is_empty() {
            return;
        }

        self.buffer.update(cx, |buffer, cx| {
            buffer.edit([(range, "")], None, cx);
        });
    }

    /// Rebuilds the whole projection from `cells`.
    ///
    /// Used when the cell list changes in a way that is not a single insertion or
    /// removal, such as reordering. Anchors are invalidated, so every cell editor has
    /// to be re-excerpted afterwards.
    pub fn rebuild(&mut self, cells: &[Cell], cx: &mut App) {
        let document = ShadowDocument::from_cells(cells, &self.comment_prefix);

        self.buffer.update(cx, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(0..end, document.text().to_string())], None, cx);
        });

        let snapshot = self.buffer.read(cx).snapshot();
        self.cells.clear();
        self.order.clear();

        for cell in document.cells() {
            self.order.push(cell.id.clone());
            self.cells.insert(
                cell.id.clone(),
                CellAnchors {
                    source: snapshot.anchor_before(cell.source_range.start)
                        ..snapshot.anchor_after(cell.source_range.end),
                    region: snapshot.anchor_before(cell.region_range.start)
                        ..snapshot.anchor_after(cell.region_range.end),
                },
            );
        }
    }

    /// How a cell's editor should be built.
    ///
    /// Code cells are excerpted out of the shared buffer. Markdown and raw cells are
    /// not: they exist in the projection only in commented form, so they keep their
    /// own buffers and their own language.
    pub(super) fn backing_for(
        &self,
        cell_id: &CellId,
        project: &Entity<Project>,
        cx: &App,
    ) -> CellBacking {
        match self.source_range(cell_id, cx) {
            Some(range) => CellBacking::Shared {
                buffer: self.buffer.clone(),
                range,
                project: project.clone(),
            },
            None => CellBacking::Local,
        }
    }

    /// Applies the notebook's language to the shared buffer. Every code cell picks it
    /// up, since they are all excerpts over this one buffer.
    pub fn set_language(&self, language: Option<Arc<Language>>, cx: &mut App) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.set_language(language, cx);
        });
    }

    /// Replaces the in-memory buffer with `buffer`, which is backed by a real file and
    /// therefore visible to language servers.
    ///
    /// Every cell's anchors are recomputed against the new buffer, so callers must
    /// re-excerpt each cell editor afterwards with the ranges from `source_range`.
    pub(super) fn adopt_buffer(&mut self, buffer: Entity<Buffer>, cells: &[Cell], cx: &mut App) {
        let document = ShadowDocument::from_cells(cells, &self.comment_prefix);

        buffer.update(cx, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(0..end, document.text().to_string())], None, cx);
        });

        self.buffer = buffer;
        self.cells.clear();
        self.order.clear();

        let snapshot = self.buffer.read(cx).snapshot();
        for cell in document.cells() {
            self.order.push(cell.id.clone());
            self.cells.insert(
                cell.id.clone(),
                CellAnchors {
                    source: snapshot.anchor_before(cell.source_range.start)
                        ..snapshot.anchor_after(cell.source_range.end),
                    region: snapshot.anchor_before(cell.region_range.start)
                        ..snapshot.anchor_after(cell.region_range.end),
                },
            );
        }
    }

    /// Whether the shared buffer is backed by a file, and therefore whether language
    /// servers can see the notebook's code at all.
    pub(super) fn is_file_backed(&self, cx: &App) -> bool {
        self.buffer.read(cx).file().is_some()
    }

    /// Clears the shared buffer's dirty state after the notebook has been written.
    pub(super) fn mark_saved(&self, cx: &mut App) {
        self.buffer.update(cx, |buffer, cx| {
            let version = buffer.version();
            buffer.did_save(version, None, cx);
        });
    }

    /// The cells as the buffer currently holds them, for writing back to the notebook.
    ///
    /// Cells are matched positionally against `parse`, so this is only meaningful when
    /// the buffer's cell order matches the notebook's.
    /// The cells as the document currently parses, for reconciling the cell list after
    /// the source view has been edited.
    pub(super) fn parse_cells(&self, cx: &App) -> Vec<super::ParsedCell> {
        let text = self.buffer.read(cx).text();
        ShadowDocument::parse(&text, &self.comment_prefix)
    }

    pub fn to_cells(&self, cx: &App) -> Vec<(CellType, String)> {
        let text = self.buffer.read(cx).text();
        ShadowDocument::parse(&text, &self.comment_prefix)
            .into_iter()
            .map(|cell| (cell.cell_type, cell.source))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;

    fn code(id: &str, source: &[&str]) -> Cell {
        Cell::Code {
            id: CellId::new(id).expect("test cell id should be valid"),
            metadata: serde_json::from_str("{}").expect("empty metadata should parse"),
            execution_count: None,
            source: source.iter().map(|line| line.to_string()).collect(),
            outputs: Vec::new(),
        }
    }

    fn cell_id(id: &str) -> CellId {
        CellId::new(id).expect("test cell id should be valid")
    }

    async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        Project::test(fs, [] as [&std::path::Path; 0], cx).await
    }

    #[gpui::test]
    async fn test_tracks_cell_sources(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let cells = [
            code("one", &["import x\n", "\n", "x.load()"]),
            code("two", &["x.plot()"]),
        ];

        let shadow = cx.update(|cx| ShadowBuffer::new(&project, &cells, None, "#", cx));

        cx.update(|cx| {
            assert_eq!(
                shadow.source_for(&cell_id("one"), cx).as_deref(),
                Some("import x\n\nx.load()")
            );
            assert_eq!(
                shadow.source_for(&cell_id("two"), cx).as_deref(),
                Some("x.plot()")
            );
        });
    }

    /// The reason for anchoring rather than storing offsets: editing one cell must not
    /// break every cell after it.
    #[gpui::test]
    async fn test_editing_one_cell_keeps_later_cells_addressable(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let cells = [code("one", &["a = 1"]), code("two", &["b = 2"])];

        let shadow = cx.update(|cx| ShadowBuffer::new(&project, &cells, None, "#", cx));

        let range = cx
            .update(|cx| shadow.source_range(&cell_id("one"), cx))
            .expect("first cell should have a range");

        shadow.buffer().update(cx, |buffer, cx| {
            buffer.edit(
                [(range, "a = 1\nimport a_much_longer_name_here")],
                None,
                cx,
            );
        });

        cx.update(|cx| {
            assert_eq!(
                shadow.source_for(&cell_id("one"), cx).as_deref(),
                Some("a = 1\nimport a_much_longer_name_here"),
                "the edited cell should report its new text"
            );
            assert_eq!(
                shadow.source_for(&cell_id("two"), cx).as_deref(),
                Some("b = 2"),
                "the following cell should still resolve after the edit shifted it"
            );
        });
    }

    #[gpui::test]
    async fn test_inserting_a_cell_keeps_the_document_parseable(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let cells = [code("one", &["a = 1"]), code("two", &["b = 2"])];

        let mut shadow = cx.update(|cx| ShadowBuffer::new(&project, &cells, None, "#", cx));

        cx.update(|cx| {
            shadow.insert_code_cell(cell_id("inserted"), Some(&cell_id("one")), cx);
        });

        let sources = cx.update(|cx| {
            (
                shadow.source_for(&cell_id("one"), cx),
                shadow.source_for(&cell_id("inserted"), cx),
                shadow.source_for(&cell_id("two"), cx),
            )
        });

        assert_eq!(sources.0.as_deref(), Some("a = 1"));
        assert_eq!(sources.1.as_deref(), Some(""), "a new cell starts empty");
        assert_eq!(sources.2.as_deref(), Some("b = 2"));

        // An empty cell still exists. Dropping it here would delete it from the
        // notebook every time the source view round-trips.
        let parsed = cx.update(|cx| shadow.to_cells(cx));
        assert_eq!(
            parsed.len(),
            3,
            "the inserted empty cell must survive parsing: {parsed:?}"
        );
        assert_eq!(parsed[1].1, "", "and it is still empty");
    }

    /// Typing into a freshly inserted cell has to land in that cell, not in its
    /// neighbours. This is the case the anchor biases exist for.
    #[gpui::test]
    async fn test_typing_into_an_inserted_cell_stays_in_it(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let cells = [code("one", &["a = 1"]), code("two", &["b = 2"])];

        let mut shadow = cx.update(|cx| ShadowBuffer::new(&project, &cells, None, "#", cx));
        cx.update(|cx| {
            shadow.insert_code_cell(cell_id("inserted"), Some(&cell_id("one")), cx);
        });

        let range = cx
            .update(|cx| shadow.source_range(&cell_id("inserted"), cx))
            .expect("inserted cell should have a range");
        shadow.buffer().update(cx, |buffer, cx| {
            buffer.edit([(range, "c = 3")], None, cx);
        });

        cx.update(|cx| {
            assert_eq!(shadow.source_for(&cell_id("one"), cx).as_deref(), Some("a = 1"));
            assert_eq!(
                shadow.source_for(&cell_id("inserted"), cx).as_deref(),
                Some("c = 3")
            );
            assert_eq!(shadow.source_for(&cell_id("two"), cx).as_deref(), Some("b = 2"));
        });

        let parsed = cx.update(|cx| shadow.to_cells(cx));
        let sources: Vec<String> = parsed.into_iter().map(|(_, source)| source).collect();
        assert_eq!(
            sources,
            vec!["a = 1".to_string(), "c = 3".to_string(), "b = 2".to_string()],
            "the inserted cell should appear between its neighbours"
        );
    }

    #[gpui::test]
    async fn test_removing_a_cell_leaves_the_others_intact(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let cells = [
            code("one", &["a = 1"]),
            code("two", &["b = 2"]),
            code("three", &["c = 3"]),
        ];

        let mut shadow = cx.update(|cx| ShadowBuffer::new(&project, &cells, None, "#", cx));
        cx.update(|cx| {
            shadow.remove_cell(&cell_id("two"), cx);
        });

        cx.update(|cx| {
            assert_eq!(shadow.source_for(&cell_id("one"), cx).as_deref(), Some("a = 1"));
            assert_eq!(shadow.source_for(&cell_id("two"), cx), None);
            assert_eq!(
                shadow.source_for(&cell_id("three"), cx).as_deref(),
                Some("c = 3")
            );
        });

        let sources: Vec<String> = cx
            .update(|cx| shadow.to_cells(cx))
            .into_iter()
            .map(|(_, source)| source)
            .collect();
        assert_eq!(sources, vec!["a = 1".to_string(), "c = 3".to_string()]);
    }

    #[gpui::test]
    async fn test_rebuild_reflows_after_reordering(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let cells = [code("one", &["a = 1"]), code("two", &["b = 2"])];

        let mut shadow = cx.update(|cx| ShadowBuffer::new(&project, &cells, None, "#", cx));

        let reordered = [code("two", &["b = 2"]), code("one", &["a = 1"])];
        cx.update(|cx| {
            shadow.rebuild(&reordered, cx);
        });

        let sources: Vec<String> = cx
            .update(|cx| shadow.to_cells(cx))
            .into_iter()
            .map(|(_, source)| source)
            .collect();
        assert_eq!(sources, vec!["b = 2".to_string(), "a = 1".to_string()]);

        cx.update(|cx| {
            assert_eq!(shadow.source_for(&cell_id("one"), cx).as_deref(), Some("a = 1"));
            assert_eq!(shadow.source_for(&cell_id("two"), cx).as_deref(), Some("b = 2"));
        });
    }

    /// Moving a cell must reorder the text and leave every other cell's anchors valid,
    /// so only the moved cell's editor needs re-excerpting.
    #[gpui::test]
    async fn test_moving_a_cell_reorders_text_and_keeps_others_valid(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let cells = [
            code("one", &["a = 1"]),
            code("two", &["b = 2"]),
            code("three", &["c = 3"]),
        ];

        let mut shadow = cx.update(|cx| ShadowBuffer::new(&project, &cells, None, "#", cx));

        // Move the last cell to the top.
        let new_range = cx.update(|cx| shadow.move_cell(&cell_id("three"), None, cx));
        assert!(new_range.is_some(), "a moved cell should report a new range");

        let sources: Vec<String> = cx
            .update(|cx| shadow.to_cells(cx))
            .into_iter()
            .map(|(_, source)| source)
            .collect();
        assert_eq!(
            sources,
            vec!["c = 3".to_string(), "a = 1".to_string(), "b = 2".to_string()]
        );

        cx.update(|cx| {
            for (id, expected) in [("one", "a = 1"), ("two", "b = 2"), ("three", "c = 3")] {
                assert_eq!(
                    shadow.source_for(&cell_id(id), cx).as_deref(),
                    Some(expected),
                    "{id} should still resolve after the move"
                );
            }
        });
    }

    /// A move in the middle of the buffer, which exercises the separator handling that
    /// a move to either end does not.
    #[gpui::test]
    async fn test_moving_a_cell_into_the_middle(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let cells = [
            code("one", &["a = 1"]),
            code("two", &["b = 2"]),
            code("three", &["c = 3"]),
        ];

        let mut shadow = cx.update(|cx| ShadowBuffer::new(&project, &cells, None, "#", cx));
        cx.update(|cx| shadow.move_cell(&cell_id("one"), Some(&cell_id("two")), cx));

        let sources: Vec<String> = cx
            .update(|cx| shadow.to_cells(cx))
            .into_iter()
            .map(|(_, source)| source)
            .collect();
        assert_eq!(
            sources,
            vec!["b = 2".to_string(), "a = 1".to_string(), "c = 3".to_string()]
        );
    }

    #[test]
    fn test_sidecar_path_sits_beside_the_notebook() {
        let notebook = ProjectPath {
            worktree_id: project::WorktreeId::from_usize(0),
            path: RelPath::new_test("analysis/notes.ipynb").into_arc(),
        };

        let sidecar = SidecarPath::for_notebook(&notebook, "py").expect("should derive a path");

        assert_eq!(
            sidecar.project_path.path.as_unix_str(),
            "analysis/.notes.lathe.py",
            "the sidecar belongs next to the notebook so relative imports resolve"
        );
        assert_eq!(sidecar.project_path.worktree_id, notebook.worktree_id);
    }

    #[test]
    fn test_sidecar_path_at_the_worktree_root() {
        let notebook = ProjectPath {
            worktree_id: project::WorktreeId::from_usize(0),
            path: RelPath::new_test("notes.ipynb").into_arc(),
        };

        let sidecar = SidecarPath::for_notebook(&notebook, "py").expect("should derive a path");
        assert_eq!(sidecar.project_path.path.as_unix_str(), ".notes.lathe.py");
    }
}
