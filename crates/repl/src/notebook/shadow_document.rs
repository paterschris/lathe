//! The notebook projected onto a single source document.
//!
//! A notebook's cells are separate buffers today, which is why cell editors have no
//! language intelligence: a language server given one cell at a time cannot resolve a
//! name defined in an earlier cell, and a detached `Buffer::local` is not attached to
//! a project at all.
//!
//! This module builds the other representation: the whole notebook as one piece of
//! text in the notebook's own language, using the `# %%` cell markers that
//! `repl_editor::jupytext_cells` already understands. It is the same shape jupytext
//! writes as a paired `.py` file, and the same shape a language server wants, so the
//! two goals are served by one artifact.
//!
//! Cells map onto ranges within that text, which is what lets a cell editor become an
//! excerpt over one shared, project-registered buffer instead of an island.
//!
//! Cell ids are deliberately not written into the document. Standard jupytext files
//! do not carry them, and emitting them would make the projection incompatible with
//! every other tool that reads this format. Cells are therefore matched positionally
//! when parsing edits back, exactly as jupytext does; the `.ipynb` remains the
//! authority for ids, metadata and outputs.

use std::ops::Range;

use nbformat::v4::{Cell, CellId, CellType};

const MARKER: &str = "%%";

/// A cell's placement within a [`ShadowDocument`].
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowCell {
    pub id: CellId,
    pub cell_type: CellType,
    /// Byte range of the cell's own source within the document text, excluding the
    /// marker line.
    ///
    /// For a code cell this is the source verbatim, so an editor showing this range
    /// shows exactly what the cell contains. For markdown and raw cells the range
    /// covers the commented form, since their text is not valid in the notebook's
    /// language; those cells keep their own buffers.
    pub source_range: Range<usize>,
    /// Byte range of everything belonging to this cell: the marker line, the source,
    /// and the blank line separating it from the next cell.
    ///
    /// Removing this range removes the cell from the document and leaves the
    /// remaining cells well formed.
    pub region_range: Range<usize>,
}

/// A notebook rendered as one percent-format source document.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowDocument {
    text: String,
    cells: Vec<ShadowCell>,
    comment_prefix: String,
}

/// A cell recovered from percent-format text.
///
/// Carries no id: the document does not store one, so callers match these against the
/// notebook's existing cells by position.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCell {
    pub cell_type: CellType,
    pub source: String,
}

impl ShadowDocument {
    /// Projects `cells` onto a single document, commenting non-code cells with
    /// `comment_prefix` (`#` for Python) so the result stays valid in the notebook's
    /// language.
    pub fn from_cells(cells: &[Cell], comment_prefix: &str) -> Self {
        let mut text = String::new();
        let mut shadow_cells = Vec::with_capacity(cells.len());

        for cell in cells {
            let region_start = text.len();
            let (cell_type, id, source) = match cell {
                Cell::Code { id, source, .. } => (CellType::Code, id.clone(), source.concat()),
                Cell::Markdown { id, source, .. } => {
                    (CellType::Markdown, id.clone(), source.concat())
                }
                Cell::Raw { id, source, .. } => (CellType::Raw, id.clone(), source.concat()),
            };

            text.push_str(&marker_line(&cell_type, comment_prefix));
            text.push('\n');

            let start = text.len();
            match cell_type {
                CellType::Code => text.push_str(&source),
                CellType::Markdown | CellType::Raw => {
                    text.push_str(&comment_out(&source, comment_prefix))
                }
            }
            let end = text.len();

            // Separate cells with a blank line so the markers stay readable and a
            // cell without a trailing newline does not run into the next marker.
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');

            shadow_cells.push(ShadowCell {
                id,
                cell_type,
                source_range: start..end,
                region_range: region_start..text.len(),
            });
        }

        // The trailing blank line after the final cell is separator, not content.
        if text.ends_with("\n\n") {
            text.pop();
            if let Some(last) = shadow_cells.last_mut() {
                last.region_range.end = text.len();
            }
        }

        Self {
            text,
            cells: shadow_cells,
            comment_prefix: comment_prefix.to_string(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cells(&self) -> &[ShadowCell] {
        &self.cells
    }

    /// The cells a language server can usefully see, with the ranges an excerpt-backed
    /// editor would cover.
    ///
    /// Markdown and raw cells are excluded: their text lives in the document only in
    /// commented form.
    pub fn code_cell_ranges(&self) -> impl Iterator<Item = (&CellId, Range<usize>)> {
        self.cells
            .iter()
            .filter(|cell| cell.cell_type == CellType::Code)
            .map(|cell| (&cell.id, cell.source_range.clone()))
    }

    /// The source of `cell` as it appears in the document.
    pub fn source_for(&self, cell: &ShadowCell) -> &str {
        self.text
            .get(cell.source_range.clone())
            .unwrap_or_default()
    }

    /// Recovers cells from percent-format text.
    ///
    /// Text before the first marker is treated as a leading code cell, matching how
    /// `repl_editor::jupytext_cells` treats a file that starts with code.
    pub fn parse(text: &str, comment_prefix: &str) -> Vec<ParsedCell> {
        let mut cells = Vec::new();
        let mut current_type = CellType::Code;
        let mut current_lines: Vec<&str> = Vec::new();
        // Whether a marker has opened a cell. A cell with no text still exists once its
        // marker has been seen; dropping it here silently deletes empty cells from the
        // notebook every time the source view round-trips.
        let mut cell_is_open = false;

        let flush = |cell_type: &CellType,
                     lines: &mut Vec<&str>,
                     cells: &mut Vec<ParsedCell>,
                     keep_when_empty: bool| {
            // A cell is separated from the next marker by a blank line that belongs to
            // the projection, not to the cell.
            while lines.last().is_some_and(|line| line.trim().is_empty()) {
                lines.pop();
            }
            if lines.is_empty() && !keep_when_empty {
                lines.clear();
                return;
            }
            let joined = lines.join("\n");
            let source = match cell_type {
                CellType::Code => joined,
                CellType::Markdown | CellType::Raw => uncomment(&joined, comment_prefix),
            };
            cells.push(ParsedCell {
                cell_type: cell_type.clone(),
                source,
            });
            lines.clear();
        };

        for line in text.lines() {
            if let Some(marker_type) = parse_marker(line, comment_prefix) {
                flush(&current_type, &mut current_lines, &mut cells, cell_is_open);
                current_type = marker_type;
                cell_is_open = true;
                continue;
            }
            current_lines.push(line);
        }
        flush(&current_type, &mut current_lines, &mut cells, cell_is_open);

        cells
    }
}

fn marker_line(cell_type: &CellType, comment_prefix: &str) -> String {
    match cell_type {
        CellType::Code => format!("{comment_prefix} {MARKER}"),
        CellType::Markdown => format!("{comment_prefix} {MARKER} [markdown]"),
        CellType::Raw => format!("{comment_prefix} {MARKER} [raw]"),
    }
}

/// Recognizes a cell marker line, returning the cell type it introduces.
///
/// Accepts the marker with or without a space after the comment prefix, since both
/// forms appear in files written by other tools.
fn parse_marker(line: &str, comment_prefix: &str) -> Option<CellType> {
    let rest = line.trim_start().strip_prefix(comment_prefix)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(MARKER)?;
    let rest = rest.trim();

    if rest.is_empty() {
        return Some(CellType::Code);
    }

    let tag = match rest.strip_prefix('[').and_then(|rest| rest.split(']').next()) {
        Some(tag) => tag.trim().to_ascii_lowercase(),
        // A marker carrying only cell metadata, such as `# %% tags=["skip"]`, still
        // starts a code cell. Returning here rather than falling through the match
        // keeps such a line from being swallowed into the preceding cell.
        None => return Some(CellType::Code),
    };

    match tag.as_str() {
        "markdown" | "md" => Some(CellType::Markdown),
        "raw" => Some(CellType::Raw),
        _ => Some(CellType::Code),
    }
}

fn comment_out(source: &str, comment_prefix: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let (content, terminator) = match line.strip_suffix('\n') {
            Some(content) => (content, "\n"),
            None => (line, ""),
        };

        if content.is_empty() {
            out.push_str(comment_prefix);
        } else {
            out.push_str(comment_prefix);
            out.push(' ');
            out.push_str(content);
        }
        out.push_str(terminator);
    }
    out
}

fn uncomment(source: &str, comment_prefix: &str) -> String {
    source
        .lines()
        .map(|line| {
            let Some(rest) = line.strip_prefix(comment_prefix) else {
                return line;
            };
            // Only the single separating space the projection added is removed, so
            // indentation inside the markdown survives.
            rest.strip_prefix(' ').unwrap_or(rest)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How a cell parsed out of the document lines up with a cell the notebook already has.
///
/// Editing the source view can add, remove, retype or reorder cells, and the document
/// carries no ids, so cells are matched by position. That is what jupytext does, and it
/// is the only thing available when the text is the only input.
#[derive(Debug, Clone, PartialEq)]
pub enum CellReconciliation {
    /// Same position and type: the existing cell is kept with its id, metadata and
    /// outputs, and only its source changes.
    Kept { index: usize, source: String },
    /// No counterpart, or the type changed, so a new cell is needed. A type change
    /// counts as a new cell because a markdown cell has no outputs to carry over and a
    /// code cell's outputs would be meaningless on prose.
    Added { cell_type: CellType, source: String },
}

/// Matches cells parsed from the document against the notebook's existing cells.
///
/// `existing` is the current cell types, in order. The result is one entry per cell in
/// the new document, in order.
pub fn reconcile_cells(existing: &[CellType], parsed: &[ParsedCell]) -> Vec<CellReconciliation> {
    parsed
        .iter()
        .enumerate()
        .map(|(index, cell)| match existing.get(index) {
            Some(existing_type) if *existing_type == cell.cell_type => {
                CellReconciliation::Kept {
                    index,
                    source: cell.source.clone(),
                }
            }
            _ => CellReconciliation::Added {
                cell_type: cell.cell_type.clone(),
                source: cell.source.clone(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(id: &str, source: &[&str]) -> Cell {
        Cell::Code {
            id: CellId::new(id).expect("test cell id should be valid"),
            metadata: serde_json::from_str("{}").expect("empty metadata should parse"),
            execution_count: None,
            source: source.iter().map(|line| line.to_string()).collect(),
            outputs: Vec::new(),
        }
    }

    fn markdown(id: &str, source: &[&str]) -> Cell {
        Cell::Markdown {
            id: CellId::new(id).expect("test cell id should be valid"),
            metadata: serde_json::from_str("{}").expect("empty metadata should parse"),
            source: source.iter().map(|line| line.to_string()).collect(),
            attachments: None,
        }
    }

    #[test]
    fn test_projects_cells_into_percent_format() {
        let cells = [
            code("one", &["import x\n", "\n", "x.load()"]),
            markdown("two", &["# Notes\n", "\n", "Some prose"]),
            code("three", &["x.plot()"]),
        ];

        let document = ShadowDocument::from_cells(&cells, "#");

        assert_eq!(
            document.text(),
            "# %%\n\
             import x\n\
             \n\
             x.load()\n\
             \n\
             # %% [markdown]\n\
             # # Notes\n\
             #\n\
             # Some prose\n\
             \n\
             # %%\n\
             x.plot()\n"
        );
    }

    /// The whole point of the projection: a code cell's range must contain exactly
    /// that cell's source, so an editor excerpting the range shows the cell.
    #[test]
    fn test_code_cell_ranges_cover_exactly_the_cell_source() {
        let cells = [
            code("one", &["import x\n", "\n", "x.load()"]),
            markdown("two", &["# Notes"]),
            code("three", &["x.plot()"]),
        ];

        let document = ShadowDocument::from_cells(&cells, "#");

        let sources: Vec<&str> = document
            .cells()
            .iter()
            .filter(|cell| cell.cell_type == CellType::Code)
            .map(|cell| document.source_for(cell))
            .collect();

        assert_eq!(sources, vec!["import x\n\nx.load()", "x.plot()"]);

        let ids: Vec<String> = document
            .code_cell_ranges()
            .map(|(id, _)| id.to_string())
            .collect();
        assert_eq!(ids, vec!["one".to_string(), "three".to_string()]);
    }

    /// Names defined in one cell have to be visible to the next one in the projected
    /// document, which is the reason for building it at all.
    #[test]
    fn test_code_cells_share_one_scope() {
        let cells = [
            code("one", &["import pandas as pd"]),
            code("two", &["pd.DataFrame()"]),
        ];

        let document = ShadowDocument::from_cells(&cells, "#");

        let import_position = document
            .text()
            .find("import pandas")
            .expect("import should be present");
        let use_position = document
            .text()
            .find("pd.DataFrame")
            .expect("use should be present");
        assert!(
            import_position < use_position,
            "the import must precede its use in the projected document:\n{}",
            document.text()
        );
    }

    #[test]
    fn test_round_trips_through_percent_format() {
        let cells = [
            code("one", &["import x\n", "\n", "x.load()"]),
            markdown("two", &["# Notes\n", "\n", "Some prose"]),
            code("three", &["x.plot()"]),
        ];

        let document = ShadowDocument::from_cells(&cells, "#");
        let parsed = ShadowDocument::parse(document.text(), "#");

        assert_eq!(
            parsed,
            vec![
                ParsedCell {
                    cell_type: CellType::Code,
                    source: "import x\n\nx.load()".to_string(),
                },
                ParsedCell {
                    cell_type: CellType::Markdown,
                    source: "# Notes\n\nSome prose".to_string(),
                },
                ParsedCell {
                    cell_type: CellType::Code,
                    source: "x.plot()".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_parses_markers_written_by_other_tools() {
        // jupytext writes markers with cell metadata, and some files omit the space
        // after the comment character.
        let text = "#%%\n\
                    a = 1\n\
                    \n\
                    # %% [markdown]\n\
                    # heading\n\
                    \n\
                    # %% tags=[\"skip\"]\n\
                    b = 2\n";

        let parsed = ShadowDocument::parse(text, "#");

        assert_eq!(
            parsed,
            vec![
                ParsedCell {
                    cell_type: CellType::Code,
                    source: "a = 1".to_string(),
                },
                ParsedCell {
                    cell_type: CellType::Markdown,
                    source: "heading".to_string(),
                },
                ParsedCell {
                    cell_type: CellType::Code,
                    source: "b = 2".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_parses_leading_code_without_a_marker() {
        let parsed = ShadowDocument::parse("a = 1\n\n# %%\nb = 2\n", "#");

        assert_eq!(
            parsed,
            vec![
                ParsedCell {
                    cell_type: CellType::Code,
                    source: "a = 1".to_string(),
                },
                ParsedCell {
                    cell_type: CellType::Code,
                    source: "b = 2".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_blank_markdown_lines_do_not_gain_trailing_whitespace() {
        let cells = [markdown("one", &["a\n", "\n", "b"])];

        let document = ShadowDocument::from_cells(&cells, "#");

        assert!(
            !document.text().contains("# \n"),
            "a blank markdown line should be `#`, not `# `:\n{:?}",
            document.text()
        );
        assert_eq!(
            ShadowDocument::parse(document.text(), "#")[0].source,
            "a\n\nb"
        );
    }

    /// Regions have to tile the document with no gap and no overlap, because
    /// inserting and removing cells works by editing exactly one region.
    #[test]
    fn test_regions_tile_the_document() {
        let cells = [
            code("one", &["import x\n", "\n", "x.load()"]),
            markdown("two", &["# Notes"]),
            code("three", &["x.plot()"]),
        ];

        let document = ShadowDocument::from_cells(&cells, "#");

        let mut expected_start = 0;
        for cell in document.cells() {
            assert_eq!(
                cell.region_range.start, expected_start,
                "region for {:?} should start where the previous one ended",
                cell.id
            );
            assert!(
                cell.region_range.start <= cell.source_range.start
                    && cell.source_range.end <= cell.region_range.end,
                "source must sit inside its region for {:?}",
                cell.id
            );
            expected_start = cell.region_range.end;
        }
        assert_eq!(
            expected_start,
            document.text().len(),
            "regions should cover the whole document"
        );
    }

    /// Removing a cell's region must leave text that still parses into the remaining
    /// cells, which is what makes incremental deletion safe.
    #[test]
    fn test_removing_a_region_leaves_a_well_formed_document() {
        let cells = [
            code("one", &["a = 1"]),
            markdown("two", &["# Notes"]),
            code("three", &["b = 2"]),
        ];

        let document = ShadowDocument::from_cells(&cells, "#");
        let middle = document.cells()[1].region_range.clone();

        let mut text = document.text().to_string();
        text.replace_range(middle, "");

        assert_eq!(
            ShadowDocument::parse(&text, "#"),
            vec![
                ParsedCell {
                    cell_type: CellType::Code,
                    source: "a = 1".to_string(),
                },
                ParsedCell {
                    cell_type: CellType::Code,
                    source: "b = 2".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_reconcile_keeps_cells_that_line_up() {
        let existing = [CellType::Code, CellType::Markdown, CellType::Code];
        let parsed = vec![
            ParsedCell { cell_type: CellType::Code, source: "a = 1".into() },
            ParsedCell { cell_type: CellType::Markdown, source: "# Notes".into() },
            ParsedCell { cell_type: CellType::Code, source: "b = 2".into() },
        ];

        let result = reconcile_cells(&existing, &parsed);

        assert!(
            result.iter().all(|entry| matches!(entry, CellReconciliation::Kept { .. })),
            "unchanged structure should keep every cell, and with it their outputs: {result:?}"
        );
    }

    #[test]
    fn test_reconcile_treats_a_type_change_as_a_new_cell() {
        let existing = [CellType::Code];
        let parsed = vec![ParsedCell {
            cell_type: CellType::Markdown,
            source: "# Notes".into(),
        }];

        assert_eq!(
            reconcile_cells(&existing, &parsed),
            vec![CellReconciliation::Added {
                cell_type: CellType::Markdown,
                source: "# Notes".into(),
            }],
            "a code cell's outputs would be meaningless carried onto prose"
        );
    }

    #[test]
    fn test_reconcile_handles_added_and_removed_cells() {
        let existing = [CellType::Code, CellType::Code];

        let added = vec![
            ParsedCell { cell_type: CellType::Code, source: "a = 1".into() },
            ParsedCell { cell_type: CellType::Code, source: "b = 2".into() },
            ParsedCell { cell_type: CellType::Code, source: "c = 3".into() },
        ];
        let result = reconcile_cells(&existing, &added);
        assert_eq!(result.len(), 3);
        assert!(matches!(result[2], CellReconciliation::Added { .. }));

        let removed = vec![ParsedCell {
            cell_type: CellType::Code,
            source: "a = 1".into(),
        }];
        let result = reconcile_cells(&existing, &removed);
        assert_eq!(result.len(), 1, "the notebook shrinks to what the document holds");
        assert!(matches!(result[0], CellReconciliation::Kept { index: 0, .. }));
    }

    /// A cell with no content is still a cell. Dropping it on parse deleted empty cells
    /// from the notebook every time the source view round-tripped, which is how it was
    /// found: toggling the view took a 26 cell notebook to 25.
    #[test]
    fn test_empty_cells_survive_a_round_trip() {
        let cells = [
            code("one", &["a = 1"]),
            code("blank", &[""]),
            code("three", &["c = 3"]),
        ];

        let document = ShadowDocument::from_cells(&cells, "#");
        let parsed = ShadowDocument::parse(document.text(), "#");

        assert_eq!(parsed.len(), 3, "no cell should be lost: {parsed:?}");
        assert_eq!(parsed[1].source, "", "the middle cell is still empty");
    }

    #[test]
    fn test_a_trailing_empty_cell_survives() {
        let cells = [code("one", &["a = 1"]), code("last", &[""])];

        let document = ShadowDocument::from_cells(&cells, "#");
        let parsed = ShadowDocument::parse(document.text(), "#");

        assert_eq!(parsed.len(), 2, "a notebook ending in a blank cell keeps it");
        assert_eq!(parsed[1].source, "");
    }

    #[test]
    fn test_empty_notebook_projects_to_empty_text() {
        let document = ShadowDocument::from_cells(&[], "#");

        assert_eq!(document.text(), "");
        assert!(document.cells().is_empty());
        assert!(ShadowDocument::parse("", "#").is_empty());
    }
}

/// Spike: confirms the projection can back cell editors as excerpts over one shared,
/// project-registered buffer.
///
/// This is the mechanism the notebook editor needs in order to get language
/// intelligence. Today each cell builds a detached `Buffer::local` with no project
/// attached, which is why there are no completions or diagnostics in a cell. These
/// tests check the two properties that make the excerpt approach viable: an editor
/// over a single excerpt shows exactly one cell, and edits made in that editor land in
/// the shared buffer the language server sees.
#[cfg(test)]
mod excerpt_spike {
    use super::*;

    use editor::{Editor, EditorMode, MultiBuffer, PathKey, SizingBehavior};
    use gpui::{AppContext as _, TestAppContext};
    use multi_buffer::{ExcerptRange, MultiBufferPoint};
    use nbformat::v4::Cell;
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

    async fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    #[gpui::test]
    async fn test_cell_editor_over_an_excerpt_shows_one_cell(cx: &mut TestAppContext) {
        init(cx).await;

        let document = ShadowDocument::from_cells(
            &[
                code("one", &["import pandas as pd\n", "\n", "frame = pd.DataFrame()"]),
                code("two", &["frame.head()"]),
            ],
            "#",
        );

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [] as [&std::path::Path; 0], cx).await;

        // The shared document the language server would see, registered with the
        // project rather than detached like today's per-cell buffers.
        let shadow_buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(document.text(), None, true, cx)
        });

        let second_cell = document
            .cells()
            .iter()
            .find(|cell| cell.id.to_string() == "two")
            .expect("second cell should exist")
            .clone();

        let (cell_editor, expected_source) = cx.update(|cx| {
            let snapshot = shadow_buffer.read(cx).snapshot();
            let range = snapshot.offset_to_point(second_cell.source_range.start)
                ..snapshot.offset_to_point(second_cell.source_range.end);

            let multi_buffer = cx.new(|cx| {
                let mut multi_buffer = MultiBuffer::new(language::Capability::ReadWrite);
                multi_buffer.set_excerpt_ranges_for_path(
                    PathKey::for_buffer(&shadow_buffer, cx),
                    shadow_buffer.clone(),
                    &snapshot,
                    vec![ExcerptRange::new(range)],
                    cx,
                );
                multi_buffer
            });

            (multi_buffer, document.source_for(&second_cell).to_string())
        });

        let cx = cx.add_empty_window();
        let editor = cx.update(|window, cx| {
            cx.new(|cx| {
                Editor::new(
                    EditorMode::Full {
                        scale_ui_elements_with_buffer_font_size: false,
                        show_active_line_background: false,
                        sizing_behavior: SizingBehavior::SizeByContent,
                    },
                    cell_editor,
                    // The project the detached per-cell buffers never had.
                    Some(project.clone()),
                    window,
                    cx,
                )
            })
        });

        let shown = cx.update(|_window, cx| editor.read(cx).text(cx));
        assert_eq!(
            shown, expected_source,
            "an editor over one excerpt should show exactly that cell"
        );
        assert!(
            !shown.contains("import pandas"),
            "it must not show the neighbouring cell: {shown:?}"
        );
    }

    #[gpui::test]
    async fn test_editing_a_cell_reaches_the_shared_buffer(cx: &mut TestAppContext) {
        init(cx).await;

        let document = ShadowDocument::from_cells(
            &[
                code("one", &["import pandas as pd"]),
                code("two", &["frame.head()"]),
            ],
            "#",
        );

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [] as [&std::path::Path; 0], cx).await;
        let shadow_buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(document.text(), None, true, cx)
        });

        let second_cell = document
            .cells()
            .iter()
            .find(|cell| cell.id.to_string() == "two")
            .expect("second cell should exist")
            .clone();

        let multi_buffer = cx.update(|cx| {
            let snapshot = shadow_buffer.read(cx).snapshot();
            let range = snapshot.offset_to_point(second_cell.source_range.start)
                ..snapshot.offset_to_point(second_cell.source_range.end);
            cx.new(|cx| {
                let mut multi_buffer = MultiBuffer::new(language::Capability::ReadWrite);
                multi_buffer.set_excerpt_ranges_for_path(
                    PathKey::for_buffer(&shadow_buffer, cx),
                    shadow_buffer.clone(),
                    &snapshot,
                    vec![ExcerptRange::new(range)],
                    cx,
                );
                multi_buffer
            })
        });

        let cx = cx.add_empty_window();
        let editor = cx.update(|window, cx| {
            cx.new(|cx| {
                Editor::new(
                    EditorMode::Full {
                        scale_ui_elements_with_buffer_font_size: false,
                        show_active_line_background: false,
                        sizing_behavior: SizingBehavior::SizeByContent,
                    },
                    multi_buffer,
                    Some(project.clone()),
                    window,
                    cx,
                )
            })
        });

        // `Editor::set_text` asserts a singleton buffer, so an excerpt-backed cell
        // editor has to go through the multibuffer edit path. This is the path real
        // typing takes, and the property under test is that it reaches the shared
        // buffer at all.
        cx.update(|_window, cx| {
            editor.update(cx, |editor, cx| {
                let end = editor.buffer().read(cx).read(cx).max_point();
                editor.buffer().update(cx, |buffer, cx| {
                    buffer.edit(
                        [(MultiBufferPoint::zero()..end, "frame.describe()")],
                        None,
                        cx,
                    );
                });
            })
        });

        let shared_text = cx.update(|_window, cx| shadow_buffer.read(cx).text());
        assert!(
            shared_text.contains("frame.describe()"),
            "the edit should land in the shared buffer a language server reads:\n{shared_text}"
        );
        assert!(
            shared_text.contains("import pandas as pd"),
            "the other cell must be untouched:\n{shared_text}"
        );
    }
}
