# Jupyter notebooks in Lathe: current state and roadmap

> **Status.** Milestone 1 is implemented on top of `3de5beed4e`, and the
> Milestone 2 is implemented: code cells are excerpts over one shared,
> project-registered buffer. The sections below
> marked **Done** describe the bug as it was; see "Milestone 1: what shipped" at the
> end for what changed. Everything else is still open.

Assessment of `crates/repl` as of `3de5beed4e`. The complaint that "notebook support in Zed
is not good" is accurate, and the reasons are specific rather than diffuse. Everything below
is grounded in the code, with file and line references.

## What actually exists today

Two separate features share the `repl` crate:

1. **Inline REPL** (`repl_editor.rs`, `session.rs`) for running code blocks in a normal
   `.py`/`.md`/`.rs` buffer against a Jupyter kernel. This part works reasonably well and is
   the feature `docs/src/repl.md` documents.
2. **Notebook editor** (`notebook/notebook_ui.rs`, `notebook/cell.rs`) for editing `.ipynb`
   files. Roughly 3,700 lines, off by default, and unfinished. It is gated three ways in
   `notebook_ui.rs:87` (`notebook_editor_enabled`):
   - `editor.notebook.notebook_enabled` setting, default `false`
     (`assets/settings/default.json:2804`)
   - the `NotebookFeatureFlag` (`crates/feature_flags/src/flags.rs:3`), which is defined but
     referenced nowhere else
   - the `LOCAL_NOTEBOOK_DEV` environment variable

The file opens with `#![allow(unused, dead_code)]` (`notebook_ui.rs:1`), which is a fair
summary of its maturity. Upstream has kept it dark; the last meaningful commit was
`055983572d repl: Put notebook editor behind setting`.

## The gaps, in priority order

### P0. Cell editors have no language intelligence

`CodeCell::new` (`cell.rs:686-688`) builds its editor from a detached local buffer:

```rust
let buffer = cx.new(|cx| Buffer::local(source.clone(), cx));
let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
let editor = cx.new(|cx| Editor::new(EditorMode::Full { .. }, multi_buffer, None, window, cx));
```

The third argument to `Editor::new` is `Option<Entity<Project>>`, and it is `None`. The buffer
is `Buffer::local`, not a project buffer. Consequences, all of them things users notice within
thirty seconds:

- no completions, hover, go-to-definition, signature help, or rename
- no diagnostics (a syntax error shows nothing until you run the cell)
- no formatting, no code actions
- no edit predictions and no inline AI assist
- the buffer is invisible to the assistant panel and to `/file` style context

Syntax highlighting works only because `_language_task` sets the language on the buffer by
hand (`cell.rs:721`).

This is the single biggest quality gap and the one that makes the editor feel like a toy next
to VS Code. Everything else on this list is smaller.

**Fix:** register each cell's source as a real project buffer so the existing LSP plumbing
applies. Two viable shapes:

- **Virtual per-cell buffers.** Give the project a synthetic path per cell
  (`notebook.ipynb#cell-<id>.py`), open it through `Project::open_buffer`, and pass the project
  into `Editor::new`. Simplest, and each cell gets its own LSP document. Downside: pyright and
  friends see each cell as an isolated module, so names defined in cell 1 are unresolved in
  cell 2. This is the same tradeoff every naive notebook LSP integration makes.
- **Concatenated shadow document (recommended).** Maintain one synthetic `.py` buffer that is
  the concatenation of all code cells, with a line-offset map back to each cell. Send that to
  the language server and translate positions on the way in and out. This is what the LSP spec's
  notebook document support (`notebookDocument/didOpen`, `textDocument/didChange` on cells) was
  designed for, and it is what VS Code and `jupyterlab-lsp` do. Cross-cell references then
  resolve correctly.

The right long-term answer is to implement LSP notebook document sync in `crates/lsp` +
`crates/project` and have the notebook editor drive it. That is a large piece of work, so the
pragmatic sequencing is: ship virtual per-cell buffers first (unblocks completions and
diagnostics within a cell, which is most of the perceived value), then upgrade to the
concatenated model.

### P0. Output rendering falls off a cliff (partly done)

`Output::new` (`outputs.rs:398-441`) handles exactly seven MIME types, ranked in
`rank_mime_type` (`outputs.rs:68`): `DataTable`, `Html`, `Json`, `Png`, `Jpeg`, `Markdown`,
`Plain`. Everything else hits:

```rust
// Any other media types are not supported
_ => Output::Message("Unsupported media type".to_string()),
```

So the following produce the literal string "Unsupported media type":

| MIME type | What breaks |
|---|---|
| `image/svg+xml` | matplotlib with the svg backend, graphviz, most diagram libraries |
| `text/latex` | SymPy, any symbolic math output |
| `application/vnd.jupyter.widget-view+json` | every `ipywidgets` slider, dropdown, progress bar |
| `application/pdf` | LaTeX/report output |
| `application/vnd.vegalite.v5+json` | Altair, Vega-Lite |
| `application/vnd.plotly.v1+json` | Plotly |

Worse, `text/html` is ranked *above* `image/png` (6 vs 4), and HTML is handled by converting
it to markdown (`outputs/html.rs:11`, `html_to_markdown`). Plotly, Bokeh, and Altair all emit
HTML that embeds a `<script>` block; `WebpageChromeRemover` strips scripts, so the user gets an
empty or garbled markdown blob instead of the chart, *even when the kernel also sent a
perfectly good PNG fallback*.

**Fixes, cheapest first:**

1. Add `image/svg+xml` rendering. Lathe already has `crates/svg_preview` and GPUI can render
   SVG. Rank it alongside PNG. Low effort, high visible payoff.
2. Add `text/latex`. Rank above markdown. Either a real math renderer or, as a stopgap, render
   the raw LaTeX in a monospace block, which is still better than "Unsupported media type".
3. Demote `text/html` below `image/png` when the HTML contains a `<script>` tag, so
   script-driven visualizations fall back to the PNG the kernel already provided.
4. For genuinely interactive HTML (Plotly, Bokeh, Vega), render it in a real web view rather
   than markdown. This needs a webview element in GPUI, which Zed has historically avoided.
   Scope it as its own project; it is the gateway to widgets as well.
5. `ipywidgets` requires implementing the Jupyter widget comm protocol (`comm_open`,
   `comm_msg` on the kernel's shell/iopub channels) plus a front end for the widget models.
   This is the largest single item on the list. Defer, but note that "no widgets" is what
   people usually mean by "Jupyter support is bad."

### P1. Save round-trip mutates every file it touches (done)

`CodeCell::to_nbformat_cell` (`cell.rs:769-770`):

```rust
let source = self.current_source(cx);
let source_lines: Vec<String> = source.lines().map(|l| format!("{}\n", l)).collect();
```

`str::lines()` drops the information about whether the input ended in a newline, and this
unconditionally appends `\n` to every line including the last. The nbformat convention is that
the final entry of a `source` array carries no trailing newline. Net effect: opening a notebook
written by Jupyter and saving it without edits rewrites the `source` array of every cell, which
shows up as a whole-file diff in git. It also silently normalizes `\r\n`.

**Fix:** split preserving terminators (`split_inclusive('\n')`), and add a round-trip test that
asserts `parse -> serialize` is byte-identical for a corpus of notebooks produced by JupyterLab,
VS Code, Colab, and Papermill. This is a small change with an outsized effect on whether the
editor is usable in a team repo.

While in there, check that unknown top-level and cell-level metadata keys survive the round
trip. `NotebookItem` (`notebook_ui.rs:1662`) should preserve `widgets` metadata, cell
`attachments`, and per-cell `id` stability rather than regenerating UUIDs
(`notebook_ui.rs:1730` already flags this as an open decision).

### P1. The item is missing standard workspace integrations (search done)

From `impl Item for NotebookEditor` (`notebook_ui.rs:1851`):

```rust
fn show_toolbar(&self) -> bool { false }
// TODO
fn pixel_position_of_cursor(&self, _: &App) -> Option<Point<Pixels>> { None }
// TODO
fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> { None }
// TODO
fn set_nav_history(&mut self, _: workspace::ItemNavHistory, ..) { }
```

So a notebook has: no buffer search (`cmd-f` does nothing), no project search results, no
breadcrumbs, no outline panel, no go-back/go-forward, no cursor position for popovers. Each is
individually modest work; together they are most of what makes the editor feel unfinished.

`as_searchable` is the one users hit first. Implementing `SearchableItem` over the cell list
(delegating to each cell's editor and mapping matches to a flat index) is a well-contained task.

An outline that lists markdown headings and top-level `def`/`class` per cell would also give
notebooks the `cmd-shift-o` behavior people expect, and it composes with the shadow-document
work in P0.

### P2. Kernel lifecycle and environment selection

`ReplStore` (`repl_store.rs`) discovers kernelspecs and `PythonEnvKernelSpecification`. Worth
auditing against what people actually do:

- auto-select the kernel from `.venv`, Poetry, uv, conda, and pixi environments in the
  worktree, without a manual pick per notebook
- persist the kernel choice in notebook metadata (`kernelspec`), which is where Jupyter
  records it, rather than only in `JupyterSettings::kernel_selections` keyed by worktree
- a visible, non-modal kernel status (idle / busy / dead) and a one-click restart-and-run-all
- run queue semantics: `RunAll` should queue cells and stop on error, matching Jupyter

### P2. Notebook-aware diffs

`.ipynb` is JSON, so `git diff` on a notebook is unreadable, and merge conflicts are close to
unresolvable. Lathe already owns `crates/buffer_diff` and `crates/git_ui`. A notebook-aware diff
view (cell-level, outputs collapsed by default) would be a genuine differentiator, since neither
VS Code nor JupyterLab does this well without `nbdime`.

A cheaper first step: a "clear all outputs on save" option, and a command to strip outputs, so
teams can keep notebooks diffable at all.

### P3. Remote and SSH notebooks

`kernels/remote_kernels.rs`, `ssh_kernel.rs`, and `wsl_kernel.rs` exist. Whether the notebook
editor path exercises them is untested. Given Lathe's remote-development story, a notebook on a
remote worktree binding to a remote kernel should be a first-class supported case, with an
explicit test.

## Suggested sequencing

The ordering below front-loads the changes with the best visible-improvement-per-line ratio.

**Milestone 1 (make it honest)**, small, self-contained, ships the editor out of "experimental":

- fix the save round-trip (`cell.rs:769`) plus round-trip tests
- SVG and LaTeX output renderers; demote script-bearing HTML below PNG
- implement `as_searchable`
- remove `#![allow(unused, dead_code)]` and clean up what it hides

**Milestone 2 (make it a real editor)**:

- virtual project buffers per cell, so completions and diagnostics work
- outline, breadcrumbs, nav history, cursor position
- kernel auto-selection from the worktree's Python environment, persisted to notebook metadata

**Milestone 3 (make it competitive)**:

- LSP notebook document sync with a concatenated shadow document, for cross-cell resolution
- webview-backed rendering for interactive HTML outputs
- notebook-aware diff view in `git_ui`

**Milestone 4 (parity)**:

- `ipywidgets` comm protocol and widget front end

Milestone 1 is a few days of work and would already move the experience a long way. Milestone 2
is where "not good" stops being a fair description.

## Doing both: the cell model as a shared spine

The notebook-editor and jupytext routes are not actually a fork in the road, and the crate is
already structured for both. What is shared today:

| Component | Lines | Used by |
|---|---|---|
| `kernels/` (native, ssh, wsl, remote) | ~1,400 | both |
| `repl_store.rs` (kernelspec discovery, selection) | 441 | both |
| `outputs.rs` + `outputs/` (all MIME rendering) | ~1,900 | both |
| `components/` (kernel picker) | ~300 | both |
| `repl_editor.rs` + `session.rs` | ~2,100 | inline REPL only |
| `notebook/` | ~3,700 | notebook editor only |

`KernelSession` (`kernels/mod.rs:205`) is the seam: both `Session` (`session.rs:978`) and
`NotebookEditor` (`notebook_ui.rs:2041`) implement it, so kernel lifecycle and message routing
are written once. Every P0 output fix pays for both views on the same commit.

Half the jupytext story also already ships: `jupytext_cells` (`repl_editor.rs:515`) parses
`# %%` markers, comment-prefix-aware, so running cells in a plain `.py` works today.

### The one missing piece serves both goals

What is absent is a bidirectional cell model mapping `.ipynb` to percent-format text. Build that
and it is the same artifact as the concatenated shadow document from P0. One piece of work,
four payoffs:

1. **LSP in notebook cells** (P0), the shadow document *is* the percent-format `.py`
2. **Jupytext-style editing**, expose that document as an editable view
3. **Notebook-aware diffs** (P2), diff the percent form, not the JSON
4. **Outline and search** (P1), both become operations over one flat text document

In Zed's architecture this is unusually cheap. A `MultiBuffer` excerpt is already a live,
bidirectional window into a shared `Buffer`. So instead of `cell.rs:686`:

```rust
// today: each cell is an island
let buffer = cx.new(|cx| Buffer::local(source.clone(), cx));
let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
let editor = cx.new(|cx| Editor::new(.., multi_buffer, None, window, cx));
```

each cell editor becomes a single-excerpt `MultiBuffer` over one shared, project-registered
shadow buffer. Edits propagate to the shadow buffer with no sync code, because it is the same
entity, and the language server sees an ordinary Python document. This needs a spike to confirm
excerpt-backed editors behave under `SizingBehavior::SizeByContent`, but the primitives are
all there.

### Derived view, not paired files

The one design decision worth getting right: the `.py` should be a **derived view of the
notebook, not a second file on disk**.

Jupytext's paired-file model is its weakest part. Two files, two save paths, one source of
truth, and the classic failure is editing the `.py`, saving, and silently discarding outputs
that only existed in the `.ipynb`. Stale pairs and "which side wins" are its standard support
load.

Lathe owns the editor, so it can skip that entirely: keep one file on disk (`.ipynb`, outputs
intact), and let the percent-format text be a view onto it. Editing either view edits the same
in-memory document. Nothing to reconcile, no outputs lost.

Offer a genuine paired file on disk only as an opt-in for teams who want the `.py` in git, and
treat the `.ipynb` as the authority when both exist.

### What this costs in UX

The real cost of both is not engineering, it is the ambiguity of two ways to open one file.
Budget for:

- a default view per file type (`.ipynb` opens as a notebook; `.py` with `# %%` opens as a normal
  buffer with the inline REPL, both already the case)
- a `notebook: toggle view` action and an "Open as..." entry, so switching is one keystroke and
  the choice is remembered per file
- a single decision about what `cmd-f`, the outline, and project search operate on, so results
  are consistent across views

### Revised sequencing

Milestone 1 is unchanged and view-agnostic: the save round-trip fix, SVG/LaTeX renderers, and
demoting script-bearing HTML all sit in shared code.

Milestones 2 and 3 collapse. Rather than "project buffers per cell" first and "LSP notebook
sync" later, build the shared cell model once and take the notebook LSP support and the
jupytext view out of it together. That removes the throwaway intermediate step of virtual
per-cell buffers, which was only ever a stopgap for cross-cell resolution.

Doing both is therefore cheaper than doing the notebook editor properly and then bolting
pairing on afterwards, provided the cell model is built as the foundation rather than as an
export feature.

## Milestone 1: what shipped

All of the following is in `crates/repl`, verified with `cargo test -p repl --lib`
(43 passing) and `./script/clippy -p repl` (clean, and the crate now compiles with no
warnings).

### Save no longer destroys notebooks

Three separate data-loss bugs in one round trip, all fixed:

1. **Source lines.** `str::lines` was used to split cell source and every line was
   re-joined with a trailing `\n`, inventing a newline the file never had.
   `source_to_nbformat_lines` in `cell.rs` now uses `split_inclusive('\n')`. Applied to
   code, markdown, and raw cells.
2. **Outputs.** `Output::to_nbformat` rebuilt an output from whatever view the editor
   had rendered, returning `None` for images, tables, markdown and JSON, so
   `outputs_to_nbformat` silently dropped them. A new `CellOutput` in `cell.rs` pairs
   each rendered view with the `nbformat` output it came from, and saving replays the
   original. Media types the editor cannot render at all (widgets, Vega specs) now
   survive a save, as do sibling representations in a bundle that the editor chose not
   to display. The lossy `Output::to_nbformat` is deleted.
3. **Markdown attachments.** Parsed, then written back as `None`, breaking every
   embedded image. `MarkdownCell` now carries them through.

Stream outputs also keep their real `stdout`/`stderr` name instead of being forced to
`stdout`.

Covered by three tests in `notebook_ui.rs` against a fixture holding a PNG with a
`text/plain` sibling, a widget view, a stderr stream, markdown attachments, and sources
with and without trailing newlines. `test_round_trip_preserves_sources_and_outputs`
asserts every cell serializes back to exactly what was parsed.

### Outputs that used to read "Unsupported media type"

`rank_mime_type` in `outputs.rs` was extended and `Output::new` grew renderers:

- **SVG** (`image/svg+xml`) renders via `ImageView::from_svg`, which uses GPUI's
  `svg_renderer` the same way `crates/svg_preview` does. This covers matplotlib's svg
  backend, graphviz, and anything using `_repr_svg_`.
- **GIF** (`image/gif`) now routes to `ImageView`, which already decoded it.
- **LaTeX** (`text/latex`) renders as monospace text. Not typeset, but SymPy output is
  now readable rather than an error string.
- **Script-driven HTML is demoted below images.** Plotly, Bokeh and Altair emit HTML
  whose content lives in a `<script>` block; the markdown converter strips scripts and
  left an empty frame, *even when the kernel had also sent a usable PNG*.
  `html_needs_scripting` detects this and ranks such HTML below `image/png`, so the
  static fallback wins. Static HTML (pandas tables) keeps its previous top ranking.

### Buffer search

`as_searchable` returned `None`, so `cmd-f` did nothing in a notebook. `NotebookEditor`
now implements `SearchableItem`, delegating to each cell's editor and tagging matches
with their cell (`NotebookSearchMatch`). Matches are gathered in cell order so
next/previous reads top to bottom; activating one selects and scrolls to its cell before
activating within it. Replace and select-all-matches delegate per cell. The
selection-scoped options are reported unsupported, since cells are separate buffers with
no single selection to scope to.

### Warning cleanup

`#![allow(unused, dead_code)]` is gone from `notebook_ui.rs`, along with what it was
hiding: unused imports, unused variables, three unused layout constants, the never-read
`remote_id` field, and `move_to_next_cell` (superseded by `advance_in_command_mode`,
which is what `RunAndAdvance` actually calls).

### Not done in this milestone

Everything under P0 "Cell editors have no language intelligence" is untouched, as are
outline, breadcrumbs, nav history, cursor position, kernel auto-selection, and
notebook-aware diffs. Interactive HTML and `ipywidgets` still need a webview and the
comm protocol respectively; they now round-trip through a save, but still display a
placeholder.

## Milestone 2 progress: the shared cell model

### The projection exists and is tested

`crates/repl/src/notebook/shadow_document.rs` builds the notebook as one percent-format
source document and tracks where each cell lives inside it:

- `ShadowDocument::from_cells` projects cells into text using the `# %%` markers that
  `repl_editor::jupytext_cells` already parses, commenting markdown and raw cells so the
  result stays valid in the notebook's language.
- `ShadowDocument::parse` recovers cells from that text, accepting the marker forms other
  tools write (`#%%` with no space, and `# %% tags=[...]` carrying metadata).
- `code_cell_ranges` gives the byte range of each code cell's source, which is what an
  excerpt-backed editor needs.

Cell ids are deliberately not written into the document, because standard jupytext files
do not carry them and emitting them would break compatibility with every other tool that
reads the format. Cells are matched positionally on the way back, as jupytext does; the
`.ipynb` stays authoritative for ids, metadata and outputs.

Eight unit tests cover projection, range accuracy, round-tripping, foreign marker forms,
and the blank-line edge cases.

### The spike answered its question: yes

`excerpt_spike` in the same file confirms both properties the design depends on:

1. An `Editor` over a single `MultiBuffer` excerpt into a shared, project-registered
   buffer shows **exactly** that cell and not its neighbours, with a real `Project`
   attached (the thing today's detached `Buffer::local` cells never have).
2. Editing through that editor lands in the shared buffer, leaving the other cells
   untouched. That shared buffer is what a language server would read.

So the excerpt approach is viable and the rewiring can proceed.

### One constraint the spike surfaced

`Editor::set_text` asserts a singleton buffer:

```rust
self.buffer.read(cx).as_singleton()
    .expect("you can only call set_text on editors for singleton buffers")
```

`CodeCell::new` and `MarkdownCell::new` both call `set_text` today. Excerpt-backed cell
editors must edit through the multibuffer instead, so those two call sites need changing
as part of the rewiring. Everything else about cell construction carries over.

### Milestone 2: what shipped

`ShadowBuffer` (`shadow_buffer.rs`) owns the live projection: one project-registered
buffer plus anchored per-cell ranges, so positions survive edits made anywhere in the
notebook. It supports incremental `insert_code_cell` and `remove_cell` (editing only the
affected region, leaving other cells' anchors intact) and a `rebuild` for reordering.
Six tests cover source tracking, anchor survival across edits, insertion, deletion,
reordering, and that typing into a freshly inserted cell stays inside it.

`CodeCell` now takes a `CellBacking`:

```rust
CellBacking::Shared { buffer, range, project }  // single-excerpt MultiBuffer
CellBacking::Local                              // detached, as before
```

`NotebookEditor` builds the `ShadowBuffer` before loading cells, backs every code cell
with an excerpt over it, applies the notebook's language to that one buffer once it
resolves, and keeps it in sync on add, delete and reload. Markdown cells stay on their
own buffers, since their text exists in the projection only in commented form.

Three tests assert the payoff directly: every code cell editor has a project attached,
all code cells excerpt the *same* buffer, and each editor still shows only its own cell.

### Bugs the rewiring exposed

Moving off singleton buffers surfaced three places that silently did the wrong thing
rather than failing:

1. `CodeCell::current_source` read via `as_singleton().map(..).unwrap_or_default()`. For
   an excerpt-backed editor that yields `""`, so **saving would have wiped every code
   cell**. Now reads through the multibuffer.
2. `execute_cell` read the code the same way, so it would have sent an empty string to
   the kernel on every run.
3. `mark_as_saved` marked each cell's singleton buffer saved. Code cells have no
   singleton, so they would have stayed dirty forever; the shared buffer is now marked
   saved directly.

The output-label language lookup had the same `as_singleton` shape and now reads
`language_at` off the multibuffer.

### Still open

## Settled: why the shared buffer alone does not give you LSP

This was flagged as an open question. It is now answered definitively, and the answer is
in Zed's own source. `LspStore::register_buffer_with_language_servers`
(`crates/project/src/lsp_store.rs:5286`) bails before registering anything when a buffer
has no file:

```rust
// ... we don't support non-file URI schemes in our LSP impl.
let Some(file) = File::from_dyn(buffer.read(cx).file()) else {
    return handle;
};
if !file.is_local() {
    return handle;
}
```

`create_local_buffer` produces an untitled buffer, so **no language server will ever
attach to the projection as it stands**. The project attachment and the shared scope are
real and tested, but they are necessary, not sufficient. Nothing in the current code
claims otherwise.

Getting a `File` means writing to disk. There is no third option inside Zed's present LSP
implementation. So the choice is:

| Option | What it costs |
|---|---|
| **A. Hidden file beside the notebook**, e.g. `.foo.lathe.py`, removed on close | Correct relative imports and project config, so pyright works properly. But a file appears in the user's repo and shows up in `git status` unless ignored. Stale files after a crash. |
| **B. File in a cache dir**, added as an invisible worktree | Nothing lands in the repo. But it sits outside the project root, so the venv, `pyproject.toml` and relative imports do not resolve, which is most of what makes completions useful. |
| **C. Teach Zed's LSP layer notebook documents** (`notebookDocument/didOpen` and cell sync per the LSP spec) | The correct answer, and what VS Code does. Substantially more work, in `lsp_store` rather than in `repl`. |

**Decision: C long term, A shipped now behind an opt-in setting.** B looks tidy but
delivers the least, because a Python file that cannot see the project is a Python file
whose completions are mostly wrong.

A is implemented as `jupyter.language_server_sidecar`, default off. When it is on,
opening `foo.ipynb` creates `.foo.lathe.py` beside it, the projection moves onto that
file-backed buffer, and every cell is re-excerpted against it. The upgrade is
asynchronous and degrades gracefully: the notebook opens immediately on the in-memory
buffer, and a read-only directory or a denied write costs language intelligence rather
than the notebook. The file is deleted on close via `cx.on_release`; because the name is
deterministic, a crash leaves one file that the next open reuses rather than
accumulating copies.

**Not verified end to end.** The tests prove the projection moves onto a file, that the
file is beside the notebook, and that cells still show their own text afterwards. They do
not prove a real language server attaches and returns completions, because the test
harness has no pyright. That needs a manual check before the feature is advertised.

C remains the correct destination. It belongs in `lsp_store`, not in `repl`, and it
removes the need for a file on disk entirely.
**Reordering is fixed.** It used to swap `cell_order` alone, leaving both the rendered
list and the shared buffer showing the old order. `ShadowBuffer::move_cell` now relocates
the cell's text as a remove-and-reinsert, so only the moved cell's anchors are
invalidated; its editor is re-pointed with `set_excerpt_range`, keeping its cursor,
selection and undo history. The cell list is spliced so the render follows. Two
debug-era `println!` calls went with it.

**Nav history and cursor position are implemented.** `NotebookEditor` stores its
`ItemNavHistory`, pushes an entry whenever the selected cell changes, and implements
`Item::navigate`. Entries hold the cell id rather than its index, so an entry still
points at the right cell after cells are added, removed or reordered, and an entry for a
deleted cell is declined rather than jumping somewhere arbitrary.
`pixel_position_of_cursor` now returns the selected cell's cursor.

Still open:

- **The jupytext view.** `ShadowBuffer::to_cells` parses the projection back into cells
  and is tested, but nothing in the UI edits the projection directly yet. This is a
  feature to build, not a loose end: it needs a view, a toggle action, and a decision
  about what `cmd-f` and project search operate on. Milestone 3.
- **Outline and breadcrumbs**, which become straightforward over one flat document now
  that the projection exists.
- **C**, above: LSP notebook document sync, which retires the sidecar.

## Settings

`clear_outputs_on_save` was added alongside the existing enable flag:

```jsonc
"jupyter": {
  "enabled": true,
  "notebook_enabled": false,
  // Strip cell outputs when saving, so committed notebooks stay diffable.
  "clear_outputs_on_save": false,
  "kernel_selections": {}
}
```

It exists because of the Milestone 1 round-trip work. Outputs now genuinely survive a
save, which is correct but means a committed notebook carries inline image data and a
diff nobody can read. This is the cheap half of the "notebook-aware diffs" item: it lets
a team keep notebooks reviewable without waiting for a diff view. Source, metadata and
markdown attachments are untouched; only `outputs` and `execution_count` are cleared.

Deliberately **not** given settings:

- **The shared buffer.** An implementation detail, not a user choice. A setting here
  would ossify a transitional state.
- **The SVG, GIF and LaTeX renderers.** Strictly more capability than "Unsupported media
  type"; nobody wants the old behavior back.
- **Script-driven HTML ranking below images.** A bug fix. Rendering an empty frame in
  preference to a working PNG was never a preference anyone held.

`language_server_sidecar` was added for the reason set out above: a file appearing next
to someone's notebook is a visible trade-off they should consent to, not discover. It is
off by default.

## Milestone 3: effort

Estimates assume one person with this codebase already in context, and they include
tests but **not** visual QA. Nothing in Milestones 1 and 2 was verified by looking at the
running app, so anything UI-heavy below carries extra risk that the numbers do not cover.

| Item | Effort | Confidence |
|---|---|---|
| Outline and breadcrumbs | 1-2 days | High |
| Jupytext view | 2-4 days | Medium |
| Notebook-aware diff | 1-2 weeks | Medium |
| LSP notebook document sync (item C) | 4-6 weeks | Low |
| Webview for interactive HTML | 6-10 weeks | Low |

### Why each lands where it does

**Outline and breadcrumbs (1-2 days).** The cheapest remaining item and the best value per
day. The projection is real text with a real language attached, so tree-sitter outline
queries already work on it; the work is mapping outline entries back to cells and
implementing `breadcrumb_location`, which `show_toolbar` currently short-circuits.

**Jupytext view (2-4 days).** Most of the parts exist: the projection, `to_cells` for
parsing back, the shared buffer, and with the sidecar on, a real file that an ordinary
editor can already open. The cost is write-back reconciliation (positional matching when
someone adds or removes cells in the text view) and stopping the two views from fighting
over the same document.

**Notebook-aware diff (1-2 weeks).** The data side is now tractable: project both sides
with `ShadowDocument::from_cells` and diff the projections rather than the JSON, which is
the whole trick. `crates/buffer_diff` (4,400 lines) and `git_ui/diff_multibuffer.rs`
already exist. The bulk is UI: cell-level hunks with outputs collapsed, wired into
git_ui's existing views.

**LSP notebook sync (4-6 weeks).** More expensive than first estimated, for a reason found
while checking: Zed's pinned `lsp-types` fork contains **no notebook types at all**, so
the protocol structs (`NotebookDocument`, `NotebookDocumentSyncOptions`,
`DidChangeNotebookDocumentParams` and the rest) have to be added or the fork upgraded
first. Then `lsp_store.rs` (17,090 lines) has to learn a second document kind, since its
registration path assumes one buffer maps to one file URI. Capability negotiation and a
fallback for servers without notebook support come after that. It also lands in a core
crate that upstream changes often, so it carries an ongoing merge cost that a fork feels
more than upstream does.

**Webview (6-10 weeks).** Confirmed: there is no webview primitive anywhere in the repo,
no `wry`, no CEF, no `WKWebView`. This is embedding a browser engine into a
GPU-composited window on three platforms, compositing native views over a GPU surface,
routing input and focus, and sandboxing untrusted output HTML. It is a GPUI project that
would serve the whole editor, not a notebook feature, and upstream has deliberately
avoided it.

### Recommendation

Do the first three, roughly **2.5 to 3 weeks**, and defer the last two.

The sidecar changes the calculus on C. It already delivers the user-visible benefit, so C
becomes a correctness and elegance upgrade rather than a capability one: it retires a file
on disk. That is worth doing eventually and hard to justify at six weeks plus merge cost
while other items are days.

The webview should be treated as out of scope for this fork. `ipywidgets` (Milestone 4)
depends on it, so both stay parked together, and notebooks using widgets keep working:
their outputs round-trip through a save untouched, they just display a placeholder.

## Milestone 3: delivered

90 tests pass, clippy clean, workspace builds. Estimates were 1-2 days, 2-4 days and
1-2 weeks; the first two landed, the third is partly done and the reason is worth
recording.

### Outline and breadcrumbs (done)

**Breadcrumbs** show the notebook, the position (`Cell 7 of 40`) and the selected cell's
label. The position is the part a plain editor cannot give you and is most of the
orientation in a long notebook. `show_toolbar` now returns true.

**Outline** is its own picker (`notebook/outline.rs`), not the editor's. The editor's
outline calls `as_singleton()` and bails when it is `None`, which a notebook always is.
It also lists the wrong thing: in a notebook the unit of structure is the cell, and
markdown headings are how people organize one. So it lists cells, labelled by heading,
then first definition, then first line, and selecting one jumps to it. Bound to
`ToggleOutline`, so the existing keybinding works.

`cell_label` is shared by both. A test caught it preferring a bare `#` over the cell's
real content.

### Jupytext view (done)

`notebook::ToggleSourceView` switches between the cell view and the notebook as one
percent-format script. Both views are editors over the *same* buffer, so switching copies
nothing and there is only ever one copy of the text. This is the derived-view design
paying off: the sync problem that makes jupytext's paired files fragile does not exist
here.

Leaving the source view re-derives the cell list, because editing text can add, remove,
retype or reorder cells. `reconcile_cells` matches by position, since the document
carries no ids:

- Same position and type: the cell is **kept**, with its id, metadata and outputs. Only
  its source changes.
- Anything else: a new cell. A type change counts as new, because a code cell's outputs
  would be meaningless carried onto prose.

A purely textual edit that touches no markers skips the rebuild entirely, so ids and
outputs survive untouched. That case is tested explicitly, because getting it wrong would
orphan every output in the notebook.

### Notebook-aware diff (done)

`notebook/diff.rs` implements the idea the projection made possible: project **both**
sides and diff the source, never the JSON.

It is now wired. `refresh_notebook_diff` reads the committed `.ipynb` through the diff
Zed already maintains for the file, projects it, and sets it as the base text of a
`BufferDiff` over the shared buffer. Cell editors are excerpts over that buffer, so each
renders the hunks falling inside it in its own gutter. There is no notebook-specific diff
UI, which is the point.

An earlier attempt was abandoned because `new_with_base_text` and
`recalculate_diff_sync` are both `#[cfg(test)]`. The production path is
`BufferDiff::new` plus `set_base_text`, which is public; missing it the first time cost a
detour.

Seven tests, including the two that matter:

- A notebook whose only change is that it was re-run produces a **byte-identical**
  projection and, end to end through `BufferDiff`, **zero hunks**. In JSON that is the
  change that buries every review in base64.
- Every code cell's multibuffer carries the diff, so hunks have somewhere to render.

## Verification: what the tests can and cannot tell you

101 tests pass. Nine of them open the notebook in a real window and drive full frames,
layout and paint included, across the states most likely to break it: rich outputs,
markdown and raw cells, an empty notebook, mid-deletion, after adding a cell, after
reordering, the source view, and after the sidecar swaps the buffer underneath every
cell.

They cover the notebook's **content**: the cell list, the empty state and the cell
controls. They cannot catch a visual defect such as wrong spacing, an unreadable color,
or an element that lays out to zero height. **Nothing here has been verified by looking
at it.**

### The kernel status bar cannot be painted in a test

Painting it deadlocks, with parking allowed or not. `KernelSelector`'s `RenderOnce::render`
calls `ReplStore::ensure_kernelspecs`, which shells out to discover kernels, and it
builds a fresh `Picker` entity on every frame.

Discovery from a render path is the underlying problem and is worth fixing on its own
merits: it also means the first paint of a notebook spawns subprocesses. The fix belongs
in `components/kernel_options.rs`, moving discovery to the call sites (the notebook
already calls `refresh_kernelspecs` in `NotebookEditor::new`, so it needs nothing extra).
It was left alone here because `KernelSelector` is shared with the inline REPL, and
changing a component used by two surfaces without being able to look at either is how
regressions get shipped.

Until then, the status bar is excluded from paint tests and is one of the things a person
should look at.

### Known cosmetic gap

The kernel status icon does not spin when the kernel is busy or starting. The status bar
renders it through the kernel selector's `start_icon`, which takes an `Icon` rather than
an element, so the animated variant cannot be passed. A dead `_status_icon_element` that
built the animated version and threw it away has been removed.

### What a reviewer should actually check

If the person shipping this does not use notebooks, the useful check is not a Jupyter
workflow, it is ten minutes of looking:

1. Open any `.ipynb` with `editor.notebook.notebook_enabled` on. Do cells render with
   their text, and outputs below them?
2. `cmd-shift-o`: does the outline list cells with sensible labels, and does picking one
   jump?
3. Are breadcrumbs present and do they track the selected cell?
4. `notebook::ToggleSourceView`: does the script view appear, and does toggling back
   preserve the cells?
5. Select a cell, move it up: does the list redraw in the new order?
6. Save, then `git diff`: is it empty when nothing changed?

Items 1 to 5 are visual and only a person can do them. Item 6 is the one that would
embarrass the editor most if wrong, and it is covered by tests.

The sidecar (`language_server_sidecar`) is the one feature that should **not** ship on in
beta, and does not: it needs a real pyright before anyone claims completions work.

## Gap audit

A pass looking for what was missed rather than what was built. Five things, all now
closed except where noted.

**A panic in the render path.** `cell_list` did
`this.cell_map.get(cell_id).unwrap()` inside the list's item closure. Cell order and the
map can disagree transiently while cells are added or removed, and panicking there takes
the editor down mid-frame. Now renders an empty row instead. A second `unwrap` on
notebook metadata in `NotebookItem::try_open` now propagates.

**Two actions had no keybinding.** `ToggleSourceView` and `ClearOutputs` existed and were
reachable only from the command palette. Bound across macOS, Linux and Windows keymaps in
both notebook contexts: `cmd-shift-y` / `ctrl-shift-y` for the source view,
`cmd-shift-backspace` / `ctrl-shift-backspace` for clearing outputs. Zed's keymap tests
pass, which is what catches a malformed keymap taking every binding down with it.

**Two settings were JSON-only.** `clear_outputs_on_save` and `language_server_sidecar`
were in the schema and the defaults but not in the settings UI, so anyone who does not
hand-edit settings could not reach them. Both now appear under
**Languages and Tools > Jupyter Notebooks** alongside the enable toggle.

**The feature documentation was stale.** `docs/features.md` still said "the editor itself
is upstream's" and that only the gating had changed, which stopped being true three
milestones ago. Rewritten to cover what is actually different, including the parts that
are not finished.

**No user-facing docs beyond that.** `docs/src/repl.md` documents the inline REPL and
mentions notebooks-as-scripts, but there is no notebook editor page in the book. Left
alone deliberately: the feature is experimental and off by default, and `features.md` is
where a Lathe user looks for what Lathe changed. Worth revisiting if the editor comes out
of experimental.

### Still open after the audit

Two of the three were fixable; the third was attempted and is better understood.

**Fixed: the kernel status spinner.** The icon now rotates while the kernel is busy,
starting, restarting or shutting down. `Button::start_icon` takes an `Icon` rather than an
element, so the animated variant could not be passed; `Button::loading` swaps the start
icon for a rotating spinner and is the intended way to do this.

**Fixed: kernel discovery from a render path.** `KernelSelector`'s `RenderOnce::render`
called `ReplStore::ensure_kernelspecs`, which shells out to find interpreters. That made
the first paint of a notebook, or of the REPL toolbar, spawn subprocesses. Discovery now
happens when each view is created: the notebook already did this in
`NotebookEditor::new`, and `repl_menu.rs` gained the same guarded call. Worth doing on its
own merits regardless of tests.

**Fixed: the status bar deadlock, and the two real defects behind it.**

A sample of the hung process gave the answer: `Picker::update_matches_with_options`
spawning, then `drop_glue<Entity<Picker<KernelPickerDelegate>>>` blocking on a channel
receive.

`KernelSelector::render` built a `Picker` entity on **every frame**. Creating one spawns
work to compute its matches, and the entity was dropped as soon as the frame ended;
dropping a picker mid-flight blocks on the task it spawned. So every frame of the
notebook status bar and the REPL toolbar allocated an entity, started a task, and threw
both away.

The picker is now built in the `PopoverMenu`'s menu closure, so it exists only while the
menu is open. `OnSelect` became `Rc` rather than `Box` so the closure can build one on
demand. This is a real performance fix in the shipping app, not only a test fix.

Two further defects fell out of making the tests deterministic:

- **`wsl_kernel_specifications` ran on every platform.** It shells out to `wsl -l -q`,
  which cannot succeed off Windows, so every kernel discovery on macOS and Linux spawned
  a doomed subprocess. Now gated to Windows.
- **Kernel discovery probes real interpreters**, running `python -c "import ipykernel"`
  per environment found. Correct in the app, fatal to a deterministic test. `ReplStore`
  gained a `test-support` `disable_discovery`, used by the notebook test helper.

All nine render tests now paint the **whole** notebook, status bar included, and the
suite passes repeatedly rather than intermittently.

### Still open

- **No visual verification of anything.** Unchanged, and not closable from here. The
  paint tests prove the notebook lays out and paints without panicking or deadlocking;
  they cannot tell you it looks right.
- **`language_server_sidecar` unverified against a real language server.** This machine
  has Python 3.12 but no pyright and no ipykernel. Zed downloads pyright on demand, so
  the check has to happen in a running app with a Python project open.
