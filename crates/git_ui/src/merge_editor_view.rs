use editor::{
    Editor, EditorEvent, HighlightKey, RowHighlightOptions,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use encoding_rs::Encoding;
use git::repository::{ConflictSideNames, ConflictStages};
use gpui::{
    AppContext as _, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Hsla, SharedString,
    Subscription, Task, WeakEntity, point,
};
use language::{
    Anchor, Bias, Buffer, BufferSnapshot, Language, LineEnding, Point, ToOffset as _, ToPoint as _,
};
use project::{ConflictRegion, ConflictSet, ConflictSetUpdate, Project, ProjectItem as _};
use std::{ops::Range, sync::Arc, time::Duration};
use ui::{Checkbox, ContextMenu, Divider, PopoverMenu, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, SaveOptions},
};

/// Buffer edits arrive per keystroke, and each refresh re-diffs every side
/// pane against the whole file, so typing in the result pane is coalesced.
const SPLIT_REFRESH_DEBOUNCE: Duration = Duration::from_millis(50);

type LineHunks = Vec<(Range<u32>, Range<u32>)>;

/// Changed blocks with more lines than this on either side get row colors
/// only; diffing them word by word would mostly highlight everything.
const MAX_WORD_DIFF_ROWS: u32 = 8;
/// Past this many bytes, `text_diff` reports a block as one replacement
/// instead of word by word, which would paint the whole block as changed.
const MAX_WORD_DIFF_BYTES: usize = 512;

/// The index versions of the file, decoded for display.
#[derive(Debug, PartialEq)]
struct StageTexts {
    base: Option<String>,
    ours: Option<String>,
    theirs: Option<String>,
}

/// Workspace item that presents every conflict in a single buffer. Card mode
/// lists each conflict with per-region resolve buttons; split mode shows the
/// editable result between full-file "ours" and "theirs" panes (plus an
/// optional base pane), all scrolled in lockstep.
pub struct MergeEditorView {
    focus_handle: FocusHandle,
    buffer: Entity<Buffer>,
    conflict_set: Entity<ConflictSet>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    file_label: SharedString,
    /// Index of the conflict currently highlighted by Prev/Next navigation.
    /// Clamped on every render so resolving a conflict (which shrinks the
    /// list) doesn't leave this dangling past the end.
    selected_index: usize,
    view_mode: MergeViewMode,
    layout: SplitLayout,
    split: Option<SplitView>,
    /// Conflicts resolved with the side panes' checkboxes. Kept so a choice
    /// can be changed or undone after the markers are gone from the buffer.
    choices: Vec<ConflictChoice>,
    /// The file's base/ours/theirs versions from the git index. Until they
    /// load, or when loading fails (e.g. remote projects), the side panes are
    /// derived from the conflict markers instead.
    stages: Option<Arc<StageTexts>>,
    /// The index says this file's conflict is over the whole file (a deletion,
    /// or binary content), which has no markers for this view to resolve.
    whole_file_conflict: bool,
    /// Opened by `lathe --merge` on behalf of `git mergetool` (or another
    /// VCS), which drives the merge itself: the view offers "Done" to save
    /// and hand the file back, and never stages anything.
    merge_tool: bool,
    show_base: bool,
    /// Compare the side panes with the result ignoring whitespace, so changes
    /// that only reindent or reflow aren't highlighted.
    ignore_whitespace: bool,
    /// Guards against scroll-sync feedback: while we propagate one pane's
    /// scroll position to the others, their echoed ScrollPositionChanged
    /// events must not trigger another round of propagation.
    syncing_split_scroll: bool,
    _load_stages: Option<Task<()>>,
    /// Branch names for the ours and theirs panes, from the operation in
    /// progress.
    side_names: ConflictSideNames,
    /// The branch names from the conflict markers, kept after the markers are
    /// resolved away, for when the operation's names can't be read.
    marker_names: ConflictSideNames,
    _load_side_names: Option<Task<()>>,
    /// Each conflict's base lines, cut from the index's base version, for
    /// conflicts whose markers carry no base section. Keyed by the conflict's
    /// range so a stale entry can't be shown on the wrong card while a refresh
    /// is pending.
    card_bases: Vec<(Range<Anchor>, SharedString)>,
    _refresh_side_contents: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, PartialEq)]
enum MergeViewMode {
    Cards,
    Split,
}

/// Where split view puts the result relative to the two sides.
#[derive(Clone, Copy, PartialEq)]
enum SplitLayout {
    /// Ours and theirs side by side, with the result across the full width
    /// below them.
    ResultBelow,
    /// Ours, result, and theirs as three columns.
    ResultBetween,
}

/// A conflict resolved through the side panes' checkboxes.
#[derive(Clone, Debug)]
struct ConflictChoice {
    /// The text in the result that the choice produced.
    range: Range<Anchor>,
    /// The whole conflict, markers included, to restore when both sides are
    /// unchecked.
    original: String,
    ours: String,
    theirs: String,
    take_ours: bool,
    take_theirs: bool,
}

/// What a side pane checkbox applies to: a conflict still in the file, or one
/// already resolved by checkbox. Both are identified by their range in the
/// result, looked up again when the box is clicked.
#[derive(Clone)]
enum CheckboxTarget {
    Conflict(Range<Anchor>),
    Choice(Range<Anchor>),
}

/// The result's text for a conflict given which sides are checked, or `None`
/// for neither, meaning the conflict goes back to its markers.
fn choice_text(ours: &str, theirs: &str, take_ours: bool, take_theirs: bool) -> Option<String> {
    match (take_ours, take_theirs) {
        (false, false) => None,
        (true, false) => Some(ours.to_string()),
        (false, true) => Some(theirs.to_string()),
        (true, true) => Some(format!("{ours}{theirs}")),
    }
}

/// Anchors around text just written at `start..end`. Text typed at either
/// edge stays outside, and an empty range is ordered so it can't invert.
fn region_anchors(snapshot: &BufferSnapshot, start: usize, end: usize) -> Range<Anchor> {
    if start == end {
        snapshot.anchor_before(start)..snapshot.anchor_after(end)
    } else {
        snapshot.anchor_after(start)..snapshot.anchor_before(end)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum ConflictSide {
    Ours,
    Theirs,
}

#[derive(Clone, Copy, PartialEq)]
enum SidePaneKind {
    Base,
    Ours,
    Theirs,
}

/// Marker type for the gutter stripe showing the selected conflict in the
/// split panes.
struct SelectedConflictGutter;
/// Marker types for the rows a checkbox choice kept, in the side pane they
/// came from and in the result.
struct TakenRows;
struct ResultOursRows;
struct ResultTheirsRows;

#[derive(Clone, Copy, Debug, PartialEq)]
enum ChangeKind {
    /// Lines this version has that the result doesn't.
    Added,
    /// Lines this version has in place of different lines in the result.
    Modified,
}

/// Rows of a side pane that differ from the result outside any conflict.
#[derive(Clone, Debug, PartialEq)]
struct SideChange {
    rows: Range<u32>,
    kind: ChangeKind,
}

/// One version of the file and how it differs from the result.
struct SideContent {
    text: String,
    /// Line diff from the result (old) to this version (new).
    hunks: LineHunks,
    changes: Vec<SideChange>,
    /// Byte ranges in `text` of the words that differ from the result.
    word_ranges: Vec<Range<usize>>,
}

/// Marker types for the row colors of side pane changes and conflicts.
struct AddedRows;
struct ModifiedRows;
struct ConflictRows;

/// A read-only pane showing one version of the file next to the result.
struct SidePane {
    kind: SidePaneKind,
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    /// Line diff from the result buffer (old) to this pane's text (new). The
    /// two differ by the conflict markers and by whatever the other side
    /// changed, so rows are mapped through this rather than assumed equal.
    hunks: LineHunks,
    changes: Vec<SideChange>,
    word_ranges: Vec<Range<usize>>,
    /// The checkbox blocks currently inserted in this pane.
    checkbox_blocks: Vec<CustomBlockId>,
}

/// Full-file split view: the real buffer in an editable result pane, with
/// each side's version of the file alongside it.
struct SplitView {
    result: Entity<Editor>,
    ours: SidePane,
    theirs: SidePane,
    base: SidePane,
    _subscriptions: Vec<Subscription>,
}

impl SplitView {
    fn side_panes(&self) -> impl Iterator<Item = &SidePane> {
        [&self.base, &self.ours, &self.theirs].into_iter()
    }

    fn side_panes_mut(&mut self) -> impl Iterator<Item = &mut SidePane> {
        [&mut self.base, &mut self.ours, &mut self.theirs].into_iter()
    }
}

fn selected_conflict_gutter(cx: &App) -> Hsla {
    cx.theme().colors().text_accent
}

// Each side keeps the color the inline conflict view gives it in the result,
// so where a block came from reads the same in every pane. Unselected
// conflicts are drawn fainter so the selected one stands out.
fn ours_background(cx: &App) -> Hsla {
    cx.theme().colors().version_control_conflict_marker_ours
}

fn ours_faint_background(cx: &App) -> Hsla {
    cx.theme()
        .colors()
        .version_control_conflict_marker_ours
        .opacity(0.4)
}

fn theirs_background(cx: &App) -> Hsla {
    cx.theme().colors().version_control_conflict_marker_theirs
}

fn theirs_faint_background(cx: &App) -> Hsla {
    cx.theme()
        .colors()
        .version_control_conflict_marker_theirs
        .opacity(0.4)
}

fn base_background(cx: &App) -> Hsla {
    cx.theme().status().conflict_background
}

fn base_faint_background(cx: &App) -> Hsla {
    cx.theme().status().conflict_background.opacity(0.4)
}

/// The (selected, unselected) conflict colors for a pane.
fn pane_conflict_colors(kind: SidePaneKind) -> (fn(&App) -> Hsla, fn(&App) -> Hsla) {
    match kind {
        SidePaneKind::Ours => (ours_background, ours_faint_background),
        SidePaneKind::Theirs => (theirs_background, theirs_faint_background),
        SidePaneKind::Base => (base_background, base_faint_background),
    }
}

/// Decodes one index version into text comparable with the file's buffer.
pub(crate) fn decode_stage(bytes: &[u8], encoding: &'static Encoding) -> String {
    // `decode` also strips a byte order mark, as opening the file does.
    let (text, _, _) = encoding.decode(bytes);
    let mut text = text.into_owned();
    // Buffers normalize line endings to LF, so the stages must too or every
    // line of a CRLF file would differ from the result.
    LineEnding::normalize(&mut text);
    text
}

/// Decodes index stages for display in the file's own encoding, or returns
/// `None` when the view should use the conflict markers instead.
fn prepare_stages(stages: &ConflictStages, encoding: &'static Encoding) -> Option<Arc<StageTexts>> {
    // No stages means the path isn't unmerged in the index (already staged, or
    // never conflicted), and binary stages have no lines to show, so in both
    // cases the markers are all there is.
    if *stages == ConflictStages::default() || stages.binary {
        return None;
    }
    let decode =
        |bytes: &Option<Vec<u8>>| bytes.as_ref().map(|bytes| decode_stage(bytes, encoding));
    Some(Arc::new(StageTexts {
        base: decode(&stages.base),
        ours: decode(&stages.ours),
        theirs: decode(&stages.theirs),
    }))
}

/// Splices each conflict's kept side into the surrounding non-conflict
/// content, producing that side's full-file text as the markers describe it.
fn derive_pane_content(
    snapshot: &BufferSnapshot,
    conflicts: &[ConflictRegion],
    kept_range: impl Fn(&ConflictRegion) -> Option<Range<Anchor>>,
) -> String {
    let mut text = String::new();
    let mut cursor = snapshot.anchor_before(0);
    for conflict in conflicts {
        text.extend(snapshot.text_for_range(cursor..conflict.range.start));
        if let Some(kept) = kept_range(conflict) {
            text.extend(snapshot.text_for_range(kept));
        }
        cursor = conflict.range.end;
    }
    let end = snapshot.anchor_after(snapshot.len());
    text.extend(snapshot.text_for_range(cursor..end));
    text
}

fn side_text(
    stages: Option<&StageTexts>,
    kind: SidePaneKind,
    snapshot: &BufferSnapshot,
    conflicts: &[ConflictRegion],
) -> String {
    if let Some(stages) = stages {
        let stage = match kind {
            SidePaneKind::Base => &stages.base,
            SidePaneKind::Ours => &stages.ours,
            SidePaneKind::Theirs => &stages.theirs,
        };
        // A missing stage means that side doesn't have the file at all.
        return stage.clone().unwrap_or_default();
    }
    derive_pane_content(snapshot, conflicts, |conflict| match kind {
        SidePaneKind::Base => conflict.base.clone(),
        SidePaneKind::Ours => Some(conflict.ours.clone()),
        SidePaneKind::Theirs => Some(conflict.theirs.clone()),
    })
}

/// Each side pane's text and how it differs from the result, in base, ours,
/// theirs order.
fn compute_side_contents(
    stages: Option<&StageTexts>,
    snapshot: &BufferSnapshot,
    conflicts: &[ConflictRegion],
    ignore_whitespace: bool,
) -> [SideContent; 3] {
    let result_text = snapshot.text();
    [SidePaneKind::Base, SidePaneKind::Ours, SidePaneKind::Theirs].map(|kind| {
        let text = side_text(stages, kind, snapshot, conflicts);
        let hunks = line_hunks(&result_text, &text, ignore_whitespace);
        let conflict_rows: Vec<Range<u32>> = conflicts
            .iter()
            .map(|conflict| side_conflict_rows(kind, &hunks, conflict, snapshot))
            .collect();
        let (changes, word_ranges) = side_changes(
            &result_text,
            &text,
            &hunks,
            &conflict_rows,
            ignore_whitespace,
        );
        SideContent {
            text,
            hunks,
            changes,
            word_ranges,
        }
    })
}

/// Line diff from `result` to `side`. Ignoring whitespace compares each line
/// with all its whitespace removed, like `git diff -w`, which keeps the line
/// count, and so every row number, the same as the real texts.
fn line_hunks(result: &str, side: &str, ignore_whitespace: bool) -> LineHunks {
    if !ignore_whitespace {
        return language::line_diff(result, side);
    }
    let strip = |text: &str| -> String {
        text.split_inclusive('\n')
            .map(|line| {
                let mut stripped = without_whitespace(line);
                if line.ends_with('\n') {
                    stripped.push('\n');
                }
                stripped
            })
            .collect()
    };
    language::line_diff(&strip(result), &strip(side))
}

/// Byte offset of the start of each line of `text`, plus its length.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
    if starts.last() != Some(&text.len()) {
        starts.push(text.len());
    }
    starts
}

fn byte_range_for_rows(starts: &[usize], rows: &Range<u32>) -> Range<usize> {
    let offset = |row: u32| {
        starts
            .get(row as usize)
            .or(starts.last())
            .copied()
            .unwrap_or(0)
    };
    offset(rows.start)..offset(rows.end)
}

/// The rows of `side` that differ from `result` outside the conflicts, and the
/// words within small changed blocks that do. `conflict_rows` are the rows of
/// `side` holding each conflict's version; those keep the conflict's color.
fn side_changes(
    result: &str,
    side: &str,
    hunks: &[(Range<u32>, Range<u32>)],
    conflict_rows: &[Range<u32>],
    ignore_whitespace: bool,
) -> (Vec<SideChange>, Vec<Range<usize>>) {
    let result_starts = line_starts(result);
    let side_starts = line_starts(side);
    let mut changes = Vec::new();
    let mut word_ranges = Vec::new();
    for (old, new) in hunks {
        // Lines only the result has leave no rows to color in this pane.
        if new.is_empty() {
            continue;
        }
        // A change next to a conflict lands in the same block of the diff
        // as the conflict's lines, so subtract the conflict's rows rather
        // than skipping the block.
        let outside_conflicts = subtract_rows(new.clone(), conflict_rows);
        let kind = if old.is_empty() {
            ChangeKind::Added
        } else {
            ChangeKind::Modified
        };
        let touches_conflict = outside_conflicts.as_slice() != std::slice::from_ref(new);
        changes.extend(
            outside_conflicts
                .into_iter()
                .map(|rows| SideChange { rows, kind }),
        );
        // Word diffs pair this block's lines with the result's, which only
        // makes sense when no conflict lines are mixed in.
        if !touches_conflict
            && kind == ChangeKind::Modified
            && old.len() as u32 <= MAX_WORD_DIFF_ROWS
            && new.len() as u32 <= MAX_WORD_DIFF_ROWS
        {
            let side_range = byte_range_for_rows(&side_starts, new);
            let result_range = byte_range_for_rows(&result_starts, old);
            let side_block = &side[side_range.clone()];
            let result_block = &result[result_range];
            if side_block.len() > MAX_WORD_DIFF_BYTES || result_block.len() > MAX_WORD_DIFF_BYTES {
                continue;
            }
            for (range, replacement) in language::text_diff(side_block, result_block) {
                if range.is_empty() {
                    continue;
                }
                let removed = &side_block[range.clone()];
                if ignore_whitespace
                    && without_whitespace(removed) == without_whitespace(&replacement)
                {
                    continue;
                }
                word_ranges.push(side_range.start + range.start..side_range.start + range.end);
            }
        }
    }
    (changes, word_ranges)
}

/// The parts of `rows` not covered by any of `holes`.
fn subtract_rows(rows: Range<u32>, holes: &[Range<u32>]) -> Vec<Range<u32>> {
    let mut remaining = vec![rows];
    for hole in holes.iter().filter(|hole| !hole.is_empty()) {
        remaining = remaining
            .into_iter()
            .flat_map(|range| {
                [
                    range.start..hole.start.min(range.end),
                    hole.end.max(range.start)..range.end,
                ]
            })
            .filter(|range| !range.is_empty())
            .collect();
    }
    remaining
}

fn without_whitespace(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// Maps a row through a line diff, from the old text to the new one, or the
/// reverse. Between hunks the texts are identical, so the offset left by the
/// last passed hunk translates exactly. Inside a hunk the row is aligned to the
/// hunk's start and clamped to the other side's extent.
fn map_row(hunks: &[(Range<u32>, Range<u32>)], row: f64, reverse: bool) -> f64 {
    let mut delta = 0.0;
    for (old, new) in hunks {
        let (from, to) = if reverse { (new, old) } else { (old, new) };
        if row < f64::from(from.start) {
            break;
        }
        if row < f64::from(from.end) {
            let offset_in_hunk = row - f64::from(from.start);
            let to_len = f64::from(to.end - to.start);
            return f64::from(to.start) + offset_in_hunk.min(to_len);
        }
        delta = f64::from(to.end) - f64::from(from.end);
    }
    (row + delta).max(0.0)
}

/// The rows of the new text that replace `rows` of the old one. Lines between
/// the edges of `rows` and the first or last hunk touching it are unchanged, so
/// they map one to one; any hunk overlapping the range is included whole.
fn map_rows_outward(hunks: &[(Range<u32>, Range<u32>)], rows: Range<u32>) -> Range<u32> {
    let overlaps = |old: &Range<u32>| {
        if old.is_empty() {
            rows.start < old.start && old.start < rows.end
        } else {
            old.start < rows.end && old.end > rows.start
        }
    };
    let mut delta: i64 = 0;
    let mut first = None;
    let mut last = None;
    for (old, new) in hunks {
        if overlaps(old) {
            first.get_or_insert((old, new));
            last = Some((old, new));
        } else if old.end <= rows.start {
            delta = i64::from(new.end) - i64::from(old.end);
        } else {
            break;
        }
    }
    let shift = |row: u32, delta: i64| u32::try_from(i64::from(row) + delta).unwrap_or(0);
    match (first, last) {
        (Some((first_old, first_new)), Some((last_old, last_new))) => {
            let start = if first_old.start <= rows.start {
                first_new.start
            } else {
                first_new.start.saturating_sub(first_old.start - rows.start)
            };
            let end = if last_old.end >= rows.end {
                last_new.end
            } else {
                last_new.end + (rows.end - last_old.end)
            };
            start..end.max(start)
        }
        _ => {
            let row = shift(rows.start, delta);
            row..row
        }
    }
}

/// The lines of `text` in `rows`, with their line endings.
fn text_for_rows(text: &str, rows: Range<u32>) -> String {
    let skip = rows.start as usize;
    let take = rows.end.saturating_sub(rows.start) as usize;
    text.split_inclusive('\n').skip(skip).take(take).collect()
}

/// Base snippets for the cards of conflicts whose markers have no base
/// section, from the index's base version and its diff from the result.
fn compute_card_bases(
    base_text: &str,
    base_hunks: &[(Range<u32>, Range<u32>)],
    snapshot: &BufferSnapshot,
    conflicts: &[ConflictRegion],
) -> Vec<(Range<Anchor>, SharedString)> {
    conflicts
        .iter()
        .filter(|conflict| conflict.base.is_none())
        .map(|conflict| {
            let rows = map_rows_outward(base_hunks, anchor_rows(snapshot, &conflict.range));
            (
                conflict.range.clone(),
                text_for_rows(base_text, rows).into(),
            )
        })
        .collect()
}

/// The buffer rows covered by `range`. A range ending at column 0 doesn't
/// include that line, and an empty range still covers the row it sits on, so
/// the position stays visible when highlighted.
fn anchor_rows(snapshot: &BufferSnapshot, range: &Range<Anchor>) -> Range<u32> {
    let start = range.start.to_point(snapshot);
    let end = range.end.to_point(snapshot);
    let end_row = if end.column == 0 && end.row > start.row {
        end.row
    } else {
        end.row + 1
    };
    start.row..end_row
}

fn build_side_pane(
    kind: SidePaneKind,
    content: SideContent,
    language: Option<Arc<Language>>,
    window: &mut Window,
    cx: &mut Context<MergeEditorView>,
) -> SidePane {
    let buffer = cx.new(|cx| {
        let mut buffer = Buffer::local(content.text, cx);
        buffer.set_language(language, cx);
        buffer
    });
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_buffer(buffer.clone(), None, window, cx);
        editor.set_read_only(true);
        editor
    });
    SidePane {
        kind,
        editor,
        buffer,
        hunks: content.hunks,
        changes: content.changes,
        word_ranges: content.word_ranges,
        checkbox_blocks: Vec::new(),
    }
}

fn render_split_pane(
    editor: Entity<Editor>,
    header: AnyElement,
    cx: &Context<MergeEditorView>,
) -> impl IntoElement {
    v_flex()
        .flex_1()
        .min_w_0()
        .child(
            h_flex()
                .px_2()
                .py_1()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(header),
        )
        .child(div().flex_1().min_h_0().child(editor))
}

/// A checkbox above one side's version of a conflict.
#[derive(Clone)]
struct SideCheckbox {
    target: CheckboxTarget,
    side: ConflictSide,
    checked: bool,
    /// The side pane rows of that side's version, which the box sits above.
    rows: Range<u32>,
    label: SharedString,
}

fn render_side_checkbox(
    checkbox: &SideCheckbox,
    view: WeakEntity<MergeEditorView>,
    cx: &mut BlockContext,
) -> AnyElement {
    let target = checkbox.target.clone();
    let side = checkbox.side;
    h_flex()
        .h(cx.line_height)
        .ml(cx.margins.gutter.width)
        .bg(cx.theme().colors().editor_background)
        .child(
            Checkbox::new(
                cx.block_id,
                if checkbox.checked {
                    ToggleState::Selected
                } else {
                    ToggleState::Unselected
                },
            )
            .label(checkbox.label.clone())
            .label_size(LabelSize::Small)
            .on_click(move |_, _, cx| {
                view.update(cx, |view, cx| view.toggle_side(&target, side, cx))
                    .log_err();
            }),
        )
        .into_any_element()
}

/// The rows of `pane` holding `conflict`'s version of the region: the side's
/// own lines map exactly, and the base's are the lines the conflict replaced.
fn side_conflict_rows(
    kind: SidePaneKind,
    hunks: &[(Range<u32>, Range<u32>)],
    conflict: &ConflictRegion,
    snapshot: &BufferSnapshot,
) -> Range<u32> {
    let result_range = match kind {
        SidePaneKind::Ours => &conflict.ours,
        SidePaneKind::Theirs => &conflict.theirs,
        SidePaneKind::Base => match &conflict.base {
            Some(base) => base,
            None => {
                return map_rows_outward(hunks, anchor_rows(snapshot, &conflict.range));
            }
        },
    };
    let rows = anchor_rows(snapshot, result_range);
    let start = map_row(hunks, f64::from(rows.start), false) as u32;
    let end = map_row(hunks, f64::from(rows.end), false) as u32;
    start..end.max(start)
}

impl MergeEditorView {
    /// `preloaded_stages` skips reading the index when the caller already
    /// has the file's versions; otherwise they're loaded in the background.
    pub fn new(
        buffer: Entity<Buffer>,
        conflict_set: Entity<ConflictSet>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        preloaded_stages: Option<ConflictStages>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let file_label: SharedString = buffer
            .read(cx)
            .file()
            .map(|f| f.path().as_std_path().to_string_lossy().to_string().into())
            .unwrap_or_else(|| "untitled".into());

        let subscriptions = vec![
            cx.subscribe(&conflict_set, Self::on_conflict_set_update),
            // Only text edits change the split panes. Observing the buffer also
            // fired on reparses and other non-edit notifies, each of which
            // re-diffed every pane.
            cx.subscribe(&buffer, |this, _, event, cx| match event {
                language::BufferEvent::Edited { .. } => {
                    this.refresh_split_content(cx);
                    this.refresh_split_highlights(cx);
                    cx.notify();
                }
                language::BufferEvent::DirtyChanged
                | language::BufferEvent::Saved
                | language::BufferEvent::FileHandleChanged => {
                    cx.emit(MergeEditorViewEvent::UpdateTab);
                    cx.notify();
                }
                // In a window that was just opened (`lathe --resolve`, `--merge`),
                // the file's language is often still loading when the panes are
                // built, so they take it when it arrives.
                language::BufferEvent::LanguageChanged(_) => this.sync_side_pane_language(cx),
                _ => {}
            }),
        ];

        let whole_file_conflict = preloaded_stages
            .as_ref()
            .is_some_and(ConflictStages::is_whole_file);
        let (stages, load_stages) = match preloaded_stages {
            Some(stages) => (prepare_stages(&stages, buffer.read(cx).encoding()), None),
            None => (None, Self::load_stages(&buffer, &project, cx)),
        };

        let load_side_names = Self::load_side_names(&buffer, &project, cx);
        let marker_names = Self::marker_names(&conflict_set, cx);

        let mut this = Self {
            focus_handle,
            buffer,
            conflict_set,
            project,
            workspace,
            file_label,
            selected_index: 0,
            view_mode: MergeViewMode::Split,
            layout: SplitLayout::ResultBelow,
            choices: Vec::new(),
            split: None,
            stages,
            whole_file_conflict,
            merge_tool: false,
            show_base: false,
            ignore_whitespace: false,
            syncing_split_scroll: false,
            _load_stages: load_stages,
            side_names: ConflictSideNames::default(),
            marker_names,
            _load_side_names: load_side_names,
            card_bases: Vec::new(),
            _refresh_side_contents: None,
            _subscriptions: subscriptions,
        };
        this.build_split_view(window, cx);
        this.scroll_split_to_selected(window, cx);
        this
    }

    fn load_stages(
        buffer: &Entity<Buffer>,
        project: &Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        let buffer_id = buffer.read(cx).remote_id();
        let (repository, path) = project
            .read(cx)
            .git_store()
            .read(cx)
            .repository_and_path_for_buffer_id(buffer_id, cx)?;
        let receiver = repository.update(cx, |repository, _| repository.load_conflict_stages(path));
        Some(cx.spawn(async move |this, cx| {
            let stages = match receiver.await {
                Ok(Ok(stages)) => stages,
                Ok(Err(error)) => {
                    log::warn!("merge editor falling back to conflict markers: {error:#}");
                    return;
                }
                Err(_) => return,
            };
            if stages.is_whole_file() {
                this.update(cx, |this, cx| {
                    this.whole_file_conflict = true;
                    cx.notify();
                })
                .log_err();
            }
            this.update(cx, |this, cx| {
                let Some(stages) = prepare_stages(&stages, this.buffer.read(cx).encoding()) else {
                    return;
                };
                this.stages = Some(stages);
                this.refresh_split_content(cx);
                this.refresh_split_highlights(cx);
                cx.notify();
            })
            .log_err();
        }))
    }

    fn load_side_names(
        buffer: &Entity<Buffer>,
        project: &Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        let buffer_id = buffer.read(cx).remote_id();
        let (repository, _) = project
            .read(cx)
            .git_store()
            .read(cx)
            .repository_and_path_for_buffer_id(buffer_id, cx)?;
        let receiver = repository.update(cx, |repository, _| repository.conflict_side_names());
        Some(cx.spawn(async move |this, cx| {
            let side_names = match receiver.await {
                Ok(Ok(side_names)) => side_names,
                Ok(Err(error)) => {
                    log::warn!("merge editor couldn't read the branch names: {error:#}");
                    return;
                }
                Err(_) => return,
            };
            this.update(cx, |this, cx| {
                this.side_names = side_names;
                this.refresh_change_highlights(cx);
                cx.notify();
            })
            .log_err();
        }))
    }

    fn marker_names(conflict_set: &Entity<ConflictSet>, cx: &App) -> ConflictSideNames {
        let conflict_set = conflict_set.read(cx);
        let Some(conflict) = conflict_set.snapshot.conflicts.first() else {
            return ConflictSideNames::default();
        };
        let name = |name: &SharedString| (!name.is_empty()).then(|| name.clone());
        ConflictSideNames {
            ours: name(&conflict.ours_branch_name),
            theirs: name(&conflict.theirs_branch_name),
        }
    }

    /// The branch name to show for one side, if any is known.
    fn side_branch_name(&self, side: ConflictSide) -> Option<SharedString> {
        match side {
            ConflictSide::Ours => self
                .side_names
                .ours
                .clone()
                .or_else(|| self.marker_names.ours.clone()),
            ConflictSide::Theirs => self
                .side_names
                .theirs
                .clone()
                .or_else(|| self.marker_names.theirs.clone()),
        }
    }

    /// Resolves every remaining conflict in the file in favor of one side.
    /// The regions are anchored, so resolving them in sequence stays correct as
    /// earlier edits shift the text.
    pub fn resolve_all(&mut self, side: ConflictSide, cx: &mut Context<Self>) {
        let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
        for conflict in conflicts.iter() {
            let kept = match side {
                ConflictSide::Ours => vec![conflict.ours.clone()],
                ConflictSide::Theirs => vec![conflict.theirs.clone()],
            };
            conflict.resolve(self.buffer.clone(), &kept, cx);
        }
        cx.notify();
    }

    /// Opens the underlying buffer in a regular editor so the user can edit
    /// the conflict markers manually. The merge editor view stays open in the
    /// background; whatever the user types updates the same buffer so the
    /// merge editor's conflict cards update live.
    fn open_in_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_path) = self.buffer.read(cx).project_path(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace
                        .open_path(project_path, None, true, window, cx)
                        .detach();
                })
                .log_err();
        })
        .detach();
    }

    fn select_prev_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let total = self.conflict_set.read(cx).snapshot.conflicts.len();
        if total == 0 {
            return;
        }
        self.selected_index = self.selected_index.saturating_sub(1);
        self.on_selected_conflict_changed(window, cx);
    }

    fn select_next_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let total = self.conflict_set.read(cx).snapshot.conflicts.len();
        if total == 0 {
            return;
        }
        self.selected_index = (self.selected_index + 1).min(total - 1);
        self.on_selected_conflict_changed(window, cx);
    }

    fn on_selected_conflict_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view_mode == MergeViewMode::Split {
            self.refresh_split_highlights(cx);
            self.scroll_split_to_selected(window, cx);
        }
        cx.notify();
    }

    fn on_conflict_set_update(
        &mut self,
        _: Entity<ConflictSet>,
        _: &ConflictSetUpdate,
        cx: &mut Context<Self>,
    ) {
        let total = self.conflict_set.read(cx).snapshot.conflicts.len();
        if total == 0 {
            self.selected_index = 0;
        } else {
            self.marker_names = Self::marker_names(&self.conflict_set, cx);
            if self.selected_index >= total {
                self.selected_index = total - 1;
            }
        }
        self.refresh_split_content(cx);
        self.refresh_split_highlights(cx);
        cx.notify();
    }

    fn resolve_with(
        &self,
        conflict: &ConflictRegion,
        ranges: Vec<std::ops::Range<Anchor>>,
        cx: &mut Context<Self>,
    ) {
        conflict.resolve(self.buffer.clone(), &ranges, cx);
        cx.notify();
    }

    /// Saves the result and closes this tab, which is what tells a waiting
    /// `lathe --merge` that the merge is finished.
    fn finish_merge_tool(&mut self, cx: &mut Context<Self>) {
        let buffer = self.buffer.clone();
        let save = self
            .project
            .update(cx, |project, cx| project.save_buffer(buffer, cx));
        cx.spawn(async move |this, cx| {
            save.await?;
            this.update(cx, |_, cx| cx.emit(MergeEditorViewEvent::Close))
        })
        .detach_and_log_err(cx);
    }

    fn save_buffer(&self, cx: &mut Context<Self>) {
        let buffer = self.buffer.clone();
        let project = self.project.clone();
        project
            .update(cx, |project, cx| project.save_buffer(buffer, cx))
            .detach_and_log_err(cx);
    }

    fn toggle_view_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.view_mode {
            MergeViewMode::Cards => {
                if self.split.is_none() {
                    self.build_split_view(window, cx);
                } else {
                    let snapshot = self.buffer.read(cx).snapshot();
                    let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
                    let contents = compute_side_contents(
                        self.stages.as_deref(),
                        &snapshot,
                        &conflicts,
                        self.ignore_whitespace,
                    );
                    self.apply_split_contents(contents, cx);
                }
                self.view_mode = MergeViewMode::Split;
                window.focus(&self.focus_handle(cx), cx);
                self.refresh_split_highlights(cx);
                self.scroll_split_to_selected(window, cx);
            }
            MergeViewMode::Split => {
                self.view_mode = MergeViewMode::Cards;
                window.focus(&self.focus_handle, cx);
            }
        }
        cx.notify();
    }

    fn toggle_ignore_whitespace(&mut self, cx: &mut Context<Self>) {
        self.ignore_whitespace = !self.ignore_whitespace;
        if self.split.is_some() {
            let snapshot = self.buffer.read(cx).snapshot();
            let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
            let contents = compute_side_contents(
                self.stages.as_deref(),
                &snapshot,
                &conflicts,
                self.ignore_whitespace,
            );
            self.apply_split_contents(contents, cx);
        }
        cx.notify();
    }

    fn toggle_show_base(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_base = !self.show_base;
        if self.show_base {
            self.scroll_split_to_selected(window, cx);
        }
        cx.notify();
    }

    /// Whether there's a base version to show: the index's stage 1 once
    /// loaded, otherwise a diff3-style base section in the markers.
    fn has_base(&self, cx: &App) -> bool {
        match &self.stages {
            Some(stages) => stages.base.is_some(),
            None => self
                .conflict_set
                .read(cx)
                .snapshot
                .conflicts
                .iter()
                .any(|conflict| conflict.base.is_some()),
        }
    }

    fn side_label(&self, kind: SidePaneKind) -> SharedString {
        let (side, name) = match kind {
            SidePaneKind::Base => (None, "Base"),
            SidePaneKind::Ours => (Some(ConflictSide::Ours), "Ours"),
            SidePaneKind::Theirs => (Some(ConflictSide::Theirs), "Theirs"),
        };
        let mut label = match side.and_then(|side| self.side_branch_name(side)) {
            Some(branch) => format!("{name} ({branch})"),
            None => name.to_string(),
        };
        if let Some(stages) = &self.stages {
            let stage = match kind {
                SidePaneKind::Base => &stages.base,
                SidePaneKind::Ours => &stages.ours,
                SidePaneKind::Theirs => &stages.theirs,
            };
            if stage.is_none() {
                label.push_str(if kind == SidePaneKind::Base {
                    ": no common version"
                } else {
                    ": deleted"
                });
            }
        }
        label.into()
    }

    fn sync_side_pane_language(&mut self, cx: &mut Context<Self>) {
        let Some(split) = &self.split else {
            return;
        };
        let language = self.buffer.read(cx).language().cloned();
        for pane in split.side_panes() {
            pane.buffer.update(cx, |buffer, cx| {
                if buffer.language() != language.as_ref() {
                    buffer.set_language(language.clone(), cx);
                }
            });
        }
    }

    fn build_split_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot();
        let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
        let language = self.buffer.read(cx).language().cloned();
        let [base, ours, theirs] = compute_side_contents(
            self.stages.as_deref(),
            &snapshot,
            &conflicts,
            self.ignore_whitespace,
        );
        let base = build_side_pane(SidePaneKind::Base, base, language.clone(), window, cx);
        let ours = build_side_pane(SidePaneKind::Ours, ours, language.clone(), window, cx);
        let theirs = build_side_pane(SidePaneKind::Theirs, theirs, language, window, cx);

        // The result pane edits the real buffer, so it gets the project's
        // language services and the inline conflict buttons like any editor.
        let result = cx.new(|cx| {
            Editor::for_buffer(self.buffer.clone(), Some(self.project.clone()), window, cx)
        });

        let mut subscriptions =
            vec![
                cx.subscribe_in(&result, window, |this, editor, event, window, cx| {
                    if let EditorEvent::SelectionsChanged { local: true } = event {
                        this.select_conflict_at_cursor(editor, window, cx);
                    }
                }),
            ];
        for editor in [&result, &base.editor, &ours.editor, &theirs.editor] {
            subscriptions.push(cx.subscribe_in(
                editor,
                window,
                |this, editor, event, window, cx| {
                    if matches!(event, EditorEvent::ScrollPositionChanged { .. }) {
                        this.sync_split_scroll(editor.entity_id(), window, cx);
                    }
                },
            ));
        }

        self.split = Some(SplitView {
            result,
            ours,
            theirs,
            base,
            _subscriptions: subscriptions,
        });
        self.refresh_change_highlights(cx);
    }

    /// Recomputes the side versions of the file and their diffs from the
    /// result, off the main thread. Split view needs all of them; card view
    /// only needs them for the base snippets, which come from the index.
    fn refresh_split_content(&mut self, cx: &mut Context<Self>) {
        let has_index_base = self
            .stages
            .as_ref()
            .is_some_and(|stages| stages.base.is_some());
        if self.split.is_none() && !has_index_base {
            return;
        }
        let snapshot = self.buffer.read(cx).snapshot();
        let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
        let stages = self.stages.clone();
        let ignore_whitespace = self.ignore_whitespace;
        self._refresh_side_contents = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SPLIT_REFRESH_DEBOUNCE).await;
            let (contents, card_bases) = cx
                .background_spawn(async move {
                    let contents = compute_side_contents(
                        stages.as_deref(),
                        &snapshot,
                        &conflicts,
                        ignore_whitespace,
                    );
                    let card_bases = if has_index_base {
                        let base = &contents[0];
                        compute_card_bases(&base.text, &base.hunks, &snapshot, &conflicts)
                    } else {
                        Vec::new()
                    };
                    (contents, card_bases)
                })
                .await;
            this.update(cx, |this, cx| {
                this.card_bases = card_bases;
                this.apply_split_contents(contents, cx);
                cx.notify();
            })
            .log_err();
        }));
    }

    fn apply_split_contents(&mut self, contents: [SideContent; 3], cx: &mut Context<Self>) {
        let Some(split) = &mut self.split else {
            return;
        };
        for (pane, content) in split.side_panes_mut().zip(contents) {
            pane.hunks = content.hunks;
            pane.changes = content.changes;
            pane.word_ranges = content.word_ranges;
            pane.buffer.update(cx, |buffer, cx| {
                if buffer.text() != content.text {
                    buffer.set_text(content.text, cx);
                }
            });
        }
        self.refresh_change_highlights(cx);
    }

    /// Colors each side pane's rows that differ from the result and the
    /// changed words within small changes, and lays out its checkboxes.
    fn refresh_change_highlights(&mut self, cx: &mut Context<Self>) {
        let Some(split) = &self.split else {
            return;
        };
        let snapshot = self.buffer.read(cx).snapshot();
        let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
        let this = cx.weak_entity();
        let mut new_blocks = Vec::new();
        for pane in split.side_panes() {
            let side_snapshot = pane.buffer.read(cx).snapshot();
            let word_points: Vec<Range<Point>> = pane
                .word_ranges
                .iter()
                .filter(|range| range.end <= side_snapshot.len())
                .map(|range| {
                    side_snapshot.offset_to_point(range.start)
                        ..side_snapshot.offset_to_point(range.end)
                })
                .collect();
            let checkboxes = self.side_checkboxes(pane, &conflicts, &snapshot);
            let changes = pane.changes.clone();
            let old_blocks = pane.checkbox_blocks.clone();
            let this = this.clone();
            let block_ids = pane.editor.update(cx, |editor, cx| {
                editor.clear_row_highlights::<AddedRows>();
                editor.clear_row_highlights::<ModifiedRows>();
                editor.clear_row_highlights::<TakenRows>();
                editor.remove_blocks(old_blocks.into_iter().collect(), None, cx);
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let anchor_range = |rows: &Range<u32>| {
                    let start = snapshot.clip_point(Point::new(rows.start, 0), Bias::Left);
                    let end = snapshot.clip_point(Point::new(rows.end, 0), Bias::Left);
                    snapshot.anchor_before(start)..snapshot.anchor_before(end)
                };
                let options = RowHighlightOptions {
                    autoscroll: false,
                    include_gutter: true,
                };
                for change in &changes {
                    let range = anchor_range(&change.rows);
                    match change.kind {
                        ChangeKind::Added => editor.highlight_rows::<AddedRows>(
                            range,
                            |cx| cx.theme().status().created_background,
                            options,
                            cx,
                        ),
                        ChangeKind::Modified => editor.highlight_rows::<ModifiedRows>(
                            range,
                            |cx| cx.theme().status().modified_background,
                            options,
                            cx,
                        ),
                    }
                }
                for checkbox in &checkboxes {
                    if checkbox.checked && !checkbox.rows.is_empty() {
                        editor.highlight_rows::<TakenRows>(
                            anchor_range(&checkbox.rows),
                            match checkbox.side {
                                ConflictSide::Ours => ours_background,
                                ConflictSide::Theirs => theirs_background,
                            },
                            options,
                            cx,
                        );
                    }
                }
                let word_anchors: Vec<_> = word_points
                    .iter()
                    .map(|range| {
                        snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end)
                    })
                    .collect();
                editor.highlight_background(
                    HighlightKey::MergeEditorWordChange,
                    &word_anchors,
                    // Words this version has and the result doesn't, styled like
                    // the word-level changes in the editor's own diffs.
                    |_, theme| theme.colors().version_control_word_added,
                    cx,
                );

                let blocks = checkboxes
                    .into_iter()
                    .map(|checkbox| {
                        let anchor =
                            anchor_range(&(checkbox.rows.start..checkbox.rows.start)).start;
                        let this = this.clone();
                        BlockProperties {
                            placement: BlockPlacement::Above(anchor),
                            height: Some(1),
                            style: BlockStyle::Sticky,
                            render: Arc::new(move |cx| {
                                render_side_checkbox(&checkbox, this.clone(), cx)
                            }),
                            priority: 0,
                        }
                    })
                    .collect::<Vec<_>>();
                editor.insert_blocks(blocks, None, cx)
            });
            new_blocks.push(block_ids);
        }
        if let Some(split) = &mut self.split {
            for (pane, block_ids) in split.side_panes_mut().zip(new_blocks) {
                pane.checkbox_blocks = block_ids;
            }
        }
        self.refresh_split_highlights(cx);
    }

    /// The checkboxes `pane` shows: one per conflict still in the file and
    /// one per conflict resolved by checkbox, above that side's lines. The
    /// base pane has none, since the base can't be taken.
    fn side_checkboxes(
        &self,
        pane: &SidePane,
        conflicts: &[ConflictRegion],
        snapshot: &BufferSnapshot,
    ) -> Vec<SideCheckbox> {
        let side = match pane.kind {
            SidePaneKind::Ours => ConflictSide::Ours,
            SidePaneKind::Theirs => ConflictSide::Theirs,
            SidePaneKind::Base => return Vec::new(),
        };
        let branch = self.side_branch_name(side);
        let label: SharedString = match (side, branch) {
            (ConflictSide::Ours, Some(branch)) => format!("Take ours ({branch})").into(),
            (ConflictSide::Ours, None) => "Take ours".into(),
            (ConflictSide::Theirs, Some(branch)) => format!("Take theirs ({branch})").into(),
            (ConflictSide::Theirs, None) => "Take theirs".into(),
        };
        let mut checkboxes: Vec<SideCheckbox> = conflicts
            .iter()
            .map(|conflict| SideCheckbox {
                target: CheckboxTarget::Conflict(conflict.range.clone()),
                side,
                checked: false,
                rows: side_conflict_rows(pane.kind, &pane.hunks, conflict, snapshot),
                label: label.clone(),
            })
            .collect();
        for choice in &self.choices {
            let checked = match side {
                ConflictSide::Ours => choice.take_ours,
                ConflictSide::Theirs => choice.take_theirs,
            };
            checkboxes.push(SideCheckbox {
                target: CheckboxTarget::Choice(choice.range.clone()),
                side,
                checked,
                rows: map_rows_outward(&pane.hunks, anchor_rows(snapshot, &choice.range)),
                label: label.clone(),
            });
        }
        checkboxes.sort_by_key(|checkbox| checkbox.rows.start);
        checkboxes
    }

    /// Marks the selected conflict with a stripe in every pane's gutter, and
    /// shows the side panes' other conflicts fainter than the selected one.
    /// The result pane keeps its own ours/theirs coloring from the inline
    /// conflict view, so it only gets the stripe.
    fn refresh_split_highlights(&self, cx: &mut Context<Self>) {
        let Some(split) = &self.split else {
            return;
        };
        let snapshot = self.buffer.read(cx).snapshot();
        let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
        let selected = self.selected_index;

        let result_rows = conflicts
            .get(selected)
            .map(|conflict| anchor_rows(&snapshot, &conflict.range));
        let mut targets = vec![(split.result.clone(), None, result_rows, Vec::new())];
        for pane in split.side_panes() {
            let rows: Vec<Range<u32>> = conflicts
                .iter()
                .map(|conflict| side_conflict_rows(pane.kind, &pane.hunks, conflict, &snapshot))
                .collect();
            // A side that contributed no lines still gets one row of stripe,
            // so the conflict's position stays visible.
            let selected_rows = rows
                .get(selected)
                .map(|rows| rows.start..rows.end.max(rows.start + 1));
            targets.push((pane.editor.clone(), Some(pane.kind), selected_rows, rows));
        }

        // The blocks checkbox choices put in the result, by the side they came
        // from: ours first, then theirs when both were taken.
        let mut result_ours = Vec::new();
        let mut result_theirs = Vec::new();
        for choice in &self.choices {
            let start = choice.range.start.to_offset(&snapshot);
            let ours_end = start
                + if choice.take_ours {
                    choice.ours.len()
                } else {
                    0
                };
            let theirs_end = ours_end
                + if choice.take_theirs {
                    choice.theirs.len()
                } else {
                    0
                };
            if choice.take_ours && ours_end > start {
                result_ours.push(
                    snapshot.offset_to_point(start).row
                        ..snapshot.offset_to_point(ours_end.min(snapshot.len())).row,
                );
            }
            if choice.take_theirs && theirs_end > ours_end {
                result_theirs.push(
                    snapshot.offset_to_point(ours_end).row
                        ..snapshot.offset_to_point(theirs_end.min(snapshot.len())).row,
                );
            }
        }
        split.result.update(cx, |editor, cx| {
            editor.clear_row_highlights::<ResultOursRows>();
            editor.clear_row_highlights::<ResultTheirsRows>();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchor_range = |rows: &Range<u32>| {
                let start = snapshot.clip_point(Point::new(rows.start, 0), Bias::Left);
                let end = snapshot.clip_point(Point::new(rows.end, 0), Bias::Left);
                snapshot.anchor_before(start)..snapshot.anchor_before(end)
            };
            let options = RowHighlightOptions {
                autoscroll: false,
                include_gutter: true,
            };
            for rows in &result_ours {
                editor.highlight_rows::<ResultOursRows>(
                    anchor_range(rows),
                    ours_faint_background,
                    options,
                    cx,
                );
            }
            for rows in &result_theirs {
                editor.highlight_rows::<ResultTheirsRows>(
                    anchor_range(rows),
                    theirs_faint_background,
                    options,
                    cx,
                );
            }
        });

        for (editor, kind, selected_rows, conflict_rows) in targets {
            editor.update(cx, |editor, cx| {
                editor.clear_gutter_highlights::<SelectedConflictGutter>(cx);
                editor.clear_row_highlights::<ConflictRows>();
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let anchor_range = |rows: &Range<u32>| {
                    let start = snapshot.clip_point(Point::new(rows.start, 0), Bias::Left);
                    let end = snapshot.clip_point(Point::new(rows.end, 0), Bias::Left);
                    snapshot.anchor_before(start)..snapshot.anchor_before(end)
                };
                let (selected_color, other_color) = kind
                    .map(pane_conflict_colors)
                    .unwrap_or((base_background, base_faint_background));
                for (index, rows) in conflict_rows.iter().enumerate() {
                    if rows.is_empty() {
                        continue;
                    }
                    editor.highlight_rows::<ConflictRows>(
                        anchor_range(rows),
                        if index == selected {
                            selected_color
                        } else {
                            other_color
                        },
                        RowHighlightOptions {
                            autoscroll: false,
                            include_gutter: true,
                        },
                        cx,
                    );
                }
                if let Some(rows) = selected_rows {
                    editor.highlight_gutter::<SelectedConflictGutter>(
                        vec![anchor_range(&rows)],
                        selected_conflict_gutter,
                        cx,
                    );
                }
            });
        }
    }

    /// Selects the conflict the cursor is in, so clicking into a conflict in
    /// the result makes it the one the header buttons act on.
    fn select_conflict_at_cursor(
        &mut self,
        editor: &Entity<Editor>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = {
            let editor = editor.read(cx);
            let head = editor.selections.newest_anchor().head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .anchor_to_buffer_anchor(head)
                .map(|(anchor, _)| anchor)
        };
        let Some(cursor) = cursor else {
            return;
        };
        let snapshot = self.buffer.read(cx).snapshot();
        let index = self
            .conflict_set
            .read(cx)
            .snapshot
            .conflicts
            .iter()
            .position(|conflict| {
                conflict.range.start.cmp(&cursor, &snapshot).is_le()
                    && conflict.range.end.cmp(&cursor, &snapshot).is_ge()
            });
        if let Some(index) = index
            && index != self.selected_index
        {
            self.selected_index = index;
            self.refresh_split_highlights(cx);
            cx.notify();
        }
    }

    /// Checks or unchecks one side of a conflict and rewrites the result to
    /// match: one side, both (ours first), or the original markers when
    /// neither is checked.
    fn toggle_side(&mut self, target: &CheckboxTarget, side: ConflictSide, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot();
        let (range, mut choice) = match target {
            CheckboxTarget::Conflict(range) => {
                let conflicts = self.conflict_set.read(cx).snapshot.conflicts.clone();
                let Some(conflict) = conflicts.iter().find(|conflict| conflict.range == *range)
                else {
                    return;
                };
                let text = |range: &Range<Anchor>| -> String {
                    snapshot.text_for_range(range.clone()).collect()
                };
                let choice = ConflictChoice {
                    range: conflict.range.clone(),
                    original: text(&conflict.range),
                    ours: text(&conflict.ours),
                    theirs: text(&conflict.theirs),
                    take_ours: false,
                    take_theirs: false,
                };
                (conflict.range.clone(), choice)
            }
            CheckboxTarget::Choice(range) => {
                let Some(position) = self
                    .choices
                    .iter()
                    .position(|choice| choice.range == *range)
                else {
                    return;
                };
                let choice = self.choices.remove(position);
                // An edit inside the region makes the choice stale: it no
                // longer describes the text, so leave the text alone.
                let expected = choice_text(
                    &choice.ours,
                    &choice.theirs,
                    choice.take_ours,
                    choice.take_theirs,
                );
                let current: String = snapshot.text_for_range(choice.range.clone()).collect();
                if expected.as_deref() != Some(current.as_str()) {
                    self.refresh_change_highlights(cx);
                    return;
                }
                (choice.range.clone(), choice)
            }
        };
        match side {
            ConflictSide::Ours => choice.take_ours = !choice.take_ours,
            ConflictSide::Theirs => choice.take_theirs = !choice.take_theirs,
        }

        let new_text = choice_text(
            &choice.ours,
            &choice.theirs,
            choice.take_ours,
            choice.take_theirs,
        );
        let replacement = new_text.clone().unwrap_or_else(|| choice.original.clone());
        let start = range.start.to_offset(&snapshot);
        let end = range.end.to_offset(&snapshot);
        self.buffer.update(cx, |buffer, cx| {
            buffer.edit([(start..end, replacement.as_str())], None, cx);
        });
        if new_text.is_some() {
            let snapshot = self.buffer.read(cx).snapshot();
            choice.range = region_anchors(&snapshot, start, start + replacement.len());
            self.choices.push(choice);
        }
        cx.notify();
    }
    fn scroll_split_to_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot();
        let Some(conflict) = self
            .conflict_set
            .read(cx)
            .snapshot
            .conflicts
            .get(self.selected_index)
            .cloned()
        else {
            return;
        };
        // Leave a couple of context lines visible above the conflict.
        let result_row = f64::from(anchor_rows(&snapshot, &conflict.range).start).max(2.0) - 2.0;
        self.scroll_split_to_result_row(result_row, None, window, cx);
    }

    fn sync_split_scroll(&mut self, source: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if self.syncing_split_scroll {
            return;
        }
        let Some(split) = &self.split else {
            return;
        };
        let result_row = if split.result.entity_id() == source {
            split
                .result
                .update(cx, |editor, cx| editor.scroll_position(cx))
                .y
        } else {
            let Some(pane) = split
                .side_panes()
                .find(|pane| pane.editor.entity_id() == source)
            else {
                return;
            };
            let row = pane
                .editor
                .update(cx, |editor, cx| editor.scroll_position(cx))
                .y;
            map_row(&pane.hunks, row, true)
        };
        self.scroll_split_to_result_row(result_row, Some(source), window, cx);
    }

    /// Scrolls every pane except `skip` to the content at `result_row` of the
    /// result buffer. Vertical only: line lengths differ between panes, so
    /// each keeps its own horizontal scroll.
    fn scroll_split_to_result_row(
        &mut self,
        result_row: f64,
        skip: Option<EntityId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(split) = &self.split else {
            return;
        };
        let mut targets = vec![(split.result.clone(), result_row)];
        for pane in split.side_panes() {
            targets.push((pane.editor.clone(), map_row(&pane.hunks, result_row, false)));
        }
        self.syncing_split_scroll = true;
        for (editor, target_y) in targets {
            if Some(editor.entity_id()) == skip {
                continue;
            }
            editor.update(cx, |editor, cx| {
                let current = editor.scroll_position(cx);
                if (current.y - target_y).abs() > 0.01 {
                    editor.set_scroll_position(point(current.x, target_y), window, cx);
                }
            });
        }
        // The set_scroll_position calls above echo ScrollPositionChanged
        // events after this handler returns; keep the guard up until those
        // have been delivered.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this, _| this.syncing_split_scroll = false)
                .log_err();
        });
    }

    fn render_split_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(split) = &self.split else {
            return div().into_any_element();
        };
        let side = |editor: &Entity<Editor>, kind: SidePaneKind| {
            render_split_pane(editor.clone(), self.render_side_header(kind, cx), cx)
        };
        let base = (self.show_base && self.has_base(cx))
            .then(|| side(&split.base.editor, SidePaneKind::Base));
        let ours = side(&split.ours.editor, SidePaneKind::Ours);
        let theirs = side(&split.theirs.editor, SidePaneKind::Theirs);
        let result = render_split_pane(split.result.clone(), self.render_result_header(cx), cx);

        match self.layout {
            SplitLayout::ResultBelow => v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .when_some(base, |row, base| row.child(base).child(Divider::vertical()))
                        .child(ours)
                        .child(Divider::vertical())
                        .child(theirs),
                )
                .child(Divider::horizontal())
                .child(div().flex().flex_1().min_h_0().child(result))
                .into_any_element(),
            SplitLayout::ResultBetween => div()
                .flex()
                .flex_1()
                .min_h_0()
                .when_some(base, |row, base| row.child(base).child(Divider::vertical()))
                .child(ours)
                .child(Divider::vertical())
                .child(result)
                .child(Divider::vertical())
                .child(theirs)
                .into_any_element(),
        }
    }

    fn render_conflict_pane(
        &self,
        label: SharedString,
        text: SharedString,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .min_w_0()
            .flex_1()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        // Render as preformatted text; these are typically
                        // small snippets, not full files; the conflict bodies
                        // come in with whitespace intact and we want the user
                        // to see them exactly.
                        Label::new(text).size(LabelSize::Small).buffer_font(cx),
                    ),
            )
    }

    fn render_conflict(
        &self,
        index: usize,
        total: usize,
        is_selected: bool,
        conflict: &ConflictRegion,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let snapshot = self.buffer.read(cx).snapshot();
        let ours_text: SharedString = snapshot
            .text_for_range(conflict.ours.clone())
            .collect::<String>()
            .into();
        let theirs_text: SharedString = snapshot
            .text_for_range(conflict.theirs.clone())
            .collect::<String>()
            .into();
        // A diff3-style base section in the markers is exact for this region;
        // otherwise fall back to the lines cut from the index's base version.
        let base_text: Option<SharedString> = match &conflict.base {
            Some(range) => Some(
                snapshot
                    .text_for_range(range.clone())
                    .collect::<String>()
                    .into(),
            ),
            None => self
                .card_bases
                .iter()
                .find(|(range, _)| *range == conflict.range)
                .map(|(_, text)| text.clone()),
        };

        let ours_branch = conflict.ours_branch_name.clone();
        let theirs_branch = conflict.theirs_branch_name.clone();

        let ours_range = conflict.ours.clone();
        let theirs_range = conflict.theirs.clone();
        let conflict_for_ours = conflict.clone();
        let conflict_for_theirs = conflict.clone();
        let conflict_for_both = conflict.clone();

        let border_color = if is_selected {
            cx.theme().colors().border_selected
        } else {
            cx.theme().colors().border_variant
        };

        v_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_1()
            .border_color(border_color)
            .rounded_md()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(format!("Conflict {} of {}", index + 1, total))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{ours_branch} ⇄ {theirs_branch}"))
                            .size(LabelSize::Small),
                    ),
            )
            .child({
                let mut row = h_flex().gap_2().items_start();
                if let Some(base) = base_text {
                    // An empty base means both sides added lines here.
                    let label = if base.is_empty() {
                        "Base (empty)"
                    } else {
                        "Base"
                    };
                    row = row.child(self.render_conflict_pane(label.into(), base, cx));
                }
                row.child(self.render_conflict_pane(
                    format!("Ours ({ours_branch})").into(),
                    ours_text,
                    cx,
                ))
                .child(self.render_conflict_pane(
                    format!("Theirs ({theirs_branch})").into(),
                    theirs_text,
                    cx,
                ))
            })
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(("take-ours", index), "Take ours")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_with(&conflict_for_ours, vec![ours_range.clone()], cx);
                            })),
                    )
                    .child(
                        Button::new(("take-theirs", index), "Take theirs")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_with(
                                    &conflict_for_theirs,
                                    vec![theirs_range.clone()],
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new(("take-both", index), "Take both")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let ranges = vec![
                                    conflict_for_both.ours.clone(),
                                    conflict_for_both.theirs.clone(),
                                ];
                                this.resolve_with(&conflict_for_both, ranges, cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for MergeEditorView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.split {
            Some(split) if self.view_mode == MergeViewMode::Split => split.result.focus_handle(cx),
            _ => self.focus_handle.clone(),
        }
    }
}

pub enum MergeEditorViewEvent {
    UpdateTab,
    Close,
}

impl EventEmitter<MergeEditorViewEvent> for MergeEditorView {}

impl Render for MergeEditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.conflict_set.read(cx).snapshot();
        let conflicts: Arc<[ConflictRegion]> = snapshot.conflicts;
        let total = conflicts.len();
        // Clamp selection so a resolved conflict doesn't leave the index past
        // the end (the conflict list shrinks).
        if total == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= total {
            self.selected_index = total - 1;
        }
        let selected_index = self.selected_index;
        let file_label = self.file_label.clone();
        let is_dirty = self.buffer.read(cx).is_dirty();
        let view_mode = self.view_mode;

        let nav_enabled = total > 1;
        let header = self.render_header(
            file_label,
            total,
            selected_index,
            nav_enabled,
            view_mode,
            is_dirty,
            cx,
        );

        let body = if view_mode == MergeViewMode::Split && self.split.is_some() {
            self.render_split_body(cx)
        } else if total == 0 {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new(if self.whole_file_conflict && self.merge_tool {
                        "This file can't be merged line by line. Close this tab and resolve it with your version control tool."
                    } else if self.whole_file_conflict {
                        "This conflict is over the whole file. Resolve it from the Conflicts tab."
                    } else {
                        "No conflicts remaining"
                    })
                    .color(Color::Muted)
                    .size(LabelSize::Small),
                )
                .into_any_element()
        } else {
            let rows = (0..total).map(|ix| {
                let conflict = conflicts[ix].clone();
                self.render_conflict(ix, total, ix == selected_index, &conflict, cx)
            });
            v_flex().p_3().gap_3().children(rows).into_any_element()
        };

        v_flex()
            .key_context("MergeEditorView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(header)
            .child(Divider::horizontal())
            .child(body)
    }
}

impl MergeEditorView {
    #[allow(clippy::too_many_arguments)]
    fn render_header(
        &self,
        file_label: SharedString,
        total: usize,
        selected_index: usize,
        nav_enabled: bool,
        view_mode: MergeViewMode,
        is_dirty: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .px_4()
            .py_2()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new("Merge editor").size(LabelSize::Large))
            .child(
                Label::new(file_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            // Split view has these in the result pane's header instead, and
            // resolves through the side panes' checkboxes.
            .when(view_mode == MergeViewMode::Cards, |header| {
                header
                    .child(
                        Label::new(if total == 0 {
                            "0 conflicts".to_string()
                        } else {
                            format!("{} of {total}", selected_index + 1)
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("merge-prev-conflict", IconName::ChevronLeft)
                            .icon_size(IconSize::Small)
                            .disabled(!nav_enabled)
                            .tooltip(Tooltip::text("Previous conflict"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.select_prev_conflict(window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("merge-next-conflict", IconName::ChevronRight)
                            .icon_size(IconSize::Small)
                            .disabled(!nav_enabled)
                            .tooltip(Tooltip::text("Next conflict"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.select_next_conflict(window, cx);
                            })),
                    )
                    .child(
                        Button::new("take-all-ours", "All ours")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .disabled(total == 0)
                            .tooltip(Tooltip::text("Resolve every conflict in this file as ours"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.resolve_all(ConflictSide::Ours, cx);
                            })),
                    )
                    .child(
                        Button::new("take-all-theirs", "All theirs")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .disabled(total == 0)
                            .tooltip(Tooltip::text(
                                "Resolve every conflict in this file as theirs",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.resolve_all(ConflictSide::Theirs, cx);
                            })),
                    )
            })
            .child(self.render_view_menu(view_mode, cx))
            .child(
                Button::new("edit-manually", "Edit manually")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(
                        "Open the buffer with conflict markers in a regular editor",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_in_editor(window, cx);
                    })),
            )
            .child(
                Button::new("save-buffer", if is_dirty { "Save" } else { "Saved" })
                    .style(if self.merge_tool {
                        ButtonStyle::Subtle
                    } else {
                        ButtonStyle::Filled
                    })
                    .label_size(LabelSize::Small)
                    .disabled(!is_dirty)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_buffer(cx);
                    })),
            )
            .when(self.merge_tool, |header| {
                let can_finish = total == 0 && !self.whole_file_conflict;
                header.child(
                    Button::new("merge-tool-done", "Done")
                        .style(ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .disabled(!can_finish)
                        .tooltip(Tooltip::text(if can_finish {
                            "Save the merged file and hand it back to git"
                        } else {
                            "Resolve every conflict first. Closing this tab instead leaves the file unresolved."
                        }))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.finish_merge_tool(cx);
                        })),
                )
            })
            .into_any_element()
    }
}

impl MergeEditorView {
    /// The view options, collected in one menu so the header keeps room for
    /// the resolve buttons.
    fn render_view_menu(
        &self,
        view_mode: MergeViewMode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.weak_entity();
        let is_split = view_mode == MergeViewMode::Split;
        let result_below = self.layout == SplitLayout::ResultBelow;
        let show_base = self.show_base;
        let has_base = self.has_base(cx);
        let ignore_whitespace = self.ignore_whitespace;
        PopoverMenu::new("merge-editor-view-menu")
            .trigger(
                Button::new("merge-editor-view", "View")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
            )
            .menu(move |window, cx| {
                let view = view.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let toggle = |update: fn(
                        &mut MergeEditorView,
                        &mut Window,
                        &mut Context<MergeEditorView>,
                    )| {
                        let view = view.clone();
                        move |window: &mut Window, cx: &mut App| {
                            view.update(cx, |view, cx| update(view, window, cx))
                                .log_err();
                        }
                    };
                    menu.toggleable_entry(
                        "Split view",
                        is_split,
                        IconPosition::Start,
                        None,
                        toggle(|view, window, cx| view.toggle_view_mode(window, cx)),
                    )
                    .separator()
                    .toggleable_entry_disabled_when(
                        "Result below the two sides",
                        result_below,
                        !is_split,
                        IconPosition::Start,
                        None,
                        toggle(|view, _, cx| view.toggle_layout(cx)),
                    )
                    .toggleable_entry_disabled_when(
                        "Show base",
                        show_base,
                        !is_split || !has_base,
                        IconPosition::Start,
                        None,
                        toggle(|view, window, cx| view.toggle_show_base(window, cx)),
                    )
                    .toggleable_entry_disabled_when(
                        "Ignore whitespace",
                        ignore_whitespace,
                        !is_split,
                        IconPosition::Start,
                        None,
                        toggle(|view, _, cx| view.toggle_ignore_whitespace(cx)),
                    )
                }))
            })
    }

    /// A side pane's header: its color, its name, and a checkbox that takes
    /// that side for every conflict in the file, or releases it.
    fn render_side_header(&self, kind: SidePaneKind, cx: &Context<Self>) -> AnyElement {
        let label = Label::new(self.side_label(kind)).size(LabelSize::Small);
        let (side, color) = match kind {
            SidePaneKind::Base => return label.color(Color::Muted).into_any_element(),
            SidePaneKind::Ours => (
                ConflictSide::Ours,
                cx.theme().colors().version_control_conflict_marker_ours,
            ),
            SidePaneKind::Theirs => (
                ConflictSide::Theirs,
                cx.theme().colors().version_control_conflict_marker_theirs,
            ),
        };
        let state = self.side_take_state(side, cx);
        h_flex()
            .gap_2()
            .child(
                Checkbox::new(("take-whole-side", kind as usize), state)
                    .disabled(self.conflict_count(cx) == 0)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.take_side_everywhere(side, cx);
                    })),
            )
            .child(div().size_2().rounded_xs().bg(color.opacity(1.)))
            .child(label)
            .into_any_element()
    }

    /// The result pane's header, with the conflict counter, the arrows that
    /// step through the conflicts, and Reset.
    fn render_result_header(&self, cx: &Context<Self>) -> AnyElement {
        let total = self.conflict_set.read(cx).snapshot.conflicts.len();
        let position: SharedString = if total == 0 {
            "No conflicts left".into()
        } else {
            let snapshot = self.buffer.read(cx).snapshot();
            let index = self.selected_index.min(total - 1);
            let rows = self
                .conflict_set
                .read(cx)
                .snapshot
                .conflicts
                .get(index)
                .map(|conflict| anchor_rows(&snapshot, &conflict.range));
            match rows {
                Some(rows) => format!(
                    "Conflict {} of {total}, lines {}-{}",
                    index + 1,
                    rows.start + 1,
                    rows.end
                )
                .into(),
                None => format!("Conflict {} of {total}", index + 1).into(),
            }
        };
        h_flex()
            .flex_1()
            .gap_2()
            .child(Label::new("Result").size(LabelSize::Small))
            .child(div().flex_1())
            .child(
                Label::new(position)
                    .size(LabelSize::Small)
                    .color(Color::Accent),
            )
            .child(
                IconButton::new("result-prev-conflict", IconName::ChevronUp)
                    .icon_size(IconSize::Small)
                    .disabled(total < 2)
                    .tooltip(Tooltip::text("Previous conflict"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.select_prev_conflict(window, cx);
                    })),
            )
            .child(
                IconButton::new("result-next-conflict", IconName::ChevronDown)
                    .icon_size(IconSize::Small)
                    .disabled(total < 2)
                    .tooltip(Tooltip::text("Next conflict"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.select_next_conflict(window, cx);
                    })),
            )
            .child(
                Button::new("reset-choices", "Reset")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .disabled(self.choices.is_empty())
                    .tooltip(Tooltip::text(
                        "Put back the conflicts resolved with the checkboxes",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.reset_choices(cx))),
            )
            .into_any_element()
    }

    #[cfg(test)]
    pub(crate) fn buffer_text(&self, cx: &App) -> String {
        self.buffer.read(cx).text()
    }

    fn conflict_count(&self, cx: &App) -> usize {
        self.conflict_set.read(cx).snapshot.conflicts.len() + self.choices.len()
    }

    /// Whether `side` is taken for every conflict, some, or none.
    fn side_take_state(&self, side: ConflictSide, cx: &App) -> ToggleState {
        let taken = self
            .choices
            .iter()
            .filter(|choice| match side {
                ConflictSide::Ours => choice.take_ours,
                ConflictSide::Theirs => choice.take_theirs,
            })
            .count();
        if taken == 0 {
            ToggleState::Unselected
        } else if taken == self.conflict_count(cx) {
            ToggleState::Selected
        } else {
            ToggleState::Indeterminate
        }
    }

    /// Takes `side` for every conflict that doesn't have it yet, or, when
    /// every conflict already has it, releases it everywhere.
    fn take_side_everywhere(&mut self, side: ConflictSide, cx: &mut Context<Self>) {
        let has_side = |choice: &ConflictChoice| match side {
            ConflictSide::Ours => choice.take_ours,
            ConflictSide::Theirs => choice.take_theirs,
        };
        let release = self.side_take_state(side, cx) == ToggleState::Selected;
        let mut targets: Vec<CheckboxTarget> = self
            .choices
            .iter()
            .filter(|choice| has_side(choice) == release)
            .map(|choice| CheckboxTarget::Choice(choice.range.clone()))
            .collect();
        if !release {
            targets.extend(
                self.conflict_set
                    .read(cx)
                    .snapshot
                    .conflicts
                    .iter()
                    .map(|conflict| CheckboxTarget::Conflict(conflict.range.clone())),
            );
        }
        for target in &targets {
            self.toggle_side(target, side, cx);
        }
    }

    /// Puts every conflict resolved by checkbox back to its markers. A choice
    /// whose text was edited by hand is left alone.
    fn reset_choices(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot();
        let mut edits = Vec::new();
        for choice in std::mem::take(&mut self.choices) {
            let expected = choice_text(
                &choice.ours,
                &choice.theirs,
                choice.take_ours,
                choice.take_theirs,
            );
            let current: String = snapshot.text_for_range(choice.range.clone()).collect();
            if expected.as_deref() == Some(current.as_str()) {
                let range =
                    choice.range.start.to_offset(&snapshot)..choice.range.end.to_offset(&snapshot);
                edits.push((range, choice.original));
            }
        }
        if !edits.is_empty() {
            self.buffer
                .update(cx, |buffer, cx| buffer.edit(edits, None, cx));
        }
        cx.notify();
    }

    fn toggle_layout(&mut self, cx: &mut Context<Self>) {
        self.layout = match self.layout {
            SplitLayout::ResultBelow => SplitLayout::ResultBetween,
            SplitLayout::ResultBetween => SplitLayout::ResultBelow,
        };
        cx.notify();
    }
}

impl Item for MergeEditorView {
    type Event = MergeEditorViewEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Merge: {}", self.file_label).into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Merge Editor Opened")
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            MergeEditorViewEvent::UpdateTab => f(ItemEvent::UpdateTab),
            MergeEditorViewEvent::Close => f(ItemEvent::CloseItem),
        }
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffer.read(cx).is_dirty()
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.buffer.read(cx).has_conflict()
    }

    fn can_save(&self, cx: &App) -> bool {
        self.buffer.read(cx).file().is_some()
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let buffer = self.buffer.clone();
        project.update(cx, |project, cx| project.save_buffer(buffer, cx))
    }
}

/// Opens the merge editor for the file in the active editor. Does nothing when
/// the active item isn't an editor over a single file.
pub fn open_merge_editor_for_active_item(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
        return;
    };
    let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
        return;
    };
    let git_store = workspace.project().read(cx).git_store().clone();
    let workspace_handle = cx.weak_entity();
    cx.spawn_in(window, async move |_, cx| {
        let conflict_set = git_store
            .update(cx, |git_store, cx| {
                git_store.open_conflict_set(buffer.clone(), cx)
            })
            .await;
        cx.update(|window, cx| {
            open_merge_editor(workspace_handle, buffer, conflict_set, window, cx);
        })
        .log_err();
    })
    .detach();
}

/// Opens the merge editor for `lathe --merge`, with the three versions read
/// from the files the merge tool was given rather than from the git index.
pub fn open_merge_tool_editor(
    workspace: &mut Workspace,
    buffer: Entity<Buffer>,
    conflict_set: Entity<ConflictSet>,
    stages: ConflictStages,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<MergeEditorView> {
    let project = workspace.project().clone();
    let workspace_weak = workspace.weak_handle();
    let view = cx.new(|cx| {
        let mut view = MergeEditorView::new(
            buffer,
            conflict_set,
            project,
            workspace_weak,
            Some(stages),
            window,
            cx,
        );
        view.merge_tool = true;
        view
    });
    workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
    view
}

/// Open the merge editor as a workspace item for the given buffer. The buffer
/// must already have its conflicts populated (which happens automatically the
/// first time an `Editor` displays it).
pub fn open_merge_editor(
    workspace: WeakEntity<Workspace>,
    buffer: Entity<Buffer>,
    conflict_set: Entity<ConflictSet>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            let project = workspace.project().clone();
            let workspace_weak = workspace.weak_handle();
            let view = cx.new(|cx| {
                MergeEditorView::new(
                    buffer,
                    conflict_set,
                    project,
                    workspace_weak,
                    None,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use git::{
        repository::repo_path,
        status::{FileStatus, UnmergedStatus, UnmergedStatusCode},
    };
    use gpui::{TestAppContext, VisualTestContext};
    use multi_buffer::ToPoint as _;
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use std::{cell::Cell, path::Path, rc::Rc};
    use theme::LoadThemes;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    const BASE: &str = "\
fn main() {
    let x = 0;
    println!(\"{x}\");
}
";

    const RESULT: &str = "\
fn main() {
<<<<<<< HEAD
    let x = 1;
=======
    let x = 2;
    let y = 3;
>>>>>>> feature
    println!(\"{x}\");
}
";
    const OURS: &str = "\
fn main() {
    let x = 1;
    println!(\"{x}\");
}
";
    const THEIRS: &str = "\
fn main() {
    let x = 2;
    let y = 3;
    println!(\"{x}\");
}
";

    #[test]
    fn test_base_snippet_for_conflict_without_base_markers() {
        let hunks = language::line_diff(RESULT, BASE);
        // The conflict spans the marker lines, rows 1 through 6.
        let rows = map_rows_outward(&hunks, 1..7);
        assert_eq!(text_for_rows(BASE, rows), "    let x = 0;\n");
    }

    /// `lathe --merge` can be handed a file outside any git repository, or
    /// one git doesn't report unmerged. The markers must still be parsed, and
    /// "Done" saves the result and closes the tab, which ends the wait.
    #[gpui::test]
    async fn test_merge_tool_mode_outside_a_repository(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/scratch"), json!({ "main.rs": RESULT }))
            .await;
        let project = Project::test(fs.clone(), [Path::new(path!("/scratch"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        cx.run_until_parked();

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/scratch/main.rs"), cx)
            })
            .await
            .unwrap();
        let git_store = project.read_with(cx, |project, _| project.git_store().clone());
        let conflict_set = git_store
            .update(cx, |git_store, cx| {
                git_store.open_merge_tool_conflict_set(buffer.clone(), cx)
            })
            .await;
        assert_eq!(
            conflict_set.read_with(cx, |set, _| set.snapshot.conflicts.len()),
            1,
            "markers are parsed even though no repository reports a conflict"
        );

        let view = workspace.update_in(cx, |workspace, window, cx| {
            open_merge_tool_editor(
                workspace,
                buffer.clone(),
                conflict_set,
                ConflictStages {
                    base: Some(BASE.into()),
                    ours: Some(OURS.into()),
                    theirs: Some(THEIRS.into()),
                    binary: false,
                },
                window,
                cx,
            )
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let split = view.split.as_ref().expect("split view should be built");
            assert_eq!(split.ours.buffer.read(cx).text(), OURS);
            assert_eq!(split.theirs.buffer.read(cx).text(), THEIRS);
            assert_eq!(split.base.buffer.read(cx).text(), BASE);
        });

        let released = Rc::new(Cell::new(false));
        let _subscription = cx.update(|_, cx| {
            let released = released.clone();
            cx.observe_release(&view, move |_, _| released.set(true))
        });
        view.update(cx, |view, cx| {
            view.resolve_all(ConflictSide::Theirs, cx);
            view.finish_merge_tool(cx);
        });
        drop(view);
        cx.run_until_parked();

        assert_eq!(
            fs.load(Path::new(path!("/scratch/main.rs"))).await.unwrap(),
            THEIRS,
            "Done saves the resolved file"
        );
        assert!(released.get(), "Done closes the merge editor's tab");
    }

    /// In a freshly opened window the file's language can still be loading
    /// when the side panes are built; they must take it once it arrives.
    #[gpui::test]
    async fn test_side_panes_take_language_set_later(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/scratch"), json!({ "main.rs": RESULT }))
            .await;
        let project = Project::test(fs.clone(), [Path::new(path!("/scratch"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        cx.run_until_parked();

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/scratch/main.rs"), cx)
            })
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| buffer.set_language(None, cx));
        let git_store = project.read_with(cx, |project, _| project.git_store().clone());
        let conflict_set = git_store
            .update(cx, |git_store, cx| {
                git_store.open_merge_tool_conflict_set(buffer.clone(), cx)
            })
            .await;
        let view = workspace.update_in(cx, |workspace, window, cx| {
            open_merge_tool_editor(
                workspace,
                buffer.clone(),
                conflict_set,
                ConflictStages {
                    base: Some(BASE.into()),
                    ours: Some(OURS.into()),
                    theirs: Some(THEIRS.into()),
                    binary: false,
                },
                window,
                cx,
            )
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let split = view.split.as_ref().expect("split view should be built");
            assert!(split.ours.buffer.read(cx).language().is_none());
        });

        let rust = language::rust_lang();
        buffer.update(cx, |buffer, cx| buffer.set_language(Some(rust.clone()), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let split = view.split.as_ref().expect("split view should be built");
            for pane in split.side_panes() {
                assert_eq!(
                    pane.buffer.read(cx).language().map(|language| language.name()),
                    Some(rust.name()),
                    "every side pane should take the file's language"
                );
            }
        });
    }

    #[gpui::test]
    async fn test_merge_editor_uses_index_stages(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(path!("/project"), json!({ ".git": {}, "main.rs": RESULT }))
            .await;
        let dot_git = Path::new(path!("/project/.git"));
        fs.set_status_for_repo(
            dot_git,
            &[(
                "main.rs",
                FileStatus::Unmerged(UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                }),
            )],
        );
        fs.with_git_state(dot_git, true, |state| {
            state.conflict_stages.insert(
                repo_path("main.rs"),
                ConflictStages {
                    base: Some(BASE.into()),
                    ours: Some(OURS.into()),
                    theirs: Some(THEIRS.into()),
                    binary: false,
                },
            );
        })
        .unwrap();

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        cx.run_until_parked();

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/main.rs"), cx)
            })
            .await
            .unwrap();
        let git_store = project.read_with(cx, |project, _| project.git_store().clone());
        let conflict_set = git_store
            .update(cx, |git_store, cx| {
                git_store.open_conflict_set(buffer.clone(), cx)
            })
            .await;
        let view = workspace.update_in(cx, |workspace, window, cx| {
            let view = cx.new(|cx| {
                MergeEditorView::new(
                    buffer.clone(),
                    conflict_set,
                    project.clone(),
                    workspace.weak_handle(),
                    None,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        });
        cx.run_until_parked();
        cx.executor().advance_clock(SPLIT_REFRESH_DEBOUNCE);
        cx.run_until_parked();

        // The markers have no base section, so the card's base comes from the
        // index's base version.
        view.read_with(cx, |view, cx| {
            assert_eq!(view.conflict_set.read(cx).snapshot.conflicts.len(), 1);
            assert!(view.stages.is_some(), "stages should load from the index");
            let card_bases: Vec<_> = view
                .card_bases
                .iter()
                .map(|(_, text)| text.to_string())
                .collect();
            assert_eq!(card_bases, ["    let x = 0;\n"]);
        });

        // Split view is the default, so it was built with the view.
        view.read_with(cx, |view, cx| {
            let split = view.split.as_ref().expect("split view should be built");
            assert_eq!(split.base.buffer.read(cx).text(), BASE);
            assert_eq!(split.ours.buffer.read(cx).text(), OURS);
            assert_eq!(split.theirs.buffer.read(cx).text(), THEIRS);
            assert_eq!(
                split.result.read(cx).buffer().read(cx).as_singleton(),
                Some(buffer.clone()),
                "the result pane should edit the file's own buffer",
            );
            assert!(!view.is_dirty(cx));

            // Outside the conflict both sides match the result, so the only
            // colored rows are each side's version of the conflict.
            for pane in split.side_panes() {
                assert_eq!(pane.changes, vec![]);
                let conflict_rows: Vec<_> = pane
                    .editor
                    .read(cx)
                    .highlighted_rows::<ConflictRows>(cx)
                    .collect();
                assert_eq!(conflict_rows.len(), 1);
            }
            let ours_rows: Vec<_> = split
                .ours
                .editor
                .read(cx)
                .highlighted_rows::<ConflictRows>(cx)
                .map(|(range, _)| {
                    let snapshot = split.ours.editor.read(cx).buffer().read(cx).snapshot(cx);
                    range.start.to_point(&snapshot).row..range.end.to_point(&snapshot).row
                })
                .collect();
            assert_eq!(ours_rows, vec![1..2]);
        });

        // The side panes' checkboxes resolve a conflict, combine both sides,
        // and can be unchecked back to the original markers.
        let text = |cx: &mut VisualTestContext| buffer.read_with(cx, |buffer, _| buffer.text());
        let conflict_range = view.read_with(cx, |view, cx| {
            view.conflict_set.read(cx).snapshot.conflicts[0]
                .range
                .clone()
        });
        let choice_range = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| {
                CheckboxTarget::Choice(view.choices[0].range.clone())
            })
        };
        view.update(cx, |view, cx| {
            view.toggle_side(
                &CheckboxTarget::Conflict(conflict_range),
                ConflictSide::Ours,
                cx,
            )
        });
        assert_eq!(text(cx), OURS);
        let target = choice_range(cx);
        view.update(cx, |view, cx| {
            view.toggle_side(&target, ConflictSide::Theirs, cx)
        });
        assert_eq!(
            text(cx),
            "fn main() {\n    let x = 1;\n    let x = 2;\n    let y = 3;\n    println!(\"{x}\");\n}\n"
        );
        let target = choice_range(cx);
        view.update(cx, |view, cx| {
            view.toggle_side(&target, ConflictSide::Ours, cx)
        });
        assert_eq!(text(cx), THEIRS);
        let target = choice_range(cx);
        view.update(cx, |view, cx| {
            view.toggle_side(&target, ConflictSide::Theirs, cx)
        });
        assert_eq!(text(cx), RESULT, "unchecking both puts the markers back");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.choices.is_empty());
            assert_eq!(view.conflict_set.read(cx).snapshot.conflicts.len(), 1);
            assert_eq!(
                view.side_take_state(ConflictSide::Ours, cx),
                ToggleState::Unselected
            );
        });

        view.update(cx, |view, cx| {
            view.take_side_everywhere(ConflictSide::Ours, cx)
        });
        assert_eq!(text(cx), OURS);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.side_take_state(ConflictSide::Ours, cx),
                ToggleState::Selected
            );
        });
        view.update(cx, |view, cx| view.reset_choices(cx));
        assert_eq!(text(cx), RESULT);
        cx.run_until_parked();

        view.update(cx, |view, cx| view.resolve_all(ConflictSide::Theirs, cx));
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), THEIRS);
        assert!(view.read_with(cx, |view, cx| view.is_dirty(cx)));

        let save = view.update_in(cx, |view, window, cx| {
            view.save(
                SaveOptions {
                    format: false,
                    force_format: false,
                    autosave: false,
                },
                project.clone(),
                window,
                cx,
            )
        });
        save.await.unwrap();
        cx.run_until_parked();
        assert_eq!(
            fs.load(Path::new(path!("/project/main.rs"))).await.unwrap(),
            THEIRS
        );
        assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));
    }

    #[test]
    fn test_line_hunks_ignoring_whitespace() {
        let result = "fn f() {\n    call(a,b);\n}\n";
        let side = "fn f() {\n  call(a, b);  \n}\n";
        assert_eq!(line_hunks(result, side, false), vec![(1..2, 1..2)]);
        assert_eq!(line_hunks(result, side, true), vec![]);
        // Real changes still show, on the same rows as without the option.
        let side = "fn f() {\n  call(a, c);\n}\n";
        assert_eq!(line_hunks(result, side, true), vec![(1..2, 1..2)]);
    }

    #[test]
    fn test_side_changes() {
        let result = "one\nshared\nold value\ngap\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> b\n";
        let side = "one\nadded\nshared\nnew value\ngap\nx\n";
        let hunks = line_hunks(result, side, false);
        // This side's version of the conflict is the "x" on row 5.
        let (changes, word_ranges) = side_changes(result, side, &hunks, &[5..6], false);
        assert_eq!(
            changes,
            vec![
                SideChange {
                    rows: 1..2,
                    kind: ChangeKind::Added,
                },
                SideChange {
                    rows: 3..4,
                    kind: ChangeKind::Modified,
                },
            ],
        );
        let words: Vec<&str> = word_ranges
            .iter()
            .map(|range| &side[range.clone()])
            .collect();
        assert_eq!(words, ["new"]);
    }

    #[test]
    fn test_side_changes_next_to_a_conflict() {
        // The changed line touches the conflict, so the diff puts both in one
        // block; the change keeps its color and the conflict rows don't get it.
        let result = "one\nold value\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> b\n";
        let side = "one\nnew value\nx\n";
        let hunks = line_hunks(result, side, false);
        let (changes, _) = side_changes(result, side, &hunks, &[2..3], false);
        assert_eq!(
            changes,
            vec![SideChange {
                rows: 1..2,
                kind: ChangeKind::Modified,
            }],
        );
    }

    #[test]
    fn test_prepare_stages_decodes_in_the_buffers_encoding() {
        let latin1 = b"caf\xe9\r\n".to_vec();
        let stages = ConflictStages {
            base: None,
            ours: Some(latin1.clone()),
            theirs: Some(latin1),
            binary: false,
        };
        let texts = prepare_stages(&stages, encoding_rs::WINDOWS_1252).unwrap();
        assert_eq!(
            *texts,
            StageTexts {
                base: None,
                ours: Some("caf\u{e9}\n".into()),
                theirs: Some("caf\u{e9}\n".into()),
            }
        );
        let binary = ConflictStages {
            binary: true,
            ..stages
        };
        assert_eq!(prepare_stages(&binary, encoding_rs::UTF_8), None);
    }

    #[test]
    fn test_side_changes_skips_word_diffs_for_long_blocks() {
        let long = "x".repeat(MAX_WORD_DIFF_BYTES);
        let result = format!("{long} old\n");
        let side = format!("{long} new\n");
        let hunks = line_hunks(&result, &side, false);
        let (changes, word_ranges) = side_changes(&result, &side, &hunks, &[], false);
        assert_eq!(changes.len(), 1, "the row is still colored");
        assert_eq!(word_ranges, Vec::<Range<usize>>::new());
    }

    #[test]
    fn test_choice_text() {
        assert_eq!(choice_text("a\n", "b\n", false, false), None);
        assert_eq!(
            choice_text("a\n", "b\n", true, false).as_deref(),
            Some("a\n")
        );
        assert_eq!(
            choice_text("a\n", "b\n", false, true).as_deref(),
            Some("b\n")
        );
        assert_eq!(
            choice_text("a\n", "b\n", true, true).as_deref(),
            Some("a\nb\n")
        );
    }

    #[test]
    fn test_subtract_rows() {
        assert_eq!(subtract_rows(0..10, &[2..4, 6..7]), vec![0..2, 4..6, 7..10]);
        assert_eq!(subtract_rows(3..5, &[0..10]), Vec::<Range<u32>>::new());
        assert_eq!(subtract_rows(3..5, &[4..4]), vec![3..5]);
    }

    #[test]
    fn test_side_changes_ignoring_whitespace_skips_whitespace_words() {
        let result = "let total = add(a,b) + 1;\n";
        let side = "let total = add(a, b) + 2;\n";
        let hunks = line_hunks(result, side, true);
        let (_, word_ranges) = side_changes(result, side, &hunks, &[], true);
        let words: Vec<&str> = word_ranges
            .iter()
            .map(|range| &side[range.clone()])
            .collect();
        assert_eq!(words, ["2"]);
        let hunks = line_hunks(result, side, false);
        let (_, word_ranges) = side_changes(result, side, &hunks, &[], false);
        assert!(
            word_ranges.len() > 1,
            "the added space counts when not ignoring"
        );
    }

    #[test]
    fn test_map_rows_outward() {
        // One hunk replacing rows 2..5 with 2..3, after an insertion at 0.
        let hunks = vec![(0..0, 0..2), (2..5, 4..5)];
        assert_eq!(map_rows_outward(&hunks, 2..5), 4..5);
        // Unchanged lines at the edges of the range map one to one.
        assert_eq!(map_rows_outward(&hunks, 1..6), 3..6);
        // A range with no hunks maps by the offset of the hunks before it.
        assert_eq!(map_rows_outward(&hunks, 6..7), 6..6);
        // A range where both sides added lines has no base lines at all.
        let hunks = vec![(3..6, 3..3)];
        assert_eq!(map_rows_outward(&hunks, 3..6), 3..3);
    }

    #[test]
    fn test_map_row_between_result_and_sides() {
        let ours = language::line_diff(RESULT, OURS);
        let theirs = language::line_diff(RESULT, THEIRS);

        // Lines outside the conflict line up exactly in both directions.
        assert_eq!(map_row(&ours, 0.0, false), 0.0);
        assert_eq!(map_row(&ours, 7.0, false), 2.0);
        assert_eq!(map_row(&theirs, 7.0, false), 3.0);
        assert_eq!(map_row(&ours, 2.0, true), 7.0);
        assert_eq!(map_row(&theirs, 3.0, true), 7.0);

        // Each side's conflict content maps onto its own lines.
        assert_eq!(map_row(&ours, 2.0, false), 1.0);
        assert_eq!(map_row(&theirs, 4.0, false), 1.0);
        assert_eq!(map_row(&theirs, 5.0, false), 2.0);

        // Marker lines collapse onto the nearest content row.
        assert_eq!(map_row(&ours, 1.0, false), 1.0);
        assert_eq!(map_row(&ours, 3.0, false), 2.0);
    }
}
