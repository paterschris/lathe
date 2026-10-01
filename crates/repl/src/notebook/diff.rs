//! Notebook-aware diffing.
//!
//! `.ipynb` is JSON, so `git diff` on a notebook is unreadable: a one-line code change
//! shows up buried in escaped strings, and re-running a cell rewrites kilobytes of
//! base64 image data. Merge conflicts are close to unresolvable.
//!
//! The fix falls out of the projection. Both sides of the comparison are notebooks, so
//! both can be projected to percent-format source with [`ShadowDocument`], and the diff
//! taken over *that* rather than over the JSON. Outputs never reach the projection, so
//! they cannot produce noise, and what is left is the code and prose a reviewer wants to
//! see.
//!
//! Because the projected text is exactly what the shared buffer holds, the result plugs
//! into Zed's ordinary `BufferDiff`, and cell editors, being excerpts over that buffer,
//! render the hunks in their own gutters with no further work.

use anyhow::{Context as _, Result};

use super::shadow_document::ShadowDocument;

/// Projects the notebook in `contents` to the text its shared buffer would hold.
///
/// `contents` is a serialized `.ipynb`, typically the committed version of the file.
/// Used as the base text of a diff against the working copy's projection.
pub fn projection_of_notebook(contents: &str, comment_prefix: &str) -> Result<String> {
    let notebook = match nbformat::parse_notebook(contents) {
        Ok(nbformat::Notebook::V4(notebook)) => notebook,
        Ok(nbformat::Notebook::Legacy(legacy)) => nbformat::upgrade_legacy_notebook(legacy)
            .context("notebook is in an older format that could not be upgraded")?,
        Ok(nbformat::Notebook::V3(_)) => {
            anyhow::bail!("nbformat v3 notebooks are not supported")
        }
        Err(error) => return Err(error).context("could not parse notebook"),
    };

    Ok(ShadowDocument::from_cells(&notebook.cells, comment_prefix)
        .text()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"{
        "metadata": {"language_info": {"name": "python"}},
        "nbformat": 4,
        "nbformat_minor": 5,
        "cells": [
            {"cell_type": "code", "id": "one", "metadata": {}, "execution_count": 1,
             "outputs": [], "source": ["frame = load()"]},
            {"cell_type": "code", "id": "two", "metadata": {}, "execution_count": 2,
             "outputs": [], "source": ["frame.head()"]}
        ]
    }"#;

    /// The whole point: a notebook whose only change is that it was re-run produces an
    /// identical projection, so it produces an empty diff. In JSON this is the change
    /// that buries every review in base64.
    #[test]
    fn test_rerunning_a_notebook_produces_no_diff() {
        let after = r#"{
            "metadata": {"language_info": {"name": "python"}},
            "nbformat": 4,
            "nbformat_minor": 5,
            "cells": [
                {"cell_type": "code", "id": "one", "metadata": {}, "execution_count": 7,
                 "outputs": [{"output_type": "display_data",
                              "data": {"image/png": "iVBORw0KGgoAAAANSUhEUg=="},
                              "metadata": {}}],
                 "source": ["frame = load()"]},
                {"cell_type": "code", "id": "two", "metadata": {}, "execution_count": 8,
                 "outputs": [{"output_type": "stream", "name": "stdout",
                              "text": "lots and lots of output\n"}],
                 "source": ["frame.head()"]}
            ]
        }"#;

        let before = projection_of_notebook(BEFORE, "#").expect("should project");
        let after = projection_of_notebook(after, "#").expect("should project");

        assert_eq!(
            before, after,
            "new outputs and execution counts must not show up as a diff"
        );
    }

    /// A real code change does show, and shows as the one line it is.
    #[test]
    fn test_a_source_change_shows_as_source() {
        let after = BEFORE.replace("frame.head()", "frame.describe()");

        let before = projection_of_notebook(BEFORE, "#").expect("should project");
        let after = projection_of_notebook(&after, "#").expect("should project");

        assert_ne!(before, after);

        let changed: Vec<&str> = after
            .lines()
            .filter(|line| !before.lines().any(|before_line| before_line == *line))
            .collect();
        assert_eq!(
            changed,
            vec!["frame.describe()"],
            "exactly one line should differ, not a wall of JSON"
        );
    }

    /// Prose changes are reviewable too, since markdown cells are projected as comments
    /// rather than dropped.
    #[test]
    fn test_markdown_changes_are_visible() {
        let before = r#"{
            "metadata": {"language_info": {"name": "python"}},
            "nbformat": 4, "nbformat_minor": 5,
            "cells": [{"cell_type": "markdown", "id": "m", "metadata": {},
                       "source": ["Old heading"]}]
        }"#;
        let after = before.replace("Old heading", "New heading");

        let before = projection_of_notebook(before, "#").expect("should project");
        let after = projection_of_notebook(&after, "#").expect("should project");

        assert!(before.contains("# Old heading"));
        assert!(after.contains("# New heading"));
    }

    #[test]
    fn test_added_and_removed_cells_appear_as_added_and_removed_lines() {
        let after = r#"{
            "metadata": {"language_info": {"name": "python"}},
            "nbformat": 4, "nbformat_minor": 5,
            "cells": [
                {"cell_type": "code", "id": "one", "metadata": {}, "execution_count": 1,
                 "outputs": [], "source": ["frame = load()"]},
                {"cell_type": "code", "id": "new", "metadata": {}, "execution_count": null,
                 "outputs": [], "source": ["frame = frame.dropna()"]},
                {"cell_type": "code", "id": "two", "metadata": {}, "execution_count": 2,
                 "outputs": [], "source": ["frame.head()"]}
            ]
        }"#;

        let before = projection_of_notebook(BEFORE, "#").expect("should project");
        let after = projection_of_notebook(after, "#").expect("should project");

        assert!(after.contains("frame = frame.dropna()"));
        assert!(!before.contains("frame = frame.dropna()"));
        assert!(
            after.lines().count() > before.lines().count(),
            "an added cell should add lines"
        );
    }

    #[test]
    fn test_unparseable_notebook_is_an_error_not_a_panic() {
        assert!(projection_of_notebook("not json at all", "#").is_err());
        assert!(projection_of_notebook("{}", "#").is_err());
    }
}
